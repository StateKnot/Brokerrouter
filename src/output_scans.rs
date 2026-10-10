//! Pull-based evidence collection by a separate, trusted security workload.
//! A completed report is a fact, never a human approval or a download grant.
use crate::{
    assets::Assets,
    error::{Error, Result},
    governance,
    store::{self, Identity},
};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;
type Tx<'a> = Transaction<'a, Postgres>;
type Candidate = (
    Option<Uuid>,
    Option<Uuid>,
    Uuid,
    Uuid,
    Vec<u8>,
    i64,
    DateTime<Utc>,
);

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub tenant_id: Uuid,
    pub id: Uuid,
    pub model_id: String,
    pub version: i32,
    pub engine_revision: String,
}
#[derive(Clone, sqlx::FromRow)]
pub struct Scanner {
    pub(crate) tenant_id: Uuid,
    pub(crate) token_id: Uuid,
    pub(crate) profile_id: Uuid,
}
#[derive(sqlx::FromRow)]
struct Attempt {
    scan_id: Uuid,
    job_id: Option<Uuid>,
    runtime_asset_id: Option<Uuid>,
    input_operation_id: Option<Uuid>,
    batch_id: Option<Uuid>,
    artifact_ordinal: Option<i32>,
    producer_revision: Option<Vec<u8>>,
    asset_id: Uuid,
    key_id: Uuid,
    sha256: Vec<u8>,
    byte_length: i64,
    chunk_count: i32,
    state: String,
    current_attempt: Option<Uuid>,
    lease_until: DateTime<Utc>,
    deadline: DateTime<Utc>,
    created_at: DateTime<Utc>,
    report_hash: Option<Vec<u8>>,
    report: Option<Value>,
    version: i32,
    engine_revision: String,
    now: DateTime<Utc>,
}
#[derive(Deserialize, Serialize, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Technical {
    Clean,
    Unsafe,
    Unsupported,
    Error,
}
#[derive(Deserialize, Serialize, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Content {
    Pass,
    Risk,
    Unknown,
}
#[derive(Deserialize, Serialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Report {
    pub sha256: String,
    pub byte_length: i64,
    pub profile_version: i32,
    pub engine_revision: String,
    pub technical: Technical,
    pub content: Content,
    pub media_type: Option<String>,
    pub reason_codes: Vec<String>,
    pub evidence_sha256: String,
    pub scanned_at: DateTime<Utc>,
}
fn hash_text(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
impl Report {
    fn validate(&mut self, a: &Attempt) -> Result<()> {
        self.reason_codes.sort();
        if self.sha256 != governance::hex(&a.sha256)
            || self.byte_length != a.byte_length
            || self.profile_version != a.version
            || self.engine_revision != a.engine_revision
            || !hash_text(&self.evidence_sha256)
            || self.scanned_at < a.created_at
            || self.scanned_at > a.now + chrono::Duration::seconds(5)
            || self.reason_codes.len() > 32
            || self.reason_codes.windows(2).any(|p| p[0] == p[1])
            || self.reason_codes.iter().any(|s| {
                s.is_empty()
                    || s.len() > 64
                    || !s
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
            })
            || (self.technical == Technical::Clean
                && self.media_type.as_deref() != Some("video/mp4"))
            || (self.technical != Technical::Clean
                && (self.content != Content::Unknown || self.media_type.is_some()))
            || ((self.technical != Technical::Clean || self.content != Content::Pass)
                && self.reason_codes.is_empty())
        {
            return Err(Error::Invalid(
                "scan report does not match its immutable contract",
            ));
        }
        Ok(())
    }
}

pub async fn publish(pool: &PgPool, p: Profile) -> Result<()> {
    if p.version < 1 || !governance::valid_text(&p.model_id, 200) || !hash_text(&p.engine_revision)
    {
        return Err(Error::Invalid("invalid scan profile"));
    }
    let mut tx = pool.begin().await?;
    governance::tenant_lock(&mut tx, p.tenant_id).await?;
    let old: Option<i32> = sqlx::query_scalar(
        "SELECT max(version) FROM output_scan_profiles WHERE tenant_id=$1 AND model_id=$2",
    )
    .bind(p.tenant_id)
    .bind(&p.model_id)
    .fetch_one(&mut *tx)
    .await?;
    if old.is_some_and(|v| v >= p.version) {
        return Err(Error::Conflict);
    }
    sqlx::query("UPDATE output_scan_profiles SET active=false WHERE tenant_id=$1 AND model_id=$2 AND active").bind(p.tenant_id).bind(&p.model_id).execute(&mut *tx).await?;
    sqlx::query("INSERT INTO output_scan_profiles(tenant_id,id,model_id,version,engine_revision) VALUES($1,$2,$3,$4,$5)").bind(p.tenant_id).bind(p.id).bind(&p.model_id).bind(p.version).bind(&p.engine_revision).execute(&mut *tx).await?;
    audit(
        &mut tx,
        p.tenant_id,
        "output_scan.profile_published",
        p.id,
        json!({"model_id":p.model_id,"version":p.version}),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}
pub async fn disable(pool: &PgPool, tenant: Uuid, profile: Uuid) -> Result<()> {
    let mut tx = pool.begin().await?;
    governance::tenant_lock(&mut tx, tenant).await?;
    if sqlx::query("UPDATE output_scan_profiles SET active=false WHERE tenant_id=$1 AND id=$2")
        .bind(tenant)
        .bind(profile)
        .execute(&mut *tx)
        .await?
        .rows_affected()
        != 1
    {
        return Err(Error::NotFound);
    }
    audit(
        &mut tx,
        tenant,
        "output_scan.profile_disabled",
        profile,
        json!({}),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}
pub async fn create_token(
    pool: &PgPool,
    tenant: Uuid,
    profile: Uuid,
    name: &str,
    expires: DateTime<Utc>,
) -> Result<(Uuid, String)> {
    if !governance::valid_text(name, 200)
        || expires <= Utc::now()
        || expires > Utc::now() + chrono::Duration::days(90)
    {
        return Err(Error::Invalid(
            "scanner credential requires a name and expiry within 90 days",
        ));
    }
    let mut tx = pool.begin().await?;
    governance::tenant_lock(&mut tx, tenant).await?;
    let live: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM output_scan_profiles WHERE tenant_id=$1 AND id=$2 AND active)",
    )
    .bind(tenant)
    .bind(profile)
    .fetch_one(&mut *tx)
    .await?;
    if !live {
        return Err(Error::NotFound);
    }
    let mut secret = [0u8; 32];
    rand::rng().fill_bytes(&mut secret);
    let token = format!("sc_br_{}", governance::hex(&secret));
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO output_scanner_tokens(tenant_id,id,profile_id,name,token_hash,expires_at) VALUES($1,$2,$3,$4,$5,$6)").bind(tenant).bind(id).bind(profile).bind(name).bind(store::hash(token.as_bytes())).bind(expires).execute(&mut *tx).await?;
    audit(
        &mut tx,
        tenant,
        "output_scan.token_created",
        id,
        json!({"profile_id":profile,"expires_at":expires}),
    )
    .await?;
    tx.commit().await?;
    Ok((id, token))
}
pub async fn revoke_token(pool: &PgPool, tenant: Uuid, id: Uuid) -> Result<()> {
    let mut tx = pool.begin().await?;
    governance::tenant_lock(&mut tx, tenant).await?;
    if sqlx::query("UPDATE output_scanner_tokens SET revoked_at=COALESCE(revoked_at,clock_timestamp()) WHERE tenant_id=$1 AND id=$2").bind(tenant).bind(id).execute(&mut *tx).await?.rows_affected()!=1 { return Err(Error::NotFound); }
    audit(&mut tx, tenant, "output_scan.token_revoked", id, json!({})).await?;
    tx.commit().await?;
    Ok(())
}
pub async fn authenticate(pool: &PgPool, token: &str) -> Result<Scanner> {
    if !token.starts_with("sc_br_") || token.len() != 70 || !hash_text(&token[6..]) {
        return Err(Error::ScannerUnauthorized);
    }
    sqlx::query_as("SELECT t.tenant_id,t.id AS token_id,t.profile_id FROM output_scanner_tokens t JOIN output_scan_profiles p ON p.tenant_id=t.tenant_id AND p.id=t.profile_id JOIN tenants n ON n.id=t.tenant_id WHERE t.token_hash=$1 AND t.revoked_at IS NULL AND t.expires_at>clock_timestamp() AND p.active AND n.active").bind(store::hash(token.as_bytes())).fetch_optional(pool).await?.ok_or(Error::ScannerUnauthorized)
}
async fn actor(tx: &mut Tx<'_>, s: &Scanner) -> Result<()> {
    governance::tenant_lock(tx, s.tenant_id).await?;
    let live:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM output_scanner_tokens t JOIN output_scan_profiles p ON p.tenant_id=t.tenant_id AND p.id=t.profile_id JOIN models m ON m.tenant_id=p.tenant_id AND m.id=p.model_id WHERE t.tenant_id=$1 AND t.id=$2 AND t.profile_id=$3 AND t.revoked_at IS NULL AND t.expires_at>clock_timestamp() AND p.active AND m.active)").bind(s.tenant_id).bind(s.token_id).bind(s.profile_id).fetch_one(&mut **tx).await?;
    if !live {
        return Err(Error::ScannerUnauthorized);
    }
    Ok(())
}

