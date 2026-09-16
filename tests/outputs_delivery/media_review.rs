use super::*;
use brokerrouter::{approval_workbench, media_review as review, media_review_api};
async fn start(d: &Delivery, p: &human::Human, id: Uuid, s: Uuid) -> Value {
    review::start(
        d,
        p,
        review::Start {
            approval_id: id,
            session_id: s,
        },
    )
    .await
    .unwrap()
}
async fn all(d: &Delivery, p: &human::Human, s: Uuid, m: &Value) {
    for n in 0..m["output"]["chunk_count"].as_i64().unwrap() as i32 {
        review::chunk(
            d,
            p,
            review::Chunk {
                session_id: s,
                ordinal: n,
            },
        )
        .await
        .unwrap();
    }
}
fn complete(s: Uuid, m: &Value) -> review::Complete {
    review::Complete {
        session_id: s,
        sha256: m["output"]["sha256"].as_str().unwrap().into(),
        reviewed: true,
    }
}
fn decide(a: Uuid, s: Uuid) -> approval_workbench::Decide {
    approval_workbench::Decide {
        approval_id: a,
        review_id: s,
        acknowledged_scope: true,
        decision: serde_json::from_value(common::approve(&["content"])).unwrap(),
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn isolated_media_owner_can_reject_or_feedback_without_reading_risky_bytes(pool: PgPool) {
    for choice in ["reject", "feedback"] {
        let subject = format!("unviewed-{choice}");
        let (f, turn, job, _, _, d) = prepared(&pool, &subject).await;
        let a = common::approval_id(&d.check(&f.who, job).await.unwrap());
        let p = common::proof(&pool, &subject, "/review", &json!({})).await;
        let r = approval_workbench::review(
            &pool,
            &common::cipher(),
            &p,
            approval_workbench::Review { approval_id: a },
        )
        .await
        .unwrap();
        let mut q = decide(a, serde_json::from_value(r["review_id"].clone()).unwrap());
        q.decision=serde_json::from_value(json!({"decision":choice,"approved_guardrail_ids":[],"feedback":if choice=="feedback"{json!("Use a different scene")}else{Value::Null}})).unwrap();
        let value = approval_workbench::decide(&pool, &common::cipher(), &p, q)
            .await
            .unwrap();
        assert_eq!(
            value["state"],
            if choice == "reject" {
                "rejected"
            } else {
                "feedback"
            }
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM guardrail_grants")
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
        if choice == "feedback" {
            assert_eq!(
                sqlx::query_scalar::<_, String>("SELECT state FROM governance_turns WHERE id=$1")
                    .bind(turn)
                    .fetch_one(&pool)
                    .await
                    .unwrap(),
                "cancelled"
            );
        }
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn isolated_media_complete_delivery_and_decision_are_separate_idempotent_transactions(
    pool: PgPool,
) {
    let (f, _, job, _, _, d) = prepared(&pool, "media-reader").await;
    let a = common::approval_id(&d.check(&f.who, job).await.unwrap());
    let p = common::proof(&pool, "media-reader", "/review", &json!({})).await;
    let s = Uuid::new_v4();
    let m = start(&d, &p, a, s).await;
    assert_eq!(m, start(&d, &p, a, s).await);
    assert!(review::complete(&d, &p, complete(s, &m)).await.is_err());
    review::chunk(
        &d,
        &p,
        review::Chunk {
            session_id: s,
            ordinal: 0,
        },
    )
    .await
    .unwrap();
    assert!(review::complete(&d, &p, complete(s, &m)).await.is_err());
    all(&d, &p, s, &m).await;
    let (x, y) = tokio::join!(
        review::complete(&d, &p, complete(s, &m)),
        review::complete(&d, &p, complete(s, &m))
    );
    assert_eq!(x.unwrap(), y.unwrap());
    assert_eq!(counts(&pool).await, (1, 1, 0));
    let r = approval_workbench::decide(&pool, &common::cipher(), &p, decide(a, s))
        .await
        .unwrap();
    assert_eq!(r["state"], "approved");
    assert_eq!(
        r,
        approval_workbench::decide(&pool, &common::cipher(), &p, decide(a, s))
            .await
            .unwrap()
    );
    assert_eq!(counts(&pool).await, (1, 1, 1));
    assert!(
        review::chunk(
            &d,
            &p,
            review::Chunk {
                session_id: s,
                ordinal: 0
            }
        )
        .await
        .is_err()
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT state FROM governance_operations WHERE stage='media_output'"
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        "waiting_approval"
    );
    assert_eq!(f.r.posts.load(Ordering::SeqCst), 1);
}
#[sqlx::test(migrations = "./migrations")]
async fn isolated_media_owner_digest_ack_and_json_receipt_cannot_be_substituted(pool: PgPool) {
    let (f, _, job, _, _, d) = prepared(&pool, "owner").await;
    let a = common::approval_id(&d.check(&f.who, job).await.unwrap());
    let p = common::proof(&pool, "owner", "/review", &json!({})).await;
    let s = Uuid::new_v4();
    let m = start(&d, &p, a, s).await;
    human::add_user(&pool, f.who.tenant_id, common::ISSUER, "other")
        .await
        .unwrap();
    let other = common::proof(&pool, "other", "/review", &json!({})).await;
    assert!(
        review::start(
            &d,
            &other,
            review::Start {
                approval_id: a,
                session_id: s
            }
        )
        .await
        .is_err()
    );
    assert!(
        review::chunk(
            &d,
            &other,
            review::Chunk {
                session_id: s,
                ordinal: 0
            }
        )
        .await
        .is_err()
    );
    all(&d, &p, s, &m).await;
    let mut q = complete(s, &m);
    q.reviewed = false;
    assert!(review::complete(&d, &p, q).await.is_err());
    let mut q = complete(s, &m);
    q.sha256 = "00".repeat(32);
    assert!(review::complete(&d, &p, q).await.is_err());
    assert!(
        approval_workbench::decide(&pool, &common::cipher(), &p, decide(a, s))
            .await
            .is_err()
    );
    let json = approval_workbench::review(
        &pool,
        &common::cipher(),
        &p,
        approval_workbench::Review { approval_id: a },
    )
    .await
    .unwrap();
    assert!(
        approval_workbench::decide(
            &pool,
            &common::cipher(),
            &p,
            decide(
                a,
                serde_json::from_value(json["review_id"].clone()).unwrap()
            )
        )
        .await
        .is_err()
    );
    assert_eq!(counts(&pool).await, (1, 1, 0));
}
#[sqlx::test(migrations = "./migrations")]
async fn isolated_media_rechecks_revocation_after_storage_and_does_not_record_chunk(pool: PgPool) {
    let (f, _, job, _, _, d) = prepared(&pool, "io-reader").await;
    let a = common::approval_id(&d.check(&f.who, job).await.unwrap());
    let p = common::proof(&pool, "io-reader", "/review", &json!({})).await;
    let s = Uuid::new_v4();
    start(&d, &p, a, s).await;
    let paused = Arc::new(paused_store::Paused {
        inner: f.objects.clone(),
        entered: tokio::sync::Notify::new(),
        resume: tokio::sync::Notify::new(),
    });
    let reader =
        Delivery::new(Assets::new(pool.clone(), paused.clone(), common::cipher(), 2).unwrap());
    let task = tokio::spawn(async move {
        review::chunk(
            &reader,
            &p,
            review::Chunk {
                session_id: s,
                ordinal: 0,
            },
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(5), paused.entered.notified())
        .await
        .unwrap();
    store::revoke_key(&pool, f.who.tenant_id, f.who.key_id)
        .await
        .unwrap();
    paused.resume.notify_one();
    assert!(task.await.unwrap().is_err());
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM media_review_chunks")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
}
#[sqlx::test(migrations = "./migrations")]
async fn isolated_media_policy_change_and_expiry_invalidate_completed_receipts(pool: PgPool) {
    for mode in 0..2 {
        let subject = format!("change-{mode}");
        let (f, _, job, profile, _, d) = prepared(&pool, &subject).await;
        let a = common::approval_id(&d.check(&f.who, job).await.unwrap());
        let p = common::proof(&pool, &subject, "/review", &json!({})).await;
        let s = Uuid::new_v4();
        let m = start(&d, &p, a, s).await;
        all(&d, &p, s, &m).await;
        review::complete(&d, &p, complete(s, &m)).await.unwrap();
        if mode == 0 {
            common::guardrail(
                &pool,
                f.who.tenant_id,
                "content",
                2,
                "media_output",
                json!({"kind":"asset_scan","profile_id":profile.id}),
            )
            .await;
        } else {
            sqlx::raw_sql("ALTER TABLE approval_reviews DISABLE TRIGGER approval_review_immutable; UPDATE approval_reviews SET expires_at=created_at+interval '1 microsecond'; ALTER TABLE approval_reviews ENABLE TRIGGER approval_review_immutable;").execute(&pool).await.unwrap();
        }
        assert!(
            approval_workbench::decide(&pool, &common::cipher(), &p, decide(a, s))
                .await
                .is_err()
        );
    }
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM guardrail_grants")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
}
#[sqlx::test(migrations = "./migrations")]
async fn isolated_media_audit_failure_rolls_back_completion_and_receipt(pool: PgPool) {
    let (f, _, job, _, _, d) = prepared(&pool, "audit-reader").await;
    let a = common::approval_id(&d.check(&f.who, job).await.unwrap());
    let p = common::proof(&pool, "audit-reader", "/review", &json!({})).await;
    let s = Uuid::new_v4();
    let m = start(&d, &p, a, s).await;
    all(&d, &p, s, &m).await;
    sqlx::raw_sql("CREATE FUNCTION fail_media_audit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.action='approval.reviewed' THEN RAISE EXCEPTION 'fixture'; END IF; RETURN NEW; END $$; CREATE TRIGGER fail_media_audit BEFORE INSERT ON audit_events FOR EACH ROW EXECUTE FUNCTION fail_media_audit();").execute(&pool).await.unwrap();
    assert!(review::complete(&d, &p, complete(s, &m)).await.is_err());
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM approval_reviews")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM media_review_completions")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    sqlx::query("DROP TRIGGER fail_media_audit ON audit_events")
        .execute(&pool)
        .await
        .unwrap();
    review::complete(&d, &p, complete(s, &m)).await.unwrap();
}
#[sqlx::test(migrations = "./migrations")]
async fn isolated_media_deployed_reader_cannot_approve_and_decider_cannot_read_bytes(pool: PgPool) {
    let (f, _, job, _, _, d) = prepared(&pool, "role-reader").await;
    let a = common::approval_id(&d.check(&f.who, job).await.unwrap());
    sqlx::raw_sql(include_str!("../../deploy/permissions.sql"))
        .execute(&pool)
        .await
        .unwrap();
    let reader = scoped(&pool, "brokerrouter_media_review").await;
    let decider = scoped(&pool, "brokerrouter_approvals").await;
    let input =
        f.o.assets
            .create(
                &f.who,
                "hidden-input",
                assets::CreateAsset {
                    byte_length: 1,
                    sha256: common::hex(&store::hash(b"x")),
                    media_type: "application/octet-stream".into(),
                    ttl_seconds: 60,
                },
            )
            .await
            .unwrap();
    for role in [&reader, &decider] {
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM assets WHERE id=$1")
                .bind(serde_json::from_value::<Uuid>(input["id"].clone()).unwrap())
                .fetch_one(role)
                .await
                .unwrap(),
            0
        );
    }
    let d =
        Delivery::new(Assets::new(reader.clone(), f.objects.clone(), common::cipher(), 2).unwrap());
    let p = common::proof(&pool, "role-reader", "/review", &json!({})).await;
    let s = Uuid::new_v4();
    let m = start(&d, &p, a, s).await;
    all(&d, &p, s, &m).await;
    review::complete(&d, &p, complete(s, &m)).await.unwrap();
    review::maintain(&reader).await.unwrap();
    assert!(
        approval_workbench::decide(&reader, &common::cipher(), &p, decide(a, s))
            .await
            .is_err()
    );
    approval_workbench::decide(&decider, &common::cipher(), &p, decide(a, s))
        .await
        .unwrap();
    for q in [
        "SELECT key_hash FROM virtual_keys",
        "UPDATE approvals SET state='approved'",
        "INSERT INTO guardrail_grants SELECT * FROM guardrail_grants",
        "UPDATE media_jobs SET state='failed'",
        "UPDATE assets SET state='deleted'",
        "UPDATE governance_operations SET state='executing'",
        "SET ROLE brokerrouter_approvals",
        "TRUNCATE media_review_sessions",
    ] {
        assert!(sqlx::query(q).execute(&reader).await.is_err(), "{q}");
    }
    assert!(
        sqlx::query("SELECT * FROM asset_chunks")
            .execute(&decider)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("UPDATE media_review_sessions SET expires_at=expires_at")
            .execute(&pool)
            .await
            .is_err()
    );
    reader.close().await;
    decider.close().await;
}

#[sqlx::test(migrations = "./migrations")]
async fn isolated_media_deadline_during_audit_rolls_back_and_active_sessions_are_bounded(
    pool: PgPool,
) {
    let (f, _, job, _, _, d) = prepared(&pool, "deadline-reader").await;
    let a = common::approval_id(&d.check(&f.who, job).await.unwrap());
    let p = common::proof(&pool, "deadline-reader", "/review", &json!({})).await;
    let s = Uuid::new_v4();
    let m = start(&d, &p, a, s).await;
    for _ in 0..3 {
        start(&d, &p, a, Uuid::new_v4()).await;
    }
    assert!(matches!(
        review::start(
            &d,
            &p,
            review::Start {
                approval_id: a,
                session_id: Uuid::new_v4()
            }
        )
        .await,
        Err(Error::Busy)
    ));
    all(&d, &p, s, &m).await;
    // Test-only clock travel; production roles cannot update immutable review facts.
    sqlx::query("ALTER TABLE media_review_sessions DISABLE TRIGGER media_review_session_immutable")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE media_review_sessions SET expires_at=clock_timestamp()+interval '1 second' WHERE id=$1").bind(s).execute(&pool).await.unwrap();
    sqlx::query("ALTER TABLE media_review_sessions ENABLE TRIGGER media_review_session_immutable")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::raw_sql("CREATE FUNCTION slow_media_audit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.action='approval.reviewed' THEN PERFORM pg_sleep(2); END IF; RETURN NEW; END $$; CREATE TRIGGER slow_media_audit BEFORE INSERT ON audit_events FOR EACH ROW EXECUTE FUNCTION slow_media_audit();").execute(&pool).await.unwrap();
    assert!(matches!(
        review::complete(&d, &p, complete(s, &m)).await,
        Err(Error::ResultUnavailable)
    ));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM approval_reviews")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM media_review_completions")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
}
#[sqlx::test(migrations = "./migrations")]
async fn isolated_media_http_is_purpose_path_body_bound_and_binary_only(pool: PgPool) {
    let (f, _, job, _, _, d) = prepared(&pool, "http-reader").await;
    let a = common::approval_id(&d.check(&f.who, job).await.unwrap());
    let s = Uuid::new_v4();
    let body = json!({"approval_id":a,"session_id":s});
    let path = "/media-review/v1/start";
    let app = media_review_api::router(media_review_api::Service {
        delivery: d.clone(),
        verifier: Arc::new(common::media_review_verifier()),
    });
    for token in [
        f.key.clone(),
        common::sign(&common::claims(f.who.tenant_id, "http-reader", path, &body)),
        common::approvals_sign(common::claims(f.who.tenant_id, "http-reader", path, &body)),
    ] {
        let r = app
            .clone()
            .oneshot(
                Request::post(path)
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), 401);
    }
    let token =
        common::media_review_sign(common::claims(f.who.tenant_id, "http-reader", path, &body));
    let r = app
        .clone()
        .oneshot(
            Request::post(path)
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let path = "/media-review/v1/chunk";
    let body = json!({"session_id":s,"ordinal":0});
    let token =
        common::media_review_sign(common::claims(f.who.tenant_id, "http-reader", path, &body));
    for (target, range, expected) in [
        ("/media-review/v1/complete", false, 401),
        ("/media-review/v1/chunk?x=1", false, 400),
        (path, true, 400),
        (path, false, 200),
    ] {
        let mut b = Request::post(target).header("authorization", format!("Bearer {token}"));
        if range {
            b = b.header("range", "bytes=0-1");
        }
        let r = app
            .clone()
            .oneshot(b.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        assert_eq!(r.status(), expected);
        if expected == 200 {
            assert_eq!(r.headers()["content-type"], "application/octet-stream");
            assert_eq!(r.headers()["content-disposition"], "attachment");
            assert_eq!(r.headers()["cache-control"], "no-store");
            assert_eq!(
                r.into_body().collect().await.unwrap().to_bytes(),
                f.r.data.slice(..CHUNK_BYTES)
            );
        }
    }
}
