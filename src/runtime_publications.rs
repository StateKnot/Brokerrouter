//! Final assistant text after native transformations, bound to real gateway calls.
use crate::{
    cipher::ResultCipher,
    error::{Error, Result},
    gateway::Gateway,
    governance::{self, Gate, Operation, Tx},
    runtime_results,
    store::{self, Identity},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

pub(crate) const OUTPUT_PREFIX: &str = "runtime-final-output:";

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub turn_id: Uuid,
    pub model_id: String,
    pub source_request_ids: Vec<Uuid>,
    pub text: String,
}

#[derive(sqlx::FromRow)]
struct Stored {
    id: Uuid,
    turn_id: Uuid,
    model_id: String,
    source_requests: Value,
    text_sha256: Vec<u8>,
    bytes: i32,
    result_ciphertext: Option<Vec<u8>>,
    expires_at: DateTime<Utc>,
}

fn metadata(row: &Stored) -> Value {
    json!({"id":row.id,"state":"recorded","turn_id":row.turn_id,
        "model_id":row.model_id,"source_request_ids":source_ids(row),
        "text_sha256":governance::hex(&row.text_sha256),"bytes":row.bytes,
        "expires_at":row.expires_at})
}
fn source_ids(row: &Stored) -> Vec<Value> {
    row.source_requests
        .as_array()
        .into_iter()
        .flatten()
        .map(|source| source["id"].clone())
        .collect()
}
async fn model_allowed(tx: &mut Tx<'_>, who: &Identity, model: &str) -> Result<()> {
    let allowed: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM key_models g JOIN models m ON m.tenant_id=g.tenant_id AND m.id=g.model_id WHERE g.tenant_id=$1 AND g.key_id=$2 AND g.model_id=$3 AND m.active)",
    ).bind(who.tenant_id).bind(who.key_id).bind(model).fetch_one(&mut **tx).await?;
    if !allowed {
        return Err(Error::Unauthorized);
    }
    Ok(())
}

