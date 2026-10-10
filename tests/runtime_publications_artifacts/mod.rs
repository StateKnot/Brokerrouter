//! Coupled publication at real PostgreSQL/service seams. Scan reports in these
//! tests are contract fixtures; real engine and native process tests are separate.
use super::*;
use brokerrouter::{
    approval_workbench,
    assets::Assets,
    media_review,
    output_delivery::Delivery as ReviewDelivery,
    output_scans::{self as scan, Report, Scans},
    runtime_artifact_delivery::Delivery,
    runtime_artifacts::{File, Manifest, Reserve, RuntimeArtifacts},
    runtime_results,
};
use bytes::Bytes;
use object_store::memory::InMemory;

struct Pair {
    runtime: RuntimeArtifacts,
    batch: Uuid,
    input: Uuid,
    token: String,
    assets: Vec<Uuid>,
    data: Vec<Bytes>,
}
impl Pair {
    fn text(&self) -> runtime_results::Record {
        runtime_results::Record {
            input_operation_id: self.input,
            claim_token: self.token.clone(),
            evidence_ref: format!("runtime://coupled/{}/{}", self.input, self.batch),
            text: "private original tool text".into(),
        }
    }
}

async fn pair(pool: &PgPool, f: &Fixture, seal: bool) -> Pair {
    brokerrouter::assets::set_quota(pool, f.who.tenant_id, None, 64 * 1024 * 1024)
        .await
        .unwrap();
    brokerrouter::assets::set_quota(pool, f.who.tenant_id, Some(f.who.key_id), 64 * 1024 * 1024)
        .await
        .unwrap();
    let op = governance::check(
        pool,
        &common::cipher(),
        &f.who,
        governance::Check {
            turn_id: f.turn,
            operation_key: "original-two-file-tool".into(),
            stage: "tool_input".into(),
            model_id: Some("model".into()),
            tool_name: Some("fixed_mp4_producer".into()),
            payload: json!({"duration_seconds":1}),
        },
    )
    .await
    .unwrap();
    let input = serde_json::from_value(op["id"].clone()).unwrap();
    let token = "22".repeat(32);
    governance::claim(
        pool,
        &common::cipher(),
        &f.who,
        input,
        governance::Claim {
            token: token.clone(),
            payload_hash: op["payload_hash"].as_str().unwrap().into(),
        },
    )
    .await
    .unwrap();
    let runtime = RuntimeArtifacts::new(
        Assets::new(pool.clone(), Arc::new(InMemory::new()), common::cipher(), 2).unwrap(),
        common::cipher(),
    );
    let data = vec![
        Bytes::from_static(b"private synthetic file A"),
        Bytes::from_static(b"private synthetic file B"),
    ];
    let b = runtime
        .reserve(
            &f.who,
            Reserve {
                input_operation_id: input,
                claim_token: token.clone(),
                input_payload_hash: op["payload_hash"].as_str().unwrap().into(),
                original_payload_hash: "33".repeat(32),
                maximum_bytes: 1024,
                maximum_artifacts: 2,
            },
        )
        .await
        .unwrap();
    let batch = serde_json::from_value(b["id"].clone()).unwrap();
    let manifest = runtime
        .record_manifest(
            &f.who,
            batch,
            Manifest {
                claim_token: token.clone(),
                evidence_ref: format!("runtime://coupled/{input}/{batch}"),
                artifacts: data
                    .iter()
                    .map(|bytes| File {
                        sha256: common::hex(&store::hash(bytes)),
                        byte_length: bytes.len() as i64,
                        media_type: "video/mp4".into(),
                        producer_revision: "44".repeat(32),
                    })
                    .collect(),
            },
        )
        .await
        .unwrap();
    let assets = manifest["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| serde_json::from_value(v["id"].clone()).unwrap())
        .collect();
    if seal {
        for (ordinal, bytes) in data.iter().enumerate() {
            runtime
                .put_chunk(&f.who, batch, ordinal as i32, 0, &token, bytes.clone())
                .await
                .unwrap();
        }
        runtime.complete(&f.who, batch, &token).await.unwrap();
    }
    Pair {
        runtime,
        batch,
        input,
        token,
        assets,
        data,
    }
}

async fn scan_pair(pool: &PgPool, f: &Fixture, p: &Pair) -> Delivery {
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
        chrono::Utc::now() + chrono::Duration::hours(1),
    )
    .await
    .unwrap();
    let scanner = scan::authenticate(pool, &token).await.unwrap();
    let scans = Scans::new(p.runtime.assets.clone());
    for _ in 0..2 {
        let a = scans
            .claim(&scanner, Uuid::new_v4())
            .await
            .unwrap()
            .unwrap();
        let id = serde_json::from_value(a["attempt_id"].clone()).unwrap();
        let stamp: chrono::DateTime<chrono::Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
            .fetch_one(pool)
            .await
            .unwrap();
        let report:Report=serde_json::from_value(json!({"sha256":a["sha256"],"byte_length":a["byte_length"],
            "profile_version":a["profile_version"],"engine_revision":a["engine_revision"],"technical":"clean",
            "content":"risk","media_type":"video/mp4","reason_codes":["fixture_risk"],
            "evidence_sha256":"66".repeat(32),"scanned_at":stamp})).unwrap();
        scans.report(&scanner, id, report).await.unwrap();
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
    Delivery::new(p.runtime.assets.clone())
}

async fn approve_file(pool: &PgPool, f: &Fixture, p: &Pair, d: &Delivery, index: usize) {
    let waiting = d.release(&f.who, p.assets[index]).await.unwrap();
    let approval = common::approval_id(&waiting);
    let proof = common::proof(pool, &f.subject, "/review", &json!({})).await;
    let session = Uuid::new_v4();
    let delivery = ReviewDelivery::new(p.runtime.assets.clone());
    let manifest = media_review::start(
        &delivery,
        &proof,
        media_review::Start {
            approval_id: approval,
            session_id: session,
        },
    )
    .await
    .unwrap();
    let bytes = media_review::chunk(
        &delivery,
        &proof,
        media_review::Chunk {
            session_id: session,
            ordinal: 0,
        },
    )
    .await
    .unwrap();
    assert_eq!(bytes, p.data[index]);
    media_review::complete(
        &delivery,
        &proof,
        media_review::Complete {
            session_id: session,
            sha256: manifest["output"]["sha256"].as_str().unwrap().into(),
            reviewed: true,
        },
    )
    .await
    .unwrap();
    approval_workbench::decide(
        pool,
        &common::cipher(),
        &proof,
        approval_workbench::Decide {
            approval_id: approval,
            review_id: session,
            acknowledged_scope: true,
            decision: serde_json::from_value(common::approve(&["native-content"])).unwrap(),
        },
    )
    .await
    .unwrap();
}

#[sqlx::test(migrations = "./migrations")]
async fn one_file_grant_cannot_release_original_tool_text_or_omit_second_file(pool: PgPool) {
    let f = setup(&pool).await;
    let p = pair(&pool, &f, true).await;
    let d = scan_pair(&pool, &f, &p).await;
    let saved = runtime_results::record(&pool, &common::cipher(), &f.who, p.text())
        .await
        .unwrap();
    approve_file(&pool, &f, &p, &d, 0).await;
    let id = serde_json::from_value(saved["id"].clone()).unwrap();
    let held = runtime_results::release(&f.gateway, &f.who, id)
        .await
        .unwrap();
    assert_ne!(
        held["state"], "ready",
        "one file grant released unapproved second file's original text"
    );
    assert!(held.get("text").is_none());
    assert_eq!(saved["artifact_set"]["files"].as_array().unwrap().len(), 2);
    assert!(d.manifest(&f.who, p.assets[1]).await.is_err());
    approve_file(&pool, &f, &p, &d, 1).await;
    let ready = runtime_results::release(&f.gateway, &f.who, id)
        .await
        .unwrap();
    assert_eq!(ready["text"], p.text().text);
    assert_eq!(ready["artifact_set"], saved["artifact_set"]);
    let retry = runtime_results::record(&pool, &common::cipher(), &f.who, p.text())
        .await
        .unwrap();
    assert_eq!(saved, retry);
    let facts:(i64,i64)=sqlx::query_as("SELECT (SELECT count(*) FROM runtime_tool_results),(SELECT count(*) FROM governance_operations WHERE stage='tool_input' AND state='completed')").fetch_one(&pool).await.unwrap();
    assert_eq!(facts, (1, 1));
    assert_eq!(f.calls.load(Ordering::SeqCst), 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn unsealed_original_batch_prevents_tool_text_registration(pool: PgPool) {
    let f = setup(&pool).await;
    let p = pair(&pool, &f, false).await;
    assert!(
        matches!(
            runtime_results::record(&pool, &common::cipher(), &f.who, p.text()).await,
            Err(Error::InProgress)
        ),
        "unsealed batch became a releasable original tool result"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM runtime_tool_results")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn final_text_cannot_silently_omit_a_sealed_batch_without_immutable_tool_text(pool: PgPool) {
    let f = setup(&pool).await;
    let _p = pair(&pool, &f, true).await;
    assert!(
        matches!(
            runtime_publications::record(&pool, &common::cipher(), &f.who, f.record()).await,
            Err(Error::InProgress)
        ),
        "final publication omitted the original artifact-bearing tool result"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM runtime_final_publications")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn final_release_rechecks_original_tool_output_permission_after_all_files_ready(
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
    let result = serde_json::from_value(saved["id"].clone()).unwrap();
    assert_eq!(
        runtime_results::release(&f.gateway, &f.who, result)
            .await
            .unwrap()["state"],
        "ready"
    );
    let final_record = runtime_publications::record(&pool, &common::cipher(), &f.who, f.record())
        .await
        .unwrap();
    let publication = serde_json::from_value(final_record["id"].clone()).unwrap();
    assert_eq!(
        runtime_publications::release(&f.gateway, &f.who, publication)
            .await
            .unwrap()["state"],
        "ready"
    );
    common::guardrail(
        &pool,
        f.who.tenant_id,
        "original-tool-output",
        1,
        "tool_output",
        json!({"kind":"always"}),
    )
    .await;
    let held = runtime_publications::release(&f.gateway, &f.who, publication)
        .await
        .unwrap();
    assert_ne!(
        held["state"], "ready",
        "final text bypassed original tool output's newly required approval"
    );
    assert!(held.get("text").is_none());
    assert_eq!(held["artifact_set"], saved["artifact_set"]);
    assert_eq!(
        f.calls.load(Ordering::SeqCst),
        1,
        "output authorization must not rerun the model"
    );
}

mod authorization;
mod binding;
