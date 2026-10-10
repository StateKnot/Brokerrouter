//! Release exactly one sealed native artifact under current source and policy.
//! A tool receipt, a scan report and a review are distinct facts. Only an exact
//! human grant may waive this file's content rules; it never authorizes a run.
use crate::{
    assets::Assets,
    error::{Error, Result},
    governance::{self, Gate, Guardrail, Operation, Turn},
    human::{self, Human},
    output_delivery,
    store::Identity,
};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;
type Tx<'a> = Transaction<'a, Postgres>;

pub(crate) const OUTPUT_PREFIX: &str = "_br_runtime_artifact_release:";

#[derive(sqlx::FromRow)]
pub(crate) struct Evidence {
    scan_id: Uuid,
    pub(crate) asset_id: Uuid,
    profile_id: Uuid,
    turn_id: Uuid,
    model_id: String,
    tool_name: String,
    batch_id: Uuid,
    input_operation_id: Uuid,
    ordinal: i32,
    producer_revision: Vec<u8>,
    original_payload_hash: Vec<u8>,
    input_payload_hash: Vec<u8>,
    pub(crate) sha256: Vec<u8>,
    pub(crate) byte_length: i64,
    pub(crate) chunk_count: i32,
    report_hash: Vec<u8>,
    report: Value,
    pub(crate) expires_at: DateTime<Utc>,
}
impl Evidence {
    pub(crate) fn view(&self) -> Value {
        json!({"contract":"runtime-artifact-mp4-v1","source":"runtime_artifact",
            "scan_id":self.scan_id,"asset_id":self.asset_id,"profile_id":self.profile_id,
            "turn_id":self.turn_id,"model_id":self.model_id,"tool_name":self.tool_name,
            "batch_id":self.batch_id,"input_operation_id":self.input_operation_id,
            "ordinal":self.ordinal,"producer_revision":governance::hex(&self.producer_revision),
            "original_payload_sha256":governance::hex(&self.original_payload_hash),
            "input_payload_sha256":governance::hex(&self.input_payload_hash),
            "sha256":governance::hex(&self.sha256),"byte_length":self.byte_length,
            "chunk_count":self.chunk_count,"chunk_bytes":crate::assets::CHUNK_BYTES,
            "report_sha256":governance::hex(&self.report_hash),"scan_report":self.report,
            "expires_at":self.expires_at,"scope":"current_artifact_in_original_turn"})
    }
    fn operation_key(&self) -> String {
        format!("{OUTPUT_PREFIX}{}:{}", self.asset_id, self.scan_id)
    }
}

