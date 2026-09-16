mod common;
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use brokerrouter::{
    catalog::{self, Connection},
    catalog_api,
    console::{self, Permission},
    human, store,
};
use common::*;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::sync::Arc;
use tower::ServiceExt;
use uuid::Uuid;
const APPLY: &str = "/catalog/v1/apply";
const MODELS: &str = "/catalog/v1/models/query";
const ENDPOINTS: &str = "/catalog/v1/endpoints/query";
struct Fixture {
    tenant: Uuid,
    user: Uuid,
    connections: Vec<Connection>,
}
async fn setup(pool: &PgPool) -> Fixture {
    let tenant = store::create_tenant(pool, "catalog", 20_000_000)
        .await
        .unwrap();
    let user = human::add_user(pool, tenant, ISSUER, "alice")
        .await
        .unwrap();
    let connections = vec![Connection {
        tenant_id: tenant,
        id: "approved".into(),
        name: "Registered provider".into(),
        base_url: "https://provider.example/compatible-mode/v1".into(),
        credential_env: "UPSTREAM_TEST_ONLY".into(),
        allow_private: false,
    }];
    Fixture {
        tenant,
        user,
        connections,
    }
}
async fn grant(pool: &PgPool, f: &Fixture, p: Permission, enabled: bool) {
    console::set_permission(pool, f.tenant, f.user, p, enabled, "contract test")
        .await
        .unwrap();
}
fn app(pool: &PgPool, f: &Fixture) -> Router {
    catalog_api::router(catalog_api::Catalog {
        pool: pool.clone(),
        verifier: Arc::new(catalog_verifier()),
        connections: Arc::new(f.connections.clone()),
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
    assert_eq!(response.headers()["cache-control"], "no-store");
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}
async fn call(pool: &PgPool, f: &Fixture, path: &str, body: &Value) -> (StatusCode, Value) {
    send(
        &app(pool, f),
        path,
        body,
        catalog_sign(claims(f.tenant, "alice", path, body)),
    )
    .await
}
fn mutation(change: Value) -> Value {
    json!({"operation_id":Uuid::new_v4(),"reason":"reviewed configuration change","change":change})
}
async fn create_model(pool: &PgPool, f: &Fixture) -> Value {
    let (s, r) = call(
        pool,
        f,
        APPLY,
        &mutation(json!({"kind":"model_create","id":"test/model","name":"Test model"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    r
}
async fn endpoint_change(pool: &PgPool, f: &Fixture) -> Value {
    let (s, r) = call(
        pool,
        f,
        ENDPOINTS,
        &json!({"model_id":"test/model","limit":30}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert!(!r.to_string().contains("credential_env"));
    assert!(!r.to_string().contains("provider.example"));
    json!({"kind":"endpoint_create","model_id":"test/model","connection_id":"approved","connection_revision":r["connections"][0]["revision"],"name":"Provider 1","upstream_model":"upstream-v1","currency":"CNY","input_per_million":"0.000000000001","output_per_million":"2.50","context_tokens":4096,"max_output_tokens":1024,"priority":100,"operation":"chat","supports_tools":false,"embedding_dimension":null})
}
#[sqlx::test(migrations = "./migrations")]
async fn catalog_permissions_purpose_tenant_and_no_secret_boundary(pool: PgPool) {
    let f = setup(&pool).await;
    let q = json!({"limit":20});
    assert_eq!(call(&pool, &f, MODELS, &q).await.0, StatusCode::FORBIDDEN);
    grant(&pool, &f, Permission::CatalogRead, true).await;
    assert_eq!(call(&pool, &f, MODELS, &q).await.1["can_write"], false);
    let m = mutation(json!({"kind":"model_create","id":"test/model","name":"Test"}));
    assert_eq!(call(&pool, &f, APPLY, &m).await.0, StatusCode::FORBIDDEN);
    grant(&pool, &f, Permission::CatalogWrite, true).await;
    for token in [
        sign(&claims(f.tenant, "alice", APPLY, &m)),
        console_sign(claims(f.tenant, "alice", APPLY, &m)),
    ] {
        assert_eq!(
            send(&app(&pool, &f), APPLY, &m, token).await.0,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(call(&pool, &f, APPLY, &m).await.0, StatusCode::OK);
    let other = setup(&pool).await;
    grant(&pool, &other, Permission::CatalogRead, true).await;
    assert!(
        call(&pool, &other, MODELS, &q).await.1["data"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        call(
            &pool,
            &other,
            ENDPOINTS,
            &json!({"model_id":"test/model","limit":20})
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    grant(&pool, &f, Permission::CatalogRead, false).await;
    assert_eq!(call(&pool, &f, APPLY, &m).await.0, StatusCode::FORBIDDEN);
}
#[sqlx::test(migrations = "./migrations")]
async fn catalog_idempotency_revision_prices_and_safe_connection_contract(pool: PgPool) {
    let f = setup(&pool).await;
    grant(&pool, &f, Permission::CatalogRead, true).await;
    grant(&pool, &f, Permission::CatalogWrite, true).await;
    create_model(&pool, &f).await;
    assert_eq!(
        call(&pool, &f, MODELS, &json!({"limit":20})).await.1["data"][0]["active"],
        false
    );
    let change = endpoint_change(&pool, &f).await;
    let m = mutation(change.clone());
    let (a, b) = tokio::join!(call(&pool, &f, APPLY, &m), call(&pool, &f, APPLY, &m));
    assert_eq!(a.0, StatusCode::OK, "{:?}", a);
    assert_eq!(a, b);
    let endpoint = a.1["object_id"].as_str().unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM endpoints")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM audit_events WHERE action='catalog.endpoint_created'"
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        1
    );
    let mut clash = m.clone();
    clash["reason"] = json!("different request");
    assert_eq!(call(&pool, &f, APPLY, &clash).await.0, StatusCode::CONFLICT);
    for (field, value) in [
        ("base_url", json!("https://attacker.example")),
        ("credential_env", json!("DATABASE_URL")),
    ] {
        let mut invalid = mutation(change.clone());
        invalid["change"][field] = value;
        assert_eq!(
            call(&pool, &f, APPLY, &invalid).await.0,
            StatusCode::BAD_REQUEST
        );
    }
    let mut invalid = change.clone();
    invalid["connection_id"] = json!("unregistered");
    assert_eq!(
        call(&pool, &f, APPLY, &mutation(invalid)).await.0,
        StatusCode::NOT_FOUND
    );
    let mut invalid = change.clone();
    invalid["connection_revision"] = json!("outdated");
    assert_eq!(
        call(&pool, &f, APPLY, &mutation(invalid)).await.0,
        StatusCode::CONFLICT
    );
    let update = mutation(
        json!({"kind":"endpoint_update","id":endpoint,"expected_revision":a.1["revision"],"name":"Changed","active":true,"priority":2,"currency":"USD","input_per_million":"1.123456789012","output_per_million":"3"}),
    );
    assert_eq!(call(&pool, &f, APPLY, &update).await.0, StatusCode::OK);
    let mut stale = update.clone();
    stale["operation_id"] = json!(Uuid::new_v4());
    assert_eq!(call(&pool, &f, APPLY, &stale).await.0, StatusCode::CONFLICT);
    let rows = call(
        &pool,
        &f,
        ENDPOINTS,
        &json!({"model_id":"test/model","limit":20}),
    )
    .await
    .1;
    assert_eq!(rows["data"][0]["active"], true);
    assert_eq!(rows["data"][0]["input_per_million"], "1.123456789012");
    // CLI changes also advance the browser's revision.
    store::put_model(&pool, f.tenant, "test/model", "Operator rename")
        .await
        .unwrap();
    let stale = mutation(
        json!({"kind":"model_update","id":"test/model","expected_revision":"1","name":"Lost update","active":true}),
    );
    assert_eq!(call(&pool, &f, APPLY, &stale).await.0, StatusCode::CONFLICT);
    let mut incompatible = change;
    incompatible["operation"] = json!("embedding");
    incompatible["embedding_dimension"] = json!(1024);
    assert_eq!(
        call(&pool, &f, APPLY, &mutation(incompatible)).await.0,
        StatusCode::BAD_REQUEST
    );
}
#[sqlx::test(migrations = "./migrations")]
async fn catalog_failure_rolls_back_configuration_audit_and_receipt(pool: PgPool) {
    let f = setup(&pool).await;
    grant(&pool, &f, Permission::CatalogRead, true).await;
    grant(&pool, &f, Permission::CatalogWrite, true).await;
    sqlx::raw_sql("CREATE FUNCTION fail_catalog_audit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.action LIKE 'catalog.%' THEN RAISE EXCEPTION 'test failure'; END IF; RETURN NEW; END $$; CREATE TRIGGER fail_catalog_audit BEFORE INSERT ON audit_events FOR EACH ROW EXECUTE FUNCTION fail_catalog_audit();").execute(&pool).await.unwrap();
    let m = mutation(json!({"kind":"model_create","id":"test/model","name":"Test"}));
    assert_eq!(
        call(&pool, &f, APPLY, &m).await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    for table in ["models", "catalog_mutations"] {
        assert_eq!(
            sqlx::query_scalar::<_, i64>(&format!("SELECT count(*) FROM {table}"))
                .fetch_one(&pool)
                .await
                .unwrap(),
            0
        );
    }
    sqlx::query("DROP TRIGGER fail_catalog_audit ON audit_events")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(call(&pool, &f, APPLY, &m).await.0, StatusCode::OK);
    let details: Value =
        sqlx::query_scalar("SELECT details FROM audit_events WHERE action='catalog.model_created'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(details["user_id"], f.user.to_string());
    assert_eq!(details["operation_id"], m["operation_id"]);
    let facts: Option<String> = sqlx::query_scalar(
        "SELECT change_reason FROM console_audit_facts WHERE action='catalog.model_created'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(facts.as_deref(), Some("reviewed configuration change"));
}
async fn scoped(pool: &PgPool, role: &'static str) -> PgPool {
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .after_connect(move |c, _| {
            Box::pin(async move {
                sqlx::query(&format!("SET SESSION AUTHORIZATION {role}"))
                    .execute(c)
                    .await?;
                Ok(())
            })
        })
        .connect_with((*pool.connect_options()).clone())
        .await
        .unwrap()
}
#[sqlx::test(migrations = "./migrations")]
async fn catalog_deployment_role_can_manage_but_cannot_grant_approve_or_read_credentials(
    pool: PgPool,
) {
    sqlx::raw_sql(include_str!("../deploy/permissions.sql"))
        .execute(&pool)
        .await
        .unwrap();
    let f = setup(&pool).await;
    grant(&pool, &f, Permission::CatalogRead, true).await;
    grant(&pool, &f, Permission::CatalogWrite, true).await;
    let config = scoped(&pool, "brokerrouter_catalog").await;
    brokerrouter::check_schema(&config).await.unwrap();
    create_model(&config, &f).await;
    let change = endpoint_change(&config, &f).await;
    let (s, r) = call(&config, &f, APPLY, &mutation(change)).await;
    assert_eq!(s, StatusCode::OK, "{r}");
    for sql in [
        "SELECT key_hash FROM virtual_keys",
        "SELECT result_ciphertext FROM requests",
        "SELECT * FROM approvals",
        "UPDATE console_permissions SET enabled=true",
        "UPDATE governance_users SET active=true",
        "UPDATE tenants SET budget_micros=1",
        "UPDATE endpoints SET base_url='https://attacker.example'",
        "DELETE FROM catalog_mutations",
        "UPDATE catalog_mutations SET response='{}'",
        "SET ROLE brokerrouter_runtime",
        "SET ROLE brokerrouter_operator",
        "SET ROLE brokerrouter_governance_control",
    ] {
        assert!(sqlx::query(sql).execute(&config).await.is_err(), "{sql}");
    }
    for role in ["brokerrouter_console", "brokerrouter_bff"] {
        let readonly = scoped(&pool, role).await;
        assert!(sqlx::query("INSERT INTO models(tenant_id,id,name) SELECT id,'forbidden','forbidden' FROM tenants").execute(&readonly).await.is_err());
        assert!(
            sqlx::query("SET ROLE brokerrouter_catalog")
                .execute(&readonly)
                .await
                .is_err()
        );
        readonly.close().await;
    }
    config.close().await;
}

#[test]
fn operator_connection_manifest_is_bounded_validated_and_tenant_scoped() {
    let c = Connection {
        tenant_id: Uuid::new_v4(),
        id: "test".into(),
        name: "test".into(),
        base_url: "https://provider.example/v1".into(),
        credential_env: "TEST_KEY".into(),
        allow_private: false,
    };
    catalog::validate_connections(std::slice::from_ref(&c)).unwrap();
    assert!(catalog::validate_connections(&[c.clone(), c.clone()]).is_err());
    let mut c = c;
    c.base_url = "http://127.0.0.1/v1".into();
    assert!(catalog::validate_connections(&[c.clone()]).is_err());
    c.allow_private = true;
    catalog::validate_connections(&[c]).unwrap();
}

#[sqlx::test(migrations = "./migrations")]
async fn catalog_revocation_while_waiting_and_expiry_roll_back_before_commit(pool: PgPool) {
    let f = setup(&pool).await;
    grant(&pool, &f, Permission::CatalogRead, true).await;
    grant(&pool, &f, Permission::CatalogWrite, true).await;
    let m = mutation(json!({"kind":"model_create","id":"test/model","name":"Test"}));
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("UPDATE console_permissions SET enabled=false WHERE tenant_id=$1 AND user_id=$2 AND permission='catalog_write'").bind(f.tenant).bind(f.user).execute(&mut *tx).await.unwrap();
    let router = app(&pool, &f);
    let body = m.clone();
    let token = catalog_sign(claims(f.tenant, "alice", APPLY, &m));
    let pending = tokio::spawn(async move { send(&router, APPLY, &body, token).await });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    tx.commit().await.unwrap();
    assert_eq!(pending.await.unwrap().0, StatusCode::FORBIDDEN);
    grant(&pool, &f, Permission::CatalogWrite, true).await;
    // Leave admission time under parallel CI load, then expire inside the audit transaction.
    // A one-second JWT can already expire in begin(), legitimately returning 403 instead of 401.
    sqlx::raw_sql("CREATE FUNCTION slow_catalog_audit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.action LIKE 'catalog.%' THEN PERFORM pg_sleep(4); END IF; RETURN NEW; END $$; CREATE TRIGGER slow_catalog_audit BEFORE INSERT ON audit_events FOR EACH ROW EXECUTE FUNCTION slow_catalog_audit();").execute(&pool).await.unwrap();
    let mut claim = claims(f.tenant, "alice", APPLY, &m);
    claim.exp = chrono::Utc::now().timestamp() + 3;
    assert_eq!(
        send(&app(&pool, &f), APPLY, &m, catalog_sign(claim))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM models")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM catalog_mutations")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn catalog_changes_do_not_rewrite_admitted_route_or_price_snapshots(pool: PgPool) {
    let f = setup(&pool).await;
    grant(&pool, &f, Permission::CatalogRead, true).await;
    grant(&pool, &f, Permission::CatalogWrite, true).await;
    create_model(&pool, &f).await;
    store::put_model(&pool, f.tenant, "test/model", "Test")
        .await
        .unwrap();
    sqlx::query("UPDATE models SET active=true WHERE tenant_id=$1")
        .bind(f.tenant)
        .execute(&pool)
        .await
        .unwrap();
    let change = endpoint_change(&pool, &f).await;
    let (_, receipt) = call(&pool, &f, APPLY, &mutation(change)).await;
    let endpoint: Uuid = serde_json::from_value(receipt["object_id"].clone()).unwrap();
    sqlx::query("UPDATE endpoints SET active=true WHERE id=$1")
        .bind(endpoint)
        .execute(&pool)
        .await
        .unwrap();
    let (key, _) = store::create_key(
        &pool,
        f.tenant,
        "test key",
        20_000_000,
        &["test/model".into()],
        None,
    )
    .await
    .unwrap();
    let who = store::Identity {
        tenant_id: f.tenant,
        key_id: key,
    };
    let e = store::endpoints(&pool, &who, "test/model", 10)
        .await
        .unwrap()
        .remove(0);
    let price = e.price(&pool).await.unwrap();
    let route = serde_json::to_value(&e).unwrap();
    let (reservation, _) = brokerrouter::accounting::reserve(
        &pool,
        brokerrouter::accounting::Admission {
            identity: &who,
            model: "test/model",
            idempotency_key: "before-change",
            request_hash: &store::hash(b"before-change"),
            maximum_micros: 100,
            snapshot: price,
            routing: Some(route.clone()),
            timeout_seconds: 60,
            governance_turn_id: None,
        },
    )
    .await
    .unwrap();
    let current = call(
        &pool,
        &f,
        ENDPOINTS,
        &json!({"model_id":"test/model","limit":20}),
    )
    .await
    .1;
    let change = mutation(
        json!({"kind":"endpoint_update","id":endpoint,"expected_revision":current["data"][0]["revision"],"name":"Disabled after admission","active":false,"priority":0,"currency":"USD","input_per_million":"100","output_per_million":"200"}),
    );
    assert_eq!(call(&pool, &f, APPLY, &change).await.0, StatusCode::OK);
    assert!(
        store::endpoints(&pool, &who, "test/model", 10)
            .await
            .unwrap()
            .is_empty()
    );
    let stored: (Value, Value) =
        sqlx::query_as("SELECT price_snapshot,routing_snapshot FROM requests WHERE id=$1")
            .bind(reservation.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(stored.0, reservation.price_snapshot);
    assert_eq!(stored.1, route);
}
