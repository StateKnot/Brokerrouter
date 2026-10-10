mod common;

use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    routing::post,
};
use brokerrouter::{api, gateway::Gateway, governance, money, runtime_results, store};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{sync::Notify, task::JoinHandle};
use tower::ServiceExt;
use uuid::Uuid;

async fn request(
    app: &Router,
    token: Option<&str>,
    method: &str,
    path: &str,
    body: Value,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let response = app
        .clone()
        .oneshot(
            request
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[sqlx::test(migrations = "./migrations")]
async fn http_release_is_post_only_and_generic_routes_cannot_consume_output_authority(
    pool: PgPool,
) {
    let tenant = store::create_tenant(&pool, "http-results", 1_000_000)
        .await
        .unwrap();
    store::put_model(&pool, tenant, "model", "Model")
        .await
        .unwrap();
    let (key, token) =
        store::create_key(&pool, tenant, "runtime", 1_000_000, &["model".into()], None)
            .await
            .unwrap();
    let who = store::Identity {
        tenant_id: tenant,
        key_id: key,
    };
    common::provision(&pool, &who, "alice").await;
    let turn = common::turn(&pool, &who, "alice", 0).await;
    let gateway = Gateway::new(pool.clone(), common::cipher(), HashMap::new(), 4, 5, 60).unwrap();
    let app = api::gateway_router(gateway.clone());
    let operation = governance::check(
        &pool,
        &common::cipher(),
        &who,
        governance::Check {
            turn_id: turn,
            operation_key: "http-input".into(),
            stage: "tool_input".into(),
            model_id: Some("model".into()),
            tool_name: Some("fixture_tool".into()),
            payload: json!({"argument":"fixture"}),
        },
    )
    .await
    .unwrap();
    let input = serde_json::from_value(operation["id"].clone()).unwrap();
    let claim_token = "11".repeat(32);
    governance::claim(
        &pool,
        &common::cipher(),
        &who,
        input,
        governance::Claim {
            token: claim_token.clone(),
            payload_hash: operation["payload_hash"].as_str().unwrap().into(),
        },
    )
    .await
    .unwrap();
    let body = json!({"input_operation_id":input,"claim_token":claim_token,"evidence_ref":"runtime://http/result","text":"protected fixture output"});
    assert_eq!(
        request(
            &app,
            None,
            "POST",
            "/v1/governance/tool-results",
            body.clone()
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    let (status, recorded) = request(
        &app,
        Some(&token),
        "POST",
        "/v1/governance/tool-results",
        body,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(recorded["state"], "recorded");
    assert!(recorded.get("text").is_none());
    let result: Uuid = serde_json::from_value(recorded["id"].clone()).unwrap();
    let path = format!("/v1/governance/tool-results/{result}/release");
    assert_eq!(
        request(&app, Some(&token), "GET", &path, json!({})).await.0,
        StatusCode::METHOD_NOT_ALLOWED
    );
    assert_eq!(
        request(
            &app,
            Some(&token),
            "POST",
            &path,
            json!({"unexpected":true})
        )
        .await
        .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let (status, released) = request(&app, Some(&token), "POST", &path, json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(released["text"], "protected fixture output");
    assert_eq!(released["result_id"], result.to_string());
    let output = released["output_operation_id"].as_str().unwrap();
    let (status, metadata) = request(
        &app,
        Some(&token),
        "GET",
        &format!("/v1/governance/operations/{output}"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(metadata.get("text").is_none());
    for (suffix, body) in [
        (
            "claim",
            json!({"token":"22".repeat(32),"payload_hash":released["payload_hash"]}),
        ),
        (
            "receipt",
            json!({"token":"22".repeat(32),"outcome":"completed","evidence_ref":"runtime://misuse"}),
        ),
    ] {
        assert_eq!(
            request(
                &app,
                Some(&token),
                "POST",
                &format!("/v1/governance/operations/{output}/{suffix}"),
                body
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    }
    let reserved = json!({"turn_id":turn,"operation_key":format!("runtime-result-output:{result}"),"stage":"tool_output","model_id":"model","tool_name":"fixture_tool","payload":{"changed":true}});
    assert_eq!(
        request(
            &app,
            Some(&token),
            "POST",
            "/v1/governance/checks",
            reserved
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let facts: (String, bool, bool, i64) = sqlx::query_as("SELECT state,claim_hash IS NULL,receipt_hash IS NULL,(SELECT count(*) FROM governance_operations) FROM governance_operations WHERE tenant_id=$1 AND id=$2")
        .bind(tenant).bind(Uuid::parse_str(output).unwrap()).fetch_one(&pool).await.unwrap();
    assert_eq!(facts, ("ready".into(), true, true, 2));
    assert_eq!(
        request(&app, Some(&token), "POST", &path, json!({}))
            .await
            .1["text"],
        "protected fixture output"
    );
    let (_, other_token) =
        store::create_key(&pool, tenant, "other", 1_000_000, &["model".into()], None)
            .await
            .unwrap();
    assert_eq!(
        request(&app, Some(&other_token), "POST", &path, json!({}))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    let results: i64 = sqlx::query_scalar("SELECT count(*) FROM runtime_tool_results")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(results, 1);
    sqlx::query(
        "UPDATE virtual_keys SET revoked_at=clock_timestamp() WHERE tenant_id=$1 AND id=$2",
    )
    .bind(tenant)
    .bind(key)
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        request(&app, Some(&token), "POST", &path, json!({}))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    gateway.drain().await;
}

#[sqlx::test(migrations = "./migrations")]
async fn record_rejects_client_chosen_authority_and_never_creates_a_result_without_a_claim(
    pool: PgPool,
) {
    let tenant = store::create_tenant(&pool, "http-invalid-result", 1_000_000)
        .await
        .unwrap();
    store::put_model(&pool, tenant, "model", "Model")
        .await
        .unwrap();
    let (_, token) =
        store::create_key(&pool, tenant, "runtime", 1_000_000, &["model".into()], None)
            .await
            .unwrap();
    let gateway = Gateway::new(pool.clone(), common::cipher(), HashMap::new(), 4, 5, 60).unwrap();
    let app = api::gateway_router(gateway.clone());
    let mut body = json!({"input_operation_id":Uuid::new_v4(),"claim_token":"11".repeat(32),"evidence_ref":"runtime://invalid","text":"fixture"});
    body["tenant_id"] = json!(Uuid::new_v4());
    assert_eq!(
        request(
            &app,
            Some(&token),
            "POST",
            "/v1/governance/tool-results",
            body.clone()
        )
        .await
        .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    body.as_object_mut().unwrap().remove("tenant_id");
    assert_eq!(
        request(
            &app,
            Some(&token),
            "POST",
            "/v1/governance/tool-results",
            body
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    let results: i64 = sqlx::query_scalar("SELECT count(*) FROM runtime_tool_results")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(results, 0);
    runtime_results::maintain(&pool).await.unwrap();
    gateway.drain().await;
}

#[derive(Clone, Default)]
struct HeldJudge {
    calls: Arc<AtomicUsize>,
    started: Arc<Notify>,
    resume: Arc<Notify>,
}

struct ReleaseFixture {
    gateway: Gateway,
    who: store::Identity,
    token: String,
    path: String,
    judge: HeldJudge,
    server: JoinHandle<()>,
}

impl Drop for ReleaseFixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl ReleaseFixture {
    async fn started(&self) {
        tokio::time::timeout(Duration::from_secs(10), self.judge.started.notified())
            .await
            .unwrap();
    }

    fn release(&self) -> JoinHandle<(StatusCode, Value)> {
        let app = api::gateway_router(self.gateway.clone());
        let token = self.token.clone();
        let path = self.path.clone();
        tokio::spawn(async move { request(&app, Some(&token), "POST", &path, json!({})).await })
    }

    async fn settled_once(&self, pool: &PgPool) {
        self.gateway.drain().await;
        let facts: (i64, i64, i64, i64, i64, i64) = sqlx::query_as(
            "SELECT (SELECT count(*) FROM requests WHERE status='succeeded'),(SELECT sum(final_micros)::bigint FROM requests),(SELECT count(*) FROM governance_events WHERE event_type='operation.claimed'),(SELECT count(*) FROM governance_events WHERE event_type='operation.receipted'),(SELECT count(*) FROM runtime_tool_results),(SELECT reserved_micros FROM tenants WHERE id=$1)",
        ).bind(self.who.tenant_id).fetch_one(pool).await.unwrap();
        assert_eq!(facts, (1, 7, 1, 1, 1, 0));
        assert_eq!(self.judge.calls.load(Ordering::SeqCst), 1);
    }
}

async fn held_release(pool: &PgPool) -> ReleaseFixture {
    let tenant = store::create_tenant(pool, "held-output", 1_000_000)
        .await
        .unwrap();
    store::put_model(pool, tenant, "model", "Model")
        .await
        .unwrap();
    let (key_id, token) =
        store::create_key(pool, tenant, "runtime", 1_000_000, &["model".into()], None)
            .await
            .unwrap();
    let who = store::Identity {
        tenant_id: tenant,
        key_id,
    };
    common::provision(pool, &who, "alice").await;
    let turn = common::turn(pool, &who, "alice", 0).await;
    let op = governance::check(
        pool,
        &common::cipher(),
        &who,
        governance::Check {
            turn_id: turn,
            operation_key: "held-input".into(),
            stage: "tool_input".into(),
            model_id: Some("model".into()),
            tool_name: Some("fixture_tool".into()),
            payload: json!({"fixture":true}),
        },
    )
    .await
    .unwrap();
    let input = serde_json::from_value(op["id"].clone()).unwrap();
    let claim_token = "11".repeat(32);
    governance::claim(
        pool,
        &common::cipher(),
        &who,
        input,
        governance::Claim {
            token: claim_token.clone(),
            payload_hash: op["payload_hash"].as_str().unwrap().into(),
        },
    )
    .await
    .unwrap();
    let recorded = runtime_results::record(
        pool,
        &common::cipher(),
        &who,
        runtime_results::Record {
            input_operation_id: input,
            claim_token,
            evidence_ref: "runtime://held-result".into(),
            text: "exact private fixture output".into(),
        },
    )
    .await
    .unwrap();
    let result = recorded["id"].as_str().unwrap();
    let judge = HeldJudge::default();
    let provider = Router::new().route("/v1/chat/completions", post(
        |State(judge): State<HeldJudge>, Json(_): Json<Value>| async move {
            judge.calls.fetch_add(1, Ordering::SeqCst);
            judge.started.notify_one();
            judge.resume.notified().await;
            Json(json!({"id":"fixture","object":"chat.completion","created":1,"model":"fixture",
                "choices":[{"index":0,"message":{"role":"assistant","content":"{\"decision\":\"pass\"}"},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":3,"completion_tokens":4,"total_tokens":7}}))
        }
    )).with_state(judge.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, provider).await.unwrap();
    });
    store::add_endpoint(
        pool,
        &store::Endpoint {
            id: Uuid::new_v4(),
            tenant_id: tenant,
            model_id: "model".into(),
            name: "held-judge".into(),
            upstream_model: "fixture".into(),
            base_url,
            credential_env: "FIXTURE_KEY".into(),
            allow_private: true,
            currency: "CNY".into(),
            input_per_million: money::nonnegative("1").unwrap(),
            output_per_million: money::nonnegative("1").unwrap(),
            context_tokens: 1000,
            max_output_tokens: 100,
            priority: 10,
            operation: "chat".into(),
            supports_tools: false,
            supports_streaming: false,
            embedding_dimension: None,
        },
    )
    .await
    .unwrap();
    common::guardrail(pool, tenant, "output-policy", 1, "tool_output", json!({
        "kind":"model_judge","model":"model","instructions":"Check this fixture.","input_format":"text_json",
        "max_input_bytes":65536,"max_output_tokens":32,"maximum_cny":"0.1",
    })).await;
    let gateway = Gateway::new(
        pool.clone(),
        common::cipher(),
        HashMap::from([("FIXTURE_KEY".into(), "fixture-secret".into())]),
        4,
        5,
        60,
    )
    .unwrap();
    ReleaseFixture {
        gateway,
        who,
        token,
        path: format!("/v1/governance/tool-results/{result}/release"),
        judge,
        server,
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn release_disconnect_settles_one_judge_and_recovers_the_frozen_result(pool: PgPool) {
    let fixture = held_release(&pool).await;
    let client = fixture.release();
    fixture.started().await;
    client.abort();
    assert!(client.await.unwrap_err().is_cancelled());
    fixture.judge.resume.notify_one();
    fixture.settled_once(&pool).await;
    let app = api::gateway_router(fixture.gateway.clone());
    let (status, released) =
        request(&app, Some(&fixture.token), "POST", &fixture.path, json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(released["state"], "ready");
    assert_eq!(released["text"], "exact private fixture output");
    fixture.settled_once(&pool).await;
}

#[sqlx::test(migrations = "./migrations")]
async fn release_inflight_policy_upgrade_requires_current_approval_without_reexecuting(
    pool: PgPool,
) {
    let fixture = held_release(&pool).await;
    let task = fixture.release();
    fixture.started().await;
    tokio::time::timeout(
        Duration::from_secs(1),
        common::guardrail(
            &pool,
            fixture.who.tenant_id,
            "output-policy",
            2,
            "tool_output",
            json!({"kind":"always"}),
        ),
    )
    .await
    .unwrap();
    fixture.judge.resume.notify_one();
    let (status, waiting) = task.await.unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(waiting["state"], "waiting_approval");
    assert!(waiting.get("text").is_none());
    common::decide(
        &pool,
        "alice",
        common::approval_id(&waiting),
        common::approve(&["output-policy"]),
    )
    .await
    .unwrap();
    let app = api::gateway_router(fixture.gateway.clone());
    let (status, released) =
        request(&app, Some(&fixture.token), "POST", &fixture.path, json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(released["text"], "exact private fixture output");
    fixture.settled_once(&pool).await;
}

#[sqlx::test(migrations = "./migrations")]
async fn release_inflight_key_revocation_blocks_text_and_preserves_judge_settlement(pool: PgPool) {
    let fixture = held_release(&pool).await;
    let task = fixture.release();
    fixture.started().await;
    sqlx::query(
        "UPDATE virtual_keys SET revoked_at=clock_timestamp() WHERE tenant_id=$1 AND id=$2",
    )
    .bind(fixture.who.tenant_id)
    .bind(fixture.who.key_id)
    .execute(&pool)
    .await
    .unwrap();
    fixture.judge.resume.notify_one();
    let (status, response) = task.await.unwrap();
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(response.get("text").is_none());
    fixture.settled_once(&pool).await;
}
