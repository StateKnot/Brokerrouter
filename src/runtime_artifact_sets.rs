//! One immutable text result binds its complete original native artifact set.
//! Source identity comes from database facts; current authority comes from B2.
//! The caller holds the tenant lock and commits text/file release atomically.
use crate::{
    error::{Error, Result},
    governance::{self, Tx},
    runtime_artifact_delivery,
    store::Identity,
};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashSet;
use uuid::Uuid;

const MAX_FILES: usize = 64;
const MAX_BATCH_FILES: usize = 8;
const MAX_BATCH_BYTES: i64 = 64 * 1024 * 1024;

pub(crate) struct Source {
    pub(crate) set: Option<Value>,
    pub(crate) deadline: Option<DateTime<Utc>>,
    pub(crate) result_ids: Vec<Uuid>,
}

#[derive(sqlx::FromRow)]
struct Fact {
    key_id: Uuid,
    input_operation_id: Uuid,
    batch_id: Uuid,
    model_id: Option<String>,
    state: String,
    files: Value,
    source_matches: bool,
    files_match: bool,
    complete: bool,
    available: bool,
    source_deadline: DateTime<Utc>,
    result_id: Option<Uuid>,
    result_matches: bool,
    result_available: bool,
    result_deadline: Option<DateTime<Utc>>,
}

// Deliberately flat. This exact representation is also emitted/digested by SQL.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct File {
    asset_id: Uuid,
    batch_id: Uuid,
    input_operation_id: Uuid,
    ordinal: i32,
    sha256: String,
    byte_length: i64,
    producer_revision: String,
    manifest_sha256: String,
    expires_at: String,
}
impl File {
    fn deadline(&self) -> Result<DateTime<Utc>> {
        let date = DateTime::parse_from_rfc3339(&self.expires_at)
            .map_err(|_| Error::Conflict)?
            .with_timezone(&Utc);
        if date.to_rfc3339_opts(SecondsFormat::Micros, true) != self.expires_at {
            return Err(Error::Conflict);
        }
        Ok(date)
    }
}

fn files(set: &Value) -> Result<Vec<File>> {
    let items = set.as_array().ok_or(Error::Conflict)?;
    if items.is_empty() || items.len() > MAX_FILES {
        return Err(Error::Invalid("unsupported native artifact set size"));
    }
    let parsed: Vec<File> = serde_json::from_value(set.clone()).map_err(|_| Error::Conflict)?;
    // Uuid deserialization also accepts uppercase/alternate forms, and JSONB
    // equality accepts 1.0 as 1. Reject both before hashing a frozen wire value.
    let canonical = serde_json::to_value(&parsed).map_err(|_| Error::Configuration)?;
    if governance::digest(&canonical)? != governance::digest(set)? {
        return Err(Error::Conflict);
    }
    let mut assets = HashSet::new();
    let mut batch_ordinals = HashSet::new();
    let mut previous: Option<(Uuid, i32)> = None;
    for f in &parsed {
        if f.ordinal < 0
            || f.ordinal >= MAX_BATCH_FILES as i32
            || f.byte_length < 1
            || f.byte_length > MAX_BATCH_BYTES
            || !hash_hex(&f.sha256)
            || !hash_hex(&f.producer_revision)
            || !hash_hex(&f.manifest_sha256)
            || !assets.insert(f.asset_id)
            || !batch_ordinals.insert((f.batch_id, f.ordinal))
            || previous.is_some_and(|old| old >= (f.input_operation_id, f.ordinal))
        {
            return Err(Error::Conflict);
        }
        f.deadline()?;
        previous = Some((f.input_operation_id, f.ordinal));
    }
    Ok(parsed)
}

fn hash_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn verify_batch(fact: &Fact, require_live: bool) -> Result<()> {
    match fact.state.as_str() {
        "reserved" | "capturing" => return Err(Error::InProgress),
        "quarantined" => {}
        _ => return Err(Error::ResultUnavailable),
    }
    if !fact.source_matches || !fact.files_match || !fact.complete {
        return Err(Error::ResultUnavailable);
    }
    if require_live && !fact.available {
        return Err(Error::ResultUnavailable);
    }
    let items = files(&fact.files)?;
    if items.len() > MAX_BATCH_FILES {
        return Err(Error::Invalid("unsupported native artifact batch size"));
    }
    let mut bytes = 0_i64;
    for (ordinal, item) in items.iter().enumerate() {
        if item.batch_id != fact.batch_id
            || item.input_operation_id != fact.input_operation_id
            || item.ordinal != ordinal as i32
            || item.deadline()? < fact.source_deadline
        {
            return Err(Error::Conflict);
        }
        bytes = bytes.checked_add(item.byte_length).ok_or(Error::Conflict)?;
    }
    if bytes > MAX_BATCH_BYTES {
        return Err(Error::Invalid("unsupported native artifact batch size"));
    }
    Ok(())
}

