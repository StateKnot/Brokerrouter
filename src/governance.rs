//! Durable governance facts. The Agent runtime, not this module, executes tools.
use crate::{
    cipher::ResultCipher,
    error::{Error, Result},
    store::{self, Identity},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

pub(crate) type Tx<'a> = Transaction<'a, Postgres>;

#[derive(Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Turn {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub key_id: Uuid,
    pub user_id: Uuid,
    pub session_id: String,
    pub message_id: String,
    pub run_id: String,
    pub generation: i32,
    pub state: String,
    pub expires_at: DateTime<Utc>,
}
#[derive(Clone, Serialize, Deserialize, sqlx::FromRow, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Guardrail {
    pub id: String,
    pub version: i32,
    pub name: String,
    pub stage: String,
    pub rule: Value,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Check {
    pub turn_id: Uuid,
    pub operation_key: String,
    pub stage: String,
    pub model_id: Option<String>,
    pub tool_name: Option<String>,
    pub payload: Value,
}
#[derive(sqlx::FromRow)]
pub(crate) struct Operation {
    pub id: Uuid,
    pub turn_id: Uuid,
    pub key_id: Uuid,
    pub stage: String,
    pub model_id: Option<String>,
    pub tool_name: Option<String>,
    pub payload_hash: Vec<u8>,
    pub payload_ciphertext: Option<Vec<u8>>,
    pub state: String,
    pub claim_hash: Option<Vec<u8>>,
    pub execution_deadline: Option<DateTime<Utc>>,
    pub receipt_hash: Option<Vec<u8>>,
    pub receipt_ciphertext: Option<Vec<u8>>,
}
#[derive(Clone, sqlx::FromRow)]
pub(crate) struct Approval {
    pub id: Uuid,
    pub operation_id: Uuid,
    pub turn_id: Uuid,
    pub policy_snapshot: Value,
    pub summary: Value,
    pub guardrails: Value,
    pub state: String,
    pub decision_hash: Option<Vec<u8>>,
    pub feedback_ciphertext: Option<Vec<u8>>,
    pub expires_at: DateTime<Utc>,
}

