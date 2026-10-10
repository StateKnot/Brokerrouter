mod common;
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
    ToolRoundtrip,
    Stream,
    StreamTiny,
    StreamLarge,
    StreamBroken,
    StreamNoUsage,
    Judge(&'static str),
    JudgeSlow,
    SmartJudge,
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
        reply @ (Reply::Stream | Reply::StreamTiny | Reply::StreamLarge | Reply::StreamBroken | Reply::StreamNoUsage) => {
            let answer = if matches!(reply, Reply::StreamLarge) { "x".repeat(300_000) } else { "private answer".into() };
            let mut events = vec![
                json!({"id":"chatcmpl-stream","object":"chat.completion.chunk","created":1,"model":"upstream","choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null,"logprobs":null}],"usage":null}),
                json!({"id":"chatcmpl-stream","object":"chat.completion.chunk","created":1,"model":"upstream","choices":[{"index":0,"delta":{"content":answer},"finish_reason":null,"logprobs":null}],"usage":null}),
                json!({"id":"chatcmpl-stream","object":"chat.completion.chunk","created":1,"model":"upstream","choices":[{"index":0,"delta":{},"finish_reason":"stop","logprobs":null}],"usage":null}),
            ];
            if matches!(reply, Reply::StreamTiny) {
                events[0]["choices"][0]["delta"]["content"] = events[1]["choices"][0]["delta"]["content"].clone();
                events.remove(1);
            }
            if !matches!(reply, Reply::StreamNoUsage) {
                events.push(json!({"id":"chatcmpl-stream","object":"chat.completion.chunk","created":1,"model":"upstream","choices":[],"usage":{"prompt_tokens":3,"completion_tokens":4,"total_tokens":7}}));
            }
            let mut text = events
                .into_iter()
                .map(|event| format!("data: {event}\n\n"))
                .collect::<String>();
            if !matches!(reply, Reply::StreamBroken) {
                text.push_str("data: [DONE]\n\n");
            }
            (
                StatusCode::OK,
                [("content-type", "text/event-stream; charset=utf-8")],
                text,
            )
                .into_response()
        }
        reply=>{
            if matches!(reply,Reply::Slow){tokio::time::sleep(Duration::from_millis(1300)).await;}
            if matches!(reply,Reply::JudgeSlow){tokio::time::sleep(Duration::from_millis(1300)).await;}
            let mut value=json!({"id":"chatcmpl-fixture","object":"chat.completion","created":1,"model":"upstream","choices":[{"index":0,"message":{"role":"assistant","content":"private answer"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":4,"total_tokens":7}});
            if let Reply::Judge(content)=reply {value["choices"][0]["message"]["content"]=content.into();}
            if matches!(reply,Reply::JudgeSlow) || (matches!(reply,Reply::SmartJudge) && body["messages"][0]["content"].as_str().is_some_and(|s|s.starts_with("You are a security classifier"))) {value["choices"][0]["message"]["content"]="{\"decision\":\"pass\"}".into();}
            if matches!(reply,Reply::MissingUsage){value.as_object_mut().unwrap().remove("usage");}
            if matches!(reply,Reply::Tool){value["choices"][0]["message"]=json!({"role":"assistant","content":null,"tool_calls":[{"id":"call_fixture","type":"function","function":{"name":"lookup","arguments":"{}"}}]});value["choices"][0]["finish_reason"]="tool_calls".into();}
            if matches!(reply,Reply::ToolRoundtrip) {
                if body["messages"].as_array().is_some_and(|messages| messages.iter().any(|message| message["role"] == "tool")) {
                    value["choices"][0]["message"]["content"]="42".into();
                } else {
                    value["choices"][0]["message"]=json!({"role":"assistant","content":null,"tool_calls":[{"index":0,"id":"call_fixture","type":"function","function":{"name":"add","arguments":"{\"a\":17,\"b\":25}"}}]});
                    value["choices"][0]["finish_reason"]="tool_calls".into();
                }
            }
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
        supports_streaming: false,
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
fn streaming_chat() -> Value {
    json!({"model":"logical/model","messages":[{"role":"user","content":"hello"}],"max_tokens":10,"stream":true})
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

fn semantic_rule() -> Value {
    json!({"kind":"model_judge","model":"logical/model","instructions":"Flag secrets or destructive actions.","input_format":"text_json","max_input_bytes":65536,"max_output_tokens":32,"maximum_cny":"0.1"})
}
fn semantic_action(turn: Uuid, key: &str) -> brokerrouter::governance::Check {
    brokerrouter::governance::Check {
        turn_id: turn,
        operation_key: key.into(),
        stage: "tool_input".into(),
        model_id: None,
        tool_name: Some("shell".into()),
        payload: json!({"command":"sensitive fixture action"}),
    }
}
async fn semantic_setup(
    pool: &PgPool,
    reply: Reply,
    timeout: i32,
    stage: &str,
) -> (Mock, store::Identity, Gateway, Uuid) {
    let mock = Mock::start(reply).await;
    let (who, _, _) = setup(pool, "chat", &mock.url).await;
    common::provision(pool, &who, "judge-user").await;
    common::guardrail(pool, who.tenant_id, "semantic", 1, stage, semantic_rule()).await;
    let turn = common::turn(pool, &who, "judge-user", 0).await;
    let g = Gateway::new(
        pool.clone(),
        common::cipher(),
        HashMap::from([("FIXTURE_PROVIDER_KEY".into(), "fake-provider-secret".into())]),
        1,
        timeout,
        60,
    )
    .unwrap();
    (mock, who, g, turn)
}

#[sqlx::test(migrations = "./migrations")]
async fn semantic_risk_has_cny_cost_and_only_human_turn_exemption(pool: PgPool) {
    let (mock, who, g, turn) = semantic_setup(
        &pool,
        Reply::Judge("{\"decision\":\"risk\"}"),
        3,
        "tool_input",
    )
    .await;
    // USD source cost is still settled in CNY using an immutable rate version.
    sqlx::query("UPDATE endpoints SET active=false")
        .execute(&pool)
        .await
        .unwrap();
    let (_, _, mut endpoint) = setup(&pool, "chat", &mock.url).await;
    endpoint.id = Uuid::new_v4();
    endpoint.tenant_id = who.tenant_id;
    endpoint.currency = "USD".into();
    store::add_endpoint(&pool, &endpoint).await.unwrap();
    let fx = store::set_fx(
        &pool,
        who.tenant_id,
        money::nonnegative("7.2").unwrap(),
        chrono::Utc::now(),
        "fixture",
    )
    .await
    .unwrap();
    let a = g.check(&who, semantic_action(turn, "first")).await.unwrap();
    assert_eq!(a["state"], "waiting_approval");
    assert_eq!(a["approval"]["guardrails"][0]["reason_code"], "judge_risk");
    let (request,cost,snapshot):(Uuid,i64,Value)=sqlx::query_as("SELECT id,final_micros,price_snapshot FROM requests WHERE tenant_id=$1 AND guardrail_evaluation_id IS NOT NULL").bind(who.tenant_id).fetch_one(&pool).await.unwrap();
    assert_eq!(cost, 51);
    assert_eq!(snapshot["fx_version"], fx);
    assert!(matches!(
        g.result(&who, request).await,
        Err(Error::NotFound)
    ));
    assert_eq!(
        g.request(&who, request).await.unwrap()["purpose"],
        "guardrail"
    );
    assert!(
        !g.logs(&who, None, 100)
            .await
            .unwrap()
            .to_string()
            .contains("sensitive fixture action")
    );
    assert_eq!(
        g.check(&who, semantic_action(turn, "first")).await.unwrap()["approval"]["id"],
        a["approval"]["id"]
    );
    assert_eq!(mock.count(), 1);
    common::decide(
        &pool,
        "judge-user",
        common::approval_id(&a),
        common::approve(&["semantic"]),
    )
    .await
    .unwrap();
    assert_eq!(
        g.check(&who, semantic_action(turn, "second"))
            .await
            .unwrap()["state"],
        "ready"
    );
    assert_eq!(mock.count(), 1);
    let next = common::turn(&pool, &who, "judge-user", 1).await;
    assert_eq!(
        g.check(&who, semantic_action(next, "third")).await.unwrap()["state"],
        "waiting_approval"
    );
    assert_eq!(mock.count(), 2);
}

#[sqlx::test(migrations = "./migrations")]
async fn semantic_model_pre_and_post_share_ledger_without_recursion(pool: PgPool) {
    let (mock, who, g, turn) = semantic_setup(&pool, Reply::SmartJudge, 3, "pre").await;
    common::guardrail(
        &pool,
        who.tenant_id,
        "semantic-output",
        1,
        "post",
        semantic_rule(),
    )
    .await;
    let (id, value) = g
        .execute_in_turn(&who, "business", chat(), "chat", Some(turn))
        .await
        .unwrap();
    assert_eq!(
        value.unwrap()["choices"][0]["message"]["content"],
        "private answer"
    );
    assert_eq!(mock.count(), 3); // input judge + business + output judge, even at concurrency=1
    g.result(&who, id).await.unwrap();
    assert_eq!(mock.count(), 3);
    let facts: (i64, i64, i64) = sqlx::query_as(
        "SELECT count(*),sum(final_micros)::bigint,count(guardrail_evaluation_id) FROM requests",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(facts, (3, 21, 2));
    assert!(
        g.execute_in_turn(&who, "_br_guardrail:forged", chat(), "chat", Some(turn))
            .await
            .is_err()
    );
    let mut forged = chat();
    forged["skip_guardrails"] = true.into();
    assert!(
        g.execute_in_turn(&who, "forged", forged, "chat", Some(turn))
            .await
            .is_err()
    );
    assert_eq!(mock.count(), 3);
}

#[sqlx::test(migrations = "./migrations")]
async fn semantic_invalid_decision_is_paid_but_requires_user_review(pool: PgPool) {
    let (mock, who, g, turn) = semantic_setup(
        &pool,
        Reply::Judge("{\"decision\":\"pass\",\"leak\":\"secret\"}"),
        3,
        "tool_input",
    )
    .await;
    let a = g
        .check(&who, semantic_action(turn, "invalid"))
        .await
        .unwrap();
    assert_eq!(a["state"], "waiting_approval");
    assert_eq!(
        a["approval"]["guardrails"][0]["reason_code"],
        "judge_invalid_decision"
    );
    assert!(!a.to_string().contains("secret"));
    let spent: i64 = sqlx::query_scalar("SELECT spent_micros FROM tenants WHERE id=$1")
        .bind(who.tenant_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(spent, 7);
    assert_eq!(mock.count(), 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn semantic_timeout_keeps_reservation_and_never_resubmits(pool: PgPool) {
    let (mock, who, g, turn) = semantic_setup(&pool, Reply::JudgeSlow, 1, "tool_input").await;
    let a = g
        .check(&who, semantic_action(turn, "timeout"))
        .await
        .unwrap();
    assert_eq!(a["state"], "waiting_approval");
    assert_eq!(
        a["approval"]["guardrails"][0]["reason_code"],
        "judge_outcome_unknown"
    );
    let (status,reserved,spent):(String,i64,i64)=sqlx::query_as("SELECT r.status,t.reserved_micros,t.spent_micros FROM requests r JOIN tenants t ON t.id=r.tenant_id").fetch_one(&pool).await.unwrap();
    assert_eq!(status, "submission_unknown");
    assert!(reserved > 0);
    assert_eq!(spent, 0);
    attempts::maintain(&pool).await.unwrap();
    brokerrouter::governance::maintain(&pool).await.unwrap();
    assert_eq!(
        g.check(&who, semantic_action(turn, "timeout"))
            .await
            .unwrap()["approval"]["id"],
        a["approval"]["id"]
    );
    assert_eq!(mock.count(), 1);
    common::decide(
        &pool,
        "judge-user",
        common::approval_id(&a),
        common::approve(&["semantic"]),
    )
    .await
    .unwrap();
    assert_eq!(
        g.check(&who, semantic_action(turn, "timeout"))
            .await
            .unwrap()["state"],
        "ready"
    );
    let after: i64 = sqlx::query_scalar("SELECT reserved_micros FROM tenants WHERE id=$1")
        .bind(who.tenant_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(after, reserved); // approval is not a refund or settlement
}

#[sqlx::test(migrations = "./migrations")]
async fn semantic_concurrent_checks_have_one_paid_dispatch(pool: PgPool) {
    let (mock, who, g, turn) = semantic_setup(&pool, Reply::JudgeSlow, 4, "tool_input").await;
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let g = g.clone();
        let who = who.clone();
        tasks.push(tokio::spawn(async move {
            g.check(&who, semantic_action(turn, "concurrent")).await
        }));
    }
    for task in tasks {
        let v = task.await.unwrap().unwrap();
        assert!(matches!(v["state"].as_str(), Some("ready" | "checking")));
    }
    assert_eq!(
        g.check(&who, semantic_action(turn, "concurrent"))
            .await
            .unwrap()["state"],
        "ready"
    );
    assert_eq!(mock.count(), 1);
    let counts: (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM guardrail_evaluations),(SELECT count(*) FROM requests)",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(counts, (1, 1));
}

#[sqlx::test(migrations = "./migrations")]
async fn semantic_admission_failures_are_reviewable_without_cost(pool: PgPool) {
    let (mock, who, g, turn) = semantic_setup(
        &pool,
        Reply::Judge("{\"decision\":\"pass\"}"),
        3,
        "tool_input",
    )
    .await;
    for (version, field, value) in [
        (2, "maximum_cny", json!("0")),
        (3, "max_input_bytes", json!(1)),
        (4, "model", json!("missing/model")),
    ] {
        let mut rule = semantic_rule();
        rule[field] = value;
        common::guardrail(
            &pool,
            who.tenant_id,
            "semantic",
            version,
            "tool_input",
            rule,
        )
        .await;
        let a = g
            .check(&who, semantic_action(turn, &format!("limited-{version}")))
            .await
            .unwrap();
        assert_eq!(a["state"], "waiting_approval");
        assert_eq!(
            a["approval"]["guardrails"][0]["evaluation_outcome"],
            "failed"
        );
    }
    common::guardrail(
        &pool,
        who.tenant_id,
        "semantic",
        5,
        "tool_input",
        semantic_rule(),
    )
    .await;
    sqlx::query("UPDATE virtual_keys SET budget_micros=0 WHERE id=$1")
        .bind(who.key_id)
        .execute(&pool)
        .await
        .unwrap();
    let a = g
        .check(&who, semantic_action(turn, "budget"))
        .await
        .unwrap();
    assert_eq!(
        a["approval"]["guardrails"][0]["reason_code"],
        "judge_budget_exceeded"
    );
    assert_eq!(mock.count(), 0);
    let requests: i64 = sqlx::query_scalar("SELECT count(*) FROM requests")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(requests, 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn semantic_crashed_owner_expires_with_event_without_new_dispatch(pool: PgPool) {
    let (mock, who, g, turn) = semantic_setup(&pool, Reply::SmartJudge, 3, "tool_input").await;
    let pending = brokerrouter::governance::check(
        &pool,
        &common::cipher(),
        &who,
        semantic_action(turn, "crashed"),
    )
    .await
    .unwrap();
    assert_eq!(pending["state"], "checking");
    let op: Uuid = serde_json::from_value(pending["id"].clone()).unwrap();
    let row = brokerrouter::governance::operation(&pool, &common::cipher(), &who, op)
        .await
        .unwrap();
    assert_eq!(row["state"], "checking");
    let hash: String = sqlx::query_scalar(
        "SELECT encode(payload_hash,'hex') FROM governance_operations WHERE id=$1",
    )
    .bind(op)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(matches!(
        brokerrouter::governance::claim(
            &pool,
            &common::cipher(),
            &who,
            op,
            brokerrouter::governance::Claim {
                token: "55".repeat(32),
                payload_hash: hash
            }
        )
        .await,
        Err(Error::InProgress)
    ));
    // Crash after ownership was durably committed, before any request existed.
    sqlx::query("UPDATE guardrail_evaluations SET state='running',deadline=clock_timestamp()-interval '1 second'").execute(&pool).await.unwrap();
    brokerrouter::governance::maintain(&pool).await.unwrap();
    let a = g
        .check(&who, semantic_action(turn, "crashed"))
        .await
        .unwrap();
    assert_eq!(a["state"], "waiting_approval");
    assert_eq!(mock.count(), 0);
    let events: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM governance_events WHERE event_type='guardrail.finished'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(events, 1);
    assert!(
        sqlx::query(
            "UPDATE guardrail_evaluations SET state='pending',reason_code=NULL,finished_at=NULL"
        )
        .execute(&pool)
        .await
        .is_err()
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn semantic_settlement_failure_does_not_repeat_paid_check(pool: PgPool) {
    let (mock, who, g, turn) = semantic_setup(
        &pool,
        Reply::Judge("{\"decision\":\"pass\"}"),
        3,
        "tool_input",
    )
    .await;
    sqlx::raw_sql("CREATE FUNCTION fail_judge_settlement() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.kind='finalize' THEN RAISE EXCEPTION 'fixture settlement failure'; END IF; RETURN NEW; END $$; CREATE TRIGGER fixture_judge_settlement BEFORE INSERT ON ledger_entries FOR EACH ROW EXECUTE FUNCTION fail_judge_settlement();").execute(&pool).await.unwrap();
    let a = g
        .check(&who, semantic_action(turn, "commit-failure"))
        .await
        .unwrap();
    assert_eq!(a["state"], "waiting_approval");
    sqlx::query("DROP TRIGGER fixture_judge_settlement ON ledger_entries")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        g.check(&who, semantic_action(turn, "commit-failure"))
            .await
            .unwrap()["approval"]["id"],
        a["approval"]["id"]
    );
    assert_eq!(mock.count(), 1);
    let request: Uuid = sqlx::query_scalar("SELECT id FROM requests")
        .fetch_one(&pool)
        .await
        .unwrap();
    attempts::reconcile(
        &pool,
        who.tenant_id,
        request,
        Some((3, 4)),
        "fixture://verified-check-usage",
    )
    .await
    .unwrap();
    let money: (i64, i64) =
        sqlx::query_as("SELECT spent_micros,reserved_micros FROM tenants WHERE id=$1")
            .bind(who.tenant_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(money, (7, 0));
    assert_eq!(
        g.check(&who, semantic_action(turn, "commit-failure"))
            .await
            .unwrap()["state"],
        "waiting_approval"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn semantic_runtime_role_can_evaluate_but_cannot_repurpose_or_approve(pool: PgPool) {
    let (mock, who, _g, turn) = semantic_setup(
        &pool,
        Reply::Judge("{\"decision\":\"risk\"}"),
        3,
        "tool_input",
    )
    .await;
    sqlx::raw_sql(include_str!("../deploy/permissions.sql"))
        .execute(&pool)
        .await
        .unwrap();
    let runtime = sqlx::postgres::PgPoolOptions::new()
        .max_connections(5)
        .after_connect(|c, _| {
            Box::pin(async move {
                sqlx::query("SET ROLE brokerrouter_runtime")
                    .execute(c)
                    .await?;
                Ok(())
            })
        })
        .connect_with(pool.connect_options().as_ref().clone())
        .await
        .unwrap();
    let g = Gateway::new(
        runtime.clone(),
        common::cipher(),
        HashMap::from([("FIXTURE_PROVIDER_KEY".into(), "fake-provider-secret".into())]),
        1,
        3,
        60,
    )
    .unwrap();
    let a = g
        .check(&who, semantic_action(turn, "restricted"))
        .await
        .unwrap();
    assert_eq!(a["state"], "waiting_approval");
    assert_eq!(mock.count(), 1);
    assert!(
        sqlx::query("UPDATE approvals SET state='approved'")
            .execute(&runtime)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("UPDATE requests SET guardrail_evaluation_id=NULL")
            .execute(&runtime)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("UPDATE requests SET guardrail_evaluation_id=NULL")
            .execute(&pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("UPDATE guardrail_evaluations SET state='passed'")
            .execute(&runtime)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("TRUNCATE guardrail_evaluations CASCADE")
            .execute(&pool)
            .await
            .is_err()
    );
    runtime.close().await;
}

#[sqlx::test(migrations = "./migrations")]
async fn semantic_inflight_policy_change_requires_new_version_evaluation(pool: PgPool) {
    let (mock, who, g, turn) = semantic_setup(&pool, Reply::JudgeSlow, 4, "tool_input").await;
    let worker = g.clone();
    let identity = who.clone();
    let task = tokio::spawn(async move {
        worker
            .check(&identity, semantic_action(turn, "policy-change"))
            .await
    });
    tokio::time::timeout(Duration::from_secs(3), async {
        while mock.count() == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    // This must not wait on the slow network request: no transaction spans HTTP.
    tokio::time::timeout(
        Duration::from_millis(800),
        common::guardrail(
            &pool,
            who.tenant_id,
            "semantic",
            2,
            "tool_input",
            semantic_rule(),
        ),
    )
    .await
    .unwrap();
    let a = task.await.unwrap().unwrap();
    assert_eq!(a["state"], "checking");
    assert_eq!(mock.count(), 1);
    assert_eq!(
        g.check(&who, semantic_action(turn, "policy-change"))
            .await
            .unwrap()["state"],
        "ready"
    );
    assert_eq!(mock.count(), 2);
    let versions: Vec<(i32, String)> =
        sqlx::query_as("SELECT version,state FROM guardrail_evaluations ORDER BY version")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(versions, vec![(1, "passed".into()), (2, "passed".into())]);
}

#[sqlx::test(migrations = "./migrations")]
async fn semantic_http_disconnect_does_not_cancel_owned_check(pool: PgPool) {
    let (mock, who, g, turn) = semantic_setup(&pool, Reply::JudgeSlow, 4, "tool_input").await;
    // Give a fresh bound key to a real HTTP handler rather than exposing fixture
    // credentials through the semantic setup return value.
    let (_, token) = store::create_key(
        &pool,
        who.tenant_id,
        "http-judge",
        10_000_000,
        &["logical/model".into()],
        None,
    )
    .await
    .unwrap();
    let other = store::authenticate(&pool, &token).await.unwrap();
    common::provision(&pool, &other, "http-judge-user").await;
    let other_turn = common::turn(&pool, &other, "http-judge-user", 0).await;
    let app = api::gateway_router(g.clone());
    let body = serde_json::to_value(semantic_action(other_turn, "disconnect-check")).unwrap();
    let task = tokio::spawn(app.oneshot(request(&token, "unused", body, "/v1/governance/checks")));
    tokio::time::timeout(Duration::from_secs(3), async {
        while mock.count() == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    task.abort();
    g.drain().await;
    let facts:(i64,String)=sqlx::query_as("SELECT r.final_micros,e.state FROM requests r JOIN guardrail_evaluations e ON e.tenant_id=r.tenant_id AND e.id=r.guardrail_evaluation_id").fetch_one(&pool).await.unwrap();
    assert_eq!(facts, (7, "passed".into()));
    assert_eq!(mock.count(), 1);
    assert!(
        g.logs(&who, None, 100).await.unwrap()["data"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let _ = turn;
}

#[sqlx::test(migrations = "./migrations")]
async fn semantic_result_get_never_starts_new_paid_policy_check(pool: PgPool) {
    let (mock, who, g, turn) = semantic_setup(&pool, Reply::SmartJudge, 4, "tool_input").await;
    let (id, result) = g
        .execute_in_turn(&who, "stored", chat(), "chat", Some(turn))
        .await
        .unwrap();
    result.unwrap();
    assert_eq!(mock.count(), 1);
    common::guardrail(
        &pool,
        who.tenant_id,
        "new-output",
        1,
        "post",
        semantic_rule(),
    )
    .await;
    for _ in 0..3 {
        assert!(matches!(g.result(&who, id).await, Err(Error::InProgress)));
    }
    assert_eq!(mock.count(), 1);
    let released = g.check_result(&who, id).await.unwrap();
    assert_eq!(
        released["choices"][0]["message"]["content"],
        "private answer"
    );
    assert_eq!(mock.count(), 2);
    g.check_result(&who, id).await.unwrap();
    g.result(&who, id).await.unwrap();
    assert_eq!(mock.count(), 2);
}

#[sqlx::test(migrations = "./migrations")]
async fn http_runtime_turn_binding_requires_the_virtual_key_and_exact_host_identity(pool: PgPool) {
    let upstream = Mock::start(Reply::Chat).await;
    let (who, key, _) = setup(&pool, "chat", &upstream.url).await;
    common::provision(&pool, &who, "hermes-user").await;
    let turn = common::turn(&pool, &who, "hermes-user", 0).await;
    let app = api::gateway_router(gateway(pool, 5));
    let path = format!("/v1/governance/turns/{turn}/bind");

    let response = app
        .clone()
        .oneshot(request(
            &key,
            "unused",
            json!({
                "session_id": "session",
                "message_id": "message",
                "run_id": "run",
                "generation": 0
            }),
            &path,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(body["id"], turn.to_string());
    assert_eq!(body["state"], "active");

    let mismatch = app
        .clone()
        .oneshot(request(
            &key,
            "unused",
            json!({
                "session_id": "session",
                "message_id": "message",
                "run_id": "other-run",
                "generation": 0
            }),
            &path,
        ))
        .await
        .unwrap();
    assert_eq!(mismatch.status(), StatusCode::NOT_FOUND);

    let unauthorized = app
        .clone()
        .oneshot(request(
            &("sk_br_".to_string() + &"2".repeat(64)),
            "unused",
            json!({
                "session_id": "session",
                "message_id": "message",
                "run_id": "run",
                "generation": 0
            }),
            &path,
        ))
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

    let unknown_field = app
        .oneshot(request(
            &key,
            "unused",
            json!({
                "session_id": "session",
                "message_id": "message",
                "run_id": "run",
                "generation": 0,
                "turn_id": turn
            }),
            &path,
        ))
        .await
        .unwrap();
    assert_eq!(unknown_field.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(upstream.count(), 0);
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
        Err(Error::UnsupportedCapability("SSE streaming"))
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
async fn missing_tool_capability_is_explicit_and_never_calls_upstream(pool: PgPool) {
    let upstream = Mock::start(Reply::Chat).await;
    let (who, _, _) = setup(&pool, "chat", &upstream.url).await;
    let mut body = chat();
    body["tools"] = json!([{"type":"function","function":{"name":"lookup","parameters":{"type":"object","properties":{}}}}]);
    let error = gateway(pool, 5)
        .execute(&who, "unsupported-tool", body, "chat")
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        Error::UnsupportedCapability("function tool roundtrips")
    ));
    assert_eq!(upstream.count(), 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn combined_tool_stream_capability_must_exist_on_one_endpoint(pool: PgPool) {
    let upstream = Mock::start(Reply::Stream).await;
    let (who, _, endpoint) = setup(&pool, "chat", &upstream.url).await;
    sqlx::query("UPDATE endpoints SET supports_tools=true WHERE id=$1")
        .bind(endpoint.id)
        .execute(&pool)
        .await
        .unwrap();
    let mut streaming_only = endpoint.clone();
    streaming_only.id = Uuid::new_v4();
    streaming_only.name = "streaming-only".into();
    streaming_only.supports_tools = false;
    streaming_only.supports_streaming = true;
    store::add_endpoint(&pool, &streaming_only).await.unwrap();
    let mut body = streaming_chat();
    body["tools"] = json!([{"type":"function","function":{"name":"lookup","parameters":{"type":"object","properties":{}}}}]);
    assert!(matches!(
        gateway(pool, 5)
            .execute_stream_in_turn(&who, "combined-capability", body, None)
            .await,
        Err(Error::UnsupportedCapability(
            "function tool roundtrips with SSE streaming"
        ))
    ));
    assert_eq!(upstream.count(), 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn provider_tool_index_is_normalized_for_exact_agent_roundtrip(pool: PgPool) {
    let upstream = Mock::start(Reply::ToolRoundtrip).await;
    let (who, _, endpoint) = setup(&pool, "chat", &upstream.url).await;
    sqlx::query("UPDATE endpoints SET supports_tools=true WHERE id=$1")
        .bind(endpoint.id)
        .execute(&pool)
        .await
        .unwrap();
    let g = gateway(pool.clone(), 5);
    let mut first = chat();
    first["tools"] = json!([{"type":"function","function":{"name":"add","description":"Add two integers","parameters":{"type":"object","properties":{"a":{"type":"integer"},"b":{"type":"integer"}},"required":["a","b"]}}}]);
    first["tool_choice"] = "auto".into();
    let assistant = g
        .execute(&who, "tool-roundtrip-1", first.clone(), "chat")
        .await
        .unwrap()
        .1
        .unwrap()["choices"][0]["message"]
        .clone();
    assert!(assistant["tool_calls"][0].get("index").is_none());
    let mut second = first;
    second["messages"] = json!([
        {"role":"user","content":"Use add for 17 and 25."},
        assistant,
        {"role":"tool","tool_call_id":"call_fixture","content":"42"}
    ]);
    let completed = g
        .execute(&who, "tool-roundtrip-2", second, "chat")
        .await
        .unwrap()
        .1
        .unwrap();
    assert_eq!(completed["choices"][0]["message"]["content"], "42");
    assert_eq!(upstream.count(), 2);
    let totals: (i64, i64) = sqlx::query_as(
        "SELECT count(*),count(*) FILTER (WHERE status='succeeded') FROM requests WHERE tenant_id=$1",
    )
    .bind(who.tenant_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(totals, (2, 2));
}

#[sqlx::test(migrations = "./migrations")]
async fn sse_stream_settles_before_done_and_replays_without_provider_call(pool: PgPool) {
    let upstream = Mock::start(Reply::Stream).await;
    let (who, key, endpoint) = setup(&pool, "chat", &upstream.url).await;
    sqlx::query("UPDATE endpoints SET supports_streaming=true WHERE id=$1")
        .bind(endpoint.id)
        .execute(&pool)
        .await
        .unwrap();
    let app = api::gateway_router(gateway(pool.clone(), 5));
    for expected_calls in [1, 1] {
        let response = app
            .clone()
            .oneshot(request(
                &key,
                "stream-idempotent",
                streaming_chat(),
                "/v1/chat/completions",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()["content-type"],
            "text/event-stream; charset=utf-8"
        );
        assert!(response.headers().contains_key("x-brokerrouter-request-id"));
        let body = String::from_utf8(
            http_body_util::BodyExt::collect(response.into_body())
                .await
                .unwrap()
                .to_bytes()
                .to_vec(),
        )
        .unwrap();
        assert!(body.contains("private answer"));
        assert!(body.ends_with("data: [DONE]\n\n"));
        assert_eq!(upstream.count(), expected_calls);
    }
    let row: (String, i64, i64) = sqlx::query_as(
        "SELECT status,final_micros,(SELECT count(*) FROM attempts WHERE request_id=requests.id) FROM requests WHERE tenant_id=$1 AND idempotency_key='stream-idempotent'",
    )
    .bind(who.tenant_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row, ("succeeded".into(), 7, 1));
    let sent = upstream.state.received.lock().unwrap();
    assert_eq!(sent[0]["body"]["stream"], true);
    assert_eq!(sent[0]["body"]["stream_options"]["include_usage"], true);
}

#[sqlx::test(migrations = "./migrations")]
async fn sse_large_event_and_replay_cross_bounded_frames(pool: PgPool) {
    let upstream = Mock::start(Reply::StreamLarge).await;
    let (who, key, endpoint) = setup(&pool, "chat", &upstream.url).await;
    sqlx::query("UPDATE endpoints SET supports_streaming=true WHERE id=$1")
        .bind(endpoint.id)
        .execute(&pool)
        .await
        .unwrap();
    let app = api::gateway_router(gateway(pool.clone(), 5));
    for _ in 0..2 {
        let response = app
            .clone()
            .oneshot(request(
                &key,
                "large-stream-replay",
                streaming_chat(),
                "/v1/chat/completions",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = http_body_util::BodyExt::collect(response.into_body())
            .await
            .unwrap()
            .to_bytes();
        assert!(
            String::from_utf8(body.to_vec())
                .unwrap()
                .contains(&"x".repeat(300_000))
        );
        assert!(body.ends_with(b"data: [DONE]\n\n"));
    }
    assert_eq!(upstream.count(), 1);
    assert_eq!(sqlx::query_scalar::<_, String>("SELECT status FROM requests WHERE tenant_id=$1 AND idempotency_key='large-stream-replay'")
        .bind(who.tenant_id).fetch_one(&pool).await.unwrap(), "succeeded");
}

#[sqlx::test(migrations = "./migrations")]
async fn client_disconnect_does_not_cancel_stream_settlement(pool: PgPool) {
    let upstream = Mock::start(Reply::Stream).await;
    let (who, _, endpoint) = setup(&pool, "chat", &upstream.url).await;
    sqlx::query("UPDATE endpoints SET supports_streaming=true WHERE id=$1")
        .bind(endpoint.id)
        .execute(&pool)
        .await
        .unwrap();
    let g = gateway(pool.clone(), 5);
    let session = g
        .execute_stream_in_turn(&who, "stream-disconnect", streaming_chat(), None)
        .await
        .unwrap();
    let id = session.id;
    drop(session);
    for _ in 0..100 {
        let state: String = sqlx::query_scalar("SELECT status FROM requests WHERE id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
        if state == "succeeded" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT status FROM requests WHERE id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap(),
        "succeeded"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn stalled_stream_keeps_delivery_slot_after_upstream_settles(pool: PgPool) {
    let upstream = Mock::start(Reply::StreamTiny).await;
    let (who, _, endpoint) = setup(&pool, "chat", &upstream.url).await;
    sqlx::query("UPDATE endpoints SET supports_streaming=true WHERE id=$1")
        .bind(endpoint.id)
        .execute(&pool)
        .await
        .unwrap();
    let g = Gateway::new(
        pool.clone(),
        ResultCipher::new(
            "fixture-v1".into(),
            HashMap::from([("fixture-v1".into(), "11".repeat(32))]),
        )
        .unwrap(),
        HashMap::from([("FIXTURE_PROVIDER_KEY".into(), "fake-provider-secret".into())]),
        4,
        5,
        60,
    )
    .unwrap();
    let session = g
        .execute_stream_in_turn(&who, "stalled-first", streaming_chat(), None)
        .await
        .unwrap();
    for _ in 0..100 {
        let state: String = sqlx::query_scalar("SELECT status FROM requests WHERE id=$1")
            .bind(session.id)
            .fetch_one(&pool)
            .await
            .unwrap();
        if state == "succeeded" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT status FROM requests WHERE id=$1")
            .bind(session.id)
            .fetch_one(&pool)
            .await
            .unwrap(),
        "succeeded"
    );
    // The upstream has finished. A fully buffered but unread response must
    // still consume its connection slot after the short send timeout elapsed.
    tokio::time::sleep(Duration::from_millis(200)).await;
    for ordinal in 0..20 {
        assert!(matches!(
            g.execute_stream_in_turn(&who, &format!("stalled-{ordinal}"), streaming_chat(), None)
                .await,
            Err(Error::Busy)
        ));
    }
    let requests: i64 = sqlx::query_scalar("SELECT count(*) FROM requests WHERE tenant_id=$1")
        .bind(who.tenant_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(requests, 1);
    drop(session);
    for _ in 0..100 {
        match g
            .execute_stream_in_turn(&who, "stalled-first", streaming_chat(), None)
            .await
        {
            Ok(mut replay) => {
                let mut data = Vec::new();
                while let Some(bytes) = replay.next().await {
                    data.extend_from_slice(&bytes);
                }
                assert!(
                    String::from_utf8(data)
                        .unwrap()
                        .ends_with("data: [DONE]\n\n")
                );
                assert_eq!(upstream.count(), 1);
                return;
            }
            Err(Error::Busy) => tokio::time::sleep(Duration::from_millis(10)).await,
            other => panic!("unexpected replay result: {}", other.err().unwrap()),
        }
    }
    panic!("delivery slot was not released after disconnect");
}

#[sqlx::test(migrations = "./migrations")]
async fn incomplete_sse_is_unknown_and_never_emits_done(pool: PgPool) {
    for (reply, include_usage) in [
        (Reply::StreamBroken, true),
        (Reply::StreamNoUsage, true),
        (Reply::StreamBroken, false),
        (Reply::StreamNoUsage, false),
    ] {
        let upstream = Mock::start(reply).await;
        let (who, key, endpoint) = setup(&pool, "chat", &upstream.url).await;
        sqlx::query("UPDATE endpoints SET supports_streaming=true WHERE id=$1")
            .bind(endpoint.id)
            .execute(&pool)
            .await
            .unwrap();
        let app = api::gateway_router(gateway(pool.clone(), 5));
        let idem = Uuid::new_v4().to_string();
        let mut request_body = streaming_chat();
        if !include_usage {
            request_body["stream_options"] = json!({"include_usage":false});
        }
        let response = app
            .oneshot(request(&key, &idem, request_body, "/v1/chat/completions"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = String::from_utf8(
            http_body_util::BodyExt::collect(response.into_body())
                .await
                .unwrap()
                .to_bytes()
                .to_vec(),
        )
        .unwrap();
        assert!(body.contains("submission_unknown"));
        assert!(!body.contains("data: [DONE]"));
        assert_eq!(
            sqlx::query_scalar::<_, String>(
                "SELECT status FROM requests WHERE tenant_id=$1 AND idempotency_key=$2",
            )
            .bind(who.tenant_id)
            .bind(idem)
            .fetch_one(&pool)
            .await
            .unwrap(),
            "submission_unknown"
        );
        assert_eq!(upstream.count(), 1);
        assert_eq!(
            upstream.state.received.lock().unwrap()[0]["body"]["stream_options"],
            json!({"include_usage":true})
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM ledger_entries WHERE tenant_id=$1 AND kind='finalize'",
            )
            .bind(who.tenant_id)
            .fetch_one(&pool)
            .await
            .unwrap(),
            0
        );
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn streaming_requires_endpoint_capability_and_no_post_guardrail(pool: PgPool) {
    let upstream = Mock::start(Reply::Stream).await;
    let (who, _, endpoint) = setup(&pool, "chat", &upstream.url).await;
    let g = gateway(pool.clone(), 5);
    assert!(matches!(
        g.execute_stream_in_turn(&who, "stream-capability", streaming_chat(), None)
            .await,
        Err(Error::UnsupportedCapability("SSE streaming"))
    ));
    sqlx::query("UPDATE endpoints SET supports_streaming=true WHERE id=$1")
        .bind(endpoint.id)
        .execute(&pool)
        .await
        .unwrap();
    common::provision(&pool, &who, "stream-user").await;
    common::guardrail(
        &pool,
        who.tenant_id,
        "post-review",
        1,
        "post",
        json!({"kind":"always"}),
    )
    .await;
    let turn = common::turn(&pool, &who, "stream-user", 0).await;
    assert!(matches!(
        g.execute_stream_in_turn(&who, "stream-post-guardrail", streaming_chat(), Some(turn),)
            .await,
        Err(Error::StreamingGuardrail)
    ));
    assert_eq!(upstream.count(), 0);
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
                governance_turn_id: None,
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

#[sqlx::test(migrations = "./migrations")]
async fn real_http_approval_loop_holds_input_and_output_then_replays_one_paid_result(pool: PgPool) {
    use brokerrouter::governance_api::{Control, control_router};
    let upstream = Mock::start(Reply::Chat).await;
    let (who, key, _) = setup(&pool, "chat", &upstream.url).await;
    common::provision(&pool, &who, "alice").await;
    let turn = common::turn(&pool, &who, "alice", 0).await;
    for (id, stage) in [("input", "pre"), ("output", "post")] {
        common::guardrail(&pool, who.tenant_id, id, 1, stage, json!({"kind":"always"})).await;
    }
    let g = Gateway::new(
        pool.clone(),
        common::cipher(),
        HashMap::from([("FIXTURE_PROVIDER_KEY".into(), "fixture".into())]),
        8,
        5,
        60,
    )
    .unwrap();
    let data_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let data_url = format!("http://{}", data_listener.local_addr().unwrap());
    let app = api::gateway_router(g.clone());
    let data_server = tokio::spawn(async move { axum::serve(data_listener, app).await.unwrap() });
    let control_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let control_url = format!("http://{}", control_listener.local_addr().unwrap());
    let app = control_router(Control {
        pool: pool.clone(),
        verifier: Arc::new(common::verifier()),
        cipher: Arc::new(common::cipher()),
    });
    let control_server =
        tokio::spawn(async move { axum::serve(control_listener, app).await.unwrap() });
    let client = reqwest::Client::builder()
        .no_proxy()
        .retry(reqwest::retry::never())
        .build()
        .unwrap();
    let send = || {
        client
            .post(format!("{data_url}/v1/chat/completions"))
            .bearer_auth(&key)
            .header("idempotency-key", "governed")
            .header("x-brokerrouter-turn-id", turn.to_string())
            .json(&chat())
            .send()
    };
    let first = send().await.unwrap();
    assert_eq!(first.status(), 409);
    assert_eq!(first.headers()["x-should-retry"], "false");
    let first: Value = first.json().await.unwrap();
    assert!(!first.to_string().contains("private answer"));
    let pre: Uuid = serde_json::from_value(first["error"]["approval"]["id"].clone()).unwrap();
    assert_eq!(upstream.count(), 0);
    let requests: i64 = sqlx::query_scalar("SELECT count(*) FROM requests")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(requests, 0);
    for (approval, guardrail) in [(pre, "input")] {
        let path = format!("/v1/approvals/{approval}/resolve");
        let body = common::approve(&[guardrail]);
        let token = common::sign(&common::claims(who.tenant_id, "alice", &path, &body));
        let denied = client
            .post(format!("{control_url}{path}"))
            .bearer_auth(&key)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(denied.status(), 401);
        let approved = client
            .post(format!("{control_url}{path}"))
            .bearer_auth(token)
            .body(serde_json::to_vec(&body).unwrap())
            .send()
            .await
            .unwrap();
        assert_eq!(approved.status(), 200);
    }
    let second = send().await.unwrap();
    assert_eq!(second.status(), 409);
    let request_id = second.headers()["x-brokerrouter-request-id"]
        .to_str()
        .unwrap()
        .parse::<Uuid>()
        .unwrap();
    let second: Value = second.json().await.unwrap();
    assert!(!second.to_string().contains("private answer"));
    assert_eq!(upstream.count(), 1);
    assert_eq!(
        g.request(&who, request_id).await.unwrap()["cost_cny"],
        "0.000007"
    );
    assert!(matches!(
        g.result(&who, request_id).await,
        Err(Error::ApprovalRequired(_))
    ));
    let post: Uuid = serde_json::from_value(second["error"]["approval"]["id"].clone()).unwrap();
    let path = format!("/v1/approvals/{post}/resolve");
    let body = common::approve(&["output"]);
    let approved = client
        .post(format!("{control_url}{path}"))
        .bearer_auth(common::sign(&common::claims(
            who.tenant_id,
            "alice",
            &path,
            &body,
        )))
        .body(serde_json::to_vec(&body).unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(approved.status(), 200);
    let result = send().await.unwrap();
    assert_eq!(result.status(), 200);
    assert_eq!(
        result.json::<Value>().await.unwrap()["choices"][0]["message"]["content"],
        "private answer"
    );
    assert_eq!(
        g.result(&who, request_id).await.unwrap()["id"],
        "chatcmpl-fixture"
    );
    assert_eq!(upstream.count(), 1);
    let finalized: i64 =
        sqlx::query_scalar("SELECT count(*) FROM ledger_entries WHERE kind='finalize'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(finalized, 1);
    let omitted = client
        .post(format!("{data_url}/v1/chat/completions"))
        .bearer_auth(&key)
        .header("idempotency-key", "omit")
        .json(&chat())
        .send()
        .await
        .unwrap();
    assert_eq!(omitted.status(), 400);
    data_server.abort();
    control_server.abort();
    g.drain().await;
}

#[sqlx::test(migrations = "./migrations")]
async fn output_rejection_destroys_result_without_reversing_real_charge(pool: PgPool) {
    let upstream = Mock::start(Reply::Chat).await;
    let (who, _, _) = setup(&pool, "chat", &upstream.url).await;
    common::provision(&pool, &who, "alice").await;
    let turn = common::turn(&pool, &who, "alice", 0).await;
    common::guardrail(
        &pool,
        who.tenant_id,
        "output",
        1,
        "post",
        json!({"kind":"always"}),
    )
    .await;
    let g = gateway(pool.clone(), 5);
    let (id, result) = g
        .execute_in_turn(&who, "reject-output", chat(), "chat", Some(turn))
        .await
        .unwrap();
    let approval = match result {
        Err(Error::ApprovalRequired(a)) => serde_json::from_value(a["id"].clone()).unwrap(),
        _ => panic!("protected output must wait"),
    };
    common::decide(&pool, "alice", approval, json!({"decision":"reject"}))
        .await
        .unwrap();
    assert!(matches!(
        g.result(&who, id).await,
        Err(Error::ResultUnavailable)
    ));
    assert_eq!(g.request(&who, id).await.unwrap()["cost_cny"], "0.000007");
    assert_eq!(upstream.count(), 1);
    let stored: Option<Vec<u8>> =
        sqlx::query_scalar("SELECT result_ciphertext FROM requests WHERE id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(stored.is_none());
}

#[test]
fn stream_options_accepts_usage_only_for_streaming() {
    for option in [
        json!({"include_usage":true}),
        json!({"include_usage":false}),
        json!({}),
        Value::Null,
    ] {
        let mut body = streaming_chat();
        body["stream_options"] = option.clone();
        let validated = brokerrouter::protocol::validate(body, "chat").unwrap();
        assert_eq!(validated.body["stream_options"], option);
    }
    for option in [
        json!({"include_usage":null}),
        json!({"include_usage":"true"}),
        json!({"include_usage":1}),
        json!({"include_usage":true,"unknown":false}),
        json!({"include_obfuscation":false}),
        json!([]),
        json!(false),
    ] {
        let mut body = streaming_chat();
        body["stream_options"] = option;
        assert!(matches!(
            brokerrouter::protocol::validate(body, "chat"),
            Err(Error::Invalid(_))
        ));
    }
    let mut nonstream = chat();
    nonstream["stream_options"] = json!({"include_usage":true});
    assert!(matches!(
        brokerrouter::protocol::validate(nonstream, "chat"),
        Err(Error::Invalid(_))
    ));
}

#[sqlx::test(migrations = "./migrations")]
async fn sse_stream_options_preserves_usage_choice_on_live_and_replay(pool: PgPool) {
    let upstream = Mock::start(Reply::Stream).await;
    let (who, key, endpoint) = setup(&pool, "chat", &upstream.url).await;
    sqlx::query("UPDATE endpoints SET supports_streaming=true,supports_tools=true WHERE id=$1")
        .bind(endpoint.id)
        .execute(&pool)
        .await
        .unwrap();
    let app = api::gateway_router(gateway(pool.clone(), 5));
    for (ordinal, (option, include_usage)) in [
        (json!({"include_usage":true}), true),
        (json!({"include_usage":false}), false),
        (json!({}), false),
        (Value::Null, true),
    ]
    .into_iter()
    .enumerate()
    {
        let mut body = streaming_chat();
        body["stream_options"] = option;
        body["tools"] = json!([{"type":"function","function":{"name":"lookup","parameters":{"type":"object","properties":{}}}}]);
        let idem = format!("stream-options-{ordinal}");
        for _ in 0..2 {
            let response = app
                .clone()
                .oneshot(request(&key, &idem, body.clone(), "/v1/chat/completions"))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert!(response.headers().contains_key("x-brokerrouter-request-id"));
            let text = String::from_utf8(
                http_body_util::BodyExt::collect(response.into_body())
                    .await
                    .unwrap()
                    .to_bytes()
                    .to_vec(),
            )
            .unwrap();
            assert!(text.contains("private answer"));
            assert!(text.ends_with("data: [DONE]\n\n"));
            let chunks: Vec<Value> = text
                .lines()
                .filter_map(|line| line.strip_prefix("data: "))
                .filter(|line| *line != "[DONE]")
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(
                chunks
                    .iter()
                    .filter(|chunk| !chunk["usage"].is_null())
                    .count(),
                usize::from(include_usage)
            );
            assert_eq!(
                chunks
                    .iter()
                    .filter(|chunk| chunk["choices"] == json!([]))
                    .count(),
                usize::from(include_usage)
            );
            assert!(chunks.iter().all(|chunk| chunk["model"] == "logical/model"));
        }
        assert_eq!(upstream.count(), ordinal + 1);
        let settled: (String, i64, i64) = sqlx::query_as("SELECT status,final_micros,(SELECT count(*) FROM ledger_entries WHERE request_id=requests.id AND kind='finalize') FROM requests WHERE tenant_id=$1 AND idempotency_key=$2")
            .bind(who.tenant_id).bind(&idem).fetch_one(&pool).await.unwrap();
        assert_eq!(settled, ("succeeded".into(), 7, 1));
    }
    let sent = upstream.state.received.lock().unwrap();
    assert!(
        sent.iter()
            .all(|call| call["body"]["stream_options"] == json!({"include_usage":true}))
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn invalid_stream_options_create_no_billable_facts(pool: PgPool) {
    let upstream = Mock::start(Reply::Stream).await;
    let (who, key, _) = setup(&pool, "chat", &upstream.url).await;
    let app = api::gateway_router(gateway(pool.clone(), 5));
    let mut requests = Vec::new();
    for option in [
        json!({"include_usage":"true"}),
        json!({"include_usage":null}),
        json!({"include_usage":true,"unknown":false}),
        json!({"include_obfuscation":false}),
        json!([]),
        json!(false),
    ] {
        let mut body = streaming_chat();
        body["stream_options"] = option;
        requests.push(body);
    }
    let mut nonstream = chat();
    nonstream["stream_options"] = json!({"include_usage":true});
    requests.push(nonstream);
    for (ordinal, body) in requests.into_iter().enumerate() {
        let response = app
            .clone()
            .oneshot(request(
                &key,
                &format!("bad-stream-options-{ordinal}"),
                body,
                "/v1/chat/completions",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            json_body(response).await["error"]["code"],
            "invalid_request"
        );
    }
    assert_eq!(upstream.count(), 0);
    for table in ["requests", "attempts", "ledger_entries"] {
        let count: i64 =
            sqlx::query_scalar(&format!("SELECT count(*) FROM {table} WHERE tenant_id=$1"))
                .bind(who.tenant_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(count, 0);
    }
}
