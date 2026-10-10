//! Immutable text results supplied by a runtime after its claimed tool ran.
//! Recording execution evidence and releasing governed text are separate facts.
use crate::{
    cipher::ResultCipher,
    error::{Error, Result},
    gateway::Gateway,
    governance::{self, Gate, Operation, Tx},
    store::{self, Identity},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

/// Leaves room for record/release metadata below the adapter's 1 MiB response cap.
pub const MAX_TEXT_JSON_BYTES: usize = 512 * 1024;
pub(crate) const OUTPUT_PREFIX: &str = "runtime-result-output:";

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub input_operation_id: Uuid,
    pub claim_token: String,
    pub evidence_ref: String,
    pub text: String,
}

#[derive(sqlx::FromRow)]
struct Stored {
    id: Uuid,
    key_id: Uuid,
    turn_id: Uuid,
    input_operation_id: Uuid,
    model_id: Option<String>,
    tool_name: String,
    claim_hash: Vec<u8>,
    receipt_hash: Vec<u8>,
    evidence_ref: String,
    text_sha256: Vec<u8>,
    bytes: i32,
    result_ciphertext: Option<Vec<u8>>,
    expires_at: DateTime<Utc>,
}

fn metadata(row: &Stored) -> Value {
    json!({"id":row.id,"state":"recorded","input_operation_id":row.input_operation_id,
        "turn_id":row.turn_id,"text_sha256":governance::hex(&row.text_sha256),
        "bytes":row.bytes,"expires_at":row.expires_at})
}

fn ttl_seconds() -> Result<i32> {
    let ttl = match std::env::var("BROKERROUTER_RESULT_TTL_SECONDS") {
        Ok(value) => value.parse().map_err(|_| Error::Configuration)?,
        Err(std::env::VarError::NotPresent) => 86400,
        Err(_) => return Err(Error::Configuration),
    };
    if !(60..=604800).contains(&ttl) {
        return Err(Error::Configuration);
    }
    Ok(ttl)
}