#[derive(sqlx::FromRow)]
struct ScanStatus {
    turn_id: Uuid,
    profile_id: Option<Uuid>,
    scan_id: Option<Uuid>,
    state: Option<String>,
    scan_expires_at: Option<DateTime<Utc>>,
    expires_at: DateTime<Utc>,
}
async fn pending_scan(tx: &mut Tx<'_>, who: &Identity, asset: Uuid) -> Result<Option<Value>> {
    let owned: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM runtime_artifacts WHERE tenant_id=$1 AND asset_id=$2 AND key_id=$3)")
        .bind(who.tenant_id).bind(asset).bind(who.key_id).fetch_one(&mut **tx).await?;
    if !owned {
        return Err(Error::NotFound);
    }
    let s: ScanStatus = sqlx::query_as(r#"
        SELECT b.turn_id,p.id AS profile_id,s.id AS scan_id,s.state,
               s.expires_at AS scan_expires_at,LEAST(a.expires_at,b.expires_at,t.expires_at) AS expires_at
        FROM runtime_artifacts r
        JOIN runtime_artifact_batches b ON b.tenant_id=r.tenant_id AND b.id=r.batch_id
        JOIN assets a ON a.tenant_id=r.tenant_id AND a.id=r.asset_id
        JOIN governance_operations origin ON origin.tenant_id=b.tenant_id AND origin.id=b.input_operation_id
        JOIN governance_turns t ON t.tenant_id=b.tenant_id AND t.id=b.turn_id
        JOIN models m ON m.tenant_id=b.tenant_id AND m.id=b.model_id
        JOIN key_models km ON km.tenant_id=m.tenant_id AND km.model_id=m.id AND km.key_id=b.key_id
        LEFT JOIN output_scan_profiles p ON p.tenant_id=b.tenant_id AND p.model_id=b.model_id AND p.active
        LEFT JOIN output_scans s ON s.tenant_id=r.tenant_id AND s.runtime_asset_id=r.asset_id AND s.profile_id=p.id
        WHERE r.tenant_id=$1 AND r.asset_id=$2 AND r.key_id=$3
          AND b.key_id=r.key_id AND b.state='quarantined' AND b.expires_at>clock_timestamp()
          AND a.kind='runtime_output' AND a.source_job_id IS NULL AND a.key_id=r.key_id
          AND a.source_operation_id=b.input_operation_id AND r.source_operation_id=b.input_operation_id
          AND a.state='available' AND a.expires_at>clock_timestamp()
          AND a.sha256=r.sha256 AND a.byte_length=r.byte_length AND m.active
          AND origin.stage='tool_input' AND origin.state='completed' AND origin.key_id=b.key_id
          AND origin.turn_id=b.turn_id AND origin.model_id IS NOT DISTINCT FROM b.model_id
          AND origin.tool_name=b.tool_name AND origin.payload_hash=b.input_payload_hash
          AND origin.claim_hash=b.claim_hash AND origin.receipt_hash=b.receipt_hash
    "#).bind(who.tenant_id).bind(asset).bind(who.key_id)
        .fetch_optional(&mut **tx).await?.ok_or(Error::ResultUnavailable)?;
    governance::turn_locked(tx, who, s.turn_id, true).await?;
    let rejected: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM runtime_artifact_deliveries d JOIN governance_operations o ON o.tenant_id=d.tenant_id AND o.id=d.operation_id WHERE d.tenant_id=$1 AND d.asset_id=$2 AND o.state IN ('rejected','feedback'))")
        .bind(who.tenant_id).bind(asset).fetch_one(&mut **tx).await?;
    if rejected {
        return Err(Error::ActionRejected);
    }
    let profile = s.profile_id.ok_or(Error::OutputPolicyMissing)?;
    let deadline = s
        .scan_expires_at
        .map_or(s.expires_at, |d| d.min(s.expires_at));
    require_live_deadline(tx, who, deadline, None).await?;
    match s.state.as_deref() {
        None | Some("scanning") => Ok(Some(
            json!({"state":"checking_scan","asset_id":asset,"profile_id":profile,"scan_id":s.scan_id,"expires_at":deadline}),
        )),
        Some("complete") => Ok(None),
        Some("closed") => Err(Error::OutputUnsafe),
        _ => Err(Error::Configuration),
    }
}

