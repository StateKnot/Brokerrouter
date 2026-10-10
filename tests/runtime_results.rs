mod common;
use axum::{Json, Router, extract::State, routing::post};
use brokerrouter::{
    error::Error,
    gateway::Gateway,
    governance::{self, Check},
    money,
    runtime_results::{self, Record},
    store,
};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use uuid::Uuid;

async fn setup(pool: &PgPool) -> (store::Identity, Uuid) {
    let tenant = store::create_tenant(pool, "runtime-results", 1_000_000)
        .await
        .unwrap();
    store::put_model(pool, tenant, "model", "Model")
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
    common::provision(pool, &who, "alice").await;
    let turn = common::turn(pool, &who, "alice", 0).await;
    (who, turn)
}

fn gateway(pool: &PgPool) -> Gateway {
    Gateway::new(
        pool.clone(),
        common::cipher(),
        HashMap::from([("FIXTURE_KEY".into(), "fixture-secret".into())]),
        4,
        5,
        60,
    )
    .unwrap()
}

async fn claimed(pool: &PgPool, who: &store::Identity, turn: Uuid, key: &str) -> Record {
    let op = governance::check(
        pool,
        &common::cipher(),
        who,
        Check {
            turn_id: turn,
            operation_key: key.into(),
            stage: "tool_input".into(),
            model_id: Some("model".into()),
            tool_name: Some("fixture_tool".into()),
            payload: json!({"argument":"bounded fixture"}),
        },
    )
    .await
    .unwrap();
    assert_eq!(op["state"], "ready");
    let id = serde_json::from_value(op["id"].clone()).unwrap();
    let token = "11".repeat(32);
    let claim = governance::claim(
        pool,
        &common::cipher(),
        who,
        id,
        governance::Claim {
            token: token.clone(),
            payload_hash: op["payload_hash"].as_str().unwrap().into(),
        },
    )
    .await
    .unwrap();
    assert_eq!(claim["dispatch_authorized"], true);
    Record {
        input_operation_id: id,
        claim_token: token,
        evidence_ref: format!("runtime://fixture/{id}"),
        text: "private tool text".into(),
    }
}

fn result_id(value: &Value) -> Uuid {
    serde_json::from_value(value["id"].clone()).unwrap()
}

#[sqlx::test(migrations = "./migrations")]
async fn recording_is_encrypted_atomic_and_response_loss_idempotent(pool: PgPool) {
    let (who, turn) = setup(&pool).await;
    let request = claimed(&pool, &who, turn, "once").await;
    let cipher = common::cipher();
    let (first, retry) = tokio::join!(
        runtime_results::record(&pool, &cipher, &who, request.clone()),
        runtime_results::record(&pool, &cipher, &who, request.clone()),
    );
    let first = first.unwrap();
    assert_eq!(first, retry.unwrap());
    assert_eq!(first["state"], "recorded");
    assert!(first.get("text").is_none());
    assert_eq!(first["turn_id"], turn.to_string());
    assert_eq!(
        first["input_operation_id"],
        request.input_operation_id.to_string()
    );
    assert_eq!(first["bytes"], request.text.len());
    assert_eq!(
        first["text_sha256"],
        common::hex(&store::hash(request.text.as_bytes()))
    );
    let id = result_id(&first);
    let ciphertext: Vec<u8> = sqlx::query_scalar(
        "SELECT result_ciphertext FROM runtime_tool_results WHERE tenant_id=$1 AND id=$2",
    )
    .bind(who.tenant_id)
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        !ciphertext
            .windows(request.text.len())
            .any(|v| v == request.text.as_bytes())
    );
    assert_eq!(
        common::cipher()
            .decrypt(who.tenant_id, id, &ciphertext)
            .unwrap(),
        request.text.as_bytes()
    );
    let valid_clock: bool = sqlx::query_scalar(
        "SELECT created_at<=clock_timestamp() AND created_at>clock_timestamp()-interval '1 minute' AND expires_at>=created_at+interval '60 seconds' AND expires_at<=created_at+interval '7 days' FROM runtime_tool_results WHERE tenant_id=$1 AND id=$2")
        .bind(who.tenant_id).bind(id).fetch_one(&pool).await.unwrap();
    assert!(valid_clock);
    let facts: (String, i64, i64) = sqlx::query_as(
        "SELECT state,(SELECT count(*) FROM runtime_tool_results),(SELECT count(*) FROM governance_events WHERE event_type='tool_result.recorded') FROM governance_operations WHERE tenant_id=$1 AND id=$2")
        .bind(who.tenant_id).bind(request.input_operation_id).fetch_one(&pool).await.unwrap();
    assert_eq!(facts, ("completed".into(), 1, 1));
    let mut changed = request.clone();
    changed.text.push_str(" changed");
    assert!(matches!(
        runtime_results::record(&pool, &common::cipher(), &who, changed).await,
        Err(Error::Conflict)
    ));
    let mut changed = request.clone();
    changed.evidence_ref.push_str("-changed");
    assert!(matches!(
        runtime_results::record(&pool, &common::cipher(), &who, changed).await,
        Err(Error::Conflict)
    ));
    let mut changed = request;
    changed.claim_token = "22".repeat(32);
    assert!(matches!(
        runtime_results::record(&pool, &common::cipher(), &who, changed).await,
        Err(Error::NotFound)
    ));
}