/// Absent means genuinely no reservation; pending/expired/broken reservations
/// remain visible and cannot silently become a text-only result.
pub(crate) async fn derive_operation(
    tx: &mut Tx<'_>,
    who: &Identity,
    input: Uuid,
    require_live: bool,
) -> Result<Source> {
    let fact: Option<Fact> = sqlx::query_as(
        "SELECT key_id,input_operation_id,batch_id,model_id,state,files,source_matches,files_match,complete,available,source_deadline,result_id,result_matches,result_available,result_deadline FROM runtime_artifact_source_facts WHERE tenant_id=$1 AND input_operation_id=$2",
    ).bind(who.tenant_id).bind(input).fetch_optional(&mut **tx).await?;
    let Some(fact) = fact else {
        return Ok(Source {
            set: None,
            deadline: None,
            result_ids: Vec::new(),
        });
    };
    if fact.key_id != who.key_id {
        return Err(Error::Conflict);
    }
    verify_batch(&fact, require_live)?;
    if require_live {
        require_live_deadline(tx, Some(fact.source_deadline)).await?;
    }
    Ok(Source {
        set: Some(fact.files),
        deadline: Some(fact.source_deadline),
        result_ids: fact.result_id.into_iter().collect(),
    })
}

/// Query every original batch in the turn first, then compare model identity.
/// Filtering by model/state would allow a caller to omit a different/NULL model
/// or an incomplete reservation when freezing a final assistant publication.
pub(crate) async fn derive_turn(
    tx: &mut Tx<'_>,
    who: &Identity,
    turn: Uuid,
    model: &str,
    require_live: bool,
) -> Result<Source> {
    let facts: Vec<Fact> = sqlx::query_as(
        "SELECT key_id,input_operation_id,batch_id,model_id,state,files,source_matches,files_match,complete,available,source_deadline,result_id,result_matches,result_available,result_deadline FROM runtime_artifact_source_facts WHERE tenant_id=$1 AND turn_id=$2 ORDER BY input_operation_id LIMIT 65",
    ).bind(who.tenant_id).bind(turn).fetch_all(&mut **tx).await?;
    if facts.is_empty() {
        return Ok(Source {
            set: None,
            deadline: None,
            result_ids: Vec::new(),
        });
    }
    // Even a one-file batch consumes one slot. The 65th batch is an explicit
    // unsupported whole set, never a truncated successful publication.
    if facts.len() > MAX_FILES {
        return Err(Error::Invalid("unsupported native artifact set size"));
    }
    let mut all = Vec::new();
    let mut deadline: Option<DateTime<Utc>> = None;
    let mut result_ids = Vec::with_capacity(facts.len());
    for fact in facts {
        verify_batch(&fact, require_live)?;
        if fact.key_id != who.key_id || fact.model_id.as_deref() != Some(model) {
            return Err(Error::Conflict);
        }
        let result_id = fact.result_id.ok_or(Error::InProgress)?;
        if !fact.result_matches || (require_live && !fact.result_available) {
            return Err(Error::ResultUnavailable);
        }
        result_ids.push(result_id);
        let expires = fact
            .source_deadline
            .min(fact.result_deadline.ok_or(Error::ResultUnavailable)?);
        deadline = Some(deadline.map_or(expires, |old| old.min(expires)));
        all.extend(
            fact.files
                .as_array()
                .ok_or(Error::Conflict)?
                .iter()
                .cloned(),
        );
        if all.len() > MAX_FILES {
            return Err(Error::Invalid("unsupported native artifact set size"));
        }
    }
    let set = Value::Array(all);
    files(&set)?;
    if require_live {
        require_live_deadline(tx, deadline).await?;
    }
    Ok(Source {
        set: Some(set),
        deadline,
        result_ids,
    })
}

pub(crate) fn set_hash(set: Option<&Value>) -> Result<Option<Vec<u8>>> {
    set.map(|value| {
        files(value)?;
        governance::digest(value)
    })
    .transpose()
}

