use super::*;
mod media_review;
mod paused_store;
use brokerrouter::{
    governance, human,
    output_delivery::Delivery,
    output_delivery_api,
    output_scans::{Content, Technical},
};

async fn scan_done(
    pool: &PgPool,
    f: &Fixture,
    who: &Scanner,
    s: &Scans,
    content: Content,
    technical: Technical,
) -> Value {
    let v = s.claim(who, Uuid::new_v4()).await.unwrap().unwrap();
    let mut report = scan_report(pool, &v).await;
    report.content = content;
    report.technical = technical;
    if report.technical != Technical::Clean {
        report.media_type = None;
        report.content = Content::Unknown;
    }
    s.report(who, attempt_id(&v), report).await.unwrap();
    assert!(f.r.posts.load(Ordering::SeqCst) > 0);
    v
}
async fn prepared(
    pool: &PgPool,
    subject: &str,
) -> (Fixture, Uuid, Uuid, scan::Profile, Scanner, Delivery) {
    let f = setup(pool).await;
    common::provision(pool, &f.who, subject).await;
    let turn = common::turn(pool, &f.who, subject, 0).await;
    let job = generate(&f, pool, Some(turn)).await;
    f.o.tick().await.unwrap();
    let (profile, who, _) = scanner(pool, &f, 1).await;
    let scans = Scans::new(f.o.assets.clone());
    scan_done(pool, &f, &who, &scans, Content::Unknown, Technical::Clean).await;
    common::guardrail(
        pool,
        f.who.tenant_id,
        "content",
        1,
        "media_output",
        json!({"kind":"asset_scan","profile_id":profile.id}),
    )
    .await;
    let d = Delivery::new(f.o.assets.clone());
    (f, turn, job, profile, who, d)
}
async fn counts(pool: &PgPool) -> (i64, i64, i64) {
    sqlx::query_as("SELECT (SELECT count(*) FROM media_output_deliveries),(SELECT count(*) FROM approvals),(SELECT count(*) FROM guardrail_grants)").fetch_one(pool).await.unwrap()
}

