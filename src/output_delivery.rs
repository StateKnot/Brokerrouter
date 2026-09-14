//! Current-turn output release. Every read revalidates facts; approval is not execution.
use crate::{
    assets::Assets,
    error::{Error, Result},
    governance::{self, Gate, Guardrail, Operation, Turn},
    human::{self, Human},
    store::Identity,
};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;
type Tx<'a> = Transaction<'a, Postgres>;

#[derive(sqlx::FromRow)]
struct Evidence {
    scan_id: Uuid,
    job_id: Uuid,
    asset_id: Uuid,
    profile_id: Uuid,
    turn_id: Uuid,
    model_id: String,
    sha256: Vec<u8>,
    byte_length: i64,
    chunk_count: i32,
    report_hash: Vec<u8>,
    report: Value,
    expires_at: DateTime<Utc>,
}
impl Evidence {
    fn view(&self) -> Value {
        json!({"contract":"current-turn-output-v1","job_id":self.job_id,"scan_id":self.scan_id,
            "asset_id":self.asset_id,"profile_id":self.profile_id,"turn_id":self.turn_id,
            "sha256":governance::hex(&self.sha256),"byte_length":self.byte_length,
            "chunk_count":self.chunk_count,"chunk_bytes":crate::assets::CHUNK_BYTES,
            "report_sha256":governance::hex(&self.report_hash),"scan_report":self.report,
            "expires_at":self.expires_at,"scope":"current_turn_only"})
    }
}

// Call only under the tenant lock. No transaction is held over object storage I/O.
async fn current(tx: &mut Tx<'_>, who: &Identity, job: Uuid) -> Result<(Evidence, Turn)> {
    let e: Evidence = sqlx::query_as("SELECT s.id AS scan_id,s.job_id,s.asset_id,s.profile_id,r.governance_turn_id AS turn_id,r.model_id,s.sha256,s.byte_length,a.chunk_count,s.report_hash,s.report,LEAST(s.expires_at,a.expires_at,g.expires_at) AS expires_at FROM media_jobs j JOIN requests r ON r.tenant_id=j.tenant_id AND r.id=j.id JOIN media_output_imports i ON i.tenant_id=j.tenant_id AND i.job_id=j.id JOIN assets a ON a.tenant_id=i.tenant_id AND a.id=i.asset_id JOIN output_scan_profiles p ON p.tenant_id=r.tenant_id AND p.model_id=r.model_id AND p.active JOIN output_scans s ON s.tenant_id=j.tenant_id AND s.job_id=j.id AND s.profile_id=p.id JOIN governance_turns g ON g.tenant_id=r.tenant_id AND g.id=r.governance_turn_id JOIN models m ON m.tenant_id=r.tenant_id AND m.id=r.model_id JOIN key_models km ON km.tenant_id=m.tenant_id AND km.model_id=m.id AND km.key_id=j.key_id WHERE j.tenant_id=$1 AND j.id=$2 AND j.key_id=$3 AND j.state='output_pending' AND NOT j.cancel_requested AND i.state='quarantined' AND a.kind='output' AND a.state='available' AND a.expires_at>clock_timestamp() AND s.state='complete' AND s.expires_at>clock_timestamp() AND s.asset_id=a.id AND s.key_id=j.key_id AND s.sha256=a.sha256 AND s.byte_length=a.byte_length AND m.active")
        .bind(who.tenant_id).bind(job).bind(who.key_id).fetch_optional(&mut **tx).await?.ok_or(Error::ResultUnavailable)?;
    let turn = governance::turn_locked(tx, who, e.turn_id, true).await?;
    if e.report["technical"] != "clean"
        || e.report["media_type"] != "video/mp4"
        || e.report["sha256"] != governance::hex(&e.sha256)
        || e.report["byte_length"] != e.byte_length
        || governance::digest(&e.report)? != e.report_hash
    {
        return Err(Error::OutputUnsafe);
    }
    let rejected:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM media_output_deliveries d JOIN governance_operations o ON o.tenant_id=d.tenant_id AND o.id=d.operation_id WHERE d.tenant_id=$1 AND d.job_id=$2 AND o.state IN ('rejected','feedback'))")
        .bind(who.tenant_id).bind(job).fetch_one(&mut **tx).await?;
    if rejected {
        return Err(Error::ActionRejected);
    }
    Ok((e, turn))
}

