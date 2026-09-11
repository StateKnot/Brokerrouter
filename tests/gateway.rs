use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::{HeaderMap, Request, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use brokerrouter::{
    api, attempts, cipher::ResultCipher, error::Error, gateway::Gateway, money, store,
};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{net::TcpListener, task::JoinHandle};
use tower::ServiceExt;
use uuid::Uuid;

#[derive(Clone)]
enum Reply {
    Chat,
    Embedding,
    MissingUsage,
    Http(u16),
    Slow,
    Tool,
}
#[derive(Clone)]
struct MockState {
    received: Arc<Mutex<Vec<Value>>>,
    reply: Reply,
}
struct Mock {
    url: String,
    state: MockState,
    task: JoinHandle<()>,
}
impl Drop for Mock {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Mock {
    async fn start(reply: Reply) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let state = MockState {
            received: Arc::new(Mutex::new(Vec::new())),
            reply,
        };
        let app = Router::new()
            .route("/v1/chat/completions", post(mock))
            .route("/v1/embeddings", post(mock))
            .with_state(state.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { url, state, task }
    }
    fn count(&self) -> usize {
        self.state.received.lock().unwrap().len()
    }
}
async fn mock(
    State(state): State<MockState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    state.received.lock().unwrap().push(json!({"body":body,"auth":headers.get("authorization").and_then(|v|v.to_str().ok()),"request_id":headers.get("x-request-id").and_then(|v|v.to_str().ok())}));
    match state.reply {
        Reply::Http(status)=>(StatusCode::from_u16(status).unwrap(),[("location","http://127.0.0.1:9/secret")],"not a billable-success response").into_response(),
        Reply::Embedding=>Json(json!({"object":"list","model":"upstream","data":[{"object":"embedding","index":0,"embedding":[0.1,0.2,0.3]}],"usage":{"prompt_tokens":3,"total_tokens":3}})).into_response(),
        reply=>{
            if matches!(reply,Reply::Slow){tokio::time::sleep(Duration::from_millis(1300)).await;}
            let mut value=json!({"id":"chatcmpl-fixture","object":"chat.completion","created":1,"model":"upstream","choices":[{"index":0,"message":{"role":"assistant","content":"private answer"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":4,"total_tokens":7}});
            if matches!(reply,Reply::MissingUsage){value.as_object_mut().unwrap().remove("usage");}
            if matches!(reply,Reply::Tool){value["choices"][0]["message"]=json!({"role":"assistant","content":null,"tool_calls":[{"id":"call_fixture","type":"function","function":{"name":"lookup","arguments":"{}"}}]});value["choices"][0]["finish_reason"]="tool_calls".into();}
            Json(value).into_response()
        }
    }
}
async fn setup(
    pool: &PgPool,
    operation: &str,
    url: &str,
) -> (store::Identity, String, store::Endpoint) {
    let tenant = store::create_tenant(pool, "gateway-test", 10_000_000)
        .await
        .unwrap();
    store::put_model(pool, tenant, "logical/model", "Logical model")
        .await
        .unwrap();
    let (key_id, key) = store::create_key(
        pool,
        tenant,
        "gateway",
        10_000_000,
        &["logical/model".into()],
        None,
    )
    .await
    .unwrap();
    let endpoint = store::Endpoint {
        id: Uuid::new_v4(),
        tenant_id: tenant,
        model_id: "logical/model".into(),
        name: "provider-a".into(),
        upstream_model: "real-upstream-model".into(),
        base_url: url.into(),
        credential_env: "FIXTURE_PROVIDER_KEY".into(),
        allow_private: true,
        currency: "CNY".into(),
        input_per_million: money::nonnegative("1").unwrap(),
        output_per_million: money::nonnegative("1").unwrap(),
        context_tokens: 1000,
        max_output_tokens: 100,
        priority: 10,
        operation: operation.into(),
        supports_tools: false,
        embedding_dimension: if operation == "embedding" {
            Some(3)
        } else {
            None
        },
    };
    store::add_endpoint(pool, &endpoint).await.unwrap();
    (
        store::Identity {
            tenant_id: tenant,
            key_id,
        },
        key,
        endpoint,
    )
}
fn gateway(pool: PgPool, timeout: i32) -> Gateway {
    Gateway::new(
        pool,
        ResultCipher::new(
            "fixture-v1".into(),
            HashMap::from([("fixture-v1".into(), "11".repeat(32))]),
        )
        .unwrap(),
        HashMap::from([("FIXTURE_PROVIDER_KEY".into(), "fake-provider-secret".into())]),
        8,
        timeout,
        60,
    )
    .unwrap()
}
fn chat() -> Value {
    json!({"model":"logical/model","messages":[{"role":"user","content":"hello"}],"max_tokens":10})
}
fn request(key: &str, idem: &str, body: Value, path: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(path)
        .header("authorization", format!("Bearer {key}"))
        .header("idempotency-key", idem)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}
async fn json_body(response: Response) -> Value {
    serde_json::from_slice(
        &http_body_util::BodyExt::collect(response.into_body())
            .await
            .unwrap()
            .to_bytes(),
    )
    .unwrap()
}

#[sqlx::test(migrations = "./migrations")]
async fn http_chat_replay_encrypted_result_and_scope(pool: PgPool) {
    let upstream = Mock::start(Reply::Chat).await;
    let (who, key, _) = setup(&pool, "chat", &upstream.url).await;
    let g = gateway(pool.clone(), 5);
    let app = api::gateway_router(g.clone());
    let response = app
        .clone()
        .oneshot(request(&key, "once", chat(), "/v1/chat/completions"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let id = Uuid::parse_str(
        response.headers()["x-brokerrouter-request-id"]
            .to_str()
            .unwrap(),
    )
    .unwrap();
    let first = json_body(response).await;
    assert_eq!(first["model"], "logical/model");
    let response = app
        .clone()
        .oneshot(request(&key, "once", chat(), "/v1/chat/completions"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json_body(response).await, first);
    assert_eq!(upstream.count(), 1);
    let recorded = upstream.state.received.lock().unwrap()[0].clone();
    assert_eq!(recorded["auth"], "Bearer fake-provider-secret");
    assert_eq!(recorded["body"]["model"], "real-upstream-model");
    assert!(Uuid::parse_str(recorded["request_id"].as_str().unwrap()).is_ok());
    let ciphertext: Vec<u8> =
        sqlx::query_scalar("SELECT result_ciphertext FROM requests WHERE id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        !ciphertext
            .windows(b"private answer".len())
            .any(|v| v == b"private answer")
    );
    let totals: (i64, i64) =
        sqlx::query_as("SELECT spent_micros,reserved_micros FROM tenants WHERE id=$1")
            .bind(who.tenant_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(totals, (7, 0));
    let mut changed = chat();
    changed["messages"][0]["content"] = "different".into();
    let conflict = app
        .clone()
        .oneshot(request(&key, "once", changed, "/v1/chat/completions"))
        .await
        .unwrap();
    assert_eq!(conflict.status(), StatusCode::CONFLICT);
    assert_eq!(conflict.headers()["x-should-retry"], "false");
    let (foreign, _, _) = setup(&pool, "chat", &upstream.url).await;
    let page = g.logs(&who, None, 1).await.unwrap();
    assert_eq!(page["data"].as_array().unwrap().len(), 1);
    assert_eq!(page["data"][0]["reserved_cny"], "0.000000");
    assert_eq!(page["data"][0]["cost_cny"], "0.000007");
    assert!(
        g.logs(&who, Some(id), 1).await.unwrap()["data"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        g.logs(&foreign, None, 20).await.unwrap()["data"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(matches!(
        g.logs(&foreign, Some(id), 20).await,
        Err(Error::NotFound)
    ));
    let logs = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/requests?limit=1")
                .header("authorization", format!("Bearer {key}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(logs.status(), StatusCode::OK);
    assert_eq!(json_body(logs).await["data"][0]["id"], id.to_string());
    assert!(matches!(g.result(&foreign, id).await, Err(Error::NotFound)));
    sqlx::query("UPDATE requests SET result_expires_at=now()-interval '1 second' WHERE id=$1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        g.result(&who, id).await,
        Err(Error::ResultUnavailable)
    ));
    attempts::maintain(&pool).await.unwrap();
    let absent: bool =
        sqlx::query_scalar("SELECT result_ciphertext IS NULL FROM requests WHERE id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(absent);
    let expired = app
        .oneshot(request(&key, "once", chat(), "/v1/chat/completions"))
        .await
        .unwrap();
    assert_eq!(expired.status(), StatusCode::GONE);
    assert_eq!(upstream.count(), 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn real_http_client_gateway_and_provider_round_trip(pool: PgPool) {
    let upstream = Mock::start(Reply::Chat).await;
    let (_, key, _) = setup(&pool, "chat", &upstream.url).await;
    let g = gateway(pool.clone(), 5);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = api::gateway_router(g.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let client = reqwest::Client::builder()
        .no_proxy()
        .retry(reqwest::retry::never())
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let response = client
        .post(format!("{base}/v1/chat/completions"))
        .bearer_auth(&key)
        .header("idempotency-key", "real-http")
        .json(&chat())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let id = response.headers()["x-brokerrouter-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    let first: Value = response.json().await.unwrap();
    let replay: Value = client
        .get(format!("{base}/v1/requests/{id}/result"))
        .bearer_auth(&key)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(first, replay);
    assert_eq!(upstream.count(), 1);
    let unauthenticated = client
        .get(format!("{base}/v1/requests/{id}/result"))
        .send()
        .await
        .unwrap();
    assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);
    server.abort();
    g.drain().await;
}

#[sqlx::test(migrations = "./migrations")]
async fn connection_failure_falls_back_with_actual_endpoint_price(pool: PgPool) {
    let closed = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead = format!("http://{}/v1", closed.local_addr().unwrap());
    drop(closed);
    let healthy = Mock::start(Reply::Chat).await;
    let (who, _, mut endpoint) = setup(&pool, "chat", &dead).await;
    endpoint.id = Uuid::new_v4();
    endpoint.base_url = healthy.url.clone();
    endpoint.priority = 20;
    endpoint.input_per_million = money::nonnegative("2").unwrap();
    endpoint.output_per_million = money::nonnegative("2").unwrap();
    store::add_endpoint(&pool, &endpoint).await.unwrap();
    let g = gateway(pool.clone(), 5);
    let (id, result) = g.execute(&who, "fallback", chat(), "chat").await.unwrap();
    result.unwrap();
    let detail = g.request(&who, id).await.unwrap();
    assert_eq!(detail["cost_cny"], "0.000014");
    assert_eq!(detail["attempts"].as_array().unwrap().len(), 2);
    assert_eq!(detail["attempts"][0]["status"], "not_sent");
    assert_eq!(detail["attempts"][1]["status"], "succeeded");
    assert_eq!(healthy.count(), 1);
    let ledger: Value = sqlx::query_scalar(
        "SELECT details FROM ledger_entries WHERE request_id=$1 AND kind='finalize'",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(ledger["cost"]["cny_micros"], 14);
}

#[sqlx::test(migrations = "./migrations")]
async fn accepted_timeout_does_not_retry_and_reconciliation_is_atomic(pool: PgPool) {
    let slow = Mock::start(Reply::Slow).await;
    let fallback = Mock::start(Reply::Chat).await;
    let (who, _, mut endpoint) = setup(&pool, "chat", &slow.url).await;
    endpoint.id = Uuid::new_v4();
    endpoint.base_url = fallback.url.clone();
    endpoint.priority = 20;
    store::add_endpoint(&pool, &endpoint).await.unwrap();
    let g = gateway(pool.clone(), 1);
    let (id, result) = g.execute(&who, "timeout", chat(), "chat").await.unwrap();
    assert!(matches!(result, Err(Error::UpstreamUnknown)));
    assert_eq!(slow.count(), 1);
    assert_eq!(fallback.count(), 0);
    let (_, replay) = gateway(pool.clone(), 5)
        .execute(&who, "timeout", chat(), "chat")
        .await
        .unwrap();
    assert!(matches!(replay, Err(Error::UpstreamUnknown)));
    assert_eq!(slow.count(), 1);
    let held: i64 = sqlx::query_scalar("SELECT reserved_micros FROM tenants WHERE id=$1")
        .bind(who.tenant_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(held > 0);
    attempts::reconcile(
        &pool,
        who.tenant_id,
        id,
        Some((3, 4)),
        "fixture-evidence-001",
    )
    .await
    .unwrap();
    attempts::reconcile(
        &pool,
        who.tenant_id,
        id,
        Some((3, 4)),
        "fixture-evidence-001",
    )
    .await
    .unwrap();
    assert!(
        attempts::reconcile(&pool, who.tenant_id, id, None, "conflicting-evidence")
            .await
            .is_err()
    );
    let detail = g.request(&who, id).await.unwrap();
    assert_eq!(detail["status"], "reconciled");
    assert_eq!(detail["cost_cny"], "0.000007");
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM ledger_entries WHERE request_id=$1 AND kind='finalize'",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1);
    let totals: (i64, i64) =
        sqlx::query_as("SELECT spent_micros,reserved_micros FROM tenants WHERE id=$1")
            .bind(who.tenant_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(totals, (7, 0));
    assert!(matches!(
        g.result(&who, id).await,
        Err(Error::ResultUnavailable)
    ));
}

#[sqlx::test(migrations = "./migrations")]
async fn invalid_usage_and_redirects_never_fall_back(pool: PgPool) {
    for reply in [Reply::MissingUsage, Reply::Http(307), Reply::Http(429)] {
        let bad = Mock::start(reply).await;
        let good = Mock::start(Reply::Chat).await;
        let (who, _, mut endpoint) = setup(&pool, "chat", &bad.url).await;
        endpoint.id = Uuid::new_v4();
        endpoint.base_url = good.url.clone();
        endpoint.priority = 20;
        store::add_endpoint(&pool, &endpoint).await.unwrap();
        let g = gateway(pool.clone(), 5);
        let (id, result) = g.execute(&who, "bad", chat(), "chat").await.unwrap();
        assert!(matches!(result, Err(Error::UpstreamUnknown)));
        assert_eq!(good.count(), 0);
        assert_eq!(bad.count(), 1);
        attempts::reconcile(
            &pool,
            who.tenant_id,
            id,
            None,
            "confirmed-not-charged-fixture",
        )
        .await
        .unwrap();
        let held: i64 = sqlx::query_scalar("SELECT reserved_micros FROM tenants WHERE id=$1")
            .bind(who.tenant_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(held, 0);
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn embeddings_capabilities_and_unapproved_networks_fail_before_post(pool: PgPool) {
    let upstream = Mock::start(Reply::Embedding).await;
    let (who, _, mut endpoint) = setup(&pool, "embedding", &upstream.url).await;
    let g = gateway(pool.clone(), 5);
    let request = json!({"model":"logical/model","input":"hello","dimensions":3});
    let (id, result) = g
        .execute(&who, "embed", request.clone(), "embedding")
        .await
        .unwrap();
    assert_eq!(
        result.unwrap()["data"][0]["embedding"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    assert_eq!(g.request(&who, id).await.unwrap()["cost_cny"], "0.000003");
    let mut bad = request.clone();
    bad["dimensions"] = 4.into();
    assert!(matches!(
        g.execute(&who, "wrong-dim", bad, "embedding").await,
        Err(Error::NoEndpoint)
    ));
    let mut unsupported = request;
    unsupported["encoding_format"] = "base64".into();
    assert!(matches!(
        g.execute(&who, "base64", unsupported, "embedding").await,
        Err(Error::Invalid(_))
    ));
    endpoint.id = Uuid::new_v4();
    endpoint.embedding_dimension = Some(4);
    assert!(store::add_endpoint(&pool, &endpoint).await.is_err());
    assert_eq!(upstream.count(), 1);
    let chat_server = Mock::start(Reply::Chat).await;
    let (other, _, endpoint) = setup(&pool, "chat", &chat_server.url).await;
    sqlx::query("UPDATE endpoints SET allow_private=false,base_url=$2 WHERE id=$1")
        .bind(endpoint.id)
        .bind(chat_server.url.replace("http:", "https:"))
        .execute(&pool)
        .await
        .unwrap();
    let (_, denied) = g.execute(&other, "ssrf", chat(), "chat").await.unwrap();
    assert!(matches!(denied, Err(Error::NotSubmitted)));
    assert_eq!(chat_server.count(), 0);
    let mut stream = chat();
    stream["stream"] = true.into();
    assert!(matches!(
        g.execute(&other, "stream", stream, "chat").await,
        Err(Error::Invalid(_))
    ));
}

#[sqlx::test(migrations = "./migrations")]
async fn concurrent_idempotency_has_one_dispatch_and_one_charge(pool: PgPool) {
    let upstream = Mock::start(Reply::Slow).await;
    let (who, _, _) = setup(&pool, "chat", &upstream.url).await;
    let g = gateway(pool.clone(), 5);
    let (a, b) = tokio::join!(
        g.execute(&who, "race", chat(), "chat"),
        g.execute(&who, "race", chat(), "chat")
    );
    let (id_a, a) = a.unwrap();
    let (id_b, b) = b.unwrap();
    assert_eq!(id_a, id_b);
    assert!(a.is_ok() || b.is_ok());
    assert_eq!(upstream.count(), 1);
    let totals: (i64, i64) =
        sqlx::query_as("SELECT spent_micros,reserved_micros FROM tenants WHERE id=$1")
            .bind(who.tenant_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(totals, (7, 0));
}

#[sqlx::test(migrations = "./migrations")]
async fn disconnected_client_does_not_cancel_charge_completion(pool: PgPool) {
    let upstream = Mock::start(Reply::Slow).await;
    let (who, key, _) = setup(&pool, "chat", &upstream.url).await;
    let g = gateway(pool.clone(), 5);
    let app = api::gateway_router(g.clone());
    let client =
        tokio::spawn(app.oneshot(request(&key, "disconnect", chat(), "/v1/chat/completions")));
    for _ in 0..100 {
        if upstream.count() > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(upstream.count(), 1);
    client.abort();
    g.drain().await;
    let restarted = gateway(pool.clone(), 5);
    let (_, result) = restarted
        .execute(&who, "disconnect", chat(), "chat")
        .await
        .unwrap();
    assert_eq!(
        result.unwrap()["choices"][0]["message"]["content"],
        "private answer"
    );
    assert_eq!(upstream.count(), 1);
    let spent: i64 = sqlx::query_scalar("SELECT spent_micros FROM tenants WHERE id=$1")
        .bind(who.tenant_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(spent, 7);
}

#[sqlx::test(migrations = "./migrations")]
async fn database_failure_after_upstream_success_keeps_unknown_and_never_resends(pool: PgPool) {
    let upstream = Mock::start(Reply::Chat).await;
    let (who, _, _) = setup(&pool, "chat", &upstream.url).await;
    sqlx::raw_sql("CREATE FUNCTION inject_settlement_failure() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.status='succeeded' THEN RAISE EXCEPTION 'injected write failure'; END IF; RETURN NEW; END $$; CREATE TRIGGER settlement_failure BEFORE UPDATE ON requests FOR EACH ROW EXECUTE FUNCTION inject_settlement_failure();").execute(&pool).await.unwrap();
    let g = gateway(pool.clone(), 5);
    let (id, result) = g.execute(&who, "db-fault", chat(), "chat").await.unwrap();
    assert!(matches!(result, Err(Error::Database(_))));
    assert_eq!(
        g.request(&who, id).await.unwrap()["status"],
        "submission_unknown"
    );
    let (_, again) = g.execute(&who, "db-fault", chat(), "chat").await.unwrap();
    assert!(matches!(again, Err(Error::UpstreamUnknown)));
    assert_eq!(upstream.count(), 1);
    let totals: (i64, i64) =
        sqlx::query_as("SELECT spent_micros,reserved_micros FROM tenants WHERE id=$1")
            .bind(who.tenant_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(totals.0, 0);
    assert!(totals.1 > 0);
    sqlx::query("DROP TRIGGER settlement_failure ON requests")
        .execute(&pool)
        .await
        .unwrap();
    attempts::reconcile(
        &pool,
        who.tenant_id,
        id,
        Some((3, 4)),
        "fixture-known-usage",
    )
    .await
    .unwrap();
    assert_eq!(upstream.count(), 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn tool_capability_filter_skips_weaker_endpoint_and_keeps_fx_snapshot(pool: PgPool) {
    let weak = Mock::start(Reply::Chat).await;
    let strong = Mock::start(Reply::Tool).await;
    let (who, _, mut endpoint) = setup(&pool, "chat", &weak.url).await;
    store::set_fx(
        &pool,
        who.tenant_id,
        money::nonnegative("7.2").unwrap(),
        chrono::Utc::now() - chrono::Duration::seconds(1),
        "manual",
    )
    .await
    .unwrap();
    endpoint.id = Uuid::new_v4();
    endpoint.base_url = strong.url.clone();
    endpoint.priority = 20;
    endpoint.supports_tools = true;
    endpoint.currency = "USD".into();
    store::add_endpoint(&pool, &endpoint).await.unwrap();
    let mut body = chat();
    body["tools"] = json!([{"type":"function","function":{"name":"lookup","parameters":{"type":"object","properties":{}}}}]);
    body["tool_choice"] = "auto".into();
    let g = gateway(pool.clone(), 5);
    let (id, result) = g.execute(&who, "tool", body.clone(), "chat").await.unwrap();
    assert_eq!(
        result.unwrap()["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
        "lookup"
    );
    assert_eq!(weak.count(), 0);
    assert_eq!(strong.count(), 1);
    assert_eq!(g.request(&who, id).await.unwrap()["cost_cny"], "0.000051");
    store::set_fx(
        &pool,
        who.tenant_id,
        money::nonnegative("8").unwrap(),
        chrono::Utc::now(),
        "manual",
    )
    .await
    .unwrap();
    g.execute(&who, "tool", body, "chat")
        .await
        .unwrap()
        .1
        .unwrap();
    assert_eq!(strong.count(), 1);
    assert_eq!(g.request(&who, id).await.unwrap()["cost_cny"], "0.000051");
}

#[sqlx::test(migrations = "./migrations")]
async fn restart_maintenance_retains_uncertain_but_releases_never_started(pool: PgPool) {
    let upstream = Mock::start(Reply::Chat).await;
    let (who, _, endpoint) = setup(&pool, "chat", &upstream.url).await;
    let price = endpoint.price(&pool).await.unwrap();
    let digest = store::hash(b"restart-fixture");
    let mut ids = Vec::new();
    for key in ["started", "not-started"] {
        let (row, _) = brokerrouter::accounting::reserve(
            &pool,
            brokerrouter::accounting::Admission {
                identity: &who,
                model: "logical/model",
                idempotency_key: key,
                request_hash: &digest,
                maximum_micros: 1010,
                snapshot: price.clone(),
                routing: Some(json!([brokerrouter::gateway::Route {
                    endpoint: endpoint.clone(),
                    price: price.clone()
                }])),
                timeout_seconds: 1,
            },
        )
        .await
        .unwrap();
        if key == "started" {
            attempts::begin(&pool, &who, row.id).await.unwrap();
        }
        ids.push(row.id);
    }
    tokio::time::sleep(Duration::from_millis(1100)).await;
    attempts::maintain(&pool).await.unwrap();
    assert_eq!(
        brokerrouter::accounting::get(&pool, &who, ids[0])
            .await
            .unwrap()
            .status,
        "submission_unknown"
    );
    assert_eq!(
        brokerrouter::accounting::get(&pool, &who, ids[1])
            .await
            .unwrap()
            .status,
        "failed"
    );
    assert_eq!(upstream.count(), 0);
    let held: i64 = sqlx::query_scalar("SELECT reserved_micros FROM tenants WHERE id=$1")
        .bind(who.tenant_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(held, 1010);
}

#[sqlx::test(migrations = "./migrations")]
async fn runtime_role_can_serve_but_only_reconciler_can_finalize_unknown(pool: PgPool) {
    sqlx::raw_sql(include_str!("../deploy/permissions.sql"))
        .execute(&pool)
        .await
        .unwrap();
    let restricted = |role: &'static str| {
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .after_connect(move |c, _| {
                Box::pin(async move {
                    sqlx::query(&format!("SET ROLE {role}")).execute(c).await?;
                    Ok(())
                })
            })
            .connect_with((*pool.connect_options()).clone())
    };
    let runtime = restricted("brokerrouter_runtime").await.unwrap();
    let reconciler = restricted("brokerrouter_reconciler").await.unwrap();
    let upstream = Mock::start(Reply::Chat).await;
    let (who, _, _) = setup(&pool, "chat", &upstream.url).await;
    let g = gateway(runtime.clone(), 5);
    g.execute(&who, "runtime", chat(), "chat")
        .await
        .unwrap()
        .1
        .unwrap();
    attempts::maintain(&runtime).await.unwrap();
    let bad = Mock::start(Reply::MissingUsage).await;
    let (other, _, _) = setup(&pool, "chat", &bad.url).await;
    let (id, _) = g
        .execute(&other, "reconcile", chat(), "chat")
        .await
        .unwrap();
    assert!(
        attempts::reconcile(&runtime, other.tenant_id, id, None, "not-agent-authorized")
            .await
            .is_err()
    );
    assert_eq!(
        g.request(&other, id).await.unwrap()["status"],
        "submission_unknown"
    );
    attempts::reconcile(
        &reconciler,
        other.tenant_id,
        id,
        None,
        "human-verified-not-charged",
    )
    .await
    .unwrap();
    assert_eq!(g.request(&other, id).await.unwrap()["status"], "reconciled");
    runtime.close().await;
    reconciler.close().await;
}