pub(crate) fn valid_text(v: &str, max: usize) -> bool {
    !v.trim().is_empty() && v.len() <= max && !v.chars().any(char::is_control)
}
pub fn digest(value: &Value) -> Result<Vec<u8>> {
    Ok(store::hash(
        &serde_json::to_vec(value).map_err(|_| Error::Invalid("invalid JSON"))?,
    ))
}
pub(crate) async fn tenant_lock(tx: &mut Tx<'_>, tenant: Uuid) -> Result<()> {
    let active: Option<bool> =
        sqlx::query_scalar("SELECT active FROM tenants WHERE id=$1 FOR UPDATE")
            .bind(tenant)
            .fetch_optional(&mut **tx)
            .await?;
    if active != Some(true) {
        return Err(Error::Unauthorized);
    }
    Ok(())
}
pub(crate) async fn turn_locked(
    tx: &mut Tx<'_>,
    who: &Identity,
    id: Uuid,
    require_active: bool,
) -> Result<Turn> {
    let row: Turn = sqlx::query_as("SELECT t.* FROM governance_turns t JOIN runtime_bindings b ON b.tenant_id=t.tenant_id AND b.key_id=t.key_id AND b.user_id=t.user_id JOIN governance_users u ON u.tenant_id=t.tenant_id AND u.id=t.user_id JOIN virtual_keys k ON k.tenant_id=t.tenant_id AND k.id=t.key_id WHERE t.tenant_id=$1 AND t.key_id=$2 AND t.id=$3 AND b.active AND u.active AND k.revoked_at IS NULL AND (k.expires_at IS NULL OR k.expires_at>clock_timestamp()) FOR UPDATE OF t")
        .bind(who.tenant_id).bind(who.key_id).bind(id).fetch_optional(&mut **tx).await?.ok_or(Error::NotFound)?;
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut **tx)
        .await?;
    if require_active && (row.state != "active" || row.expires_at <= now) {
        return Err(Error::TurnClosed);
    }
    Ok(row)
}
pub async fn context(pool: &PgPool, who: &Identity, turn: Option<Uuid>, model: &str) -> Result<()> {
    let mut tx = pool.begin().await?;
    tenant_lock(&mut tx, who.tenant_id).await?;
    if let Some(id) = turn {
        turn_locked(&mut tx, who, id, true).await?;
    } else {
        let required: bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM runtime_bindings WHERE tenant_id=$1 AND key_id=$2) OR EXISTS(SELECT 1 FROM guardrail_policies p JOIN guardrails g ON g.tenant_id=p.tenant_id AND g.id=p.guardrail_id WHERE p.tenant_id=$1 AND p.active AND g.active AND (p.key_id IS NULL OR p.key_id=$2) AND (p.model_id IS NULL OR p.model_id=$3) AND p.tool_name IS NULL)").bind(who.tenant_id).bind(who.key_id).bind(model).fetch_one(&mut *tx).await?;
        if required {
            return Err(Error::GovernanceContext);
        }
    }
    tx.commit().await?;
    Ok(())
}
pub(crate) async fn selected(
    tx: &mut Tx<'_>,
    who: &Identity,
    stage: &str,
    model: Option<&str>,
    tool: Option<&str>,
) -> Result<Vec<Guardrail>> {
    let rules: Vec<Guardrail>=sqlx::query_as("SELECT DISTINCT v.id,v.version,v.name,v.stage,v.rule FROM guardrail_policies p JOIN guardrails g ON g.tenant_id=p.tenant_id AND g.id=p.guardrail_id JOIN guardrail_versions v ON v.tenant_id=g.tenant_id AND v.id=g.id AND v.version=g.version JOIN runtime_bindings b ON b.tenant_id=p.tenant_id AND b.key_id=$2 WHERE p.tenant_id=$1 AND p.active AND g.active AND v.stage=$3 AND (p.key_id IS NULL OR p.key_id=$2) AND (p.model_id IS NULL OR p.model_id=$4) AND (p.tool_name IS NULL OR p.tool_name=$5) AND (p.runtime_id IS NULL OR p.runtime_id=b.runtime_id) ORDER BY v.id LIMIT 257")
        .bind(who.tenant_id).bind(who.key_id).bind(stage).bind(model).bind(tool).fetch_all(&mut **tx).await?;
    if rules.len() > 256 {
        return Err(Error::Configuration);
    }
    Ok(rules)
}
pub fn validate_rule(rule: &Value) -> Result<()> {
    let Some(object) = rule.as_object() else {
        return Err(Error::Invalid("rule must be an object"));
    };
    match rule["kind"].as_str() {
        Some("always") if object.len() == 1 => Ok(()),
        Some("contains_any") if object.len() == 3 && rule["case_sensitive"].is_boolean() => {
            let terms = rule["terms"]
                .as_array()
                .ok_or(Error::Invalid("rule terms required"))?;
            if terms.is_empty()
                || terms.len() > 64
                || terms
                    .iter()
                    .any(|v| v.as_str().is_none_or(|s| !valid_text(s, 256)))
            {
                return Err(Error::Invalid("expected 1 to 64 bounded literal terms"));
            }
            Ok(())
        }
        Some("model_judge") => crate::judge::Rule::parse(rule).map(|_| ()),
        Some("asset_scan") => crate::output_delivery::validate_rule(rule),
        _ => Err(Error::Invalid("unsupported guardrail rule")),
    }
}
fn strings(value: &Value, output: &mut String) {
    match value {
        Value::String(s) => {
            output.push_str(s);
            output.push('\n');
        }
        Value::Array(a) => {
            for v in a {
                strings(v, output);
            }
        }
        Value::Object(o) => {
            for v in o.values() {
                strings(v, output);
            }
        }
        _ => {}
    }
}
fn risk(rule: &Value, text: &str) -> Result<bool> {
    validate_rule(rule).map_err(|_| Error::Configuration)?;
    if rule["kind"] == "always" {
        return Ok(true);
    }
    let sensitive = rule["case_sensitive"]
        .as_bool()
        .ok_or(Error::Configuration)?;
    let haystack = if sensitive {
        text.to_owned()
    } else {
        text.to_lowercase()
    };
    Ok(rule["terms"]
        .as_array()
        .ok_or(Error::Configuration)?
        .iter()
        .any(|v| {
            let term = v.as_str().unwrap_or("");
            haystack.contains(&if sensitive {
                term.to_owned()
            } else {
                term.to_lowercase()
            })
        }))
}
pub(crate) async fn event(tx: &mut Tx<'_>, turn: &Turn, kind: &str, data: Value) -> Result<()> {
    // The turn lock serializes allocation AND commit. No skipped late commits behind a consumer cursor.
    let seq: i64=sqlx::query_scalar("UPDATE governance_turns SET event_clock=event_clock+1 WHERE tenant_id=$1 AND id=$2 RETURNING event_clock").bind(turn.tenant_id).bind(turn.id).fetch_one(&mut **tx).await?;
    sqlx::query("INSERT INTO governance_events(tenant_id,turn_id,sequence,event_type,data) VALUES($1,$2,$3,$4,$5)").bind(turn.tenant_id).bind(turn.id).bind(seq).bind(kind).bind(data).execute(&mut **tx).await?;
    Ok(())
}
pub(crate) enum Gate {
    Ready,
    Approval(Value),
    Checking(Vec<Uuid>),
}

