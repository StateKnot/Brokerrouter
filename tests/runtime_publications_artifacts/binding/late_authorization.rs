//! Late whole-set authority and purpose regressions. This is a child of
//! binding.rs so its actual PostgreSQL/gateway/capture helpers remain private.
//! The metadata delay preserves every original source/file/grant byte. A
//! nontransactional sequence proves the last gate and delayed read happened
//! even when the fixed release transaction rolls back.
use super::*;

async fn short_text_grant(pool: &PgPool) {
    sqlx::raw_sql(
        r#"
        CREATE FUNCTION fixture_short_text_grant() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN
            IF NEW.guardrail_id='late-text-authority' THEN
                NEW.expires_at=LEAST(NEW.expires_at,clock_timestamp()+interval '3 seconds');
            END IF;
            RETURN NEW;
        END $$;
        CREATE TRIGGER fixture_short_text_grant BEFORE INSERT ON guardrail_grants
            FOR EACH ROW EXECUTE FUNCTION fixture_short_text_grant();
    "#,
    )
    .execute(pool)
    .await
    .unwrap();
}

async fn approve_text(pool: &PgPool, f: &Fixture, pending: &Value) {
    let human = common::proof(pool, &f.subject, "/review", &json!({})).await;
    let reviewed = approval_workbench::review(
        pool,
        &common::cipher(),
        &human,
        approval_workbench::Review {
            approval_id: common::approval_id(pending),
        },
    )
    .await
    .unwrap();
    assert_eq!(reviewed["can_continue"], true);
    assert!(reviewed["payload_text"].as_str().is_some());
    approval_workbench::decide(
        pool,
        &common::cipher(),
        &human,
        approval_workbench::Decide {
            approval_id: common::approval_id(pending),
            review_id: serde_json::from_value(reviewed["review_id"].clone()).unwrap(),
            acknowledged_scope: true,
            decision: serde_json::from_value(common::approve(&["late-text-authority"])).unwrap(),
        },
    )
    .await
    .unwrap();
}

async fn delay_metadata_after_last_text_gate(pool: &PgPool, output: Uuid) {
    sqlx::raw_sql(r#"
        CREATE TABLE fixture_late_text_target(operation_id uuid NOT NULL);
        CREATE SEQUENCE fixture_late_text_gates;
        CREATE SEQUENCE fixture_late_metadata_reads;
        CREATE FUNCTION fixture_arm_late_metadata() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN
            IF NEW.event_type='guardrails.evaluated'
               AND NEW.data->>'operation_id'=(SELECT operation_id::text FROM fixture_late_text_target) THEN
                PERFORM nextval('fixture_late_text_gates');
            END IF;
            RETURN NEW;
        END $$;
        CREATE TRIGGER fixture_arm_late_metadata BEFORE INSERT ON governance_events
            FOR EACH ROW EXECUTE FUNCTION fixture_arm_late_metadata();
        CREATE FUNCTION fixture_wait_after_text_gate(scope_tenant uuid) RETURNS boolean
        LANGUAGE plpgsql VOLATILE AS $$
        DECLARE armed boolean; waited boolean; until_at timestamptz;
        BEGIN
            SELECT is_called AND last_value>=2 INTO armed FROM fixture_late_text_gates;
            SELECT is_called INTO waited FROM fixture_late_metadata_reads;
            IF armed AND NOT waited THEN
                PERFORM nextval('fixture_late_metadata_reads');
                SELECT min(expires_at) INTO until_at FROM guardrail_grants
                    WHERE tenant_id=scope_tenant AND guardrail_id='late-text-authority';
                IF until_at IS NULL THEN RAISE EXCEPTION 'fixture requires a real text grant'; END IF;
                PERFORM pg_sleep(GREATEST(0,extract(epoch FROM until_at-clock_timestamp()))+0.05);
            END IF;
            RETURN true;
        END $$;
        CREATE OR REPLACE VIEW runtime_artifact_asset_facts WITH (security_barrier=true) AS
        SELECT tenant_id,id,key_id,kind,state,source_job_id,source_operation_id,
            sha256,byte_length,chunk_count,expires_at
        FROM assets WHERE kind='runtime_output' AND fixture_wait_after_text_gate(tenant_id);
    "#).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO fixture_late_text_target VALUES($1)")
        .bind(output)
        .execute(pool)
        .await
        .unwrap();
}

