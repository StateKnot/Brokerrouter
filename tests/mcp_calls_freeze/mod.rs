//! Actual MCP-purpose dispatch observes final-publication freezing. Include as
//! a child of mcp_calls.rs to reuse its pinned stdio executor and purpose pools.
use super::*;
use brokerrouter::{governance, runtime_publications};
use std::sync::atomic::AtomicUsize;

struct ModelSource {
    gateway: Gateway,
    request: Uuid,
    calls: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for ModelSource {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn model_source(pool: &PgPool, f: &Fixture) -> ModelSource {
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, Router::new().route("/v1/chat/completions", post(move || {
            let observed = observed.clone();
            async move {
                observed.fetch_add(1, Ordering::SeqCst);
                Json(json!({"id":"provider-id-is-not-the-source-request","object":"chat.completion","created":1,
                    "model":"upstream","choices":[{"index":0,"message":{"role":"assistant","content":"synthetic final source"},"finish_reason":"stop"}],
                    "usage":{"prompt_tokens":2,"completion_tokens":3,"total_tokens":5}}))
            }
        }))).await.unwrap();
    });
    store::add_endpoint(
        pool,
        &store::Endpoint {
            id: Uuid::new_v4(),
            tenant_id: f.who.tenant_id,
            model_id: "fixture/model".into(),
            name: "local-final-source".into(),
            upstream_model: "upstream".into(),
            base_url: url,
            credential_env: "FINAL_SOURCE_KEY".into(),
            allow_private: true,
            currency: "CNY".into(),
            input_per_million: money::nonnegative("0").unwrap(),
            output_per_million: money::nonnegative("0").unwrap(),
            context_tokens: 1000,
            max_output_tokens: 100,
            priority: 1,
            operation: "chat".into(),
            supports_tools: true,
            supports_streaming: false,
            embedding_dimension: None,
        },
    )
    .await
    .unwrap();
    let gateway = Gateway::new(
        scoped(pool, "brokerrouter_runtime").await,
        result_cipher(),
        HashMap::from([("FINAL_SOURCE_KEY".into(), "synthetic".into())]),
        4,
        5,
        60,
    )
    .unwrap();
    let (request, response) = gateway.execute_in_turn(&f.who, "native-final-source",
        json!({"model":"fixture/model","messages":[{"role":"user","content":"synthetic"}],"max_tokens":20}),
        "chat", Some(f.turn)).await.unwrap();
    assert!(response.is_ok());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    ModelSource {
        gateway,
        request,
        calls,
        task,
    }
}

async fn freeze_turn(pool: &PgPool, f: &Fixture) -> ModelSource {
    let source = model_source(pool, f).await;
    let saved = runtime_publications::record(
        &source.gateway.pool,
        &cipher(),
        &f.who,
        runtime_publications::Record {
            turn_id: f.turn,
            model_id: "fixture/model".into(),
            source_request_ids: vec![source.request],
            text: "exact captured final text".into(),
        },
    )
    .await
    .unwrap();
    assert_eq!(saved["state"], "recorded");
    assert!(saved.get("text").is_none());
    let frozen: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM runtime_output_set_freezes WHERE tenant_id=$1 AND turn_id=$2 AND input_operation_id IS NULL)")
        .bind(f.who.tenant_id).bind(f.turn).fetch_one(&source.gateway.pool).await.unwrap();
    assert!(frozen);
    source
}

fn edge(pool: PgPool) -> Edge {
    Edge::new(pool, cipher())
}
fn worker(pool: PgPool, f: &Fixture) -> Worker {
    Worker::new(
        pool,
        cipher(),
        cipher(),
        InjectedCredentialResolver::default(),
    )
    .with_stdio_registry_path(f.registry.clone())
}

