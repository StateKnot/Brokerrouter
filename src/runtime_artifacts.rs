//! Native output bytes captured under the original claim into private assets.
//! Quarantine is a storage fact, never a scan verdict or publication grant.
use crate::{
    accounting,
    assets::{Assets, CHUNK_BYTES},
    cipher::ResultCipher,
    error::{Error, Result},
    governance::{self, Operation, Tx},
    store::{self, Identity},
};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;
use uuid::Uuid;

pub const MAX_BYTES: i64 = 64 * 1024 * 1024;
pub const MAX_ARTIFACTS: usize = 8;
pub(crate) const ASSET_PREFIX: &str = "_br_runtime_artifact:";

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Reserve {
    pub input_operation_id: Uuid,
    pub claim_token: String,
    pub input_payload_hash: String,
    pub original_payload_hash: String,
    pub maximum_bytes: i64,
    pub maximum_artifacts: i32,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct File {
    pub sha256: String,
    pub byte_length: i64,
    pub media_type: String,
    /// Deployment/producer revision is evidence, not format certification.
    pub producer_revision: String,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub claim_token: String,
    pub evidence_ref: String,
    pub artifacts: Vec<File>,
}
#[derive(Clone, sqlx::FromRow)]
struct Batch {
    tenant_id: Uuid,
    id: Uuid,
    key_id: Uuid,
    turn_id: Uuid,
    input_operation_id: Uuid,
    model_id: Option<String>,
    tool_name: String,
    claim_hash: Vec<u8>,
    input_payload_hash: Vec<u8>,
    original_payload_hash: Vec<u8>,
    maximum_bytes: i64,
    maximum_artifacts: i32,
    reserved_bytes: i64,
    manifest_hash: Option<Vec<u8>>,
    receipt_hash: Option<Vec<u8>>,
    evidence_ref: Option<String>,
    state: String,
    upload_expires_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
}
#[derive(sqlx::FromRow)]
struct Artifact {
    ordinal: i32,
    asset_id: Uuid,
    sha256: Vec<u8>,
    byte_length: i64,
    declared_media_type: String,
    producer_revision: Vec<u8>,
}
#[derive(Clone)]
pub struct RuntimeArtifacts {
    pub assets: Assets,
    cipher: Arc<ResultCipher>,
}
impl RuntimeArtifacts {
    pub fn new(assets: Assets, cipher: ResultCipher) -> Self {
        Self {
            assets,
            cipher: Arc::new(cipher),
        }
    }
    pub async fn reserve(&self, who: &Identity, request: Reserve) -> Result<Value> {
        if !(1..=MAX_BYTES).contains(&request.maximum_bytes)
            || !(1..=MAX_ARTIFACTS as i32).contains(&request.maximum_artifacts)
        {
            return Err(Error::Invalid("unsupported runtime artifact capacity"));
        }
        let claim = claim_hash(&request.claim_token)?;
        let payload = decode_hash(&request.input_payload_hash)?;
        let original = decode_hash(&request.original_payload_hash)?;
        let ttl = ttl_seconds()?;
        let mut tx = self.assets.pool.begin().await?;
        governance::tenant_lock(&mut tx, who.tenant_id).await?;
        accounting::lock_accounts(&mut tx, who).await?;
        let op: Operation = sqlx::query_as("SELECT * FROM governance_operations WHERE tenant_id=$1 AND key_id=$2 AND id=$3 FOR UPDATE")
            .bind(who.tenant_id).bind(who.key_id).bind(request.input_operation_id)
            .fetch_optional(&mut *tx).await?.ok_or(Error::NotFound)?;
        if op.stage != "tool_input"
            || op.tool_name.is_none()
            || op.claim_hash.as_ref() != Some(&claim)
        {
            return Err(Error::NotFound);
        }
        if op.payload_hash != payload {
            return Err(Error::Conflict);
        }
        let turn = governance::turn_locked(&mut tx, who, op.turn_id, false).await?;
        require_model(&mut tx, who, op.model_id.as_deref()).await?;
        let existing: Option<Batch> = sqlx::query_as("SELECT * FROM runtime_artifact_batches WHERE tenant_id=$1 AND input_operation_id=$2 FOR UPDATE")
            .bind(who.tenant_id).bind(op.id).fetch_optional(&mut *tx).await?;
        if let Some(b) = existing {
            if b.key_id != who.key_id
                || b.turn_id != turn.id
                || b.claim_hash != claim
                || b.input_payload_hash != payload
                || b.original_payload_hash != original
                || b.model_id != op.model_id
                || op.tool_name.as_deref() != Some(b.tool_name.as_str())
                || b.maximum_bytes != request.maximum_bytes
                || b.maximum_artifacts != request.maximum_artifacts
            {
                return Err(Error::Conflict);
            }
            let value = metadata(&mut tx, who, &b).await?;
            tx.commit().await?;
            return Ok(value);
        }
        governance::turn_locked(&mut tx, who, op.turn_id, true).await?;
        let live: bool = sqlx::query_scalar("SELECT state='executing' AND execution_deadline>clock_timestamp() FROM governance_operations WHERE tenant_id=$1 AND id=$2")
            .bind(who.tenant_id).bind(op.id).fetch_one(&mut *tx).await?;
        if !live {
            return Err(Error::InProgress);
        }
        adjust_capacity(&mut tx, who, request.maximum_bytes).await?;
        let b: Batch = sqlx::query_as("INSERT INTO runtime_artifact_batches(tenant_id,id,key_id,turn_id,input_operation_id,model_id,tool_name,claim_hash,input_payload_hash,original_payload_hash,maximum_bytes,maximum_artifacts,reserved_bytes,created_at,upload_expires_at,expires_at) SELECT $1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$11,s.ts,s.ts+make_interval(secs=>LEAST($13::integer,3600)),s.ts+make_interval(secs=>$13::integer) FROM (SELECT clock_timestamp() ts) s RETURNING *")
            .bind(who.tenant_id).bind(Uuid::new_v4()).bind(who.key_id).bind(turn.id).bind(op.id)
            .bind(op.model_id).bind(op.tool_name.ok_or(Error::Configuration)?).bind(claim).bind(payload).bind(original)
            .bind(request.maximum_bytes).bind(request.maximum_artifacts).bind(ttl)
            .fetch_one(&mut *tx).await?;
        audit(&mut tx, who, b.id, "runtime_artifact.reserved", json!({"input_operation_id":op.id,"maximum_bytes":b.maximum_bytes,"maximum_artifacts":b.maximum_artifacts})).await?;
        require_live(&mut tx, &b, false).await?;
        let value = metadata(&mut tx, who, &b).await?;
        require_live(&mut tx, &b, false).await?;
        let live: bool = sqlx::query_scalar("SELECT state='executing' AND execution_deadline>clock_timestamp() FROM governance_operations WHERE tenant_id=$1 AND id=$2")
            .bind(who.tenant_id).bind(b.input_operation_id).fetch_one(&mut *tx).await?;
        if !live {
            return Err(Error::InProgress);
        }
        tx.commit().await?;
        Ok(value)
    }
    /// The manifest and original completed receipt commit together. No tool is
    /// claimed or executed here, including lost-response retries.
    pub async fn record_manifest(
        &self,
        who: &Identity,
        id: Uuid,
        request: Manifest,
    ) -> Result<Value> {
        let claim = claim_hash(&request.claim_token)?;
        if !governance::valid_text(&request.evidence_ref, 1000)
            || request.artifacts.is_empty()
            || request.artifacts.len() > MAX_ARTIFACTS
        {
            return Err(Error::Invalid("invalid runtime artifact manifest"));
        }
        let mut total = 0_i64;
        for f in &request.artifacts {
            if f.media_type != "video/mp4" || !(1..=MAX_BYTES).contains(&f.byte_length) {
                return Err(Error::Invalid(
                    "unsupported runtime artifact; only bounded MP4 candidates are accepted",
                ));
            }
            decode_hash(&f.sha256)?;
            decode_hash(&f.producer_revision)?;
            total = total.checked_add(f.byte_length).ok_or(Error::AssetQuota)?;
        }
        if total > MAX_BYTES {
            return Err(Error::AssetQuota);
        }
        let manifest_hash = governance::digest(&json!(request.artifacts))?;
        let receipt_hash = governance::digest(
            &json!({"outcome":"completed","evidence_ref":request.evidence_ref}),
        )?;
        let mut tx = self.assets.pool.begin().await?;
        let b = locked(&mut tx, who, id, &claim).await?;
        if let Some(hash) = &b.manifest_hash {
            if hash != &manifest_hash
                || b.receipt_hash.as_ref() != Some(&receipt_hash)
                || b.evidence_ref.as_deref() != Some(request.evidence_ref.as_str())
            {
                return Err(Error::Conflict);
            }
            let value = metadata(&mut tx, who, &b).await?;
            tx.commit().await?;
            return Ok(value);
        }
        require_live(&mut tx, &b, true).await?;
        if b.state != "reserved" {
            return Err(Error::ResultUnavailable);
        }
        if total > b.reserved_bytes || request.artifacts.len() > b.maximum_artifacts as usize {
            return Err(Error::AssetQuota);
        }
        governance::receipt_in_tx(
            &mut tx,
            &self.cipher,
            who,
            b.input_operation_id,
            &governance::Receipt {
                token: request.claim_token,
                outcome: "completed".into(),
                evidence_ref: request.evidence_ref.clone(),
            },
        )
        .await?;
        adjust_capacity(&mut tx, who, total - b.reserved_bytes).await?;
        let b: Batch=sqlx::query_as("UPDATE runtime_artifact_batches SET state='capturing',reserved_bytes=0,manifest_hash=$3,receipt_hash=$4,evidence_ref=$5 WHERE tenant_id=$1 AND id=$2 RETURNING *")
            .bind(who.tenant_id).bind(id).bind(manifest_hash).bind(receipt_hash).bind(request.evidence_ref)
            .fetch_one(&mut *tx).await?;
        for (ordinal, f) in request.artifacts.iter().enumerate() {
            let asset = Uuid::new_v4();
            let hash = decode_hash(&f.sha256)?;
            let producer = decode_hash(&f.producer_revision)?;
            let digest =
                governance::digest(&json!({"batch_id":id,"ordinal":ordinal,"artifact":f}))?;
            let chunks = ((f.byte_length + CHUNK_BYTES as i64 - 1) / CHUNK_BYTES as i64) as i32;
            sqlx::query("INSERT INTO assets(tenant_id,id,key_id,idempotency_key,request_hash,sha256,byte_length,declared_media_type,chunk_count,upload_expires_at,expires_at,kind,source_operation_id) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,'runtime_output',$12)")
                .bind(who.tenant_id).bind(asset).bind(who.key_id).bind(format!("{ASSET_PREFIX}{id}:{ordinal}"))
                .bind(digest).bind(&hash).bind(f.byte_length).bind(&f.media_type).bind(chunks)
                .bind(b.upload_expires_at).bind(b.expires_at).bind(b.input_operation_id).execute(&mut *tx).await?;
            sqlx::query("INSERT INTO runtime_artifacts(tenant_id,batch_id,key_id,ordinal,asset_id,source_operation_id,sha256,byte_length,declared_media_type,producer_revision) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)")
                .bind(who.tenant_id).bind(id).bind(who.key_id).bind(ordinal as i32).bind(asset)
                .bind(b.input_operation_id).bind(hash).bind(f.byte_length).bind(&f.media_type).bind(producer)
                .execute(&mut *tx).await?;
        }
        audit(&mut tx,who,id,"runtime_artifact.manifest_recorded",json!({"input_operation_id":b.input_operation_id,"artifacts":request.artifacts.len(),"bytes":total})).await?;
        require_live(&mut tx, &b, true).await?;
        let value = metadata(&mut tx, who, &b).await?;
        require_live(&mut tx, &b, true).await?;
        tx.commit().await?;
        Ok(value)
    }
    pub async fn metadata(&self, who: &Identity, id: Uuid, token: &str) -> Result<Value> {
        let claim = claim_hash(token)?;
        let mut tx = self.assets.pool.begin().await?;
        let b = locked(&mut tx, who, id, &claim).await?;
        let value = metadata(&mut tx, who, &b).await?;
        tx.commit().await?;
        Ok(value)
    }
    pub async fn put_chunk(
        &self,
        who: &Identity,
        id: Uuid,
        ordinal: i32,
        chunk: i32,
        token: &str,
        bytes: Bytes,
    ) -> Result<Value> {
        let claim = claim_hash(token)?;
        let mut tx = self.assets.pool.begin().await?;
        let b = locked(&mut tx, who, id, &claim).await?;
        require_live(&mut tx, &b, true).await?;
        if b.state != "capturing" {
            return Err(Error::ResultUnavailable);
        }
        let asset = artifact_id(&mut tx, who, id, ordinal).await?;
        tx.commit().await?;
        self.assets
            .put_runtime_chunk(who, asset, chunk, bytes, id, claim)
            .await
    }
    /// Re-read authenticated storage even on replay. A database available flag
    /// cannot bless missing/corrupt bytes. Success remains quarantined.
    pub async fn complete(&self, who: &Identity, id: Uuid, token: &str) -> Result<Value> {
        let claim = claim_hash(token)?;
        let mut tx = self.assets.pool.begin().await?;
        let b = locked(&mut tx, who, id, &claim).await?;
        require_live(&mut tx, &b, b.state != "quarantined").await?;
        if !matches!(b.state.as_str(), "capturing" | "quarantined") {
            return Err(Error::InProgress);
        }
        let files = artifacts(&mut tx, who, id).await?;
        if files.is_empty() {
            return Err(Error::InProgress);
        }
        tx.commit().await?;
        for f in &files {
            self.assets
                .complete_runtime(who, f.asset_id, id, claim.clone())
                .await?;
        }
        let mut tx = self.assets.pool.begin().await?;
        let b = locked(&mut tx, who, id, &claim).await?;
        require_live(&mut tx, &b, b.state != "quarantined").await?;
        let all:bool=sqlx::query_scalar("SELECT NOT EXISTS(SELECT 1 FROM runtime_artifacts f JOIN assets a ON a.tenant_id=f.tenant_id AND a.id=f.asset_id WHERE f.tenant_id=$1 AND f.batch_id=$2 AND (a.state<>'available' OR a.expires_at<=clock_timestamp()))")
            .bind(who.tenant_id).bind(id).fetch_one(&mut *tx).await?;
        if !all {
            return Err(Error::ResultUnavailable);
        }
        if b.state == "capturing" {
            sqlx::query("UPDATE runtime_artifact_batches SET state='quarantined' WHERE tenant_id=$1 AND id=$2")
                .bind(who.tenant_id).bind(id).execute(&mut *tx).await?;
            audit(
                &mut tx,
                who,
                id,
                "runtime_artifact.quarantined",
                json!({"artifacts":files.len(),"content_safety":"not_evaluated"}),
            )
            .await?;
        }
        require_live(&mut tx, &b, b.state != "quarantined").await?;
        let mut current = b;
        current.state = "quarantined".into();
        let value = metadata(&mut tx, who, &current).await?;
        require_live(&mut tx, &current, false).await?;
        tx.commit().await?;
        Ok(value)
    }
    pub async fn maintain(&self) -> Result<()> {
        let expired:Vec<(Uuid,Uuid,Uuid)>=sqlx::query_as("SELECT tenant_id,key_id,id FROM runtime_artifact_batches WHERE state<>'expired' AND (expires_at<=clock_timestamp() OR (state<>'quarantined' AND upload_expires_at<=clock_timestamp())) ORDER BY expires_at,id LIMIT 100")
            .fetch_all(&self.assets.pool).await?;
        for (tenant_id, key_id, id) in expired {
            let who = Identity { tenant_id, key_id };
            let mut tx = self.assets.pool.begin().await?;
            accounting::lock_accounts(&mut tx, &who).await?;
            let b: Batch = sqlx::query_as(
                "SELECT * FROM runtime_artifact_batches WHERE tenant_id=$1 AND id=$2 FOR UPDATE",
            )
            .bind(tenant_id)
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
            let expired: bool = sqlx::query_scalar(
                "SELECT $1<=clock_timestamp() OR ($2<>'quarantined' AND $3<=clock_timestamp())",
            )
            .bind(b.expires_at)
            .bind(&b.state)
            .bind(b.upload_expires_at)
            .fetch_one(&mut *tx)
            .await?;
            if b.state != "expired" && expired {
                adjust_capacity(&mut tx, &who, -b.reserved_bytes).await?;
                sqlx::query("UPDATE runtime_artifact_batches SET state='expired',reserved_bytes=0 WHERE tenant_id=$1 AND id=$2")
                    .bind(tenant_id).bind(id).execute(&mut *tx).await?;
                for f in artifacts(&mut tx, &who, id).await? {
                    crate::assets::tombstone_runtime(&mut tx, &who, f.asset_id).await?;
                }
                audit(
                    &mut tx,
                    &who,
                    id,
                    "runtime_artifact.expired",
                    json!({"input_operation_id":b.input_operation_id}),
                )
                .await?;
            }
            tx.commit().await?;
        }
        self.assets.maintain().await
    }
}

async fn locked(tx: &mut Tx<'_>, who: &Identity, id: Uuid, claim: &[u8]) -> Result<Batch> {
    governance::tenant_lock(tx, who.tenant_id).await?;
    accounting::lock_accounts(tx, who).await?;
    let b:Batch=sqlx::query_as("SELECT * FROM runtime_artifact_batches WHERE tenant_id=$1 AND key_id=$2 AND id=$3 FOR UPDATE")
        .bind(who.tenant_id).bind(who.key_id).bind(id).fetch_optional(&mut **tx).await?.ok_or(Error::NotFound)?;
    if b.claim_hash != claim {
        return Err(Error::NotFound);
    }
    governance::turn_locked(tx, who, b.turn_id, false).await?;
    require_model(tx, who, b.model_id.as_deref()).await?;
    Ok(b)
}
async fn require_model(tx: &mut Tx<'_>, who: &Identity, model: Option<&str>) -> Result<()> {
    if let Some(model) = model {
        let allowed:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM key_models g JOIN models m ON m.tenant_id=g.tenant_id AND m.id=g.model_id WHERE g.tenant_id=$1 AND g.key_id=$2 AND g.model_id=$3 AND m.active)")
            .bind(who.tenant_id).bind(who.key_id).bind(model).fetch_one(&mut **tx).await?;
        if !allowed {
            return Err(Error::Unauthorized);
        }
    }
    Ok(())
}
async fn require_live(tx: &mut Tx<'_>, b: &Batch, upload: bool) -> Result<()> {
    let live:bool=sqlx::query_scalar("SELECT $1<> 'expired' AND $2>clock_timestamp() AND (NOT $3 OR $4>clock_timestamp()) AND EXISTS(SELECT 1 FROM tenants t JOIN virtual_keys k ON k.tenant_id=t.id JOIN runtime_bindings r ON r.tenant_id=k.tenant_id AND r.key_id=k.id JOIN governance_users u ON u.tenant_id=r.tenant_id AND u.id=r.user_id WHERE t.id=$5 AND k.id=$6 AND t.active AND k.revoked_at IS NULL AND (k.expires_at IS NULL OR k.expires_at>clock_timestamp()) AND r.active AND u.active)")
        .bind(&b.state).bind(b.expires_at).bind(upload).bind(b.upload_expires_at).bind(b.tenant_id).bind(b.key_id).fetch_one(&mut **tx).await?;
    if !live {
        return Err(Error::ResultUnavailable);
    }
    Ok(())
}
pub(crate) async fn write_authority(
    tx: &mut Tx<'_>,
    who: &Identity,
    asset: Uuid,
    batch: Uuid,
    claim: &[u8],
) -> Result<()> {
    let b = locked(tx, who, batch, claim).await?;
    require_live(tx, &b, b.state != "quarantined").await?;
    if !matches!(b.state.as_str(), "capturing" | "quarantined") {
        return Err(Error::ResultUnavailable);
    }
    let matches:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM runtime_artifacts f JOIN assets a ON a.tenant_id=f.tenant_id AND a.id=f.asset_id WHERE f.tenant_id=$1 AND f.batch_id=$2 AND f.key_id=$3 AND f.asset_id=$4 AND a.kind='runtime_output' AND a.source_operation_id=$5 AND a.sha256=f.sha256 AND a.byte_length=f.byte_length)")
        .bind(who.tenant_id).bind(batch).bind(who.key_id).bind(asset).bind(b.input_operation_id).fetch_one(&mut **tx).await?;
    if !matches {
        return Err(Error::NotFound);
    }
    require_live(tx, &b, b.state != "quarantined").await?;
    Ok(())
}
async fn artifacts(tx: &mut Tx<'_>, who: &Identity, id: Uuid) -> Result<Vec<Artifact>> {
    Ok(sqlx::query_as("SELECT * FROM runtime_artifacts WHERE tenant_id=$1 AND key_id=$2 AND batch_id=$3 ORDER BY ordinal")
        .bind(who.tenant_id).bind(who.key_id).bind(id).fetch_all(&mut **tx).await?)
}
async fn artifact_id(tx: &mut Tx<'_>, who: &Identity, id: Uuid, ordinal: i32) -> Result<Uuid> {
    sqlx::query_scalar("SELECT asset_id FROM runtime_artifacts WHERE tenant_id=$1 AND key_id=$2 AND batch_id=$3 AND ordinal=$4")
        .bind(who.tenant_id).bind(who.key_id).bind(id).bind(ordinal).fetch_optional(&mut **tx).await?.ok_or(Error::NotFound)
}
async fn metadata(tx: &mut Tx<'_>, who: &Identity, b: &Batch) -> Result<Value> {
    let execution: (String, Option<DateTime<Utc>>) = sqlx::query_as(
        "SELECT state,execution_deadline FROM governance_operations WHERE tenant_id=$1 AND key_id=$2 AND id=$3",
    ).bind(who.tenant_id).bind(who.key_id).bind(b.input_operation_id)
        .fetch_one(&mut **tx).await?;
    let live: bool = sqlx::query_scalar(
        "SELECT $1>clock_timestamp() AND ($2='quarantined' OR $3>clock_timestamp())",
    )
    .bind(b.expires_at)
    .bind(&b.state)
    .bind(b.upload_expires_at)
    .fetch_one(&mut **tx)
    .await?;
    let files = artifacts(tx, who, b.id).await?;
    Ok(
        json!({"id":b.id,"input_operation_id":b.input_operation_id,"turn_id":b.turn_id,
        "state":if live {b.state.as_str()} else {"expired"},"maximum_bytes":b.maximum_bytes,
        "maximum_artifacts":b.maximum_artifacts,"manifest_sha256":b.manifest_hash.as_ref().map(|v|governance::hex(v)),
        "content_safety":"not_evaluated","upload_expires_at":b.upload_expires_at,"expires_at":b.expires_at,
        "source":{"input_operation_id":b.input_operation_id,"turn_id":b.turn_id,"model_id":b.model_id,
            "tool_name":b.tool_name,"input_payload_sha256":governance::hex(&b.input_payload_hash),
            "original_payload_sha256":governance::hex(&b.original_payload_hash)},
        "execution":{"state":execution.0,"deadline":execution.1},
        "artifacts":files.into_iter().map(|f|json!({"ordinal":f.ordinal,"id":f.asset_id,"sha256":governance::hex(&f.sha256),
            "byte_length":f.byte_length,"media_type":f.declared_media_type,"producer_revision":governance::hex(&f.producer_revision),
            "chunk_bytes":CHUNK_BYTES,"chunk_count":(f.byte_length+CHUNK_BYTES as i64-1)/CHUNK_BYTES as i64})).collect::<Vec<_>>()}),
    )
}
async fn adjust_capacity(tx: &mut Tx<'_>, who: &Identity, bytes: i64) -> Result<()> {
    let t=sqlx::query("UPDATE tenants SET asset_used_bytes=asset_used_bytes+$2 WHERE id=$1 AND asset_used_bytes+$2 BETWEEN 0 AND asset_quota_bytes")
        .bind(who.tenant_id).bind(bytes).execute(&mut **tx).await?;
    let k=sqlx::query("UPDATE virtual_keys SET asset_used_bytes=asset_used_bytes+$3 WHERE tenant_id=$1 AND id=$2 AND asset_used_bytes+$3 BETWEEN 0 AND asset_quota_bytes")
        .bind(who.tenant_id).bind(who.key_id).bind(bytes).execute(&mut **tx).await?;
    if t.rows_affected() != 1 || k.rows_affected() != 1 {
        return Err(Error::AssetQuota);
    }
    Ok(())
}
async fn audit(
    tx: &mut Tx<'_>,
    who: &Identity,
    id: Uuid,
    action: &str,
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
fn claim_hash(token: &str) -> Result<Vec<u8>> {
    decode_hash(token)?;
    Ok(store::hash(token.as_bytes()))
}
fn decode_hash(value: &str) -> Result<Vec<u8>> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(Error::Invalid("expected lowercase SHA-256 hex"));
    }
    value
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|v| {
            u8::from_str_radix(
                std::str::from_utf8(v).map_err(|_| Error::Invalid("invalid hash"))?,
                16,
            )
            .map_err(|_| Error::Invalid("invalid hash"))
        })
        .collect()
}
fn ttl_seconds() -> Result<i32> {
    let value = match std::env::var("BROKERROUTER_RUNTIME_ARTIFACT_TTL_SECONDS") {
        Ok(v) => v.parse().map_err(|_| Error::Configuration)?,
        Err(std::env::VarError::NotPresent) => 86400,
        Err(_) => return Err(Error::Configuration),
    };
    if !(60..=604800).contains(&value) {
        return Err(Error::Configuration);
    }
    Ok(value)
}