pub(crate) async fn gate(
    tx: &mut Tx<'_>,
    who: &Identity,
    turn: &Turn,
    op: &Operation,
    payload: &Value,
) -> Result<Gate> {
    if matches!(op.state.as_str(), "rejected" | "feedback") {
        return Err(Error::ActionRejected);
    }
    if op.stage == "media_output"
        && crate::output_delivery::validate_operation(tx, who, op).await? != *payload
    {
        return Err(Error::PolicyChanged);
    }
    let rules = selected(
        tx,
        who,
        &op.stage,
        op.model_id.as_deref(),
        op.tool_name.as_deref(),
    )
    .await?;
    let snapshot = serde_json::to_value(&rules).map_err(|_| Error::Configuration)?;
    let mut text = String::new();
    strings(payload, &mut text);
    let mut needed = Vec::new();
    let mut evaluated = Vec::new();
    let mut pending = Vec::new();
    if rules
        .iter()
        .filter(|r| r.rule["kind"] == "model_judge")
        .count()
        > 8
    {
        return Err(Error::Configuration);
    }
    for r in rules {
        let grant: Option<Uuid>=sqlx::query_scalar("SELECT approval_id FROM guardrail_grants WHERE tenant_id=$1 AND turn_id=$2 AND guardrail_id=$3 AND version=$4 AND expires_at>clock_timestamp()")
            .bind(who.tenant_id).bind(turn.id).bind(&r.id).bind(r.version).fetch_optional(&mut **tx).await?;
        let (outcome, reason) = if grant.is_some() {
            (
                "skipped_by_user_approval".to_owned(),
                "approved_current_turn".to_owned(),
            )
        } else if r.rule["kind"] == "asset_scan" && op.stage == "media_output" {
            crate::output_delivery::outcome(&r.rule, payload)?
        } else if r.rule["kind"] == "model_judge" {
            match crate::judge::observe(tx, who, op, &r).await? {
                crate::judge::Observation::Pending(id) => {
                    pending.push(id);
                    continue;
                }
                crate::judge::Observation::Complete { outcome, reason } => (outcome, reason),
            }
        } else if risk(&r.rule, &text)? {
            ("risk".to_owned(), "configured_rule_match".to_owned())
        } else {
            ("passed".to_owned(), "configured_rule_passed".to_owned())
        };
        if outcome == "risk" || outcome == "failed" {
            // Preserve the original deterministic approval hash across upgrades.
            let mut item = json!({"id":r.id,"version":r.version,"name":r.name,"stage":r.stage,"reason_code":reason});
            if r.rule["kind"] == "model_judge" || r.rule["kind"] == "asset_scan" {
                item["evaluation_outcome"] = json!(outcome);
            }
            needed.push(item);
        }
        evaluated
            .push(json!({"id":r.id,"version":r.version,"outcome":outcome,"approval_id":grant}));
    }
    if !pending.is_empty() {
        sqlx::query("UPDATE governance_operations SET state='checking' WHERE tenant_id=$1 AND id=$2 AND state IN ('ready','checking','waiting_approval')")
            .bind(who.tenant_id).bind(op.id).execute(&mut **tx).await?;
        return Ok(Gate::Checking(pending));
    }
    if needed.is_empty() {
        if !evaluated.is_empty() {
            event(
                tx,
                turn,
                "guardrails.evaluated",
                json!({"operation_id":op.id,"results":evaluated}),
            )
            .await?;
        }
        if matches!(op.state.as_str(), "waiting_approval" | "checking") {
            sqlx::query(
                "UPDATE governance_operations SET state='ready' WHERE tenant_id=$1 AND id=$2",
            )
            .bind(who.tenant_id)
            .bind(op.id)
            .execute(&mut **tx)
            .await?;
        }
        return Ok(Gate::Ready);
    }
    let hash = digest(&json!({"policy":snapshot,"needed":needed}))?;
    let expires = if op.stage == "post" {
        let expiry:Option<DateTime<Utc>>=sqlx::query_scalar("SELECT result_expires_at FROM requests WHERE tenant_id=$1 AND governance_turn_id=$2 AND id::text=(SELECT operation_key FROM governance_operations WHERE tenant_id=$1 AND id=$3) AND result_ciphertext IS NOT NULL AND result_expires_at>clock_timestamp()")
            .bind(who.tenant_id).bind(turn.id).bind(op.id).fetch_optional(&mut **tx).await?.flatten();
        turn.expires_at.min(expiry.ok_or(Error::ResultUnavailable)?)
    } else if op.stage == "media_output" {
        let expiry: DateTime<Utc> = serde_json::from_value(payload["expires_at"].clone())
            .map_err(|_| Error::Configuration)?;
        turn.expires_at.min(expiry)
    } else {
        turn.expires_at
    };
    let id = Uuid::new_v4();
    let mut summary = json!({"model_id":op.model_id,"tool_name":op.tool_name,"stage":op.stage,"payload_hash":hex(&op.payload_hash),"session_id":turn.session_id,"run_id":turn.run_id,"generation":turn.generation});
    if op.stage == "media_output" {
        summary["output"] = payload.clone();
    }
    let inserted=sqlx::query("INSERT INTO approvals(tenant_id,id,operation_id,turn_id,policy_hash,policy_snapshot,guardrails,expires_at,summary) VALUES($1,$2,$3,$4,$5,$6,$7,CASE WHEN $10 THEN $8 ELSE LEAST($8,clock_timestamp()+interval '15 minutes') END,$9) ON CONFLICT(tenant_id,operation_id,policy_hash) DO NOTHING")
        .bind(who.tenant_id).bind(id).bind(op.id).bind(turn.id).bind(&hash).bind(snapshot).bind(json!(needed)).bind(expires).bind(summary).bind(op.stage=="media_output").execute(&mut **tx).await?;
    let approval: Approval = sqlx::query_as(
        "SELECT * FROM approvals WHERE tenant_id=$1 AND operation_id=$2 AND policy_hash=$3",
    )
    .bind(who.tenant_id)
    .bind(op.id)
    .bind(hash)
    .fetch_one(&mut **tx)
    .await?;
    if approval.expires_at <= Utc::now() {
        return Err(Error::TurnClosed);
    }
    if inserted.rows_affected() == 1 {
        event(tx,turn,"approval.required",json!({"approval_id":approval.id,"operation_id":op.id,"guardrails":approval.guardrails,"evaluations":evaluated})).await?;
    }
    sqlx::query("UPDATE governance_operations SET state='waiting_approval' WHERE tenant_id=$1 AND id=$2 AND state IN ('ready','checking','waiting_approval')").bind(who.tenant_id).bind(op.id).execute(&mut **tx).await?;
    Ok(Gate::Approval(approval_view(&approval)))
}
pub(crate) fn approval_view(a: &Approval) -> Value {
    json!({"id":a.id,"operation_id":a.operation_id,"turn_id":a.turn_id,"state":a.state,"guardrails":a.guardrails,"summary":a.summary,"expires_at":a.expires_at,"scope":"current_turn_only","resume_requires_recheck":true,"scope_notice":"继续本轮后，该轮后续动作也会跳过所选护栏；不扩大工具权限，不适用于新轮次。","options":["continue_this_turn","reject","feedback"]})
}
pub async fn check(
    pool: &PgPool,
    cipher: &ResultCipher,
    who: &Identity,
    check: Check,
) -> Result<Value> {
    if !valid_text(&check.operation_key, 200)
        || !matches!(
            check.stage.as_str(),
            "pre" | "post" | "tool_input" | "tool_output" | "artifact"
        )
        || check
            .tool_name
            .as_deref()
            .is_some_and(|s| !valid_text(s, 200))
    {
        return Err(Error::Invalid("invalid governance operation"));
    }
    let encoded =
        serde_json::to_vec(&check.payload).map_err(|_| Error::Invalid("invalid payload"))?;
    if encoded.len() > 4 * 1024 * 1024 {
        return Err(Error::Invalid("governance payload too large"));
    }
    if let Some(model) = &check.model_id {
        store::authorize_model(pool, who, model).await?;
    }
    let mut tx = pool.begin().await?;
    tenant_lock(&mut tx, who.tenant_id).await?;
    let turn = turn_locked(&mut tx, who, check.turn_id, true).await?;
    let hash = digest(
        &json!({"turn":check.turn_id,"model":check.model_id,"tool":check.tool_name,"payload":check.payload}),
    )?;
    let id = Uuid::new_v4();
    let encrypted = cipher.encrypt(who.tenant_id, id, &encoded)?;
    sqlx::query("INSERT INTO governance_operations(tenant_id,id,turn_id,key_id,operation_key,stage,model_id,tool_name,payload_hash,payload_ciphertext) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10) ON CONFLICT(tenant_id,key_id,operation_key,stage) DO NOTHING")
        .bind(who.tenant_id).bind(id).bind(turn.id).bind(who.key_id).bind(&check.operation_key).bind(&check.stage).bind(&check.model_id).bind(&check.tool_name).bind(&hash).bind(encrypted).execute(&mut *tx).await?;
    let op: Operation=sqlx::query_as("SELECT * FROM governance_operations WHERE tenant_id=$1 AND key_id=$2 AND operation_key=$3 AND stage=$4 FOR UPDATE").bind(who.tenant_id).bind(who.key_id).bind(check.operation_key).bind(check.stage).fetch_one(&mut *tx).await?;
    if op.payload_hash != hash {
        return Err(Error::Conflict);
    }
    if matches!(
        op.state.as_str(),
        "executing" | "execution_unknown" | "completed"
    ) {
        let result = operation_view(&op);
        tx.commit().await?;
        return Ok(result);
    }
    let waiting = gate(&mut tx, who, &turn, &op, &check.payload).await?;
    tx.commit().await?;
    Ok(match waiting {
        Gate::Approval(approval) => {
            json!({"id":op.id,"state":"waiting_approval","approval":approval})
        }
        Gate::Checking(evaluations) => {
            json!({"id":op.id,"state":"checking","evaluations":evaluations,"turn_id":turn.id})
        }
        Gate::Ready => {
            json!({"id":op.id,"state":"ready","turn_id":turn.id,"payload_hash":hex(&hash)})
        }
    })
}
pub fn require_ready(value: Value) -> Result<()> {
    if value["state"] == "ready" {
        Ok(())
    } else if value["state"] == "waiting_approval" {
        Err(Error::ApprovalRequired(value["approval"].clone()))
    } else {
        Err(Error::InProgress)
    }
}
pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn operation_view(op: &Operation) -> Value {
    let state = if op.state == "executing" && op.execution_deadline.is_some_and(|d| d <= Utc::now())
    {
        "execution_unknown"
    } else {
        &op.state
    };
    json!({"id":op.id,"turn_id":op.turn_id,"state":state,"stage":op.stage,"payload_hash":hex(&op.payload_hash),"execution_deadline":op.execution_deadline})
}
pub async fn operation(
    pool: &PgPool,
    cipher: &ResultCipher,
    who: &Identity,
    id: Uuid,
) -> Result<Value> {
    let mut tx = pool.begin().await?;
    tenant_lock(&mut tx, who.tenant_id).await?;
    let op: Operation = sqlx::query_as(
        "SELECT * FROM governance_operations WHERE tenant_id=$1 AND key_id=$2 AND id=$3",
    )
    .bind(who.tenant_id)
    .bind(who.key_id)
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(Error::NotFound)?;
    let turn = turn_locked(&mut tx, who, op.turn_id, false).await?;
    let mut view = operation_view(&op);
    // Receipts are opaque runtime references, not protected tool output. No pending payload endpoint.
    if turn.state == "active"
        && turn.expires_at > Utc::now()
        && let Some(bytes) = op.receipt_ciphertext
    {
        view["receipt"] = serde_json::from_slice(&cipher.decrypt(who.tenant_id, id, &bytes)?)
            .map_err(|_| Error::ResultUnavailable)?;
    }
    tx.commit().await?;
    Ok(view)
}
pub async fn approvals(
    pool: &PgPool,
    cipher: &ResultCipher,
    who: &Identity,
    turn_id: Uuid,
) -> Result<Value> {
    let mut tx = pool.begin().await?;
    tenant_lock(&mut tx, who.tenant_id).await?;
    let turn = turn_locked(&mut tx, who, turn_id, false).await?;
    let rows: Vec<Approval>=sqlx::query_as("SELECT * FROM approvals WHERE tenant_id=$1 AND turn_id=$2 ORDER BY created_at DESC,id DESC LIMIT 100").bind(who.tenant_id).bind(turn_id).fetch_all(&mut *tx).await?;
    let mut data = Vec::new();
    for a in rows {
        let mut v = approval_view(&a);
        if a.state == "pending" && (turn.state != "active" || a.expires_at <= Utc::now()) {
            v["state"] = "expired".into();
        }
        if a.expires_at + chrono::Duration::hours(24) > Utc::now()
            && let Some(bytes) = a.feedback_ciphertext
        {
            v["feedback"] = serde_json::from_slice(&cipher.decrypt(who.tenant_id, a.id, &bytes)?)
                .map_err(|_| Error::ResultUnavailable)?;
        }
        data.push(v);
    }
    tx.commit().await?;
    Ok(json!({"data":data,"limit":100,"events_are_complete_history":true}))
}
pub async fn events(
    pool: &PgPool,
    who: &Identity,
    turn_id: Uuid,
    after: i64,
    ack: Option<i64>,
) -> Result<Value> {
    if after < 0 || ack.is_some_and(|v| v < 0) {
        return Err(Error::Invalid("negative event cursor"));
    }
    let mut tx = pool.begin().await?;
    tenant_lock(&mut tx, who.tenant_id).await?;
    turn_locked(&mut tx, who, turn_id, false).await?;
    if let Some(ack) = ack {
        let max: i64 = sqlx::query_scalar(
            "SELECT event_clock FROM governance_turns WHERE tenant_id=$1 AND id=$2",
        )
        .bind(who.tenant_id)
        .bind(turn_id)
        .fetch_one(&mut *tx)
        .await?;
        if ack > max {
            return Err(Error::Invalid("cannot acknowledge unseen future events"));
        }
        sqlx::query("INSERT INTO governance_event_acks(tenant_id,turn_id,sequence) VALUES($1,$2,$3) ON CONFLICT(tenant_id,turn_id) DO UPDATE SET sequence=GREATEST(governance_event_acks.sequence,EXCLUDED.sequence)").bind(who.tenant_id).bind(turn_id).bind(ack).execute(&mut *tx).await?;
    }
    let rows: Vec<(i64,String,Value)>=sqlx::query_as("SELECT sequence,event_type,data FROM governance_events WHERE tenant_id=$1 AND turn_id=$2 AND sequence>$3 ORDER BY sequence LIMIT 100").bind(who.tenant_id).bind(turn_id).bind(after).fetch_all(&mut *tx).await?;
    let next = rows.last().map_or(after, |r| r.0);
    tx.commit().await?;
    Ok(
        json!({"data":rows.into_iter().map(|(sequence,event_type,data)|json!({"sequence":sequence,"type":event_type,"data":data})).collect::<Vec<_>>(),"next_cursor":next}),
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Claim {
    pub token: String,
    pub payload_hash: String,
}
pub async fn claim(
    pool: &PgPool,
    cipher: &ResultCipher,
    who: &Identity,
    id: Uuid,
    claim: Claim,
) -> Result<Value> {
    // Runtime persists a random 256-bit claim token BEFORE this call. A lost response can be retried by that owner only.
    if claim.token.len() != 64 || !claim.token.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::Invalid(
            "claim token must be 32 random bytes encoded as hex",
        ));
    }
    let mut tx = pool.begin().await?;
    tenant_lock(&mut tx, who.tenant_id).await?;
    let mut op: Operation = sqlx::query_as(
        "SELECT * FROM governance_operations WHERE tenant_id=$1 AND key_id=$2 AND id=$3 FOR UPDATE",
    )
    .bind(who.tenant_id)
    .bind(who.key_id)
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(Error::NotFound)?;
    let turn = turn_locked(&mut tx, who, op.turn_id, true).await?;
    if !matches!(op.stage.as_str(), "tool_input" | "tool_output" | "artifact") {
        return Err(Error::Invalid(
            "model operations cannot be claimed by a runtime",
        ));
    }
    if claim.payload_hash != hex(&op.payload_hash) {
        return Err(Error::Conflict);
    }
    let token_hash = store::hash(claim.token.as_bytes());
    if op.state == "executing"
        && op.claim_hash.as_ref() == Some(&token_hash)
        && op.execution_deadline.is_some_and(|d| d > Utc::now())
    {
        tx.commit().await?;
        let mut view = operation_view(&op);
        view["dispatch_authorized"] = false.into();
        return Ok(view);
    }
    if matches!(
        op.state.as_str(),
        "executing" | "execution_unknown" | "completed"
    ) {
        return Err(Error::InProgress);
    }
    let payload: Value = serde_json::from_slice(
        &cipher.decrypt(
            who.tenant_id,
            id,
            op.payload_ciphertext
                .as_deref()
                .ok_or(Error::ResultUnavailable)?,
        )?,
    )
    .map_err(|_| Error::ResultUnavailable)?;
    match gate(&mut tx, who, &turn, &op, &payload).await? {
        Gate::Approval(a) => {
            tx.commit().await?;
            return Err(Error::ApprovalRequired(a));
        }
        Gate::Checking(_) => {
            tx.commit().await?;
            return Err(Error::InProgress);
        }
        Gate::Ready => {}
    }
    let deadline: DateTime<Utc>=sqlx::query_scalar("UPDATE governance_operations SET state='executing',claim_hash=$3,execution_deadline=LEAST($4,clock_timestamp()+interval '60 seconds') WHERE tenant_id=$1 AND id=$2 RETURNING execution_deadline").bind(who.tenant_id).bind(id).bind(token_hash).bind(turn.expires_at).fetch_one(&mut *tx).await?;
    event(
        &mut tx,
        &turn,
        "operation.claimed",
        json!({"operation_id":id,"execution_deadline":deadline}),
    )
    .await?;
    tx.commit().await?;
    op.state = "executing".into();
    op.execution_deadline = Some(deadline);
    let mut view = operation_view(&op);
    view["dispatch_authorized"] = true.into();
    Ok(view)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub token: String,
    pub outcome: String,
    pub evidence_ref: String,
}
pub async fn receipt(
    pool: &PgPool,
    cipher: &ResultCipher,
    who: &Identity,
    id: Uuid,
    receipt: Receipt,
) -> Result<Value> {
    if !matches!(receipt.outcome.as_str(), "completed" | "execution_unknown")
        || !valid_text(&receipt.evidence_ref, 1000)
    {
        return Err(Error::Invalid("invalid execution receipt"));
    }
    let value = json!({"outcome":receipt.outcome,"evidence_ref":receipt.evidence_ref});
    let hash = digest(&value)?;
    let mut tx = pool.begin().await?;
    tenant_lock(&mut tx, who.tenant_id).await?;
    let op: Operation = sqlx::query_as(
        "SELECT * FROM governance_operations WHERE tenant_id=$1 AND key_id=$2 AND id=$3 FOR UPDATE",
    )
    .bind(who.tenant_id)
    .bind(who.key_id)
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(Error::NotFound)?;
    // A late receipt may record an already-started side effect after turn closure; it never starts a new action.
    let turn = turn_locked(&mut tx, who, op.turn_id, false).await?;
    if op.claim_hash.as_ref() != Some(&store::hash(receipt.token.as_bytes())) {
        return Err(Error::NotFound);
    }
    if let Some(existing) = op.receipt_hash {
        if existing != hash {
            return Err(Error::Conflict);
        }
        tx.commit().await?;
        return Ok(value);
    }
    if !matches!(op.state.as_str(), "executing" | "execution_unknown") {
        return Err(Error::InProgress);
    }
    let encrypted = cipher.encrypt(
        who.tenant_id,
        id,
        &serde_json::to_vec(&value).map_err(|_| Error::Invalid("invalid receipt"))?,
    )?;
    sqlx::query("UPDATE governance_operations SET state=$3,receipt_hash=$4,receipt_ciphertext=$5,payload_ciphertext=NULL WHERE tenant_id=$1 AND id=$2").bind(who.tenant_id).bind(id).bind(receipt.outcome).bind(hash).bind(encrypted).execute(&mut *tx).await?;
    event(
        &mut tx,
        &turn,
        "operation.receipted",
        json!({"operation_id":id,"outcome":value["outcome"]}),
    )
    .await?;
    tx.commit().await?;
    Ok(value)
}
pub async fn maintain(pool: &PgPool) -> Result<()> {
    crate::judge::maintain(pool).await?;
    sqlx::query("WITH batch AS (SELECT tenant_id,id FROM governance_operations WHERE state='executing' AND execution_deadline<=clock_timestamp() ORDER BY execution_deadline LIMIT 100 FOR UPDATE SKIP LOCKED) UPDATE governance_operations o SET state='execution_unknown' FROM batch b WHERE o.tenant_id=b.tenant_id AND o.id=b.id").execute(pool).await?;
    sqlx::query("WITH batch AS (SELECT o.tenant_id,o.id FROM governance_operations o JOIN governance_turns t ON o.tenant_id=t.tenant_id AND o.turn_id=t.id WHERE (t.state<>'active' OR t.expires_at<=clock_timestamp()) AND (o.payload_ciphertext IS NOT NULL OR o.receipt_ciphertext IS NOT NULL) ORDER BY t.expires_at,o.id LIMIT 100 FOR UPDATE OF o SKIP LOCKED) UPDATE governance_operations o SET payload_ciphertext=NULL,receipt_ciphertext=NULL FROM batch b WHERE o.tenant_id=b.tenant_id AND o.id=b.id").execute(pool).await?;
    sqlx::query("WITH batch AS (SELECT r.id FROM requests r JOIN governance_turns t ON r.tenant_id=t.tenant_id AND r.governance_turn_id=t.id WHERE (t.state<>'active' OR t.expires_at<=clock_timestamp()) AND r.result_ciphertext IS NOT NULL ORDER BY t.expires_at,r.id LIMIT 100 FOR UPDATE OF r SKIP LOCKED) UPDATE requests r SET result_ciphertext=NULL FROM batch b WHERE r.id=b.id").execute(pool).await?;
    Ok(())
}

