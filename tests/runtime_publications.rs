mod common;
use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    routing::post,
};
use brokerrouter::{
    api,
    error::Error,
    gateway::Gateway,
    governance, money,
    runtime_publications::{self, Record},
    store,
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use tower::ServiceExt;
use uuid::Uuid;

struct Fixture {
    who: store::Identity,
    token: String,
    turn: Uuid,
    gateway: Gateway,
    request: Uuid,
    calls: Arc<AtomicUsize>,
    subject: String,
    url: String,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Fixture {
    fn record(&self) -> Record {
        Record {
            turn_id: self.turn,
            model_id: "model".into(),
            source_request_ids: vec![self.request],
            text: "final 中文\n加工后的正文\u{1f40e}".into(),
        }
    }
}
async fn setup(pool: &PgPool) -> Fixture {
    let tenant = store::create_tenant(pool, "final-publication", 1_000_000)
        .await
        .unwrap();
    store::put_model(pool, tenant, "model", "Model")
        .await
        .unwrap();
    let (key, token) =
        store::create_key(pool, tenant, "runtime", 1_000_000, &["model".into()], None)
            .await
            .unwrap();
    let who = store::Identity {
        tenant_id: tenant,
        key_id: key,
    };
    let subject = Uuid::new_v4().to_string();
    common::provision(pool, &who, &subject).await;
    let turn = common::turn(pool, &who, &subject, 0).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let fixture=Router::new().route("/v1/chat/completions",post(|State(calls):State<Arc<AtomicUsize>>,Json(body):Json<Value>|async move {
        calls.fetch_add(1,Ordering::SeqCst);
        let content=if body["messages"][0]["content"].as_str().is_some_and(|text|text.starts_with("You are a security classifier")) {
            "{\"decision\":\"risk\"}"
        } else {"raw provider answer"};
        Json(json!({"id":"provider-id-is-not-service-id","object":"chat.completion","created":1,
            "model":"upstream","choices":[{"index":0,"message":{"role":"assistant","content":content},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":2,"completion_tokens":3,"total_tokens":5}}))
    })).with_state(calls.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, fixture).await.unwrap();
    });
    store::add_endpoint(
        pool,
        &store::Endpoint {
            id: Uuid::new_v4(),
            tenant_id: tenant,
            model_id: "model".into(),
            name: "local-fixture".into(),
            upstream_model: "upstream".into(),
            base_url: url.clone(),
            credential_env: "FIXTURE_KEY".into(),
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
        pool.clone(),
        common::cipher(),
        HashMap::from([("FIXTURE_KEY".into(), "synthetic".into())]),
        4,
        5,
        60,
    )
    .unwrap();
    let (request, response) = gateway
        .execute_in_turn(
            &who,
            "model-call-1",
            json!({"model":"model",
        "messages":[{"role":"user","content":"test"}],"max_tokens":20}),
            "chat",
            Some(turn),
        )
        .await
        .unwrap();
    assert!(response.is_ok());
    Fixture {
        who,
        token,
        turn,
        gateway,
        request,
        calls,
        subject,
        url,
        task,
    }
}
fn id(value: &Value) -> Uuid {
    serde_json::from_value(value["id"].clone()).unwrap()
}

async fn approval_http(pool: &PgPool, f: &Fixture, path: &str, body: Value) -> (StatusCode, Value) {
    let router = brokerrouter::approvals_api::router(brokerrouter::governance_api::Control {
        pool: pool.clone(),
        verifier: Arc::new(common::approvals_verifier()),
        cipher: Arc::new(common::cipher()),
    });
    let token = common::approvals_sign(common::claims(f.who.tenant_id, &f.subject, path, &body));
    let response = router
        .oneshot(
            Request::post(path)
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    assert_eq!(response.headers()["cache-control"], "no-store");
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}

async fn reviewed_decide(pool: &PgPool, f: &Fixture, pending: &Value) {
    let approval = common::approval_id(pending);
    let (status, reviewed) = approval_http(
        pool,
        f,
        "/approvals/v1/review",
        json!({"approval_id":approval}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{reviewed}");
    assert_eq!(reviewed["can_continue"], true, "{reviewed}");
    let payload: Value = serde_json::from_str(reviewed["payload_text"].as_str().unwrap()).unwrap();
    assert_eq!(payload["text"], f.record().text);
    assert_eq!(payload["source_request_ids"], json!([f.request]));
    let (status, decision) = approval_http(
        pool,
        f,
        "/approvals/v1/decide",
        json!({
        "approval_id":approval,"review_id":reviewed["review_id"],"acknowledged_scope":true,
        "decision":common::approve(&["final-review"])}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{decision}");
}

#[sqlx::test(migrations = "./migrations")]
async fn native_final_text_is_immutable_and_distinct_from_provider_response(pool: PgPool) {
    let f = setup(&pool).await;
    let request = f.record();
    let stored = runtime_publications::record(&pool, &common::cipher(), &f.who, request.clone())
        .await
        .unwrap();
    assert!(stored.get("text").is_none());
    assert_eq!(
        stored["text_sha256"],
        common::hex(&store::hash(request.text.as_bytes()))
    );
    let publication = id(&stored);
    let again = runtime_publications::record(&pool, &common::cipher(), &f.who, request.clone())
        .await
        .unwrap();
    assert_eq!(stored, again);
    let mut changed = request.clone();
    changed.text.push('!');
    assert!(matches!(
        runtime_publications::record(&pool, &common::cipher(), &f.who, changed).await,
        Err(Error::Conflict)
    ));
    let (encrypted,expiry_bound):(Vec<u8>,bool)=sqlx::query_as("SELECT p.result_ciphertext,p.expires_at<=r.result_expires_at FROM runtime_final_publications p JOIN requests r ON r.tenant_id=p.tenant_id AND r.id=$2 WHERE p.id=$1")
        .bind(publication).bind(f.request).fetch_one(&pool).await.unwrap();
    assert!(expiry_bound);
    assert_eq!(
        common::cipher()
            .decrypt(f.who.tenant_id, publication, &encrypted)
            .unwrap(),
        request.text.as_bytes()
    );
    let ready = runtime_publications::release(&f.gateway, &f.who, publication)
        .await
        .unwrap();
    assert_eq!(ready["state"], "ready");
    assert_eq!(ready["text"], request.text);
    let stage: String = sqlx::query_scalar("SELECT stage FROM governance_operations WHERE id=$1")
        .bind(id(&ready))
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(stage, "post");
    assert_eq!(f.calls.load(Ordering::SeqCst), 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn source_identity_and_list_cannot_be_forged(pool: PgPool) {
    let f = setup(&pool).await;
    let other = setup(&pool).await;
    for ids in [
        vec![],
        vec![f.request, f.request],
        vec![Uuid::new_v4()],
        vec![other.request],
    ] {
        let mut request = f.record();
        request.source_request_ids = ids;
        assert!(
            runtime_publications::record(&pool, &common::cipher(), &f.who, request)
                .await
                .is_err()
        );
    }
    let mut wrong = f.record();
    wrong.model_id = "another-model".into();
    assert!(
        runtime_publications::record(&pool, &common::cipher(), &f.who, wrong)
            .await
            .is_err()
    );
    let next = common::turn(&pool, &f.who, &f.subject, 1).await;
    let mut wrong = f.record();
    wrong.turn_id = next;
    assert!(matches!(
        runtime_publications::record(&pool, &common::cipher(), &f.who, wrong).await,
        Err(Error::ResultUnavailable)
    ));
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM runtime_final_publications")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    assert!(
        runtime_publications::record(&pool, &common::cipher(), &other.who, f.record())
            .await
            .is_err()
    );
    assert_eq!(f.calls.load(Ordering::SeqCst), 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn current_post_policy_and_version_gate_the_transformed_text(pool: PgPool) {
    let f = setup(&pool).await;
    let stored = runtime_publications::record(&pool, &common::cipher(), &f.who, f.record())
        .await
        .unwrap();
    let publication = id(&stored);
    common::guardrail(
        &pool,
        f.who.tenant_id,
        "final-review",
        1,
        "post",
        json!({"kind":"always"}),
    )
    .await;
    let pending = runtime_publications::release(&f.gateway, &f.who, publication)
        .await
        .unwrap();
    assert_eq!(pending["state"], "waiting_approval");
    assert!(pending.get("text").is_none());
    assert_eq!(pending["approval"]["summary"]["stage"], "post");
    reviewed_decide(&pool, &f, &pending).await;
    assert_eq!(
        runtime_publications::release(&f.gateway, &f.who, publication)
            .await
            .unwrap()["text"],
        f.record().text
    );
    common::guardrail(
        &pool,
        f.who.tenant_id,
        "final-review",
        2,
        "post",
        json!({"kind":"always"}),
    )
    .await;
    let upgraded = runtime_publications::release(&f.gateway, &f.who, publication)
        .await
        .unwrap();
    assert_eq!(upgraded["state"], "waiting_approval");
    assert!(upgraded.get("text").is_none());
    assert_ne!(pending["approval"]["id"], upgraded["approval"]["id"]);
    reviewed_decide(&pool, &f, &upgraded).await;
    let ready = runtime_publications::release(&f.gateway, &f.who, publication)
        .await
        .unwrap();
    assert_eq!(ready["state"], "ready");
    assert_eq!(id(&ready), id(&pending));
    assert_eq!(f.calls.load(Ordering::SeqCst), 1);
    let counts:(i64,i64,i64)=sqlx::query_as("SELECT (SELECT count(*) FROM runtime_final_publications),(SELECT count(*) FROM requests),(SELECT count(*) FROM approvals)").fetch_one(&pool).await.unwrap();
    assert_eq!(counts, (1, 1, 2));
}

#[sqlx::test(migrations = "./migrations")]
async fn deployed_workbench_role_reviews_exact_final_facts_without_ciphertext_access(pool: PgPool) {
    let f = setup(&pool).await;
    sqlx::raw_sql(include_str!("../deploy/permissions.sql"))
        .execute(&pool)
        .await
        .unwrap();
    let publication =
        id(
            &runtime_publications::record(&pool, &common::cipher(), &f.who, f.record())
                .await
                .unwrap(),
        );
    common::guardrail(
        &pool,
        f.who.tenant_id,
        "final-review",
        1,
        "post",
        json!({"kind":"always"}),
    )
    .await;
    let pending = runtime_publications::release(&f.gateway, &f.who, publication)
        .await
        .unwrap();
    let restricted = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .after_connect(|c, _| {
            Box::pin(async move {
                sqlx::query("SET SESSION AUTHORIZATION brokerrouter_approvals")
                    .execute(c)
                    .await?;
                Ok(())
            })
        })
        .connect_with(pool.connect_options().as_ref().clone())
        .await
        .unwrap();
    brokerrouter::check_schema(&restricted).await.unwrap();
    reviewed_decide(&restricted, &f, &pending).await;
    let ready = runtime_publications::release(&f.gateway, &f.who, publication)
        .await
        .unwrap();
    assert_eq!(ready["text"], f.record().text);
    for sql in [
        "SELECT result_ciphertext FROM runtime_final_publications",
        "SELECT result_ciphertext FROM requests",
        "UPDATE runtime_final_publications SET result_ciphertext=NULL",
        "UPDATE runtime_final_publications SET text_sha256=text_sha256",
        "DELETE FROM runtime_final_publications",
    ] {
        let error = sqlx::query(sql).execute(&restricted).await.unwrap_err();
        assert_eq!(
            error.as_database_error().unwrap().code().as_deref(),
            Some("42501"),
            "{sql}"
        );
    }
    let audit: Value =
        sqlx::query_scalar("SELECT details FROM audit_events WHERE action='approval.reviewed'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!audit.to_string().contains(&f.record().text));
    assert_eq!(f.calls.load(Ordering::SeqCst), 1);
    restricted.close().await;
}

#[sqlx::test(migrations = "./migrations")]
async fn final_review_rejects_forged_shape_and_missing_original_source(pool: PgPool) {
    let f = setup(&pool).await;
    let publication =
        id(
            &runtime_publications::record(&pool, &common::cipher(), &f.who, f.record())
                .await
                .unwrap(),
        );
    common::guardrail(
        &pool,
        f.who.tenant_id,
        "final-review",
        1,
        "post",
        json!({"kind":"always"}),
    )
    .await;
    // Even an internal operation with a genuine publication ID is not reviewable
    // when its body was not produced by the dedicated exact-facts bridge.
    let forged = governance::check(
        &pool,
        &common::cipher(),
        &f.who,
        governance::Check {
            turn_id: f.turn,
            operation_key: format!("runtime-final-output:{publication}"),
            stage: "post".into(),
            model_id: Some("model".into()),
            tool_name: None,
            payload: json!({"text":f.record().text,"choices":[{"message":{"content":"wrong"}}]}),
        },
    )
    .await
    .unwrap();
    let (status, _) = approval_http(
        &pool,
        &f,
        "/approvals/v1/review",
        json!({"approval_id":common::approval_id(&forged)}),
    )
    .await;
    assert_eq!(status, StatusCode::GONE);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM approval_reviews")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    let g = setup(&pool).await;
    let publication =
        id(
            &runtime_publications::record(&pool, &common::cipher(), &g.who, g.record())
                .await
                .unwrap(),
        );
    common::guardrail(
        &pool,
        g.who.tenant_id,
        "final-review",
        1,
        "post",
        json!({"kind":"always"}),
    )
    .await;
    let pending = runtime_publications::release(&g.gateway, &g.who, publication)
        .await
        .unwrap();
    let (status, reviewed) = approval_http(
        &pool,
        &g,
        "/approvals/v1/review",
        json!({"approval_id":common::approval_id(&pending)}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{reviewed}");
    assert_eq!(reviewed["can_continue"], true);
    sqlx::query("UPDATE requests SET result_ciphertext=NULL WHERE id=$1")
        .bind(g.request)
        .execute(&pool)
        .await
        .unwrap();
    let (status, _) = approval_http(
        &pool,
        &g,
        "/approvals/v1/decide",
        json!({
        "approval_id":common::approval_id(&pending),"review_id":reviewed["review_id"],
        "acknowledged_scope":true,"decision":common::approve(&["final-review"])}),
    )
    .await;
    assert_eq!(status, StatusCode::GONE);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM guardrail_grants")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        approval_http(
            &pool,
            &g,
            "/approvals/v1/review",
            json!({"approval_id":common::approval_id(&pending)})
        )
        .await
        .0,
        StatusCode::GONE
    );
    assert_eq!(g.calls.load(Ordering::SeqCst), 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn source_expiring_during_real_decision_audit_rolls_back_final_grants(pool: PgPool) {
    let f = setup(&pool).await;
    let _ = common::approvals_sign(common::claims(
        f.who.tenant_id,
        &f.subject,
        "/approvals/v1/review",
        &json!({}),
    ));
    sqlx::query(
        "UPDATE requests SET result_expires_at=clock_timestamp()+interval '6 seconds' WHERE id=$1",
    )
    .bind(f.request)
    .execute(&pool)
    .await
    .unwrap();
    let publication =
        id(
            &runtime_publications::record(&pool, &common::cipher(), &f.who, f.record())
                .await
                .unwrap(),
        );
    common::guardrail(
        &pool,
        f.who.tenant_id,
        "final-review",
        1,
        "post",
        json!({"kind":"always"}),
    )
    .await;
    let pending = runtime_publications::release(&f.gateway, &f.who, publication)
        .await
        .unwrap();
    let (status, reviewed) = approval_http(
        &pool,
        &f,
        "/approvals/v1/review",
        json!({"approval_id":common::approval_id(&pending)}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{reviewed}");
    assert_eq!(reviewed["can_continue"], true);
    sqlx::raw_sql("CREATE SEQUENCE fixture_final_decision_reached; CREATE FUNCTION fixture_slow_final_decision() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.action='approval.decided' THEN PERFORM nextval('fixture_final_decision_reached'); PERFORM pg_sleep(7); END IF; RETURN NEW; END $$; CREATE TRIGGER fixture_final_decision_audit BEFORE INSERT ON audit_events FOR EACH ROW EXECUTE FUNCTION fixture_slow_final_decision();")
        .execute(&pool).await.unwrap();
    let (status, _) = approval_http(
        &pool,
        &f,
        "/approvals/v1/decide",
        json!({
        "approval_id":common::approval_id(&pending),"review_id":reviewed["review_id"],
        "acknowledged_scope":true,"decision":common::approve(&["final-review"])}),
    )
    .await;
    assert_eq!(status, StatusCode::GONE);
    assert!(
        sqlx::query_scalar::<_, bool>("SELECT is_called FROM fixture_final_decision_reached")
            .fetch_one(&pool)
            .await
            .unwrap()
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM guardrail_grants")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT state FROM approvals WHERE id=$1")
            .bind(common::approval_id(&pending))
            .fetch_one(&pool)
            .await
            .unwrap(),
        "pending"
    );
    assert_eq!(f.calls.load(Ordering::SeqCst), 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn revocation_and_missing_source_stop_release_without_rerun(pool: PgPool) {
    let f = setup(&pool).await;
    let publication =
        id(
            &runtime_publications::record(&pool, &common::cipher(), &f.who, f.record())
                .await
                .unwrap(),
        );
    sqlx::query("UPDATE models SET active=false WHERE tenant_id=$1 AND id='model'")
        .bind(f.who.tenant_id)
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        runtime_publications::release(&f.gateway, &f.who, publication).await,
        Err(Error::Unauthorized)
    ));
    sqlx::query("UPDATE models SET active=true WHERE tenant_id=$1 AND id='model'")
        .bind(f.who.tenant_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE runtime_bindings SET active=false WHERE tenant_id=$1 AND key_id=$2")
        .bind(f.who.tenant_id)
        .bind(f.who.key_id)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        runtime_publications::release(&f.gateway, &f.who, publication)
            .await
            .is_err()
    );
    sqlx::query("UPDATE runtime_bindings SET active=true WHERE tenant_id=$1 AND key_id=$2")
        .bind(f.who.tenant_id)
        .bind(f.who.key_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE requests SET result_ciphertext=NULL WHERE id=$1")
        .bind(f.request)
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        runtime_publications::release(&f.gateway, &f.who, publication).await,
        Err(Error::ResultUnavailable)
    ));
    assert_eq!(f.calls.load(Ordering::SeqCst), 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn database_facts_cannot_be_replaced_deleted_or_have_ttl_renewed(pool: PgPool) {
    let f = setup(&pool).await;
    let publication =
        id(
            &runtime_publications::record(&pool, &common::cipher(), &f.who, f.record())
                .await
                .unwrap(),
        );
    for statement in [
        "UPDATE runtime_final_publications SET expires_at=expires_at+interval '1 second' WHERE id=$1",
        "UPDATE runtime_final_publications SET bytes=bytes+1 WHERE id=$1",
        "UPDATE runtime_final_publications SET result_ciphertext=NULL WHERE id=$1",
        "DELETE FROM runtime_final_publications WHERE id=$1",
    ] {
        assert!(
            sqlx::query(statement)
                .bind(publication)
                .execute(&pool)
                .await
                .is_err(),
            "{statement}"
        );
    }
    assert!(sqlx::query("INSERT INTO runtime_final_publications(tenant_id,id,key_id,turn_id,model_id,source_requests,text_sha256,bytes,result_ciphertext,created_at,expires_at) SELECT tenant_id,$2,key_id,turn_id,model_id,jsonb_build_array(jsonb_build_object('id',$3::text,'request_hash',repeat('00',32),'ciphertext_sha256',repeat('00',32),'expires_at',expires_at)),text_sha256,bytes,result_ciphertext,created_at,expires_at FROM runtime_final_publications WHERE id=$1")
        .bind(publication).bind(Uuid::new_v4()).bind(Uuid::new_v4()).execute(&pool).await.is_err());
}

async fn http(
    app: &Router,
    token: &str,
    method: &str,
    path: &str,
    body: Value,
) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
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
async fn http_contract_reserves_output_actions_and_never_uses_claims(pool: PgPool) {
    let f = setup(&pool).await;
    let app = api::gateway_router(f.gateway.clone());
    let (status, stored) = http(
        &app,
        &f.token,
        "POST",
        "/v1/governance/final-publications",
        json!(f.record()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(stored.get("text").is_none());
    let publication = id(&stored);
    let path = format!("/v1/governance/final-publications/{publication}/release");
    assert_eq!(
        http(&app, &f.token, "GET", &path, json!({})).await.0,
        StatusCode::METHOD_NOT_ALLOWED
    );
    assert_eq!(
        http(&app, &f.token, "POST", &path, json!({"text":"replacement"}))
            .await
            .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let (status, ready) = http(&app, &f.token, "POST", &path, json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ready["state"], "ready");
    let op = id(&ready);
    for action in ["claim", "receipt"] {
        let body = if action == "claim" {
            json!({"token":"11".repeat(32),"payload_hash":ready["payload_hash"]})
        } else {
            json!({"token":"11".repeat(32),"outcome":"completed","evidence_ref":"runtime://forged"})
        };
        assert_eq!(
            http(
                &app,
                &f.token,
                "POST",
                &format!("/v1/governance/operations/{op}/{action}"),
                body
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(http(&app,&f.token,"POST","/v1/governance/checks",json!({"turn_id":f.turn,
        "operation_key":format!("runtime-final-output:{publication}"),"stage":"tool_output","model_id":"model","tool_name":"forged","payload":{}})).await.0,StatusCode::BAD_REQUEST);
    assert_eq!(f.calls.load(Ordering::SeqCst), 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn final_gate_cannot_cross_the_real_approval_deadline(pool: PgPool) {
    let f = setup(&pool).await;
    let publication =
        id(
            &runtime_publications::record(&pool, &common::cipher(), &f.who, f.record())
                .await
                .unwrap(),
        );
    common::guardrail(
        &pool,
        f.who.tenant_id,
        "short-final-grant",
        1,
        "post",
        json!({"kind":"always"}),
    )
    .await;
    let pending = runtime_publications::release(&f.gateway, &f.who, publication)
        .await
        .unwrap();
    let approval = common::approval_id(&pending);
    sqlx::raw_sql("CREATE SEQUENCE fixture_final_publication_gate_count;
        CREATE FUNCTION fixture_final_publication_delay() RETURNS trigger LANGUAGE plpgsql AS $$
        DECLARE deadline timestamptz;
        BEGIN
            IF NEW.event_type='guardrails.evaluated' AND nextval('fixture_final_publication_gate_count')=2 THEN
                SELECT min(expires_at) INTO deadline FROM guardrail_grants WHERE tenant_id=NEW.tenant_id AND turn_id=NEW.turn_id;
                PERFORM pg_sleep(GREATEST(0,extract(epoch FROM deadline-clock_timestamp()))+0.05);
            END IF;
            RETURN NEW;
        END; $$;
        CREATE TRIGGER fixture_final_publication_delay BEFORE INSERT ON governance_events
            FOR EACH ROW EXECUTE FUNCTION fixture_final_publication_delay();")
        .execute(&pool).await.unwrap();
    sqlx::query("UPDATE approvals SET expires_at=clock_timestamp()+interval '2 seconds' WHERE tenant_id=$1 AND id=$2")
        .bind(f.who.tenant_id).bind(approval).execute(&pool).await.unwrap();
    common::decide(
        &pool,
        &f.subject,
        approval,
        common::approve(&["short-final-grant"]),
    )
    .await
    .unwrap();
    assert!(matches!(
        runtime_publications::release(&f.gateway, &f.who, publication).await,
        Err(Error::PolicyChanged)
    ));
    let count: i64 =
        sqlx::query_scalar("SELECT last_value FROM fixture_final_publication_gate_count")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 2);
    assert_eq!(f.calls.load(Ordering::SeqCst), 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn runtime_role_records_and_releases_but_cannot_mint_grants_or_modify_facts(pool: PgPool) {
    let f = setup(&pool).await;
    sqlx::raw_sql(include_str!("../deploy/permissions.sql"))
        .execute(&pool)
        .await
        .unwrap();
    let scoped = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
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
    let publication =
        id(
            &runtime_publications::record(&scoped, &common::cipher(), &f.who, f.record())
                .await
                .unwrap(),
        );
    let g = Gateway::new(scoped.clone(), common::cipher(), HashMap::new(), 2, 5, 60).unwrap();
    assert_eq!(
        runtime_publications::release(&g, &f.who, publication)
            .await
            .unwrap()["text"],
        f.record().text
    );
    for sql in [
        "UPDATE approvals SET state='approved'",
        "INSERT INTO guardrail_grants SELECT * FROM guardrail_grants",
        "UPDATE runtime_final_publications SET expires_at=clock_timestamp()+interval '1 hour'",
        "DELETE FROM runtime_final_publications",
    ] {
        let error = sqlx::query(sql).execute(&scoped).await.unwrap_err();
        assert_eq!(
            error.as_database_error().unwrap().code().as_deref(),
            Some("42501"),
            "{sql}"
        );
    }
    for role in [
        "brokerrouter_runtime_assets",
        "brokerrouter_approvals",
        "brokerrouter_governance_control",
        "brokerrouter_console",
    ] {
        let readable: bool = sqlx::query_scalar(
            "SELECT has_table_privilege($1,'runtime_final_publications','SELECT')",
        )
        .bind(role)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(!readable, "{role}");
    }
    scoped.close().await;
}

#[sqlx::test(migrations = "./migrations")]
async fn wire_size_and_concurrent_capture_do_not_replace_final_bytes(pool: PgPool) {
    let f = setup(&pool).await;
    for text in ["x".repeat(512 * 1024), "\0".repeat(90_000)] {
        let mut record = f.record();
        record.text = text;
        assert!(matches!(
            runtime_publications::record(&pool, &common::cipher(), &f.who, record).await,
            Err(Error::Invalid(_))
        ));
    }
    let cipher = common::cipher();
    let one = f.record();
    let mut two = one.clone();
    two.text = "racing different final".into();
    let (a, b) = tokio::join!(
        runtime_publications::record(&pool, &cipher, &f.who, one),
        runtime_publications::record(&pool, &cipher, &f.who, two)
    );
    assert!(a.is_ok() ^ b.is_ok());
    assert!(matches!(
        if a.is_err() {
            a.unwrap_err()
        } else {
            b.unwrap_err()
        },
        Error::Conflict
    ));
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM runtime_final_publications")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    assert_eq!(f.calls.load(Ordering::SeqCst), 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn expiry_cleans_both_encrypted_bodies_without_replacing_facts(pool: PgPool) {
    let f = setup(&pool).await;
    let request = f.record();
    let publication = Uuid::new_v4();
    let encrypted = common::cipher()
        .encrypt(f.who.tenant_id, publication, request.text.as_bytes())
        .unwrap();
    // The migration owner imports an already expired publication backed by the
    // genuine local model call; all production source/immutable triggers remain on.
    sqlx::query("INSERT INTO runtime_final_publications(tenant_id,id,key_id,turn_id,model_id,source_requests,text_sha256,bytes,result_ciphertext,created_at,expires_at) SELECT r.tenant_id,$2,r.key_id,r.governance_turn_id,r.model_id,jsonb_build_array(jsonb_build_object('id',r.id::text,'request_hash',encode(r.request_hash,'hex'),'ciphertext_sha256',encode(sha256(r.result_ciphertext),'hex'),'expires_at',r.result_expires_at)),$3,$4,$5,stamp.ts,stamp.ts+interval '60 seconds' FROM requests r CROSS JOIN(SELECT clock_timestamp()-interval '61 seconds' AS ts) stamp WHERE r.id=$1")
        .bind(f.request).bind(publication).bind(store::hash(request.text.as_bytes()))
        .bind(i32::try_from(request.text.len()).unwrap()).bind(encrypted).execute(&pool).await.unwrap();
    governance::check(
        &pool,
        &common::cipher(),
        &f.who,
        governance::Check {
            turn_id: f.turn,
            operation_key: format!("runtime-final-output:{publication}"),
            stage: "post".into(),
            model_id: Some("model".into()),
            tool_name: None,
            payload: json!({"text":request.text}),
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        runtime_publications::release(&f.gateway, &f.who, publication).await,
        Err(Error::ResultUnavailable)
    ));
    assert_eq!(runtime_publications::maintain(&pool).await.unwrap(), 1);
    assert_eq!(runtime_publications::maintain(&pool).await.unwrap(), 0);
    let facts:(bool,Vec<u8>,bool)=sqlx::query_as("SELECT p.result_ciphertext IS NULL,p.text_sha256,o.payload_ciphertext IS NULL FROM runtime_final_publications p JOIN governance_operations o ON o.tenant_id=p.tenant_id AND o.operation_key='runtime-final-output:'||p.id::text WHERE p.id=$1")
        .bind(publication).fetch_one(&pool).await.unwrap();
    assert_eq!(facts, (true, store::hash(request.text.as_bytes()), true));
    let retry = runtime_publications::record(&pool, &common::cipher(), &f.who, request)
        .await
        .unwrap();
    assert_eq!(id(&retry), publication);
    assert!(retry.get("text").is_none());
    assert_eq!(f.calls.load(Ordering::SeqCst), 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn multiple_gateway_sources_are_canonical_and_closed_turn_still_cannot_release(pool: PgPool) {
    let f = setup(&pool).await;
    let (second, response) = f
        .gateway
        .execute_in_turn(
            &f.who,
            "model-call-2",
            json!({"model":"model",
        "messages":[{"role":"user","content":"second logical model request"}],"max_tokens":20}),
            "chat",
            Some(f.turn),
        )
        .await
        .unwrap();
    assert!(response.is_ok());
    let mut request = f.record();
    request.source_request_ids = vec![second, f.request];
    let record = runtime_publications::record(&pool, &common::cipher(), &f.who, request.clone())
        .await
        .unwrap();
    request.source_request_ids.reverse();
    assert_eq!(
        record,
        runtime_publications::record(&pool, &common::cipher(), &f.who, request.clone())
            .await
            .unwrap()
    );
    governance::close_runtime(&pool, &f.who, f.turn, "completed")
        .await
        .unwrap();
    assert_eq!(
        record,
        runtime_publications::record(&pool, &common::cipher(), &f.who, request)
            .await
            .unwrap()
    );
    assert!(matches!(
        runtime_publications::release(&f.gateway, &f.who, id(&record)).await,
        Err(Error::TurnClosed)
    ));
    assert_eq!(f.calls.load(Ordering::SeqCst), 2);
}

#[sqlx::test(migrations = "./migrations")]
async fn semantic_final_release_settles_one_evaluation_and_does_not_repeat_the_model(pool: PgPool) {
    let f = setup(&pool).await;
    let publication =
        id(
            &runtime_publications::record(&pool, &common::cipher(), &f.who, f.record())
                .await
                .unwrap(),
        );
    store::add_endpoint(
        &pool,
        &store::Endpoint {
            id: Uuid::new_v4(),
            tenant_id: f.who.tenant_id,
            model_id: "model".into(),
            name: "local-paid-judge".into(),
            upstream_model: "upstream".into(),
            base_url: f.url.clone(),
            credential_env: "FIXTURE_KEY".into(),
            allow_private: true,
            currency: "CNY".into(),
            input_per_million: money::nonnegative("1").unwrap(),
            output_per_million: money::nonnegative("1").unwrap(),
            context_tokens: 1000,
            max_output_tokens: 100,
            priority: 0,
            operation: "chat".into(),
            supports_tools: false,
            supports_streaming: false,
            embedding_dimension: None,
        },
    )
    .await
    .unwrap();
    common::guardrail(&pool,f.who.tenant_id,"semantic-final",1,"post",
        json!({"kind":"model_judge","model":"model","instructions":"Flag risky final text.",
            "input_format":"text_json","max_input_bytes":65536,"max_output_tokens":32,"maximum_cny":"0.1"})).await;
    let pending = runtime_publications::release(&f.gateway, &f.who, publication)
        .await
        .unwrap();
    assert_eq!(pending["state"], "waiting_approval");
    assert!(pending.get("text").is_none());
    assert_eq!(f.calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        runtime_publications::release(&f.gateway, &f.who, publication)
            .await
            .unwrap()["approval"]["id"],
        pending["approval"]["id"]
    );
    assert_eq!(f.calls.load(Ordering::SeqCst), 2);
    common::decide(
        &pool,
        &f.subject,
        common::approval_id(&pending),
        common::approve(&["semantic-final"]),
    )
    .await
    .unwrap();
    assert_eq!(
        runtime_publications::release(&f.gateway, &f.who, publication)
            .await
            .unwrap()["text"],
        f.record().text
    );
    let facts:(i64,i64,i64,i64)=sqlx::query_as("SELECT (SELECT count(*) FROM requests),(SELECT count(*) FROM requests WHERE guardrail_evaluation_id IS NOT NULL),(SELECT count(*) FROM ledger_entries WHERE kind='finalize'),(SELECT sum(final_micros)::bigint FROM requests)").fetch_one(&pool).await.unwrap();
    assert_eq!(facts, (2, 1, 2, 5));
    assert_eq!(f.calls.load(Ordering::SeqCst), 2);
}
