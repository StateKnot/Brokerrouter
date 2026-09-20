use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use brokerrouter::{
    accounting::{self, Admission},
    api,
    error::Error,
    money::{self, PriceSnapshot},
    store,
};
use chrono::{Duration, Utc};
use rust_decimal::Decimal;
use serde_json::json;
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

fn personal_profile(name: &str) -> store::PersonalProfile {
    serde_json::from_value(json!({
        "profile_name":name,
        "tenant_budget_cny":"20",
        "key_budget_cny":"10",
        "key_ttl_hours":24,
        "model_id":"jiaclaw/qwen",
        "model_name":"JiaClaw Qwen",
        "endpoint":{
            "name":"local-compatible-provider",
            "upstream_model":"qwen-fixture",
            "base_url":"http://127.0.0.1:18080/v1",
            "credential_env":"JIACLAW_PROVIDER_KEY",
            "allow_private":true,
            "input_per_million_cny":"0.3",
            "output_per_million_cny":"0.6",
            "context_tokens":4096,
            "max_output_tokens":1024,
            "supports_tools":true,
            "supports_streaming":true
        }
    }))
    .unwrap()
}

#[sqlx::test(migrations = "./migrations")]
async fn personal_profile_is_atomic_bounded_and_one_time(pool: PgPool) {
    let created = store::init_personal(&pool, personal_profile("developer"))
        .await
        .unwrap();
    let identity = store::authenticate(&pool, &created.key).await.unwrap();
    assert_eq!(identity.tenant_id, created.tenant_id);
    assert_eq!(identity.key_id, created.key_id);
    assert!(created.expires_at > Utc::now());
    let endpoints = store::endpoints(&pool, &identity, &created.model_id, 1024)
        .await
        .unwrap();
    assert_eq!(endpoints.len(), 1);
    assert!(endpoints[0].supports_tools);
    assert!(endpoints[0].supports_streaming);
    assert_eq!(endpoints[0].id, created.endpoint_id);
    assert!(matches!(
        store::init_personal(&pool, personal_profile("developer")).await,
        Err(Error::PersonalProfileExists)
    ));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM tenants")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM audit_events WHERE tenant_id=$1",)
            .bind(created.tenant_id)
            .fetch_one(&pool)
            .await
            .unwrap(),
        4
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn personal_profile_rolls_back_every_resource_when_audit_fails(pool: PgPool) {
    sqlx::query("CREATE FUNCTION reject_personal_audit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.details->>'actor'='init_personal' THEN RAISE EXCEPTION 'fixture'; END IF; RETURN NEW; END $$")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("CREATE TRIGGER reject_personal_audit BEFORE INSERT ON audit_events FOR EACH ROW EXECUTE FUNCTION reject_personal_audit()")
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        store::init_personal(&pool, personal_profile("rollback"))
            .await
            .is_err()
    );
    for table in [
        "tenants",
        "models",
        "endpoints",
        "virtual_keys",
        "key_models",
    ] {
        let count: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0, "{table}");
    }
}

async fn fixture(pool: &PgPool, budget: i64) -> (store::Identity, String) {
    let tenant = store::create_tenant(pool, "test", budget).await.unwrap();
    store::put_model(pool, tenant, "test/model", "Model")
        .await
        .unwrap();
    let (key_id, key) =
        store::create_key(pool, tenant, "test", budget, &["test/model".into()], None)
            .await
            .unwrap();
    (
        store::Identity {
            tenant_id: tenant,
            key_id,
        },
        key,
    )
}

