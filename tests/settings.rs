mod common;
#[path = "settings_governance/mod.rs"]
mod governance_settings;
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use brokerrouter::{
    console::{self, Permission},
    human, settings_api, store,
};
use chrono::{Duration, Utc};
use common::*;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::sync::Arc;
use tower::ServiceExt;
use uuid::Uuid;
const APPLY: &str = "/settings/v1/apply";
const KEYS: &str = "/settings/v1/keys/query";
const DETAIL: &str = "/settings/v1/keys/detail";
const FX: &str = "/settings/v1/fx/query";
struct Fixture {
    tenant: Uuid,
    user: Uuid,
}
async fn setup(pool: &PgPool) -> Fixture {
    let tenant = store::create_tenant(pool, "settings", 20_000_000)
        .await
        .unwrap();
    let user = human::add_user(pool, tenant, ISSUER, "alice")
        .await
        .unwrap();
    for id in ["test/model", "test/other"] {
        store::put_model(pool, tenant, id, id).await.unwrap();
    }
    Fixture { tenant, user }
}
async fn grant(pool: &PgPool, f: &Fixture, p: Permission, enabled: bool) {
    console::set_permission(pool, f.tenant, f.user, p, enabled, "settings test")
        .await
        .unwrap();
}
async fn all(pool: &PgPool, f: &Fixture) {
    for p in [
        Permission::KeysRead,
        Permission::KeysWrite,
        Permission::FxRead,
        Permission::FxWrite,
    ] {
        grant(pool, f, p, true).await;
    }
}
fn app(pool: &PgPool) -> Router {
    settings_api::router(settings_api::Settings {
        pool: pool.clone(),
        verifier: Arc::new(settings_verifier()),
    })
}
async fn send(app: &Router, path: &str, body: &Value, token: String) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::post(path)
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(serde_json::to_vec(body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    assert_eq!(response.headers()["cache-control"], "no-store");
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}
async fn call(pool: &PgPool, f: &Fixture, path: &str, body: &Value) -> (StatusCode, Value) {
    send(
        &app(pool),
        path,
        body,
        settings_sign(claims(f.tenant, "alice", path, body)),
    )
    .await
}
fn mutation(change: Value) -> Value {
    json!({"operation_id":Uuid::new_v4(),"reason":"reviewed settings change","change":change})
}
fn create() -> (Value, String) {
    // Test-only random input, never an upstream provider credential.
    let token = format!(
        "sk_br_{}{}",
        Uuid::new_v4().simple(),
        Uuid::new_v4().simple()
    );
    (
        mutation(
            json!({"kind":"key_create","name":"test key","key_hash":hex(&store::hash(token.as_bytes())),"key_prefix":&token[..14],"budget_cny":"10.000001","models":["test/model"],"expires_at":(Utc::now()+Duration::days(30)).to_rfc3339_opts(chrono::SecondsFormat::Micros,true)}),
        ),
        token,
    )
}
async fn detail(pool: &PgPool, f: &Fixture, id: &Value) -> Value {
    let (s, v) = call(pool, f, DETAIL, &json!({"id":id})).await;
    assert_eq!(s, 200, "{v}");
    v
}
fn update(row: &Value) -> Value {
    mutation(
        json!({"kind":"key_update","id":row["id"],"expected_revision":row["revision"],"name":"updated key","budget_cny":"10.000002","models":["test/model","test/other"],"expires_at":row["expires_at"]}),
    )
}

#[sqlx::test(migrations = "./migrations")]
async fn settings_permissions_purpose_and_tenant_isolation(pool: PgPool) {
    let f = setup(&pool).await;
    let (m, _) = create();
    assert_eq!(call(&pool, &f, KEYS, &json!({"limit":20})).await.0, 403);
    grant(&pool, &f, Permission::CatalogWrite, true).await;
    grant(&pool, &f, Permission::KeysWrite, true).await;
    assert_eq!(call(&pool, &f, APPLY, &m).await.0, 403);
    grant(&pool, &f, Permission::KeysRead, true).await;
    assert_eq!(
        send(
            &app(&pool),
            APPLY,
            &m,
            catalog_sign(claims(f.tenant, "alice", APPLY, &m))
        )
        .await
        .0,
        401
    );
    assert_eq!(
        send(
            &app(&pool),
            APPLY,
            &m,
            sign(&claims(f.tenant, "alice", APPLY, &m))
        )
        .await
        .0,
        401
    );
    let (s, r) = call(&pool, &f, APPLY, &m).await;
    assert_eq!(s, 200, "{r}");
    let other = setup(&pool).await;
    all(&pool, &other).await;
    assert_eq!(
        call(&pool, &other, DETAIL, &json!({"id":r["object_id"]}))
            .await
            .0,
        404
    );
    assert_eq!(
        call(&pool, &other, KEYS, &json!({"limit":20})).await.1["data"],
        json!([])
    );
    assert_eq!(call(&pool, &f, FX, &json!({"limit":20})).await.0, 403);
    let mut unknown = m.clone();
    unknown["change"]["token"] = json!("never accepted");
    assert_eq!(call(&pool, &f, APPLY, &unknown).await.0, 400);
    grant(&pool, &f, Permission::KeysWrite, false).await;
    assert_eq!(call(&pool, &f, APPLY, &m).await.0, 403);
    assert_eq!(
        call(&pool, &f, KEYS, &json!({"limit":20})).await.1["can_write"],
        false
    );
}
#[sqlx::test(migrations = "./migrations")]
async fn key_creation_retry_digest_only_revision_and_revoke(pool: PgPool) {
    let f = setup(&pool).await;
    all(&pool, &f).await;
    let (m, token) = create();
    let (a, b) = tokio::join!(call(&pool, &f, APPLY, &m), call(&pool, &f, APPLY, &m));
    assert_eq!(a.0, 200, "{a:?}");
    assert_eq!(a, b);
    let who = store::authenticate(&pool, &token).await.unwrap();
    assert_eq!(who.tenant_id, f.tenant);
    let row = detail(&pool, &f, &a.1["object_id"]).await;
    assert_eq!(row["key"]["budget_cny"], "10.000001");
    assert_eq!(row["models"], json!(["test/model"]));
    let text = call(&pool, &f, KEYS, &json!({"limit":20}))
        .await
        .1
        .to_string();
    assert!(!text.contains(&token));
    assert!(!text.contains("key_hash"));
    let audit: Value =
        sqlx::query_scalar("SELECT details FROM audit_events WHERE action='settings.key_created'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!audit.to_string().contains(&token));
    assert!(
        !audit
            .to_string()
            .contains(m["change"]["key_hash"].as_str().unwrap())
    );
    let receipt: Value = sqlx::query_scalar("SELECT response FROM settings_mutations")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(receipt, a.1);
    let mut changed = m.clone();
    changed["reason"] = json!("different");
    assert_eq!(call(&pool, &f, APPLY, &changed).await.0, 409);
    let u = update(&row["key"]);
    let (s, r) = call(&pool, &f, APPLY, &u).await;
    assert_eq!(s, 200, "{r}");
    let mut stale = u.clone();
    stale["operation_id"] = json!(Uuid::new_v4());
    assert_eq!(call(&pool, &f, APPLY, &stale).await.0, 409);
    let d = detail(&pool, &f, &a.1["object_id"]).await;
    assert_eq!(d["models"], json!(["test/model", "test/other"]));
    let revoke = mutation(
        json!({"kind":"key_revoke","id":a.1["object_id"],"expected_revision":d["key"]["revision"]}),
    );
    assert_eq!(call(&pool, &f, APPLY, &revoke).await.0, 200);
    assert_eq!(call(&pool, &f, APPLY, &revoke).await.0, 200);
    assert!(store::authenticate(&pool, &token).await.is_err());
    let d = detail(&pool, &f, &a.1["object_id"]).await;
    assert_eq!(call(&pool, &f, APPLY, &update(&d["key"])).await.0, 409);
    // Retrying old creation only returns its old receipt; it never resurrects the key.
    assert_eq!(call(&pool, &f, APPLY, &m).await, a);
    assert!(store::authenticate(&pool, &token).await.is_err());
}
#[sqlx::test(migrations = "./migrations")]
async fn key_validation_budget_reservations_and_cli_revisions(pool: PgPool) {
    let f = setup(&pool).await;
    all(&pool, &f).await;
    let (m, _) = create();
    for (field, value) in [
        ("budget_cny", json!("20.000001")),
        ("budget_cny", json!("0.0000001")),
        ("key_hash", json!("G".repeat(64))),
        ("models", json!(["test/model", "test/model"])),
        ("models", json!(["foreign/model"])),
        ("models", json!([])),
        ("expires_at", json!(Utc::now() - Duration::days(1))),
    ] {
        let mut invalid = m.clone();
        invalid["change"][field] = value;
        assert_eq!(call(&pool, &f, APPLY, &invalid).await.0, 400, "{field}");
    }
    let (_, r) = call(&pool, &f, APPLY, &m).await;
    let id: Uuid = serde_json::from_value(r["object_id"].clone()).unwrap();
    let d = detail(&pool, &f, &r["object_id"]).await;
    let mut accounting = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM tenants WHERE id=$1 FOR UPDATE")
        .bind(f.tenant)
        .fetch_one(&mut *accounting)
        .await
        .unwrap();
    sqlx::query("UPDATE virtual_keys SET reserved_micros=9000000 WHERE id=$1")
        .bind(id)
        .execute(&mut *accounting)
        .await
        .unwrap();
    let a = app(&pool);
    let mut u = update(&d["key"]);
    u["change"]["budget_cny"] = json!("5");
    let proof = settings_sign(claims(f.tenant, "alice", APPLY, &u));
    let task = tokio::spawn(async move { send(&a, APPLY, &u, proof).await });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    accounting.commit().await.unwrap();
    assert_eq!(task.await.unwrap().0, 409);
    let d2 = detail(&pool, &f, &r["object_id"]).await;
    assert_eq!(d2["key"]["revision"], d["key"]["revision"]); // Usage is not configuration.
    sqlx::query("DELETE FROM key_models WHERE tenant_id=$1 AND key_id=$2")
        .bind(f.tenant)
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    let d3 = detail(&pool, &f, &r["object_id"]).await;
    assert_ne!(d3["key"]["revision"], d2["key"]["revision"]);
    assert_eq!(call(&pool, &f, APPLY, &update(&d2["key"])).await.0, 409);
    store::revoke_key(&pool, f.tenant, id).await.unwrap();
    let d4 = detail(&pool, &f, &r["object_id"]).await;
    assert_ne!(d4["key"]["revision"], d3["key"]["revision"]);
}
#[sqlx::test(migrations = "./migrations")]
async fn fx_exact_append_only_schedule_conflicts_and_paging(pool: PgPool) {
    let f = setup(&pool).await;
    all(&pool, &f).await;
    let q = call(&pool, &f, FX, &json!({"limit":1})).await;
    assert_eq!(q.0, 200, "{q:?}");
    assert!(q.1["current"].is_null());
    let m = mutation(
        json!({"kind":"fx_publish","expected_latest_id":null,"usd_cny":"7.1234567890","source":"finance contract","effective_at":null}),
    );
    let (a, b) = tokio::join!(call(&pool, &f, APPLY, &m), call(&pool, &f, APPLY, &m));
    assert_eq!(a.0, 200, "{a:?}");
    assert_eq!(a, b);
    let current = store::current_fx(&pool, f.tenant).await.unwrap().unwrap();
    assert_eq!(current.usd_cny.to_string(), "7.1234567890");
    let mut future = m.clone();
    future["operation_id"] = json!(Uuid::new_v4());
    future["change"]["expected_latest_id"] = a.1["revision"].clone();
    future["change"]["effective_at"] = json!(
        (Utc::now() + Duration::days(1)).to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
    );
    future["change"]["usd_cny"] = json!("7.25");
    let (s, r) = call(&pool, &f, APPLY, &future).await;
    assert_eq!(s, 200, "{r}");
    let q = call(&pool, &f, FX, &json!({"limit":1})).await.1;
    assert_eq!(q["current"]["id"], a.1["revision"]);
    assert_eq!(q["data"][0]["id"], r["revision"]);
    assert_eq!(q["next_before"], r["revision"]);
    let next = call(&pool, &f, FX, &json!({"limit":1,"before":q["next_before"]}))
        .await
        .1;
    assert_eq!(next["data"][0]["id"], a.1["revision"]);
    assert!(next["next_before"].is_null());
    let mut stale = future.clone();
    stale["operation_id"] = json!(Uuid::new_v4());
    assert_eq!(call(&pool, &f, APPLY, &stale).await.0, 409);
    let cli = store::set_fx(
        &pool,
        f.tenant,
        "7.3".parse().unwrap(),
        Utc::now(),
        "CLI correction",
    )
    .await
    .unwrap();
    stale["change"]["expected_latest_id"] = r["revision"].clone();
    assert_eq!(call(&pool, &f, APPLY, &stale).await.0, 409);
    stale["change"]["expected_latest_id"] = json!(cli.to_string());
    stale["change"]["effective_at"] = Value::Null;
    for rate in ["0", "-1", "1000001", "0.00000000001"] {
        stale["change"]["usd_cny"] = json!(rate);
        assert_eq!(call(&pool, &f, APPLY, &stale).await.0, 400);
    }
    assert_eq!(
        store::fx_history(&pool, f.tenant, None, 100)
            .await
            .unwrap()
            .len(),
        3
    );
    assert!(
        sqlx::query("UPDATE fx_rates SET usd_cny=9")
            .execute(&pool)
            .await
            .is_err()
    );
}
#[sqlx::test(migrations = "./migrations")]
async fn settings_audit_failure_and_expired_proof_roll_back_all_effects(pool: PgPool) {
    let f = setup(&pool).await;
    all(&pool, &f).await;
    let (m, _) = create();
    sqlx::raw_sql("CREATE FUNCTION fail_settings() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.action LIKE 'settings.%' THEN RAISE EXCEPTION 'test audit failure'; END IF; RETURN NEW; END $$; CREATE TRIGGER fail_settings BEFORE INSERT ON audit_events FOR EACH ROW EXECUTE FUNCTION fail_settings();").execute(&pool).await.unwrap();
    assert_ne!(call(&pool, &f, APPLY, &m).await.0, 200);
    for table in ["virtual_keys", "key_models", "settings_mutations"] {
        assert_eq!(
            sqlx::query_scalar::<_, i64>(&format!("SELECT count(*) FROM {table}"))
                .fetch_one(&pool)
                .await
                .unwrap(),
            0
        );
    }
    sqlx::query("DROP TRIGGER fail_settings ON audit_events")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::raw_sql("CREATE FUNCTION slow_settings() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.action LIKE 'settings.%' THEN PERFORM pg_sleep(2); END IF; RETURN NEW; END $$; CREATE TRIGGER slow_settings BEFORE INSERT ON audit_events FOR EACH ROW EXECUTE FUNCTION slow_settings();").execute(&pool).await.unwrap();
    let mut claim = claims(f.tenant, "alice", APPLY, &m);
    claim.exp = Utc::now().timestamp() + 1;
    assert_eq!(
        send(&app(&pool), APPLY, &m, settings_sign(claim)).await.0,
        401
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM virtual_keys")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
}
#[sqlx::test(migrations = "./migrations")]
async fn settings_revocation_while_waiting_is_not_bypassed(pool: PgPool) {
    let f = setup(&pool).await;
    all(&pool, &f).await;
    let (m, _) = create();
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("UPDATE console_permissions SET enabled=false WHERE tenant_id=$1 AND user_id=$2 AND permission='keys_write'").bind(f.tenant).bind(f.user).execute(&mut *tx).await.unwrap();
    let a = app(&pool);
    let proof = settings_sign(claims(f.tenant, "alice", APPLY, &m));
    let task = tokio::spawn(async move { send(&a, APPLY, &m, proof).await });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    tx.commit().await.unwrap();
    assert_eq!(task.await.unwrap().0, 403);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM virtual_keys")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
}
async fn scoped(pool: &PgPool, role: &'static str) -> PgPool {
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(3)
        .after_connect(move |connection, _| {
            Box::pin(async move {
                sqlx::query(&format!("SET SESSION AUTHORIZATION {role}"))
                    .execute(connection)
                    .await?;
                Ok(())
            })
        })
        .connect_with(pool.connect_options().as_ref().clone())
        .await
        .unwrap()
}
#[sqlx::test(migrations = "./migrations")]
async fn settings_deployed_role_has_no_hash_read_or_runtime_catalog_approval_authority(
    pool: PgPool,
) {
    sqlx::raw_sql(include_str!("../deploy/permissions.sql"))
        .execute(&pool)
        .await
        .unwrap();
    let f = setup(&pool).await;
    all(&pool, &f).await;
    let restricted = scoped(&pool, "brokerrouter_settings").await;
    brokerrouter::check_schema(&restricted).await.unwrap();
    let (m, _) = create();
    let (s, r) = call(&restricted, &f, APPLY, &m).await;
    assert_eq!(s, 200, "{r}");
    assert_eq!(
        call(&restricted, &f, KEYS, &json!({"limit":20})).await.0,
        200
    );
    let d = detail(&restricted, &f, &r["object_id"]).await;
    assert_eq!(
        call(&restricted, &f, APPLY, &update(&d["key"])).await.0,
        200
    );
    assert_eq!(
        call(
            &restricted,
            &f,
            "/settings/v1/models/query",
            &json!({"limit":20})
        )
        .await
        .0,
        200
    );
    let fx = mutation(
        json!({"kind":"fx_publish","expected_latest_id":null,"usd_cny":"7.25","source":"contract","effective_at":null}),
    );
    assert_eq!(call(&restricted, &f, APPLY, &fx).await.0, 200);
    assert_eq!(call(&restricted, &f, FX, &json!({"limit":20})).await.0, 200);
    for sql in [
        "SELECT key_hash FROM virtual_keys",
        "UPDATE virtual_keys SET key_hash=decode(repeat('ab',32),'hex')",
        "UPDATE virtual_keys SET spent_micros=0",
        "UPDATE tenants SET budget_micros=100",
        "SELECT * FROM endpoints",
        "SELECT * FROM approvals",
        "SELECT * FROM requests",
        "UPDATE console_permissions SET enabled=true",
        "UPDATE governance_users SET active=true",
        "DELETE FROM settings_mutations",
        "UPDATE settings_mutations SET response='{}'",
        "SET ROLE brokerrouter_operator",
        "SET ROLE brokerrouter_runtime",
        "SET ROLE brokerrouter_catalog",
    ] {
        assert!(
            sqlx::query(sql).execute(&restricted).await.is_err(),
            "{sql}"
        );
    }
    for role in [
        "brokerrouter_console",
        "brokerrouter_bff",
        "brokerrouter_catalog",
        "brokerrouter_runtime",
    ] {
        let other = scoped(&pool, role).await;
        assert!(
            sqlx::query("SET ROLE brokerrouter_settings")
                .execute(&other)
                .await
                .is_err()
        );
        assert!(
            sqlx::query("INSERT INTO settings_mutations SELECT * FROM settings_mutations")
                .execute(&other)
                .await
                .is_err()
        );
        other.close().await;
    }
    let operator = scoped(&pool, "brokerrouter_operator").await;
    store::create_key(
        &operator,
        f.tenant,
        "CLI key",
        0,
        &["test/model".into()],
        None,
    )
    .await
    .unwrap();
    operator.close().await;
    restricted.close().await;
}