#[sqlx::test(migrations = "./migrations")]
async fn concurrent_different_results_cannot_replace_the_winner(pool: PgPool) {
    let (who, turn) = setup(&pool).await;
    let one = claimed(&pool, &who, turn, "race").await;
    let mut two = one.clone();
    two.text = "different completion".into();
    let cipher = common::cipher();
    let (a, b) = tokio::join!(
        runtime_results::record(&pool, &cipher, &who, one),
        runtime_results::record(&pool, &cipher, &who, two),
    );
    assert!(matches!(
        (&a, &b),
        (Ok(_), Err(Error::Conflict)) | (Err(Error::Conflict), Ok(_))
    ));
    let counts: (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM runtime_tool_results),(SELECT count(*) FROM governance_events WHERE event_type='operation.receipted')")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(counts, (1, 1));
}

#[sqlx::test(migrations = "./migrations")]
async fn schema_protects_immutable_facts_and_requires_a_claimed_tool_input(pool: PgPool) {
    let (who, turn) = setup(&pool).await;
    let request = claimed(&pool, &who, turn, "schema").await;
    let recorded = runtime_results::record(&pool, &common::cipher(), &who, request.clone())
        .await
        .unwrap();
    let id = result_id(&recorded);
    // These use the migration owner so the immutable trigger, rather than only
    // a column privilege denial, is what protects the original evidence.
    for statement in [
        "UPDATE runtime_tool_results SET text_sha256=decode(repeat('22',32),'hex') WHERE id=$1",
        "UPDATE runtime_tool_results SET evidence_ref='forged' WHERE id=$1",
        "UPDATE runtime_tool_results SET claim_hash=decode(repeat('22',32),'hex') WHERE id=$1",
        "UPDATE runtime_tool_results SET tool_name='different-tool' WHERE id=$1",
        "UPDATE runtime_tool_results SET bytes=bytes+1 WHERE id=$1",
        "UPDATE runtime_tool_results SET expires_at=expires_at+interval '1 minute' WHERE id=$1",
        "DELETE FROM runtime_tool_results WHERE id=$1",
    ] {
        assert!(
            sqlx::query(statement)
                .bind(id)
                .execute(&pool)
                .await
                .is_err(),
            "{statement}"
        );
    }
    let output = governance::check(
        &pool,
        &common::cipher(),
        &who,
        Check {
            turn_id: turn,
            operation_key: "wrong-stage".into(),
            stage: "tool_output".into(),
            model_id: Some("model".into()),
            tool_name: Some("fixture_tool".into()),
            payload: json!({"text":"not an executed input"}),
        },
    )
    .await
    .unwrap();
    let wrong_input = result_id(&output);
    let mut wrong = request.clone();
    wrong.input_operation_id = wrong_input;
    assert!(matches!(
        runtime_results::record(&pool, &common::cipher(), &who, wrong).await,
        Err(Error::Invalid(_))
    ));
    assert!(sqlx::query("INSERT INTO runtime_tool_results(tenant_id,id,key_id,turn_id,input_operation_id,model_id,tool_name,claim_hash,receipt_hash,evidence_ref,text_sha256,bytes,result_ciphertext,created_at,expires_at) SELECT tenant_id,$2,key_id,turn_id,$3,model_id,tool_name,claim_hash,receipt_hash,evidence_ref,text_sha256,bytes,result_ciphertext,created_at,expires_at FROM runtime_tool_results WHERE id=$1")
        .bind(id).bind(Uuid::new_v4()).bind(wrong_input).execute(&pool).await.is_err());
    let unclaimed = governance::check(
        &pool,
        &common::cipher(),
        &who,
        Check {
            turn_id: turn,
            operation_key: "unclaimed".into(),
            stage: "tool_input".into(),
            model_id: Some("model".into()),
            tool_name: Some("fixture_tool".into()),
            payload: json!({}),
        },
    )
    .await
    .unwrap();
    let mut wrong = request;
    wrong.input_operation_id = result_id(&unclaimed);
    assert!(matches!(
        runtime_results::record(&pool, &common::cipher(), &who, wrong).await,
        Err(Error::NotFound)
    ));
}