// The caller owns the tenant lock. Object storage is read outside transactions.
async fn current(tx: &mut Tx<'_>, who: &Identity, asset: Uuid) -> Result<(Evidence, Turn)> {
    let owned: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM runtime_artifacts WHERE tenant_id=$1 AND asset_id=$2 AND key_id=$3)",
    )
    .bind(who.tenant_id)
    .bind(asset)
    .bind(who.key_id)
    .fetch_one(&mut **tx)
    .await?;
    if !owned {
        return Err(Error::NotFound);
    }
    let e: Evidence = sqlx::query_as(r#"
        SELECT s.id AS scan_id,r.asset_id,s.profile_id,b.turn_id,b.model_id,b.tool_name,
               b.id AS batch_id,b.input_operation_id,r.ordinal,r.producer_revision,
               b.original_payload_hash,b.input_payload_hash,s.sha256,s.byte_length,
               a.chunk_count,s.report_hash,s.report,
               LEAST(s.expires_at,a.expires_at,b.expires_at,t.expires_at) AS expires_at
        FROM runtime_artifacts r
        JOIN runtime_artifact_batches b ON b.tenant_id=r.tenant_id AND b.id=r.batch_id
        JOIN assets a ON a.tenant_id=r.tenant_id AND a.id=r.asset_id
        JOIN governance_operations origin ON origin.tenant_id=b.tenant_id AND origin.id=b.input_operation_id
        JOIN governance_turns t ON t.tenant_id=b.tenant_id AND t.id=b.turn_id
        JOIN models m ON m.tenant_id=b.tenant_id AND m.id=b.model_id
        JOIN key_models km ON km.tenant_id=m.tenant_id AND km.model_id=m.id AND km.key_id=b.key_id
        JOIN output_scan_profiles p ON p.tenant_id=b.tenant_id AND p.model_id=b.model_id AND p.active
        JOIN output_scans s ON s.tenant_id=r.tenant_id AND s.runtime_asset_id=r.asset_id AND s.profile_id=p.id
        WHERE r.tenant_id=$1 AND r.asset_id=$2 AND r.key_id=$3
          AND b.key_id=r.key_id AND b.state='quarantined' AND b.expires_at>clock_timestamp()
          AND a.kind='runtime_output' AND a.source_job_id IS NULL AND a.key_id=r.key_id
          AND a.source_operation_id=b.input_operation_id AND r.source_operation_id=b.input_operation_id
          AND a.state='available' AND a.expires_at>clock_timestamp()
          AND a.sha256=r.sha256 AND a.byte_length=r.byte_length
          AND s.job_id IS NULL AND s.asset_id=r.asset_id AND s.key_id=r.key_id
          AND s.sha256=r.sha256 AND s.byte_length=r.byte_length
          AND s.state='complete' AND s.expires_at>clock_timestamp() AND m.active
          AND origin.stage='tool_input' AND origin.state='completed' AND origin.key_id=b.key_id
          AND origin.turn_id=b.turn_id AND origin.model_id IS NOT DISTINCT FROM b.model_id
          AND origin.tool_name=b.tool_name AND origin.payload_hash=b.input_payload_hash
          AND origin.claim_hash=b.claim_hash AND origin.receipt_hash=b.receipt_hash
    "#)
    .bind(who.tenant_id).bind(asset).bind(who.key_id)
    .fetch_optional(&mut **tx).await?.ok_or(Error::ResultUnavailable)?;
    let turn = governance::turn_locked(tx, who, e.turn_id, true).await?;
    if e.report["technical"] != "clean"
        || e.report["media_type"] != "video/mp4"
        || e.report["sha256"] != governance::hex(&e.sha256)
        || e.report["byte_length"] != e.byte_length
        || governance::digest(&e.report)? != e.report_hash
    {
        return Err(Error::OutputUnsafe);
    }
    let rejected: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM runtime_artifact_deliveries d JOIN governance_operations o ON o.tenant_id=d.tenant_id AND o.id=d.operation_id WHERE d.tenant_id=$1 AND d.asset_id=$2 AND o.state IN ('rejected','feedback'))")
        .bind(who.tenant_id).bind(asset).fetch_one(&mut **tx).await?;
    if rejected {
        return Err(Error::ActionRejected);
    }
    Ok((e, turn))
}
async fn policies(tx: &mut Tx<'_>, who: &Identity, e: &Evidence) -> Result<Vec<Guardrail>> {
    let rules =
        governance::selected(tx, who, "artifact", Some(&e.model_id), Some(&e.tool_name)).await?;
    let mut has_scan_rule = false;
    for rule in &rules {
        match rule.rule["kind"].as_str() {
            Some("asset_scan") => {
                output_delivery::validate_rule(&rule.rule).map_err(|_| Error::Configuration)?;
                if rule.rule["profile_id"] != e.profile_id.to_string() {
                    return Err(Error::PolicyChanged);
                }
                has_scan_rule = true;
            }
            Some("always") => governance::validate_rule(&rule.rule)?,
            _ => return Err(Error::Configuration),
        }
    }
    if !has_scan_rule {
        return Err(Error::OutputPolicyMissing);
    }
    Ok(rules)
}
async fn operation(tx: &mut Tx<'_>, who: &Identity, e: &Evidence) -> Result<Operation> {
    sqlx::query_as("SELECT o.* FROM runtime_artifact_deliveries d JOIN governance_operations o ON o.tenant_id=d.tenant_id AND o.id=d.operation_id WHERE d.tenant_id=$1 AND d.scan_id=$2 AND d.asset_id=$3 AND o.key_id=$4")
        .bind(who.tenant_id).bind(e.scan_id).bind(e.asset_id).bind(who.key_id)
        .fetch_optional(&mut **tx).await?.ok_or(Error::InProgress)
}
fn bind(op: &Operation, e: &Evidence) -> Result<()> {
    if op.stage != "artifact"
        || op.operation_key != e.operation_key()
        || op.turn_id != e.turn_id
        || op.model_id.as_ref() != Some(&e.model_id)
        || op.tool_name.as_deref() != Some(e.tool_name.as_str())
        || op.payload_hash != governance::digest(&e.view())?
    {
        return Err(Error::PolicyChanged);
    }
    if !matches!(op.state.as_str(), "ready" | "waiting_approval") {
        return Err(Error::ActionRejected);
    }
    Ok(())
}
async fn operation_evidence(tx: &mut Tx<'_>, who: &Identity, op: &Operation) -> Result<Evidence> {
    let asset: Uuid = sqlx::query_scalar(
        "SELECT asset_id FROM runtime_artifact_deliveries WHERE tenant_id=$1 AND operation_id=$2",
    )
    .bind(who.tenant_id)
    .bind(op.id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(Error::NotFound)?;
    let (e, _) = current(tx, who, asset).await?;
    bind(op, &e)?;
    if operation(tx, who, &e).await?.id != op.id {
        return Err(Error::PolicyChanged);
    }
    Ok(e)
}

/// Shared gate and human control validate real immutable bytes, never metadata supplied over HTTP.
pub(crate) async fn validate_operation(
    tx: &mut Tx<'_>,
    who: &Identity,
    op: &Operation,
) -> Result<Value> {
    let e = operation_evidence(tx, who, op).await?;
    policies(tx, who, &e).await?;
    require_live_output(tx, who, &e, None).await?;
    Ok(e.view())
}
/// Minted grants inherit this fixed source deadline. Replaying cannot extend it.
pub(crate) async fn grant_until(
    tx: &mut Tx<'_>,
    who: &Identity,
    op: &Operation,
) -> Result<DateTime<Utc>> {
    let e = operation_evidence(tx, who, op).await?;
    policies(tx, who, &e).await?;
    require_live_output(tx, who, &e, None).await?;
    Ok(e.expires_at)
}
/// Native output never consumes the legacy turn-wide grant namespace.
pub(crate) async fn lookup_grant(
    tx: &mut Tx<'_>,
    who: &Identity,
    op: &Operation,
    rule: &Guardrail,
    full_policy_hash: &[u8],
) -> Result<Option<(Uuid, DateTime<Utc>)>> {
    let e = operation_evidence(tx, who, op).await?;
    let current = policies(tx, who, &e).await?;
    let policy = serde_json::to_value(&current).map_err(|_| Error::Configuration)?;
    if governance::digest(&policy)? != full_policy_hash
        || !current
            .iter()
            .any(|r| r.id == rule.id && r.version == rule.version)
    {
        return Err(Error::PolicyChanged);
    }
    let grant = sqlx::query_as("SELECT g.approval_id,g.expires_at FROM runtime_artifact_grants g JOIN approvals a ON a.tenant_id=g.tenant_id AND a.id=g.approval_id JOIN runtime_artifact_deliveries d ON d.tenant_id=g.tenant_id AND d.operation_id=g.operation_id WHERE g.tenant_id=$1 AND g.operation_id=$2 AND g.guardrail_id=$3 AND g.version=$4 AND g.payload_hash=$5 AND g.policy_hash=$6 AND g.expires_at>clock_timestamp() AND g.expires_at<=$7 AND a.state='approved' AND a.operation_id=g.operation_id AND a.turn_id=$8 AND a.policy_snapshot=$9 AND d.scan_id=$10 AND d.asset_id=$11")
        .bind(who.tenant_id).bind(op.id).bind(&rule.id).bind(rule.version).bind(&op.payload_hash)
        .bind(full_policy_hash).bind(e.expires_at).bind(e.turn_id).bind(policy)
        .bind(e.scan_id).bind(e.asset_id).fetch_optional(&mut **tx).await?;
    require_live_output(tx, who, &e, None).await?;
    Ok(grant)
}

#[derive(Clone)]
pub struct Delivery {
    pub assets: Assets,
}
impl Delivery {
    pub fn new(assets: Assets) -> Self {
        Self { assets }
    }
    pub async fn release(&self, who: &Identity, asset: Uuid) -> Result<Value> {
        let mut tx = self.assets.pool.begin().await?;
        governance::tenant_lock(&mut tx, who.tenant_id).await?;
        if let Some(status) = pending_scan(&mut tx, who, asset).await? {
            tx.commit().await?;
            return Ok(status);
        }
        let (e, turn) = current(&mut tx, who, asset).await?;
        policies(&mut tx, who, &e).await?;
        let payload = e.view();
        let key = e.operation_key();
        sqlx::query("INSERT INTO governance_operations(tenant_id,id,turn_id,key_id,operation_key,stage,model_id,tool_name,payload_hash) VALUES($1,$2,$3,$4,$5,'artifact',$6,$7,$8) ON CONFLICT(tenant_id,key_id,operation_key,stage) DO NOTHING")
            .bind(who.tenant_id).bind(Uuid::new_v4()).bind(turn.id).bind(who.key_id).bind(&key)
            .bind(&e.model_id).bind(&e.tool_name).bind(governance::digest(&payload)?).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO runtime_artifact_deliveries(tenant_id,scan_id,asset_id,operation_id) SELECT $1,$2,$3,id FROM governance_operations WHERE tenant_id=$1 AND key_id=$4 AND operation_key=$5 AND stage='artifact' ON CONFLICT(tenant_id,scan_id) DO NOTHING")
            .bind(who.tenant_id).bind(e.scan_id).bind(asset).bind(who.key_id).bind(key).execute(&mut *tx).await?;
        let op = operation(&mut tx, who, &e).await?;
        bind(&op, &e)?;
        let (gate, deadline) =
            governance::gate_with_grant_deadline(&mut tx, who, &turn, &op, &payload).await?;
        let result = match gate {
            Gate::Ready => json!({"state":"ready","operation_id":op.id,"output":payload}),
            Gate::Approval(a) => {
                json!({"state":"waiting_approval","operation_id":op.id,"approval":a})
            }
            Gate::Checking(_) => return Err(Error::Configuration),
        };
        require_live_output(&mut tx, who, &e, None).await?;
        governance::require_live_grants(&mut tx, deadline).await?;
        tx.commit().await?;
        Ok(result)
    }
    pub async fn manifest(&self, who: &Identity, asset: Uuid) -> Result<Value> {
        let mut tx = self.assets.pool.begin().await?;
        governance::tenant_lock(&mut tx, who.tenant_id).await?;
        let (e, deadline) = released(&mut tx, who, asset).await?;
        require_live_output(&mut tx, who, &e, None).await?;
        governance::require_live_grants(&mut tx, deadline).await?;
        tx.commit().await?;
        Ok(json!({"state":"ready","output":e.view()}))
    }
    pub async fn chunk(&self, who: &Identity, asset: Uuid, ordinal: i32) -> Result<Bytes> {
        let mut tx = self.assets.pool.begin().await?;
        governance::tenant_lock(&mut tx, who.tenant_id).await?;
        let (e, _) = released(&mut tx, who, asset).await?;
        if ordinal < 0 || ordinal >= e.chunk_count {
            return Err(Error::NotFound);
        }
        tx.commit().await?;
        let bytes = self
            .assets
            .quarantined_runtime_chunk(who, asset, ordinal)
            .await?;
        let mut tx = self.assets.pool.begin().await?;
        governance::tenant_lock(&mut tx, who.tenant_id).await?;
        let (now, deadline) = released(&mut tx, who, asset).await?;
        if now.scan_id != e.scan_id
            || governance::digest(&now.view())? != governance::digest(&e.view())?
        {
            return Err(Error::PolicyChanged);
        }
        sqlx::query("INSERT INTO audit_events(tenant_id,action,object_id,details) VALUES($1,'runtime_artifact.chunk_served',$2,$3)")
            .bind(who.tenant_id).bind(asset.to_string())
            .bind(json!({"key_id":who.key_id,"turn_id":e.turn_id,"scan_id":e.scan_id,"input_operation_id":e.input_operation_id,"ordinal":ordinal,"execution_completed":false})).execute(&mut *tx).await?;
        require_live_output(&mut tx, who, &now, None).await?;
        governance::require_live_grants(&mut tx, deadline).await?;
        tx.commit().await?;
        Ok(bytes)
    }
}
async fn released(
    tx: &mut Tx<'_>,
    who: &Identity,
    asset: Uuid,
) -> Result<(Evidence, Option<DateTime<Utc>>)> {
    let (e, _) = current(tx, who, asset).await?;
    let op = operation(tx, who, &e).await?;
    bind(&op, &e)?;
    let rules = policies(tx, who, &e).await?;
    let hash =
        governance::digest(&serde_json::to_value(&rules).map_err(|_| Error::Configuration)?)?;
    let mut deadline: Option<DateTime<Utc>> = None;
    for rule in rules {
        if rule.rule["kind"] == "always"
            || output_delivery::outcome(&rule.rule, &e.view())?.0 != "passed"
        {
            let (_, expiry) = lookup_grant(tx, who, &op, &rule, &hash)
                .await?
                .ok_or(Error::InProgress)?;
            deadline = Some(deadline.map_or(expiry, |old| old.min(expiry)));
        }
    }
    require_live_output(tx, who, &e, None).await?;
    governance::require_live_grants(tx, deadline).await?;
    Ok((e, deadline))
}
pub(crate) async fn review(
    tx: &mut Tx<'_>,
    human: &Human,
    id: Uuid,
) -> Result<(Identity, Evidence)> {
    human::authorize(tx, human).await?;
    let (key, asset, operation_id, policy, deadline): (Uuid, Uuid, Uuid, Value, DateTime<Utc>) = sqlx::query_as("SELECT o.key_id,d.asset_id,o.id,a.policy_snapshot,a.expires_at FROM approvals a JOIN governance_operations o ON o.tenant_id=a.tenant_id AND o.id=a.operation_id JOIN governance_turns t ON t.tenant_id=o.tenant_id AND t.id=o.turn_id JOIN runtime_artifact_deliveries d ON d.tenant_id=o.tenant_id AND d.operation_id=o.id WHERE a.tenant_id=$1 AND a.id=$2 AND t.user_id=$3 AND a.state='pending' AND o.stage='artifact' AND o.state='waiting_approval'")
        .bind(human.tenant_id).bind(id).bind(human.id).fetch_optional(&mut **tx).await?.ok_or(Error::NotFound)?;
    let who = Identity {
        tenant_id: human.tenant_id,
        key_id: key,
    };
    // Resolve owned source first, then the approval deadline, so natural source
    // expiry remains 410 even when approval/review expire at the same instant.
    let (e, _) = current(tx, &who, asset).await?;
    let op = operation(tx, &who, &e).await?;
    bind(&op, &e)?;
    if op.id != operation_id
        || serde_json::to_value(policies(tx, &who, &e).await?).map_err(|_| Error::Configuration)?
            != policy
    {
        return Err(Error::PolicyChanged);
    }
    require_live_output(tx, &who, &e, Some(deadline)).await?;
    crate::approval_workbench::fresh(tx, human).await?;
    Ok((who, e))
}
async fn require_live_output(
    tx: &mut Tx<'_>,
    who: &Identity,
    e: &Evidence,
    additional_deadline: Option<DateTime<Utc>>,
) -> Result<()> {
    require_live_deadline(tx, who, e.expires_at, additional_deadline).await
}
async fn require_live_deadline(
    tx: &mut Tx<'_>,
    who: &Identity,
    deadline: DateTime<Utc>,
    additional_deadline: Option<DateTime<Utc>>,
) -> Result<()> {
    let live: bool = sqlx::query_scalar("SELECT clock_timestamp()<LEAST($3,$4) AND EXISTS(SELECT 1 FROM tenants t JOIN virtual_keys k ON k.tenant_id=t.id WHERE t.id=$1 AND k.id=$2 AND t.active AND k.revoked_at IS NULL AND (k.expires_at IS NULL OR k.expires_at>clock_timestamp()))")
        .bind(who.tenant_id).bind(who.key_id).bind(deadline).bind(additional_deadline).fetch_one(&mut **tx).await?;
    if !live {
        return Err(Error::ResultUnavailable);
    }
    Ok(())
}
