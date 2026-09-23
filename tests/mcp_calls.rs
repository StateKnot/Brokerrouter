mod common;

use axum::{body::Body, http::Request};
use brokerrouter::{
    cipher::ResultCipher,
    error::Error,
    mcp_calls::{Edge, Worker},
    mcp_catalog::{
        self, Authentication, CacheScope, RecordDiscovery, RegisterServer, SetToolGrant, Transport,
        UpdateServer,
    },
    mcp_client::InjectedCredentialResolver,
    mcp_server,
    store::{self, Identity},
};
use http_body_util::BodyExt;
use rmcp::model::{CallToolResponse, Tool};
use serde_json::{Value, json};
use sqlx::{PgPool, postgres::PgPoolOptions};
use std::{collections::HashMap, path::PathBuf, sync::Arc};
use tower::ServiceExt;
use uuid::Uuid;

fn cipher() -> Arc<ResultCipher> {
    Arc::new(
        ResultCipher::new(
            "mcp-fixture".into(),
            HashMap::from([("mcp-fixture".into(), "45".repeat(32))]),
        )
        .unwrap(),
    )
}

struct Fixture {
    who: Identity,
    token: String,
    turn: Uuid,
    tool: mcp_catalog::AuthorizedTool,
    edge: Edge,
    worker: Worker,
    registry: PathBuf,
    marker: PathBuf,
}