#[sqlx::test(migrations = "./migrations")]
async fn actual_mcp_edge_cannot_enqueue_new_tool_input_after_final_freeze(pool: PgPool) {
    sqlx::raw_sql(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/deploy/permissions.sql"
    )))
    .execute(&pool)
    .await
    .unwrap();
    let f = fixture(&pool, "mcp-final-frozen-edge", false).await;
    let server_pool = scoped(&pool, "brokerrouter_mcp_server").await;
    let worker_pool = scoped(&pool, "brokerrouter_mcp_worker").await;
    let edge = edge(server_pool.clone());
    let worker = worker(worker_pool.clone(), &f);
    // Preserve a real positive purpose path, not a blanket denial of MCP.
    let completed = Uuid::new_v4();
    let completed_args = json!({"id":"completed-before-freeze"});
    assert!(matches!(
        edge.call(
            &f.who,
            f.turn,
            completed,
            f.tool.clone(),
            completed_args.clone()
        )
        .await,
        Err(Error::InProgress)
    ));
    assert!(worker.tick().await.unwrap());
    assert!(matches!(
        edge.call(
            &f.who,
            f.turn,
            completed,
            f.tool.clone(),
            completed_args.clone()
        )
        .await
        .unwrap(),
        CallToolResponse::Complete(_)
    ));
    assert_eq!(call_count(&f.marker), 1);
    let source = freeze_turn(&pool, &f).await;
    let later = Uuid::new_v4();
    assert!(
        matches!(
            edge.call(
                &f.who,
                f.turn,
                later,
                f.tool.clone(),
                json!({"id":"after-final-freeze"})
            )
            .await,
            Err(Error::Conflict)
        ),
        "final freeze must be a known rejection, not a SQL permission failure"
    );
    let queued: i64 =
        sqlx::query_scalar("SELECT count(*) FROM mcp_tool_calls WHERE tenant_id=$1 AND id=$2")
            .bind(f.who.tenant_id)
            .bind(later)
            .fetch_one(&server_pool)
            .await
            .unwrap();
    assert_eq!(queued, 0);
    assert_eq!(sqlx::query_scalar::<_,i64>("SELECT count(*) FROM governance_operations WHERE tenant_id=$1 AND operation_key=$2 AND stage='tool_input'")
        .bind(f.who.tenant_id).bind(format!("mcp-tool:{later}")).fetch_one(&server_pool).await.unwrap(), 0);
    assert!(!worker.tick().await.unwrap());
    // Original completed ACK/result remains readable under its current policy.
    assert!(matches!(
        edge.call(&f.who, f.turn, completed, f.tool.clone(), completed_args)
            .await
            .unwrap(),
        CallToolResponse::Complete(_)
    ));
    assert_eq!(call_count(&f.marker), 1);
    assert_eq!(source.calls.load(Ordering::SeqCst), 1);
    for sql in [
        "SELECT result_ciphertext FROM runtime_tool_results LIMIT 1",
        "SELECT result_ciphertext FROM runtime_final_publications LIMIT 1",
    ] {
        for purpose in [&server_pool, &worker_pool] {
            let error = sqlx::query(sql).fetch_all(purpose).await.unwrap_err();
            assert_eq!(
                error
                    .as_database_error()
                    .and_then(|error| error.code())
                    .as_deref(),
                Some("42501")
            );
        }
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn actual_queued_mcp_worker_records_known_non_dispatch_after_final_freeze(pool: PgPool) {
    sqlx::raw_sql(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/deploy/permissions.sql"
    )))
    .execute(&pool)
    .await
    .unwrap();
    let f = fixture(&pool, "mcp-final-frozen-worker", false).await;
    let server_pool = scoped(&pool, "brokerrouter_mcp_server").await;
    let worker_pool = scoped(&pool, "brokerrouter_mcp_worker").await;
    let edge = edge(server_pool.clone());
    let worker = worker(worker_pool.clone(), &f);
    let call = Uuid::new_v4();
    let args = json!({"id":"queued-before-final-freeze"});
    assert!(matches!(
        edge.call(&f.who, f.turn, call, f.tool.clone(), args.clone())
            .await,
        Err(Error::InProgress)
    ));
    let (input, hash): (Uuid, Vec<u8>) = sqlx::query_as("SELECT c.input_operation_id,o.payload_hash FROM mcp_tool_calls c JOIN governance_operations o ON o.tenant_id=c.tenant_id AND o.id=c.input_operation_id WHERE c.tenant_id=$1 AND c.id=$2")
        .bind(f.who.tenant_id).bind(call).fetch_one(&worker_pool).await.unwrap();
    let source = freeze_turn(&pool, &f).await;
    assert!(
        matches!(
            governance::claim(
                &worker_pool,
                &cipher(),
                &f.who,
                input,
                governance::Claim {
                    token: "77".repeat(32),
                    payload_hash: common::hex(&hash)
                }
            )
            .await,
            Err(Error::Conflict)
        ),
        "queued worker purpose must observe the final fence before authorizing dispatch"
    );
    assert!(
        worker.tick().await.unwrap(),
        "known freeze denial must be durably recorded, not execution_unknown"
    );
    let facts: (String, Option<String>, bool) = sqlx::query_as("SELECT state,failure_code,completed_at IS NOT NULL FROM mcp_tool_calls WHERE tenant_id=$1 AND id=$2")
        .bind(f.who.tenant_id).bind(call).fetch_one(&worker_pool).await.unwrap();
    assert_eq!(
        facts,
        (
            "failed".into(),
            Some("governance_claim_denied".into()),
            true
        )
    );
    let claim: (String, bool, bool) = sqlx::query_as("SELECT state,claim_hash IS NULL,execution_deadline IS NULL FROM governance_operations WHERE tenant_id=$1 AND id=$2")
        .bind(f.who.tenant_id).bind(input).fetch_one(&worker_pool).await.unwrap();
    assert_eq!(claim, ("ready".into(), true, true));
    assert_eq!(sqlx::query_scalar::<_,i64>("SELECT count(*) FROM audit_events WHERE tenant_id=$1 AND object_id=$2 AND action='mcp.tool_call_not_submitted'")
        .bind(f.who.tenant_id).bind(call.to_string()).fetch_one(&pool).await.unwrap(), 1);
    assert_eq!(call_count(&f.marker), 0);
    assert!(!worker.tick().await.unwrap());
    assert_eq!(call_count(&f.marker), 0);
    assert_eq!(source.calls.load(Ordering::SeqCst), 1);
}