/// Freeze one result and its completed receipt in the same transaction.
/// Retrying a lost response with the same claim, evidence and text returns the
/// original metadata; expiry never permits replacing or regenerating a result.
pub async fn record(
    pool: &PgPool,
    cipher: &ResultCipher,
    who: &Identity,
    request: Record,
) -> Result<Value> {
    if request.claim_token.len() != 64
        || !request.claim_token.bytes().all(|b| b.is_ascii_hexdigit())
        || !governance::valid_text(&request.evidence_ref, 1000)
    {
        return Err(Error::Invalid(
            "invalid tool result claim or evidence reference",
        ));
    }
    let encoded_text = serde_json::to_vec(&request.text)
        .map_err(|_| Error::Invalid("invalid tool result text"))?;
    if encoded_text.len() > MAX_TEXT_JSON_BYTES {
        return Err(Error::Invalid(
            "unsupported tool result: text JSON exceeds 512 KiB; no truncation is performed",
        ));
    }
    let ttl = ttl_seconds()?;
    let text_hash = store::hash(request.text.as_bytes());
    let claim_hash = store::hash(request.claim_token.as_bytes());
    let receipt_hash =
        governance::digest(&json!({"outcome":"completed","evidence_ref":request.evidence_ref}))?;
    let mut tx = pool.begin().await?;
    governance::tenant_lock(&mut tx, who.tenant_id).await?;
    let op: Operation = sqlx::query_as(
        "SELECT * FROM governance_operations WHERE tenant_id=$1 AND key_id=$2 AND id=$3 FOR UPDATE",
    )
    .bind(who.tenant_id)
    .bind(who.key_id)
    .bind(request.input_operation_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(Error::NotFound)?;
    if op.stage != "tool_input" || op.tool_name.is_none() {
        return Err(Error::Invalid(
            "a named claimed tool_input operation is required",
        ));
    }
    if op.claim_hash.as_ref() != Some(&claim_hash) {
        return Err(Error::NotFound);
    }
    let turn = governance::turn_locked(&mut tx, who, op.turn_id, false).await?;
    let existing: Option<Stored> = sqlx::query_as(
        "SELECT * FROM runtime_tool_results WHERE tenant_id=$1 AND input_operation_id=$2 FOR UPDATE",
    )
    .bind(who.tenant_id)
    .bind(op.id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(row) = existing {
        if row.key_id != who.key_id
            || row.turn_id != turn.id
            || row.model_id != op.model_id
            || op.tool_name.as_deref() != Some(row.tool_name.as_str())
            || row.claim_hash != claim_hash
            || row.receipt_hash != receipt_hash
            || row.evidence_ref != request.evidence_ref
            || row.text_sha256 != text_hash
            || usize::try_from(row.bytes).ok() != Some(request.text.len())
        {
            return Err(Error::Conflict);
        }
        let response = metadata(&row);
        tx.commit().await?;
        return Ok(response);
    }
    if op.receipt_hash.is_some() && op.state != "completed" {
        return Err(Error::InProgress);
    }
    governance::receipt_in_tx(
        &mut tx,
        cipher,
        who,
        op.id,
        &governance::Receipt {
            token: request.claim_token,
            outcome: "completed".into(),
            evidence_ref: request.evidence_ref.clone(),
        },
    )
    .await?;
    let id = Uuid::new_v4();
    let encrypted = cipher.encrypt(who.tenant_id, id, request.text.as_bytes())?;
    let row: Stored = sqlx::query_as(
        "INSERT INTO runtime_tool_results(tenant_id,id,key_id,turn_id,input_operation_id,model_id,tool_name,claim_hash,receipt_hash,evidence_ref,text_sha256,bytes,result_ciphertext,created_at,expires_at) SELECT $1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,stamp.ts,stamp.ts+make_interval(secs=>$14::integer) FROM (SELECT clock_timestamp() AS ts) stamp RETURNING *",
    )
    .bind(who.tenant_id)
    .bind(id)
    .bind(who.key_id)
    .bind(turn.id)
    .bind(op.id)
    .bind(op.model_id)
    .bind(op.tool_name.ok_or(Error::Configuration)?)
    .bind(claim_hash)
    .bind(receipt_hash)
    .bind(request.evidence_ref)
    .bind(text_hash)
    .bind(i32::try_from(request.text.len()).map_err(|_| Error::Configuration)?)
    .bind(encrypted)
    .bind(ttl)
    .fetch_one(&mut *tx)
    .await?;
    governance::event(
        &mut tx,
        &turn,
        "tool_result.recorded",
        json!({"result_id":id,"input_operation_id":op.id,
            "text_sha256":governance::hex(&row.text_sha256),"bytes":row.bytes}),
    )
    .await?;
    let response = metadata(&row);
    tx.commit().await?;
    Ok(response)
}

async fn current(
    tx: &mut Tx<'_>,
    cipher: &ResultCipher,
    who: &Identity,
    id: Uuid,
) -> Result<(Stored, governance::Turn, String)> {
    let row: Stored = sqlx::query_as(
        "SELECT * FROM runtime_tool_results WHERE tenant_id=$1 AND key_id=$2 AND id=$3 FOR UPDATE",
    )
    .bind(who.tenant_id)
    .bind(who.key_id)
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(Error::NotFound)?;
    let turn = governance::turn_locked(tx, who, row.turn_id, true).await?;
    if let Some(model) = &row.model_id {
        let allowed: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM key_models g JOIN models m ON m.tenant_id=g.tenant_id AND m.id=g.model_id WHERE g.tenant_id=$1 AND g.key_id=$2 AND g.model_id=$3 AND m.active)",
        )
        .bind(who.tenant_id)
        .bind(who.key_id)
        .bind(model)
        .fetch_one(&mut **tx)
        .await?;
        if !allowed {
            return Err(Error::Unauthorized);
        }
    }
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut **tx)
        .await?;
    if row.expires_at <= now {
        return Err(Error::ResultUnavailable);
    }
    let text = String::from_utf8(
        cipher.decrypt(
            who.tenant_id,
            id,
            row.result_ciphertext
                .as_deref()
                .ok_or(Error::ResultUnavailable)?,
        )?,
    )
    .map_err(|_| Error::ResultUnavailable)?;
    if store::hash(text.as_bytes()) != row.text_sha256
        || usize::try_from(row.bytes).ok() != Some(text.len())
        || serde_json::to_vec(&text)
            .map_err(|_| Error::ResultUnavailable)?
            .len()
            > MAX_TEXT_JSON_BYTES
    {
        return Err(Error::ResultUnavailable);
    }
    Ok((row, turn, text))
}

fn output(row: &Stored, text: &str) -> governance::Check {
    governance::Check {
        turn_id: row.turn_id,
        operation_key: format!("{OUTPUT_PREFIX}{}", row.id),
        stage: "tool_output".into(),
        model_id: row.model_id.clone(),
        tool_name: Some(row.tool_name.clone()),
        payload: json!({"result_id":row.id,"input_operation_id":row.input_operation_id,"text":text}),
    }
}

