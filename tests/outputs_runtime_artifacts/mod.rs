//! Source-owned scan regressions. Synthetic reports here
//! test the gateway contract; actual detector certification is separate.
use super::*;
use brokerrouter::{
    governance::{self, Check},
    runtime_artifacts::{File, Manifest, Reserve, RuntimeArtifacts},
};

struct HttpScan {
    origin: String,
    server: tokio::task::JoinHandle<()>,
}
impl Drop for HttpScan {
    fn drop(&mut self) {
        self.server.abort();
    }
}
async fn serve(app: Router) -> HttpScan {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    HttpScan { origin, server }
}
fn private_headers(response: &reqwest::Response) {
    assert_eq!(response.headers()["cache-control"], "private, no-store");
    assert_eq!(response.headers()["x-content-type-options"], "nosniff");
}
async fn http_assignment(
    client: &reqwest::Client,
    service: &HttpScan,
    token: &str,
    claim: Uuid,
) -> Value {
    let response = client
        .post(format!("{}/v1/output-scans/claim", service.origin))
        .bearer_auth(token)
        .json(&json!({"claim_id":claim}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    private_headers(&response);
    response.json().await.unwrap()
}

pub(super) async fn native(
    pool: &PgPool,
    f: &Fixture,
    name: &str,
) -> (RuntimeArtifacts, Uuid, Uuid, Uuid) {
    let subject = format!("native-scan-{}", f.who.tenant_id);
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM governance_users WHERE tenant_id=$1 AND issuer=$2 AND subject=$3)",
    )
    .bind(f.who.tenant_id)
    .bind(common::ISSUER)
    .bind(&subject)
    .fetch_one(pool)
    .await
    .unwrap();
    if !exists {
        common::provision(pool, &f.who, &subject).await;
    }
    let turn = common::turn(pool, &f.who, &subject, 0).await;
    let check = governance::check(
        pool,
        &common::cipher(),
        &f.who,
        Check {
            turn_id: turn,
            operation_key: name.into(),
            stage: "tool_input".into(),
            model_id: Some(f.e.model_id.clone()),
            tool_name: Some("bounded_mp4_fixture".into()),
            payload: json!({"final":"arguments"}),
        },
    )
    .await
    .unwrap();
    let input: Uuid = serde_json::from_value(check["id"].clone()).unwrap();
    let token = common::hex(&store::hash(name.as_bytes()));
    governance::claim(
        pool,
        &common::cipher(),
        &f.who,
        input,
        governance::Claim {
            token: token.clone(),
            payload_hash: check["payload_hash"].as_str().unwrap().into(),
        },
    )
    .await
    .unwrap();
    let runtime = RuntimeArtifacts::new(f.o.assets.clone(), common::cipher());
    let reserve = Reserve {
        input_operation_id: input,
        claim_token: token.clone(),
        input_payload_hash: check["payload_hash"].as_str().unwrap().into(),
        original_payload_hash: common::hex(&store::hash(b"original-arguments")),
        maximum_bytes: f.r.data.len() as i64,
        maximum_artifacts: 1,
    };
    let batch: Uuid =
        serde_json::from_value(runtime.reserve(&f.who, reserve).await.unwrap()["id"].clone())
            .unwrap();
    let manifest = runtime
        .record_manifest(
            &f.who,
            batch,
            Manifest {
                claim_token: token.clone(),
                evidence_ref: format!("runtime://scan-fixture/{input}"),
                artifacts: vec![File {
                    sha256: common::hex(&store::hash(&f.r.data)),
                    byte_length: f.r.data.len() as i64,
                    media_type: "video/mp4".into(),
                    producer_revision: "33".repeat(32),
                }],
            },
        )
        .await
        .unwrap();
    let asset: Uuid = serde_json::from_value(manifest["artifacts"][0]["id"].clone()).unwrap();
    for (ordinal, chunk) in f.r.data.chunks(CHUNK_BYTES).enumerate() {
        runtime
            .put_chunk(
                &f.who,
                batch,
                0,
                ordinal as i32,
                &token,
                Bytes::copy_from_slice(chunk),
            )
            .await
            .unwrap();
    }
    runtime.complete(&f.who, batch, &token).await.unwrap();
    (runtime, batch, asset, input)
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires pinned local ClamAV/FFmpeg image; run explicitly for native artifact scanning"]
async fn native_real_mp4_scan_uses_three_engines_and_does_not_grant_delivery(pool: PgPool) {
    use brokerrouter::scanner_worker::Worker;
    let mut local = detector::Local::new("http://127.0.0.1/".into(), true);
    local.engine().preflight().await.unwrap();
    let f = setup_data(&pool, Bytes::from(local.video().await)).await;
    let (_, batch, asset, input) = native(&pool, &f, "native-real-mp4").await;
    let (token, server) = worker_setup(&pool, &f, &mut local).await;
    let worker = Worker::new(local.config.clone(), token).unwrap();
    let result = worker.once().await.unwrap();
    assert_eq!(result["technical"], "clean");
    assert_eq!(result["content"], "unknown");
    assert_eq!(result["delivery"], "not_released");
    let report: Value = sqlx::query_scalar(
        "SELECT report FROM output_scans WHERE runtime_asset_id=$1 AND state='complete'",
    )
    .bind(asset)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(report["sha256"], common::hex(&store::hash(&f.r.data)));
    assert_eq!(report["byte_length"], f.r.data.len());
    let evidence_bytes = std::fs::read(local.config.work_dir.join(format!(
        "evidence-{}.json",
        report["evidence_sha256"].as_str().unwrap()
    )))
    .unwrap();
    assert_eq!(
        report["evidence_sha256"],
        common::hex(&store::hash(&evidence_bytes))
    );
    let evidence: Value = serde_json::from_slice(&evidence_bytes).unwrap();
    assert_eq!(evidence["source"], "runtime_artifact");
    assert_eq!(evidence["input_operation_id"], input.to_string());
    assert_eq!(evidence["runtime_asset_id"], asset.to_string());
    assert_eq!(evidence["batch_id"], batch.to_string());
    assert!(evidence.get("job_id").is_none());
    assert_eq!(evidence["steps"].as_array().unwrap().len(), 3);
    assert!(
        evidence["steps"]
            .as_array()
            .unwrap()
            .iter()
            .all(|step| step["outcome"] == "passed")
    );
    let facts: (i64, i64, i64, i64) = sqlx::query_as("SELECT (SELECT count(*) FROM media_jobs),(SELECT count(*) FROM runtime_artifact_deliveries),(SELECT count(*) FROM runtime_artifact_grants),(SELECT count(*) FROM approvals)").fetch_one(&pool).await.unwrap();
    assert_eq!(facts, (0, 0, 0, 0));
    assert_eq!(f.r.posts.load(Ordering::SeqCst), 0);
    assert!(!local.config.work_dir.join("input").exists());
    server.abort();
}

#[sqlx::test(migrations = "./migrations")]
async fn native_scan_keeps_real_source_and_exact_bytes_without_fake_media_or_grant(pool: PgPool) {
    let f = setup(&pool).await;
    let (_, batch, asset, input) = native(&pool, &f, "native").await;
    let (_, who, _) = scanner(&pool, &f, 1).await;
    let scans = Scans::new(f.o.assets.clone());
    let claim = Uuid::new_v4();
    let assigned = scans.claim(&who, claim).await.unwrap().unwrap();
    assert_eq!(assigned["source"], "runtime_artifact");
    assert!(assigned.get("job_id").is_none());
    assert_eq!(assigned["runtime_asset_id"], asset.to_string());
    assert_eq!(assigned["asset_id"], asset.to_string());
    assert_eq!(assigned["batch_id"], batch.to_string());
    assert_eq!(assigned["input_operation_id"], input.to_string());
    assert_eq!(assigned["ordinal"], 0);
    assert_eq!(assigned["producer_revision"], "33".repeat(32));
    let attempt: Uuid = serde_json::from_value(assigned["attempt_id"].clone()).unwrap();
    assert_eq!(scans.claim(&who, claim).await.unwrap().unwrap(), assigned);
    let mut actual = Vec::new();
    for ordinal in 0..assigned["chunk_count"].as_i64().unwrap() {
        actual.extend_from_slice(&scans.chunk(&who, attempt, ordinal as i32).await.unwrap());
    }
    assert_eq!(actual.as_slice(), f.r.data.as_ref());
    let report = scan_report(&pool, &assigned).await;
    let complete = scans.report(&who, attempt, report.clone()).await.unwrap();
    assert_eq!(complete["state"], "complete");
    assert_eq!(scans.report(&who, attempt, report).await.unwrap(), complete);
    assert_eq!(complete["approval"], "not_granted");
    let facts: (i64, i64, i64, i64) = sqlx::query_as("SELECT (SELECT count(*) FROM media_jobs),(SELECT count(*) FROM runtime_artifact_deliveries),(SELECT count(*) FROM runtime_artifact_grants),(SELECT count(*) FROM governance_operations WHERE state='completed')").fetch_one(&pool).await.unwrap();
    assert_eq!(facts, (0, 0, 0, 1));
    assert_eq!(f.r.posts.load(Ordering::SeqCst), 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn native_and_legacy_share_token_limits_and_source_assignment_remains_fair(pool: PgPool) {
    let f = setup_data(&pool, Bytes::from_static(b"source limit contract fixture")).await;
    // The legacy candidate comes through the real fixture provider/import flow,
    // independently of native artifacts; no native source is assigned a job ID.
    let job = generate(&f, &pool, None).await;
    f.o.tick().await.unwrap();
    native(&pool, &f, "native-one").await;
    let (_, who, _) = scanner(&pool, &f, 1).await;
    let scans = Scans::new(f.o.assets.clone());
    let first = scans.claim(&who, Uuid::new_v4()).await.unwrap().unwrap();
    let second = scans.claim(&who, Uuid::new_v4()).await.unwrap().unwrap();
    let sources = [
        first.get("job_id").is_some(),
        second.get("job_id").is_some(),
    ];
    assert_eq!(sources.iter().filter(|&&legacy| legacy).count(), 1);
    let legacy = if sources[0] { &first } else { &second };
    assert_eq!(legacy["job_id"], job.to_string());
    assert!(legacy.get("source").is_none());
    native(&pool, &f, "native-two").await;
    assert!(matches!(
        scans.claim(&who, Uuid::new_v4()).await,
        Err(Error::Busy)
    ));
    assert_eq!(f.r.posts.load(Ordering::SeqCst), 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn concurrent_native_claim_has_one_owner_and_profile_upgrade_fences_original_bytes(
    pool: PgPool,
) {
    let f = setup(&pool).await;
    native(&pool, &f, "native").await;
    let (_, who, old_token) = scanner(&pool, &f, 1).await;
    let scans = Scans::new(f.o.assets.clone());
    let claim = Uuid::new_v4();
    let (left, right) = tokio::join!(scans.claim(&who, claim), scans.claim(&who, claim));
    let left = left.unwrap().unwrap();
    assert_eq!(Some(left.clone()), right.unwrap());
    let attempt: Uuid = serde_json::from_value(left["attempt_id"].clone()).unwrap();
    let (_, next, _) = scanner(&pool, &f, 2).await;
    assert!(scan::authenticate(&pool, &old_token).await.is_err());
    assert!(scans.chunk(&who, attempt, 0).await.is_err());
    assert!(
        scans
            .report(&who, attempt, scan_report(&pool, &left).await)
            .await
            .is_err()
    );
    let current = scans.claim(&next, Uuid::new_v4()).await.unwrap().unwrap();
    assert_eq!(current["asset_id"], left["asset_id"]);
    assert_ne!(current["scan_id"], left["scan_id"]);
    assert_eq!(current["profile_version"], 2);
    assert_eq!(f.r.posts.load(Ordering::SeqCst), 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn native_scanner_role_reads_source_only_and_cannot_publish_or_approve(pool: PgPool) {
    let f = setup(&pool).await;
    native(&pool, &f, "native").await;
    let (_, who, _) = scanner(&pool, &f, 1).await;
    let hidden =
        f.o.assets
            .create(
                &f.who,
                "hidden-input",
                assets::CreateAsset {
                    byte_length: 1,
                    sha256: common::hex(&store::hash(b"a")),
                    media_type: "application/octet-stream".into(),
                    ttl_seconds: 60,
                },
            )
            .await
            .unwrap();
    sqlx::raw_sql(include_str!("../../deploy/permissions.sql"))
        .execute(&pool)
        .await
        .unwrap();
    let restricted = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .after_connect(|c, _| {
            Box::pin(async move {
                sqlx::query("SET SESSION AUTHORIZATION brokerrouter_output_scan")
                    .execute(c)
                    .await?;
                Ok(())
            })
        })
        .connect_with(pool.connect_options().as_ref().clone())
        .await
        .unwrap();
    let scans = Scans::new(
        Assets::new(restricted.clone(), f.objects.clone(), common::cipher(), 2).unwrap(),
    );
    let assigned = scans.claim(&who, Uuid::new_v4()).await.unwrap().unwrap();
    let attempt: Uuid = serde_json::from_value(assigned["attempt_id"].clone()).unwrap();
    assert_eq!(
        scans.chunk(&who, attempt, 0).await.unwrap(),
        f.r.data.slice(..CHUNK_BYTES)
    );
    let hidden: Uuid = serde_json::from_value(hidden["id"].clone()).unwrap();
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM assets WHERE id=$1")
        .bind(hidden)
        .fetch_one(&restricted)
        .await
        .unwrap();
    assert_eq!(count, 0);
    for sql in [
        "UPDATE approvals SET state='approved'",
        "INSERT INTO runtime_artifact_grants SELECT * FROM runtime_artifact_grants",
        "INSERT INTO runtime_artifact_deliveries SELECT * FROM runtime_artifact_deliveries",
        "UPDATE runtime_artifact_batches SET state='expired'",
    ] {
        let error = sqlx::query(sql).execute(&restricted).await.unwrap_err();
        assert_eq!(
            error.as_database_error().and_then(|e| e.code()).as_deref(),
            Some("42501")
        );
    }
    restricted.close().await;
}

#[sqlx::test(migrations = "./migrations")]
async fn native_scan_token_expiring_during_final_audit_returns_no_bytes(pool: PgPool) {
    let f = setup(&pool).await;
    native(&pool, &f, "native").await;
    let (profile, _, _) = scanner(&pool, &f, 1).await;
    let expires = sqlx::query_scalar("SELECT clock_timestamp()+interval '4 seconds'")
        .fetch_one(&pool)
        .await
        .unwrap();
    let (token_id, token) =
        scan::create_token(&pool, f.who.tenant_id, profile.id, "deadline", expires)
            .await
            .unwrap();
    let who = scan::authenticate(&pool, &token).await.unwrap();
    let scans = Scans::new(f.o.assets.clone());
    let assigned = scans.claim(&who, Uuid::new_v4()).await.unwrap().unwrap();
    let attempt: Uuid = serde_json::from_value(assigned["attempt_id"].clone()).unwrap();
    sqlx::raw_sql("CREATE SEQUENCE scan_deadline_audit_seen; CREATE FUNCTION delay_native_scan_audit() RETURNS trigger LANGUAGE plpgsql AS $$ DECLARE until_at timestamptz; BEGIN IF NEW.action='output_scan.chunk_read' THEN PERFORM nextval('scan_deadline_audit_seen'); SELECT expires_at INTO until_at FROM output_scanner_tokens WHERE id=(NEW.details->>'token_id')::uuid; PERFORM pg_sleep(GREATEST(0,extract(epoch FROM until_at-clock_timestamp()))+0.05); END IF; RETURN NEW; END; $$; CREATE TRIGGER delay_native_scan_audit BEFORE INSERT ON audit_events FOR EACH ROW EXECUTE FUNCTION delay_native_scan_audit();").execute(&pool).await.unwrap();
    assert!(matches!(
        scans.chunk(&who, attempt, 0).await,
        Err(Error::ScannerUnauthorized)
    ));
    let seen: bool = sqlx::query_scalar("SELECT is_called FROM scan_deadline_audit_seen")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(seen);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM audit_events WHERE action='output_scan.chunk_read'"
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM output_scanner_tokens WHERE id=$1")
            .bind(token_id)
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn native_scan_http_cannot_be_read_or_approved_through_input_or_legacy_delivery(
    pool: PgPool,
) {
    let f = setup_data(
        &pool,
        Bytes::from_static(b"quarantined native source bytes"),
    )
    .await;
    let (_, batch, asset, _) = native(&pool, &f, "native-http").await;
    let (_, _, token) = scanner(&pool, &f, 1).await;
    let client = reqwest::Client::new();
    let service = serve(brokerrouter::output_scan_api::router(Scans::new(
        f.o.assets.clone(),
    )))
    .await;
    let assigned = http_assignment(&client, &service, &token, Uuid::new_v4()).await;
    let attempt = attempt_id(&assigned);
    let response = client
        .post(format!(
            "{}/v1/output-scans/attempts/{attempt}/report",
            service.origin
        ))
        .bearer_auth(&token)
        .json(&scan_report(&pool, &assigned).await)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.json::<Value>().await.unwrap()["approval"],
        "not_granted"
    );
    let input = serve(asset_api::router(f.o.assets.clone())).await;
    for suffix in ["", "/chunks/0", "/content"] {
        let response = client
            .get(format!("{}/v1/assets/{asset}{suffix}", input.origin))
            .bearer_auth(&f.key)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        private_headers(&response);
        assert!(
            !response
                .bytes()
                .await
                .unwrap()
                .windows(f.r.data.len())
                .any(|p| p == f.r.data.as_ref())
        );
    }
    let legacy = serve(brokerrouter::output_delivery_api::router(
        brokerrouter::output_delivery_api::Service {
            delivery: brokerrouter::output_delivery::Delivery::new(f.o.assets.clone()),
            verifier: Arc::new(common::verifier()),
        },
    ))
    .await;
    let scan: Uuid = serde_json::from_value(assigned["scan_id"].clone()).unwrap();
    for id in [asset, batch, scan] {
        for suffix in ["", "/chunks/0"] {
            let response = client
                .get(format!(
                    "{}/v1/media/jobs/{id}/output{suffix}",
                    legacy.origin
                ))
                .bearer_auth(&f.key)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::GONE);
            private_headers(&response);
        }
        let response = client
            .post(format!("{}/v1/media/jobs/{id}/output/check", legacy.origin))
            .bearer_auth(&f.key)
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::GONE);
    }
    assert_eq!(
        sqlx::query_as::<_, (i64, i64, i64)>("SELECT (SELECT count(*) FROM approvals),(SELECT count(*) FROM runtime_artifact_grants),(SELECT count(*) FROM media_output_deliveries)")
            .fetch_one(&pool)
            .await
            .unwrap(),
        (0, 0, 0)
    );
    assert_eq!(f.r.posts.load(Ordering::SeqCst), 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn native_scan_http_credential_source_profile_and_claim_binding_is_not_caller_supplied(
    pool: PgPool,
) {
    let f = setup_data(&pool, Bytes::from_static(b"scanner-only native bytes")).await;
    native(&pool, &f, "native-http").await;
    let (profile, _, token) = scanner(&pool, &f, 1).await;
    let (_, sibling_token) = scan::create_token(
        &pool,
        f.who.tenant_id,
        profile.id,
        "same profile different owner",
        chrono::Utc::now() + chrono::Duration::hours(1),
    )
    .await
    .unwrap();
    store::put_model(&pool, f.who.tenant_id, "other/model", "Other")
        .await
        .unwrap();
    let other_profile = scan::Profile {
        tenant_id: f.who.tenant_id,
        id: Uuid::new_v4(),
        model_id: "other/model".into(),
        version: 1,
        engine_revision: profile.engine_revision.clone(),
    };
    scan::publish(&pool, other_profile.clone()).await.unwrap();
    let (_, other_profile_token) = scan::create_token(
        &pool,
        f.who.tenant_id,
        other_profile.id,
        "different profile",
        chrono::Utc::now() + chrono::Duration::hours(1),
    )
    .await
    .unwrap();
    let other = setup_data(&pool, Bytes::from_static(b"other tenant bytes")).await;
    let (_, _, other_tenant_token) = scanner(&pool, &other, 1).await;
    let service = serve(brokerrouter::output_scan_api::router(Scans::new(
        f.o.assets.clone(),
    )))
    .await;
    let client = reqwest::Client::new();
    for credential in [&f.key, "not-a-scanner"] {
        let response = client
            .post(format!("{}/v1/output-scans/claim", service.origin))
            .bearer_auth(credential)
            .json(&json!({"claim_id":Uuid::new_v4()}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        private_headers(&response);
    }
    for field in [
        "tenant_id",
        "asset_id",
        "job_id",
        "runtime_asset_id",
        "profile_id",
    ] {
        let mut body = json!({"claim_id":Uuid::new_v4()});
        body[field] = json!(Uuid::new_v4());
        let response = client
            .post(format!("{}/v1/output-scans/claim", service.origin))
            .bearer_auth(&token)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }
    let claim = Uuid::new_v4();
    let assignment = http_assignment(&client, &service, &token, claim).await;
    assert_eq!(
        http_assignment(&client, &service, &token, claim).await,
        assignment
    );
    let attempt = attempt_id(&assignment);
    let chunk = format!(
        "{}/v1/output-scans/attempts/{attempt}/chunks/0",
        service.origin
    );
    for credential in [&sibling_token, &other_profile_token, &other_tenant_token] {
        let response = client
            .get(&chunk)
            .bearer_auth(credential)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        private_headers(&response);
    }
    assert_eq!(
        http_assignment(&client, &service, &other_profile_token, Uuid::new_v4()).await["state"],
        "no_assignment"
    );
    let response = client
        .get(&chunk)
        .bearer_auth(&token)
        .header("range", "bytes=0-1")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let response = client.get(&chunk).bearer_auth(&token).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    private_headers(&response);
    assert_eq!(
        response.headers()["content-length"],
        f.r.data.len().to_string()
    );
    assert_eq!(response.bytes().await.unwrap(), f.r.data);
    let mut report = serde_json::to_value(scan_report(&pool, &assignment).await).unwrap();
    report["approved"] = json!(true);
    let response = client
        .post(format!(
            "{}/v1/output-scans/attempts/{attempt}/report",
            service.origin
        ))
        .bearer_auth(&token)
        .json(&report)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(f.r.posts.load(Ordering::SeqCst), 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn native_scan_http_profile_upgrade_and_revocation_block_bytes_before_dispatch(pool: PgPool) {
    let f = setup_data(&pool, Bytes::from_static(b"profile-owned native bytes")).await;
    native(&pool, &f, "native-http").await;
    let (_, _, token) = scanner(&pool, &f, 1).await;
    let service = serve(brokerrouter::output_scan_api::router(Scans::new(
        f.o.assets.clone(),
    )))
    .await;
    let client = reqwest::Client::new();
    let old = http_assignment(&client, &service, &token, Uuid::new_v4()).await;
    let old_attempt = attempt_id(&old);
    let old_url = format!(
        "{}/v1/output-scans/attempts/{old_attempt}/chunks/0",
        service.origin
    );
    let (current_profile, _, current_token) = scanner(&pool, &f, 2).await;
    let response = client
        .get(&old_url)
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    private_headers(&response);
    let response = client
        .get(&old_url)
        .bearer_auth(&current_token)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let current = http_assignment(&client, &service, &current_token, Uuid::new_v4()).await;
    assert_eq!(current["asset_id"], old["asset_id"]);
    assert_ne!(current["scan_id"], old["scan_id"]);
    let current_attempt = attempt_id(&current);
    scan::disable(&pool, f.who.tenant_id, current_profile.id)
        .await
        .unwrap();
    for suffix in ["/chunks/0", "/chunks/1"] {
        let response = client
            .get(format!(
                "{}/v1/output-scans/attempts/{current_attempt}{suffix}",
                service.origin
            ))
            .bearer_auth(&current_token)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
    let response = client
        .post(format!(
            "{}/v1/output-scans/attempts/{current_attempt}/report",
            service.origin
        ))
        .bearer_auth(&current_token)
        .json(&scan_report(&pool, &current).await)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(f.r.posts.load(Ordering::SeqCst), 0);
}