async fn require_late_authority_denied(
    pool: &PgPool,
    f: &Fixture,
    released: brokerrouter::error::Result<Value>,
) {
    let gates: (i64, bool) =
        sqlx::query_as("SELECT last_value,is_called FROM fixture_late_text_gates")
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(
        gates,
        (2, true),
        "the exact output's final gate was not reached"
    );
    assert!(
        sqlx::query_scalar::<_, bool>("SELECT is_called FROM fixture_late_metadata_reads")
            .fetch_one(pool)
            .await
            .unwrap(),
        "the last source metadata read did not wait"
    );
    assert!(sqlx::query_scalar::<_, bool>("SELECT expires_at<=clock_timestamp() FROM guardrail_grants WHERE tenant_id=$1 AND guardrail_id='late-text-authority'")
        .bind(f.who.tenant_id).fetch_one(pool).await.unwrap());
    assert!(
        matches!(
            released,
            Err(Error::PolicyChanged | Error::ResultUnavailable)
        ),
        "plaintext crossed the real text grant deadline during final file authorization: {released:?}"
    );
    assert_eq!(f.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM guardrail_evaluations")
            .fetch_one(pool)
            .await
            .unwrap(),
        0
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn original_tool_release_cannot_outlive_text_grant_during_last_file_authorization(
    pool: PgPool,
) {
    let f = setup(&pool).await;
    let p = pair(&pool, &f, true).await;
    let d = scan_pair(&pool, &f, &p).await;
    let saved = runtime_results::record(&pool, &common::cipher(), &f.who, p.text())
        .await
        .unwrap();
    approve_file(&pool, &f, &p, &d, 0).await;
    approve_file(&pool, &f, &p, &d, 1).await;
    common::guardrail(
        &pool,
        f.who.tenant_id,
        "late-text-authority",
        1,
        "tool_output",
        json!({"kind":"always"}),
    )
    .await;
    let pending = runtime_results::release(&f.gateway, &f.who, id(&saved))
        .await
        .unwrap();
    assert_eq!(pending["state"], "waiting_approval");
    assert!(pending.get("text").is_none());
    short_text_grant(&pool).await;
    approve_text(&pool, &f, &pending).await;
    delay_metadata_after_last_text_gate(&pool, id(&pending)).await;
    let released = tokio::time::timeout(
        Duration::from_secs(5),
        runtime_results::release(&f.gateway, &f.who, id(&saved)),
    )
    .await
    .expect("late-authority probe must complete without a timeout");
    require_late_authority_denied(&pool, &f, released).await;
}

#[sqlx::test(migrations = "./migrations")]
async fn final_release_cannot_outlive_post_grant_during_last_file_authorization(pool: PgPool) {
    let f = setup(&pool).await;
    let p = pair(&pool, &f, true).await;
    let d = scan_pair(&pool, &f, &p).await;
    let saved = runtime_results::record(&pool, &common::cipher(), &f.who, p.text())
        .await
        .unwrap();
    approve_file(&pool, &f, &p, &d, 0).await;
    approve_file(&pool, &f, &p, &d, 1).await;
    assert_eq!(
        runtime_results::release(&f.gateway, &f.who, id(&saved))
            .await
            .unwrap()["state"],
        "ready"
    );
    let final_text = runtime_publications::record(&pool, &common::cipher(), &f.who, f.record())
        .await
        .unwrap();
    common::guardrail(
        &pool,
        f.who.tenant_id,
        "late-text-authority",
        1,
        "post",
        json!({"kind":"always"}),
    )
    .await;
    let pending = runtime_publications::release(&f.gateway, &f.who, id(&final_text))
        .await
        .unwrap();
    assert_eq!(pending["state"], "waiting_approval");
    assert!(pending.get("text").is_none());
    short_text_grant(&pool).await;
    approve_text(&pool, &f, &pending).await;
    delay_metadata_after_last_text_gate(&pool, id(&pending)).await;
    let released = tokio::time::timeout(
        Duration::from_secs(5),
        runtime_publications::release(&f.gateway, &f.who, id(&final_text)),
    )
    .await
    .expect("late-authority probe must complete without a timeout");
    require_late_authority_denied(&pool, &f, released).await;
}

async fn scan_whole_turn(
    pool: &PgPool,
    f: &Fixture,
    runtime: &RuntimeArtifacts,
    files: usize,
) -> Delivery {
    let profile = scan::Profile {
        tenant_id: f.who.tenant_id,
        id: Uuid::new_v4(),
        model_id: "model".into(),
        version: 1,
        engine_revision: "55".repeat(32),
    };
    scan::publish(pool, profile.clone()).await.unwrap();
    let (_, token) = scan::create_token(
        pool,
        f.who.tenant_id,
        profile.id,
        "contract fixture",
        Utc::now() + chrono::Duration::hours(1),
    )
    .await
    .unwrap();
    let scanner = scan::authenticate(pool, &token).await.unwrap();
    let scans = Scans::new(runtime.assets.clone());
    for _ in 0..files {
        let a = scans
            .claim(&scanner, Uuid::new_v4())
            .await
            .unwrap()
            .unwrap();
        let stamp: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
            .fetch_one(pool)
            .await
            .unwrap();
        let report: Report = serde_json::from_value(json!({"sha256":a["sha256"],"byte_length":a["byte_length"],
            "profile_version":a["profile_version"],"engine_revision":a["engine_revision"],"technical":"clean",
            "content":"risk","media_type":"video/mp4","reason_codes":["fixture_risk"],
            "evidence_sha256":"66".repeat(32),"scanned_at":stamp})).unwrap();
        scans
            .report(
                &scanner,
                serde_json::from_value(a["attempt_id"].clone()).unwrap(),
                report,
            )
            .await
            .unwrap();
    }
    common::guardrail(
        pool,
        f.who.tenant_id,
        "native-content",
        1,
        "artifact",
        json!({"kind":"asset_scan","profile_id":profile.id}),
    )
    .await;
    Delivery::new(runtime.assets.clone())
}

#[sqlx::test(migrations = "./migrations")]
async fn final_whole_turn_file_gate_precedes_every_original_tool_classifier(pool: PgPool) {
    let f = setup(&pool).await;
    let first = pair(&pool, &f, true).await;
    let second = extra_batch(
        &pool,
        &f,
        &first.runtime,
        "independent-second-tool",
        Some("model"),
    )
    .await;
    let delivery = scan_whole_turn(
        &pool,
        &f,
        &first.runtime,
        first.assets.len() + second.assets.len(),
    )
    .await;
    runtime_results::record(&pool, &common::cipher(), &f.who, first.text())
        .await
        .unwrap();
    runtime_results::record(&pool, &common::cipher(), &f.who, second.text())
        .await
        .unwrap();
    // derive_turn orders original inputs, not record creation time. Make the
    // first batch in that real order wholly authorized and leave the other held.
    let (approved, held) = if first.input < second.input {
        (&first, &second)
    } else {
        (&second, &first)
    };
    for index in 0..approved.assets.len() {
        approve_file(&pool, &f, approved, &delivery, index).await;
    }
    store::add_endpoint(
        &pool,
        &store::Endpoint {
            id: Uuid::new_v4(),
            tenant_id: f.who.tenant_id,
            model_id: "model".into(),
            name: "local-paid-original-judge".into(),
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
    common::guardrail(&pool, f.who.tenant_id, "whole-turn-original-judge", 1, "tool_output",
        json!({"kind":"model_judge","model":"model","instructions":"Check original result.",
            "input_format":"text_json","max_input_bytes":65536,"max_output_tokens":32,"maximum_cny":"0.1"})).await;
    let publication = runtime_publications::record(&pool, &common::cipher(), &f.who, f.record())
        .await
        .unwrap();
    assert_eq!(
        publication["artifact_set"]["files"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    let pending = runtime_publications::release(&f.gateway, &f.who, id(&publication))
        .await
        .unwrap();
    assert_eq!(
        f.calls.load(Ordering::SeqCst),
        1,
        "one batch's classifier ran before the whole final file set was authorized"
    );
    assert_eq!(pending["state"], "waiting_artifacts");
    assert!(pending.get("text").is_none());
    let counts: (i64,i64) = sqlx::query_as("SELECT (SELECT count(*) FROM guardrail_evaluations),(SELECT count(*) FROM requests WHERE guardrail_evaluation_id IS NOT NULL)")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(counts, (0, 0));
    for index in 0..held.assets.len() {
        approve_file(&pool, &f, held, &delivery, index).await;
    }
    let text_pending = runtime_publications::release(&f.gateway, &f.who, id(&publication))
        .await
        .unwrap();
    assert_eq!(text_pending["state"], "waiting_tool_outputs");
    assert!(text_pending.get("text").is_none());
    assert_eq!(
        f.calls.load(Ordering::SeqCst),
        2,
        "the actual classifier must run once the whole file set is ready"
    );
    let costs: (i64,i64) = sqlx::query_as("SELECT count(*),COALESCE(sum(final_micros),0)::bigint FROM requests WHERE guardrail_evaluation_id IS NOT NULL")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(costs, (1, 5));
}

#[sqlx::test(migrations = "./migrations")]
async fn operator_can_read_current_whole_set_metadata_without_runtime_body_authority(pool: PgPool) {
    let f = setup(&pool).await;
    let p = pair(&pool, &f, true).await;
    let stored = runtime_results::record(&pool, &common::cipher(), &f.who, p.text())
        .await
        .unwrap();
    apply_permissions(&pool).await;
    let operator = PgPoolOptions::new()
        .max_connections(1)
        .after_connect(|connection, _| {
            Box::pin(async move {
                sqlx::query("SET SESSION AUTHORIZATION brokerrouter_operator")
                    .execute(connection)
                    .await?;
                Ok(())
            })
        })
        .connect_with(pool.connect_options().as_ref().clone())
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT current_user")
            .fetch_one(&operator)
            .await
            .unwrap(),
        "brokerrouter_operator"
    );
    let set: Value = sqlx::query_scalar("SELECT files FROM runtime_artifact_source_facts WHERE tenant_id=$1 AND input_operation_id=$2")
        .bind(f.who.tenant_id).bind(p.input).fetch_one(&operator).await.expect("operator's granted source metadata view must be executable");
    assert_eq!(set.as_array().unwrap().len(), 2);
    let available: bool = sqlx::query_scalar(
        "SELECT available FROM runtime_tool_result_review_facts WHERE tenant_id=$1 AND id=$2",
    )
    .bind(f.who.tenant_id)
    .bind(id(&stored))
    .fetch_one(&operator)
    .await
    .expect("operator's granted result metadata view must be executable");
    assert!(available);
    for query in [
        "SELECT result_ciphertext FROM runtime_tool_results LIMIT 1",
        "SELECT result_ciphertext FROM runtime_final_publications LIMIT 1",
        "SELECT result_ciphertext FROM runtime_tool_result_review_facts LIMIT 1",
    ] {
        let error = sqlx::query(query).fetch_all(&operator).await.unwrap_err();
        let code = error
            .as_database_error()
            .and_then(|error| error.code())
            .unwrap()
            .into_owned();
        assert!(
            matches!(code.as_str(), "42501" | "42703"),
            "wrong negative error for {query}: {error}"
        );
    }
    operator.close().await;
    assert_eq!(f.calls.load(Ordering::SeqCst), 1);
}