#[sqlx::test(migrations = "./migrations")]
async fn tenant_isolation_key_revocation_and_fx_history(pool: PgPool) {
    let (a, key_a) = fixture(&pool, 10_000_000).await;
    let (b, key_b) = fixture(&pool, 10_000_000).await;
    let now = Utc::now();
    let version = store::set_fx(
        &pool,
        a.tenant_id,
        money::nonnegative("7.123456789").unwrap(),
        now - Duration::seconds(2),
        "manual",
    )
    .await
    .unwrap();
    assert!(
        store::current_fx(&pool, b.tenant_id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store::set_fx(&pool, a.tenant_id, Decimal::ZERO, now, "manual")
            .await
            .is_err()
    );
    let mut endpoint = store::Endpoint {
        id: Uuid::new_v4(),
        tenant_id: a.tenant_id,
        model_id: "test/model".into(),
        name: "upstream".into(),
        upstream_model: "upstream-model".into(),
        base_url: "https://example.invalid/v1".into(),
        credential_env: "TEST_PROVIDER_KEY".into(),
        allow_private: false,
        currency: "USD".into(),
        input_per_million: money::nonnegative("0.1").unwrap(),
        output_per_million: Decimal::ONE,
        context_tokens: 1000,
        max_output_tokens: 100,
        priority: 10,
        operation: "chat".into(),
        supports_tools: false,
        supports_streaming: false,
        embedding_dimension: None,
    };
    store::add_endpoint(&pool, &endpoint).await.unwrap();
    let snapshot = endpoint.price(&pool).await.unwrap();
    store::set_fx(
        &pool,
        a.tenant_id,
        Decimal::from(8),
        now - Duration::seconds(1),
        "manual",
    )
    .await
    .unwrap();
    store::set_fx(
        &pool,
        a.tenant_id,
        Decimal::from(9),
        now + Duration::days(1),
        "scheduled",
    )
    .await
    .unwrap();
    assert_eq!(snapshot.fx_version, Some(version));
    assert_eq!(snapshot.cost(1_000_000, 0).unwrap(), 712_346);
    assert_eq!(
        endpoint
            .price(&pool)
            .await
            .unwrap()
            .cost(1_000_000, 0)
            .unwrap(),
        800_000
    );
    endpoint.tenant_id = b.tenant_id;
    endpoint.id = Uuid::new_v4();
    assert!(matches!(endpoint.price(&pool).await, Err(Error::MissingFx)));
    endpoint.currency = "CNY".into();
    assert_eq!(
        endpoint
            .price(&pool)
            .await
            .unwrap()
            .cost(1_000_000, 0)
            .unwrap(),
        100_000
    );
    assert!(
        store::endpoints(&pool, &b, "test/model", 100)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store::endpoints(&pool, &a, "test/model", 100)
            .await
            .unwrap()
            .len(),
        1
    );
    let app = api::router(pool.clone());
    let request = |key: &str| {
        Request::builder()
            .uri("/v1/models")
            .header("authorization", format!("Bearer {key}"))
            .body(Body::empty())
            .unwrap()
    };
    let response = app.clone().oneshot(request(&key_b)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = http_body_util::BodyExt::collect(response.into_body())
        .await
        .unwrap()
        .to_bytes();
    assert!(
        serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["data"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    store::revoke_key(&pool, a.tenant_id, a.key_id)
        .await
        .unwrap();
    assert_eq!(
        app.oneshot(request(&key_a)).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    assert!(
        sqlx::query("UPDATE fx_rates SET usd_cny=1 WHERE id=$1")
            .bind(version)
            .execute(&pool)
            .await
            .is_err()
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn concurrent_budget_idempotency_and_uncertain_submission(pool: PgPool) {
    let (who, _) = fixture(&pool, 1_000_000).await;
    let digest = store::hash(b"request");
    let snapshot = PriceSnapshot {
        currency: "CNY".into(),
        input_per_million: Decimal::ONE,
        output_per_million: Decimal::ONE,
        cny_per_unit: Decimal::ONE,
        fx_version: None,
    };
    let make = |key| Admission {
        identity: &who,
        model: "test/model",
        idempotency_key: key,
        request_hash: &digest,
        maximum_micros: 700_000,
        snapshot: snapshot.clone(),
        routing: None,
        timeout_seconds: 120,
        governance_turn_id: None,
    };
    let (a, b) = tokio::join!(
        accounting::reserve(&pool, make("a")),
        accounting::reserve(&pool, make("b"))
    );
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
    let row = a.or(b).unwrap().0;
    let key: String = sqlx::query_scalar("SELECT idempotency_key FROM requests WHERE id=$1")
        .bind(row.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let (repeated, created) = accounting::reserve(&pool, make(&key)).await.unwrap();
    assert!(!created);
    assert_eq!(repeated.id, row.id);
    let bad = store::hash(b"different");
    assert!(matches!(
        accounting::reserve(
            &pool,
            Admission {
                request_hash: &bad,
                ..make(&key)
            }
        )
        .await,
        Err(Error::Conflict)
    ));
    accounting::mark_submitting(&pool, &who, row.id)
        .await
        .unwrap();
    assert!(
        accounting::mark_submitting(&pool, &who, row.id)
            .await
            .is_err()
    );
    accounting::mark_unknown(&pool, &who, row.id, "upstream_timeout")
        .await
        .unwrap();
    assert!(
        accounting::release_unsubmitted(&pool, &who, row.id)
            .await
            .is_err()
    );
    let reserved: i64 = sqlx::query_scalar("SELECT reserved_micros FROM tenants WHERE id=$1")
        .bind(who.tenant_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(reserved, 700_000);
    accounting::finish(
        &pool,
        &who,
        row.id,
        250_000,
        None,
        json!({"usage":"verified"}),
    )
    .await
    .unwrap();
    accounting::finish(&pool, &who, row.id, 250_000, None, json!({}))
        .await
        .unwrap();
    let totals: (i64, i64) =
        sqlx::query_as("SELECT spent_micros,reserved_micros FROM tenants WHERE id=$1")
            .bind(who.tenant_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(totals, (250_000, 0));
    let entries: i64 =
        sqlx::query_scalar("SELECT count(*) FROM ledger_entries WHERE request_id=$1")
            .bind(row.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(entries, 3);
    assert!(
        accounting::finish(&pool, &who, row.id, 350_000, None, json!({}))
            .await
            .is_err()
    );
    let (foreign, _) = fixture(&pool, 1_000_000).await;
    assert!(matches!(
        accounting::get(&pool, &foreign, row.id).await,
        Err(Error::NotFound)
    ));
    assert!(
        sqlx::query("DELETE FROM ledger_entries WHERE request_id=$1")
            .bind(row.id)
            .execute(&pool)
            .await
            .is_err()
    );
    // Snapshot is data, not a reference to mutable current FX configuration.
    let p = PriceSnapshot {
        currency: "CNY".into(),
        input_per_million: Decimal::ONE,
        output_per_million: Decimal::ONE,
        cny_per_unit: Decimal::ONE,
        fx_version: None,
    };
    assert_eq!(p.cost(10, 10).unwrap(), 20);
}

#[sqlx::test(migrations = "./migrations")]
async fn cny_ledger_keeps_original_usd_and_immutable_fx_snapshot(pool: PgPool) {
    let (who, _) = fixture(&pool, 10_000_000).await;
    let now = Utc::now();
    let rate = money::nonnegative("7.123456789").unwrap();
    let version = store::set_fx(
        &pool,
        who.tenant_id,
        rate,
        now - Duration::seconds(2),
        "manual",
    )
    .await
    .unwrap();
    let price = PriceSnapshot {
        currency: "USD".into(),
        input_per_million: money::nonnegative("0.1").unwrap(),
        output_per_million: Decimal::ZERO,
        cny_per_unit: rate,
        fx_version: Some(version),
    };
    let digest = store::hash(b"usd request");
    let make = |snapshot| Admission {
        identity: &who,
        model: "test/model",
        idempotency_key: "usd-test",
        request_hash: &digest,
        maximum_micros: 1_000_000,
        snapshot,
        routing: None,
        timeout_seconds: 120,
        governance_turn_id: None,
    };
    let (other, _) = fixture(&pool, 10_000_000).await;
    let foreign_version = store::set_fx(&pool, other.tenant_id, rate, now, "manual")
        .await
        .unwrap();
    assert!(
        accounting::reserve(
            &pool,
            make(PriceSnapshot {
                fx_version: Some(foreign_version),
                ..price.clone()
            })
        )
        .await
        .is_err()
    );
    let (row, created) = accounting::reserve(&pool, make(price.clone()))
        .await
        .unwrap();
    assert!(created);
    let second = store::set_fx(
        &pool,
        who.tenant_id,
        Decimal::from(8),
        now - Duration::seconds(1),
        "manual",
    )
    .await
    .unwrap();
    let future = store::set_fx(
        &pool,
        who.tenant_id,
        Decimal::from(9),
        now + Duration::days(1),
        "scheduled",
    )
    .await
    .unwrap();
    assert_eq!(
        store::current_fx(&pool, who.tenant_id)
            .await
            .unwrap()
            .unwrap()
            .id,
        second
    );
    let page = store::fx_history(&pool, who.tenant_id, None, 2)
        .await
        .unwrap();
    assert_eq!(
        page.iter().map(|fx| fx.id).collect::<Vec<_>>(),
        vec![future, second]
    );
    let older = store::fx_history(&pool, who.tenant_id, Some(second), 2)
        .await
        .unwrap();
    assert_eq!(older.len(), 1);
    assert_eq!(older[0].id, version);
    assert!(
        store::fx_history(&pool, who.tenant_id, None, 101)
            .await
            .is_err()
    );
    assert!(
        store::set_fx(&pool, who.tenant_id, rate, now, " ")
            .await
            .is_err()
    );
    assert!(
        sqlx::query("UPDATE requests SET price_snapshot='{}' WHERE id=$1")
            .bind(row.id)
            .execute(&pool)
            .await
            .is_err()
    );
    accounting::mark_submitting(&pool, &who, row.id)
        .await
        .unwrap();
    // Revocation stops new admission, not settlement of work already dispatched.
    store::revoke_key(&pool, who.tenant_id, who.key_id)
        .await
        .unwrap();
    assert!(matches!(
        accounting::reserve(&pool, make(price)).await,
        Err(Error::Unauthorized)
    ));
    accounting::settle_usage(&pool, &who, row.id, 1_000_000, 0, None)
        .await
        .unwrap();
    let final_row = accounting::get(&pool, &who, row.id).await.unwrap();
    assert_eq!(final_row.final_micros, Some(712_346));
    assert_eq!(final_row.price_snapshot["fx_version"], json!(version));
    let details: serde_json::Value = sqlx::query_scalar(
        "SELECT details FROM ledger_entries WHERE request_id=$1 AND kind='finalize'",
    )
    .bind(row.id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(details["cost"]["original_currency"], "USD");
    assert_eq!(
        details["cost"]["original_amount"]
            .as_str()
            .unwrap()
            .parse::<Decimal>()
            .unwrap(),
        money::nonnegative("0.1").unwrap()
    );
    assert_eq!(
        details["cost"]["cny_exact"]
            .as_str()
            .unwrap()
            .parse::<Decimal>()
            .unwrap(),
        money::nonnegative("0.7123456789").unwrap()
    );
    assert_eq!(details["cost"]["cny_micros"], 712_346);
    assert!(
        sqlx::query("TRUNCATE fx_rates")
            .execute(&pool)
            .await
            .is_err()
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn readiness_detects_schema_drift_and_generates_request_ids(pool: PgPool) {
    let app = api::router(pool.clone());
    let request = || {
        Request::builder()
            .uri("/health/ready")
            .header("x-request-id", "untrusted")
            .body(Body::empty())
            .unwrap()
    };
    let ready = app.clone().oneshot(request()).await.unwrap();
    assert_eq!(ready.status(), StatusCode::NO_CONTENT);
    assert!(Uuid::parse_str(ready.headers()["x-request-id"].to_str().unwrap()).is_ok());
    sqlx::query("UPDATE _sqlx_migrations SET checksum='\\x00'")
        .execute(&pool)
        .await
        .unwrap();
    let not_ready = app.oneshot(request()).await.unwrap();
    assert_eq!(not_ready.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(not_ready.headers()["x-should-retry"], "false");
}

#[sqlx::test(migrations = "./migrations")]
async fn key_budget_failure_rolls_back_tenant_and_revoked_model_cannot_dispatch(pool: PgPool) {
    let (who, _) = fixture(&pool, 1_000_000).await;
    sqlx::query("UPDATE virtual_keys SET budget_micros=100 WHERE id=$1")
        .bind(who.key_id)
        .execute(&pool)
        .await
        .unwrap();
    let price = PriceSnapshot {
        currency: "CNY".into(),
        input_per_million: Decimal::ONE,
        output_per_million: Decimal::ONE,
        cny_per_unit: Decimal::ONE,
        fx_version: None,
    };
    let digest = store::hash(b"budget-test");
    let make = |maximum| Admission {
        identity: &who,
        model: "test/model",
        idempotency_key: "budget-test",
        request_hash: &digest,
        maximum_micros: maximum,
        snapshot: price.clone(),
        routing: None,
        timeout_seconds: 120,
        governance_turn_id: None,
    };
    assert!(matches!(
        accounting::reserve(&pool, make(101)).await,
        Err(Error::Budget)
    ));
    let held: i64 = sqlx::query_scalar("SELECT reserved_micros FROM tenants WHERE id=$1")
        .bind(who.tenant_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(held, 0);
    let (request, _) = accounting::reserve(&pool, make(100)).await.unwrap();
    sqlx::query("UPDATE models SET active=false WHERE tenant_id=$1")
        .bind(who.tenant_id)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        accounting::mark_submitting(&pool, &who, request.id)
            .await
            .is_err()
    );
    accounting::release_unsubmitted(&pool, &who, request.id)
        .await
        .unwrap();
    accounting::release_unsubmitted(&pool, &who, request.id)
        .await
        .unwrap();
    let accounts: (i64, i64) = sqlx::query_as("SELECT t.reserved_micros,k.reserved_micros FROM tenants t JOIN virtual_keys k ON k.tenant_id=t.id WHERE k.id=$1").bind(who.key_id).fetch_one(&pool).await.unwrap();
    assert_eq!(accounts, (0, 0));
}

#[sqlx::test(migrations = "./migrations")]
async fn production_database_roles_allow_accounting_but_deny_configuration_and_fact_mutation(
    pool: PgPool,
) {
    sqlx::raw_sql(include_str!("../deploy/permissions.sql"))
        .execute(&pool)
        .await
        .unwrap();
    let runtime = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .after_connect(|connection, _| {
            Box::pin(async move {
                sqlx::query("SET ROLE brokerrouter_runtime")
                    .execute(connection)
                    .await?;
                Ok(())
            })
        })
        .connect_with((*pool.connect_options()).clone())
        .await
        .unwrap();
    let operator = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .after_connect(|connection, _| {
            Box::pin(async move {
                sqlx::query("SET ROLE brokerrouter_operator")
                    .execute(connection)
                    .await?;
                Ok(())
            })
        })
        .connect_with((*pool.connect_options()).clone())
        .await
        .unwrap();
    let (who, key) = fixture(&operator, 1_000_000).await;
    store::set_fx(
        &operator,
        who.tenant_id,
        Decimal::from(7),
        Utc::now(),
        "manual",
    )
    .await
    .unwrap();
    brokerrouter::check_schema(&runtime).await.unwrap();
    assert!(store::authenticate(&runtime, &key).await.is_ok());
    assert!(
        store::set_fx(
            &runtime,
            who.tenant_id,
            Decimal::from(8),
            Utc::now(),
            "forbidden"
        )
        .await
        .is_err()
    );
    let price = PriceSnapshot {
        currency: "CNY".into(),
        input_per_million: Decimal::ONE,
        output_per_million: Decimal::ONE,
        cny_per_unit: Decimal::ONE,
        fx_version: None,
    };
    let digest = store::hash(b"runtime-test");
    let (request, _) = accounting::reserve(
        &runtime,
        Admission {
            identity: &who,
            model: "test/model",
            idempotency_key: "runtime-test",
            request_hash: &digest,
            maximum_micros: 100,
            snapshot: price,
            routing: None,
            timeout_seconds: 120,
            governance_turn_id: None,
        },
    )
    .await
    .unwrap();
    accounting::mark_submitting(&runtime, &who, request.id)
        .await
        .unwrap();
    accounting::settle_usage(&runtime, &who, request.id, 10, 20, None)
        .await
        .unwrap();
    assert_eq!(
        accounting::get(&runtime, &who, request.id)
            .await
            .unwrap()
            .final_micros,
        Some(30)
    );
    for sql in [
        "TRUNCATE ledger_entries",
        "DELETE FROM audit_events",
        "UPDATE fx_rates SET usd_cny=1",
        "ALTER TABLE ledger_entries DISABLE TRIGGER ALL",
        "UPDATE tenants SET budget_micros=10000000",
        "UPDATE _sqlx_migrations SET success=false",
    ] {
        assert!(
            sqlx::query(sql).execute(&runtime).await.is_err(),
            "runtime unexpectedly allowed: {sql}"
        );
    }
    assert!(
        sqlx::query("UPDATE tenants SET spent_micros=0")
            .execute(&operator)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("TRUNCATE fx_rates")
            .execute(&operator)
            .await
            .is_err()
    );
    runtime.close().await;
    operator.close().await;
}
