//! Native file release at public service/review seams. Reports are synthetic
//! contract fixtures, not certification of the detector or MP4 format quality.
use super::*;
use brokerrouter::{
    approval_workbench, human, media_review as review,
    output_delivery::Delivery as ReviewDelivery,
    output_scans::{Content, Technical},
    runtime_artifact_delivery::Delivery,
    runtime_artifact_delivery_api,
};

async fn prepared(pool: &PgPool) -> (Fixture, Uuid, scan::Profile, Scanner, Delivery) {
    let f = setup(pool).await;
    let (_, _, asset, _) = runtime_artifact_scan_tests::native(pool, &f, "native-release").await;
    let (profile, who, _) = scanner(pool, &f, 1).await;
    let scans = Scans::new(f.o.assets.clone());
    let assignment = scans.claim(&who, Uuid::new_v4()).await.unwrap().unwrap();
    scans
        .report(
            &who,
            attempt_id(&assignment),
            scan_report(pool, &assignment).await,
        )
        .await
        .unwrap();
    common::guardrail(
        pool,
        f.who.tenant_id,
        "native-content",
        1,
        "artifact",
        json!({"kind":"asset_scan","profile_id":profile.id}),
    )
    .await;
    let delivery = Delivery::new(f.o.assets.clone());
    (f, asset, profile, who, delivery)
}
async fn proof(pool: &PgPool, f: &Fixture) -> human::Human {
    common::proof(
        pool,
        &format!("native-scan-{}", f.who.tenant_id),
        "/review",
        &json!({}),
    )
    .await
}
fn decide(approval: Uuid, review: Uuid) -> approval_workbench::Decide {
    approval_workbench::Decide {
        approval_id: approval,
        review_id: review,
        acknowledged_scope: true,
        decision: serde_json::from_value(common::approve(&["native-content"])).unwrap(),
    }
}
async fn all_bytes_review(
    pool: &PgPool,
    f: &Fixture,
    approval: Uuid,
) -> (human::Human, Uuid, Value) {
    let p = proof(pool, f).await;
    let session = Uuid::new_v4();
    let d = ReviewDelivery::new(f.o.assets.clone());
    let manifest = review::start(
        &d,
        &p,
        review::Start {
            approval_id: approval,
            session_id: session,
        },
    )
    .await
    .unwrap();
    let mut bytes = Vec::new();
    for ordinal in 0..manifest["output"]["chunk_count"].as_i64().unwrap() as i32 {
        bytes.extend_from_slice(
            &review::chunk(
                &d,
                &p,
                review::Chunk {
                    session_id: session,
                    ordinal,
                },
            )
            .await
            .unwrap(),
        );
    }
    assert_eq!(bytes.as_slice(), f.r.data.as_ref());
    let complete = review::complete(
        &d,
        &p,
        review::Complete {
            session_id: session,
            sha256: manifest["output"]["sha256"].as_str().unwrap().into(),
            reviewed: true,
        },
    )
    .await
    .unwrap();
    (p, session, complete)
}
async fn approve(pool: &PgPool, f: &Fixture, d: &Delivery, asset: Uuid) -> Value {
    let waiting = d.release(&f.who, asset).await.unwrap();
    let a = common::approval_id(&waiting);
    let (p, session, _) = all_bytes_review(pool, f, a).await;
    approval_workbench::decide(pool, &common::cipher(), &p, decide(a, session))
        .await
        .unwrap();
    waiting
}

