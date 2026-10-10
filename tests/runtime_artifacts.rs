mod common;
use brokerrouter::{
    assets::{self, Assets, CHUNK_BYTES, CreateAsset},
    error::Error,
    governance::{self, Check},
    runtime_artifacts::{File, MAX_BYTES, Manifest, Reserve, RuntimeArtifacts},
    store,
};
use bytes::Bytes;
use object_store::{ObjectStoreExt, memory::InMemory, path::Path};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use uuid::Uuid;
#[path = "common/write_fault.rs"]
mod write_fault;

async fn fixture(pool: &PgPool) -> (store::Identity, Uuid, RuntimeArtifacts, Arc<InMemory>) {
    let tenant = store::create_tenant(pool, "runtime-artifacts", 1_000_000)
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
    let subject = format!("alice-{tenant}");
    common::provision(pool, &who, &subject).await;
    let turn = common::turn(pool, &who, &subject, 0).await;
    assets::set_quota(pool, tenant, None, MAX_BYTES * 2)
        .await
        .unwrap();
    assets::set_quota(pool, tenant, Some(key_id), MAX_BYTES * 2)
        .await
        .unwrap();
    let objects = Arc::new(InMemory::new());
    let a = RuntimeArtifacts::new(
        Assets::new(pool.clone(), objects.clone(), common::cipher(), 2).unwrap(),
        common::cipher(),
    );
    (who, turn, a, objects)
}
async fn claimed(pool: &PgPool, who: &store::Identity, turn: Uuid, key: &str) -> Reserve {
    let check = governance::check(
        pool,
        &common::cipher(),
        who,
        Check {
            turn_id: turn,
            operation_key: key.into(),
            stage: "tool_input".into(),
            model_id: Some("model".into()),
            tool_name: Some("fixture_tool".into()),
            payload: json!({"final":"arguments"}),
        },
    )
    .await
    .unwrap();
    let input_operation_id = id(&check);
    let token = "11".repeat(32);
    governance::claim(
        pool,
        &common::cipher(),
        who,
        input_operation_id,
        governance::Claim {
            token: token.clone(),
            payload_hash: check["payload_hash"].as_str().unwrap().into(),
        },
    )
    .await
    .unwrap();
    Reserve {
        input_operation_id,
        claim_token: token,
        input_payload_hash: check["payload_hash"].as_str().unwrap().into(),
        original_payload_hash: "22".repeat(32),
        maximum_bytes: MAX_BYTES,
        maximum_artifacts: 8,
    }
}
fn manifest(r: &Reserve, bytes: &[u8]) -> Manifest {
    Manifest {
        claim_token: r.claim_token.clone(),
        evidence_ref: format!("runtime://fixture/{}", r.input_operation_id),
        artifacts: vec![File {
            sha256: common::hex(&store::hash(bytes)),
            byte_length: bytes.len() as i64,
            media_type: "video/mp4".into(),
            producer_revision: "33".repeat(32),
        }],
    }
}
fn id(v: &Value) -> Uuid {
    serde_json::from_value(v["id"].clone()).unwrap()
}
fn asset(v: &Value) -> Uuid {
    id(&v["artifacts"][0])
}
async fn used(pool: &PgPool, who: &store::Identity) -> (i64, i64) {
    sqlx::query_as("SELECT t.asset_used_bytes,k.asset_used_bytes FROM tenants t JOIN virtual_keys k ON k.tenant_id=t.id WHERE t.id=$1 AND k.id=$2").bind(who.tenant_id).bind(who.key_id).fetch_one(pool).await.unwrap()
}
async fn path(pool: &PgPool, who: &store::Identity, asset: Uuid, ordinal: i32) -> Path {
    let c: Uuid = sqlx::query_scalar(
        "SELECT id FROM asset_chunks WHERE tenant_id=$1 AND asset_id=$2 AND ordinal=$3",
    )
    .bind(who.tenant_id)
    .bind(asset)
    .bind(ordinal)
    .fetch_one(pool)
    .await
    .unwrap();
    Path::from(format!("assets-v1/{}/{}/{}", who.tenant_id, asset, c))
}