#[derive(Clone)]
pub struct Scans {
    pub assets: Assets,
}
impl Scans {
    pub fn new(assets: Assets) -> Self {
        Self { assets }
    }
    pub async fn claim(&self, s: &Scanner, claim: Uuid) -> Result<Option<Value>> {
        let mut tx = self.assets.pool.begin().await?;
        actor(&mut tx, s).await?;
        let old:Option<Uuid>=sqlx::query_scalar("SELECT id FROM output_scan_attempts WHERE tenant_id=$1 AND token_id=$2 AND claim_id=$3").bind(s.tenant_id).bind(s.token_id).bind(claim).fetch_optional(&mut *tx).await?;
        if let Some(id) = old {
            let a = attempt(&mut tx, s, id, false).await?;
            tx.commit().await?;
            return Ok(Some(view(id, &a)));
        }
        let counts:(i64,i64,i64)=sqlx::query_as("SELECT count(*),count(*) FILTER(WHERE a.token_id=$2),count(*) FILTER(WHERE x.profile_id=$3) FROM output_scans x JOIN output_scan_attempts a ON a.tenant_id=x.tenant_id AND a.id=x.current_attempt JOIN output_scanner_tokens t ON t.tenant_id=a.tenant_id AND t.id=a.token_id WHERE x.tenant_id=$1 AND x.state='scanning' AND a.lease_until>clock_timestamp() AND t.revoked_at IS NULL AND t.expires_at>clock_timestamp()").bind(s.tenant_id).bind(s.token_id).bind(s.profile_id).fetch_one(&mut *tx).await?;
        if counts.0 >= 16 || counts.1 >= 2 || counts.2 >= 8 {
            return Err(Error::Busy);
        }
        // Selection is serialized by the tenant lock; no upstream execution or network I/O here.
        let candidate: Option<Candidate> = sqlx::query_as(CANDIDATES)
            .bind(s.tenant_id)
            .bind(s.profile_id)
            .fetch_optional(&mut *tx)
            .await?;
        let Some((job, runtime_asset, asset, key, sha, bytes, expires)) = candidate else {
            tx.commit().await?;
            return Ok(None);
        };
        let scan = Uuid::new_v4();
        sqlx::query("INSERT INTO output_scans(tenant_id,id,job_id,runtime_asset_id,asset_id,key_id,profile_id,sha256,byte_length,expires_at) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10) ON CONFLICT DO NOTHING")
            .bind(s.tenant_id).bind(scan).bind(job).bind(runtime_asset).bind(asset).bind(key)
            .bind(s.profile_id).bind(sha).bind(bytes).bind(expires).execute(&mut *tx).await?;
        let scan: Uuid = sqlx::query_scalar("SELECT id FROM output_scans WHERE tenant_id=$1 AND profile_id=$2 AND ((job_id=$3 AND runtime_asset_id IS NULL) OR (job_id IS NULL AND runtime_asset_id=$4))")
            .bind(s.tenant_id).bind(s.profile_id).bind(job).bind(runtime_asset)
            .fetch_one(&mut *tx).await?;
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO output_scan_attempts(tenant_id,id,scan_id,token_id,claim_id,created_at,lease_until,deadline) VALUES($1,$2,$3,$4,$5,statement_timestamp(),LEAST(statement_timestamp()+interval '5 minutes',$6),LEAST(statement_timestamp()+interval '30 minutes',$6))").bind(s.tenant_id).bind(id).bind(scan).bind(s.token_id).bind(claim).bind(expires).execute(&mut *tx).await?;
        sqlx::query("UPDATE output_scans SET current_attempt=$3,attempts=attempts+1 WHERE tenant_id=$1 AND id=$2").bind(s.tenant_id).bind(scan).bind(id).execute(&mut *tx).await?;
        attempt(&mut tx, s, id, true).await?;
        audit(
            &mut tx,
            s.tenant_id,
            "output_scan.claimed",
            scan,
            if let Some(job) = job {
                json!({"attempt_id":id,"token_id":s.token_id,"profile_id":s.profile_id,"job_id":job})
            } else {
                json!({"attempt_id":id,"token_id":s.token_id,"profile_id":s.profile_id,"runtime_asset_id":runtime_asset})
            },
        )
        .await?;
        actor(&mut tx, s).await?;
        let a = attempt(&mut tx, s, id, true).await?;
        tx.commit().await?;
        Ok(Some(view(id, &a)))
    }
    pub async fn renew(&self, s: &Scanner, id: Uuid) -> Result<Value> {
        let mut tx = self.assets.pool.begin().await?;
        actor(&mut tx, s).await?;
        attempt(&mut tx, s, id, true).await?;
        sqlx::query("UPDATE output_scan_attempts SET lease_until=LEAST(clock_timestamp()+interval '5 minutes',deadline) WHERE tenant_id=$1 AND id=$2").bind(s.tenant_id).bind(id).execute(&mut *tx).await?;
        let a = attempt(&mut tx, s, id, true).await?;
        tx.commit().await?;
        Ok(view(id, &a))
    }
    pub async fn chunk(&self, s: &Scanner, id: Uuid, ordinal: i32) -> Result<Bytes> {
        let mut tx = self.assets.pool.begin().await?;
        actor(&mut tx, s).await?;
        let a = attempt(&mut tx, s, id, true).await?;
        if ordinal < 0 || ordinal >= a.chunk_count {
            return Err(Error::NotFound);
        }
        tx.commit().await?;
        let who = Identity {
            tenant_id: s.tenant_id,
            key_id: a.key_id,
        };
        let bytes = if a.runtime_asset_id.is_some() {
            self.assets
                .quarantined_runtime_chunk(&who, a.asset_id, ordinal)
                .await?
        } else {
            self.assets
                .quarantined_chunk(&who, a.asset_id, ordinal)
                .await?
        };
        let mut tx = self.assets.pool.begin().await?;
        actor(&mut tx, s).await?;
        attempt(&mut tx, s, id, true).await?;
        audit(
            &mut tx,
            s.tenant_id,
            "output_scan.chunk_read",
            a.scan_id,
            json!({"attempt_id":id,"token_id":s.token_id,"ordinal":ordinal}),
        )
        .await?;
        actor(&mut tx, s).await?;
        attempt(&mut tx, s, id, true).await?;
        tx.commit().await?;
        Ok(bytes)
    }
    pub async fn report(&self, s: &Scanner, id: Uuid, mut report: Report) -> Result<Value> {
        let mut tx = self.assets.pool.begin().await?;
        actor(&mut tx, s).await?;
        let a = attempt(&mut tx, s, id, false).await?;
        report.validate(&a)?;
        let data = serde_json::to_value(report).map_err(|_| Error::Configuration)?;
        let hash = governance::digest(&data)?;
        if let Some(old) = &a.report_hash {
            if *old != hash {
                return Err(Error::Conflict);
            }
            tx.commit().await?;
            return Ok(view(id, &a));
        }
        sqlx::query("UPDATE output_scans SET state='complete',report=$3,report_hash=$4,finished_at=clock_timestamp() WHERE tenant_id=$1 AND id=$2").bind(s.tenant_id).bind(a.scan_id).bind(data).bind(hash).execute(&mut *tx).await?;
        audit(&mut tx,s.tenant_id,"output_scan.report_recorded",a.scan_id,json!({"attempt_id":id,"token_id":s.token_id,"delivery":"not_released","approval":"not_granted"})).await?;
        actor(&mut tx, s).await?;
        let a = attempt(&mut tx, s, id, false).await?;
        tx.commit().await?;
        Ok(view(id, &a))
    }
    pub async fn maintain(&self) -> Result<()> {
        // Use the same predicate for candidate selection and the locked recheck. Healthy work
        // must not fill the batch and starve later revoked or exhausted work.
        let invalid = format!(
            "s.state='scanning' AND (NOT EXISTS(SELECT 1 FROM output_scans x {SOURCE_JOINS} WHERE x.tenant_id=s.tenant_id AND x.id=s.id AND {SOURCE_LIVE}) OR (s.attempts>=3 AND EXISTS(SELECT 1 FROM output_scan_attempts z JOIN output_scanner_tokens zt ON zt.tenant_id=z.tenant_id AND zt.id=z.token_id WHERE z.tenant_id=s.tenant_id AND z.id=s.current_attempt AND (z.lease_until<=clock_timestamp() OR zt.revoked_at IS NOT NULL OR zt.expires_at<=clock_timestamp()))))"
        );
        let candidates:Vec<(Uuid,Uuid)>=sqlx::query_as(&format!("SELECT s.tenant_id,s.id FROM output_scans s WHERE {invalid} ORDER BY s.expires_at LIMIT 100")).fetch_all(&self.assets.pool).await?;
        for (tenant, id) in candidates {
            let mut tx = self.assets.pool.begin().await?;
            // Cleanup must also work for disabled tenants; the lock alone grants no data access.
            sqlx::query("SELECT id FROM tenants WHERE id=$1 FOR UPDATE")
                .bind(tenant)
                .execute(&mut *tx)
                .await?;
            let n=sqlx::query(&format!("UPDATE output_scans s SET state='closed',finished_at=clock_timestamp() WHERE s.tenant_id=$1 AND s.id=$2 AND {invalid}")).bind(tenant).bind(id).execute(&mut *tx).await?;
            if n.rows_affected() == 1 {
                audit(
                    &mut tx,
                    tenant,
                    "output_scan.closed",
                    id,
                    json!({"delivery":"not_released","money":"unchanged"}),
                )
                .await?;
            }
            tx.commit().await?;
        }
        Ok(())
    }
}
// Shared source revalidation for reads/reporting and cleanup. Native facts
// remain attached to their actual completed claim, never a synthetic media job.
const SOURCE_JOINS: &str = r#"
JOIN output_scan_profiles p ON p.tenant_id=x.tenant_id AND p.id=x.profile_id
JOIN assets a ON a.tenant_id=x.tenant_id AND a.id=x.asset_id
JOIN tenants n ON n.id=x.tenant_id
JOIN virtual_keys k ON k.tenant_id=x.tenant_id AND k.id=x.key_id
LEFT JOIN media_output_imports i ON i.tenant_id=x.tenant_id AND i.job_id=x.job_id
LEFT JOIN media_jobs j ON j.tenant_id=x.tenant_id AND j.id=x.job_id
LEFT JOIN requests r ON r.tenant_id=j.tenant_id AND r.id=j.id
LEFT JOIN runtime_artifacts ra ON ra.tenant_id=x.tenant_id AND ra.asset_id=x.runtime_asset_id
LEFT JOIN runtime_artifact_batches rb ON rb.tenant_id=ra.tenant_id AND rb.id=ra.batch_id
LEFT JOIN governance_operations origin ON origin.tenant_id=rb.tenant_id AND origin.id=rb.input_operation_id
JOIN models m ON m.tenant_id=x.tenant_id AND m.id=COALESCE(r.model_id,rb.model_id)
JOIN key_models km ON km.tenant_id=m.tenant_id AND km.model_id=m.id AND km.key_id=x.key_id
LEFT JOIN governance_turns g ON g.tenant_id=x.tenant_id AND g.id=COALESCE(r.governance_turn_id,rb.turn_id)
LEFT JOIN runtime_bindings b ON b.tenant_id=g.tenant_id AND b.key_id=g.key_id AND b.user_id=g.user_id
LEFT JOIN governance_users u ON u.tenant_id=g.tenant_id AND u.id=g.user_id
"#;
const SOURCE_LIVE: &str = r#"
n.active AND p.active AND m.active AND m.id=p.model_id
AND a.key_id=x.key_id AND a.state='available' AND a.expires_at>clock_timestamp()
AND a.sha256=x.sha256 AND a.byte_length=x.byte_length AND x.expires_at>clock_timestamp()
AND k.revoked_at IS NULL AND (k.expires_at IS NULL OR k.expires_at>clock_timestamp())
AND (COALESCE(r.governance_turn_id,rb.turn_id) IS NULL
    OR (g.key_id=x.key_id AND g.state='active' AND g.expires_at>clock_timestamp() AND b.active AND u.active))
