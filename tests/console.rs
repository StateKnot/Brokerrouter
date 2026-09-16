mod common;
#[path = "console_monitoring/mod.rs"]
mod monitoring;
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use brokerrouter::{
    accounting::{self, Admission},
    console::{self, Permission},
    console_api,
    error::Error,
    human,
    money::PriceSnapshot,
    store,
};
use chrono::{DateTime, Duration, Utc};
use common::*;
use http_body_util::BodyExt;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::sync::Arc;
use tower::ServiceExt;
use uuid::Uuid;

const USAGE: &str = "/console/v1/usage/query";
const REQUESTS: &str = "/console/v1/requests/query";
const GROUPS: &str = "/console/v1/usage/groups/query";
const AUDIT: &str = "/console/v1/audit/query";
struct Fixture {
    who: store::Identity,
    key: String,
    user: Uuid,
}
async fn setup(pool: &PgPool) -> Fixture {
    let tenant = store::create_tenant(pool, "console", 1_000_000)
        .await
        .unwrap();
    for model in ["alpha", "beta"] {
        store::put_model(pool, tenant, model, model).await.unwrap();
    }
    let (key_id, key) = store::create_key(
        pool,
        tenant,
        "test alias",
        1_000_000,
        &["alpha".into(), "beta".into()],
        None,
    )
    .await
    .unwrap();
    let user = human::add_user(pool, tenant, ISSUER, "alice")
        .await
        .unwrap();
    Fixture {
        who: store::Identity {
            tenant_id: tenant,
            key_id,
        },
        key,
        user,
    }
}
fn app(pool: &PgPool) -> Router {
    console_api::router(console_api::Console {
        pool: pool.clone(),
        verifier: Arc::new(console_verifier()),
    })
}
fn filter() -> Value {
    json!({"start":Utc::now()-Duration::days(1),"end":Utc::now()+Duration::days(1)})
}
fn query() -> Value {
    json!({"filter":filter()})
}
async fn send(app: &Router, token: &str, path: &str, body: &Value) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::post(path)
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert_eq!(response.headers()["x-content-type-options"], "nosniff");
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}
async fn call(pool: &PgPool, f: &Fixture, path: &str, body: &Value) -> (StatusCode, Value) {
    send(
        &app(pool),
        &console_sign(claims(f.who.tenant_id, "alice", path, body)),
        path,
        body,
    )
    .await
}
async fn grant(pool: &PgPool, f: &Fixture, p: Permission, enabled: bool) {
    console::set_permission(
        pool,
        f.who.tenant_id,
        f.user,
        p,
        enabled,
        "fixture permission evidence",
    )
    .await
    .unwrap();
}
async fn reservation(pool: &PgPool, f: &Fixture, model: &str) -> Uuid {
    let nonce = Uuid::new_v4().to_string();
    accounting::reserve(
        pool,
        Admission {
            identity: &f.who,
            model,
            idempotency_key: &nonce,
            request_hash: &store::hash(nonce.as_bytes()),
            maximum_micros: 1000,
            snapshot: PriceSnapshot {
                currency: "CNY".into(),
                input_per_million: Decimal::ONE,
                output_per_million: Decimal::from(2),
                cny_per_unit: Decimal::ONE,
                fx_version: None,
            },
            routing: None,
            timeout_seconds: 60,
            governance_turn_id: None,
        },
    )
    .await
    .unwrap()
    .0
    .id
}
async fn success(pool: &PgPool, f: &Fixture, model: &str) -> Uuid {
    let id = reservation(pool, f, model).await;
    accounting::mark_submitting(pool, &f.who, id).await.unwrap();
    accounting::settle_usage(pool, &f.who, id, 10, 20, None)
        .await
        .unwrap();
    id
}