pub async fn approval(
    pool: &PgPool,
    cipher: &ResultCipher,
    who: &Identity,
    id: Uuid,
) -> Result<Value> {
    let mut tx = pool.begin().await?;
    tenant_lock(&mut tx, who.tenant_id).await?;
    let a:Approval=sqlx::query_as("SELECT a.* FROM approvals a JOIN governance_operations o ON o.tenant_id=a.tenant_id AND o.id=a.operation_id WHERE a.tenant_id=$1 AND a.id=$2 AND o.key_id=$3").bind(who.tenant_id).bind(id).bind(who.key_id).fetch_optional(&mut *tx).await?.ok_or(Error::NotFound)?;
    let turn = turn_locked(&mut tx, who, a.turn_id, false).await?;
    let mut v = approval_view(&a);
    if a.state == "pending" && (turn.state != "active" || a.expires_at <= Utc::now()) {
        v["state"] = "expired".into();
    }
    if a.expires_at + chrono::Duration::hours(24) > Utc::now()
        && let Some(bytes) = a.feedback_ciphertext
    {
        v["feedback"] = serde_json::from_slice(&cipher.decrypt(who.tenant_id, id, &bytes)?)
            .map_err(|_| Error::ResultUnavailable)?;
    }
    tx.commit().await?;
    Ok(v)
}
pub async fn close_runtime(pool: &PgPool, who: &Identity, id: Uuid, state: &str) -> Result<Value> {
    if !matches!(state, "completed" | "cancelled" | "failed") {
        return Err(Error::Invalid("invalid terminal turn state"));
    }
    let mut tx = pool.begin().await?;
    tenant_lock(&mut tx, who.tenant_id).await?;
    let turn = turn_locked(&mut tx, who, id, false).await?;
    if turn.state != "active" && turn.state != state {
        return Err(Error::Conflict);
    }
    if turn.state == "active" {
        sqlx::query("UPDATE governance_turns SET state=$3 WHERE tenant_id=$1 AND id=$2")
            .bind(who.tenant_id)
            .bind(id)
            .bind(state)
            .execute(&mut *tx)
            .await?;
        event(
            &mut tx,
            &turn,
            "turn.closed",
            json!({"state":state,"actor":"bound_runtime"}),
        )
        .await?;
    }
    tx.commit().await?;
    Ok(json!({"id":id,"state":state}))
}