#[sqlx::test(migrations = "./migrations")]
async fn native_release_requires_exact_complete_byte_review_and_recovers_without_tool_execution(
    pool: PgPool,
) {
    let (f, asset, _, _, d) = prepared(&pool).await;
    let (left, right) = tokio::join!(d.release(&f.who, asset), d.release(&f.who, asset));
    let waiting = left.unwrap();
    assert_eq!(waiting, right.unwrap());
    assert_eq!(waiting["state"], "waiting_approval");
    assert_eq!(
        waiting["approval"]["scope"],
        "current_artifact_in_original_turn"
    );
    assert_eq!(
        waiting["approval"]["summary"]["output"]["asset_id"],
        asset.to_string()
    );
    assert!(
        waiting["approval"]["summary"]["output"]
            .get("job_id")
            .is_none()
    );
    assert!(d.manifest(&f.who, asset).await.is_err());
    assert!(d.chunk(&f.who, asset, 0).await.is_err());
    let a = common::approval_id(&waiting);
    let p = proof(&pool, &f).await;
    let summary = approval_workbench::review(
        &pool,
        &common::cipher(),
        &p,
        approval_workbench::Review { approval_id: a },
    )
    .await
    .unwrap();
    assert_eq!(summary["can_continue"], false);
    assert!(
        approval_workbench::decide(
            &pool,
            &common::cipher(),
            &p,
            decide(
                a,
                serde_json::from_value(summary["review_id"].clone()).unwrap()
            )
        )
        .await
        .is_err()
    );
    let subject = format!("native-scan-{}", f.who.tenant_id);
    assert!(
        common::decide(&pool, &subject, a, common::approve(&["native-content"]))
            .await
            .is_err()
    );
    let reviewer = ReviewDelivery::new(f.o.assets.clone());
    let session = Uuid::new_v4();
    let m = review::start(
        &reviewer,
        &p,
        review::Start {
            approval_id: a,
            session_id: session,
        },
    )
    .await
    .unwrap();
    review::chunk(
        &reviewer,
        &p,
        review::Chunk {
            session_id: session,
            ordinal: 0,
        },
    )
    .await
    .unwrap();
    assert!(
        review::complete(
            &reviewer,
            &p,
            review::Complete {
                session_id: session,
                sha256: m["output"]["sha256"].as_str().unwrap().into(),
                reviewed: true
            }
        )
        .await
        .is_err()
    );
    review::chunk(
        &reviewer,
        &p,
        review::Chunk {
            session_id: session,
            ordinal: 1,
        },
    )
    .await
    .unwrap();
    let complete = review::complete(
        &reviewer,
        &p,
        review::Complete {
            session_id: session,
            sha256: m["output"]["sha256"].as_str().unwrap().into(),
            reviewed: true,
        },
    )
    .await
    .unwrap();
    assert_eq!(complete["stage"], "artifact");
    assert!(complete["payload_text"].is_null());
    assert_eq!(complete["media"]["kind"], "isolated_mp4");
    assert_eq!(complete["media"]["source"], "runtime_artifact");
    assert_eq!(
        complete["media"]["output"],
        waiting["approval"]["summary"]["output"]
    );
    let decision = approval_workbench::decide(&pool, &common::cipher(), &p, decide(a, session))
        .await
        .unwrap();
    assert_eq!(decision["state"], "approved");
    assert_eq!(
        decision,
        approval_workbench::decide(&pool, &common::cipher(), &p, decide(a, session))
            .await
            .unwrap()
    );
    let restarted = Delivery::new(f.o.assets.clone());
    assert_eq!(
        restarted.release(&f.who, asset).await.unwrap()["state"],
        "ready"
    );
    let m = restarted.manifest(&f.who, asset).await.unwrap();
    let mut actual = Vec::new();
    for ordinal in 0..m["output"]["chunk_count"].as_i64().unwrap() as i32 {
        actual.extend_from_slice(&restarted.chunk(&f.who, asset, ordinal).await.unwrap());
    }
    assert_eq!(actual.as_slice(), f.r.data.as_ref());
    let facts: (i64, i64, i64, i64) = sqlx::query_as("SELECT (SELECT count(*) FROM runtime_artifact_grants),(SELECT count(*) FROM guardrail_grants),(SELECT count(*) FROM media_jobs),(SELECT count(*) FROM governance_operations WHERE stage='tool_input' AND state='completed')").fetch_one(&pool).await.unwrap();
    assert_eq!(facts, (1, 0, 0, 1));
    assert_eq!(f.r.posts.load(Ordering::SeqCst), 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn native_grant_is_for_one_file_and_exact_current_full_policy_not_whole_turn(pool: PgPool) {
    let (f, first, profile, who, d) = prepared(&pool).await;
    let before = approve(&pool, &f, &d, first).await;
    d.chunk(&f.who, first, 0).await.unwrap();
    let (_, _, second, _) = runtime_artifact_scan_tests::native(&pool, &f, "second-release").await;
    let scans = Scans::new(f.o.assets.clone());
    let v = scans.claim(&who, Uuid::new_v4()).await.unwrap().unwrap();
    scans
        .report(&who, attempt_id(&v), scan_report(&pool, &v).await)
        .await
        .unwrap();
    assert_eq!(
        d.release(&f.who, second).await.unwrap()["state"],
        "waiting_approval"
    );
    assert!(d.chunk(&f.who, second, 0).await.is_err());
    common::guardrail(
        &pool,
        f.who.tenant_id,
        "native-content",
        2,
        "artifact",
        json!({"kind":"asset_scan","profile_id":profile.id}),
    )
    .await;
    assert!(d.manifest(&f.who, first).await.is_err());
    let upgraded = d.release(&f.who, first).await.unwrap();
    assert_eq!(upgraded["state"], "waiting_approval");
    assert_eq!(upgraded["operation_id"], before["operation_id"]);
    assert_ne!(common::approval_id(&upgraded), common::approval_id(&before));
    let native_deadlines: bool = sqlx::query_scalar("SELECT bool_and(g.expires_at<=LEAST(t.expires_at,s.expires_at,a.expires_at)) FROM runtime_artifact_grants g JOIN runtime_artifact_deliveries d ON d.tenant_id=g.tenant_id AND d.operation_id=g.operation_id JOIN governance_operations o ON o.tenant_id=d.tenant_id AND o.id=d.operation_id JOIN governance_turns t ON t.tenant_id=o.tenant_id AND t.id=o.turn_id JOIN output_scans s ON s.tenant_id=d.tenant_id AND s.id=d.scan_id JOIN assets a ON a.tenant_id=d.tenant_id AND a.id=d.asset_id").fetch_one(&pool).await.unwrap();
    assert!(native_deadlines);
    assert_eq!(f.r.posts.load(Ordering::SeqCst), 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn native_rejection_survives_new_scan_profile_and_does_not_require_risky_byte_review(
    pool: PgPool,
) {
    let (f, asset, _, _, d) = prepared(&pool).await;
    let waiting = d.release(&f.who, asset).await.unwrap();
    let a = common::approval_id(&waiting);
    let p = proof(&pool, &f).await;
    let summary = approval_workbench::review(
        &pool,
        &common::cipher(),
        &p,
        approval_workbench::Review { approval_id: a },
    )
    .await
    .unwrap();
    let mut q = decide(
        a,
        serde_json::from_value(summary["review_id"].clone()).unwrap(),
    );
    q.decision = serde_json::from_value(
        json!({"decision":"reject","approved_guardrail_ids":[],"feedback":null}),
    )
    .unwrap();
    assert_eq!(
        approval_workbench::decide(&pool, &common::cipher(), &p, q)
            .await
            .unwrap()["state"],
        "rejected"
    );
    let (profile, who, _) = scanner(&pool, &f, 2).await;
    let scans = Scans::new(f.o.assets.clone());
    let v = scans.claim(&who, Uuid::new_v4()).await.unwrap().unwrap();
    scans
        .report(&who, attempt_id(&v), scan_report(&pool, &v).await)
        .await
        .unwrap();
    common::guardrail(
        &pool,
        f.who.tenant_id,
        "native-content",
        2,
        "artifact",
        json!({"kind":"asset_scan","profile_id":profile.id}),
    )
    .await;
    assert!(matches!(
        d.release(&f.who, asset).await,
        Err(Error::ActionRejected)
    ));
    assert!(d.chunk(&f.who, asset, 0).await.is_err());
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM runtime_artifact_grants")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM media_review_sessions")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    assert_eq!(f.r.posts.load(Ordering::SeqCst), 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn native_technical_unsafe_unsupported_error_cannot_be_waived_by_prior_file_approval(
    pool: PgPool,
) {
    for technical in [Technical::Unsafe, Technical::Unsupported, Technical::Error] {
        let (f, asset, _, _, d) = prepared(&pool).await;
        approve(&pool, &f, &d, asset).await;
        let (profile, who, _) = scanner(&pool, &f, 2).await;
        let scans = Scans::new(f.o.assets.clone());
        let v = scans.claim(&who, Uuid::new_v4()).await.unwrap().unwrap();
        let mut report = scan_report(&pool, &v).await;
        report.technical = technical;
        report.content = Content::Unknown;
        report.media_type = None;
        scans.report(&who, attempt_id(&v), report).await.unwrap();
        common::guardrail(
            &pool,
            f.who.tenant_id,
            "native-content",
            2,
            "artifact",
            json!({"kind":"asset_scan","profile_id":profile.id}),
        )
        .await;
        assert!(matches!(
            d.release(&f.who, asset).await,
            Err(Error::OutputUnsafe)
        ));
        assert!(matches!(
            d.chunk(&f.who, asset, 0).await,
            Err(Error::OutputUnsafe)
        ));
        assert_eq!(f.r.posts.load(Ordering::SeqCst), 0);
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn native_grant_expiring_in_final_audit_returns_no_bytes_or_committed_read_audit(
    pool: PgPool,
) {
    let (f, asset, _, _, d) = prepared(&pool).await;
    // Fault injection mints a valid shorter immutable grant. No existing expiry
    // fact is rewritten and the actual final boundary uses the PG clock.
    sqlx::raw_sql("CREATE FUNCTION short_native_grant() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN NEW.expires_at=clock_timestamp()+interval '4 seconds'; RETURN NEW; END; $$; CREATE TRIGGER short_native_grant BEFORE INSERT ON runtime_artifact_grants FOR EACH ROW EXECUTE FUNCTION short_native_grant();").execute(&pool).await.unwrap();
    approve(&pool, &f, &d, asset).await;
    sqlx::raw_sql("CREATE SEQUENCE native_read_audit_seen; CREATE FUNCTION delay_native_read_audit() RETURNS trigger LANGUAGE plpgsql AS $$ DECLARE until_at timestamptz; BEGIN IF NEW.action='runtime_artifact.chunk_served' THEN PERFORM nextval('native_read_audit_seen'); SELECT min(g.expires_at) INTO until_at FROM runtime_artifact_grants g JOIN runtime_artifact_deliveries d ON d.tenant_id=g.tenant_id AND d.operation_id=g.operation_id WHERE d.asset_id=NEW.object_id::uuid; PERFORM pg_sleep(GREATEST(0,extract(epoch FROM until_at-clock_timestamp()))+0.05); END IF; RETURN NEW; END; $$; CREATE TRIGGER delay_native_read_audit BEFORE INSERT ON audit_events FOR EACH ROW EXECUTE FUNCTION delay_native_read_audit();").execute(&pool).await.unwrap();
    assert!(matches!(
        d.chunk(&f.who, asset, 0).await,
        Err(Error::PolicyChanged)
    ));
    assert!(
        sqlx::query_scalar::<_, bool>("SELECT is_called FROM native_read_audit_seen")
            .fetch_one(&pool)
            .await
            .unwrap()
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM audit_events WHERE action='runtime_artifact.chunk_served'"
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        0
    );
    assert_eq!(f.r.posts.load(Ordering::SeqCst), 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn native_complete_review_crossing_source_scan_expiry_cannot_commit_a_continue_receipt(
    pool: PgPool,
) {
    let f = setup_data(&pool, Bytes::from_static(b"short-lived scan authority")).await;
    let (_, _, asset, _) = runtime_artifact_scan_tests::native(&pool, &f, "expiring-native").await;
    let (profile, who, _) = scanner(&pool, &f, 1).await;
    // Shorter insert-time TTL is valid and immutable; it never changes an
    // existing asset or scan. This isolates the final natural-expiry boundary.
    sqlx::raw_sql("CREATE FUNCTION short_native_scan() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.runtime_asset_id IS NOT NULL THEN NEW.expires_at=clock_timestamp()+interval '4 seconds'; END IF; RETURN NEW; END; $$; CREATE TRIGGER short_native_scan BEFORE INSERT ON output_scans FOR EACH ROW EXECUTE FUNCTION short_native_scan();").execute(&pool).await.unwrap();
    let scans = Scans::new(f.o.assets.clone());
    let v = scans.claim(&who, Uuid::new_v4()).await.unwrap().unwrap();
    scans
        .report(&who, attempt_id(&v), scan_report(&pool, &v).await)
        .await
        .unwrap();
    common::guardrail(
        &pool,
        f.who.tenant_id,
        "native-content",
        1,
        "artifact",
        json!({"kind":"asset_scan","profile_id":profile.id}),
    )
    .await;
    let d = Delivery::new(f.o.assets.clone());
    let a = common::approval_id(&d.release(&f.who, asset).await.unwrap());
    let p = proof(&pool, &f).await;
    let reviewer = ReviewDelivery::new(f.o.assets.clone());
    let session = Uuid::new_v4();
    let m = review::start(
        &reviewer,
        &p,
        review::Start {
            approval_id: a,
            session_id: session,
        },
    )
    .await
    .unwrap();
    review::chunk(
        &reviewer,
        &p,
        review::Chunk {
            session_id: session,
            ordinal: 0,
        },
    )
    .await
    .unwrap();
    sqlx::raw_sql("CREATE SEQUENCE native_review_expiry_seen; CREATE FUNCTION delay_native_review_audit() RETURNS trigger LANGUAGE plpgsql AS $$ DECLARE until_at timestamptz; BEGIN IF NEW.action='approval.reviewed' AND NEW.details->>'review_kind'='isolated_mp4' THEN PERFORM nextval('native_review_expiry_seen'); SELECT s.expires_at INTO until_at FROM media_review_sessions s WHERE s.id=(NEW.details->>'review_id')::uuid; PERFORM pg_sleep(GREATEST(0,extract(epoch FROM until_at-clock_timestamp()))+0.05); END IF; RETURN NEW; END; $$; CREATE TRIGGER delay_native_review_audit BEFORE INSERT ON audit_events FOR EACH ROW EXECUTE FUNCTION delay_native_review_audit();").execute(&pool).await.unwrap();
    assert!(matches!(
        review::complete(
            &reviewer,
            &p,
            review::Complete {
                session_id: session,
                sha256: m["output"]["sha256"].as_str().unwrap().into(),
                reviewed: true
            }
        )
        .await,
        Err(Error::ResultUnavailable)
    ));
    assert!(
        sqlx::query_scalar::<_, bool>("SELECT is_called FROM native_review_expiry_seen")
            .fetch_one(&pool)
            .await
            .unwrap()
    );
    assert_eq!(sqlx::query_as::<_, (i64, i64, i64)>("SELECT (SELECT count(*) FROM approval_reviews),(SELECT count(*) FROM media_review_completions),(SELECT count(*) FROM runtime_artifact_grants)").fetch_one(&pool).await.unwrap(), (0, 0, 0));
    assert!(matches!(
        d.release(&f.who, asset).await,
        Err(Error::ResultUnavailable)
    ));
    assert_eq!(f.r.posts.load(Ordering::SeqCst), 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn native_release_scan_pending_is_metadata_only_and_failed_source_never_creates_approval(
    pool: PgPool,
) {
    let f = setup_data(
        &pool,
        Bytes::from_static(b"not released before a real scan"),
    )
    .await;
    let (_, _, asset, _) = runtime_artifact_scan_tests::native(&pool, &f, "pending-native").await;
    let (_, who, _) = scanner(&pool, &f, 1).await;
    let d = Delivery::new(f.o.assets.clone());
    let pending = d.release(&f.who, asset).await.unwrap();
    assert_eq!(pending["state"], "checking_scan");
    assert!(pending.get("output").is_none());
    assert!(pending.get("approval").is_none());
    assert!(pending["scan_id"].is_null());
    assert!(d.manifest(&f.who, asset).await.is_err());
    assert!(d.chunk(&f.who, asset, 0).await.is_err());
    let scans = Scans::new(f.o.assets.clone());
    let attempt = scans.claim(&who, Uuid::new_v4()).await.unwrap().unwrap();
    assert_eq!(
        d.release(&f.who, asset).await.unwrap()["scan_id"],
        attempt["scan_id"]
    );
    let mut report = scan_report(&pool, &attempt).await;
    report.technical = Technical::Error;
    report.media_type = None;
    report.content = Content::Unknown;
    scans
        .report(&who, attempt_id(&attempt), report)
        .await
        .unwrap();
    assert!(matches!(
        d.release(&f.who, asset).await,
        Err(Error::OutputUnsafe)
    ));
    let (_, _, closed_asset, _) =
        runtime_artifact_scan_tests::native(&pool, &f, "closed-native").await;
    let closed = scans.claim(&who, Uuid::new_v4()).await.unwrap().unwrap();
    assert_eq!(closed["asset_id"], closed_asset.to_string());
    sqlx::query("UPDATE output_scans SET state='closed',finished_at=clock_timestamp() WHERE runtime_asset_id=$1")
        .bind(closed_asset)
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        d.release(&f.who, closed_asset).await,
        Err(Error::OutputUnsafe)
    ));
    assert!(d.manifest(&f.who, closed_asset).await.is_err());
    assert!(d.chunk(&f.who, closed_asset, 0).await.is_err());
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM approvals")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    assert_eq!(f.r.posts.load(Ordering::SeqCst), 0);
}

fn http_request(method: &str, uri: &str, key: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {key}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_owned()))
        .unwrap()
}

#[sqlx::test(migrations = "./migrations")]
async fn native_file_authority_cannot_be_recreated_or_consumed_as_tool_execution_over_http(
    pool: PgPool,
) {
    let (f, asset, _, _, d) = prepared(&pool).await;
    approve(&pool, &f, &d, asset).await;
    let ready = d.release(&f.who, asset).await.unwrap();
    let operation: Uuid = serde_json::from_value(ready["operation_id"].clone()).unwrap();
    let (turn, key, hash): (Uuid, String, Vec<u8>) = sqlx::query_as("SELECT turn_id,operation_key,payload_hash FROM governance_operations WHERE tenant_id=$1 AND id=$2")
        .bind(f.who.tenant_id).bind(operation).fetch_one(&pool).await.unwrap();
    assert!(key.starts_with("_br_runtime_artifact_release:"));
    let gateway = Gateway::new(pool.clone(), common::cipher(), HashMap::new(), 4, 5, 60).unwrap();
    let app = brokerrouter::api::gateway_router(gateway);
    for (path, body) in [
        (
            "/v1/governance/checks".to_owned(),
            json!({"turn_id":turn,"operation_key":key,"stage":"artifact","model_id":f.e.model_id,"tool_name":"bounded_mp4_fixture","payload":ready["output"]}),
        ),
        (
            format!("/v1/governance/operations/{operation}/claim"),
            json!({"token":"44".repeat(32),"payload_hash":common::hex(&hash)}),
        ),
        (
            format!("/v1/governance/operations/{operation}/receipt"),
            json!({"token":"44".repeat(32),"outcome":"completed","evidence_ref":"runtime://forged-execution"}),
        ),
    ] {
        let response = app
            .clone()
            .oneshot(http_request("POST", &path, &f.key, &body.to_string()))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap()["error"]["code"],
            "invalid_request"
        );
    }
    let forged = json!({"turn_id":turn,"operation_key":"caller-declared-artifact","stage":"artifact","model_id":f.e.model_id,"tool_name":"bounded_mp4_fixture","payload":ready["output"]});
    let response = app
        .clone()
        .oneshot(http_request(
            "POST",
            "/v1/governance/checks",
            &f.key,
            &forged.to_string(),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["error"]["code"], "invalid_request");
    assert!(body.get("text").is_none() && body.get("approval").is_none());
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM governance_operations WHERE operation_key='caller-declared-artifact'")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    let facts: (String, bool, bool, i64, i64, i64, i64) = sqlx::query_as("SELECT state,claim_hash IS NULL,receipt_hash IS NULL,(SELECT count(*) FROM runtime_artifact_deliveries),(SELECT count(*) FROM governance_operations WHERE stage='tool_input' AND state='completed'),(SELECT count(*) FROM runtime_artifact_grants),(SELECT count(*) FROM guardrail_grants) FROM governance_operations WHERE tenant_id=$1 AND id=$2")
        .bind(f.who.tenant_id).bind(operation).fetch_one(&pool).await.unwrap();
    assert_eq!(facts, ("ready".into(), true, true, 1, 1, 1, 0));
    assert_eq!(f.r.posts.load(Ordering::SeqCst), 0);
}

async fn json_body(response: Response) -> Value {
    assert_eq!(response.headers()["cache-control"], "private, no-store");
    assert_eq!(response.headers()["x-content-type-options"], "nosniff");
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

#[sqlx::test(migrations = "./migrations")]
async fn native_delivery_http_rejects_unbound_requests_and_holds_capacity_through_binary_body(
    pool: PgPool,
) {
    let (f, asset, _, _, d) = prepared(&pool).await;
    approve(&pool, &f, &d, asset).await;
    let single = Assets::new(pool.clone(), f.objects.clone(), common::cipher(), 1).unwrap();
    let app = runtime_artifact_delivery_api::router(Delivery::new(single));
    let chunk = format!("/v1/runtime-artifacts/{asset}/chunks/0");
    for (path, header, value) in [
        (
            format!("{chunk}?token=caller-supplied"),
            "content-encoding",
            "identity",
        ),
        (chunk.clone(), "range", "bytes=0-1"),
        (chunk.clone(), "content-encoding", "gzip"),
    ] {
        let mut request = http_request("GET", &path, &f.key, "");
        request.headers_mut().insert(header, value.parse().unwrap());
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            json_body(response).await["error"]["code"],
            "invalid_request"
        );
    }
    let release = format!("/v1/runtime-artifacts/{asset}/release");
    for body in [
        "{\"approved\":true}",
        "{\"path\":\"/outside/file\"}",
        "{\"claim_token\":\"unbound\"}",
    ] {
        assert_eq!(
            app.clone()
                .oneshot(http_request("POST", &release, &f.key, body))
                .await
                .unwrap()
                .status(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }
    assert_eq!(
        app.clone()
            .oneshot(http_request("PUT", &chunk, &f.key, "{}"))
            .await
            .unwrap()
            .status(),
        StatusCode::METHOD_NOT_ALLOWED
    );
    assert_eq!(
        app.clone()
            .oneshot(http_request("POST", "/v1/assets", &f.key, "{}"))
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        app.clone()
            .oneshot(http_request("POST", "/v1/approvals/resolve", &f.key, "{}"))
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    let other = setup_data(&pool, Bytes::from_static(b"other tenant")).await;
    assert_eq!(
        app.clone()
            .oneshot(http_request("GET", &chunk, &other.key, ""))
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    let held = app
        .clone()
        .oneshot(http_request("GET", &chunk, &f.key, ""))
        .await
        .unwrap();
    assert_eq!(held.status(), StatusCode::OK);
    assert_eq!(held.headers()["content-type"], "application/octet-stream");
    let response = app
        .clone()
        .oneshot(http_request("GET", &chunk, &f.key, ""))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(json_body(response).await["error"]["code"], "gateway_busy");
    assert_eq!(
        held.into_body().collect().await.unwrap().to_bytes(),
        f.r.data.slice(..CHUNK_BYTES)
    );
    let response = app
        .clone()
        .oneshot(http_request("GET", &chunk, &f.key, ""))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    drop(response);
    assert_eq!(f.r.posts.load(Ordering::SeqCst), 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn native_delivery_release_response_loss_finishes_owned_approval_and_retry_keeps_one_operation(
    pool: PgPool,
) {
    let (f, asset, _, _, d) = prepared(&pool).await;
    sqlx::raw_sql("CREATE SEQUENCE native_release_seen; CREATE FUNCTION hold_native_release_event() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.event_type='approval.required' THEN PERFORM nextval('native_release_seen'); PERFORM pg_sleep(1); END IF; RETURN NEW; END; $$; CREATE TRIGGER hold_native_release_event BEFORE INSERT ON governance_events FOR EACH ROW EXECUTE FUNCTION hold_native_release_event();").execute(&pool).await.unwrap();
    let app = runtime_artifact_delivery_api::router(d.clone());
    let path = format!("/v1/runtime-artifacts/{asset}/release");
    let task = tokio::spawn(
        app.clone()
            .oneshot(http_request("POST", &path, &f.key, "{}")),
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if sqlx::query_scalar::<_, bool>("SELECT is_called FROM native_release_seen")
                .fetch_one(&pool)
                .await
                .unwrap()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    tokio::time::timeout(Duration::from_secs(5), d.assets.drain())
        .await
        .unwrap();
    let restarted = runtime_artifact_delivery_api::router(Delivery::new(
        Assets::new(pool.clone(), f.objects.clone(), common::cipher(), 4).unwrap(),
    ));
    let response = restarted
        .oneshot(http_request("POST", &path, &f.key, "{}"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let retry = json_body(response).await;
    assert_eq!(retry["state"], "waiting_approval");
    assert_eq!(sqlx::query_as::<_, (i64, i64, i64)>("SELECT (SELECT count(*) FROM runtime_artifact_deliveries),(SELECT count(*) FROM approvals),(SELECT count(*) FROM governance_events WHERE event_type='approval.required')").fetch_one(&pool).await.unwrap(), (1, 1, 1));
    assert_eq!(f.r.posts.load(Ordering::SeqCst), 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn native_delivery_readiness_is_bounded_even_when_database_capacity_is_exhausted(
    pool: PgPool,
) {
    let constrained = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect_with(pool.connect_options().as_ref().clone())
        .await
        .unwrap();
    let held = constrained.acquire().await.unwrap();
    let d = Delivery::new(
        Assets::new(
            constrained.clone(),
            Arc::new(InMemory::new()),
            common::cipher(),
            1,
        )
        .unwrap(),
    );
    let response = tokio::time::timeout(
        Duration::from_secs(5),
        runtime_artifact_delivery_api::router(d).oneshot(
            Request::builder()
                .uri("/health/ready")
                .body(Body::empty())
                .unwrap(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        json_body(response).await["error"]["code"],
        "schema_mismatch"
    );
    drop(held);
    constrained.close().await;
}

async fn purpose(pool: &PgPool, role: &'static str) -> PgPool {
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
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
async fn native_delivery_review_and_decision_use_separate_real_database_purposes(pool: PgPool) {
    let (f, asset, _, _, _) = prepared(&pool).await;
    sqlx::raw_sql(include_str!("../../deploy/permissions.sql"))
        .execute(&pool)
        .await
        .unwrap();
    let delivery_pool = purpose(&pool, "brokerrouter_output_delivery").await;
    let review_pool = purpose(&pool, "brokerrouter_media_review").await;
    let approvals_pool = purpose(&pool, "brokerrouter_approvals").await;
    let d = Delivery::new(
        Assets::new(
            delivery_pool.clone(),
            f.objects.clone(),
            common::cipher(),
            2,
        )
        .unwrap(),
    );
    let waiting = d.release(&f.who, asset).await.unwrap();
    let a = common::approval_id(&waiting);
    let p = proof(&pool, &f).await;
    let reviewer = ReviewDelivery::new(
        Assets::new(review_pool.clone(), f.objects.clone(), common::cipher(), 2).unwrap(),
    );
    let session = Uuid::new_v4();
    let m = review::start(
        &reviewer,
        &p,
        review::Start {
            approval_id: a,
            session_id: session,
        },
    )
    .await
    .unwrap();
    for ordinal in 0..m["output"]["chunk_count"].as_i64().unwrap() as i32 {
        review::chunk(
            &reviewer,
            &p,
            review::Chunk {
                session_id: session,
                ordinal,
            },
        )
        .await
        .unwrap();
    }
    review::complete(
        &reviewer,
        &p,
        review::Complete {
            session_id: session,
            sha256: m["output"]["sha256"].as_str().unwrap().into(),
            reviewed: true,
        },
    )
    .await
    .unwrap();
    approval_workbench::decide(&approvals_pool, &common::cipher(), &p, decide(a, session))
        .await
        .unwrap();
    assert_eq!(
        d.chunk(&f.who, asset, 0).await.unwrap(),
        f.r.data.slice(..CHUNK_BYTES)
    );
    for (pool, forbidden) in [
        (
            &delivery_pool,
            "INSERT INTO runtime_artifact_grants SELECT * FROM runtime_artifact_grants",
        ),
        (&review_pool, "UPDATE approvals SET state='approved'"),
        (
            &review_pool,
            "INSERT INTO runtime_artifact_grants SELECT * FROM runtime_artifact_grants",
        ),
        (&approvals_pool, "SELECT * FROM asset_chunks"),
        (
            &delivery_pool,
            "UPDATE runtime_artifact_batches SET state='expired'",
        ),
    ] {
        let error = sqlx::query(forbidden).execute(pool).await.unwrap_err();
        assert_eq!(
            error.as_database_error().and_then(|e| e.code()).as_deref(),
            Some("42501")
        );
    }
    for pool in [&delivery_pool, &review_pool, &approvals_pool] {
        pool.close().await;
    }
    assert_eq!(f.r.posts.load(Ordering::SeqCst), 0);
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires pinned local ClamAV/FFmpeg image; run explicitly for native artifact certification"]
async fn native_real_mp4_scan_and_full_human_review_release_exact_bytes_without_media_provider(
    pool: PgPool,
) {
    use brokerrouter::scanner_worker::Worker;
    let mut local = detector::Local::new("http://127.0.0.1/".into(), true);
    local.engine().preflight().await.unwrap();
    let video = local.video().await;
    let f = setup_data(&pool, Bytes::from(video)).await;
    let (_, _, asset, input) =
        runtime_artifact_scan_tests::native(&pool, &f, "native-real-mp4").await;
    let (token, server) = worker_setup(&pool, &f, &mut local).await;
    let worker = Worker::new(local.config.clone(), token).unwrap();
    let result = worker.once().await.unwrap();
    assert_eq!(result["technical"], "clean");
    assert_eq!(result["content"], "unknown");
    assert_eq!(result["delivery"], "not_released");
    let (report, profile): (Value, Uuid) =
        sqlx::query_as("SELECT report,profile_id FROM output_scans WHERE runtime_asset_id=$1")
            .bind(asset)
            .fetch_one(&pool)
            .await
            .unwrap();
    let evidence = std::fs::read(local.config.work_dir.join(format!(
        "evidence-{}.json",
        report["evidence_sha256"].as_str().unwrap()
    )))
    .unwrap();
    let evidence: Value = serde_json::from_slice(&evidence).unwrap();
    assert_eq!(evidence["source"], "runtime_artifact");
    assert_eq!(evidence["input_operation_id"], input.to_string());
    assert_eq!(evidence["runtime_asset_id"], asset.to_string());
    assert!(evidence.get("job_id").is_none());
    assert_eq!(evidence["steps"].as_array().unwrap().len(), 3);
    common::guardrail(
        &pool,
        f.who.tenant_id,
        "native-content",
        1,
        "artifact",
        json!({"kind":"asset_scan","profile_id":profile}),
    )
    .await;
    let d = Delivery::new(f.o.assets.clone());
    assert_eq!(
        d.release(&f.who, asset).await.unwrap()["state"],
        "waiting_approval"
    );
    assert!(d.chunk(&f.who, asset, 0).await.is_err());
    approve(&pool, &f, &d, asset).await;
    assert_eq!(d.chunk(&f.who, asset, 0).await.unwrap(), f.r.data);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM media_jobs")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    assert_eq!(f.r.posts.load(Ordering::SeqCst), 0);
    assert!(!local.config.work_dir.join("input").exists());
    server.abort();
}