async fn fixture(pool: &PgPool, label: &str, close_on_call: bool) -> Fixture {
    let tenant = store::create_tenant(pool, label, 0).await.unwrap();
    store::put_model(pool, tenant, "fixture/model", "Fixture model")
        .await
        .unwrap();
    let (key_id, token) = store::create_key(
        pool,
        tenant,
        "MCP fixture",
        0,
        &["fixture/model".into()],
        None,
    )
    .await
    .unwrap();
    let who = Identity {
        tenant_id: tenant,
        key_id,
    };
    common::provision(pool, &who, "alice").await;
    let turn = common::turn(pool, &who, "alice", 0).await;
    let config_cipher = cipher();
    let transport = Transport::Stdio {
        launcher_profile: "fixture".into(),
    };
    let registered = mcp_catalog::register_server(
        pool,
        &config_cipher,
        RegisterServer {
            tenant_id: tenant,
            operation_id: Uuid::new_v4(),
            actor: "operator:fixture".into(),
            reason: "isolated MCP test".into(),
            display_name: "Fixture tools".into(),
            transport: transport.clone(),
            authentication: Authentication::None,
        },
    )
    .await
    .unwrap();
    let server_id: Uuid = serde_json::from_value(registered["server_id"].clone()).unwrap();
    let updated = mcp_catalog::update_server(
        pool,
        &config_cipher,
        UpdateServer {
            tenant_id: tenant,
            server_id,
            operation_id: Uuid::new_v4(),
            actor: "operator:fixture".into(),
            reason: "activate pinned fixture".into(),
            expected_revision: 1,
            display_name: "Fixture tools".into(),
            transport,
            authentication: Authentication::None,
            active: true,
        },
    )
    .await
    .unwrap();
    assert_eq!(updated["revision"], "2");
    let descriptor: Value = serde_json::to_value(
        serde_json::from_value::<Tool>(json!({
            "name":"write_record",
            "description":"Writes one fixture record",
            "inputSchema":{"type":"object","properties":{"id":{"type":"string"}},"required":["id"],"additionalProperties":false}
        }))
        .unwrap(),
    )
    .unwrap();
    mcp_catalog::record_discovery(
        pool,
        RecordDiscovery {
            tenant_id: tenant,
            server_id,
            operation_id: Uuid::new_v4(),
            actor: "connector:fixture".into(),
            expected_server_revision: 2,
            protocol_version: mcp_catalog::PROTOCOL_VERSION.into(),
            result_type: "complete".into(),
            supported_versions: vec![mcp_catalog::PROTOCOL_VERSION.into()],
            capabilities: json!({"tools":{}}),
            server_info: Some(json!({"name":"fixture","version":"1"})),
            instructions: None,
            ttl_ms: 300_000,
            cache_scope: CacheScope::Private,
            tools: vec![descriptor.clone()],
            tools_ttl_ms: Some(300_000),
            tools_cache_scope: Some(CacheScope::Private),
            skills: vec![],
            skills_ttl_ms: None,
            skills_cache_scope: None,
            authorization_context_fingerprint: Some("a".repeat(64)),
        },
    )
    .await
    .unwrap();
    let review = mcp_catalog::review_tools(pool, tenant, server_id)
        .await
        .unwrap();
    let downstream = review["tools"][0]["downstream_name"]
        .as_str()
        .unwrap()
        .to_owned();
    mcp_catalog::set_tool_grant(
        pool,
        SetToolGrant {
            tenant_id: tenant,
            key_id,
            server_id,
            operation_id: Uuid::new_v4(),
            actor: "operator:fixture".into(),
            reason: "reviewed exact fixture descriptor".into(),
            downstream_name: downstream.clone(),
            descriptor_sha256: review["tools"][0]["descriptor_sha256"]
                .as_str()
                .unwrap()
                .into(),
            active: true,
        },
    )
    .await
    .unwrap();
    let tool = mcp_catalog::authorized_tool(pool, &who, &downstream)
        .await
        .unwrap()
        .unwrap();

    let directory = std::env::current_dir()
        .unwrap()
        .join("target")
        .join(format!("mcp-call-fixture-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    let marker = directory.join("calls");
    let registry = directory.join("registry.json");
    let shell = PathBuf::from("/bin/sh");
    let program_hash = store::hash(&std::fs::read(&shell).unwrap())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let call_response = if close_on_call {
        format!("printf 'x\\n' >> '{}'; exit 0", marker.display())
    } else {
        format!(
            "printf 'x\\n' >> '{}'; printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":'\"$id\"',\"result\":{{\"resultType\":\"complete\",\"content\":[{{\"type\":\"text\",\"text\":\"done\"}}],\"isError\":false}}}}'",
            marker.display()
        )
    };
    let script = format!(
        r#"while IFS= read -r line; do
id=$(printf '%s\n' "$line" | /usr/bin/sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
case "$line" in
  *'"method":"server/discover"'*)
    printf '%s\n' '{{"jsonrpc":"2.0","id":'"$id"',"result":{{"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{{"tools":{{}}}},"ttlMs":300000,"cacheScope":"private","_meta":{{"io.modelcontextprotocol/serverInfo":{{"name":"fixture","version":"1"}}}}}}}}'
    ;;
  *'"method":"tools/list"'*)
    printf '%s\n' '{{"jsonrpc":"2.0","id":'"$id"',"result":{{"resultType":"complete","tools":[{}],"ttlMs":300000,"cacheScope":"private"}}}}'
    ;;
  *'"method":"tools/call"'*)
    {}
    ;;
esac
done"#,
        serde_json::to_string(&descriptor).unwrap(),
        call_response
    );
    std::fs::write(
        &registry,
        serde_json::to_vec(&json!({
            "version":1,
            "profiles":[{
                "name":"fixture",
                "program":shell,
                "program_sha256":program_hash,
                "working_directory":directory,
                "arguments":["-c",script],
                "environment":[],
                "timeout_ms":5000
            }]
        }))
        .unwrap(),
    )
    .unwrap();
    Fixture {
        who,
        token,
        turn,
        tool,
        edge: Edge::new(pool.clone(), cipher()),
        worker: Worker::new(
            pool.clone(),
            cipher(),
            config_cipher,
            InjectedCredentialResolver::default(),
        )
        .with_stdio_registry_path(registry.clone()),
        registry,
        marker,
    }
}