/// Capture transformed final text. A lost ACK cannot replace its body or sources.
pub async fn record(
    pool: &PgPool,
    cipher: &ResultCipher,
    who: &Identity,
    mut request: Record,
) -> Result<Value> {
    if !governance::valid_text(&request.model_id, 200)
        || !(1..=64).contains(&request.source_request_ids.len())
        || serde_json::to_vec(&request.text)
            .map_err(|_| Error::Invalid("invalid final text"))?
            .len()
            > runtime_results::MAX_TEXT_JSON_BYTES
    {
        return Err(Error::Invalid("invalid final text or gateway source list"));
    }
    request.source_request_ids.sort_unstable();
    if request
        .source_request_ids
        .windows(2)
        .any(|pair| pair[0] == pair[1])
    {
        return Err(Error::Invalid("duplicate gateway source request"));
    }
    let ttl = runtime_results::ttl_seconds()?;
    let hash = store::hash(request.text.as_bytes());
    let mut tx = pool.begin().await?;
    governance::tenant_lock(&mut tx, who.tenant_id).await?;
    let turn = governance::turn_locked(&mut tx, who, request.turn_id, false).await?;
    let existing: Option<Stored>=sqlx::query_as(
        "SELECT * FROM runtime_final_publications WHERE tenant_id=$1 AND key_id=$2 AND turn_id=$3 FOR UPDATE"
    ).bind(who.tenant_id).bind(who.key_id).bind(turn.id).fetch_optional(&mut *tx).await?;
    if let Some(row) = existing {
        let ids: Vec<Value> = request
            .source_request_ids
            .iter()
            .map(|id| json!(id))
            .collect();
        if row.model_id != request.model_id
            || source_ids(&row) != ids
            || row.text_sha256 != hash
            || usize::try_from(row.bytes).ok() != Some(request.text.len())
        {
            return Err(Error::Conflict);
        }
        let response = metadata(&row);
        tx.commit().await?;
        return Ok(response);
    }
    model_allowed(&mut tx, who, &request.model_id).await?;
    let mut sources = Vec::with_capacity(request.source_request_ids.len());
    let mut earliest: Option<DateTime<Utc>> = None;
    for id in request.source_request_ids {
        let source: Option<(Vec<u8>,Vec<u8>,DateTime<Utc>)>=sqlx::query_as(
            "SELECT request_hash,result_ciphertext,result_expires_at FROM requests WHERE tenant_id=$1 AND key_id=$2 AND id=$3 AND governance_turn_id=$4 AND model_id=$5 AND status='succeeded' AND guardrail_evaluation_id IS NULL AND routing_snapshot->0->'endpoint'->>'operation'='chat' AND result_ciphertext IS NOT NULL AND result_expires_at>clock_timestamp() FOR SHARE"
        ).bind(who.tenant_id).bind(who.key_id).bind(id).bind(turn.id)
            .bind(&request.model_id).fetch_optional(&mut *tx).await?;
        let (request_hash, encrypted, expiry) = source.ok_or(Error::ResultUnavailable)?;
        sources.push(
            json!({"id":id,"request_hash":governance::hex(&request_hash),
            "ciphertext_sha256":governance::hex(&store::hash(&encrypted)),"expires_at":expiry}),
        );
        earliest = Some(earliest.map_or(expiry, |old| old.min(expiry)));
    }
    let id = Uuid::new_v4();
    let encrypted = cipher.encrypt(who.tenant_id, id, request.text.as_bytes())?;
    let row:Stored=sqlx::query_as(
        "INSERT INTO runtime_final_publications(tenant_id,id,key_id,turn_id,model_id,source_requests,text_sha256,bytes,result_ciphertext,created_at,expires_at) SELECT $1,$2,$3,$4,$5,$6,$7,$8,$9,stamp.ts,LEAST(stamp.ts+make_interval(secs=>$10::integer),$11) FROM (SELECT clock_timestamp() AS ts) stamp RETURNING *"
    ).bind(who.tenant_id).bind(id).bind(who.key_id).bind(turn.id).bind(request.model_id)
        .bind(json!(sources)).bind(hash).bind(i32::try_from(request.text.len()).map_err(|_|Error::Configuration)?)
        .bind(encrypted).bind(ttl).bind(earliest.ok_or(Error::Configuration)?)
        .fetch_one(&mut *tx).await?;
    governance::event(
        &mut tx,
        &turn,
        "final_text.recorded",
        json!({"publication_id":id,
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
    let row:Stored=sqlx::query_as(
        "SELECT * FROM runtime_final_publications WHERE tenant_id=$1 AND key_id=$2 AND id=$3 FOR UPDATE"
    ).bind(who.tenant_id).bind(who.key_id).bind(id).fetch_optional(&mut **tx).await?.ok_or(Error::NotFound)?;
    let turn = governance::turn_locked(tx, who, row.turn_id, true).await?;
    model_allowed(tx, who, &row.model_id).await?;
    let valid:bool=sqlx::query_scalar(
        "SELECT available FROM runtime_final_publication_review_facts WHERE tenant_id=$1 AND key_id=$2 AND id=$3"
    ).bind(who.tenant_id).bind(who.key_id).bind(id).fetch_one(&mut **tx).await?;
    if !valid {
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
    {
        return Err(Error::ResultUnavailable);
    }
    Ok((row, turn, text))
}
fn output(row: &Stored, turn: &governance::Turn, text: &str) -> governance::Check {
    governance::Check {
        turn_id: row.turn_id,
        operation_key: format!("{OUTPUT_PREFIX}{}", row.id),
        stage: "post".into(),
        model_id: Some(row.model_id.clone()),
        tool_name: None,
        payload: json!({"publication_id":row.id,"source_request_ids":source_ids(row),
            "session_id":turn.session_id,"message_id":turn.message_id,"run_id":turn.run_id,
            "generation":turn.generation,"text":text}),
    }
}

/// Recognize only the exact server-owned final publication and live source facts.
/// This does not give human review access to the separately stored ciphertext.
pub(crate) async fn review_payload(
    tx: &mut Tx<'_>,
    who: &Identity,
    op: &Operation,
    turn: &governance::Turn,
    payload: &Value,
) -> Result<()> {
    let id = op
        .operation_key
        .strip_prefix(OUTPUT_PREFIX)
        .and_then(|value| Uuid::parse_str(value).ok())
        .ok_or(Error::ResultUnavailable)?;
    if op.stage != "post"
        || op.tool_name.is_some()
        || op.operation_key != format!("{OUTPUT_PREFIX}{id}")
        || op.turn_id != turn.id
    {
        return Err(Error::ResultUnavailable);
    }
    let row: Option<Stored> = sqlx::query_as(
        "SELECT id,turn_id,model_id,source_requests,text_sha256,bytes,NULL::bytea AS result_ciphertext,expires_at FROM runtime_final_publication_review_facts WHERE tenant_id=$1 AND key_id=$2 AND turn_id=$3 AND id=$4 AND available"
    ).bind(who.tenant_id).bind(op.key_id).bind(op.turn_id).bind(id).fetch_optional(&mut **tx).await?;
    let row = row.ok_or(Error::ResultUnavailable)?;
    let text = payload["text"].as_str().ok_or(Error::ResultUnavailable)?;
    let expected = output(&row, turn, text);
    if op.model_id != Some(row.model_id.clone())
        || payload != &expected.payload
        || store::hash(text.as_bytes()) != row.text_sha256
        || usize::try_from(row.bytes).ok() != Some(text.len())
    {
        return Err(Error::ResultUnavailable);
    }
    Ok(())
}

pub(crate) async fn validate_approval(
    tx: &mut Tx<'_>,
    cipher: &ResultCipher,
    who: &Identity,
    op: &Operation,
) -> Result<()> {
    let turn = governance::turn_locked(tx, who, op.turn_id, true).await?;
    model_allowed(
        tx,
        who,
        op.model_id.as_deref().ok_or(Error::ResultUnavailable)?,
    )
    .await?;
    let value: Value = serde_json::from_slice(
        &cipher.decrypt(
            who.tenant_id,
            op.id,
            op.payload_ciphertext
                .as_deref()
                .ok_or(Error::ResultUnavailable)?,
        )?,
    )
    .map_err(|_| Error::ResultUnavailable)?;
    if governance::digest(&json!({"turn":op.turn_id,"model":op.model_id,
        "tool":op.tool_name,"payload":value}))?
        != op.payload_hash
    {
        return Err(Error::ResultUnavailable);
    }
    review_payload(tx, who, op, &turn, &value).await
}

/// POST may drive paid semantic evaluation; no output execution claim is made.
pub async fn release(gateway: &Gateway, who: &Identity, id: Uuid) -> Result<Value> {
    let mut tx = gateway.pool.begin().await?;
    governance::tenant_lock(&mut tx, who.tenant_id).await?;
    let (row, turn, text) = current(&mut tx, &gateway.cipher, who, id).await?;
    let check = output(&row, &turn, &text);
    tx.commit().await?;
    let checked = gateway.check(who, check).await?;
    let output_id: Uuid =
        serde_json::from_value(checked["id"].clone()).map_err(|_| Error::Configuration)?;
    let mut tx = gateway.pool.begin().await?;
    governance::tenant_lock(&mut tx, who.tenant_id).await?;
    let (row, turn, text) = current(&mut tx, &gateway.cipher, who, id).await?;
    let check = output(&row, &turn, &text);
    let hash = governance::digest(&json!({"turn":check.turn_id,"model":check.model_id,
        "tool":check.tool_name,"payload":check.payload}))?;
    let op:Operation=sqlx::query_as(
        "SELECT * FROM governance_operations WHERE tenant_id=$1 AND key_id=$2 AND id=$3 AND operation_key=$4 AND stage='post' FOR UPDATE"
    ).bind(who.tenant_id).bind(who.key_id).bind(output_id).bind(&check.operation_key)
        .fetch_optional(&mut *tx).await?.ok_or(Error::NotFound)?;
    if op.turn_id != row.turn_id
        || op.model_id != check.model_id
        || op.tool_name.is_some()
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
    let (waiting, deadline) =
        governance::gate_with_grant_deadline(&mut tx, who, &turn, &op, &check.payload).await?;
    // Locks cannot stop clock expiry. Repeat authority, source and content checks
    // after observation/event writes and immediately before returning plaintext.
    current(&mut tx, &gateway.cipher, who, id).await?;
    governance::require_live_grants(&mut tx, deadline).await?;
    let mut response = metadata(&row);
    response["id"] = json!(op.id);
    response["publication_id"] = json!(row.id);
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
pub async fn maintain(pool: &PgPool) -> Result<u64> {
    let mut tx = pool.begin().await?;
    let rows:Vec<(Uuid,Uuid,Uuid)>=sqlx::query_as(
        "SELECT tenant_id,id,key_id FROM runtime_final_publications WHERE expires_at<=clock_timestamp() AND result_ciphertext IS NOT NULL ORDER BY expires_at,id LIMIT 100 FOR UPDATE SKIP LOCKED"
    ).fetch_all(&mut *tx).await?;
    for (tenant, id, key) in &rows {
        sqlx::query("UPDATE governance_operations SET payload_ciphertext=NULL WHERE tenant_id=$1 AND key_id=$2 AND operation_key=$3 AND stage='post'")
            .bind(tenant).bind(key).bind(format!("{OUTPUT_PREFIX}{id}")).execute(&mut *tx).await?;
        sqlx::query("UPDATE runtime_final_publications SET result_ciphertext=NULL WHERE tenant_id=$1 AND id=$2")
            .bind(tenant).bind(id).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(rows.len() as u64)
}
