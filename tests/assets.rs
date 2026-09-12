use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use brokerrouter::{
    asset_api,
    assets::{self, Assets, CHUNK_BYTES, CreateAsset},
    cipher::ResultCipher,
    error::Error,
    store,
};
use bytes::Bytes;
use http_body_util::BodyExt;
use object_store::{ObjectStore, ObjectStoreExt, memory::InMemory, path::Path};
use serde_json::Value;
use sqlx::PgPool;
use std::{collections::HashMap, sync::Arc};
use tower::ServiceExt;
use uuid::Uuid;

fn cipher() -> ResultCipher {
    ResultCipher::new(
        "test-v1".into(),
        HashMap::from([("test-v1".into(), "12".repeat(32))]),
    )
    .unwrap()
}
async fn identity(pool: &PgPool) -> (store::Identity, String) {
    let tenant = store::create_tenant(pool, "asset test", 10_000_000)
        .await
        .unwrap();
    store::put_model(pool, tenant, "test/model", "Test")
        .await
        .unwrap();
    let (key_id, key) = store::create_key(
        pool,
        tenant,
        "assets",
        10_000_000,
        &["test/model".into()],
        None,
    )
    .await
    .unwrap();
    let who = store::Identity {
        tenant_id: tenant,
        key_id,
    };
    assets::set_quota(pool, tenant, None, 10 * CHUNK_BYTES as i64)
        .await
        .unwrap();
    assets::set_quota(pool, tenant, Some(key_id), 10 * CHUNK_BYTES as i64)
        .await
        .unwrap();
    (who, key)
}
fn request(bytes: &[u8]) -> CreateAsset {
    CreateAsset {
        byte_length: bytes.len() as i64,
        sha256: store::hash(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
        media_type: "application/octet-stream".into(),
        ttl_seconds: 3600,
    }
}
fn id(value: &Value) -> Uuid {
    serde_json::from_value(value["id"].clone()).unwrap()
}
fn service(pool: &PgPool, store: Arc<dyn ObjectStore>) -> Assets {
    Assets::new(pool.clone(), store, cipher(), 8).unwrap()
}
async fn path(pool: &PgPool, who: &store::Identity, id: Uuid, ordinal: i32) -> Path {
    let chunk: Uuid = sqlx::query_scalar(
        "SELECT id FROM asset_chunks WHERE tenant_id=$1 AND asset_id=$2 AND ordinal=$3",
    )
    .bind(who.tenant_id)
    .bind(id)
    .bind(ordinal)
    .fetch_one(pool)
    .await
    .unwrap();
    Path::from(format!("assets-v1/{}/{}/{}", who.tenant_id, id, chunk))
}

#[sqlx::test(migrations = "./migrations")]
async fn input_assets_are_private_encrypted_immutable_and_recoverable(pool: PgPool) {
    let (who, _) = identity(&pool).await;
    let objects = Arc::new(InMemory::new());
    let a = service(&pool, objects.clone());
    let bytes = Bytes::from(vec![71; CHUNK_BYTES + 3]);
    let asset = id(&a.create(&who, "first", request(&bytes)).await.unwrap());
    assert!(matches!(
        a.content_chunk(&who, asset, 0).await,
        Err(Error::ResultUnavailable)
    ));
    assert!(matches!(
        a.complete(&who, asset).await,
        Err(Error::InProgress)
    ));
    a.put_chunk(&who, asset, 0, bytes.slice(..CHUNK_BYTES))
        .await
        .unwrap();
    a.put_chunk(&who, asset, 0, bytes.slice(..CHUNK_BYTES))
        .await
        .unwrap();
    assert!(matches!(
        a.put_chunk(&who, asset, 0, Bytes::from(vec![72; CHUNK_BYTES]))
            .await,
        Err(Error::Conflict)
    ));
    assert!(
        a.put_chunk(&who, asset, 1, Bytes::from_static(b"xx"))
            .await
            .is_err()
    );
    a.put_chunk(&who, asset, 1, bytes.slice(CHUNK_BYTES..))
        .await
        .unwrap();
    let p = path(&pool, &who, asset, 1).await;
    let encrypted = objects.get(&p).await.unwrap().bytes().await.unwrap();
    assert_ne!(encrypted, bytes.slice(CHUNK_BYTES..));
    assert!(encrypted.len() > 3);
    let complete = a.complete(&who, asset).await.unwrap();
    assert_eq!(complete["state"], "available");
    assert_eq!(complete["content_safety"], "not_evaluated");
    assert_eq!(a.complete(&who, asset).await.unwrap(), complete);
    assert_eq!(
        a.content_chunk(&who, asset, 1).await.unwrap(),
        bytes.slice(CHUNK_BYTES..)
    );
    assert_eq!(
        id(&a.create(&who, "first", request(&bytes)).await.unwrap()),
        asset
    );
    assert!(
        !a.metadata(&who, asset)
            .await
            .unwrap()
            .to_string()
            .contains("assets-v1")
    );
    assert!(
        sqlx::query("UPDATE assets SET sha256=$2 WHERE id=$1")
            .bind(asset)
            .bind(store::hash(b"other"))
            .execute(&pool)
            .await
            .is_err()
    );
    objects
        .put(&p, Bytes::from_static(b"corrupt").into())
        .await
        .unwrap();
    assert!(matches!(
        a.content_chunk(&who, asset, 1).await,
        Err(Error::Storage)
    ));
}

#[sqlx::test(migrations = "./migrations")]
async fn asset_quota_is_atomic_idempotent_and_not_money(pool: PgPool) {
    let (who, _) = identity(&pool).await;
    let a = service(&pool, Arc::new(InMemory::new()));
    assets::set_quota(&pool, who.tenant_id, None, 3)
        .await
        .unwrap();
    assets::set_quota(&pool, who.tenant_id, Some(who.key_id), 3)
        .await
        .unwrap();
    let (x, y) = tokio::join!(
        a.create(&who, "x", request(b"abc")),
        a.create(&who, "y", request(b"abc"))
    );
    assert_eq!(usize::from(x.is_ok()) + usize::from(y.is_ok()), 1);
    let winner = x.or(y).unwrap();
    assert!(
        assets::set_quota(&pool, who.tenant_id, None, 2)
            .await
            .is_err()
    );
    let (used, spent, reserved): (i64, i64, i64) = sqlx::query_as(
        "SELECT asset_used_bytes,spent_micros,reserved_micros FROM tenants WHERE id=$1",
    )
    .bind(who.tenant_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((used, spent, reserved), (3, 0, 0));
    let asset = id(&winner);
    a.delete(&who, asset).await.unwrap();
    a.delete(&who, asset).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT asset_used_bytes FROM tenants WHERE id=$1")
            .bind(who.tenant_id)
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    assert!(
        a.put_chunk(&who, asset, 0, Bytes::from_static(b"abc"))
            .await
            .is_err()
    );
    assert!(
        sqlx::query("UPDATE assets SET state='uploading',deleted_at=NULL WHERE id=$1")
            .bind(asset)
            .execute(&pool)
            .await
            .is_err()
    );
    assets::set_quota(&pool, who.tenant_id, Some(who.key_id), 0)
        .await
        .unwrap();
    assert!(matches!(
        a.create(&who, "z", request(b"abc")).await,
        Err(Error::AssetQuota)
    ));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT asset_used_bytes FROM tenants WHERE id=$1")
            .bind(who.tenant_id)
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn cross_tenant_cross_key_revocation_and_gc_are_enforced(pool: PgPool) {
    let (who, _) = identity(&pool).await;
    let (other, _) = identity(&pool).await;
    let (key_id, _) = store::create_key(
        &pool,
        who.tenant_id,
        "different key",
        1,
        &["test/model".into()],
        None,
    )
    .await
    .unwrap();
    let same_tenant = store::Identity {
        tenant_id: who.tenant_id,
        key_id,
    };
    let objects = Arc::new(InMemory::new());
    let a = service(&pool, objects.clone());
    let asset = id(&a.create(&who, "private", request(b"abc")).await.unwrap());
    for denied in [&other, &same_tenant] {
        assert!(matches!(
            a.metadata(denied, asset).await,
            Err(Error::NotFound)
        ));
        assert!(matches!(
            a.delete(denied, asset).await,
            Err(Error::NotFound)
        ));
        assert!(matches!(
            a.put_chunk(denied, asset, 0, Bytes::from_static(b"abc"))
                .await,
            Err(Error::NotFound)
        ));
    }
    a.put_chunk(&who, asset, 0, Bytes::from_static(b"abc"))
        .await
        .unwrap();
    a.complete(&who, asset).await.unwrap();
    let p = path(&pool, &who, asset, 0).await;
    let encrypted = objects.get(&p).await.unwrap().bytes().await.unwrap();
    a.delete(&who, asset).await.unwrap();
    sqlx::query("UPDATE assets SET gc_after=clock_timestamp()-interval '1 second' WHERE id=$1")
        .bind(asset)
        .execute(&pool)
        .await
        .unwrap();
    a.maintain().await.unwrap();
    assert!(objects.get(&p).await.is_err());
    // A late PUT acknowledgement is reconciled again without restoring logical access.
    objects.put(&p, encrypted.into()).await.unwrap();
    sqlx::query("UPDATE assets SET gc_after=clock_timestamp()-interval '1 second' WHERE id=$1")
        .bind(asset)
        .execute(&pool)
        .await
        .unwrap();
    a.maintain().await.unwrap();
    assert!(objects.get(&p).await.is_err());
    store::revoke_key(&pool, who.tenant_id, who.key_id)
        .await
        .unwrap();
    assert!(matches!(
        a.metadata(&who, asset).await,
        Err(Error::Unauthorized)
    ));
    assert!(matches!(
        a.create(&who, "revoked", request(b"x")).await,
        Err(Error::Unauthorized)
    ));
}

#[sqlx::test(migrations = "./migrations")]
async fn object_written_before_database_failure_recovers_without_overwrite(pool: PgPool) {
    let (who, _) = identity(&pool).await;
    let objects = Arc::new(InMemory::new());
    let a = service(&pool, objects.clone());
    let asset = id(&a.create(&who, "failure", request(b"abc")).await.unwrap());
    sqlx::raw_sql("CREATE FUNCTION fail_chunk_update() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected failure'; END; $$; CREATE TRIGGER injected_failure BEFORE UPDATE ON asset_chunks FOR EACH ROW EXECUTE FUNCTION fail_chunk_update();").execute(&pool).await.unwrap();
    assert!(
        a.put_chunk(&who, asset, 0, Bytes::from_static(b"abc"))
            .await
            .is_err()
    );
    let p = path(&pool, &who, asset, 0).await;
    let before = objects.get(&p).await.unwrap().bytes().await.unwrap();
    assert_eq!(
        a.metadata(&who, asset).await.unwrap()["chunks"][0]["state"],
        "pending"
    );
    sqlx::query("DROP TRIGGER injected_failure ON asset_chunks")
        .execute(&pool)
        .await
        .unwrap();
    a.put_chunk(&who, asset, 0, Bytes::from_static(b"abc"))
        .await
        .unwrap();
    assert_eq!(
        objects.get(&p).await.unwrap().bytes().await.unwrap(),
        before
    );
    a.complete(&who, asset).await.unwrap();
    assert_eq!(a.content_chunk(&who, asset, 0).await.unwrap(), b"abc"[..]);
}

#[sqlx::test(migrations = "./migrations")]
async fn full_hash_mismatch_missing_objects_and_bad_manifests_fail_closed(pool: PgPool) {
    let (who, _) = identity(&pool).await;
    let objects = Arc::new(InMemory::new());
    let a = service(&pool, objects.clone());
    let asset = id(&a.create(&who, "wrong hash", request(b"abc")).await.unwrap());
    a.put_chunk(&who, asset, 0, Bytes::from_static(b"def"))
        .await
        .unwrap();
    assert!(matches!(
        a.complete(&who, asset).await,
        Err(Error::Invalid(_))
    ));
    assert!(a.content_chunk(&who, asset, 0).await.is_err());
    objects
        .delete(&path(&pool, &who, asset, 0).await)
        .await
        .unwrap();
    assert!(matches!(a.complete(&who, asset).await, Err(Error::Storage)));
    let mut bad = request(b"a");
    bad.ttl_seconds = 2592001;
    assert!(a.create(&who, "bad", bad).await.is_err());
    let mut bad = request(b"a");
    bad.media_type = "text/html".into();
    assert!(a.create(&who, "bad", bad).await.is_err());
    assert!(a.create(&who, "empty", request(b"")).await.is_err());
    assert!(matches!(
        a.create(&who, "wrong hash", request(b"changed")).await,
        Err(Error::Conflict)
    ));
}

#[sqlx::test(migrations = "./migrations")]
async fn asset_http_streams_exact_bytes_and_blocks_range_and_unauthorized_access(pool: PgPool) {
    let (who, key) = identity(&pool).await;
    let a = service(&pool, Arc::new(InMemory::new()));
    let bytes = Bytes::from(vec![31; CHUNK_BYTES + 2]);
    let asset = id(&a.create(&who, "http", request(&bytes)).await.unwrap());
    a.put_chunk(&who, asset, 0, bytes.slice(..CHUNK_BYTES))
        .await
        .unwrap();
    a.put_chunk(&who, asset, 1, bytes.slice(CHUNK_BYTES..))
        .await
        .unwrap();
    a.complete(&who, asset).await.unwrap();
    let app = asset_api::router(a);
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/v1/assets/{asset}/content"))
                .header("authorization", format!("Bearer {key}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["cache-control"], "private, no-store");
    assert_eq!(
        response.headers()["content-type"],
        "application/octet-stream"
    );
    assert_eq!(response.headers()["content-disposition"], "attachment");
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        bytes
    );
    let range = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/v1/assets/{asset}/content"))
                .header("authorization", format!("Bearer {key}"))
                .header("range", "bytes=0-1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(range.status(), StatusCode::BAD_REQUEST);
    let no_key = app
        .oneshot(
            Request::builder()
                .uri(format!("/v1/assets/{asset}/content"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(no_key.status(), StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "./migrations")]
async fn asset_database_role_cannot_change_money_or_approvals(pool: PgPool) {
    let (who, _) = identity(&pool).await;
    sqlx::raw_sql(include_str!("../deploy/permissions.sql"))
        .execute(&pool)
        .await
        .unwrap();
    let scoped = sqlx::postgres::PgPoolOptions::new()
        .max_connections(3)
        .after_connect(|c, _| {
            Box::pin(async move {
                sqlx::query("SET ROLE brokerrouter_assets")
                    .execute(c)
                    .await?;
                Ok(())
            })
        })
        .connect_with(pool.connect_options().as_ref().clone())
        .await
        .unwrap();
    let a = service(&scoped, Arc::new(InMemory::new()));
    let asset = id(&a.create(&who, "role", request(b"abc")).await.unwrap());
    a.put_chunk(&who, asset, 0, Bytes::from_static(b"abc"))
        .await
        .unwrap();
    a.complete(&who, asset).await.unwrap();
    assert_eq!(a.content_chunk(&who, asset, 0).await.unwrap(), b"abc"[..]);
    a.delete(&who, asset).await.unwrap();
    assert!(
        sqlx::query("UPDATE tenants SET spent_micros=0")
            .execute(&scoped)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("UPDATE approvals SET state='approved'")
            .execute(&scoped)
            .await
            .is_err()
    );
    assert!(
        assets::set_quota(&scoped, who.tenant_id, None, 10)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("DELETE FROM assets")
            .execute(&scoped)
            .await
            .is_err()
    );
    scoped.close().await;
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires the isolated S3 fixture and BROKERROUTER_ASSET_* configuration"]
async fn s3_conditional_write_encrypted_roundtrip_and_physical_cleanup(pool: PgPool) {
    let (who, _) = identity(&pool).await;
    let a = Assets::from_env(pool.clone()).unwrap();
    let e = |name| std::env::var(name).expect("explicit S3 fixture configuration required");
    let objects = object_store::aws::AmazonS3Builder::new()
        .with_endpoint(e("BROKERROUTER_ASSET_S3_ENDPOINT"))
        .with_bucket_name(e("BROKERROUTER_ASSET_S3_BUCKET"))
        .with_region(e("BROKERROUTER_ASSET_S3_REGION"))
        .with_access_key_id(e("BROKERROUTER_ASSET_S3_ACCESS_KEY_ID"))
        .with_secret_access_key(e("BROKERROUTER_ASSET_S3_SECRET_ACCESS_KEY"))
        .with_allow_http(e("BROKERROUTER_ASSET_ALLOW_HTTP") == "true")
        .build()
        .unwrap();
    let asset = id(&a
        .create(&who, "s3", request(b"private input"))
        .await
        .unwrap());
    a.put_chunk(&who, asset, 0, Bytes::from_static(b"private input"))
        .await
        .unwrap();
    let p = path(&pool, &who, asset, 0).await;
    let before = objects.get(&p).await.unwrap().bytes().await.unwrap();
    a.put_chunk(&who, asset, 0, Bytes::from_static(b"private input"))
        .await
        .unwrap();
    assert_eq!(
        before,
        objects.get(&p).await.unwrap().bytes().await.unwrap(),
        "conditional retry must not overwrite ciphertext"
    );
    assert_ne!(before, b"private input"[..]);
    a.complete(&who, asset).await.unwrap();
    assert_eq!(
        a.content_chunk(&who, asset, 0).await.unwrap(),
        b"private input"[..]
    );
    a.delete(&who, asset).await.unwrap();
    sqlx::query("UPDATE assets SET gc_after=clock_timestamp()-interval '1 second' WHERE id=$1")
        .bind(asset)
        .execute(&pool)
        .await
        .unwrap();
    a.maintain().await.unwrap();
    assert!(matches!(
        objects.get(&p).await,
        Err(object_store::Error::NotFound { .. })
    ));
}

#[sqlx::test(migrations = "./migrations")]
async fn stream_revocation_prevents_subsequent_chunks(pool: PgPool) {
    let (who, key) = identity(&pool).await;
    let a = service(&pool, Arc::new(InMemory::new()));
    let bytes = Bytes::from(vec![9; CHUNK_BYTES + 1]);
    let asset = id(&a.create(&who, "stream", request(&bytes)).await.unwrap());
    a.put_chunk(&who, asset, 0, bytes.slice(..CHUNK_BYTES))
        .await
        .unwrap();
    a.put_chunk(&who, asset, 1, bytes.slice(CHUNK_BYTES..))
        .await
        .unwrap();
    a.complete(&who, asset).await.unwrap();
    let response = asset_api::router(a)
        .oneshot(
            Request::builder()
                .uri(format!("/v1/assets/{asset}/content"))
                .header("authorization", format!("Bearer {key}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body();
    assert_eq!(
        body.frame()
            .await
            .unwrap()
            .unwrap()
            .into_data()
            .unwrap()
            .len(),
        CHUNK_BYTES
    );
    store::revoke_key(&pool, who.tenant_id, who.key_id)
        .await
        .unwrap();
    assert!(body.frame().await.unwrap().is_err());
}

#[sqlx::test(migrations = "./migrations")]
async fn expired_uploads_release_quota_even_for_revoked_keys(pool: PgPool) {
    let (who, _) = identity(&pool).await;
    let a = service(&pool, Arc::new(InMemory::new()));
    let asset = Uuid::new_v4();
    // Seed a historical fact, without mutating immutable expiry fields after insertion.
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("UPDATE tenants SET asset_used_bytes=3 WHERE id=$1")
        .bind(who.tenant_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("UPDATE virtual_keys SET asset_used_bytes=3 WHERE id=$1")
        .bind(who.key_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("INSERT INTO assets(tenant_id,id,key_id,idempotency_key,request_hash,sha256,byte_length,declared_media_type,chunk_count,created_at,upload_expires_at,expires_at) VALUES($1,$2,$3,'historic',$4,$4,3,'application/octet-stream',1,clock_timestamp()-interval '2 hours',clock_timestamp()-interval '1 hour',clock_timestamp()+interval '1 hour')")
        .bind(who.tenant_id).bind(asset).bind(who.key_id).bind(store::hash(b"abc")).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(a.metadata(&who, asset).await.unwrap()["state"], "expired");
    assert!(
        a.put_chunk(&who, asset, 0, Bytes::from_static(b"abc"))
            .await
            .is_err()
    );
    store::revoke_key(&pool, who.tenant_id, who.key_id)
        .await
        .unwrap();
    a.maintain().await.unwrap();
    a.maintain().await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT asset_used_bytes FROM tenants WHERE id=$1")
            .bind(who.tenant_id)
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT state FROM assets WHERE id=$1")
            .bind(asset)
            .fetch_one(&pool)
            .await
            .unwrap(),
        "deleted"
    );
}
