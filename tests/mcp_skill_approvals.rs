mod common;

use axum::{body::Body, http::Request};
use brokerrouter::{
    approvals_api,
    error::Error,
    governance_api::Control,
    mcp_catalog::{self, CacheScope, RecordSkillGet},
    mcp_skill_approvals::{self, Request as SkillRequest},
    store::{self, Identity},
};
use common::*;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::sync::Arc;
use tower::ServiceExt;
use uuid::Uuid;

const URI: &str = "skill://reviewed/SKILL.md";

struct Fixture {
    who: Identity,
    turn_id: Uuid,
    server_id: Uuid,
}

async fn setup(pool: &PgPool) -> Fixture {
    let tenant_id = store::create_tenant(pool, "Skill consent", 1_000_000)
        .await
        .unwrap();
    store::put_model(pool, tenant_id, "model", "model")
        .await
        .unwrap();
    let (key_id, _) = store::create_key(
        pool,
        tenant_id,
        "runtime",
        1_000_000,
        &["model".into()],
        None,
    )
    .await
    .unwrap();
    let who = Identity { tenant_id, key_id };
    provision(pool, &who, "alice").await;
    let turn_id = turn(pool, &who, "alice", 0).await;
    let server_id = Uuid::new_v4();
    sqlx::query("INSERT INTO mcp_servers(tenant_id,id,display_name,transport,endpoint_url,auth_mode,active) VALUES($1,$2,'Skill source','streamable_http','https://example.org/mcp','none',true)")
        .bind(tenant_id).bind(server_id).execute(pool).await.unwrap();
    observe(pool, &who, server_id, "a").await;
    Fixture {
        who,
        turn_id,
        server_id,
    }
}

async fn observe(pool: &PgPool, who: &Identity, server_id: Uuid, digest: &str) {
    let entry = json!({
        "uri":URI,
        "frontmatter":{"name":"reviewed","description":"A reviewed workflow"},
        "resources":[{"uri":URI,"digest":format!("sha256:{}",digest.repeat(64)),"size":42}]
    });
    mcp_catalog::record_skill_get(pool, RecordSkillGet {
        tenant_id:who.tenant_id,
        server_id,
        operation_id:Uuid::new_v4(),
        actor:"mcp-connector:test".into(),
        expected_server_revision:1,
        protocol_version:mcp_catalog::PROTOCOL_VERSION.into(),
        result_type:"complete".into(),
        supported_versions:vec![mcp_catalog::PROTOCOL_VERSION.into()],
        capabilities:json!({"resources":{},"extensions":{"io.modelcontextprotocol/skills":{}}}),
        discovery_ttl_ms:120_000,
        discovery_cache_scope:CacheScope::Public,
        requested_uri:URI.into(),
        entry,
        skill_ttl_ms:120_000,
        skill_cache_scope:CacheScope::Public,
        authorization_context_fingerprint:None,
    }).await.unwrap();
}

async fn signed(pool: &PgPool, path: &str, subject: &str, body: Value) -> (u16, Value) {
    let token = approvals_sign(claims(
        sqlx::query_scalar("SELECT tenant_id FROM governance_users WHERE subject='alice' LIMIT 1")
            .fetch_one(pool)
            .await
            .unwrap(),
        subject,
        path,
        &body,
    ));
    let response = approvals_api::router(Control {
        pool: pool.clone(),
        verifier: Arc::new(approvals_verifier()),
        cipher: Arc::new(cipher()),
    })
    .oneshot(
        Request::builder()
            .method("POST")
            .uri(path)
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap(),
    )
    .await
    .unwrap();
    let code = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (code, serde_json::from_slice(&bytes).unwrap())
}