#[sqlx::test(migrations = "./migrations")]
async fn original_claim_reservation_is_atomic_immutable_and_has_no_fake_media_or_input(
    pool: PgPool,
) {
    let (who, turn, a, _) = fixture(&pool).await;
    let r = claimed(&pool, &who, turn, "once").await;
    let (one, two) = tokio::join!(a.reserve(&who, r.clone()), a.reserve(&who, r.clone()));
    let one = one.unwrap();
    assert_eq!(one, two.unwrap());
    assert_eq!(one["state"], "reserved");
    assert_eq!(used(&pool, &who).await, (MAX_BYTES, MAX_BYTES));
    let counts:(i64,i64,i64)=sqlx::query_as("SELECT (SELECT count(*) FROM runtime_artifact_batches),(SELECT count(*) FROM media_jobs),(SELECT count(*) FROM assets)").fetch_one(&pool).await.unwrap();
    assert_eq!(counts, (1, 0, 0));
    let mut drift = r.clone();
    drift.original_payload_hash = "44".repeat(32);
    assert!(matches!(a.reserve(&who, drift).await, Err(Error::Conflict)));
    let mut drift = r.clone();
    drift.maximum_bytes -= 1;
    assert!(matches!(a.reserve(&who, drift).await, Err(Error::Conflict)));
    let mut drift = r.clone();
    drift.claim_token = "55".repeat(32);
    assert!(matches!(a.reserve(&who, drift).await, Err(Error::NotFound)));
    let mut drift = r;
    drift.input_payload_hash = "66".repeat(32);
    assert!(matches!(a.reserve(&who, drift).await, Err(Error::Conflict)));
    let mutation=sqlx::query("UPDATE runtime_artifact_batches SET original_payload_hash=decode(repeat('77',32),'hex') WHERE id=$1").bind(id(&one)).execute(&pool).await;
    assert!(mutation.is_err());
    assert!(
        sqlx::query("DELETE FROM runtime_artifact_batches")
            .execute(&pool)
            .await
            .is_err()
    );
    assert_eq!(used(&pool, &who).await, (MAX_BYTES, MAX_BYTES));
}