AND ((x.job_id IS NOT NULL AND x.runtime_asset_id IS NULL AND a.kind='output'
        AND a.source_job_id=x.job_id AND i.asset_id=a.id AND i.state='quarantined'
        AND j.key_id=x.key_id AND j.state='output_pending' AND NOT j.cancel_requested)
    OR (x.job_id IS NULL AND x.runtime_asset_id=a.id AND a.kind='runtime_output'
        AND ra.key_id=x.key_id AND ra.sha256=a.sha256 AND ra.byte_length=a.byte_length
        AND rb.key_id=x.key_id AND rb.state='quarantined' AND rb.expires_at>clock_timestamp()
        AND a.source_operation_id=rb.input_operation_id AND origin.stage='tool_input'
        AND origin.state='completed' AND origin.key_id=rb.key_id AND origin.turn_id=rb.turn_id
        AND origin.claim_hash=rb.claim_hash AND origin.receipt_hash=rb.receipt_hash
        AND origin.payload_hash=rb.input_payload_hash AND origin.model_id IS NOT DISTINCT FROM rb.model_id
        AND origin.tool_name=rb.tool_name))
"#;

const CANDIDATES: &str = r#"
SELECT candidate.job_id,candidate.runtime_asset_id,candidate.asset_id,
       candidate.key_id,candidate.sha256,candidate.byte_length,candidate.expires_at
