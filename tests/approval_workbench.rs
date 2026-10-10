mod common;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use brokerrouter::{
    approvals_api,
    governance::{self, Check},
    governance_api::Control,
    human, store,
};
use common::*;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::sync::Arc;
use tower::ServiceExt;
use uuid::Uuid;
const QUERY: &str = "/approvals/v1/query";
const REVIEW: &str = "/approvals/v1/review";
const DECIDE: &str = "/approvals/v1/decide";

#[sqlx::test(migrations = "./migrations")]
async fn artifact_metadata_cannot_mint_binary_grants(pool: PgPool) {
    let mut f = setup(&pool, "alice").await;
    guardrail(
        &pool,
        f.who.tenant_id,
        "asset-review",
        1,
        "artifact",
        json!({"kind":"always"}),
    )
    .await;
    f.check.stage = "artifact".into();
    f.check.operation_key = "asset-operation".into();
    f.approval = approval_id(
        &governance::check(&pool, &cipher(), &f.who, f.check.clone())
            .await
            .unwrap(),
    );
    let r = review(&pool, &f).await;
    assert_eq!(r["can_continue"], false);
    assert!(r["payload_text"].is_null());
    let mut body = decision(&f, &r);
    body["decision"] = approve(&["asset-review"]);
    assert_eq!(call(&pool, &f, "alice", DECIDE, &body).await.0, 410);
    body["decision"] = json!({"decision":"reject","approved_guardrail_ids":[],"feedback":null});
    assert_eq!(call(&pool, &f, "alice", DECIDE, &body).await.0, 200);
}
#[sqlx::test(migrations = "./migrations")]
async fn proof_expiring_during_decision_audit_rolls_back_grants_and_decision(pool: PgPool) {
    let f = setup(&pool, "alice").await;
    let r = review(&pool, &f).await;
    let body = decision(&f, &r);
    sqlx::raw_sql("CREATE FUNCTION slow_approval_audit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.action='approval.decided' THEN PERFORM pg_sleep(4); END IF; RETURN NEW; END $$;CREATE TRIGGER slow_approval BEFORE INSERT ON audit_events FOR EACH ROW EXECUTE FUNCTION slow_approval_audit();").execute(&pool).await.unwrap();
    let mut c = claims(f.who.tenant_id, "alice", DECIDE, &body);
    c.exp = chrono::Utc::now().timestamp() + 3;
    assert_eq!(send(&pool, DECIDE, &body, approvals_sign(c)).await.0, 401);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM guardrail_grants")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT state FROM approvals WHERE id=$1")
            .bind(f.approval)
            .fetch_one(&pool)
            .await
            .unwrap(),
        "pending"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn review_expiring_during_decision_audit_rolls_back_grants_and_decision(pool: PgPool) {
    let f = setup(&pool, "alice").await;
    let mut r = review(&pool, &f).await;
    let id = Uuid::new_v4();
    sqlx::query("WITH moment AS MATERIALIZED (SELECT clock_timestamp() AS now) INSERT INTO approval_reviews SELECT tenant_id,$1,approval_id,user_id,payload_hash,policy_hash,can_continue,now-interval '294 seconds',now+interval '6 seconds' FROM approval_reviews CROSS JOIN moment WHERE id=$2")
        .bind(id).bind(Uuid::parse_str(r["review_id"].as_str().unwrap()).unwrap()).execute(&pool).await.unwrap();
    r["review_id"] = json!(id);
    let body = decision(&f, &r);
    sqlx::raw_sql("CREATE SEQUENCE fixture_review_decision_reached; CREATE FUNCTION slow_review_decision_audit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.action='approval.decided' THEN PERFORM nextval('fixture_review_decision_reached'); PERFORM pg_sleep(7); END IF; RETURN NEW; END $$; CREATE TRIGGER slow_review_decision BEFORE INSERT ON audit_events FOR EACH ROW EXECUTE FUNCTION slow_review_decision_audit();")
        .execute(&pool).await.unwrap();
    assert_eq!(call(&pool, &f, "alice", DECIDE, &body).await.0, 409);
    assert!(
        sqlx::query_scalar::<_, bool>("SELECT is_called FROM fixture_review_decision_reached")
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
            .bind(f.approval)
            .fetch_one(&pool)
            .await
            .unwrap(),
        "pending"
    );
}
struct Fixture {
    who: store::Identity,
    turn: Uuid,
    check: Check,
    approval: Uuid,
}
async fn setup(pool: &PgPool, subject: &str) -> Fixture {
    let tenant = store::create_tenant(pool, "approval fixture", 1_000_000)
        .await
        .unwrap();
    store::put_model(pool, tenant, "model", "model")
        .await
        .unwrap();
    let (key_id, _) =
        store::create_key(pool, tenant, "runtime", 1_000_000, &["model".into()], None)
            .await
            .unwrap();
    let who = store::Identity {
        tenant_id: tenant,
        key_id,
    };
    provision(pool, &who, subject).await;
    let turn = turn(pool, &who, subject, 0).await;
    for id in ["a", "b"] {
        guardrail(pool, tenant, id, 1, "tool_input", json!({"kind":"always"})).await;
    }
    let check = Check {
        turn_id: turn,
        operation_key: "operation".into(),
        stage: "tool_input".into(),
        model_id: None,
        tool_name: Some("send_message".into()),
        payload: json!({"message":"private-review-only <script>bad()</script>","integer":9007199254740993_i64}),
    };
    let approval = approval_id(
        &governance::check(pool, &cipher(), &who, check.clone())
            .await
            .unwrap(),
    );
    Fixture {
        who,
        turn,
        check,
        approval,
    }
}
async fn send(pool: &PgPool, path: &str, body: &Value, token: String) -> (StatusCode, Value) {
    let router = approvals_api::router(Control {
        pool: pool.clone(),
        verifier: Arc::new(approvals_verifier()),
        cipher: Arc::new(cipher()),
    });
    let response = router
        .oneshot(
            Request::post(path)
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(serde_json::to_vec(body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    assert_eq!(response.headers()["cache-control"], "no-store");
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}
async fn call(
    pool: &PgPool,
    f: &Fixture,
    subject: &str,
    path: &str,
    body: &Value,
) -> (StatusCode, Value) {
    send(
        pool,
        path,
        body,
        approvals_sign(claims(f.who.tenant_id, subject, path, body)),
    )
    .await
}
async fn review(pool: &PgPool, f: &Fixture) -> Value {
    let (s, r) = call(pool, f, "alice", REVIEW, &json!({"approval_id":f.approval})).await;
    assert_eq!(s, 200, "{r}");
    r
}
fn decision(f: &Fixture, r: &Value) -> Value {
    json!({"approval_id":f.approval,"review_id":r["review_id"],"acknowledged_scope":true,"decision":approve(&["a"])})
}

#[sqlx::test(migrations = "./migrations")]
async fn inbox_is_owner_scoped_paginated_and_never_reads_sensitive_content(pool: PgPool) {
    let f = setup(&pool, "alice").await;
    let other = setup(&pool, "other-tenant").await;
    human::add_user(&pool, f.who.tenant_id, ISSUER, "bob")
        .await
        .unwrap();
    let mut check = f.check.clone();
    check.operation_key = "second".into();
    governance::check(&pool, &cipher(), &f.who, check)
        .await
        .unwrap();
    let q = json!({"limit":1,"pending_only":true});
    let (s, r) = call(&pool, &f, "alice", QUERY, &q).await;
    assert_eq!(s, 200, "{r}");
    assert_eq!(r["data"].as_array().unwrap().len(), 1);
    assert!(!r["next_after"].is_null());
    let (_, next) = call(
        &pool,
        &f,
        "alice",
        QUERY,
        &json!({"limit":1,"pending_only":true,"after":r["next_after"]}),
    )
    .await;
    assert_ne!(next["data"][0]["id"], r["data"][0]["id"]);
    assert!(!r.to_string().contains("private-review-only"));
    assert!(!r.to_string().contains("guardrails"));
    assert_eq!(call(&pool, &f, "bob", QUERY, &q).await.1["data"], json!([]));
    for id in [f.approval, other.approval] {
        assert_eq!(
            call(&pool, &f, "bob", REVIEW, &json!({"approval_id":id}))
                .await
                .0,
            404
        );
    }
    assert_eq!(
        call(
            &pool,
            &f,
            "alice",
            REVIEW,
            &json!({"approval_id":other.approval})
        )
        .await
        .0,
        404
    );
    assert_eq!(
        call(
            &pool,
            &f,
            "alice",
            QUERY,
            &json!({"limit":51,"pending_only":true})
        )
        .await
        .0,
        400
    );
}
#[sqlx::test(migrations = "./migrations")]
async fn purpose_bound_review_preserves_exact_json_and_audit_contains_only_metadata(pool: PgPool) {
    let f = setup(&pool, "alice").await;
    let body = json!({"approval_id":f.approval});
    let c = claims(f.who.tenant_id, "alice", REVIEW, &body);
    for token in [
        sign(&c),
        console_sign(c.clone()),
        settings_sign(c.clone()),
        approvals_sign(claims(f.who.tenant_id, "alice", DECIDE, &body)),
    ] {
        assert_eq!(send(&pool, REVIEW, &body, token).await.0, 401);
    }
    let r = review(&pool, &f).await;
    assert_eq!(
        r["payload_text"],
        serde_json::to_string(&f.check.payload).unwrap()
    );
    assert!(
        r["payload_text"]
            .as_str()
            .unwrap()
            .contains("9007199254740993")
    );
    assert_eq!(r["can_continue"], true);
    let audit: Value =
        sqlx::query_scalar("SELECT details FROM audit_events WHERE action='approval.reviewed'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(audit["review_id"], r["review_id"]);
    assert!(!audit.to_string().contains("private-review-only"));
    let actor: Option<String> = sqlx::query_scalar(
        "SELECT actor_user_id FROM console_audit_facts WHERE action='approval.reviewed'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(actor.is_some());
    assert!(
        sqlx::query("UPDATE approval_reviews SET can_continue=false")
            .execute(&pool)
            .await
            .is_err()
    );
}
#[sqlx::test(migrations = "./migrations")]
async fn reviewed_decisions_are_idempotent_partial_and_never_execute(pool: PgPool) {
    let f = setup(&pool, "alice").await;
    let r = review(&pool, &f).await;
    let body = decision(&f, &r);
    let (a, b) = tokio::join!(
        call(&pool, &f, "alice", DECIDE, &body),
        call(&pool, &f, "alice", DECIDE, &body)
    );
    assert_eq!(a.0, 200, "{}", a.1);
    assert_eq!(a, b);
    let state: String = sqlx::query_scalar("SELECT state FROM governance_operations")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(state, "waiting_approval");
    let grants: i64 = sqlx::query_scalar("SELECT count(*) FROM guardrail_grants")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(grants, 1);
    let mut other = body.clone();
    other["decision"] = approve(&["b"]);
    assert_eq!(call(&pool, &f, "alice", DECIDE, &other).await.0, 409);
    let rechecked = governance::check(&pool, &cipher(), &f.who, f.check.clone())
        .await
        .unwrap();
    assert_eq!(rechecked["state"], "waiting_approval");
    assert_eq!(rechecked["approval"]["guardrails"][0]["id"], "b");
    let events: Vec<Value> = sqlx::query_scalar(
        "SELECT data FROM governance_events WHERE event_type='approval.decided'",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["review_id"], r["review_id"]);
}
#[sqlx::test(migrations = "./migrations")]
async fn missing_expired_and_stale_reviews_cannot_mint_grants(pool: PgPool) {
    let f = setup(&pool, "alice").await;
    let r = review(&pool, &f).await;
    let mut body = decision(&f, &r);
    body["acknowledged_scope"] = json!(false);
    assert_eq!(call(&pool, &f, "alice", DECIDE, &body).await.0, 400);
    body["acknowledged_scope"] = json!(true);
    body["review_id"] = json!(Uuid::new_v4());
    assert_eq!(call(&pool, &f, "alice", DECIDE, &body).await.0, 404);
    let old = Uuid::new_v4();
    sqlx::query("INSERT INTO approval_reviews SELECT tenant_id,$1,approval_id,user_id,payload_hash,policy_hash,can_continue,clock_timestamp()-interval '10 minutes',clock_timestamp()-interval '6 minutes' FROM approval_reviews WHERE id=$2")
        .bind(old).bind(Uuid::parse_str(r["review_id"].as_str().unwrap()).unwrap()).execute(&pool).await.unwrap();
    body["review_id"] = json!(old);
    assert_eq!(call(&pool, &f, "alice", DECIDE, &body).await.0, 409);
    body = decision(&f, &r);
    guardrail(
        &pool,
        f.who.tenant_id,
        "a",
        2,
        "tool_input",
        json!({"kind":"always"}),
    )
    .await;
    assert_eq!(call(&pool, &f, "alice", DECIDE, &body).await.0, 409);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM guardrail_grants")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
}
#[sqlx::test(migrations = "./migrations")]
async fn unavailable_payload_prevents_continue_but_feedback_closes_without_execution(pool: PgPool) {
    let f = setup(&pool, "alice").await;
    sqlx::query("UPDATE governance_operations SET payload_ciphertext=NULL")
        .execute(&pool)
        .await
        .unwrap();
    let r = review(&pool, &f).await;
    assert_eq!(r["can_continue"], false);
    assert!(r["payload_text"].is_null());
    let mut body = decision(&f, &r);
    assert_eq!(call(&pool, &f, "alice", DECIDE, &body).await.0, 410);
    body["decision"] = json!({"decision":"feedback","feedback":"use a safer destination","approved_guardrail_ids":[]});
    let (s, v) = call(&pool, &f, "alice", DECIDE, &body).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["state"], "feedback");
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT state FROM governance_turns WHERE id=$1")
            .bind(f.turn)
            .fetch_one(&pool)
            .await
            .unwrap(),
        "cancelled"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM guardrail_grants")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    assert!(!v.to_string().contains("safer destination"));
}
#[sqlx::test(migrations = "./migrations")]
async fn review_audit_failure_rolls_back_receipt_and_revocation_denies_content(pool: PgPool) {
    let f = setup(&pool, "alice").await;
    sqlx::raw_sql("CREATE FUNCTION fail_review_audit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.action='approval.reviewed' THEN RAISE EXCEPTION 'fixture'; END IF; RETURN NEW; END $$;CREATE TRIGGER fail_review BEFORE INSERT ON audit_events FOR EACH ROW EXECUTE FUNCTION fail_review_audit();").execute(&pool).await.unwrap();
    assert_eq!(
        call(
            &pool,
            &f,
            "alice",
            REVIEW,
            &json!({"approval_id":f.approval})
        )
        .await
        .0,
        503
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM approval_reviews")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    sqlx::query("UPDATE governance_users SET active=false")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        call(
            &pool,
            &f,
            "alice",
            REVIEW,
            &json!({"approval_id":f.approval})
        )
        .await
        .0,
        401
    );
}
#[sqlx::test(migrations = "./migrations")]
async fn deployed_approval_role_can_review_and_decide_without_execution_or_asset_authority(
    pool: PgPool,
) {
    sqlx::raw_sql(include_str!("../deploy/permissions.sql"))
        .execute(&pool)
        .await
        .unwrap();
    let f = setup(&pool, "alice").await;
    let restricted = sqlx::postgres::PgPoolOptions::new()
        .max_connections(3)
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
    human::maintain(&restricted).await.unwrap();
    let r = review(&restricted, &f).await;
    let (s, v) = call(&restricted, &f, "alice", DECIDE, &decision(&f, &r)).await;
    assert_eq!(s, 200, "{v}");
    for sql in [
        "SELECT key_hash FROM virtual_keys",
        "SELECT * FROM asset_chunks",
        "INSERT INTO governance_turns SELECT * FROM governance_turns",
        "UPDATE governance_operations SET receipt_hash=NULL",
        "UPDATE guardrail_versions SET name='bad'",
        "SET ROLE brokerrouter_governance_control",
        "SET ROLE brokerrouter_runtime",
    ] {
        assert!(
            sqlx::query(sql).execute(&restricted).await.is_err(),
            "{sql}"
        );
    }
    restricted.close().await;
}