pub(crate) fn validate_rule(rule: &Value) -> Result<()> {
    if rule.as_object().is_none_or(|o| o.len() != 2)
        || rule["kind"] != "asset_scan"
        || rule["profile_id"]
            .as_str()
            .and_then(|s| Uuid::parse_str(s).ok())
            .is_none()
    {
        return Err(Error::Invalid(
            "asset_scan requires an exact published profile_id",
        ));
    }
    Ok(())
}
fn validate_policy(rules: &[Guardrail], e: &Evidence) -> Result<()> {
    let mut scan_rule = false;
    for r in rules {
        match r.rule["kind"].as_str() {
            Some("asset_scan") => {
                validate_rule(&r.rule).map_err(|_| Error::Configuration)?;
                if r.rule["profile_id"] != e.profile_id.to_string() {
                    return Err(Error::PolicyChanged);
                }
                scan_rule = true;
            }
            Some("always") => governance::validate_rule(&r.rule)?,
            _ => return Err(Error::Configuration),
        }
    }
    if !scan_rule {
        return Err(Error::OutputPolicyMissing);
    }
    Ok(())
}
pub(crate) fn outcome(rule: &Value, payload: &Value) -> Result<(String, String)> {
    validate_rule(rule).map_err(|_| Error::Configuration)?;
    match payload["scan_report"]["content"].as_str() {
        Some("pass") => Ok(("passed".into(), "asset_content_pass".into())),
        Some("risk") => Ok(("risk".into(), "asset_content_risk".into())),
        Some("unknown") => Ok(("failed".into(), "asset_content_unknown".into())),
        _ => Err(Error::Configuration),
    }
}
async fn policies(tx: &mut Tx<'_>, who: &Identity, e: &Evidence) -> Result<Vec<Guardrail>> {
    let rules = governance::selected(tx, who, "media_output", Some(&e.model_id), None).await?;
    validate_policy(&rules, e)?;
    Ok(rules)
}
async fn operation(tx: &mut Tx<'_>, who: &Identity, e: &Evidence) -> Result<Operation> {
    sqlx::query_as("SELECT o.* FROM media_output_deliveries d JOIN governance_operations o ON o.tenant_id=d.tenant_id AND o.id=d.operation_id WHERE d.tenant_id=$1 AND d.scan_id=$2 AND o.key_id=$3")
        .bind(who.tenant_id).bind(e.scan_id).bind(who.key_id).fetch_optional(&mut **tx).await?.ok_or(Error::InProgress)
}
fn bind(op: &Operation, e: &Evidence) -> Result<()> {
    if op.stage != "media_output"
        || op.turn_id != e.turn_id
        || op.model_id.as_ref() != Some(&e.model_id)
        || op.payload_hash != governance::digest(&e.view())?
    {
        return Err(Error::PolicyChanged);
    }
    if !matches!(op.state.as_str(), "ready" | "waiting_approval") {
        return Err(Error::ActionRejected);
    }
    Ok(())
}
// Used by the separate human control before any grant is minted, and by the generic gate.
pub(crate) async fn validate_operation(
    tx: &mut Tx<'_>,
    who: &Identity,
    op: &Operation,
) -> Result<Value> {
    let job: Uuid = sqlx::query_scalar(
        "SELECT job_id FROM media_output_deliveries WHERE tenant_id=$1 AND operation_id=$2",
    )
    .bind(who.tenant_id)
    .bind(op.id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(Error::NotFound)?;
    let (e, _) = current(tx, who, job).await?;
    bind(op, &e)?;
    let linked = operation(tx, who, &e).await?;
    if linked.id != op.id {
        return Err(Error::PolicyChanged);
    }
    policies(tx, who, &e).await?;
    Ok(e.view())
}

#[derive(Clone)]
pub struct Delivery {
    pub assets: Assets,
}
impl Delivery {
    pub fn new(assets: Assets) -> Self {
        Self { assets }
    }
    pub async fn check(&self, who: &Identity, job: Uuid) -> Result<Value> {
        let mut tx = self.assets.pool.begin().await?;
        governance::tenant_lock(&mut tx, who.tenant_id).await?;
        let (e, turn) = current(&mut tx, who, job).await?;
        policies(&mut tx, who, &e).await?;
        let id = Uuid::new_v4();
        let payload = e.view();
        let hash = governance::digest(&payload)?;
        sqlx::query("INSERT INTO governance_operations(tenant_id,id,turn_id,key_id,operation_key,stage,model_id,payload_hash) VALUES($1,$2,$3,$4,$5,'media_output',$6,$7) ON CONFLICT(tenant_id,key_id,operation_key,stage) DO NOTHING")
            .bind(who.tenant_id).bind(id).bind(turn.id).bind(who.key_id).bind(format!("media-output:{}",e.scan_id)).bind(&e.model_id).bind(hash).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO media_output_deliveries(tenant_id,scan_id,job_id,operation_id) SELECT $1,$2,$3,id FROM governance_operations WHERE tenant_id=$1 AND key_id=$4 AND operation_key=$5 AND stage='media_output' ON CONFLICT(tenant_id,scan_id) DO NOTHING")
            .bind(who.tenant_id).bind(e.scan_id).bind(job).bind(who.key_id).bind(format!("media-output:{}",e.scan_id)).execute(&mut *tx).await?;
        let op = operation(&mut tx, who, &e).await?;
        bind(&op, &e)?;
        let gate = governance::gate(&mut tx, who, &turn, &op, &payload).await?;
        let result = match gate {
            Gate::Ready => json!({"state":"ready","operation_id":op.id,"output":payload}),
            Gate::Approval(a) => {
                json!({"state":"waiting_approval","operation_id":op.id,"approval":a})
            }
            Gate::Checking(_) => return Err(Error::Configuration),
        };
        tx.commit().await?;
        Ok(result)
    }
    pub async fn manifest(&self, who: &Identity, job: Uuid) -> Result<Value> {
        let mut tx = self.assets.pool.begin().await?;
        governance::tenant_lock(&mut tx, who.tenant_id).await?;
        let e = released(&mut tx, who, job).await?;
        tx.commit().await?;
        Ok(json!({"state":"ready","output":e.view()}))
    }
    pub async fn chunk(&self, who: &Identity, job: Uuid, n: i32) -> Result<Bytes> {
        let mut tx = self.assets.pool.begin().await?;
        governance::tenant_lock(&mut tx, who.tenant_id).await?;
        let e = released(&mut tx, who, job).await?;
        if n < 0 || n >= e.chunk_count {
            return Err(Error::NotFound);
        }
        tx.commit().await?;
        let bytes = self.assets.quarantined_chunk(who, e.asset_id, n).await?;
        let mut tx = self.assets.pool.begin().await?;
        governance::tenant_lock(&mut tx, who.tenant_id).await?;
        let now = released(&mut tx, who, job).await?;
        if now.scan_id != e.scan_id {
            return Err(Error::PolicyChanged);
        }
        audit(&mut tx, who, &e, n, None).await?;
        tx.commit().await?;
        Ok(bytes)
    }
    pub async fn review_manifest(&self, human: &Human, approval: Uuid) -> Result<Value> {
        let mut tx = self.assets.pool.begin().await?;
        let (_, e) = review(&mut tx, human, approval).await?;
        tx.commit().await?;
        Ok(
            json!({"state":"review_only","approval_id":approval,"output":e.view(),"notice":"Content risk is unreviewed. Use an isolated viewer. Reading does not approve or release this output."}),
        )
    }
    pub async fn review_chunk(&self, human: &Human, approval: Uuid, n: i32) -> Result<Bytes> {
        let mut tx = self.assets.pool.begin().await?;
        let (who, e) = review(&mut tx, human, approval).await?;
        if n < 0 || n >= e.chunk_count {
            return Err(Error::NotFound);
        }
        tx.commit().await?;
        let bytes = self.assets.quarantined_chunk(&who, e.asset_id, n).await?;
        let mut tx = self.assets.pool.begin().await?;
        let (_, now) = review(&mut tx, human, approval).await?;
        if now.scan_id != e.scan_id {
            return Err(Error::PolicyChanged);
        }
        audit(&mut tx, &who, &e, n, Some(human.id)).await?;
        tx.commit().await?;
        Ok(bytes)
    }
}
async fn released(tx: &mut Tx<'_>, who: &Identity, job: Uuid) -> Result<Evidence> {
    let (e, _) = current(tx, who, job).await?;
    let op = operation(tx, who, &e).await?;
    bind(&op, &e)?;
    for r in policies(tx, who, &e).await? {
        let grant:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM guardrail_grants WHERE tenant_id=$1 AND turn_id=$2 AND guardrail_id=$3 AND version=$4 AND expires_at>clock_timestamp())")
            .bind(who.tenant_id).bind(e.turn_id).bind(&r.id).bind(r.version).fetch_one(&mut **tx).await?;
        if !grant && (r.rule["kind"] == "always" || outcome(&r.rule, &e.view())?.0 != "passed") {
            return Err(Error::InProgress);
        }
    }
    Ok(e)
}
async fn review(tx: &mut Tx<'_>, human: &Human, id: Uuid) -> Result<(Identity, Evidence)> {
    human::authorize(tx, human).await?;
    let (key,job,op,policy):(Uuid,Uuid,Uuid,Value)=sqlx::query_as("SELECT o.key_id,d.job_id,o.id,a.policy_snapshot FROM approvals a JOIN governance_operations o ON o.tenant_id=a.tenant_id AND o.id=a.operation_id JOIN governance_turns t ON t.tenant_id=a.tenant_id AND t.id=a.turn_id JOIN media_output_deliveries d ON d.tenant_id=o.tenant_id AND d.operation_id=o.id WHERE a.tenant_id=$1 AND a.id=$2 AND t.user_id=$3 AND a.state='pending' AND a.expires_at>clock_timestamp() AND o.stage='media_output' AND o.state='waiting_approval'")
        .bind(human.tenant_id).bind(id).bind(human.id).fetch_optional(&mut **tx).await?.ok_or(Error::NotFound)?;
    let who = Identity {
        tenant_id: human.tenant_id,
        key_id: key,
    };
    let (e, _) = current(tx, &who, job).await?;
    let current = operation(tx, &who, &e).await?;
    bind(&current, &e)?;
    if current.id != op
        || serde_json::to_value(policies(tx, &who, &e).await?).map_err(|_| Error::Configuration)?
            != policy
    {
        return Err(Error::PolicyChanged);
    }
    Ok((who, e))
}
async fn audit(
    tx: &mut Tx<'_>,
    who: &Identity,
    e: &Evidence,
    n: i32,
    reviewer: Option<Uuid>,
) -> Result<()> {
    sqlx::query("INSERT INTO audit_events(tenant_id,action,object_id,details) VALUES($1,$2,$3,$4)")
        .bind(who.tenant_id).bind(if reviewer.is_some(){"output.review_chunk_served"}else{"output.chunk_served"})
        .bind(e.job_id.to_string()).bind(json!({"key_id":who.key_id,"turn_id":e.turn_id,"scan_id":e.scan_id,"ordinal":n,"reviewer_id":reviewer,"execution_completed":false})).execute(&mut **tx).await?;
    Ok(())
}