FROM (
    SELECT i.job_id,NULL::uuid AS runtime_asset_id,a.id AS asset_id,a.key_id,a.sha256,a.byte_length,
           LEAST(a.expires_at,COALESCE(g.expires_at,a.expires_at)) AS expires_at
    FROM media_output_imports i
    JOIN media_jobs j ON j.tenant_id=i.tenant_id AND j.id=i.job_id
    JOIN assets a ON a.tenant_id=i.tenant_id AND a.id=i.asset_id
    JOIN requests r ON r.tenant_id=j.tenant_id AND r.id=j.id
    JOIN output_scan_profiles p ON p.tenant_id=r.tenant_id AND p.model_id=r.model_id
    JOIN virtual_keys k ON k.tenant_id=a.tenant_id AND k.id=a.key_id
    JOIN key_models km ON km.tenant_id=k.tenant_id AND km.key_id=k.id AND km.model_id=r.model_id
    LEFT JOIN governance_turns g ON g.tenant_id=r.tenant_id AND g.id=r.governance_turn_id
    LEFT JOIN runtime_bindings b ON b.tenant_id=g.tenant_id AND b.key_id=g.key_id
    LEFT JOIN governance_users u ON u.tenant_id=g.tenant_id AND u.id=g.user_id
    LEFT JOIN output_scans x ON x.tenant_id=j.tenant_id AND x.job_id=j.id AND x.profile_id=p.id
    LEFT JOIN output_scan_attempts z ON z.tenant_id=x.tenant_id AND z.id=x.current_attempt
    LEFT JOIN output_scanner_tokens zt ON zt.tenant_id=z.tenant_id AND zt.id=z.token_id
    WHERE i.tenant_id=$1 AND p.id=$2 AND i.state='quarantined' AND j.state='output_pending'
      AND NOT j.cancel_requested AND a.kind='output' AND a.state='available'
      AND a.expires_at>clock_timestamp() AND k.revoked_at IS NULL
      AND (k.expires_at IS NULL OR k.expires_at>clock_timestamp())
      AND (r.governance_turn_id IS NULL OR (g.state='active' AND g.expires_at>clock_timestamp() AND b.active AND u.active))
      AND (x.id IS NULL OR (x.state='scanning' AND x.attempts<3 AND x.expires_at>clock_timestamp()
        AND (z.lease_until<=clock_timestamp() OR zt.revoked_at IS NOT NULL OR zt.expires_at<=clock_timestamp())))
    UNION ALL
    SELECT NULL::uuid,ra.asset_id,a.id,a.key_id,a.sha256,a.byte_length,
           LEAST(a.expires_at,rb.expires_at,g.expires_at,clock_timestamp()+interval '24 hours')
    FROM runtime_artifacts ra
    JOIN runtime_artifact_batches rb ON rb.tenant_id=ra.tenant_id AND rb.id=ra.batch_id
    JOIN governance_operations origin ON origin.tenant_id=rb.tenant_id AND origin.id=rb.input_operation_id
    JOIN assets a ON a.tenant_id=ra.tenant_id AND a.id=ra.asset_id AND a.key_id=ra.key_id
    JOIN output_scan_profiles p ON p.tenant_id=rb.tenant_id AND p.model_id=rb.model_id
    JOIN models m ON m.tenant_id=p.tenant_id AND m.id=p.model_id
    JOIN virtual_keys k ON k.tenant_id=a.tenant_id AND k.id=a.key_id
    JOIN key_models km ON km.tenant_id=k.tenant_id AND km.key_id=k.id AND km.model_id=m.id
    JOIN governance_turns g ON g.tenant_id=rb.tenant_id AND g.id=rb.turn_id AND g.key_id=ra.key_id
    JOIN runtime_bindings b ON b.tenant_id=g.tenant_id AND b.key_id=g.key_id AND b.user_id=g.user_id
    JOIN governance_users u ON u.tenant_id=g.tenant_id AND u.id=g.user_id
    LEFT JOIN output_scans x ON x.tenant_id=ra.tenant_id AND x.runtime_asset_id=ra.asset_id AND x.profile_id=p.id
    LEFT JOIN output_scan_attempts z ON z.tenant_id=x.tenant_id AND z.id=x.current_attempt
    LEFT JOIN output_scanner_tokens zt ON zt.tenant_id=z.tenant_id AND zt.id=z.token_id
    WHERE ra.tenant_id=$1 AND p.id=$2 AND p.active AND m.active AND rb.state='quarantined'
      AND a.kind='runtime_output' AND a.state='available' AND a.sha256=ra.sha256 AND a.byte_length=ra.byte_length
      AND a.source_operation_id=rb.input_operation_id AND a.expires_at>clock_timestamp()
      AND rb.expires_at>clock_timestamp() AND origin.stage='tool_input' AND origin.state='completed'
      AND origin.key_id=rb.key_id AND origin.turn_id=rb.turn_id AND origin.claim_hash=rb.claim_hash
      AND origin.receipt_hash=rb.receipt_hash AND origin.payload_hash=rb.input_payload_hash
      AND origin.model_id IS NOT DISTINCT FROM rb.model_id AND origin.tool_name=rb.tool_name
      AND k.revoked_at IS NULL AND (k.expires_at IS NULL OR k.expires_at>clock_timestamp())
      AND g.state='active' AND g.expires_at>clock_timestamp() AND b.active AND u.active
      AND (x.id IS NULL OR (x.state='scanning' AND x.attempts<3 AND x.expires_at>clock_timestamp()
        AND (z.lease_until<=clock_timestamp() OR zt.revoked_at IS NOT NULL OR zt.expires_at<=clock_timestamp())))
) candidate ORDER BY candidate.expires_at,candidate.asset_id LIMIT 1
"#;