/// POST-only action: semantic policy evaluation may incur its existing ledger
/// cost. This function never executes or claims a tool/output operation.
pub async fn release(gateway: &Gateway, who: &Identity, id: Uuid) -> Result<Value> {
    let mut tx = gateway.pool.begin().await?;
    governance::tenant_lock(&mut tx, who.tenant_id).await?;
    let (row, _, text) = current(&mut tx, &gateway.cipher, who, id).await?;
    let check = output(&row, &text);
    tx.commit().await?;
    // Gateway::check drives pending semantic jobs. Its completed shortcut is
    // deliberately insufficient as publication authority; gate is re-run below.
    let checked = gateway.check(who, check).await?;
    let output_id: Uuid =
        serde_json::from_value(checked["id"].clone()).map_err(|_| Error::Configuration)?;

    let mut tx = gateway.pool.begin().await?;
    governance::tenant_lock(&mut tx, who.tenant_id).await?;
    let (row, turn, text) = current(&mut tx, &gateway.cipher, who, id).await?;
    let check = output(&row, &text);
    let hash = governance::digest(&json!({"turn":check.turn_id,"model":check.model_id,
        "tool":check.tool_name,"payload":check.payload}))?;
    let op: Operation = sqlx::query_as(
        "SELECT * FROM governance_operations WHERE tenant_id=$1 AND key_id=$2 AND id=$3 AND operation_key=$4 AND stage='tool_output' FOR UPDATE",
    )
    .bind(who.tenant_id)
    .bind(who.key_id)
    .bind(output_id)
    .bind(&check.operation_key)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(Error::NotFound)?;
    if op.turn_id != row.turn_id
        || op.model_id != row.model_id
        || op.tool_name != check.tool_name
        || op.payload_hash != hash
    {
        return Err(Error::Conflict);
    }
    if let Some(encrypted) = &op.payload_ciphertext {
        let payload: Value =
            serde_json::from_slice(&gateway.cipher.decrypt(who.tenant_id, op.id, encrypted)?)
                .map_err(|_| Error::ResultUnavailable)?;
        if payload != check.payload {
            return Err(Error::ResultUnavailable);
        }
    }
    let (waiting, grant_deadline) =
        governance::gate_with_grant_deadline(&mut tx, who, &turn, &op, &check.payload).await?;
    // Gate evaluation can outlive a wall-clock expiry even while all authority
    // rows are locked. Check the live turn/key and result deadline once more.
    governance::turn_locked(&mut tx, who, row.turn_id, true).await?;
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut *tx)
        .await?;
    if row.expires_at <= now {
        return Err(Error::ResultUnavailable);
    }
    governance::require_live_grants(&mut tx, grant_deadline).await?;
    let mut response = metadata(&row);
    response["id"] = json!(op.id);
    response["result_id"] = json!(row.id);
    response["output_operation_id"] = json!(op.id);
    response["payload_hash"] = json!(governance::hex(&hash));
    match waiting {
        Gate::Ready => {
            response["state"] = json!("ready");
            response["text"] = json!(text);
        }
        Gate::Approval(approval) => {
            response["state"] = json!("waiting_approval");
            response["approval"] = approval;
        }
        Gate::Checking(evaluations) => {
            // A generic runtime receipt clears operation payload. A new judge
            // cannot evaluate that operation safely; never turn its completion
            // or pending row into permission to return the separately held text.
            if op.payload_ciphertext.is_none() {
                return Err(Error::ResultUnavailable);
            }
            response["state"] = json!("checking");
            response["evaluations"] = json!(evaluations);
        }
    }
    tx.commit().await?;
    Ok(response)
}

/// Remove expired encrypted bodies and their semantic-input copies, retaining
/// immutable execution identity, receipt hashes, evidence and content hashes.
pub async fn maintain(pool: &PgPool) -> Result<u64> {
    let mut tx = pool.begin().await?;
    let rows: Vec<(Uuid, Uuid, Uuid)> = sqlx::query_as(
        "SELECT tenant_id,id,key_id FROM runtime_tool_results WHERE expires_at<=clock_timestamp() AND result_ciphertext IS NOT NULL ORDER BY expires_at,id LIMIT 100 FOR UPDATE SKIP LOCKED",
    )
    .fetch_all(&mut *tx)
    .await?;
    for (tenant, id, key) in &rows {
        sqlx::query("UPDATE governance_operations SET payload_ciphertext=NULL WHERE tenant_id=$1 AND key_id=$2 AND operation_key=$3 AND stage='tool_output'")
            .bind(tenant).bind(key).bind(format!("{OUTPUT_PREFIX}{id}"))
            .execute(&mut *tx).await?;
        sqlx::query(
            "UPDATE runtime_tool_results SET result_ciphertext=NULL WHERE tenant_id=$1 AND id=$2",
        )
        .bind(tenant)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(rows.len() as u64)
}