#[sqlx::test(migrations = "./migrations")]
async fn reviewed_skill_consent_is_turn_scoped_and_revoked_by_manifest_change(pool: PgPool) {
    let f = setup(&pool).await;
    let request_id = Uuid::new_v4();
    let input = SkillRequest {
        request_id,
        turn_id: f.turn_id,
        server_id: f.server_id,
        uri: URI.into(),
    };
    let first = mcp_skill_approvals::request(&pool, &f.who, input)
        .await
        .unwrap();
    assert_eq!(first["state"], "pending");
    assert_eq!(
        first["activation"],
        "disabled_until_verified_resource_read_and_explicit_tool_grant"
    );
    brokerrouter::human::add_user(&pool, f.who.tenant_id, ISSUER, "bob")
        .await
        .unwrap();
    let (code, _) = signed(
        &pool,
        "/approvals/v1/skills/review",
        "bob",
        json!({"request_id":request_id}),
    )
    .await;
    assert_eq!(code, 404);
    // A fresh direct lookup with identical complete metadata preserves the scope.
    observe(&pool, &f.who, f.server_id, "a").await;
    let (code,inbox) = signed(&pool,"/approvals/v1/skills/query","alice",json!({"turn_id":f.turn_id,"pending_only":true,"limit":10,"before_created_at":null,"before_id":null})).await;
    assert_eq!(code, 200);
    assert_eq!(inbox["data"][0]["request_id"], request_id.to_string());
    assert!(inbox["data"][0].get("entry").is_none());
    let (code, reviewed) = signed(
        &pool,
        "/approvals/v1/skills/review",
        "alice",
        json!({"request_id":request_id}),
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(reviewed["entry"]["uri"], URI);
    assert_eq!(reviewed["requests_tool_grant"], false);
    let review_id = reviewed["review_id"].as_str().unwrap();
    let body = json!({"request_id":request_id,"review_id":review_id,"acknowledged_scope":true,"decision":"approve_this_turn","feedback":null});
    let (code, approved) =
        signed(&pool, "/approvals/v1/skills/decide", "alice", body.clone()).await;
    assert_eq!(code, 200);
    assert_eq!(approved["state"], "approved");
    assert_eq!(
        mcp_skill_approvals::status(&pool, &cipher(), &f.who, request_id)
            .await
            .unwrap()["state"],
        "approved"
    );
    observe(&pool, &f.who, f.server_id, "a").await;
    assert_eq!(
        mcp_skill_approvals::status(&pool, &cipher(), &f.who, request_id)
            .await
            .unwrap()["state"],
        "approved"
    );
    let (code, retry) = signed(&pool, "/approvals/v1/skills/decide", "alice", body).await;
    assert_eq!(code, 200);
    assert_eq!(retry["state"], "approved");
    observe(&pool, &f.who, f.server_id, "b").await;
    assert_eq!(
        mcp_skill_approvals::status(&pool, &cipher(), &f.who, request_id)
            .await
            .unwrap()["state"],
        "stale"
    );
    assert!(matches!(
        mcp_skill_approvals::status(
            &pool,
            &cipher(),
            &Identity {
                tenant_id: f.who.tenant_id,
                key_id: Uuid::new_v4()
            },
            request_id
        )
        .await,
        Err(Error::NotFound)
    ));
    let facts: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM mcp_skill_approval_reviews WHERE tenant_id=$1 AND request_id=$2",
    )
    .bind(f.who.tenant_id)
    .bind(request_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(facts, 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn skill_decision_audit_failure_rolls_back_state_and_event(pool: PgPool) {
    let f = setup(&pool).await;
    let request_id = Uuid::new_v4();
    mcp_skill_approvals::request(
        &pool,
        &f.who,
        SkillRequest {
            request_id,
            turn_id: f.turn_id,
            server_id: f.server_id,
            uri: URI.into(),
        },
    )
    .await
    .unwrap();
    let (code, reviewed) = signed(
        &pool,
        "/approvals/v1/skills/review",
        "alice",
        json!({"request_id":request_id}),
    )
    .await;
    assert_eq!(code, 200);
    sqlx::raw_sql("CREATE FUNCTION fail_skill_decision_audit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.action='mcp.skill_approval_decided' THEN RAISE EXCEPTION 'fixture'; END IF; RETURN NEW; END $$; CREATE TRIGGER fail_skill_decision BEFORE INSERT ON audit_events FOR EACH ROW EXECUTE FUNCTION fail_skill_decision_audit();")
        .execute(&pool).await.unwrap();
    let (code,_) = signed(&pool,"/approvals/v1/skills/decide","alice",json!({"request_id":request_id,"review_id":reviewed["review_id"],"acknowledged_scope":true,"decision":"approve_this_turn","feedback":null})).await;
    assert_eq!(code, 503);
    let state: String =
        sqlx::query_scalar("SELECT state FROM mcp_skill_approval_requests WHERE id=$1")
            .bind(request_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(state, "pending");
    let events:i64 = sqlx::query_scalar("SELECT count(*) FROM governance_events WHERE tenant_id=$1 AND turn_id=$2 AND event_type='skill.approval_decided'")
        .bind(f.who.tenant_id).bind(f.turn_id).fetch_one(&pool).await.unwrap();
    assert_eq!(events, 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn changed_manifest_cannot_be_decided_and_feedback_closes_turn(pool: PgPool) {
    let f = setup(&pool).await;
    let request_id = Uuid::new_v4();
    mcp_skill_approvals::request(
        &pool,
        &f.who,
        SkillRequest {
            request_id,
            turn_id: f.turn_id,
            server_id: f.server_id,
            uri: URI.into(),
        },
    )
    .await
    .unwrap();
    let (code, reviewed) = signed(
        &pool,
        "/approvals/v1/skills/review",
        "alice",
        json!({"request_id":request_id}),
    )
    .await;
    assert_eq!(code, 200);
    let review_id = reviewed["review_id"].as_str().unwrap();
    observe(&pool, &f.who, f.server_id, "b").await;
    let (code,_) = signed(&pool,"/approvals/v1/skills/decide","alice",json!({"request_id":request_id,"review_id":review_id,"acknowledged_scope":true,"decision":"approve_this_turn","feedback":null})).await;
    assert_eq!(code, 409);
    let state: String =
        sqlx::query_scalar("SELECT state FROM mcp_skill_approval_requests WHERE id=$1")
            .bind(request_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(state, "pending");
    // A distinct request after the new observation can receive a human response.
    let replacement = Uuid::new_v4();
    mcp_skill_approvals::request(
        &pool,
        &f.who,
        SkillRequest {
            request_id: replacement,
            turn_id: f.turn_id,
            server_id: f.server_id,
            uri: URI.into(),
        },
    )
    .await
    .unwrap();
    let (code, reviewed) = signed(
        &pool,
        "/approvals/v1/skills/review",
        "alice",
        json!({"request_id":replacement}),
    )
    .await;
    assert_eq!(code, 200);
    let (code,feedback) = signed(&pool,"/approvals/v1/skills/decide","alice",json!({"request_id":replacement,"review_id":reviewed["review_id"],"acknowledged_scope":true,"decision":"feedback","feedback":"先改方案"})).await;
    assert_eq!(code, 200);
    assert_eq!(feedback["state"], "feedback");
    assert_eq!(
        mcp_skill_approvals::status(&pool, &cipher(), &f.who, replacement)
            .await
            .unwrap()["feedback"],
        "先改方案"
    );
    let state: String =
        sqlx::query_scalar("SELECT state FROM governance_turns WHERE tenant_id=$1 AND id=$2")
            .bind(f.who.tenant_id)
            .bind(f.turn_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(state, "cancelled");
}

#[sqlx::test(migrations = "./migrations")]
async fn skill_approval_database_roles_separate_request_and_decision(pool: PgPool) {
    let f = setup(&pool).await;
    sqlx::raw_sql(include_str!("../deploy/permissions.sql"))
        .execute(&pool)
        .await
        .unwrap();
    let runtime = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .after_connect(|c, _| {
            Box::pin(async move {
                sqlx::query("SET SESSION AUTHORIZATION brokerrouter_runtime")
                    .execute(c)
                    .await?;
                Ok(())
            })
        })
        .connect_with(pool.connect_options().as_ref().clone())
        .await
        .unwrap();
    let approvals = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
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
    brokerrouter::check_schema(&runtime).await.unwrap();
    brokerrouter::check_schema(&approvals).await.unwrap();
    let request_id = Uuid::new_v4();
    mcp_skill_approvals::request(
        &runtime,
        &f.who,
        SkillRequest {
            request_id,
            turn_id: f.turn_id,
            server_id: f.server_id,
            uri: URI.into(),
        },
    )
    .await
    .unwrap();
    let review_body = json!({"request_id":request_id});
    let review_token = approvals_sign(claims(
        f.who.tenant_id,
        "alice",
        "/approvals/v1/skills/review",
        &review_body,
    ));
    let human = approvals_verifier()
        .verify(
            &approvals,
            &review_token,
            "POST",
            "/approvals/v1/skills/review",
            &serde_json::to_vec(&review_body).unwrap(),
        )
        .await
        .unwrap();
    let reviewed = mcp_skill_approvals::review(
        &approvals,
        &human,
        serde_json::from_value(review_body).unwrap(),
    )
    .await
    .unwrap();
    let decide_body = json!({"request_id":request_id,"review_id":reviewed["review_id"],"acknowledged_scope":true,"decision":"approve_this_turn","feedback":null});
    let decide_token = approvals_sign(claims(
        f.who.tenant_id,
        "alice",
        "/approvals/v1/skills/decide",
        &decide_body,
    ));
    let human = approvals_verifier()
        .verify(
            &approvals,
            &decide_token,
            "POST",
            "/approvals/v1/skills/decide",
            &serde_json::to_vec(&decide_body).unwrap(),
        )
        .await
        .unwrap();
    let decided = mcp_skill_approvals::decide(
        &approvals,
        &cipher(),
        &human,
        serde_json::from_value(decide_body).unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(decided["state"], "approved");
    assert!(
        sqlx::query("UPDATE mcp_skill_approval_requests SET state='approved'")
            .execute(&runtime)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("INSERT INTO mcp_skill_approval_requests DEFAULT VALUES")
            .execute(&approvals)
            .await
            .is_err()
    );
    assert!(
        sqlx::query(
            "INSERT INTO mcp_skill_approval_requests(tenant_id,id,state) VALUES($1,$2,'approved')"
        )
        .bind(f.who.tenant_id)
        .bind(Uuid::new_v4())
        .execute(&runtime)
        .await
        .is_err()
    );
    assert!(
        sqlx::query("SELECT * FROM mcp_skill_approval_reviews")
            .execute(&runtime)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("SELECT * FROM mcp_servers")
            .execute(&runtime)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("SELECT * FROM mcp_servers")
            .execute(&approvals)
            .await
            .is_err()
    );
    assert!(sqlx::query("INSERT INTO audit_events(tenant_id,action,object_id,details) VALUES($1,'unauthorized','x','{}')")
        .bind(f.who.tenant_id).execute(&runtime).await.is_err());
}
