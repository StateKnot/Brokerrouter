//! Durable provider-to-private-storage import. Quarantine is never a delivery grant.
use crate::{
    accounting,
    assets::{self, Assets, CHUNK_BYTES},
    cipher::ResultCipher,
    error::{Error, Result},
    governance,
    media_protocol::Endpoint,
    store::Identity,
};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, Transaction};
use std::{sync::Arc, time::Duration};
use uuid::Uuid;
type Tx<'a> = Transaction<'a, Postgres>;

#[derive(sqlx::FromRow)]
struct Import {
    job_id: Uuid,
    maximum_bytes: i64,
    reserved_bytes: i64,
    asset_id: Option<Uuid>,
    source_etag: Option<String>,
    source_hash: Option<Vec<u8>>,
    state: String,
    lease_token: Option<Uuid>,
    lease_valid: bool,
    due: bool,
    expires_at: DateTime<Utc>,
}
#[derive(sqlx::FromRow)]
struct Source {
    state: String,
    cancel_requested: bool,
    output_ciphertext: Option<Vec<u8>>,
    output_expires_at: Option<DateTime<Utc>>,
    routing_snapshot: Option<Value>,
    governance_turn_id: Option<Uuid>,
}
#[derive(Clone)]
pub struct Outputs {
    pub assets: Assets,
    source_cipher: Arc<ResultCipher>,
}
impl Outputs {
    pub fn new(assets: Assets, source_cipher: ResultCipher) -> Self {
        Self {
            assets,
            source_cipher: Arc::new(source_cipher),
        }
    }
    pub fn from_env(pool: PgPool) -> Result<Self> {
        Ok(Self::new(
            Assets::from_env(pool)?,
            ResultCipher::from_env()?,
        ))
    }
    pub async fn tick(&self) -> Result<bool> {
        self.maintain().await?;
        let candidates: Vec<(Uuid,Uuid,Uuid)> = sqlx::query_as("SELECT i.tenant_id,i.key_id,i.job_id FROM media_output_imports i JOIN media_jobs j ON j.tenant_id=i.tenant_id AND j.id=i.job_id WHERE i.state IN ('pending','copying') AND j.state='output_pending' AND i.next_attempt_at<=clock_timestamp() AND (i.lease_until IS NULL OR i.lease_until<=clock_timestamp()) ORDER BY i.next_attempt_at,i.job_id LIMIT 16").fetch_all(&self.assets.pool).await?;
        for (tenant_id, key_id, id) in candidates {
            let who = Identity { tenant_id, key_id };
            let mut tx = self.assets.pool.begin().await?;
            let (i, s) = locked(&mut tx, &who, id).await?;
            if i.lease_valid
                || !i.due
                || !matches!(i.state.as_str(), "pending" | "copying")
                || s.state != "output_pending"
            {
                continue;
            }
            active(&mut tx, &who, &s).await?;
            let token = Uuid::new_v4();
            sqlx::query("UPDATE media_output_imports SET lease_token=$3,lease_until=clock_timestamp()+interval '660 seconds' WHERE tenant_id=$1 AND job_id=$2").bind(tenant_id).bind(id).bind(token).execute(&mut *tx).await?;
            tx.commit().await?;
            let result =
                tokio::time::timeout(Duration::from_secs(600), self.copy(&who, i, s, token))
                    .await
                    .unwrap_or(Err(Error::Storage));
            match result {
                Ok(()) => {}
                Err(Error::InProgress) => return Err(Error::InProgress),
                Err(Error::Database(e)) => return Err(Error::Database(e)),
                Err(Error::Storage) => self.retry(&who, id, token).await?,
                Err(Error::Unauthorized | Error::TurnClosed | Error::ActionRejected) => {
                    self.stop(
                        &who,
                        id,
                        Some(token),
                        "cancelled",
                        "output_authority_revoked",
                    )
                    .await?
                }
                Err(_) => {
                    self.stop(
                        &who,
                        id,
                        Some(token),
                        "failed",
                        "output_source_contract_failed",
                    )
                    .await?
                }
            }
            return Ok(true);
        }
        Ok(false)
    }
    async fn copy(&self, who: &Identity, i: Import, s: Source, token: Uuid) -> Result<()> {
        if let Some(asset) = i.asset_id {
            let ready: bool = sqlx::query_scalar("SELECT state='available' AND sha256 IS NOT NULL FROM assets WHERE tenant_id=$1 AND id=$2").bind(who.tenant_id).bind(asset).fetch_one(&self.assets.pool).await?;
            if ready {
                return self.quarantine(who, i.job_id, asset, token).await;
            }
        }
        let e: Endpoint = serde_json::from_value(
            s.routing_snapshot
                .as_ref()
                .and_then(|v| v.get("endpoint"))
                .cloned()
                .ok_or(Error::Configuration)?,
        )
        .map_err(|_| Error::Configuration)?;
        e.validate()?;
        e.validate_output_contract()?;
        let clear = self.source_cipher.decrypt(
            who.tenant_id,
            i.job_id,
            s.output_ciphertext
                .as_deref()
                .ok_or(Error::ResultUnavailable)?,
        )?;
        let output: Value = serde_json::from_slice(&clear).map_err(|_| Error::ResultUnavailable)?;
        let raw = output["video_url"]
            .as_str()
            .ok_or(Error::ResultUnavailable)?;
        let url = source_url(&e, raw)?;
        let source_hash = crate::store::hash(raw.as_bytes());
        if i.source_hash.as_ref().is_some_and(|h| h != &source_hash) {
            return Err(Error::Conflict);
        }
        let client =
            crate::egress::pinned_client(&url, e.allow_private, Duration::from_secs(300)).await?;
        let mut request = client.get(url).header("Accept-Encoding", "identity");
        if let Some(etag) = &i.source_etag {
            request = request.header("If-Match", etag);
        }
        // No Authorization, Cookie, Referer, redirects, proxies or provider credentials.
        let mut response = request.send().await.map_err(|_| Error::Storage)?;
        if response.status().is_server_error() || response.status().as_u16() == 429 {
            return Err(Error::Storage);
        }
        if response.status().as_u16() != 200
            || response.headers().contains_key("content-range")
            || response
                .headers()
                .get("content-encoding")
                .is_some_and(|h| h != "identity")
        {
            return Err(Error::Invalid(
                "output source did not provide an unencoded full object",
            ));
        }
        let length = response
            .content_length()
            .and_then(|n| i64::try_from(n).ok())
            .filter(|n| *n > 0 && *n <= i.maximum_bytes)
            .ok_or(Error::Invalid("output length missing or exceeds admission"))?;
        let etag = response
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .filter(|v| strong_etag(v))
            .ok_or(Error::Invalid("strong output ETag required"))?
            .to_owned();
        if i.source_etag.as_ref().is_some_and(|old| old != &etag) {
            return Err(Error::Conflict);
        }
        let asset = self
            .materialize(who, i.job_id, token, length, &etag, &source_hash)
            .await?;
        let mut buffer = Vec::with_capacity(CHUNK_BYTES);
        let mut received = 0i64;
        let mut ordinal = 0i32;
        while let Some(part) = response.chunk().await.map_err(|_| Error::Storage)? {
            received = received
                .checked_add(part.len() as i64)
                .ok_or(Error::Storage)?;
            if received > length {
                return Err(Error::Invalid(
                    "output is larger than its immutable manifest",
                ));
            }
            let mut rest = part.as_ref();
            while !rest.is_empty() {
                let n = (CHUNK_BYTES - buffer.len()).min(rest.len());
                buffer.extend_from_slice(&rest[..n]);
                rest = &rest[n..];
                if buffer.len() == CHUNK_BYTES {
                    let bytes = Bytes::from(std::mem::replace(
                        &mut buffer,
                        Vec::with_capacity(CHUNK_BYTES),
                    ));
                    self.assets
                        .put_generated_chunk(who, asset, ordinal, bytes, token)
                        .await?;
                    ordinal += 1;
                }
            }
        }
        if received != length {
            return Err(Error::Storage);
        }
        if !buffer.is_empty() {
            self.assets
                .put_generated_chunk(who, asset, ordinal, buffer.into(), token)
                .await?;
        }
        self.assets.complete_generated(who, asset, token).await?;
        self.quarantine(who, i.job_id, asset, token).await
    }
    async fn materialize(
        &self,
        who: &Identity,
        id: Uuid,
        token: Uuid,
        length: i64,
        etag: &str,
        hash: &[u8],
    ) -> Result<Uuid> {
        let mut tx = self.assets.pool.begin().await?;
        let (i, s) = locked(&mut tx, who, id).await?;
        owned(&i, token)?;
        active(&mut tx, who, &s).await?;
        source_live(&i, &s)?;
        if let Some(asset) = i.asset_id {
            let old: i64 = sqlx::query_scalar("SELECT byte_length FROM assets WHERE tenant_id=$1 AND id=$2 AND kind='output' AND state='uploading'").bind(who.tenant_id).bind(asset).fetch_optional(&mut *tx).await?.ok_or(Error::ResultUnavailable)?;
            if old != length
                || i.source_etag.as_deref() != Some(etag)
                || i.source_hash.as_deref() != Some(hash)
            {
                return Err(Error::Conflict);
            }
            tx.commit().await?;
            return Ok(asset);
        }
        if !(1..=i.reserved_bytes).contains(&length) {
            return Err(Error::AssetQuota);
        }
        let asset = Uuid::new_v4();
        let chunks = ((length + CHUNK_BYTES as i64 - 1) / CHUNK_BYTES as i64) as i32;
        let digest = governance::digest(
            &json!({"job":id,"source_hash":governance::hex(hash),"etag":etag,"length":length}),
        )?;
        let deadline = s.output_expires_at.ok_or(Error::ResultUnavailable)?;
        sqlx::query("INSERT INTO assets(tenant_id,id,key_id,idempotency_key,request_hash,sha256,byte_length,declared_media_type,chunk_count,upload_expires_at,expires_at,kind,source_job_id) VALUES($1,$2,$3,$4,$5,NULL,$6,'video/mp4',$7,LEAST($8,$9),$9,'output',$10)")
            .bind(who.tenant_id).bind(asset).bind(who.key_id).bind(format!("_br_output:{id}")).bind(digest).bind(length).bind(chunks).bind(deadline).bind(i.expires_at).bind(id).execute(&mut *tx).await?;
        reduce_bytes(&mut tx, who, i.reserved_bytes - length).await?;
        sqlx::query("UPDATE media_output_imports SET state='copying',reserved_bytes=0,asset_id=$3,source_etag=$4,source_hash=$5,error_code=NULL WHERE tenant_id=$1 AND job_id=$2")
            .bind(who.tenant_id).bind(id).bind(asset).bind(etag).bind(hash).execute(&mut *tx).await?;
        audit(
            &mut tx,
            who,
            id,
            "output.import_started",
            json!({"asset_id":asset,"bytes":length}),
        )
        .await?;
        tx.commit().await?;
        Ok(asset)
    }
    async fn quarantine(&self, who: &Identity, id: Uuid, asset: Uuid, token: Uuid) -> Result<()> {
        let mut tx = self.assets.pool.begin().await?;
        let (i, s) = locked(&mut tx, who, id).await?;
        owned(&i, token)?;
        active(&mut tx, who, &s).await?;
        if i.expires_at <= Utc::now() || i.asset_id != Some(asset) {
            return Err(Error::ResultUnavailable);
        }
        let hash:Vec<u8>=sqlx::query_scalar("SELECT sha256 FROM assets WHERE tenant_id=$1 AND key_id=$2 AND id=$3 AND kind='output' AND state='available' AND expires_at>clock_timestamp()").bind(who.tenant_id).bind(who.key_id).bind(asset).fetch_optional(&mut *tx).await?.ok_or(Error::ResultUnavailable)?;
        sqlx::query("UPDATE media_output_imports SET state='quarantined',error_code=NULL,lease_token=NULL,lease_until=NULL WHERE tenant_id=$1 AND job_id=$2").bind(who.tenant_id).bind(id).execute(&mut *tx).await?;
        sqlx::query("UPDATE media_jobs SET output_ciphertext=NULL WHERE tenant_id=$1 AND id=$2")
            .bind(who.tenant_id)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        audit(&mut tx,who,id,"output.quarantined",json!({"asset_id":asset,"sha256":governance::hex(&hash),"content_safety":"not_evaluated","delivery":"not_released"})).await?;
        tx.commit().await?;
        Ok(())
    }
    async fn retry(&self, who: &Identity, id: Uuid, token: Uuid) -> Result<()> {
        let mut tx = self.assets.pool.begin().await?;
        let (i, _) = locked(&mut tx, who, id).await?;
        owned(&i, token)?;
        sqlx::query("UPDATE media_output_imports SET error_code='output_copy_retry',next_attempt_at=clock_timestamp()+interval '15 seconds',lease_token=NULL,lease_until=NULL WHERE tenant_id=$1 AND job_id=$2").bind(who.tenant_id).bind(id).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }
    async fn stop(
        &self,
        who: &Identity,
        id: Uuid,
        token: Option<Uuid>,
        state: &str,
        code: &str,
    ) -> Result<()> {
        let mut tx = self.assets.pool.begin().await?;
        let (i, s) = locked(&mut tx, who, id).await?;
        if matches!(i.state.as_str(), "failed" | "cancelled" | "expired") {
            return Ok(());
        }
        if let Some(token) = token {
            owned(&i, token)?;
        } else if !invalid(&mut tx, who, &i, &s).await? {
            return Ok(());
        }
        if let Some(asset) = i.asset_id {
            assets::tombstone_output(&mut tx, who, asset).await?;
        }
        reduce_bytes(&mut tx, who, i.reserved_bytes).await?;
        sqlx::query("UPDATE media_output_imports SET state=$3,reserved_bytes=0,error_code=$4,lease_token=NULL,lease_until=NULL WHERE tenant_id=$1 AND job_id=$2").bind(who.tenant_id).bind(id).bind(state).bind(code).execute(&mut *tx).await?;
        sqlx::query("UPDATE media_jobs SET output_ciphertext=NULL WHERE tenant_id=$1 AND id=$2")
            .bind(who.tenant_id)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        audit(
            &mut tx,
            who,
            id,
            "output.closed",
            json!({"state":state,"reason":code,"money":"unchanged"}),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }
    pub async fn maintain(&self) -> Result<()> {
        // Read candidates without locks; recheck under the standard tenant/key/job order.
        let rows:Vec<(Uuid,Uuid,Uuid)>=sqlx::query_as("SELECT i.tenant_id,i.key_id,i.job_id FROM media_output_imports i JOIN media_jobs j ON j.tenant_id=i.tenant_id AND j.id=i.job_id JOIN requests r ON r.tenant_id=i.tenant_id AND r.id=i.job_id JOIN tenants t ON t.id=i.tenant_id JOIN virtual_keys k ON k.tenant_id=i.tenant_id AND k.id=i.key_id LEFT JOIN assets a ON a.tenant_id=i.tenant_id AND a.id=i.asset_id LEFT JOIN governance_turns g ON g.tenant_id=r.tenant_id AND g.id=r.governance_turn_id LEFT JOIN runtime_bindings b ON b.tenant_id=g.tenant_id AND b.key_id=g.key_id LEFT JOIN governance_users u ON u.tenant_id=g.tenant_id AND u.id=g.user_id WHERE i.state IN ('pending','copying','quarantined') AND (i.expires_at<=clock_timestamp() OR j.cancel_requested OR j.state IN ('cancelled','reconciled','needs_reconciliation') OR NOT t.active OR k.revoked_at IS NOT NULL OR k.expires_at<=clock_timestamp() OR (g.id IS NOT NULL AND (g.state<>'active' OR g.expires_at<=clock_timestamp() OR NOT b.active OR NOT u.active)) OR a.state='deleted' OR (i.state<>'quarantined' AND a.state IS DISTINCT FROM 'available' AND j.state='output_pending' AND (j.output_ciphertext IS NULL OR j.output_expires_at<=clock_timestamp()))) ORDER BY i.expires_at LIMIT 100").fetch_all(&self.assets.pool).await?;
        for (tenant_id, key_id, id) in rows {
            let who = Identity { tenant_id, key_id };
            self.stop(
                &who,
                id,
                None,
                "expired",
                "output_retention_or_authority_ended",
            )
            .await?;
        }
        self.assets.maintain().await
    }
}

async fn invalid(tx: &mut Tx<'_>, who: &Identity, i: &Import, s: &Source) -> Result<bool> {
    let asset_state: Option<String> = if let Some(asset) = i.asset_id {
        sqlx::query_scalar("SELECT state FROM assets WHERE tenant_id=$1 AND id=$2")
            .bind(who.tenant_id)
            .bind(asset)
            .fetch_optional(&mut **tx)
            .await?
    } else {
        None
    };
    let authority_invalid = match active(tx, who, s).await {
        Ok(()) => false,
        Err(Error::Database(e)) => return Err(Error::Database(e)),
        Err(_) => true,
    };
    Ok(i.expires_at <= Utc::now()
        || s.cancel_requested
        || matches!(
            s.state.as_str(),
            "cancelled" | "reconciled" | "needs_reconciliation"
        )
        || authority_invalid
        || asset_state.as_deref() == Some("deleted")
        || (i.state != "quarantined"
            && asset_state.as_deref() != Some("available")
            && s.state == "output_pending"
            && source_live(i, s).is_err()))
}

pub(crate) async fn reserve(tx: &mut Tx<'_>, who: &Identity, id: Uuid, maximum: i64) -> Result<()> {
    if !(1..=assets::MAX_ASSET_BYTES).contains(&maximum) {
        return Err(Error::Configuration);
    }
    let t=sqlx::query("UPDATE tenants SET asset_used_bytes=asset_used_bytes+$2 WHERE id=$1 AND asset_quota_bytes-asset_used_bytes>=$2").bind(who.tenant_id).bind(maximum).execute(&mut **tx).await?;
    let k=sqlx::query("UPDATE virtual_keys SET asset_used_bytes=asset_used_bytes+$3 WHERE tenant_id=$1 AND id=$2 AND asset_quota_bytes-asset_used_bytes>=$3").bind(who.tenant_id).bind(who.key_id).bind(maximum).execute(&mut **tx).await?;
    if t.rows_affected() != 1 || k.rows_affected() != 1 {
        return Err(Error::AssetQuota);
    }
    sqlx::query("INSERT INTO media_output_imports(tenant_id,job_id,key_id,maximum_bytes,reserved_bytes) VALUES($1,$2,$3,$4,$4)").bind(who.tenant_id).bind(id).bind(who.key_id).bind(maximum).execute(&mut **tx).await?;
    Ok(())
}
pub(crate) async fn release_unmaterialized(
    tx: &mut Tx<'_>,
    who: &Identity,
    id: Uuid,
) -> Result<()> {
    let bytes:Option<i64>=sqlx::query_scalar("SELECT reserved_bytes FROM media_output_imports WHERE tenant_id=$1 AND job_id=$2 AND state='pending' AND asset_id IS NULL FOR UPDATE").bind(who.tenant_id).bind(id).fetch_optional(&mut **tx).await?;
    if let Some(bytes) = bytes {
        reduce_bytes(tx, who, bytes).await?;
        sqlx::query("UPDATE media_output_imports SET reserved_bytes=0,state='cancelled',error_code='generation_closed_before_import' WHERE tenant_id=$1 AND job_id=$2").bind(who.tenant_id).bind(id).execute(&mut **tx).await?;
    }
    Ok(())
}
async fn reduce_bytes(tx: &mut Tx<'_>, who: &Identity, n: i64) -> Result<()> {
    if n == 0 {
        return Ok(());
    }
    sqlx::query("UPDATE tenants SET asset_used_bytes=asset_used_bytes-$2 WHERE id=$1")
        .bind(who.tenant_id)
        .bind(n)
        .execute(&mut **tx)
        .await?;
    sqlx::query(
        "UPDATE virtual_keys SET asset_used_bytes=asset_used_bytes-$3 WHERE tenant_id=$1 AND id=$2",
    )
    .bind(who.tenant_id)
    .bind(who.key_id)
    .bind(n)
    .execute(&mut **tx)
    .await?;
    Ok(())
}
async fn locked(tx: &mut Tx<'_>, who: &Identity, id: Uuid) -> Result<(Import, Source)> {
    accounting::lock_accounts(tx, who).await?;
    let s:Source=sqlx::query_as("SELECT j.state,j.cancel_requested,j.output_ciphertext,j.output_expires_at,r.routing_snapshot,r.governance_turn_id FROM media_jobs j JOIN requests r ON r.tenant_id=j.tenant_id AND r.id=j.id WHERE j.tenant_id=$1 AND j.key_id=$2 AND j.id=$3 FOR UPDATE OF j").bind(who.tenant_id).bind(who.key_id).bind(id).fetch_optional(&mut **tx).await?.ok_or(Error::NotFound)?;
    let i=sqlx::query_as("SELECT *,COALESCE(lease_until>clock_timestamp(),false) AS lease_valid,next_attempt_at<=clock_timestamp() AS due FROM media_output_imports WHERE tenant_id=$1 AND key_id=$2 AND job_id=$3 FOR UPDATE").bind(who.tenant_id).bind(who.key_id).bind(id).fetch_optional(&mut **tx).await?.ok_or(Error::NotFound)?;
    Ok((i, s))
}
async fn active(tx: &mut Tx<'_>, who: &Identity, s: &Source) -> Result<()> {
    let live:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM tenants t JOIN virtual_keys k ON k.tenant_id=t.id WHERE t.id=$1 AND k.id=$2 AND t.active AND k.revoked_at IS NULL AND (k.expires_at IS NULL OR k.expires_at>clock_timestamp()))").bind(who.tenant_id).bind(who.key_id).fetch_one(&mut **tx).await?;
    if !live || s.cancel_requested {
        return Err(Error::Unauthorized);
    }
    if let Some(turn) = s.governance_turn_id {
        let allowed:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM governance_turns g JOIN runtime_bindings b ON b.tenant_id=g.tenant_id AND b.key_id=g.key_id AND b.user_id=g.user_id JOIN governance_users u ON u.tenant_id=g.tenant_id AND u.id=g.user_id WHERE g.tenant_id=$1 AND g.key_id=$2 AND g.id=$3 AND g.state='active' AND g.expires_at>clock_timestamp() AND b.active AND u.active)").bind(who.tenant_id).bind(who.key_id).bind(turn).fetch_one(&mut **tx).await?;
        if !allowed {
            return Err(Error::TurnClosed);
        }
    }
    Ok(())
}
fn source_live(i: &Import, s: &Source) -> Result<()> {
    if i.expires_at <= Utc::now()
        || s.state != "output_pending"
        || s.output_ciphertext.is_none()
        || s.output_expires_at.is_none_or(|t| t <= Utc::now())
    {
        Err(Error::ResultUnavailable)
    } else {
        Ok(())
    }
}
fn owned(i: &Import, token: Uuid) -> Result<()> {
    if i.lease_token == Some(token)
        && i.lease_valid
        && matches!(i.state.as_str(), "pending" | "copying")
    {
        Ok(())
    } else {
        Err(Error::InProgress)
    }
}
pub(crate) async fn write_authority(
    tx: &mut Tx<'_>,
    who: &Identity,
    asset: Uuid,
    token: Uuid,
) -> Result<()> {
    let id: Uuid = sqlx::query_scalar(
        "SELECT job_id FROM media_output_imports WHERE tenant_id=$1 AND key_id=$2 AND asset_id=$3",
    )
    .bind(who.tenant_id)
    .bind(who.key_id)
    .bind(asset)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(Error::NotFound)?;
    let (i, s) = locked(tx, who, id).await?;
    owned(&i, token)?;
    active(tx, who, &s).await?;
    source_live(&i, &s)
}
pub fn source_url(e: &Endpoint, raw: &str) -> Result<url::Url> {
    e.validate_output_contract()?;
    let u = url::Url::parse(raw).map_err(|_| Error::Invalid("invalid output source URL"))?;
    if raw.len() > 8192
        || !u.username().is_empty()
        || u.password().is_some()
        || u.fragment().is_some()
        || !e.output_origins.iter().any(|origin| {
            url::Url::parse(origin).is_ok_and(|allowed| allowed.origin() == u.origin())
        })
    {
        return Err(Error::Invalid("output URL origin is not allowed"));
    }
    Ok(u)
}
fn strong_etag(s: &str) -> bool {
    (2..=512).contains(&s.len())
        && s.starts_with('"')
        && s.ends_with('"')
        && s[1..s.len() - 1]
            .bytes()
            .all(|b| b == 0x21 || (0x23..=0x7e).contains(&b))
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
