//! Immutable, chunk-encrypted assets. Public APIs are input-only; outputs remain quarantined.
use crate::{
    accounting,
    cipher::ResultCipher,
    error::{Error, Result},
    governance,
    store::{self, Identity},
};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures_util::TryStreamExt;
use object_store::{ObjectStore, ObjectStoreExt, PutMode, PutOptions, path::Path};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Postgres, Transaction};
use std::{sync::Arc, time::Duration};
use uuid::Uuid;

pub const CHUNK_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_ASSET_BYTES: i64 = 512 * 1024 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(30);
type Tx<'a> = Transaction<'a, Postgres>;

#[derive(Clone, sqlx::FromRow)]
struct Asset {
    tenant_id: Uuid,
    id: Uuid,
    key_id: Uuid,
    request_hash: Vec<u8>,
    sha256: Option<Vec<u8>>,
    kind: String,
    byte_length: i64,
    declared_media_type: String,
    chunk_count: i32,
    state: String,
    upload_expires_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
}
#[derive(Clone, sqlx::FromRow)]
struct Chunk {
    tenant_id: Uuid,
    asset_id: Uuid,
    id: Uuid,
    ordinal: i32,
    byte_length: i32,
    sha256: Vec<u8>,
    state: String,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CreateAsset {
    pub byte_length: i64,
    pub sha256: String,
    pub media_type: String,
    pub ttl_seconds: i32,
}

#[derive(Clone)]
pub struct Assets {
    pub pool: PgPool,
    store: Arc<dyn ObjectStore>,
    cipher: Arc<ResultCipher>,
    pub(crate) capacity: Arc<tokio::sync::Semaphore>,
    pub(crate) tasks: tokio_util::task::TaskTracker,
}
#[derive(Debug)]
struct StorageTransport(reqwest::Client);
impl object_store::client::HttpConnector for StorageTransport {
    fn connect(
        &self,
        _: &object_store::ClientOptions,
    ) -> object_store::Result<object_store::client::HttpClient> {
        Ok(object_store::client::HttpClient::new(self.0.clone()))
    }
}

pub(crate) fn storage_http_client() -> Result<reqwest::Client> {
    crate::tls_roots::with_ca_file(reqwest::Client::builder(), "BROKERROUTER_ASSET_S3_CA_FILE")?
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .timeout(IO_TIMEOUT)
        .connect_timeout(Duration::from_secs(5))
        .no_gzip()
        .no_brotli()
        .no_deflate()
        .no_zstd()
        .build()
        .map_err(|_| Error::Configuration)
}

impl Assets {
    pub fn new(
        pool: PgPool,
        store: Arc<dyn ObjectStore>,
        cipher: ResultCipher,
        concurrency: usize,
    ) -> Result<Self> {
        if !(1..=64).contains(&concurrency) {
            return Err(Error::Configuration);
        }
        Ok(Self {
            pool,
            store,
            cipher: Arc::new(cipher),
            capacity: Arc::new(tokio::sync::Semaphore::new(concurrency)),
            tasks: tokio_util::task::TaskTracker::new(),
        })
    }
    pub fn from_env(pool: PgPool) -> Result<Self> {
        let env = |key| std::env::var(key).map_err(|_| Error::Configuration);
        let endpoint = env("BROKERROUTER_ASSET_S3_ENDPOINT")?;
        let bucket = env("BROKERROUTER_ASSET_S3_BUCKET")?;
        let region = env("BROKERROUTER_ASSET_S3_REGION")?;
        let allow_http = std::env::var("BROKERROUTER_ASSET_ALLOW_HTTP").as_deref() == Ok("true");
        let url = url::Url::parse(&endpoint).map_err(|_| Error::Configuration)?;
        if !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || url.host_str().is_none()
            || url.path() != "/"
            || !(url.scheme() == "https" || (allow_http && url.scheme() == "http"))
            || !(3..=63).contains(&bucket.len())
            || !bucket
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.')
            || !governance::valid_text(&region, 100)
        {
            return Err(Error::Configuration);
        }
        // The destination is deployment configuration, never supplied by a tenant.
        // Do not inherit HTTP proxies, follow redirects, or discover ambient credentials.
        let client = storage_http_client()?;
        let mut builder = object_store::aws::AmazonS3Builder::new()
            .with_endpoint(endpoint)
            .with_bucket_name(bucket)
            .with_region(region)
            .with_access_key_id(env("BROKERROUTER_ASSET_S3_ACCESS_KEY_ID")?)
            .with_secret_access_key(env("BROKERROUTER_ASSET_S3_SECRET_ACCESS_KEY")?)
            .with_allow_http(allow_http)
            .with_virtual_hosted_style_request(false)
            .with_http_connector(StorageTransport(client))
            .with_retry(object_store::RetryConfig {
                max_retries: 0,
                retry_timeout: IO_TIMEOUT,
                ..Default::default()
            });
        if let Ok(token) = std::env::var("BROKERROUTER_ASSET_S3_SESSION_TOKEN") {
            builder = builder.with_token(token);
        }
        let store = builder.build().map_err(|_| Error::Configuration)?;
        let concurrency = std::env::var("BROKERROUTER_ASSET_CONCURRENCY")
            .unwrap_or_else(|_| "8".into())
            .parse()
            .map_err(|_| Error::Configuration)?;
        Self::new(
            pool,
            Arc::new(store),
            ResultCipher::from_env()?,
            concurrency,
        )
    }
    pub async fn drain(&self) {
        self.tasks.close();
        self.tasks.wait().await;
    }
    pub async fn create(&self, who: &Identity, idem: &str, body: CreateAsset) -> Result<Value> {
        if !governance::valid_text(idem, 200)
            || idem.starts_with("_br_output:")
            || !(1..=MAX_ASSET_BYTES).contains(&body.byte_length)
            || !(60..=2592000).contains(&body.ttl_seconds)
            || !valid_media_type(&body.media_type)
        {
            return Err(Error::Invalid("invalid asset upload contract"));
        }
        let digest = decode_hash(&body.sha256)?;
        let hash = governance::digest(&json!(body))?;
        let mut tx = self.pool.begin().await?;
        authorize(&mut tx, who).await?;
        if let Some(a) = sqlx::query_as::<_, Asset>(
            "SELECT * FROM assets WHERE tenant_id=$1 AND key_id=$2 AND idempotency_key=$3",
        )
        .bind(who.tenant_id)
        .bind(who.key_id)
        .bind(idem)
        .fetch_optional(&mut *tx)
        .await?
        {
            input_only(&a)?;
            if a.request_hash != hash {
                return Err(Error::Conflict);
            }
            tx.commit().await?;
            return Ok(view(&a));
        }
        let t=sqlx::query("UPDATE tenants SET asset_used_bytes=asset_used_bytes+$2 WHERE id=$1 AND asset_quota_bytes-asset_used_bytes>=$2").bind(who.tenant_id).bind(body.byte_length).execute(&mut *tx).await?;
        let k=sqlx::query("UPDATE virtual_keys SET asset_used_bytes=asset_used_bytes+$3 WHERE tenant_id=$1 AND id=$2 AND asset_quota_bytes-asset_used_bytes>=$3").bind(who.tenant_id).bind(who.key_id).bind(body.byte_length).execute(&mut *tx).await?;
        if t.rows_affected() != 1 || k.rows_affected() != 1 {
            return Err(Error::AssetQuota);
        }
        let id = Uuid::new_v4();
        let chunks = ((body.byte_length + CHUNK_BYTES as i64 - 1) / CHUNK_BYTES as i64) as i32;
        let a:Asset=sqlx::query_as("INSERT INTO assets(tenant_id,id,key_id,idempotency_key,request_hash,sha256,byte_length,declared_media_type,chunk_count,upload_expires_at,expires_at) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,statement_timestamp()+make_interval(secs=>LEAST($10,3600)),statement_timestamp()+make_interval(secs=>$10)) RETURNING *")
            .bind(who.tenant_id).bind(id).bind(who.key_id).bind(idem).bind(hash).bind(digest).bind(body.byte_length).bind(body.media_type).bind(chunks).bind(f64::from(body.ttl_seconds)).fetch_one(&mut *tx).await?;
        audit(
            &mut tx,
            who,
            "asset.created",
            id,
            json!({"bytes":a.byte_length,"kind":"input"}),
        )
        .await?;
        tx.commit().await?;
        Ok(view(&a))
    }
    pub async fn metadata(&self, who: &Identity, id: Uuid) -> Result<Value> {
        let mut tx = self.pool.begin().await?;
        authorize(&mut tx, who).await?;
        let a = asset(&mut tx, who, id).await?;
        input_only(&a)?;
        let chunks:Vec<(i32,String)>=sqlx::query_as("SELECT ordinal,state FROM asset_chunks WHERE tenant_id=$1 AND asset_id=$2 ORDER BY ordinal").bind(who.tenant_id).bind(id).fetch_all(&mut *tx).await?;
        let mut v = view(&a);
        v["chunks"] = json!(
            chunks
                .into_iter()
                .map(|(ordinal, state)| json!({"ordinal":ordinal,"state":state}))
                .collect::<Vec<_>>()
        );
        tx.commit().await?;
        Ok(v)
    }
    pub async fn put_chunk(
        &self,
        who: &Identity,
        id: Uuid,
        ordinal: i32,
        bytes: Bytes,
    ) -> Result<Value> {
        self.put_chunk_inner(who, id, ordinal, bytes, None).await
    }
    pub(crate) async fn put_generated_chunk(
        &self,
        who: &Identity,
        id: Uuid,
        ordinal: i32,
        bytes: Bytes,
        owner: Uuid,
    ) -> Result<Value> {
        self.put_chunk_inner(who, id, ordinal, bytes, Some(owner))
            .await
    }
    async fn put_chunk_inner(
        &self,
        who: &Identity,
        id: Uuid,
        ordinal: i32,
        bytes: Bytes,
        owner: Option<Uuid>,
    ) -> Result<Value> {
        if bytes.is_empty() || bytes.len() > CHUNK_BYTES {
            return Err(Error::Invalid("invalid asset chunk size"));
        }
        let hash = store::hash(&bytes);
        let mut tx = self.pool.begin().await?;
        authorize(&mut tx, who).await?;
        let a = asset(&mut tx, who, id).await?;
        write_authority(&mut tx, who, &a, owner).await?;
        writable(&a)?;
        if ordinal < 0
            || ordinal >= a.chunk_count
            || bytes.len() as i64
                != (a.byte_length - i64::from(ordinal) * CHUNK_BYTES as i64).min(CHUNK_BYTES as i64)
        {
            return Err(Error::Invalid(
                "chunk ordinal or length does not match manifest",
            ));
        }
        sqlx::query("INSERT INTO asset_chunks(tenant_id,asset_id,id,ordinal,byte_length,sha256) VALUES($1,$2,$3,$4,$5,$6) ON CONFLICT DO NOTHING")
            .bind(who.tenant_id).bind(id).bind(Uuid::new_v4()).bind(ordinal).bind(bytes.len() as i32).bind(&hash).execute(&mut *tx).await?;
        let c = chunk(&mut tx, who.tenant_id, id, ordinal).await?;
        if c.sha256 != hash {
            return Err(Error::Conflict);
        }
        tx.commit().await?;
        if c.state == "stored" {
            // An acknowledged immutable chunk needs no second PUT. Verify storage reality,
            // not just the database flag, before replaying its success.
            self.read_chunk(&c).await?;
        } else {
            let encrypted = self.cipher.encrypt(who.tenant_id, c.id, &bytes)?;
            let put = tokio::time::timeout(
                IO_TIMEOUT,
                self.store.put_opts(
                    &location(&c),
                    encrypted.into(),
                    PutOptions {
                        mode: PutMode::Create,
                        ..Default::default()
                    },
                ),
            )
            .await;
            if !matches!(put, Ok(Ok(_))) {
                // A lost/failed conditional response does not prove the write failed.
                // Recover only from the exact existing authenticated plaintext and hash.
                // Missing, corrupt, or unreadable objects still fail; never repeat a PUT here.
                self.read_chunk(&c).await?;
            }
        }
        let mut tx = self.pool.begin().await?;
        authorize(&mut tx, who).await?;
        let current = asset(&mut tx, who, id).await?;
        write_authority(&mut tx, who, &current, owner).await?;
        writable(&current)?;
        sqlx::query("UPDATE asset_chunks SET state='stored' WHERE tenant_id=$1 AND asset_id=$2 AND ordinal=$3").bind(who.tenant_id).bind(id).bind(ordinal).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(
            json!({"asset_id":id,"ordinal":ordinal,"state":"stored","sha256":governance::hex(&hash)}),
        )
    }
    async fn read_chunk(&self, c: &Chunk) -> Result<Bytes> {
        let bytes = tokio::time::timeout(IO_TIMEOUT, async {
            let result = self
                .store
                .get(&location(c))
                .await
                .map_err(|_| Error::Storage)?;
            if result.meta.size > CHUNK_BYTES as u64 + 64 {
                return Err(Error::Storage);
            }
            let mut stream = result.into_stream();
            let mut bytes = Vec::new();
            while let Some(part) = stream.try_next().await.map_err(|_| Error::Storage)? {
                if bytes.len() + part.len() > CHUNK_BYTES + 64 {
                    return Err(Error::Storage);
                }
                bytes.extend_from_slice(&part);
            }
            Ok(bytes)
        })
        .await
        .map_err(|_| Error::Storage)??;
        let clear = self
            .cipher
            .decrypt(c.tenant_id, c.id, &bytes)
            .map_err(|_| Error::Storage)?;
        if clear.len() != c.byte_length as usize || store::hash(&clear) != c.sha256 {
            return Err(Error::Storage);
        }
        Ok(clear.into())
    }
    pub async fn complete(&self, who: &Identity, id: Uuid) -> Result<Value> {
        self.complete_inner(who, id, None).await
    }
    pub(crate) async fn complete_generated(
        &self,
        who: &Identity,
        id: Uuid,
        owner: Uuid,
    ) -> Result<Value> {
        self.complete_inner(who, id, Some(owner)).await
    }
    async fn complete_inner(&self, who: &Identity, id: Uuid, owner: Option<Uuid>) -> Result<Value> {
        let mut tx = self.pool.begin().await?;
        authorize(&mut tx, who).await?;
        let a = asset(&mut tx, who, id).await?;
        write_authority(&mut tx, who, &a, owner).await?;
        if a.state == "available" && a.expires_at > Utc::now() {
            tx.commit().await?;
            return Ok(view(&a));
        }
        writable(&a)?;
        let chunks: Vec<Chunk> = sqlx::query_as(
            "SELECT * FROM asset_chunks WHERE tenant_id=$1 AND asset_id=$2 ORDER BY ordinal",
        )
        .bind(who.tenant_id)
        .bind(id)
        .fetch_all(&mut *tx)
        .await?;
        if chunks.len() != a.chunk_count as usize || chunks.iter().any(|c| c.state != "stored") {
            return Err(Error::InProgress);
        }
        tx.commit().await?;
        let mut sha = Sha256::new();
        let mut length = 0;
        for c in &chunks {
            let data = self.read_chunk(c).await?;
            length += data.len() as i64;
            sha.update(&data);
        }
        let digest = sha.finalize().to_vec();
        if length != a.byte_length
            || a.sha256
                .as_ref()
                .is_some_and(|expected| expected != &digest)
            || (a.kind == "input" && a.sha256.is_none())
        {
            return Err(Error::Invalid(
                "complete asset SHA-256 does not match manifest",
            ));
        }
        let mut tx = self.pool.begin().await?;
        authorize(&mut tx, who).await?;
        let a = asset(&mut tx, who, id).await?;
        write_authority(&mut tx, who, &a, owner).await?;
        if a.state == "available" && a.expires_at > Utc::now() {
            tx.commit().await?;
            return Ok(view(&a));
        }
        writable(&a)?;
        let a: Asset = if owner.is_some() {
            sqlx::query_as("UPDATE assets SET state='available',sha256=$3,completed_at=clock_timestamp() WHERE tenant_id=$1 AND id=$2 RETURNING *").bind(who.tenant_id).bind(id).bind(digest).fetch_one(&mut *tx).await?
        } else {
            sqlx::query_as("UPDATE assets SET state='available',completed_at=clock_timestamp() WHERE tenant_id=$1 AND id=$2 RETURNING *").bind(who.tenant_id).bind(id).fetch_one(&mut *tx).await?
        };
        audit(
            &mut tx,
            who,
            "asset.completed",
            id,
            json!({"bytes":a.byte_length,"content_safety":"not_evaluated"}),
        )
        .await?;
        tx.commit().await?;
        Ok(view(&a))
    }
    pub async fn content_chunk(&self, who: &Identity, id: Uuid, ordinal: i32) -> Result<Bytes> {
        let mut tx = self.pool.begin().await?;
        authorize(&mut tx, who).await?;
        let a = asset(&mut tx, who, id).await?;
        input_only(&a)?;
        readable(&a)?;
        let c = chunk(&mut tx, who.tenant_id, id, ordinal).await?;
        audit(
            &mut tx,
            who,
            "asset.chunk_read",
            id,
            json!({"ordinal":ordinal}),
        )
        .await?;
        tx.commit().await?;
        let data = self.read_chunk(&c).await?;
        // Revocation/deletion during storage I/O must take effect before this chunk is released.
        let mut tx = self.pool.begin().await?;
        authorize(&mut tx, who).await?;
        readable(&asset(&mut tx, who, id).await?)?;
        tx.commit().await?;
        Ok(data)
    }
    // Callers must enforce their own scanner/reviewer/delivery authority before and after I/O.
    // Never exposed by the generic input asset routes.
    pub(crate) async fn quarantined_chunk(
        &self,
        who: &Identity,
        id: Uuid,
        ordinal: i32,
    ) -> Result<Bytes> {
        let c:Chunk=sqlx::query_as("SELECT c.* FROM asset_chunks c JOIN assets a ON a.tenant_id=c.tenant_id AND a.id=c.asset_id WHERE a.tenant_id=$1 AND a.key_id=$2 AND a.id=$3 AND a.kind='output' AND a.state='available' AND a.expires_at>clock_timestamp() AND c.ordinal=$4").bind(who.tenant_id).bind(who.key_id).bind(id).bind(ordinal).fetch_optional(&self.pool).await?.ok_or(Error::NotFound)?;
        self.read_chunk(&c).await
    }
    pub async fn delete(&self, who: &Identity, id: Uuid) -> Result<Value> {
        let mut tx = self.pool.begin().await?;
        authorize(&mut tx, who).await?;
        let a = asset(&mut tx, who, id).await?;
        input_only(&a)?;
        tombstone(&mut tx, &a).await?;
        tx.commit().await?;
        Ok(json!({"id":id,"state":"deleted","physical_cleanup":"asynchronous"}))
    }
    pub async fn maintain(&self) -> Result<()> {
        let expired:Vec<(Uuid,Uuid,Uuid)>=sqlx::query_as("SELECT tenant_id,key_id,id FROM assets WHERE state<>'deleted' AND (expires_at<=clock_timestamp() OR (state='uploading' AND upload_expires_at<=clock_timestamp())) ORDER BY expires_at LIMIT 100").fetch_all(&self.pool).await?;
        for (tenant_id, key_id, id) in expired {
            let who = Identity { tenant_id, key_id };
            let mut tx = self.pool.begin().await?;
            accounting::lock_accounts(&mut tx, &who).await?;
            let a = asset(&mut tx, &who, id).await?;
            if a.expires_at <= Utc::now()
                || (a.state == "uploading" && a.upload_expires_at <= Utc::now())
            {
                tombstone(&mut tx, &a).await?;
            }
            tx.commit().await?;
        }
        // Keep tombstones and periodically reconcile their known object keys.
        // A late object-store PUT cannot resurrect a logically deleted asset.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let mut failed = false;
        let garbage:Vec<(Uuid,Uuid,i32)>=sqlx::query_as("SELECT tenant_id,id,gc_cursor FROM assets WHERE state='deleted' AND gc_after<=clock_timestamp() ORDER BY gc_after LIMIT 16").fetch_all(&self.pool).await?;
        for (tenant, id, cursor) in garbage {
            let chunks: Vec<Chunk> =
                sqlx::query_as("SELECT * FROM asset_chunks WHERE tenant_id=$1 AND asset_id=$2 AND ordinal >= $3 ORDER BY ordinal")
                    .bind(tenant)
                    .bind(id)
                    .bind(cursor)
                    .fetch_all(&self.pool)
                    .await?;
            let mut complete = true;
            for c in chunks {
                if tokio::time::Instant::now() >= deadline {
                    return if failed { Err(Error::Storage) } else { Ok(()) };
                }
                match tokio::time::timeout_at(deadline, self.store.delete(&location(&c))).await {
                    Ok(Ok(())) | Ok(Err(object_store::Error::NotFound { .. })) => {}
                    _ => {
                        sqlx::query("UPDATE assets SET gc_after=clock_timestamp()+interval '5 minutes' WHERE tenant_id=$1 AND id=$2").bind(tenant).bind(id).execute(&self.pool).await?;
                        failed = true;
                        complete = false;
                        break;
                    }
                }
                sqlx::query("UPDATE assets SET gc_cursor=GREATEST(gc_cursor,$3) WHERE tenant_id=$1 AND id=$2").bind(tenant).bind(id).bind(c.ordinal+1).execute(&self.pool).await?;
            }
            if complete {
                sqlx::query("UPDATE assets SET gc_after=clock_timestamp()+interval '24 hours',gc_checked_at=clock_timestamp(),gc_cursor=0 WHERE tenant_id=$1 AND id=$2 AND state='deleted'").bind(tenant).bind(id).execute(&self.pool).await?;
            }
        }
        if failed { Err(Error::Storage) } else { Ok(()) }
    }
}

fn input_only(a: &Asset) -> Result<()> {
    if a.kind == "input" {
        Ok(())
    } else {
        Err(Error::NotFound)
    }
}
async fn write_authority(
    tx: &mut Tx<'_>,
    who: &Identity,
    a: &Asset,
    owner: Option<Uuid>,
) -> Result<()> {
    match owner {
        None => input_only(a),
        Some(token) if a.kind == "output" => {
            crate::outputs::write_authority(tx, who, a.id, token).await
        }
        Some(_) => Err(Error::NotFound),
    }
}
pub(crate) async fn tombstone_output(tx: &mut Tx<'_>, who: &Identity, id: Uuid) -> Result<()> {
    let a = asset(tx, who, id).await?;
    if a.kind != "output" {
        return Err(Error::NotFound);
    }
    tombstone(tx, &a).await
}

async fn authorize(tx: &mut Tx<'_>, who: &Identity) -> Result<()> {
    accounting::lock_accounts(tx, who).await?;
    let allowed:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM tenants t JOIN virtual_keys k ON k.tenant_id=t.id WHERE t.id=$1 AND k.id=$2 AND t.active AND k.revoked_at IS NULL AND (k.expires_at IS NULL OR k.expires_at>clock_timestamp()))").bind(who.tenant_id).bind(who.key_id).fetch_one(&mut **tx).await?;
    if !allowed {
        return Err(Error::Unauthorized);
    }
    Ok(())
}
async fn asset(tx: &mut Tx<'_>, who: &Identity, id: Uuid) -> Result<Asset> {
    sqlx::query_as("SELECT * FROM assets WHERE tenant_id=$1 AND key_id=$2 AND id=$3 FOR UPDATE")
        .bind(who.tenant_id)
        .bind(who.key_id)
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(Error::NotFound)
}
async fn chunk(tx: &mut Tx<'_>, tenant: Uuid, id: Uuid, ordinal: i32) -> Result<Chunk> {
    sqlx::query_as("SELECT * FROM asset_chunks WHERE tenant_id=$1 AND asset_id=$2 AND ordinal=$3")
        .bind(tenant)
        .bind(id)
        .bind(ordinal)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(Error::NotFound)
}
fn location(c: &Chunk) -> Path {
    Path::from(format!("assets-v1/{}/{}/{}", c.tenant_id, c.asset_id, c.id))
}
fn writable(a: &Asset) -> Result<()> {
    if a.state != "uploading" || a.upload_expires_at <= Utc::now() || a.expires_at <= Utc::now() {
        Err(Error::ResultUnavailable)
    } else {
        Ok(())
    }
}
fn readable(a: &Asset) -> Result<()> {
    if a.state != "available" || a.expires_at <= Utc::now() {
        Err(Error::ResultUnavailable)
    } else {
        Ok(())
    }
}
fn view(a: &Asset) -> Value {
    let state = if a.state != "deleted"
        && (a.expires_at <= Utc::now()
            || (a.state == "uploading" && a.upload_expires_at <= Utc::now()))
    {
        "expired"
    } else {
        &a.state
    };
    json!({"id":a.id,"kind":a.kind,"state":state,"byte_length":a.byte_length,"sha256":a.sha256.as_ref().map(|h|governance::hex(h)),"declared_media_type":a.declared_media_type,"content_safety":"not_evaluated","chunk_bytes":CHUNK_BYTES,"chunk_count":a.chunk_count,"upload_expires_at":a.upload_expires_at,"expires_at":a.expires_at})
}
fn decode_hash(s: &str) -> Result<Vec<u8>> {
    if s.len() != 64
        || !s
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(Error::Invalid("expected lowercase SHA-256 hex"));
    }
    s.as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| {
            u8::from_str_radix(
                std::str::from_utf8(b).map_err(|_| Error::Invalid("invalid hash"))?,
                16,
            )
            .map_err(|_| Error::Invalid("invalid hash"))
        })
        .collect()
}
fn valid_media_type(s: &str) -> bool {
    matches!(
        s,
        "image/png"
            | "image/jpeg"
            | "image/webp"
            | "image/gif"
            | "video/mp4"
            | "video/webm"
            | "audio/wav"
            | "audio/mpeg"
            | "audio/ogg"
            | "model/gltf-binary"
            | "application/octet-stream"
    )
}
async fn audit(
    tx: &mut Tx<'_>,
    who: &Identity,
    action: &str,
    id: Uuid,
    details: Value,
) -> Result<()> {
    sqlx::query("INSERT INTO audit_events(tenant_id,action,object_id,details) VALUES($1,$2,$3,$4)")
        .bind(who.tenant_id)
        .bind(action)
        .bind(id.to_string())
        .bind(json!({"key_id":who.key_id,"details":details}))
        .execute(&mut **tx)
        .await?;
    Ok(())
}
async fn tombstone(tx: &mut Tx<'_>, a: &Asset) -> Result<()> {
    if a.state == "deleted" {
        return Ok(());
    }
    sqlx::query("UPDATE assets SET state='deleted',deleted_at=clock_timestamp(),gc_after=clock_timestamp()+interval '2 minutes' WHERE tenant_id=$1 AND id=$2").bind(a.tenant_id).bind(a.id).execute(&mut **tx).await?;
    sqlx::query("UPDATE tenants SET asset_used_bytes=asset_used_bytes-$2 WHERE id=$1")
        .bind(a.tenant_id)
        .bind(a.byte_length)
        .execute(&mut **tx)
        .await?;
    sqlx::query(
        "UPDATE virtual_keys SET asset_used_bytes=asset_used_bytes-$3 WHERE tenant_id=$1 AND id=$2",
    )
    .bind(a.tenant_id)
    .bind(a.key_id)
    .bind(a.byte_length)
    .execute(&mut **tx)
    .await?;
    audit(
        tx,
        &Identity {
            tenant_id: a.tenant_id,
            key_id: a.key_id,
        },
        "asset.deleted",
        a.id,
        json!({"bytes":a.byte_length,"physical_cleanup":"pending"}),
    )
    .await
}
pub async fn set_quota(pool: &PgPool, tenant: Uuid, key: Option<Uuid>, bytes: i64) -> Result<()> {
    if !(0..=10995116277760).contains(&bytes) {
        return Err(Error::Invalid("invalid asset quota"));
    }
    let mut tx = pool.begin().await?;
    governance::tenant_lock(&mut tx, tenant).await?;
    let n = if let Some(key) = key {
        sqlx::query("UPDATE virtual_keys SET asset_quota_bytes=$3 WHERE tenant_id=$1 AND id=$2 AND asset_used_bytes<=$3").bind(tenant).bind(key).bind(bytes).execute(&mut *tx).await?
    } else {
        sqlx::query("UPDATE tenants SET asset_quota_bytes=$2 WHERE id=$1 AND asset_used_bytes<=$2")
            .bind(tenant)
            .bind(bytes)
            .execute(&mut *tx)
            .await?
    };
    if n.rows_affected() != 1 {
        return Err(Error::Invalid(
            "quota below current use or account not found",
        ));
    }
    sqlx::query("INSERT INTO audit_events(tenant_id,action,object_id,details) VALUES($1,'asset.quota_set',$2,$3)").bind(tenant).bind(key.unwrap_or(tenant).to_string()).bind(json!({"key_id":key,"quota_bytes":bytes})).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}