#[sqlx::test(migrations = "./migrations")]
async fn console_requires_separate_signed_type_exact_request_and_explicit_permission(pool: PgPool) {
    let f = setup(&pool).await;
    let q = query();
    let c = claims(f.who.tenant_id, "alice", USAGE, &q);
    for token in [&f.key, &sign(&c)] {
        assert_eq!(
            send(&app(&pool), token, USAGE, &q).await.0,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(call(&pool, &f, USAGE, &q).await.0, StatusCode::FORBIDDEN);
    grant(&pool, &f, Permission::UsageRead, true).await;
    assert_eq!(call(&pool, &f, USAGE, &q).await.0, StatusCode::OK);
    let token = console_sign(c.clone());
    assert_eq!(
        send(&app(&pool), &token, REQUESTS, &q).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        send(
            &app(&pool),
            &token,
            USAGE,
            &json!({"filter":filter(),"timezone":"UTC"})
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    for bad in 0..4 {
        let mut c = c.clone();
        match bad {
            0 => c.sub = "mallory".into(),
            1 => c.exp = c.iat - 1,
            2 => c.interactive = false,
            _ => c.method = "GET".into(),
        };
        assert_eq!(
            send(&app(&pool), &console_sign(c), USAGE, &q).await.0,
            StatusCode::UNAUTHORIZED
        );
    }
    assert!(
        verifier()
            .verify(
                &pool,
                &token,
                "POST",
                USAGE,
                &serde_json::to_vec(&q).unwrap()
            )
            .await
            .is_err()
    );
    let audit = filter();
    assert_eq!(
        call(&pool, &f, AUDIT, &audit).await.0,
        StatusCode::FORBIDDEN
    );
    grant(&pool, &f, Permission::UsageRead, false).await;
    assert_eq!(
        send(&app(&pool), &token, USAGE, &q).await.0,
        StatusCode::FORBIDDEN
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn reports_reconcile_ledger_states_unknown_cost_and_token_coverage(pool: PgPool) {
    let f = setup(&pool).await;
    grant(&pool, &f, Permission::UsageRead, true).await;
    success(&pool, &f, "alpha").await;
    let failed = reservation(&pool, &f, "alpha").await;
    accounting::release_unsubmitted(&pool, &f.who, failed)
        .await
        .unwrap();
    let unknown = reservation(&pool, &f, "beta").await;
    accounting::mark_submitting(&pool, &f.who, unknown)
        .await
        .unwrap();
    accounting::mark_unknown(&pool, &f.who, unknown, "lost_response")
        .await
        .unwrap();
    reservation(&pool, &f, "beta").await;
    let reconciled = reservation(&pool, &f, "beta").await;
    accounting::mark_submitting(&pool, &f.who, reconciled)
        .await
        .unwrap();
    accounting::finish(
        &pool,
        &f.who,
        reconciled,
        75,
        None,
        json!({"reconciliation":"fixture_no_token_evidence"}),
    )
    .await
    .unwrap();
    sqlx::query("UPDATE requests SET status='reconciled' WHERE id=$1")
        .bind(reconciled)
        .execute(&pool)
        .await
        .unwrap();
    let (status, r) = call(&pool, &f, USAGE, &query()).await;
    assert_eq!(status, StatusCode::OK, "{r}");
    let t = &r["totals"];
    for (k, v) in [
        ("requests", "5"),
        ("succeeded", "1"),
        ("failed", "1"),
        ("reconciled", "1"),
        ("in_progress", "1"),
        ("submission_unknown", "1"),
        ("known_cost_requests", "3"),
        ("unsettled_cost_requests", "2"),
        ("known_cost_cny", "0.000125"),
        ("reserved_cny", "0.002000"),
        ("token_usage_requests", "1"),
        ("total_tokens", "30"),
    ] {
        assert_eq!(t[k], v, "{k}: {r}");
    }
    let billed:i64=sqlx::query_scalar("SELECT sum(amount_micros)::bigint FROM ledger_entries WHERE tenant_id=$1 AND kind='finalize'").bind(f.who.tenant_id).fetch_one(&pool).await.unwrap();
    assert_eq!(billed, 125);
    let sum: Decimal = r["daily"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| {
            d["totals"]["known_cost_cny"]
                .as_str()
                .unwrap()
                .parse::<Decimal>()
                .unwrap()
        })
        .sum();
    assert_eq!(sum, Decimal::new(125, 6));
    let (_, logs) = call(&pool, &f, REQUESTS, &query()).await;
    let row = logs["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == unknown.to_string())
        .unwrap();
    assert!(row["cost_cny"].is_null());
    assert!(row["input_tokens"].is_null());
    assert_eq!(row["reserved_cny"], "0.001000");
    let counts: (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM requests),(SELECT count(*) FROM ledger_entries)",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    call(&pool, &f, USAGE, &query()).await;
    assert_eq!(
        counts,
        sqlx::query_as(
            "SELECT (SELECT count(*) FROM requests),(SELECT count(*) FROM ledger_entries)"
        )
        .fetch_one(&pool)
        .await
        .unwrap()
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn tenant_filters_and_keyset_pages_never_leak_or_duplicate(pool: PgPool) {
    let f = setup(&pool).await;
    let other = setup(&pool).await;
    grant(&pool, &f, Permission::UsageRead, true).await;
    success(&pool, &f, "alpha").await;
    success(&pool, &f, "beta").await;
    let foreign = success(&pool, &other, "alpha").await;
    let mut q = query();
    q["limit"] = json!(1);
    let (status, first) = call(&pool, &f, REQUESTS, &q).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(first["data"].as_array().unwrap().len(), 1);
    q["before"] = first["next_before"].clone();
    let (_, second) = call(&pool, &f, REQUESTS, &q).await;
    assert_ne!(first["data"][0]["id"], second["data"][0]["id"]);
    assert!(second["next_before"].is_null());
    for (key, value) in [
        ("key_id", json!(other.who.key_id)),
        ("request_id", json!(foreign)),
        ("model_id", json!("alpha' OR true --")),
    ] {
        let mut q = query();
        q["filter"][key] = value;
        let (_, r) = call(&pool, &f, REQUESTS, &q).await;
        assert_eq!(r["data"], json!([]));
    }
    let mut q = query();
    q["dimension"] = json!("model");
    q["limit"] = json!(1);
    let (status, r) = call(&pool, &f, GROUPS, &q).await;
    assert_eq!(status, StatusCode::OK, "{r}");
    assert_eq!(r["data"][0]["object_id"], "alpha");
    assert_eq!(r["next_after"], "alpha");
    q["after"] = r["next_after"].clone();
    let (_, r) = call(&pool, &f, GROUPS, &q).await;
    assert_eq!(r["data"][0]["object_id"], "beta");
    assert!(r["next_after"].is_null());
    q["dimension"] = json!("key");
    q.as_object_mut().unwrap().remove("after");
    let (status, r) = call(&pool, &f, GROUPS, &q).await;
    assert_eq!(status, StatusCode::OK, "{r}");
    assert_eq!(r["data"][0]["totals"]["requests"], "2");
    assert_eq!(r["data"][0]["label"], "test alias");
}

#[sqlx::test(migrations = "./migrations")]
async fn time_windows_are_half_open_empty_days_zero_filled_and_invalid_queries_rejected(
    pool: PgPool,
) {
    let f = setup(&pool).await;
    grant(&pool, &f, Permission::UsageRead, true).await;
    let id = success(&pool, &f, "alpha").await;
    let at: DateTime<Utc> = sqlx::query_scalar("SELECT created_at FROM requests WHERE id=$1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    for (start, end, expected) in [
        (at, at + Duration::microseconds(1), "1"),
        (at - Duration::seconds(1), at, "0"),
    ] {
        let (_, r) = call(
            &pool,
            &f,
            USAGE,
            &json!({"filter":{"start":start,"end":end},"timezone":"UTC"}),
        )
        .await;
        assert_eq!(r["totals"]["requests"], expected);
    }
    for (zone, date) in [("UTC", "2026-01-01"), ("Asia/Shanghai", "2026-01-02")] {
        let (status,r)=call(&pool,&f,USAGE,&json!({"filter":{"start":"2026-01-01T18:00:00Z","end":"2026-01-01T19:00:00Z"},"timezone":zone})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(r["daily"][0]["date"], date);
        assert_eq!(r["daily"][0]["totals"]["known_cost_cny"], "0.000000");
        assert!(r["totals"]["mean_known_request_cost_cny"].is_null());
    }
    for q in [
        json!({"filter":{"start":at,"end":at}}),
        json!({"filter":{"start":at,"end":at+Duration::days(32)}}),
        json!({"filter":filter(),"timezone":"unknown"}),
        json!({"filter":filter(),"tenant_id":Uuid::new_v4()}),
    ] {
        assert_eq!(call(&pool, &f, USAGE, &q).await.0, StatusCode::BAD_REQUEST);
    }
    let mut q = query();
    q["limit"] = json!(101);
    assert_eq!(
        call(&pool, &f, REQUESTS, &q).await.0,
        StatusCode::BAD_REQUEST
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn audit_is_append_only_metadata_projection_with_roundtrippable_cursor(pool: PgPool) {
    let f = setup(&pool).await;
    grant(&pool, &f, Permission::AuditRead, true).await;
    for n in 0..3 {
        sqlx::query("INSERT INTO audit_events(tenant_id,action,object_id,details) VALUES($1,'fixture.sensitive',$2,$3)").bind(f.who.tenant_id).bind(n.to_string()).bind(json!({"secret":"DO_NOT_EXPOSE","payload":{"prompt":"private"}})).execute(&pool).await.unwrap();
    }
    let mut q = filter();
    q["action"] = json!("fixture.sensitive");
    q["limit"] = json!(2);
    let (status, first) = call(&pool, &f, AUDIT, &q).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(first["data"].as_array().unwrap().len(), 2);
    assert!(!first.to_string().contains("DO_NOT_EXPOSE"));
    assert!(!first.to_string().contains("details"));
    q["before"] = first["next_before"].clone();
    let (status, last) = call(&pool, &f, AUDIT, &q).await;
    assert_eq!(status, StatusCode::OK, "{last}");
    assert_eq!(last["data"].as_array().unwrap().len(), 1);
    assert!(last["next_before"].is_null());
    assert_eq!(
        call(&pool, &f, REQUESTS, &query()).await.0,
        StatusCode::FORBIDDEN
    );
    assert!(
        sqlx::query("DELETE FROM audit_events")
            .execute(&pool)
            .await
            .is_err()
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn permission_changes_are_idempotent_audited_and_rollback_on_audit_failure(pool: PgPool) {
    let f = setup(&pool).await;
    grant(&pool, &f, Permission::UsageRead, true).await;
    grant(&pool, &f, Permission::UsageRead, true).await;
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events WHERE action='console.permission_changed'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1);
    sqlx::raw_sql("CREATE FUNCTION console_test_audit_fail() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.action='console.permission_changed' THEN RAISE EXCEPTION 'injected'; END IF; RETURN NEW; END $$; CREATE TRIGGER console_test_fail BEFORE INSERT ON audit_events FOR EACH ROW EXECUTE FUNCTION console_test_audit_fail();").execute(&pool).await.unwrap();
    assert!(
        console::set_permission(
            &pool,
            f.who.tenant_id,
            f.user,
            Permission::UsageRead,
            false,
            "rollback test"
        )
        .await
        .is_err()
    );
    assert_eq!(call(&pool, &f, USAGE, &query()).await.0, StatusCode::OK);
    assert!(
        console::set_permission(
            &pool,
            f.who.tenant_id,
            f.user,
            Permission::UsageRead,
            true,
            ""
        )
        .await
        .is_err()
    );
    let other = setup(&pool).await;
    assert!(matches!(
        console::set_permission(
            &pool,
            f.who.tenant_id,
            other.user,
            Permission::UsageRead,
            true,
            "wrong tenant"
        )
        .await,
        Err(Error::NotFound)
    ));
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
async fn console_database_identity_can_only_read_projections_not_secrets_or_mutate(pool: PgPool) {
    sqlx::raw_sql(include_str!("../deploy/permissions.sql"))
        .execute(&pool)
        .await
        .unwrap();
    let f = setup(&pool).await;
    success(&pool, &f, "alpha").await;
    let operator = scoped(&pool, "brokerrouter_operator").await;
    grant(&operator, &f, Permission::UsageRead, true).await;
    grant(&operator, &f, Permission::AuditRead, true).await;
    let reader = scoped(&pool, "brokerrouter_console").await;
    brokerrouter::check_schema(&reader).await.unwrap();
    for (path, q) in [
        (USAGE, query()),
        (REQUESTS, query()),
        (GROUPS, json!({"filter":filter(),"dimension":"key"})),
        (AUDIT, filter()),
    ] {
        let (status, r) = call(&reader, &f, path, &q).await;
        assert_eq!(status, StatusCode::OK, "{r}");
    }
    for sql in [
        "SELECT key_hash FROM virtual_keys",
        "SELECT result_ciphertext FROM requests",
        "SELECT details FROM ledger_entries",
        "SELECT details FROM audit_events",
        "SELECT credential_env FROM endpoints",
        "UPDATE tenants SET budget_micros=0",
        "UPDATE console_permissions SET enabled=true",
        "DELETE FROM console_permissions",
        "INSERT INTO audit_events(tenant_id,action,object_id) SELECT id,'forged','x' FROM tenants",
        "SET ROLE brokerrouter_runtime",
        "SET ROLE brokerrouter_governance_control",
        "SET ROLE brokerrouter_operator",
    ] {
        assert!(sqlx::query(sql).execute(&reader).await.is_err(), "{sql}");
    }
    assert!(
        console::set_permission(
            &reader,
            f.who.tenant_id,
            f.user,
            Permission::AuditRead,
            false,
            "forbidden"
        )
        .await
        .is_err()
    );
    reader.close().await;
    operator.close().await;
}

#[sqlx::test(migrations = "./migrations")]
async fn permission_revoked_while_report_waits_is_rechecked_before_release(pool: PgPool) {
    let f = setup(&pool).await;
    grant(&pool, &f, Permission::UsageRead, true).await;
    let q = query();
    let token = console_sign(claims(f.who.tenant_id, "alice", USAGE, &q));
    let mut lock = pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE requests IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *lock)
        .await
        .unwrap();
    let app = app(&pool);
    let task = tokio::spawn(async move { send(&app, &token, USAGE, &q).await });
    let mut waiting = false;
    for _ in 0..100 {
        waiting=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND pid<>pg_backend_pid() AND wait_event_type='Lock' AND query LIKE 'SELECT count(*) AS requests%')").fetch_one(&pool).await.unwrap();
        if waiting {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(waiting, "report did not reach controlled wait");
    grant(&pool, &f, Permission::UsageRead, false).await;
    lock.rollback().await.unwrap();
    assert_eq!(task.await.unwrap().0, StatusCode::FORBIDDEN);
}

#[sqlx::test(migrations = "./migrations")]
async fn large_integer_amounts_survive_reports_without_javascript_precision_loss(pool: PgPool) {
    let f = setup(&pool).await;
    grant(&pool, &f, Permission::UsageRead, true).await;
    sqlx::query("UPDATE tenants SET budget_micros=1000000000000000000 WHERE id=$1")
        .bind(f.who.tenant_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE virtual_keys SET budget_micros=1000000000000000000 WHERE id=$1")
        .bind(f.who.key_id)
        .execute(&pool)
        .await
        .unwrap();
    let amount = 9_007_199_254_740_993_i64;
    let id = accounting::reserve(
        &pool,
        Admission {
            identity: &f.who,
            model: "alpha",
            idempotency_key: "large-integer",
            request_hash: &store::hash(b"large"),
            maximum_micros: amount,
            snapshot: PriceSnapshot {
                currency: "CNY".into(),
                input_per_million: Decimal::ONE,
                output_per_million: Decimal::ONE,
                cny_per_unit: Decimal::ONE,
                fx_version: None,
            },
            routing: None,
            timeout_seconds: 60,
            governance_turn_id: None,
        },
    )
    .await
    .unwrap()
    .0
    .id;
    accounting::mark_submitting(&pool, &f.who, id)
        .await
        .unwrap();
    accounting::settle_usage(&pool, &f.who, id, amount as u64, 0, None)
        .await
        .unwrap();
    let (status, r) = call(&pool, &f, USAGE, &query()).await;
    assert_eq!(status, StatusCode::OK, "{r}");
    assert_eq!(r["totals"]["known_cost_cny"], "9007199254.740993");
    assert_eq!(r["totals"]["input_tokens"], "9007199254740993");
    let (_, r) = call(&pool, &f, REQUESTS, &query()).await;
    assert_eq!(r["data"][0]["cost_cny"], "9007199254.740993");
}

#[sqlx::test(migrations = "./migrations")]
async fn real_http_console_enforces_body_limit_no_cors_and_revoked_user(pool: PgPool) {
    let f = setup(&pool).await;
    grant(&pool, &f, Permission::UsageRead, true).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = app(&pool);
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap();
    let q = query();
    let token = console_sign(claims(f.who.tenant_id, "alice", USAGE, &q));
    let url = format!("http://{addr}{USAGE}");
    let response = client
        .post(&url)
        .bearer_auth(&token)
        .header("origin", "https://untrusted.example")
        .json(&q)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response
            .headers()
            .get("access-control-allow-origin")
            .is_none()
    );
    assert_eq!(response.headers()["cache-control"], "no-store");
    let response = client
        .post(&url)
        .bearer_auth(&token)
        .body("x".repeat(16385))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    sqlx::query("UPDATE governance_users SET active=false WHERE tenant_id=$1 AND id=$2")
        .bind(f.who.tenant_id)
        .bind(f.user)
        .execute(&pool)
        .await
        .unwrap();
    let response = client
        .post(&url)
        .bearer_auth(&token)
        .json(&q)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    server.abort();
    let _ = server.await;
}