async fn scoped(pool: &PgPool, role: &'static str) -> PgPool {
    PgPoolOptions::new()
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
async fn least_privilege_mcp_roles_execute_without_crossing_secret_or_approval_boundaries(
    pool: PgPool,
) {
    sqlx::raw_sql(include_str!("../deploy/permissions.sql"))
        .execute(&pool)
        .await
        .unwrap();
    let fixture = fixture(&pool, "mcp-roles-call", false).await;
    let edge_pool = scoped(&pool, "brokerrouter_mcp_server").await;
    let worker_pool = scoped(&pool, "brokerrouter_mcp_worker").await;
    brokerrouter::check_schema(&edge_pool).await.unwrap();
    brokerrouter::check_schema(&worker_pool).await.unwrap();
    let edge = Edge::new(edge_pool.clone(), cipher());
    let worker = Worker::new(
        worker_pool.clone(),
        cipher(),
        cipher(),
        InjectedCredentialResolver::default(),
    )
    .with_stdio_registry_path(fixture.registry.clone());
    let call_id = Uuid::new_v4();
    let args = json!({"id":"restricted-role"});
    assert!(matches!(
        edge.call(
            &fixture.who,
            fixture.turn,
            call_id,
            fixture.tool.clone(),
            args.clone()
        )
        .await,
        Err(Error::InProgress)
    ));
    assert!(worker.tick().await.unwrap());
    assert!(matches!(
        edge.call(
            &fixture.who,
            fixture.turn,
            call_id,
            fixture.tool.clone(),
            args
        )
        .await
        .unwrap(),
        CallToolResponse::Complete(_)
    ));
    assert_eq!(call_count(&fixture.marker), 1);
    assert!(sqlx::query("UPDATE mcp_tool_calls SET response_hash=decode(repeat('cd',32),'hex') WHERE tenant_id=$1 AND id=$2")
        .bind(fixture.who.tenant_id).bind(call_id).execute(&worker_pool).await.is_err());
    assert!(sqlx::query("UPDATE mcp_tool_calls SET state='expired',response_kind=NULL,response_hash=NULL,response_ciphertext=NULL WHERE tenant_id=$1 AND id=$2")
        .bind(fixture.who.tenant_id).bind(call_id).execute(&worker_pool).await.is_err());
    for sql in [
        "SELECT credential_ref_ciphertext FROM mcp_servers",
        "UPDATE approvals SET state='approved'",
        "UPDATE mcp_key_tool_grants SET active=false",
    ] {
        assert!(sqlx::query(sql).execute(&edge_pool).await.is_err(), "{sql}");
    }
    let can_enqueue: bool =
        sqlx::query_scalar("SELECT has_table_privilege(current_user,'mcp_tool_calls','INSERT')")
            .fetch_one(&worker_pool)
            .await
            .unwrap();
    assert!(!can_enqueue);
    for sql in [
        "UPDATE approvals SET state='approved'",
        "UPDATE mcp_key_tool_grants SET active=false",
    ] {
        assert!(
            sqlx::query(sql).execute(&worker_pool).await.is_err(),
            "{sql}"
        );
    }
}

fn call_count(marker: &PathBuf) -> usize {
    std::fs::read_to_string(marker)
        .map(|value| value.lines().count())
        .unwrap_or(0)
}

fn mcp_call_request(fixture: &Fixture, call_id: Uuid, with_turn: bool) -> Request<Body> {
    let mut meta = json!({
        "io.modelcontextprotocol/protocolVersion":"2026-07-28",
        "io.modelcontextprotocol/clientInfo":{"name":"fixture","version":"1.0.0"},
        "io.modelcontextprotocol/clientCapabilities":{},
        "io.brokerrouter/callId":call_id.to_string(),
    });
    if with_turn {
        meta["io.brokerrouter/turnId"] = fixture.turn.to_string().into();
    }
    let body = json!({
        "jsonrpc":"2.0",
        "id":1,
        "method":"tools/call",
        "params":{
            "name":fixture.tool.downstream_name,
            "arguments":{"id":"http-wire"},
            "_meta":meta,
        },
    });
    Request::post("/mcp")
        .header("host", "gateway.example")
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", "2026-07-28")
        .header("mcp-method", "tools/call")
        .header("mcp-name", &fixture.tool.downstream_name)
        .header("authorization", format!("Bearer {}", fixture.token))
        .body(Body::from(body.to_string()))
        .unwrap()
}

#[sqlx::test(migrations = "./migrations")]
async fn real_downstream_http_call_uses_durable_governance_and_idempotent_release(pool: PgPool) {
    let fixture = fixture(&pool, "mcp-http-call", false).await;
    let config = mcp_server::HttpConfig::new(vec!["gateway.example".into()], vec![]).unwrap();
    let app = mcp_server::router(pool.clone(), config, None, cipher()).unwrap();
    let call_id = Uuid::new_v4();
    let missing_turn = app
        .clone()
        .oneshot(mcp_call_request(&fixture, call_id, false))
        .await
        .unwrap();
    let missing: Value =
        serde_json::from_slice(&missing_turn.into_body().collect().await.unwrap().to_bytes())
            .unwrap();
    assert!(missing.get("error").is_some());
    assert_eq!(call_count(&fixture.marker), 0);

    let pending = app
        .clone()
        .oneshot(mcp_call_request(&fixture, call_id, true))
        .await
        .unwrap();
    assert_eq!(pending.status(), 200);
    let pending: Value =
        serde_json::from_slice(&pending.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(pending["error"]["data"]["code"], "tool_call_pending");
    assert!(fixture.worker.tick().await.unwrap());

    for _ in 0..2 {
        let response = app
            .clone()
            .oneshot(mcp_call_request(&fixture, call_id, true))
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body: Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(body["result"]["resultType"], "complete");
        assert_eq!(body["result"]["content"][0]["text"], "done");
    }
    assert_eq!(call_count(&fixture.marker), 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn governed_tool_call_executes_once_and_releases_only_after_output_approval(pool: PgPool) {
    let fixture = fixture(&pool, "mcp-call", false).await;
    common::guardrail(
        &pool,
        fixture.who.tenant_id,
        "review-output",
        1,
        "tool_output",
        json!({"kind":"always"}),
    )
    .await;
    let call_id = Uuid::new_v4();
    let args = json!({"id":"record-1"});
    assert!(matches!(
        fixture
            .edge
            .call(
                &fixture.who,
                fixture.turn,
                call_id,
                fixture.tool.clone(),
                args.clone()
            )
            .await,
        Err(Error::InProgress)
    ));
    assert!(fixture.worker.tick().await.unwrap());
    assert_eq!(call_count(&fixture.marker), 1);
    let pending = fixture
        .edge
        .call(
            &fixture.who,
            fixture.turn,
            call_id,
            fixture.tool.clone(),
            args.clone(),
        )
        .await;
    assert!(matches!(pending, Err(Error::ApprovalRequired(_))));
    let approval_id: Uuid = sqlx::query_scalar(
        "SELECT id FROM approvals WHERE tenant_id=$1 AND turn_id=$2 AND state='pending'",
    )
    .bind(fixture.who.tenant_id)
    .bind(fixture.turn)
    .fetch_one(&pool)
    .await
    .unwrap();
    common::decide(
        &pool,
        "alice",
        approval_id,
        common::approve(&["review-output"]),
    )
    .await
    .unwrap();
    let result = fixture
        .edge
        .call(
            &fixture.who,
            fixture.turn,
            call_id,
            fixture.tool.clone(),
            args.clone(),
        )
        .await
        .unwrap();
    assert!(matches!(result, CallToolResponse::Complete(_)));
    let repeated = fixture
        .edge
        .call(
            &fixture.who,
            fixture.turn,
            call_id,
            fixture.tool.clone(),
            args,
        )
        .await
        .unwrap();
    assert!(matches!(repeated, CallToolResponse::Complete(_)));
    assert_eq!(call_count(&fixture.marker), 1);
    assert!(matches!(
        fixture
            .edge
            .call(
                &fixture.who,
                fixture.turn,
                call_id,
                fixture.tool.clone(),
                json!({"id":"different"})
            )
            .await,
        Err(Error::Conflict)
    ));
}

#[sqlx::test(migrations = "./migrations")]
async fn input_approval_and_grant_revocation_prevent_upstream_dispatch(pool: PgPool) {
    let fixture = fixture(&pool, "mcp-input", false).await;
    common::guardrail(
        &pool,
        fixture.who.tenant_id,
        "review-input",
        1,
        "tool_input",
        json!({"kind":"always"}),
    )
    .await;
    let call_id = Uuid::new_v4();
    let args = json!({"id":"protected"});
    let pending = fixture
        .edge
        .call(
            &fixture.who,
            fixture.turn,
            call_id,
            fixture.tool.clone(),
            args.clone(),
        )
        .await;
    assert!(matches!(pending, Err(Error::ApprovalRequired(_))));
    assert!(!fixture.worker.tick().await.unwrap());
    assert_eq!(call_count(&fixture.marker), 0);
    let approval_id: Uuid = sqlx::query_scalar(
        "SELECT id FROM approvals WHERE tenant_id=$1 AND turn_id=$2 AND state='pending'",
    )
    .bind(fixture.who.tenant_id)
    .bind(fixture.turn)
    .fetch_one(&pool)
    .await
    .unwrap();
    common::decide(
        &pool,
        "alice",
        approval_id,
        common::approve(&["review-input"]),
    )
    .await
    .unwrap();
    assert!(matches!(
        fixture
            .edge
            .call(
                &fixture.who,
                fixture.turn,
                call_id,
                fixture.tool.clone(),
                args
            )
            .await,
        Err(Error::InProgress)
    ));
    sqlx::query("UPDATE mcp_key_tool_grants SET active=false WHERE tenant_id=$1 AND key_id=$2")
        .bind(fixture.who.tenant_id)
        .bind(fixture.who.key_id)
        .execute(&pool)
        .await
        .unwrap();
    assert!(fixture.worker.tick().await.unwrap());
    assert_eq!(call_count(&fixture.marker), 0);
    let state: String =
        sqlx::query_scalar("SELECT state FROM mcp_tool_calls WHERE tenant_id=$1 AND id=$2")
            .bind(fixture.who.tenant_id)
            .bind(call_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(state, "failed");
}

#[sqlx::test(migrations = "./migrations")]
async fn lost_tool_response_is_execution_unknown_and_never_redispatched(pool: PgPool) {
    let fixture = fixture(&pool, "mcp-unknown", true).await;
    let call_id = Uuid::new_v4();
    let args = json!({"id":"uncertain"});
    assert!(matches!(
        fixture
            .edge
            .call(
                &fixture.who,
                fixture.turn,
                call_id,
                fixture.tool.clone(),
                args.clone()
            )
            .await,
        Err(Error::InProgress)
    ));
    assert!(fixture.worker.tick().await.unwrap());
    assert_eq!(call_count(&fixture.marker), 1);
    assert!(matches!(
        fixture
            .edge
            .call(
                &fixture.who,
                fixture.turn,
                call_id,
                fixture.tool.clone(),
                args
            )
            .await,
        Err(Error::UpstreamUnknown)
    ));
    assert!(!fixture.worker.tick().await.unwrap());
    assert_eq!(call_count(&fixture.marker), 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn expired_worker_lease_is_audited_and_never_redispatched(pool: PgPool) {
    sqlx::raw_sql(include_str!("../deploy/permissions.sql"))
        .execute(&pool)
        .await
        .unwrap();
    let fixture = fixture(&pool, "mcp-expired-lease", false).await;
    let call_id = Uuid::new_v4();
    let args = json!({"id":"lease-expired"});
    assert!(matches!(
        fixture
            .edge
            .call(
                &fixture.who,
                fixture.turn,
                call_id,
                fixture.tool.clone(),
                args.clone(),
            )
            .await,
        Err(Error::InProgress)
    ));
    sqlx::query("UPDATE mcp_tool_calls SET state='authorizing',claim_hash=decode(repeat('ab',32),'hex'),execution_deadline=clock_timestamp()-interval '1 second' WHERE tenant_id=$1 AND id=$2")
        .bind(fixture.who.tenant_id).bind(call_id).execute(&pool).await.unwrap();
    let worker_pool = scoped(&pool, "brokerrouter_mcp_worker").await;
    let worker = Worker::new(
        worker_pool,
        cipher(),
        cipher(),
        InjectedCredentialResolver::default(),
    )
    .with_stdio_registry_path(fixture.registry.clone());
    assert!(!worker.tick().await.unwrap());
    assert_eq!(call_count(&fixture.marker), 0);
    assert!(matches!(
        fixture
            .edge
            .call(&fixture.who, fixture.turn, call_id, fixture.tool, args)
            .await,
        Err(Error::UpstreamUnknown)
    ));
    let audit_count: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE tenant_id=$1 AND object_id=$2 AND action='mcp.tool_call_execution_unknown'")
        .bind(fixture.who.tenant_id).bind(call_id.to_string()).fetch_one(&pool).await.unwrap();
    assert_eq!(audit_count, 1);
}