#[sqlx::test(migrations = "./migrations")]
async fn earlier_completed_receipt_can_be_bound_to_its_identical_result(pool: PgPool) {
    let (who, turn) = setup(&pool).await;
    let request = claimed(&pool, &who, turn, "receipt-response-loss").await;
    governance::receipt(
        &pool,
        &common::cipher(),
        &who,
        request.input_operation_id,
        governance::Receipt {
            token: request.claim_token.clone(),
            outcome: "completed".into(),
            evidence_ref: request.evidence_ref.clone(),
        },
    )
    .await
    .unwrap();
    let recorded = runtime_results::record(&pool, &common::cipher(), &who, request.clone())
        .await
        .unwrap();
    assert_eq!(
        recorded,
        runtime_results::record(&pool, &common::cipher(), &who, request)
            .await
            .unwrap()
    );
    let receipts: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM governance_events WHERE event_type='operation.receipted'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(receipts, 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn failed_result_insert_rolls_back_its_completed_receipt(pool: PgPool) {
    let (who, turn) = setup(&pool).await;
    let request = claimed(&pool, &who, turn, "atomic").await;
    sqlx::raw_sql("CREATE FUNCTION fixture_result_abort() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'fixture insert failure'; END $$; CREATE TRIGGER fixture_result_abort BEFORE INSERT ON runtime_tool_results FOR EACH ROW EXECUTE FUNCTION fixture_result_abort();")
        .execute(&pool).await.unwrap();
    assert!(
        runtime_results::record(&pool, &common::cipher(), &who, request.clone())
            .await
            .is_err()
    );
    let state: (String, bool, i64) = sqlx::query_as(
        "SELECT state,receipt_hash IS NULL,(SELECT count(*) FROM runtime_tool_results) FROM governance_operations WHERE tenant_id=$1 AND id=$2")
        .bind(who.tenant_id).bind(request.input_operation_id).fetch_one(&pool).await.unwrap();
    assert_eq!(state, ("executing".into(), true, 0));
    sqlx::query("DROP TRIGGER fixture_result_abort ON runtime_tool_results")
        .execute(&pool)
        .await
        .unwrap();
    runtime_results::record(&pool, &common::cipher(), &who, request)
        .await
        .unwrap();
}

#[sqlx::test(migrations = "./migrations")]
async fn wire_limit_rejects_oversized_json_without_truncation_or_receipt(pool: PgPool) {
    let (who, turn) = setup(&pool).await;
    let mut request = claimed(&pool, &who, turn, "wire").await;
    for text in [
        "x".repeat(runtime_results::MAX_TEXT_JSON_BYTES),
        "\0".repeat(90_000),
    ] {
        request.text = text;
        assert!(matches!(
            runtime_results::record(&pool, &common::cipher(), &who, request.clone()).await,
            Err(Error::Invalid(_))
        ));
        let state: (String, bool) = sqlx::query_as(
            "SELECT state,receipt_hash IS NULL FROM governance_operations WHERE tenant_id=$1 AND id=$2")
            .bind(who.tenant_id).bind(request.input_operation_id).fetch_one(&pool).await.unwrap();
        assert_eq!(state, ("executing".into(), true));
    }
    request.text = "x".repeat(runtime_results::MAX_TEXT_JSON_BYTES - 2);
    let recorded = runtime_results::record(&pool, &common::cipher(), &who, request.clone())
        .await
        .unwrap();
    let released = runtime_results::release(&gateway(&pool), &who, result_id(&recorded))
        .await
        .unwrap();
    assert_eq!(released["text"], request.text);
    assert!(serde_json::to_vec(&released).unwrap().len() < 1024 * 1024);
    let mut body = serde_json::to_value(&request).unwrap();
    body["ttl_seconds"] = json!(604800);
    assert!(serde_json::from_value::<Record>(body).is_err());
}

#[sqlx::test(migrations = "./migrations")]
async fn release_rechecks_current_output_policy_and_grant_versions(pool: PgPool) {
    let (who, turn) = setup(&pool).await;
    let request = claimed(&pool, &who, turn, "output").await;
    let recorded = runtime_results::record(&pool, &common::cipher(), &who, request.clone())
        .await
        .unwrap();
    let id = result_id(&recorded);
    let g = gateway(&pool);
    let first = runtime_results::release(&g, &who, id).await.unwrap();
    assert_eq!(first["state"], "ready");
    assert_eq!(first["text"], request.text);
    assert_eq!(first["result_id"], id.to_string());
    assert_eq!(first["id"], first["output_operation_id"]);
    common::guardrail(
        &pool,
        who.tenant_id,
        "output-risk",
        1,
        "tool_output",
        json!({"kind":"always"}),
    )
    .await;
    let waiting = runtime_results::release(&g, &who, id).await.unwrap();
    assert_eq!(waiting["state"], "waiting_approval");
    assert_eq!(waiting["id"], first["id"]);
    assert_eq!(waiting["approval"]["operation_id"], waiting["id"]);
    assert!(waiting.get("text").is_none());
    assert!(!waiting.to_string().contains(&request.text));
    common::decide(
        &pool,
        "alice",
        common::approval_id(&waiting),
        common::approve(&["output-risk"]),
    )
    .await
    .unwrap();
    assert_eq!(
        runtime_results::release(&g, &who, id).await.unwrap()["text"],
        request.text
    );
    common::guardrail(
        &pool,
        who.tenant_id,
        "output-risk",
        2,
        "tool_output",
        json!({"kind":"always"}),
    )
    .await;
    let next = runtime_results::release(&g, &who, id).await.unwrap();
    assert_eq!(next["state"], "waiting_approval");
    assert_ne!(next["approval"]["id"], waiting["approval"]["id"]);
    assert!(next.get("text").is_none());
    let claimed: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM governance_events WHERE event_type='operation.claimed'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(claimed, 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn grant_expiry_during_the_final_release_gate_never_publishes_text(pool: PgPool) {
    let (who, turn) = setup(&pool).await;
    let request = claimed(&pool, &who, turn, "grant-final-clock").await;
    let recorded = runtime_results::record(&pool, &common::cipher(), &who, request)
        .await
        .unwrap();
    let id = result_id(&recorded);
    common::guardrail(
        &pool,
        who.tenant_id,
        "short-output-grant",
        1,
        "tool_output",
        json!({"kind":"always"}),
    )
    .await;
    let g = gateway(&pool);
    let waiting = runtime_results::release(&g, &who, id).await.unwrap();
    let approval = common::approval_id(&waiting);
    // Delay only the second gate: Gateway::check's first gate must finish while
    // its grant is valid. A sequence proves the final gate ran even if its
    // transaction rolls back, without weakening immutable grant evidence.
    sqlx::raw_sql(
        "CREATE SEQUENCE fixture_final_gate_count;
        CREATE FUNCTION fixture_delay_final_gate() RETURNS trigger LANGUAGE plpgsql AS $$
        DECLARE deadline timestamptz;
        BEGIN
            IF NEW.event_type='guardrails.evaluated'
               AND nextval('fixture_final_gate_count')=2 THEN
                SELECT max(expires_at) INTO deadline FROM guardrail_grants
                    WHERE tenant_id=NEW.tenant_id AND turn_id=NEW.turn_id;
                PERFORM pg_sleep(GREATEST(0,extract(epoch FROM deadline-clock_timestamp()))+0.05);
            END IF;
            RETURN NEW;
        END;
        $$;
        CREATE TRIGGER fixture_delay_final_gate BEFORE INSERT ON governance_events
            FOR EACH ROW EXECUTE FUNCTION fixture_delay_final_gate();",
    )
    .execute(&pool)
    .await
    .unwrap();
    // The migration owner imports a short pending approval for a fast clock
    // fixture; the real human decision still mints the grant through its API.
    sqlx::query("UPDATE approvals SET expires_at=clock_timestamp()+interval '2 seconds' WHERE tenant_id=$1 AND id=$2")
        .bind(who.tenant_id).bind(approval).execute(&pool).await.unwrap();
    common::decide(
        &pool,
        "alice",
        approval,
        common::approve(&["short-output-grant"]),
    )
    .await
    .unwrap();
    let released = runtime_results::release(&g, &who, id).await;
    let second_gate: i64 = sqlx::query_scalar("SELECT last_value FROM fixture_final_gate_count")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(second_gate, 2);
    let expired: bool = sqlx::query_scalar("SELECT expires_at<=clock_timestamp() FROM guardrail_grants WHERE tenant_id=$1 AND turn_id=$2 AND guardrail_id='short-output-grant'")
        .bind(who.tenant_id).bind(turn).fetch_one(&pool).await.unwrap();
    assert!(expired);
    assert!(
        matches!(released, Err(Error::PolicyChanged)),
        "release state after grant expiry: {:?}",
        released.as_ref().ok().map(|v| &v["state"]),
    );
    let facts: (i64, i64) = sqlx::query_as("SELECT (SELECT count(*) FROM runtime_tool_results),(SELECT count(*) FROM governance_events WHERE event_type='operation.claimed')")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(facts, (1, 1));
}

#[sqlx::test(migrations = "./migrations")]
async fn grant_expiry_during_claim_event_rolls_back_dispatch_authority(pool: PgPool) {
    let (who, turn) = setup(&pool).await;
    common::guardrail(
        &pool,
        who.tenant_id,
        "short-input-grant",
        1,
        "tool_input",
        json!({"kind":"always"}),
    )
    .await;
    let waiting = governance::check(
        &pool,
        &common::cipher(),
        &who,
        Check {
            turn_id: turn,
            operation_key: "grant-claim-clock".into(),
            stage: "tool_input".into(),
            model_id: Some("model".into()),
            tool_name: Some("fixture_tool".into()),
            payload: json!({"argument":"fixed claim argument"}),
        },
    )
    .await
    .unwrap();
    let input = result_id(&waiting);
    let operation = governance::operation(&pool, &common::cipher(), &who, input)
        .await
        .unwrap();
    let approval = common::approval_id(&waiting);
    sqlx::raw_sql(
        "CREATE FUNCTION fixture_delay_claim_event() RETURNS trigger LANGUAGE plpgsql AS $$
        DECLARE deadline timestamptz;
        BEGIN
            IF NEW.event_type='operation.claimed' THEN
                SELECT max(expires_at) INTO deadline FROM guardrail_grants
                    WHERE tenant_id=NEW.tenant_id AND turn_id=NEW.turn_id;
                PERFORM pg_sleep(GREATEST(0,extract(epoch FROM deadline-clock_timestamp()))+0.05);
            END IF;
            RETURN NEW;
        END;
        $$;
        CREATE TRIGGER fixture_delay_claim_event BEFORE INSERT ON governance_events
            FOR EACH ROW EXECUTE FUNCTION fixture_delay_claim_event();",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE approvals SET expires_at=clock_timestamp()+interval '2 seconds' WHERE tenant_id=$1 AND id=$2")
        .bind(who.tenant_id).bind(approval).execute(&pool).await.unwrap();
    common::decide(
        &pool,
        "alice",
        approval,
        common::approve(&["short-input-grant"]),
    )
    .await
    .unwrap();
    let claimed = governance::claim(
        &pool,
        &common::cipher(),
        &who,
        input,
        governance::Claim {
            token: "22".repeat(32),
            payload_hash: operation["payload_hash"].as_str().unwrap().into(),
        },
    )
    .await;
    assert!(
        matches!(claimed, Err(Error::PolicyChanged)),
        "claim authorized after grant expiry: {:?}",
        claimed.as_ref().ok().map(|v| &v["dispatch_authorized"]),
    );
    let facts: (String, bool, bool, i64) = sqlx::query_as("SELECT state,claim_hash IS NULL,execution_deadline IS NULL,(SELECT count(*) FROM governance_events WHERE event_type='operation.claimed') FROM governance_operations WHERE tenant_id=$1 AND id=$2")
        .bind(who.tenant_id).bind(input).fetch_one(&pool).await.unwrap();
    assert_eq!(facts, ("waiting_approval".into(), true, true, 0));
}

fn semantic_rule() -> Value {
    json!({"kind":"model_judge","model":"model","instructions":"Flag risky tool results.",
        "input_format":"text_json","max_input_bytes":65536,"max_output_tokens":32,"maximum_cny":"0.1"})
}

#[sqlx::test(migrations = "./migrations")]
async fn generic_completed_output_cannot_bypass_new_policy_or_missing_judge_payload(pool: PgPool) {
    let (who, turn) = setup(&pool).await;
    let request = claimed(&pool, &who, turn, "generic-completion").await;
    let recorded = runtime_results::record(&pool, &common::cipher(), &who, request)
        .await
        .unwrap();
    let id = result_id(&recorded);
    let g = gateway(&pool);
    let released = runtime_results::release(&g, &who, id).await.unwrap();
    let output = result_id(&released);
    let token = "22".repeat(32);
    governance::claim(
        &pool,
        &common::cipher(),
        &who,
        output,
        governance::Claim {
            token: token.clone(),
            payload_hash: released["payload_hash"].as_str().unwrap().into(),
        },
    )
    .await
    .unwrap();
    governance::receipt(
        &pool,
        &common::cipher(),
        &who,
        output,
        governance::Receipt {
            token,
            outcome: "completed".into(),
            evidence_ref: "runtime://generic-output-claim".into(),
        },
    )
    .await
    .unwrap();
    common::guardrail(
        &pool,
        who.tenant_id,
        "new-output-rule",
        1,
        "tool_output",
        json!({"kind":"always"}),
    )
    .await;
    let waiting = runtime_results::release(&g, &who, id).await.unwrap();
    assert_eq!(waiting["state"], "waiting_approval");
    assert!(waiting.get("text").is_none());
    common::guardrail(
        &pool,
        who.tenant_id,
        "new-judge",
        1,
        "tool_output",
        semantic_rule(),
    )
    .await;
    assert!(matches!(
        runtime_results::release(&g, &who, id).await,
        Err(Error::ResultUnavailable)
    ));
    let pending: i64 =
        sqlx::query_scalar("SELECT count(*) FROM guardrail_evaluations WHERE state='pending'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(pending, 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn current_model_grant_and_runtime_binding_are_required_at_release(pool: PgPool) {
    let (who, turn) = setup(&pool).await;
    let request = claimed(&pool, &who, turn, "revocation").await;
    let recorded = runtime_results::record(&pool, &common::cipher(), &who, request)
        .await
        .unwrap();
    let id = result_id(&recorded);
    let g = gateway(&pool);
    assert_eq!(
        runtime_results::release(&g, &who, id).await.unwrap()["state"],
        "ready"
    );
    sqlx::query("DELETE FROM key_models WHERE tenant_id=$1 AND key_id=$2 AND model_id='model'")
        .bind(who.tenant_id)
        .bind(who.key_id)
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        runtime_results::release(&g, &who, id).await,
        Err(Error::Unauthorized)
    ));
    sqlx::query("INSERT INTO key_models(tenant_id,key_id,model_id) VALUES($1,$2,'model')")
        .bind(who.tenant_id)
        .bind(who.key_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE runtime_bindings SET active=false WHERE tenant_id=$1 AND key_id=$2")
        .bind(who.tenant_id)
        .bind(who.key_id)
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        runtime_results::release(&g, &who, id).await,
        Err(Error::NotFound)
    ));
    let other = store::Identity {
        tenant_id: who.tenant_id,
        key_id: Uuid::new_v4(),
    };
    assert!(matches!(
        runtime_results::release(&g, &other, id).await,
        Err(Error::NotFound)
    ));
    let other = store::Identity {
        tenant_id: Uuid::new_v4(),
        key_id: who.key_id,
    };
    assert!(matches!(
        runtime_results::release(&g, &other, id).await,
        Err(Error::Unauthorized)
    ));
}

#[sqlx::test(migrations = "./migrations")]
async fn late_execution_fact_survives_turn_close_but_text_is_not_released(pool: PgPool) {
    let (who, turn) = setup(&pool).await;
    let request = claimed(&pool, &who, turn, "late").await;
    governance::close_runtime(&pool, &who, turn, "failed")
        .await
        .unwrap();
    governance::maintain(&pool).await.unwrap();
    // Revoking an execution model after dispatch cannot erase its late receipt.
    sqlx::query("DELETE FROM key_models WHERE tenant_id=$1 AND key_id=$2 AND model_id='model'")
        .bind(who.tenant_id)
        .bind(who.key_id)
        .execute(&pool)
        .await
        .unwrap();
    let recorded = runtime_results::record(&pool, &common::cipher(), &who, request.clone())
        .await
        .unwrap();
    assert_eq!(
        recorded,
        runtime_results::record(&pool, &common::cipher(), &who, request)
            .await
            .unwrap()
    );
    assert!(matches!(
        runtime_results::release(&gateway(&pool), &who, result_id(&recorded)).await,
        Err(Error::TurnClosed)
    ));
    let state: String =
        sqlx::query_scalar("SELECT state FROM governance_operations WHERE tenant_id=$1 AND id=$2")
            .bind(who.tenant_id)
            .bind(serde_json::from_value::<Uuid>(recorded["input_operation_id"].clone()).unwrap())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(state, "completed");
}

async fn expired_fixture(
    pool: &PgPool,
    who: &store::Identity,
    request: &Record,
    turn: Uuid,
) -> Uuid {
    governance::receipt(
        pool,
        &common::cipher(),
        who,
        request.input_operation_id,
        governance::Receipt {
            token: request.claim_token.clone(),
            outcome: "completed".into(),
            evidence_ref: request.evidence_ref.clone(),
        },
    )
    .await
    .unwrap();
    let id = Uuid::new_v4();
    let ciphertext = common::cipher()
        .encrypt(who.tenant_id, id, request.text.as_bytes())
        .unwrap();
    // Import a genuine expired fact rather than weakening the immutable trigger
    // or waiting a full production TTL during the PostgreSQL test.
    sqlx::query("INSERT INTO runtime_tool_results(tenant_id,id,key_id,turn_id,input_operation_id,model_id,tool_name,claim_hash,receipt_hash,evidence_ref,text_sha256,bytes,result_ciphertext,created_at,expires_at) SELECT tenant_id,$2,key_id,turn_id,id,model_id,tool_name,claim_hash,receipt_hash,$3,$4,$5,$6,stamp.ts,stamp.ts+interval '60 seconds' FROM governance_operations CROSS JOIN (SELECT clock_timestamp()-interval '61 seconds' AS ts) stamp WHERE tenant_id=$1 AND id=$7")
        .bind(who.tenant_id).bind(id).bind(&request.evidence_ref)
        .bind(store::hash(request.text.as_bytes())).bind(i32::try_from(request.text.len()).unwrap())
        .bind(ciphertext).bind(request.input_operation_id).execute(pool).await.unwrap();
    governance::check(pool, &common::cipher(), who, Check {
        turn_id: turn, operation_key: format!("runtime-result-output:{id}"), stage: "tool_output".into(),
        model_id: Some("model".into()), tool_name: Some("fixture_tool".into()),
        payload: json!({"result_id":id,"input_operation_id":request.input_operation_id,"text":request.text}),
    }).await.unwrap();
    id
}

#[sqlx::test(migrations = "./migrations")]
async fn expiry_clears_both_bodies_but_preserves_execution_evidence_and_idempotency(pool: PgPool) {
    let (who, turn) = setup(&pool).await;
    let request = claimed(&pool, &who, turn, "expired").await;
    let id = expired_fixture(&pool, &who, &request, turn).await;
    assert!(matches!(
        runtime_results::release(&gateway(&pool), &who, id).await,
        Err(Error::ResultUnavailable)
    ));
    assert_eq!(runtime_results::maintain(&pool).await.unwrap(), 1);
    assert_eq!(runtime_results::maintain(&pool).await.unwrap(), 0);
    let fact: (bool, Vec<u8>, String, bool) = sqlx::query_as(
        "SELECT r.result_ciphertext IS NULL,r.text_sha256,r.evidence_ref,o.payload_ciphertext IS NULL FROM runtime_tool_results r JOIN governance_operations o ON o.tenant_id=r.tenant_id AND o.key_id=r.key_id AND o.operation_key='runtime-result-output:'||r.id::text AND o.stage='tool_output' WHERE r.tenant_id=$1 AND r.id=$2")
        .bind(who.tenant_id).bind(id).fetch_one(&pool).await.unwrap();
    assert_eq!(
        fact,
        (
            true,
            store::hash(request.text.as_bytes()),
            request.evidence_ref.clone(),
            true
        )
    );
    let retry = runtime_results::record(&pool, &common::cipher(), &who, request.clone())
        .await
        .unwrap();
    assert_eq!(retry["id"], id.to_string());
    let gone: bool = sqlx::query_scalar(
        "SELECT result_ciphertext IS NULL FROM runtime_tool_results WHERE tenant_id=$1 AND id=$2",
    )
    .bind(who.tenant_id)
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(gone);
    assert!(
        sqlx::query(
            "UPDATE runtime_tool_results SET result_ciphertext=$3 WHERE tenant_id=$1 AND id=$2"
        )
        .bind(who.tenant_id)
        .bind(id)
        .bind(
            common::cipher()
                .encrypt(who.tenant_id, id, request.text.as_bytes())
                .unwrap()
        )
        .execute(&pool)
        .await
        .is_err()
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn actual_runtime_role_can_record_release_and_expire_but_cannot_rewrite_or_approve(
    pool: PgPool,
) {
    let (who, turn) = setup(&pool).await;
    let request = claimed(&pool, &who, turn, "restricted").await;
    sqlx::raw_sql(include_str!("../deploy/permissions.sql"))
        .execute(&pool)
        .await
        .unwrap();
    let runtime = sqlx::postgres::PgPoolOptions::new()
        .max_connections(3)
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
    let recorded = runtime_results::record(&runtime, &common::cipher(), &who, request)
        .await
        .unwrap();
    let id = result_id(&recorded);
    let g = gateway(&runtime);
    assert_eq!(
        runtime_results::release(&g, &who, id).await.unwrap()["state"],
        "ready"
    );
    for statement in [
        "UPDATE runtime_tool_results SET evidence_ref='forged' WHERE id=$1",
        "UPDATE runtime_tool_results SET text_sha256=decode(repeat('22',32),'hex') WHERE id=$1",
        "UPDATE runtime_tool_results SET expires_at=expires_at+interval '1 minute' WHERE id=$1",
        "DELETE FROM runtime_tool_results WHERE id=$1",
        "UPDATE runtime_tool_results SET result_ciphertext=NULL WHERE id=$1",
        "UPDATE runtime_tool_results SET result_ciphertext=decode(repeat('22',32),'hex') WHERE id=$1",
    ] {
        assert!(
            sqlx::query(statement)
                .bind(id)
                .execute(&runtime)
                .await
                .is_err(),
            "{statement}"
        );
    }
    common::guardrail(
        &pool,
        who.tenant_id,
        "restricted-risk",
        1,
        "tool_output",
        json!({"kind":"always"}),
    )
    .await;
    let waiting = runtime_results::release(&g, &who, id).await.unwrap();
    let approval = common::approval_id(&waiting);
    assert!(
        sqlx::query("UPDATE approvals SET state='approved' WHERE id=$1")
            .bind(approval)
            .execute(&runtime)
            .await
            .is_err()
    );
    assert!(sqlx::query("INSERT INTO guardrail_grants(tenant_id,turn_id,guardrail_id,version,approval_id,expires_at) VALUES($1,$2,'restricted-risk',1,$3,clock_timestamp()+interval '1 minute')")
        .bind(who.tenant_id).bind(turn).bind(approval).execute(&runtime).await.is_err());
    let operator_can_read: bool = sqlx::query_scalar(
        "SELECT has_table_privilege('brokerrouter_operator','runtime_tool_results','SELECT')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(!operator_can_read);
    let expired_request = claimed(&pool, &who, turn, "restricted-expired").await;
    let expired_id = expired_fixture(&pool, &who, &expired_request, turn).await;
    assert_eq!(runtime_results::maintain(&runtime).await.unwrap(), 1);
    let expired: bool = sqlx::query_scalar(
        "SELECT result_ciphertext IS NULL FROM runtime_tool_results WHERE tenant_id=$1 AND id=$2",
    )
    .bind(who.tenant_id)
    .bind(expired_id)
    .fetch_one(&runtime)
    .await
    .unwrap();
    assert!(expired);
    drop(g);
    runtime.close().await;
}

#[sqlx::test(migrations = "./migrations")]
async fn semantic_release_drives_one_paid_evaluation_then_replays_without_tool_execution(
    pool: PgPool,
) {
    let (who, turn) = setup(&pool).await;
    let request = claimed(&pool, &who, turn, "semantic").await;
    let recorded = runtime_results::record(&pool, &common::cipher(), &who, request)
        .await
        .unwrap();
    let id = result_id(&recorded);
    let calls = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route(
            "/v1/chat/completions",
            post(
                |State(calls): State<Arc<AtomicUsize>>, Json(_): Json<Value>| async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Json(
                        json!({"id":"fixture-judge","object":"chat.completion","created":1,
                "model":"fixture-model","choices":[{"index":0,"message":{"role":"assistant",
                    "content":"{\"decision\":\"risk\"}"},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":3,"completion_tokens":4,"total_tokens":7}}),
                    )
                },
            ),
        )
        .with_state(calls.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    store::add_endpoint(
        &pool,
        &store::Endpoint {
            id: Uuid::new_v4(),
            tenant_id: who.tenant_id,
            model_id: "model".into(),
            name: "fixture-judge".into(),
            upstream_model: "fixture-model".into(),
            base_url: url,
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
    common::guardrail(
        &pool,
        who.tenant_id,
        "semantic-output",
        1,
        "tool_output",
        semantic_rule(),
    )
    .await;
    let g = gateway(&pool);
    let waiting = runtime_results::release(&g, &who, id).await.unwrap();
    assert_eq!(waiting["state"], "waiting_approval");
    assert!(waiting.get("text").is_none());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        runtime_results::release(&g, &who, id).await.unwrap()["approval"]["id"],
        waiting["approval"]["id"]
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    common::decide(
        &pool,
        "alice",
        common::approval_id(&waiting),
        common::approve(&["semantic-output"]),
    )
    .await
    .unwrap();
    assert_eq!(
        runtime_results::release(&g, &who, id).await.unwrap()["state"],
        "ready"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let facts: (i64, i64, i64) = sqlx::query_as("SELECT (SELECT count(*) FROM requests WHERE status='succeeded'),(SELECT sum(final_micros)::bigint FROM requests),(SELECT count(*) FROM governance_events WHERE event_type='operation.claimed')")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(facts, (1, 7, 1));
    server.abort();
}