async fn attempt(tx: &mut Tx<'_>, s: &Scanner, id: Uuid, running: bool) -> Result<Attempt> {
    let query = format!(
        "SELECT x.id AS scan_id,x.job_id,x.runtime_asset_id,rb.input_operation_id,ra.batch_id,ra.ordinal AS artifact_ordinal,ra.producer_revision,x.asset_id,x.key_id,x.sha256,x.byte_length,a.chunk_count,x.state,x.current_attempt,z.lease_until,z.deadline,z.created_at,x.report_hash,x.report,p.version,p.engine_revision,clock_timestamp() AS now FROM output_scan_attempts z JOIN output_scans x ON x.tenant_id=z.tenant_id AND x.id=z.scan_id {SOURCE_JOINS} WHERE z.tenant_id=$1 AND z.id=$2 AND z.token_id=$3 AND x.profile_id=$4 AND {SOURCE_LIVE} FOR UPDATE OF x"
    );
    let a: Attempt = sqlx::query_as(&query)
        .bind(s.tenant_id)
        .bind(id)
        .bind(s.token_id)
        .bind(s.profile_id)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(Error::NotFound)?;
    if a.current_attempt != Some(id)
        || a.state == "closed"
        || ((running || a.state != "complete")
            && (a.state != "scanning" || a.lease_until <= a.now || a.deadline <= a.now))
    {
        return Err(Error::ScanLeaseEnded);
    }
    Ok(a)
}
fn view(id: Uuid, a: &Attempt) -> Value {
    let mut v = json!({"attempt_id":id,"scan_id":a.scan_id,"asset_id":a.asset_id,"sha256":governance::hex(&a.sha256),"byte_length":a.byte_length,"chunk_count":a.chunk_count,"profile_version":a.version,"engine_revision":a.engine_revision,"state":a.state,"lease_until":a.lease_until,"deadline":a.deadline,"report":a.report,"delivery":"not_released","approval":"not_granted"});
    if let Some(job) = a.job_id {
        // Preserve the exact old assignment shape, including absence of source.
        v["job_id"] = json!(job);
    } else {
        v["source"] = json!("runtime_artifact");
        v["runtime_asset_id"] = json!(a.runtime_asset_id);
        v["input_operation_id"] = json!(a.input_operation_id);
        v["batch_id"] = json!(a.batch_id);
        v["ordinal"] = json!(a.artifact_ordinal);
        v["producer_revision"] = json!(a.producer_revision.as_ref().map(|r| governance::hex(r)));
    }
    v
}
async fn audit(
    tx: &mut Tx<'_>,
    tenant: Uuid,
    action: &str,
    id: Uuid,
    details: Value,
) -> Result<()> {
    sqlx::query("INSERT INTO audit_events(tenant_id,action,object_id,details) VALUES($1,$2,$3,$4)")
        .bind(tenant)
        .bind(action)
        .bind(id.to_string())
        .bind(details)
        .execute(&mut **tx)
        .await?;
    Ok(())
}