#[sqlx::test(migrations = "./migrations")]
async fn output_delivery_unknown_review_approval_restart_and_exact_bytes(pool: PgPool) {
    let (f, turn, job, _, _, d) = prepared(&pool, "reader").await;
    assert!(matches!(
        d.manifest(&f.who, job).await,
        Err(Error::InProgress)
    ));
    let (a, b) = tokio::join!(d.check(&f.who, job), d.check(&f.who, job));
    let a = a.unwrap();
    assert_eq!(a, b.unwrap());
    assert_eq!(a["state"], "waiting_approval");
    assert_eq!(
        a["approval"]["summary"]["output"]["scan_report"]["content"],
        "unknown"
    );
    let approval = common::approval_id(&a);
    let proof = common::proof(&pool, "reader", "/review", &json!({})).await;
    assert_eq!(
        d.review_manifest(&proof, approval).await.unwrap()["state"],
        "review_only"
    );
    assert_eq!(
        d.review_chunk(&proof, approval, 0).await.unwrap(),
        f.r.data.slice(..CHUNK_BYTES)
    );
    assert!(d.chunk(&f.who, job, 0).await.is_err());
    assert_eq!(counts(&pool).await, (1, 1, 0));
    common::decide(&pool, "reader", approval, common::approve(&["content"]))
        .await
        .unwrap();
    common::decide(&pool, "reader", approval, common::approve(&["content"]))
        .await
        .unwrap();
    assert!(d.review_chunk(&proof, approval, 0).await.is_err());
    let restarted = Delivery::new(f.o.assets.clone());
    let manifest = restarted.manifest(&f.who, job).await.unwrap();
    assert_eq!(manifest["output"]["turn_id"], turn.to_string());
    let mut data = Vec::new();
    for n in 0..manifest["output"]["chunk_count"].as_i64().unwrap() as i32 {
        data.extend_from_slice(&restarted.chunk(&f.who, job, n).await.unwrap());
    }
    assert_eq!(data.as_slice(), f.r.data.as_ref());
    assert_eq!(counts(&pool).await, (1, 1, 1));
    assert_eq!(balances(&pool, &f.who).await.0, 500000);
    assert_eq!(f.r.posts.load(Ordering::SeqCst), 1);
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT state FROM governance_operations WHERE stage='media_output'"
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        "waiting_approval",
        "grant permits reads but is not an execution receipt"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn output_delivery_grant_is_current_turn_only_and_technical_gate_never_skips(pool: PgPool) {
    let (f, turn, job, _, scanner, d) = prepared(&pool, "turn-user").await;
    assets::set_quota(&pool, f.who.tenant_id, None, 80 * 1024 * 1024)
        .await
        .unwrap();
    assets::set_quota(&pool, f.who.tenant_id, Some(f.who.key_id), 80 * 1024 * 1024)
        .await
        .unwrap();
    let a = d.check(&f.who, job).await.unwrap();
    common::decide(
        &pool,
        "turn-user",
        common::approval_id(&a),
        common::approve(&["content"]),
    )
    .await
    .unwrap();
    let expiry_equal:bool=sqlx::query_scalar("SELECT g.expires_at=t.expires_at FROM guardrail_grants g JOIN governance_turns t ON t.tenant_id=g.tenant_id AND t.id=g.turn_id").fetch_one(&pool).await.unwrap();
    assert!(expiry_equal);
    let second = generate(&f, &pool, Some(turn)).await;
    f.o.tick().await.unwrap();
    let s = Scans::new(f.o.assets.clone());
    scan_done(&pool, &f, &scanner, &s, Content::Risk, Technical::Clean).await;
    assert_eq!(d.check(&f.who, second).await.unwrap()["state"], "ready");
    d.chunk(&f.who, second, 0).await.unwrap();
    let unsafe_job = generate(&f, &pool, Some(turn)).await;
    f.o.tick().await.unwrap();
    scan_done(&pool, &f, &scanner, &s, Content::Unknown, Technical::Unsafe).await;
    assert!(matches!(
        d.check(&f.who, unsafe_job).await,
        Err(Error::OutputUnsafe)
    ));
    assert!(matches!(
        d.chunk(&f.who, unsafe_job, 0).await,
        Err(Error::OutputUnsafe)
    ));
    governance::close_runtime(&pool, &f.who, turn, "completed")
        .await
        .unwrap();
    assert!(d.chunk(&f.who, job, 0).await.is_err());
    let new_turn = common::turn(&pool, &f.who, "turn-user", 1).await;
    let third = generate(&f, &pool, Some(new_turn)).await;
    f.o.tick().await.unwrap();
    scan_done(&pool, &f, &scanner, &s, Content::Risk, Technical::Clean).await;
    assert_eq!(
        d.check(&f.who, third).await.unwrap()["state"],
        "waiting_approval"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn output_delivery_profile_and_policy_upgrades_invalidate_approval_and_grants(pool: PgPool) {
    let (f, _, job, _, _, d) = prepared(&pool, "upgrade").await;
    let old = d.check(&f.who, job).await.unwrap();
    let old_id = common::approval_id(&old);
    let (new_profile, new_scanner, _) = scanner(&pool, &f, 2).await;
    assert!(
        common::decide(&pool, "upgrade", old_id, common::approve(&["content"]))
            .await
            .is_err()
    );
    let s = Scans::new(f.o.assets.clone());
    scan_done(
        &pool,
        &f,
        &new_scanner,
        &s,
        Content::Unknown,
        Technical::Clean,
    )
    .await;
    assert!(matches!(
        d.check(&f.who, job).await,
        Err(Error::PolicyChanged)
    ));
    common::guardrail(
        &pool,
        f.who.tenant_id,
        "content",
        2,
        "media_output",
        json!({"kind":"asset_scan","profile_id":new_profile.id}),
    )
    .await;
    let new = d.check(&f.who, job).await.unwrap();
    assert_ne!(new["operation_id"], old["operation_id"]);
    assert!(
        common::decide(&pool, "upgrade", old_id, common::approve(&["content"]))
            .await
            .is_err()
    );
    common::decide(
        &pool,
        "upgrade",
        common::approval_id(&new),
        common::approve(&["content"]),
    )
    .await
    .unwrap();
    d.chunk(&f.who, job, 0).await.unwrap();
    common::guardrail(
        &pool,
        f.who.tenant_id,
        "content",
        3,
        "media_output",
        json!({"kind":"asset_scan","profile_id":new_profile.id}),
    )
    .await;
    assert!(d.manifest(&f.who, job).await.is_err());
    assert_eq!(
        d.check(&f.who, job).await.unwrap()["state"],
        "waiting_approval"
    );
    sqlx::query("UPDATE guardrail_policies SET active=false WHERE tenant_id=$1")
        .bind(f.who.tenant_id)
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        d.manifest(&f.who, job).await,
        Err(Error::OutputPolicyMissing)
    ));
}

#[sqlx::test(migrations = "./migrations")]
async fn output_delivery_partial_approval_reject_and_feedback_never_regenerate(pool: PgPool) {
    for (subject, decision) in [("reject-user", "reject"), ("feedback-user", "feedback")] {
        let (f, turn, job, _, _, d) = prepared(&pool, subject).await;
        common::guardrail(
            &pool,
            f.who.tenant_id,
            "review-all",
            1,
            "media_output",
            json!({"kind":"always"}),
        )
        .await;
        let first = d.check(&f.who, job).await.unwrap();
        common::decide(
            &pool,
            subject,
            common::approval_id(&first),
            common::approve(&["content"]),
        )
        .await
        .unwrap();
        assert!(d.chunk(&f.who, job, 0).await.is_err());
        let second = d.check(&f.who, job).await.unwrap();
        assert_ne!(first["approval"]["id"], second["approval"]["id"]);
        let feedback = if decision == "feedback" {
            json!("use a different scene")
        } else {
            Value::Null
        };
        common::decide(
            &pool,
            subject,
            common::approval_id(&second),
            json!({"decision":decision,"approved_guardrail_ids":[],"feedback":feedback}),
        )
        .await
        .unwrap();
        assert!(d.check(&f.who, job).await.is_err());
        assert!(d.chunk(&f.who, job, 0).await.is_err());
        let state: String = sqlx::query_scalar("SELECT state FROM governance_turns WHERE id=$1")
            .bind(turn)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            state,
            if decision == "feedback" {
                "cancelled"
            } else {
                "active"
            }
        );
        assert_eq!(f.r.posts.load(Ordering::SeqCst), 1);
        assert_eq!(balances(&pool, &f.who).await.0, 500000);
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn output_delivery_authority_revocation_and_tenant_isolation(pool: PgPool) {
    for mode in 0..6 {
        let subject = format!("revoke-{mode}");
        let (f, turn, job, profile, _, d) = prepared(&pool, &subject).await;
        let a = d.check(&f.who, job).await.unwrap();
        let id = common::approval_id(&a);
        common::decide(&pool, &subject, id, common::approve(&["content"]))
            .await
            .unwrap();
        let other = setup(&pool).await;
        assert!(d.manifest(&other.who, job).await.is_err());
        match mode {
            0 => {
                f.m.cancel(&f.who, job).await.unwrap();
            }
            1 => {
                store::revoke_key(&pool, f.who.tenant_id, f.who.key_id)
                    .await
                    .unwrap();
            }
            2 => {
                governance::close_runtime(&pool, &f.who, turn, "completed")
                    .await
                    .unwrap();
            }
            3 => {
                scan::disable(&pool, f.who.tenant_id, profile.id)
                    .await
                    .unwrap();
            }
            4 => {
                sqlx::query("UPDATE models SET active=false WHERE tenant_id=$1")
                    .bind(f.who.tenant_id)
                    .execute(&pool)
                    .await
                    .unwrap();
            }
            _ => {
                sqlx::query("UPDATE runtime_bindings SET active=false WHERE tenant_id=$1")
                    .bind(f.who.tenant_id)
                    .execute(&pool)
                    .await
                    .unwrap();
            }
        }
        assert!(d.chunk(&f.who, job, 0).await.is_err());
        assert!(d.check(&f.who, job).await.is_err());
        assert_eq!(balances(&pool, &f.who).await.0, 500000);
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn output_delivery_transaction_faults_rollback_bindings_approvals_and_grants(pool: PgPool) {
    let (f, _, job, _, _, d) = prepared(&pool, "fault").await;
    sqlx::raw_sql("CREATE FUNCTION fail_delivery_approval() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'fixture failure'; END $$; CREATE TRIGGER fail_delivery_approval BEFORE INSERT ON approvals FOR EACH ROW EXECUTE FUNCTION fail_delivery_approval();").execute(&pool).await.unwrap();
    assert!(d.check(&f.who, job).await.is_err());
    assert_eq!(counts(&pool).await, (0, 0, 0));
    sqlx::query("DROP TRIGGER fail_delivery_approval ON approvals")
        .execute(&pool)
        .await
        .unwrap();
    let a = d.check(&f.who, job).await.unwrap();
    sqlx::raw_sql("CREATE TRIGGER fail_delivery_decision BEFORE UPDATE ON approvals FOR EACH ROW EXECUTE FUNCTION fail_delivery_approval();").execute(&pool).await.unwrap();
    assert!(
        common::decide(
            &pool,
            "fault",
            common::approval_id(&a),
            common::approve(&["content"])
        )
        .await
        .is_err()
    );
    assert_eq!(counts(&pool).await, (1, 1, 0));
    assert!(d.chunk(&f.who, job, 0).await.is_err());
    sqlx::query("DROP TRIGGER fail_delivery_decision ON approvals")
        .execute(&pool)
        .await
        .unwrap();
    common::decide(
        &pool,
        "fault",
        common::approval_id(&a),
        common::approve(&["content"]),
    )
    .await
    .unwrap();
    let a = Delivery::new(f.o.assets.clone())
        .check(&f.who, job)
        .await
        .unwrap();
    assert_eq!(a["state"], "ready");
    assert_eq!(f.r.posts.load(Ordering::SeqCst), 1);
    assert!(
        sqlx::query("UPDATE media_output_deliveries SET job_id=$1")
            .bind(Uuid::new_v4())
            .execute(&pool)
            .await
            .is_err()
    );
}

async fn scoped(pool: &PgPool, role: &'static str) -> PgPool {
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(3)
        .after_connect(move |c, _| {
            Box::pin(async move {
                sqlx::query(&format!("SET SESSION AUTHORIZATION {role}"))
                    .execute(c)
                    .await?;
                Ok(())
            })
        })
        .connect_with(pool.connect_options().as_ref().clone())
        .await
        .unwrap()
}
#[sqlx::test(migrations = "./migrations")]
async fn output_delivery_database_role_separates_reads_checks_and_human_decisions(pool: PgPool) {
    let (f, _, job, _, _, _) = prepared(&pool, "roles").await;
    let input =
        f.o.assets
            .create(
                &f.who,
                "delivery-hidden-input",
                assets::CreateAsset {
                    byte_length: 1,
                    sha256: common::hex(&store::hash(b"x")),
                    media_type: "application/octet-stream".into(),
                    ttl_seconds: 60,
                },
            )
            .await
            .unwrap();
    let input: Uuid = serde_json::from_value(input["id"].clone()).unwrap();
    sqlx::raw_sql(include_str!("../../deploy/permissions.sql"))
        .execute(&pool)
        .await
        .unwrap();
    let p = scoped(&pool, "brokerrouter_output_delivery").await;
    let d = Delivery::new(Assets::new(p.clone(), f.objects.clone(), common::cipher(), 2).unwrap());
    let a = d.check(&f.who, job).await.unwrap();
    let id = common::approval_id(&a);
    let proof = common::proof(&pool, "roles", "/review", &json!({})).await;
    d.review_chunk(&proof, id, 0).await.unwrap();
    assert!(
        common::decide(&p, "roles", id, common::approve(&["content"]))
            .await
            .is_err()
    );
    let control = scoped(&pool, "brokerrouter_governance_control").await;
    for role in [&p, &control] {
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM assets WHERE id=$1")
                .bind(input)
                .fetch_one(role)
                .await
                .unwrap(),
            0
        );
    }
    assert!(
        sqlx::query("SELECT * FROM asset_chunks")
            .execute(&control)
            .await
            .is_err()
    );
    common::decide(&control, "roles", id, common::approve(&["content"]))
        .await
        .unwrap();
    d.chunk(&f.who, job, 0).await.unwrap();
    for q in [
        "UPDATE assets SET state='deleted'",
        "UPDATE tenants SET spent_micros=0",
        "UPDATE approvals SET state='approved'",
        "DELETE FROM media_output_deliveries",
        "UPDATE output_scans SET state='closed'",
        "SET ROLE brokerrouter_governance_control",
        "SET ROLE brokerrouter_runtime",
        "SET ROLE brokerrouter_assets",
        "SET ROLE brokerrouter_output_scan",
        "SET ROLE brokerrouter_media_outputs",
    ] {
        assert!(sqlx::query(q).execute(&p).await.is_err(), "{q}");
    }
    for role in [
        "brokerrouter_runtime",
        "brokerrouter_assets",
        "brokerrouter_output_scan",
        "brokerrouter_governance_control",
    ] {
        let r = scoped(&pool, role).await;
        assert!(
            sqlx::query("SET ROLE brokerrouter_output_delivery")
                .execute(&r)
                .await
                .is_err()
        );
        assert!(sqlx::query("INSERT INTO media_output_deliveries(tenant_id,scan_id,job_id,operation_id) VALUES($1,$2,$3,$4)").bind(f.who.tenant_id).bind(Uuid::new_v4()).bind(job).bind(Uuid::new_v4()).execute(&r).await.is_err());
        r.close().await;
    }
    p.close().await;
    control.close().await;
}

#[sqlx::test(migrations = "./migrations")]
async fn output_delivery_http_human_proof_binary_headers_and_unforgeable_stage(pool: PgPool) {
    let (f, turn, job, _, _, d) = prepared(&pool, "http").await;
    let app = output_delivery_api::router(output_delivery_api::Service {
        delivery: d.clone(),
        verifier: Arc::new(common::verifier()),
    });
    let check = format!("/v1/media/jobs/{job}/output/check");
    let r = app
        .clone()
        .oneshot(
            Request::post(&check)
                .header("authorization", format!("Bearer {}", f.key))
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let a: Value =
        serde_json::from_slice(&r.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let id = common::approval_id(&a);
    let path = format!("/v1/approvals/{id}/output/review/chunks/0");
    human::add_user(&pool, f.who.tenant_id, common::ISSUER, "foreign-reviewer")
        .await
        .unwrap();
    let other = common::proof(&pool, "foreign-reviewer", &path, &json!({})).await;
    assert!(d.review_chunk(&other, id, 0).await.is_err());
    let jwt = common::sign(&common::claims(f.who.tenant_id, "http", &path, &json!({})));
    for (target, token) in [
        (path.clone(), f.key.clone()),
        (
            format!("/v1/approvals/{id}/output/review/chunks/1"),
            jwt.clone(),
        ),
    ] {
        let r = app
            .clone()
            .oneshot(
                Request::post(target)
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    }
    let r = app
        .clone()
        .oneshot(
            Request::post(&path)
                .header("authorization", format!("Bearer {jwt}"))
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.headers()["cache-control"], "private, no-store");
    assert_eq!(r.headers()["content-disposition"], "attachment");
    assert_eq!(r.headers()["content-type"], "application/octet-stream");
    assert_eq!(r.headers()["x-content-type-options"], "nosniff");
    assert_eq!(
        r.into_body().collect().await.unwrap().to_bytes(),
        f.r.data.slice(..CHUNK_BYTES)
    );
    let raw = governance::Check {
        turn_id: turn,
        operation_key: "forged".into(),
        stage: "media_output".into(),
        model_id: Some(f.e.model_id.clone()),
        tool_name: None,
        payload: json!({"scan_report":{"content":"pass"}}),
    };
    assert!(
        governance::check(&pool, &common::cipher(), &f.who, raw)
            .await
            .is_err()
    );
    assert!(
        f.o.assets
            .content_chunk(&f.who, asset_id(&pool, job).await, 0)
            .await
            .is_err()
    );
    common::decide(&pool, "http", id, common::approve(&["content"]))
        .await
        .unwrap();
    let path = format!("/v1/media/jobs/{job}/output/chunks/0");
    let r = app
        .clone()
        .oneshot(
            Request::get(&path)
                .header("authorization", format!("Bearer {}", f.key))
                .header("range", "bytes=0-3")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    let r = app
        .oneshot(
            Request::get(&path)
                .header("authorization", format!("Bearer {}", f.key))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(
        r.into_body().collect().await.unwrap().to_bytes(),
        f.r.data.slice(..CHUNK_BYTES)
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn output_delivery_rechecks_after_object_io_for_humans_and_agents(pool: PgPool) {
    for mode in 0..3 {
        let subject = format!("io-{mode}");
        let (f, _, job, _, _, d) = prepared(&pool, &subject).await;
        let a = d.check(&f.who, job).await.unwrap();
        let id = common::approval_id(&a);
        let proof = common::proof(&pool, &subject, "/review", &json!({})).await;
        if mode != 0 {
            common::decide(&pool, &subject, id, common::approve(&["content"]))
                .await
                .unwrap();
        }
        let paused = Arc::new(paused_store::Paused {
            inner: f.objects.clone(),
            entered: tokio::sync::Notify::new(),
            resume: tokio::sync::Notify::new(),
        });
        let d =
            Delivery::new(Assets::new(pool.clone(), paused.clone(), common::cipher(), 2).unwrap());
        let who = f.who.clone();
        let task = tokio::spawn(async move {
            if mode == 0 {
                d.review_chunk(&proof, id, 0).await
            } else {
                d.chunk(&who, job, 0).await
            }
        });
        tokio::time::timeout(Duration::from_secs(5), paused.entered.notified())
            .await
            .unwrap();
        if mode == 2 {
            // Revalidation succeeds, but an audit commit failure must still withhold bytes.
            sqlx::raw_sql("CREATE FUNCTION fail_delivery_audit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.action='output.chunk_served' THEN RAISE EXCEPTION 'fixture audit failure'; END IF; RETURN NEW; END $$; CREATE TRIGGER fail_delivery_audit BEFORE INSERT ON audit_events FOR EACH ROW EXECUTE FUNCTION fail_delivery_audit();").execute(&pool).await.unwrap();
        } else {
            f.m.cancel(&f.who, job).await.unwrap();
        }
        paused.resume.notify_one();
        assert!(task.await.unwrap().is_err());
        if mode == 2 {
            sqlx::query("DROP TRIGGER fail_delivery_audit ON audit_events")
                .execute(&pool)
                .await
                .unwrap();
        }
        assert_eq!(sqlx::query_scalar::<_,i64>("SELECT count(*) FROM audit_events WHERE object_id=$1 AND action IN ('output.chunk_served','output.review_chunk_served')").bind(job.to_string()).fetch_one(&pool).await.unwrap(),0);
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn output_delivery_closed_without_policy_or_trusted_turn_and_rules_are_strict(pool: PgPool) {
    let f = setup(&pool).await;
    let job = generate(&f, &pool, None).await;
    f.o.tick().await.unwrap();
    let (p, scanner, _) = scanner(&pool, &f, 1).await;
    let s = Scans::new(f.o.assets.clone());
    scan_done(&pool, &f, &scanner, &s, Content::Pass, Technical::Clean).await;
    let d = Delivery::new(f.o.assets.clone());
    assert!(d.check(&f.who, job).await.is_err());
    common::provision(&pool, &f.who, "strict").await;
    let turn = common::turn(&pool, &f.who, "strict", 0).await;
    let job = generate(&f, &pool, Some(turn)).await;
    f.o.tick().await.unwrap();
    scan_done(&pool, &f, &scanner, &s, Content::Pass, Technical::Clean).await;
    assert!(matches!(
        d.check(&f.who, job).await,
        Err(Error::OutputPolicyMissing)
    ));
    for (stage, rule) in [
        ("artifact", json!({"kind":"asset_scan","profile_id":p.id})),
        (
            "media_output",
            json!({"kind":"contains_any","case_sensitive":false,"terms":["risk"]}),
        ),
        (
            "media_output",
            json!({"kind":"asset_scan","profile_id":p.id,"fail_open":true}),
        ),
        (
            "media_output",
            json!({"kind":"asset_scan","profile_id":"bad"}),
        ),
    ] {
        assert!(
            human::publish_guardrail(
                &pool,
                f.who.tenant_id,
                governance::Guardrail {
                    id: "bad".into(),
                    name: "bad".into(),
                    version: 1,
                    stage: stage.into(),
                    rule
                }
            )
            .await
            .is_err()
        );
    }
    common::guardrail(
        &pool,
        f.who.tenant_id,
        "content",
        1,
        "media_output",
        json!({"kind":"asset_scan","profile_id":p.id}),
    )
    .await;
    assert_eq!(d.check(&f.who, job).await.unwrap()["state"], "ready");
    assert_eq!(counts(&pool).await, (1, 0, 0));
    d.chunk(&f.who, job, 0).await.unwrap();
    assert!(
        governance::claim(
            &pool,
            &common::cipher(),
            &f.who,
            serde_json::from_value(d.check(&f.who, job).await.unwrap()["operation_id"].clone())
                .unwrap(),
            serde_json::from_value(json!({"token":"11".repeat(32),"payload_hash":"00".repeat(32)}))
                .unwrap()
        )
        .await
        .is_err()
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn output_delivery_expiry_and_long_turn_approval_are_bounded(pool: PgPool) {
    for mode in 0..2 {
        let subject = format!("expiry-{mode}");
        let f = setup(&pool).await;
        common::provision(&pool, &f.who, &subject).await;
        let body = json!({"key_id":f.who.key_id,"session_id":"long","message_id":"one","run_id":"run","generation":0,"ttl_seconds":3600});
        let proof = common::proof(&pool, &subject, "/v1/governance/turns", &body).await;
        let v = human::create_turn(&pool, &proof, serde_json::from_value(body).unwrap())
            .await
            .unwrap();
        let turn = serde_json::from_value(v["id"].clone()).unwrap();
        let job = generate(&f, &pool, Some(turn)).await;
        f.o.tick().await.unwrap();
        let (p, scanner, _) = scanner(&pool, &f, 1).await;
        let s = Scans::new(f.o.assets.clone());
        scan_done(&pool, &f, &scanner, &s, Content::Unknown, Technical::Clean).await;
        common::guardrail(
            &pool,
            f.who.tenant_id,
            "content",
            1,
            "media_output",
            json!({"kind":"asset_scan","profile_id":p.id}),
        )
        .await;
        let d = Delivery::new(f.o.assets.clone());
        let a = d.check(&f.who, job).await.unwrap();
        assert_eq!(a["approval"]["expires_at"], v["expires_at"]);
        common::decide(
            &pool,
            &subject,
            common::approval_id(&a),
            common::approve(&["content"]),
        )
        .await
        .unwrap();
        d.manifest(&f.who, job).await.unwrap();
        // Isolated clock travel, never available to deployment roles.
        if mode == 0 {
            sqlx::raw_sql("ALTER TABLE governance_turns DISABLE TRIGGER turn_lifecycle_immutable; UPDATE governance_turns SET expires_at=created_at+interval '1 microsecond'; ALTER TABLE governance_turns ENABLE TRIGGER turn_lifecycle_immutable;").execute(&pool).await.unwrap();
        } else {
            sqlx::raw_sql("ALTER TABLE output_scans DISABLE TRIGGER output_scan_immutable; UPDATE output_scans SET expires_at=created_at+interval '1 microsecond'; ALTER TABLE output_scans ENABLE TRIGGER output_scan_immutable;").execute(&pool).await.unwrap();
        }
        assert!(d.chunk(&f.who, job, 0).await.is_err());
        assert!(d.check(&f.who, job).await.is_err());
        assert_eq!(balances(&pool, &f.who).await.0, 500000);
    }
}
