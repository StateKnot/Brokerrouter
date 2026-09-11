mod common;
use brokerrouter::{
    error::Error,
    governance::{self, Check},
    human, store,
};
use common::*;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

async fn setup(pool: &PgPool) -> (store::Identity, String, Uuid) {
    let tenant = store::create_tenant(pool, "governance", 1_000_000)
        .await
        .unwrap();
    store::put_model(pool, tenant, "model", "model")
        .await
        .unwrap();
    let (key_id, key) =
        store::create_key(pool, tenant, "runtime", 1_000_000, &["model".into()], None)
            .await
            .unwrap();
    let who = store::Identity {
        tenant_id: tenant,
        key_id,
    };
    provision(pool, &who, "alice").await;
    let turn = turn(pool, &who, "alice", 0).await;
    (who, key, turn)
}
fn action(turn: Uuid, key: &str, payload: Value) -> Check {
    Check {
        turn_id: turn,
        operation_key: key.into(),
        stage: "tool_input".into(),
        model_id: None,
        tool_name: Some("send_message".into()),
        payload,
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn signed_human_assertions_reject_agent_forgery_wrong_audience_body_path_and_expiry(
    pool: PgPool,
) {
    let (who, key, _) = setup(&pool).await;
    let body = json!({});
    let path = "/v1/governance/turns";
    assert!(matches!(
        verifier().verify(&pool, &key, "POST", path, b"{}").await,
        Err(Error::HumanUnauthorized)
    ));
    let good = claims(who.tenant_id, "alice", path, &body);
    assert!(
        verifier()
            .verify(&pool, &sign(&good), "POST", path, b"{}")
            .await
            .is_ok()
    );
    for bad in 0..7 {
        let mut c = good.clone();
        match bad {
            0 => c.aud = "other".into(),
            1 => c.iss = "https://wrong.example".into(),
            2 => c.exp = c.iat - 1,
            3 => c.interactive = false,
            4 => c.nbf = c.iat + 60,
            5 => c.sub = "mallory".into(),
            _ => c.auth_time = c.iat - 7200,
        };
        assert!(
            verifier()
                .verify(&pool, &sign(&c), "POST", path, b"{}")
                .await
                .is_err()
        );
    }
    assert!(
        verifier()
            .verify(&pool, &sign(&good), "POST", "/else", b"{}")
            .await
            .is_err()
    );
    assert!(
        verifier()
            .verify(&pool, &sign(&good), "POST", path, b"{ }")
            .await
            .is_err()
    );
    assert!(
        verifier()
            .verify(&pool, &sign(&good), "GET", path, b"{}")
            .await
            .is_err()
    );
}
#[sqlx::test(migrations = "./migrations")]
async fn input_approvals_are_durable_deduplicated_and_partial_grants_only_cover_this_turn(
    pool: PgPool,
) {
    let (who, _, t) = setup(&pool).await;
    for id in ["a", "b"] {
        guardrail(
            &pool,
            who.tenant_id,
            id,
            1,
            "tool_input",
            json!({"kind":"always"}),
        )
        .await;
    }
    let check = action(t, "op", json!({"text":"sensitive"}));
    let a = governance::check(&pool, &cipher(), &who, check.clone())
        .await
        .unwrap();
    let b = governance::check(&pool, &cipher(), &who, check.clone())
        .await
        .unwrap();
    assert_eq!(a, b);
    let id = approval_id(&a);
    decide(&pool, "alice", id, approve(&["a"])).await.unwrap();
    let partial = governance::check(&pool, &cipher(), &who, check.clone())
        .await
        .unwrap();
    assert_eq!(
        partial["approval"]["guardrails"].as_array().unwrap().len(),
        1
    );
    assert_eq!(partial["approval"]["guardrails"][0]["id"], "b");
    decide(&pool, "alice", approval_id(&partial), approve(&["b"]))
        .await
        .unwrap();
    assert_eq!(
        governance::check(&pool, &cipher(), &who, check)
            .await
            .unwrap()["state"],
        "ready"
    );
    assert_eq!(
        governance::check(
            &pool,
            &cipher(),
            &who,
            action(t, "later", json!({"different":"content"}))
        )
        .await
        .unwrap()["state"],
        "ready"
    );
    let second = turn(&pool, &who, "alice", 1).await;
    assert!(matches!(
        governance::check(&pool, &cipher(), &who, action(t, "stale-turn", json!({}))).await,
        Err(Error::TurnClosed)
    ));
    assert_eq!(turn(&pool, &who, "alice", 0).await, t); // replay cannot revive the old generation or close the new one
    assert_eq!(
        governance::check(
            &pool,
            &cipher(),
            &who,
            action(second, "new-turn", json!({}))
        )
        .await
        .unwrap()["state"],
        "waiting_approval"
    );
    let grants: i64 = sqlx::query_scalar("SELECT count(*) FROM guardrail_grants")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(grants, 2);
}
#[sqlx::test(migrations = "./migrations")]
async fn concurrent_conflicting_decisions_have_one_grant_and_duplicate_decision_replays(
    pool: PgPool,
) {
    let (who, _, t) = setup(&pool).await;
    guardrail(
        &pool,
        who.tenant_id,
        "risk",
        1,
        "tool_input",
        json!({"kind":"always"}),
    )
    .await;
    let a = governance::check(&pool, &cipher(), &who, action(t, "op", json!({})))
        .await
        .unwrap();
    let id = approval_id(&a);
    let (approved, rejected) = tokio::join!(
        decide(&pool, "alice", id, approve(&["risk"])),
        decide(&pool, "alice", id, json!({"decision":"reject"}))
    );
    assert_eq!(
        usize::from(approved.is_ok()) + usize::from(rejected.is_ok()),
        1
    );
    let winning = if approved.is_ok() {
        approve(&["risk"])
    } else {
        json!({"decision":"reject"})
    };
    let state = decide(&pool, "alice", id, winning).await.unwrap();
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM governance_events WHERE event_type='approval.decided'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1);
    assert!(state["state"] == "approved" || state["state"] == "rejected");
}
#[sqlx::test(migrations = "./migrations")]
async fn closed_turn_key_revocation_foreign_user_and_new_policy_reject_late_approval(pool: PgPool) {
    let (who, _, t) = setup(&pool).await;
    guardrail(
        &pool,
        who.tenant_id,
        "risk",
        1,
        "tool_input",
        json!({"kind":"always"}),
    )
    .await;
    let a = governance::check(&pool, &cipher(), &who, action(t, "op", json!({})))
        .await
        .unwrap();
    let id = approval_id(&a);
    human::add_user(&pool, who.tenant_id, ISSUER, "bob")
        .await
        .unwrap();
    assert!(matches!(
        decide(&pool, "bob", id, approve(&["risk"])).await,
        Err(Error::NotFound)
    ));
    human::publish_guardrail(
        &pool,
        who.tenant_id,
        governance::Guardrail {
            id: "risk".into(),
            version: 2,
            name: "risk".into(),
            stage: "tool_input".into(),
            rule: json!({"kind":"always"}),
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        decide(&pool, "alice", id, approve(&["risk"])).await,
        Err(Error::PolicyChanged)
    ));
    let newer = governance::check(&pool, &cipher(), &who, action(t, "op", json!({})))
        .await
        .unwrap();
    assert_ne!(approval_id(&newer), id);
    let p = proof(&pool, "alice", "/close", &json!({"state":"completed"})).await;
    human::close_turn(&pool, &p, t, "completed").await.unwrap();
    assert!(matches!(
        decide(&pool, "alice", approval_id(&newer), approve(&["risk"])).await,
        Err(Error::TurnClosed)
    ));
    let t2 = turn(&pool, &who, "alice", 1).await;
    let a = governance::check(&pool, &cipher(), &who, action(t2, "op2", json!({})))
        .await
        .unwrap();
    store::revoke_key(&pool, who.tenant_id, who.key_id)
        .await
        .unwrap();
    assert!(
        decide(&pool, "alice", approval_id(&a), approve(&["risk"]))
            .await
            .is_err()
    );
}
#[sqlx::test(migrations = "./migrations")]
async fn feedback_is_not_approval_and_cancels_old_execution_generation(pool: PgPool) {
    let (who, _, t) = setup(&pool).await;
    guardrail(
        &pool,
        who.tenant_id,
        "risk",
        1,
        "tool_input",
        json!({"kind":"always"}),
    )
    .await;
    let a = governance::check(&pool, &cipher(), &who, action(t, "op", json!({})))
        .await
        .unwrap();
    decide(
        &pool,
        "alice",
        approval_id(&a),
        json!({"decision":"feedback","feedback":"remove personal data first"}),
    )
    .await
    .unwrap();
    let grants: i64 = sqlx::query_scalar("SELECT count(*) FROM guardrail_grants")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(grants, 0);
    assert!(matches!(
        governance::check(&pool, &cipher(), &who, action(t, "replan", json!({}))).await,
        Err(Error::TurnClosed)
    ));
    let approvals = governance::approvals(&pool, &cipher(), &who, t)
        .await
        .unwrap();
    assert_eq!(
        approvals["data"][0]["feedback"],
        "remove personal data first"
    );
    let raw: Vec<u8> = sqlx::query_scalar("SELECT feedback_ciphertext FROM approvals")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(!raw.windows(6).any(|w| w == b"remove"));
}
#[sqlx::test(migrations = "./migrations")]
async fn execution_claim_is_single_owner_and_expired_execution_is_never_reissued(pool: PgPool) {
    let (who, _, t) = setup(&pool).await;
    let a = governance::check(
        &pool,
        &cipher(),
        &who,
        action(t, "op", json!({"send":"message"})),
    )
    .await
    .unwrap();
    let id = serde_json::from_value(a["id"].clone()).unwrap();
    let hash = a["payload_hash"].as_str().unwrap().to_owned();
    let claim = |token: &str| governance::Claim {
        token: token.repeat(32),
        payload_hash: hash.clone(),
    };
    let c1 = cipher();
    let c2 = cipher();
    let (first, second) = tokio::join!(
        governance::claim(&pool, &c1, &who, id, claim("11")),
        governance::claim(&pool, &c2, &who, id, claim("22"))
    );
    assert_eq!(usize::from(first.is_ok()) + usize::from(second.is_ok()), 1);
    let token = if first.is_ok() { "11" } else { "22" };
    let replay = governance::claim(&pool, &cipher(), &who, id, claim(token))
        .await
        .unwrap();
    assert_eq!(replay["dispatch_authorized"], false);
    sqlx::query("UPDATE governance_operations SET execution_deadline=now()-interval '1 second'")
        .execute(&pool)
        .await
        .unwrap();
    governance::maintain(&pool).await.unwrap();
    assert_eq!(
        governance::operation(&pool, &cipher(), &who, id)
            .await
            .unwrap()["state"],
        "execution_unknown"
    );
    assert!(
        governance::claim(&pool, &cipher(), &who, id, claim(token))
            .await
            .is_err()
    );
    let receipt = || governance::Receipt {
        token: token.repeat(32),
        outcome: "completed".into(),
        evidence_ref: "runtime://persisted-receipt/1".into(),
    };
    governance::receipt(&pool, &cipher(), &who, id, receipt())
        .await
        .unwrap();
    governance::receipt(&pool, &cipher(), &who, id, receipt())
        .await
        .unwrap();
    assert_eq!(
        governance::operation(&pool, &cipher(), &who, id)
            .await
            .unwrap()["state"],
        "completed"
    );
}
#[sqlx::test(migrations = "./migrations")]
async fn policy_selectors_literal_rules_and_payload_hash_prevent_action_substitution(pool: PgPool) {
    let (who, _, t) = setup(&pool).await;
    human::publish_guardrail(
        &pool,
        who.tenant_id,
        governance::Guardrail {
            id: "risk".into(),
            version: 1,
            name: "risk".into(),
            stage: "tool_input".into(),
            rule: json!({"kind":"contains_any","terms":["secret"],"case_sensitive":false}),
        },
    )
    .await
    .unwrap();
    let mut policy = human::Policy {
        id: Uuid::new_v4(),
        guardrail_id: "risk".into(),
        model_id: None,
        key_id: Some(who.key_id),
        runtime_id: Some("different-runtime".into()),
        tool_name: Some("send_message".into()),
        active: true,
    };
    human::put_policy(
        &pool,
        who.tenant_id,
        serde_json::from_value(serde_json::to_value(&policy).unwrap()).unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(
        governance::check(
            &pool,
            &cipher(),
            &who,
            action(t, "unmatched", json!({"text":"SECRET"}))
        )
        .await
        .unwrap()["state"],
        "ready"
    );
    policy.runtime_id = Some("hermes-fixture".into());
    human::put_policy(&pool, who.tenant_id, policy)
        .await
        .unwrap();
    assert_eq!(
        governance::check(
            &pool,
            &cipher(),
            &who,
            action(t, "risk", json!({"text":"SECRET"}))
        )
        .await
        .unwrap()["state"],
        "waiting_approval"
    );
    assert!(matches!(
        governance::check(
            &pool,
            &cipher(),
            &who,
            action(t, "risk", json!({"text":"safe"}))
        )
        .await,
        Err(Error::Conflict)
    ));
    let a = governance::check(
        &pool,
        &cipher(),
        &who,
        action(t, "safe", json!({"text":"hello"})),
    )
    .await
    .unwrap();
    assert!(
        governance::claim(
            &pool,
            &cipher(),
            &who,
            serde_json::from_value(a["id"].clone()).unwrap(),
            governance::Claim {
                token: "11".repeat(32),
                payload_hash: "wrong".into()
            }
        )
        .await
        .is_err()
    );
}
#[sqlx::test(migrations = "./migrations")]
async fn durable_events_resume_and_ack_monotonically_without_content_leak(pool: PgPool) {
    let (who, _, t) = setup(&pool).await;
    guardrail(
        &pool,
        who.tenant_id,
        "risk",
        1,
        "tool_input",
        json!({"kind":"always"}),
    )
    .await;
    let a = governance::check(
        &pool,
        &cipher(),
        &who,
        action(t, "op", json!({"secret":"NEVER_LOG_THIS"})),
    )
    .await
    .unwrap();
    let before = governance::events(&pool, &who, t, 0, None).await.unwrap();
    let cursor = before["next_cursor"].as_i64().unwrap();
    decide(&pool, "alice", approval_id(&a), approve(&["risk"]))
        .await
        .unwrap();
    let after = governance::events(&pool, &who, t, cursor, Some(cursor))
        .await
        .unwrap();
    assert_eq!(after["data"].as_array().unwrap().len(), 1);
    assert!(!before.to_string().contains("NEVER_LOG_THIS"));
    assert!(!after.to_string().contains("NEVER_LOG_THIS"));
    governance::events(&pool, &who, t, 0, Some(0))
        .await
        .unwrap();
    let ack: i64 = sqlx::query_scalar("SELECT sequence FROM governance_event_acks")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(ack, cursor);
    assert!(
        governance::events(&pool, &who, t, 0, Some(999))
            .await
            .is_err()
    );
}
#[sqlx::test(migrations = "./migrations")]
async fn real_database_roles_separate_agent_runtime_from_human_grant_authority(pool: PgPool) {
    let (who, _, t) = setup(&pool).await;
    guardrail(
        &pool,
        who.tenant_id,
        "risk",
        1,
        "tool_input",
        json!({"kind":"always"}),
    )
    .await;
    sqlx::raw_sql(include_str!("../deploy/permissions.sql"))
        .execute(&pool)
        .await
        .unwrap();
    let role = |name: &'static str| {
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(3)
            .after_connect(move |c, _| {
                Box::pin(async move {
                    sqlx::query(&format!("SET ROLE {name}")).execute(c).await?;
                    Ok(())
                })
            })
            .connect_with(pool.connect_options().as_ref().clone())
    };
    let runtime = role("brokerrouter_runtime").await.unwrap();
    let control = role("brokerrouter_governance_control").await.unwrap();
    let a = governance::check(&runtime, &cipher(), &who, action(t, "op", json!({})))
        .await
        .unwrap();
    let id = approval_id(&a);
    assert!(
        sqlx::query("UPDATE approvals SET state='approved' WHERE id=$1")
            .bind(id)
            .execute(&runtime)
            .await
            .is_err()
    );
    assert!(sqlx::query("INSERT INTO guardrail_grants(tenant_id,turn_id,guardrail_id,version,approval_id,expires_at) VALUES($1,$2,'risk',1,$3,now()+interval '1 minute')").bind(who.tenant_id).bind(t).bind(id).execute(&runtime).await.is_err());
    decide(&control, "alice", id, approve(&["risk"]))
        .await
        .unwrap();
    assert_eq!(
        governance::check(&runtime, &cipher(), &who, action(t, "op", json!({})))
            .await
            .unwrap()["state"],
        "ready"
    );
    assert!(
        sqlx::query("UPDATE tenants SET spent_micros=1")
            .execute(&control)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("DELETE FROM governance_events")
            .execute(&runtime)
            .await
            .is_err()
    );
    human::maintain(&control).await.unwrap();
    governance::maintain(&runtime).await.unwrap();
    governance::close_runtime(&runtime, &who, t, "completed")
        .await
        .unwrap();
    assert!(
        sqlx::query("UPDATE governance_turns SET state='active' WHERE id=$1")
            .bind(t)
            .execute(&runtime)
            .await
            .is_err()
    );
    assert!(
        decide(&control, "alice", id, approve(&["risk"]))
            .await
            .is_ok()
    ); // immutable decision replay, not new authority
}

#[sqlx::test(migrations = "./migrations")]
async fn approval_commit_failure_rolls_back_decision_grant_and_event(pool: PgPool) {
    let (who, _, t) = setup(&pool).await;
    guardrail(
        &pool,
        who.tenant_id,
        "risk",
        1,
        "tool_input",
        json!({"kind":"always"}),
    )
    .await;
    let a = governance::check(&pool, &cipher(), &who, action(t, "op", json!({})))
        .await
        .unwrap();
    let id = approval_id(&a);
    sqlx::raw_sql("CREATE FUNCTION reject_grant() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected failure'; END $$; CREATE TRIGGER fail_grant BEFORE INSERT ON guardrail_grants FOR EACH ROW EXECUTE FUNCTION reject_grant();").execute(&pool).await.unwrap();
    assert!(
        decide(&pool, "alice", id, approve(&["risk"]))
            .await
            .is_err()
    );
    assert_eq!(
        governance::approval(&pool, &cipher(), &who, id)
            .await
            .unwrap()["state"],
        "pending"
    );
    let facts:(i64,i64)=sqlx::query_as("SELECT (SELECT count(*) FROM guardrail_grants),(SELECT count(*) FROM governance_events WHERE event_type='approval.decided')").fetch_one(&pool).await.unwrap();
    assert_eq!(facts, (0, 0));
    sqlx::query("DROP TRIGGER fail_grant ON guardrail_grants")
        .execute(&pool)
        .await
        .unwrap();
    decide(&pool, "alice", id, approve(&["risk"]))
        .await
        .unwrap();
}
#[sqlx::test(migrations = "./migrations")]
async fn human_reconciliation_closes_unknown_tool_without_authorizing_reexecution(pool: PgPool) {
    let (who, _, t) = setup(&pool).await;
    let a = governance::check(&pool, &cipher(), &who, action(t, "op", json!({})))
        .await
        .unwrap();
    let id: Uuid = serde_json::from_value(a["id"].clone()).unwrap();
    let claim = || governance::Claim {
        token: "99".repeat(32),
        payload_hash: a["payload_hash"].as_str().unwrap().into(),
    };
    governance::claim(&pool, &cipher(), &who, id, claim())
        .await
        .unwrap();
    governance::receipt(
        &pool,
        &cipher(),
        &who,
        id,
        governance::Receipt {
            token: "99".repeat(32),
            outcome: "execution_unknown".into(),
            evidence_ref: "runtime://lost-ack".into(),
        },
    )
    .await
    .unwrap();
    let body = json!({"outcome":"confirmed_not_executed","evidence_ref":"audit://external-service-query/7"});
    let p = proof(&pool, "alice", "/reconcile", &body).await;
    human::reconcile_operation(
        &pool,
        &cipher(),
        &p,
        id,
        serde_json::from_value(body.clone()).unwrap(),
    )
    .await
    .unwrap();
    human::reconcile_operation(
        &pool,
        &cipher(),
        &p,
        id,
        serde_json::from_value(body).unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(
        governance::operation(&pool, &cipher(), &who, id)
            .await
            .unwrap()["state"],
        "rejected"
    );
    assert!(
        governance::claim(&pool, &cipher(), &who, id, claim())
            .await
            .is_err()
    );
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM governance_events WHERE event_type='operation.reconciled'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1);
}
#[sqlx::test(migrations = "./migrations")]
async fn expired_turn_content_and_human_assertions_are_purged_but_metadata_remains(pool: PgPool) {
    let (who, _, t) = setup(&pool).await;
    guardrail(
        &pool,
        who.tenant_id,
        "risk",
        1,
        "tool_input",
        json!({"kind":"always"}),
    )
    .await;
    let a = governance::check(
        &pool,
        &cipher(),
        &who,
        action(t, "op", json!({"text":"private"})),
    )
    .await
    .unwrap();
    let id = approval_id(&a);
    decide(
        &pool,
        "alice",
        id,
        json!({"decision":"feedback","feedback":"private user feedback"}),
    )
    .await
    .unwrap();
    sqlx::query("UPDATE approvals SET expires_at=now()-interval '25 hours'")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE human_assertions SET expires_at=now()-interval '1 second'")
        .execute(&pool)
        .await
        .unwrap();
    governance::maintain(&pool).await.unwrap();
    human::maintain(&pool).await.unwrap();
    let counts:(i64,i64,i64)=sqlx::query_as("SELECT (SELECT count(*) FROM governance_operations WHERE payload_ciphertext IS NOT NULL),(SELECT count(*) FROM approvals WHERE feedback_ciphertext IS NOT NULL),(SELECT count(*) FROM human_assertions)").fetch_one(&pool).await.unwrap();
    assert_eq!(counts, (0, 0, 0));
    let view = governance::approval(&pool, &cipher(), &who, id)
        .await
        .unwrap();
    assert_eq!(view["state"], "feedback");
    assert!(view.get("feedback").is_none());
}