#[sqlx::test(migrations = "./migrations")]
async fn manifest_receipt_and_actual_encrypted_bytes_remain_quarantined_and_replay_verifies_storage(
    pool: PgPool,
) {
    let (who, turn, a, objects) = fixture(&pool).await;
    let r = claimed(&pool, &who, turn, "native").await;
    let b = id(&a.reserve(&who, r.clone()).await.unwrap());
    let bytes = b"zero-fee candidate bytes: not certified MP4";
    let m = manifest(&r, bytes);
    let (first, retry) = tokio::join!(
        a.record_manifest(&who, b, m.clone()),
        a.record_manifest(&who, b, m.clone())
    );
    let first = first.unwrap();
    assert_eq!(first, retry.unwrap());
    let asset = asset(&first);
    assert_eq!(
        used(&pool, &who).await,
        (bytes.len() as i64, bytes.len() as i64)
    );
    let facts:(String,i64,i64)=sqlx::query_as("SELECT state,(SELECT count(*) FROM runtime_artifacts),(SELECT count(*) FROM governance_events WHERE event_type='operation.receipted') FROM governance_operations WHERE tenant_id=$1 AND id=$2").bind(who.tenant_id).bind(r.input_operation_id).fetch_one(&pool).await.unwrap();
    assert_eq!(facts, ("completed".into(), 1, 1));
    assert!(matches!(
        a.complete(&who, b, &r.claim_token).await,
        Err(Error::InProgress)
    ));
    a.put_chunk(&who, b, 0, 0, &r.claim_token, Bytes::from_static(bytes))
        .await
        .unwrap();
    let p = path(&pool, &who, asset, 0).await;
    let encrypted = objects.get(&p).await.unwrap().bytes().await.unwrap();
    assert!(!encrypted.windows(bytes.len()).any(|v| v == bytes));
    let complete = a.complete(&who, b, &r.claim_token).await.unwrap();
    assert_eq!(complete["state"], "quarantined");
    assert_eq!(complete["content_safety"], "not_evaluated");
    assert!(complete.get("content").is_none());
    assert_eq!(a.complete(&who, b, &r.claim_token).await.unwrap(), complete);
    assert!(matches!(
        a.assets.metadata(&who, asset).await,
        Err(Error::NotFound)
    ));
    assert!(matches!(
        a.assets.content_chunk(&who, asset, 0).await,
        Err(Error::NotFound)
    ));
    assert!(matches!(
        a.assets.complete(&who, asset).await,
        Err(Error::NotFound)
    ));
    assert!(matches!(
        a.assets
            .put_chunk(&who, asset, 0, Bytes::from_static(bytes))
            .await,
        Err(Error::NotFound)
    ));
    objects
        .put(&p, Bytes::from_static(b"corrupt").into())
        .await
        .unwrap();
    assert!(matches!(
        a.complete(&who, b, &r.claim_token).await,
        Err(Error::Storage)
    ));
    objects.delete(&p).await.unwrap();
    assert!(matches!(
        a.complete(&who, b, &r.claim_token).await,
        Err(Error::Storage)
    ));
    assert_eq!(
        used(&pool, &who).await,
        (bytes.len() as i64, bytes.len() as i64)
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn lost_object_response_recovers_only_exact_bytes_and_does_not_repeat_acknowledged_put(
    pool: PgPool,
) {
    let (who, turn, base, objects) = fixture(&pool).await;
    let fault = Arc::new(write_fault::Fault {
        inner: objects.clone(),
        mode: AtomicUsize::new(1),
        puts: AtomicUsize::new(0),
    });
    let a = RuntimeArtifacts::new(
        Assets::new(pool.clone(), fault.clone(), common::cipher(), 2).unwrap(),
        common::cipher(),
    );
    let r = claimed(&pool, &who, turn, "lost-put").await;
    let b = id(&a.reserve(&who, r.clone()).await.unwrap());
    let m = manifest(&r, b"abc");
    let first = a.record_manifest(&who, b, m.clone()).await.unwrap();
    a.put_chunk(&who, b, 0, 0, &r.claim_token, Bytes::from_static(b"abc"))
        .await
        .unwrap();
    assert_eq!(fault.puts.load(Ordering::SeqCst), 1);
    let reopened = RuntimeArtifacts::new(base.assets, common::cipher());
    assert_eq!(reopened.record_manifest(&who, b, m).await.unwrap(), first);
    a.put_chunk(&who, b, 0, 0, &r.claim_token, Bytes::from_static(b"abc"))
        .await
        .unwrap();
    assert_eq!(fault.puts.load(Ordering::SeqCst), 1);
    assert_eq!(
        reopened.complete(&who, b, &r.claim_token).await.unwrap()["state"],
        "quarantined"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM governance_operations WHERE state='completed'"
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        1
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn real_full_file_hash_and_exact_chunk_manifest_prevent_content_replacement(pool: PgPool) {
    let (who, turn, a, _) = fixture(&pool).await;
    let r = claimed(&pool, &who, turn, "hash").await;
    let b = id(&a.reserve(&who, r.clone()).await.unwrap());
    let bytes = vec![b'X'; CHUNK_BYTES + 1];
    let m = manifest(&r, &bytes);
    a.record_manifest(&who, b, m.clone()).await.unwrap();
    assert!(
        a.put_chunk(&who, b, 0, 0, &r.claim_token, Bytes::from_static(b"X"))
            .await
            .is_err()
    );
    a.put_chunk(
        &who,
        b,
        0,
        0,
        &r.claim_token,
        Bytes::from(vec![b'X'; CHUNK_BYTES]),
    )
    .await
    .unwrap();
    a.put_chunk(&who, b, 0, 1, &r.claim_token, Bytes::from_static(b"Y"))
        .await
        .unwrap();
    assert!(matches!(
        a.complete(&who, b, &r.claim_token).await,
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        a.put_chunk(&who, b, 0, 1, &r.claim_token, Bytes::from_static(b"X"))
            .await,
        Err(Error::Conflict)
    ));
    let mut changed = m.clone();
    changed.artifacts[0].sha256 = common::hex(&store::hash(b"different"));
    assert!(matches!(
        a.record_manifest(&who, b, changed).await,
        Err(Error::Conflict)
    ));
    let mut changed = m;
    changed.artifacts[0].producer_revision = "44".repeat(32);
    assert!(matches!(
        a.record_manifest(&who, b, changed).await,
        Err(Error::Conflict)
    ));
    assert_eq!(
        a.metadata(&who, b, &r.claim_token).await.unwrap()["state"],
        "capturing"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn capacity_failure_rolls_back_receipt_and_reserved_namespace_cannot_be_input(pool: PgPool) {
    let (who, turn, a, _) = fixture(&pool).await;
    let r = claimed(&pool, &who, turn, "small").await;
    let mut reserve = r.clone();
    reserve.maximum_bytes = 2;
    reserve.maximum_artifacts = 1;
    let b = id(&a.reserve(&who, reserve).await.unwrap());
    assert!(matches!(
        a.record_manifest(&who, b, manifest(&r, b"abc")).await,
        Err(Error::AssetQuota)
    ));
    let op: (String, bool) =
        sqlx::query_as("SELECT state,receipt_hash IS NULL FROM governance_operations WHERE id=$1")
            .bind(r.input_operation_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(op, ("executing".into(), true));
    assert_eq!(used(&pool, &who).await, (2, 2));
    let body = CreateAsset {
        byte_length: 3,
        sha256: common::hex(&store::hash(b"abc")),
        media_type: "video/mp4".into(),
        ttl_seconds: 3600,
    };
    assert!(matches!(
        a.assets
            .create(&who, "_br_runtime_artifact:fake", body)
            .await,
        Err(Error::Invalid(_))
    ));
    let mut unsupported = manifest(&r, b"ab");
    unsupported.artifacts[0].media_type = "image/png".into();
    assert!(matches!(
        a.record_manifest(&who, b, unsupported).await,
        Err(Error::Invalid(_))
    ));
}

#[sqlx::test(migrations = "./migrations")]
async fn revocation_model_and_other_key_prevent_upload_and_completed_tool_cannot_reserve_again(
    pool: PgPool,
) {
    let (who, turn, a, _) = fixture(&pool).await;
    let r = claimed(&pool, &who, turn, "authority").await;
    let b = id(&a.reserve(&who, r.clone()).await.unwrap());
    a.record_manifest(&who, b, manifest(&r, b"abc"))
        .await
        .unwrap();
    let (other_key, _) = store::create_key(
        &pool,
        who.tenant_id,
        "other",
        1_000_000,
        &["model".into()],
        None,
    )
    .await
    .unwrap();
    let other = store::Identity {
        tenant_id: who.tenant_id,
        key_id: other_key,
    };
    assert!(matches!(
        a.metadata(&other, b, &r.claim_token).await,
        Err(Error::NotFound)
    ));
    sqlx::query("DELETE FROM key_models WHERE tenant_id=$1 AND key_id=$2")
        .bind(who.tenant_id)
        .bind(who.key_id)
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        a.put_chunk(&who, b, 0, 0, &r.claim_token, Bytes::from_static(b"abc"))
            .await,
        Err(Error::Unauthorized)
    ));
    sqlx::query("INSERT INTO key_models(tenant_id,key_id,model_id) VALUES($1,$2,'model')")
        .bind(who.tenant_id)
        .bind(who.key_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE virtual_keys SET revoked_at=clock_timestamp() WHERE tenant_id=$1 AND id=$2",
    )
    .bind(who.tenant_id)
    .bind(who.key_id)
    .execute(&pool)
    .await
    .unwrap();
    assert!(
        a.put_chunk(&who, b, 0, 0, &r.claim_token, Bytes::from_static(b"abc"))
            .await
            .is_err()
    );
    let (who2, turn2, a2, _) = fixture(&pool).await;
    let r2 = claimed(&pool, &who2, turn2, "completed-no-reserve").await;
    governance::receipt(
        &pool,
        &common::cipher(),
        &who2,
        r2.input_operation_id,
        governance::Receipt {
            token: r2.claim_token.clone(),
            outcome: "completed".into(),
            evidence_ref: "runtime://completed".into(),
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        a2.reserve(&who2, r2).await,
        Err(Error::InProgress)
    ));
}

// Privileged fixture clock setup, never available to a production caller.
async fn expire_fixture(pool: &PgPool, id: Uuid) {
    sqlx::query(
        "ALTER TABLE runtime_artifact_batches DISABLE TRIGGER runtime_artifact_batch_immutable",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("ALTER TABLE assets DISABLE TRIGGER asset_immutable")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("UPDATE runtime_artifact_batches SET created_at=clock_timestamp()-interval '121 seconds',expires_at=clock_timestamp()-interval '1 second',upload_expires_at=clock_timestamp()-interval '2 seconds' WHERE id=$1").bind(id).execute(pool).await.unwrap();
    sqlx::query("UPDATE assets a SET created_at=b.created_at,expires_at=b.expires_at,upload_expires_at=b.upload_expires_at FROM runtime_artifact_batches b JOIN runtime_artifacts f ON f.tenant_id=b.tenant_id AND f.batch_id=b.id WHERE b.id=$1 AND a.tenant_id=f.tenant_id AND a.id=f.asset_id").bind(id).execute(pool).await.unwrap();
    sqlx::query("ALTER TABLE assets ENABLE TRIGGER asset_immutable")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "ALTER TABLE runtime_artifact_batches ENABLE TRIGGER runtime_artifact_batch_immutable",
    )
    .execute(pool)
    .await
    .unwrap();
}
#[sqlx::test(migrations = "./migrations")]
async fn expiry_returns_capacity_once_and_preserves_original_manifest_tombstones(pool: PgPool) {
    let (who, turn, a, _) = fixture(&pool).await;
    let r = claimed(&pool, &who, turn, "unused").await;
    let reserve = a.reserve(&who, r.clone()).await.unwrap();
    let b = id(&reserve);
    expire_fixture(&pool, b).await;
    a.maintain().await.unwrap();
    a.maintain().await.unwrap();
    assert_eq!(used(&pool, &who).await, (0, 0));
    let value = a.reserve(&who, r.clone()).await.unwrap();
    assert_eq!(value["state"], "expired");
    assert_eq!(id(&value), b);
    assert!(
        a.record_manifest(&who, b, manifest(&r, b"abc"))
            .await
            .is_err()
    );
    let r2 = claimed(&pool, &who, turn, "materialized").await;
    let b2 = id(&a.reserve(&who, r2.clone()).await.unwrap());
    let m = manifest(&r2, b"abc");
    let facts = a.record_manifest(&who, b2, m.clone()).await.unwrap();
    a.put_chunk(&who, b2, 0, 0, &r2.claim_token, Bytes::from_static(b"abc"))
        .await
        .unwrap();
    a.complete(&who, b2, &r2.claim_token).await.unwrap();
    expire_fixture(&pool, b2).await;
    a.maintain().await.unwrap();
    a.maintain().await.unwrap();
    assert_eq!(used(&pool, &who).await, (0, 0));
    let later = a.record_manifest(&who, b2, m).await.unwrap();
    assert_eq!(later["state"], "expired");
    assert_eq!(later["artifacts"], facts["artifacts"]);
    assert!(a.complete(&who, b2, &r2.claim_token).await.is_err());
    let state: String = sqlx::query_scalar("SELECT state FROM assets WHERE id=$1")
        .bind(asset(&facts))
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(state, "deleted");
    assert!(
        sqlx::query("DELETE FROM runtime_artifacts")
            .execute(&pool)
            .await
            .is_err()
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn runtime_storage_role_captures_original_claim_but_cannot_read_input_or_execute_approve_scan(
    pool: PgPool,
) {
    let (who, turn, admin, objects) = fixture(&pool).await;
    let input = id(&admin
        .assets
        .create(
            &who,
            "ordinary-input",
            CreateAsset {
                byte_length: 3,
                sha256: common::hex(&store::hash(b"abc")),
                media_type: "video/mp4".into(),
                ttl_seconds: 3600,
            },
        )
        .await
        .unwrap());
    admin
        .assets
        .put_chunk(&who, input, 0, Bytes::from_static(b"abc"))
        .await
        .unwrap();
    admin.assets.complete(&who, input).await.unwrap();
    let r = claimed(&pool, &who, turn, "purpose-scoped").await;
    sqlx::raw_sql(include_str!("../deploy/permissions.sql"))
        .execute(&pool)
        .await
        .unwrap();
    let scoped = sqlx::postgres::PgPoolOptions::new()
        .max_connections(3)
        .after_connect(|c, _| {
            Box::pin(async move {
                sqlx::query("SET ROLE brokerrouter_runtime_assets")
                    .execute(c)
                    .await?;
                Ok(())
            })
        })
        .connect_with(pool.connect_options().as_ref().clone())
        .await
        .unwrap();
    let a = RuntimeArtifacts::new(
        Assets::new(scoped.clone(), objects, common::cipher(), 2).unwrap(),
        common::cipher(),
    );
    let b = id(&a.reserve(&who, r.clone()).await.unwrap());
    let facts = a
        .record_manifest(&who, b, manifest(&r, b"abc"))
        .await
        .unwrap();
    a.put_chunk(&who, b, 0, 0, &r.claim_token, Bytes::from_static(b"abc"))
        .await
        .unwrap();
    assert_eq!(
        a.complete(&who, b, &r.claim_token).await.unwrap()["state"],
        "quarantined"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM assets WHERE kind='input'")
            .fetch_one(&scoped)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM asset_chunks WHERE asset_id=$1")
            .bind(input)
            .fetch_one(&scoped)
            .await
            .unwrap(),
        0
    );
    assert!(matches!(
        a.assets.content_chunk(&who, input, 0).await,
        Err(Error::NotFound)
    ));
    assert!(matches!(
        a.assets.metadata(&who, asset(&facts)).await,
        Err(Error::NotFound)
    ));
    for statement in [
        "UPDATE governance_operations SET claim_hash=decode(repeat('55',32),'hex')",
        "UPDATE governance_operations SET execution_deadline=clock_timestamp()+interval '1 hour'",
        "UPDATE tenants SET spent_micros=0",
        "UPDATE tenants SET asset_quota_bytes=10995116277760",
        "UPDATE approvals SET state='approved'",
        "UPDATE guardrail_grants SET expires_at=clock_timestamp()+interval '1 hour'",
        "UPDATE output_scans SET state='clean'",
        "DELETE FROM runtime_artifacts",
    ] {
        let error = sqlx::query(statement).execute(&scoped).await.unwrap_err();
        assert_eq!(
            error.as_database_error().and_then(|e| e.code()).as_deref(),
            Some("42501"),
            "expected permission denial for {statement}"
        );
    }
    let inheritance:bool=sqlx::query_scalar("SELECT pg_has_role(current_user,'brokerrouter_assets','MEMBER') OR pg_has_role(current_user,'brokerrouter_runtime','MEMBER') OR pg_has_role(current_user,'brokerrouter_output_scan','MEMBER') OR pg_has_role(current_user,'brokerrouter_media_review','MEMBER') OR pg_has_role(current_user,'brokerrouter_approvals','MEMBER')").fetch_one(&scoped).await.unwrap();
    assert!(!inheritance);
    for role in [
        "brokerrouter_assets",
        "brokerrouter_media_outputs",
        "brokerrouter_output_scan",
        "brokerrouter_output_delivery",
        "brokerrouter_media_review",
    ] {
        let connection = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .after_connect(move |c, _| {
                Box::pin(async move {
                    sqlx::query(&format!("SET ROLE {role}")).execute(c).await?;
                    Ok(())
                })
            })
            .connect_with(pool.connect_options().as_ref().clone())
            .await
            .unwrap();
        let readable: i64 =
            sqlx::query_scalar("SELECT count(*) FROM assets WHERE kind='runtime_output'")
                .fetch_one(&connection)
                .await
                .unwrap();
        assert_eq!(readable, 0, "unreviewed runtime asset visible to {role}");
        connection.close().await;
    }
    expire_fixture(&pool, b).await;
    a.maintain().await.unwrap();
    a.maintain().await.unwrap();
    assert_eq!(
        used(&pool, &who).await,
        (3, 3),
        "only ordinary input capacity remains"
    );
    scoped.close().await;
}

#[sqlx::test(migrations = "./migrations")]
async fn key_expiring_during_manifest_audit_rolls_back_receipt_assets_and_quota_conversion(
    pool: PgPool,
) {
    let (who, turn, a, _) = fixture(&pool).await;
    let r = claimed(&pool, &who, turn, "expiry-boundary").await;
    let b = id(&a.reserve(&who, r.clone()).await.unwrap());
    sqlx::raw_sql("CREATE SEQUENCE artifact_audit_reached; CREATE FUNCTION hold_artifact_audit() RETURNS trigger LANGUAGE plpgsql AS $$ DECLARE deadline timestamptz; BEGIN IF NEW.action='runtime_artifact.manifest_recorded' THEN PERFORM nextval('artifact_audit_reached'); SELECT expires_at INTO deadline FROM virtual_keys WHERE tenant_id=NEW.tenant_id LIMIT 1; PERFORM pg_sleep(GREATEST(0,EXTRACT(EPOCH FROM deadline-clock_timestamp()))+0.1); END IF; RETURN NEW; END; $$; CREATE TRIGGER hold_artifact_audit BEFORE INSERT ON audit_events FOR EACH ROW EXECUTE FUNCTION hold_artifact_audit();").execute(&pool).await.unwrap();
    sqlx::query("UPDATE virtual_keys SET expires_at=clock_timestamp()+interval '2 seconds' WHERE tenant_id=$1 AND id=$2").bind(who.tenant_id).bind(who.key_id).execute(&pool).await.unwrap();
    assert!(matches!(
        a.record_manifest(&who, b, manifest(&r, b"abc")).await,
        Err(Error::ResultUnavailable)
    ));
    let reached: bool = sqlx::query_scalar("SELECT is_called FROM artifact_audit_reached")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(
        reached,
        "must exercise audit wait, not fail before the boundary"
    );
    let facts:(String,bool,i64,i64)=sqlx::query_as("SELECT state,receipt_hash IS NULL,(SELECT count(*) FROM runtime_artifacts),(SELECT count(*) FROM assets) FROM governance_operations WHERE id=$1").bind(r.input_operation_id).fetch_one(&pool).await.unwrap();
    assert_eq!(facts, ("executing".into(), true, 0, 0));
    assert_eq!(used(&pool, &who).await, (MAX_BYTES, MAX_BYTES));
}