/// Public wire wrapper; database/policy canonical source remains the raw array.
/// This formats previously required immutable facts; it does not authorize them.
pub(crate) fn view(set: Option<&Value>, hash: Option<&[u8]>) -> Option<Value> {
    set.zip(hash).map(|(files, hash)| {
        json!({"contract":"runtime-artifact-set-v1",
        "files":files,"sha256":governance::hex(hash)})
    })
}

/// Use on every record retry and current/read-only review, including NULL old
/// rows. Legacy NULL results with an existing batch fail closed, never backfill.
pub(crate) fn require_set(source: &Source, set: Option<&Value>, hash: Option<&[u8]>) -> Result<()> {
    let expected = set_hash(source.set.as_ref())?;
    let supplied = set_hash(set)?;
    if source.set.as_ref() != set || expected != supplied || expected.as_deref() != hash {
        return Err(Error::Conflict);
    }
    Ok(())
}

/// Register/check every current file gate before any text/model-judge path.
/// A pending return contains metadata only; a scan/grant never authorizes text.
pub(crate) async fn gate_all(
    tx: &mut Tx<'_>,
    who: &Identity,
    set: Option<&Value>,
) -> Result<Option<Value>> {
    let Some(set) = set else {
        return Ok(None);
    };
    let items = files(set)?;
    let mut pending = Vec::new();
    let mut deadline: Option<DateTime<Utc>> = None;
    for f in items {
        let expires = f.deadline()?;
        deadline = Some(deadline.map_or(expires, |old| old.min(expires)));
        let result = runtime_artifact_delivery::release_in_tx(tx, who, f.asset_id).await?;
        match result["state"].as_str() {
            Some("ready") => {}
            Some("checking_scan") => {
                pending.push(result);
            }
            Some("waiting_approval") => {
                pending.push(result);
            }
            _ => return Err(Error::Configuration),
        }
    }
    require_live_deadline(tx, deadline).await?;
    if pending.is_empty() {
        // Recheck all used grants together before the caller is permitted to
        // register or drive a text classifier. Individual Ready responses can
        // otherwise outlive the first file's grant while later gates run.
        authorize_all(tx, who, Some(set)).await?;
        Ok(None)
    } else {
        Ok(Some(json!({"state":"waiting_artifacts",
            "artifacts":pending,"artifact_set_sha256":governance::hex(&governance::digest(set)?)})))
    }
}

/// Read-only authority for current release and Human review/decision. Reuse B2
/// exact-file policy/profile/report/grants; never call model judges here.
/// Caller rederives/requires the immutable set first, and repeats this after
/// any blocking audit then checks the returned deadline with its text grants.
pub(crate) async fn authorize_all(
    tx: &mut Tx<'_>,
    who: &Identity,
    set: Option<&Value>,
) -> Result<Option<DateTime<Utc>>> {
    let Some(set) = set else {
        return Ok(None);
    };
    let mut deadline: Option<DateTime<Utc>> = None;
    for f in files(set)? {
        let (e, grant_deadline) = runtime_artifact_delivery::released(tx, who, f.asset_id).await?;
        let view = e.view();
        let frozen = f.deadline()?;
        if e.asset_id != f.asset_id
            || governance::hex(&e.sha256) != f.sha256
            || e.byte_length != f.byte_length
            || e.expires_at > frozen
            || view["batch_id"] != f.batch_id.to_string()
            || view["input_operation_id"] != f.input_operation_id.to_string()
            || view["ordinal"] != f.ordinal
            || view["producer_revision"] != f.producer_revision
        {
            return Err(Error::Conflict);
        }
        // B2's deadline includes current scan + asset + batch + turn. Preserve
        // the immutable original expiry too, and every exact grant actually used.
        let mut expires = e.expires_at.min(frozen);
        if let Some(grant) = grant_deadline {
            expires = expires.min(grant);
        }
        deadline = Some(deadline.map_or(expires, |old| old.min(expires)));
    }
    // The first file/grant can expire while a later file is being inspected.
    require_live_deadline(tx, deadline).await?;
    Ok(deadline)
}

async fn require_live_deadline(tx: &mut Tx<'_>, deadline: Option<DateTime<Utc>>) -> Result<()> {
    let Some(deadline) = deadline else {
        return Ok(());
    };
    let live: bool = sqlx::query_scalar("SELECT clock_timestamp()<$1")
        .bind(deadline)
        .fetch_one(&mut **tx)
        .await?;
    if !live {
        return Err(Error::ResultUnavailable);
    }
    Ok(())
}
