use super::*;

async fn short_native_grants(pool: &PgPool) {
    // Mint a valid shorter original grant. Never UPDATE/extend immutable facts.
    sqlx::raw_sql("CREATE FUNCTION fixture_short_whole_set_grant() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN NEW.expires_at=LEAST(NEW.expires_at,clock_timestamp()+interval '3 seconds'); RETURN NEW; END $$; CREATE TRIGGER fixture_short_whole_set_grant BEFORE INSERT ON runtime_artifact_grants FOR EACH ROW EXECUTE FUNCTION fixture_short_whole_set_grant();")
        .execute(pool).await.unwrap();
}
async fn delay_text_review_to_file_expiry(pool: &PgPool) {
    sqlx::raw_sql("CREATE SEQUENCE whole_text_review_seen; CREATE FUNCTION fixture_delay_whole_text_review() RETURNS trigger LANGUAGE plpgsql AS $$ DECLARE until_at timestamptz; BEGIN IF NEW.action='approval.reviewed' AND NEW.details->>'review_kind' IS NULL THEN PERFORM nextval('whole_text_review_seen'); SELECT min(expires_at) INTO until_at FROM runtime_artifact_grants WHERE tenant_id=NEW.tenant_id; PERFORM pg_sleep(GREATEST(0,extract(epoch FROM until_at-clock_timestamp()))+0.05); END IF; RETURN NEW; END $$; CREATE TRIGGER fixture_delay_whole_text_review BEFORE INSERT ON audit_events FOR EACH ROW EXECUTE FUNCTION fixture_delay_whole_text_review();")
        .execute(pool).await.unwrap();
}
async fn review_text(
    pool: &PgPool,
    f: &Fixture,
    pending: &Value,
) -> brokerrouter::error::Result<Value> {
    let proof = common::proof(pool, &f.subject, "/review", &json!({})).await;
    approval_workbench::review(
        pool,
        &common::cipher(),
        &proof,
        approval_workbench::Review {
            approval_id: common::approval_id(pending),
        },
    )
    .await
}
async fn require_failed_audit_review(
    pool: &PgPool,
    f: &Fixture,
    result: brokerrouter::error::Result<Value>,
) {
    assert!(
        matches!(
            result,
            Err(Error::ResultUnavailable | Error::ActionRejected | Error::InProgress)
        ),
        "Human text was delivered after original file authority expired: {result:?}"
    );
    assert!(
        sqlx::query_scalar::<_, bool>("SELECT is_called FROM whole_text_review_seen")
            .fetch_one(pool)
            .await
            .unwrap()
    );
    assert!(
        sqlx::query_scalar::<_, bool>(
            "SELECT min(expires_at)<=clock_timestamp() FROM runtime_artifact_grants"
        )
        .fetch_one(pool)
        .await
        .unwrap()
    );
    assert_eq!(sqlx::query_scalar::<_,i64>("SELECT count(*) FROM audit_events WHERE action='approval.reviewed' AND details->>'review_kind' IS NULL").fetch_one(pool).await.unwrap(),0);
    assert_eq!(sqlx::query_scalar::<_,i64>("SELECT count(*) FROM approval_reviews WHERE can_continue AND id NOT IN (SELECT review_id FROM media_review_completions)").fetch_one(pool).await.unwrap(),0);
    assert_eq!(f.calls.load(Ordering::SeqCst), 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn original_tool_text_review_crossing_file_grant_deadline_returns_no_body(pool: PgPool) {
    let f = setup(&pool).await;
    let p = pair(&pool, &f, true).await;
    let d = scan_pair(&pool, &f, &p).await;
    let saved = runtime_results::record(&pool, &common::cipher(), &f.who, p.text())
        .await
        .unwrap();
    short_native_grants(&pool).await;
    approve_file(&pool, &f, &p, &d, 0).await;
    approve_file(&pool, &f, &p, &d, 1).await;
    common::guardrail(
        &pool,
        f.who.tenant_id,
        "text-review",
        1,
        "tool_output",
        json!({"kind":"always"}),
    )
    .await;
    let pending = runtime_results::release(&f.gateway, &f.who, id(&saved))
        .await
        .unwrap();
    assert_eq!(pending["state"], "waiting_approval");
    delay_text_review_to_file_expiry(&pool).await;
    require_failed_audit_review(&pool, &f, review_text(&pool, &f, &pending).await).await;
}

#[sqlx::test(migrations = "./migrations")]
async fn final_text_review_crossing_file_grant_deadline_returns_no_body(pool: PgPool) {
    let f = setup(&pool).await;
    let p = pair(&pool, &f, true).await;
    let d = scan_pair(&pool, &f, &p).await;
    let saved = runtime_results::record(&pool, &common::cipher(), &f.who, p.text())
        .await
        .unwrap();
    short_native_grants(&pool).await;
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
        "text-review",
        1,
        "post",
        json!({"kind":"always"}),
    )
    .await;
    let pending = runtime_publications::release(&f.gateway, &f.who, id(&final_text))
        .await
        .unwrap();
    assert_eq!(pending["state"], "waiting_approval");
    delay_text_review_to_file_expiry(&pool).await;
    require_failed_audit_review(&pool, &f, review_text(&pool, &f, &pending).await).await;
}

#[sqlx::test(migrations = "./migrations")]
async fn human_review_of_original_tool_policy_never_registers_or_drives_paid_judge(pool: PgPool) {
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
        "text-review",
        1,
        "post",
        json!({"kind":"always"}),
    )
    .await;
    let pending = runtime_publications::release(&f.gateway, &f.who, id(&final_text))
        .await
        .unwrap();
    let reviewed = review_text(&pool, &f, &pending).await.unwrap();
    common::guardrail(&pool,f.who.tenant_id,"new-original-judge",1,"tool_output",json!({"kind":"model_judge","model":"model","instructions":"Check original result.","input_format":"text_json","max_input_bytes":65536,"max_output_tokens":32,"maximum_cny":"0.1"})).await;
    assert!(matches!(
        review_text(&pool, &f, &pending).await,
        Err(Error::InProgress)
    ));
    let proof = common::proof(&pool, &f.subject, "/decide", &json!({})).await;
    assert!(matches!(
        approval_workbench::decide(
            &pool,
            &common::cipher(),
            &proof,
            approval_workbench::Decide {
                approval_id: common::approval_id(&pending),
                review_id: serde_json::from_value(reviewed["review_id"].clone()).unwrap(),
                acknowledged_scope: true,
                decision: serde_json::from_value(common::approve(&["text-review"])).unwrap(),
            }
        )
        .await,
        Err(Error::InProgress)
    ));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM guardrail_evaluations")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM guardrail_grants WHERE guardrail_id='text-review'"
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        0
    );
    assert_eq!(f.calls.load(Ordering::SeqCst), 1);
}
