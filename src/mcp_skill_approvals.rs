//! Turn-scoped human consent for a complete, directly observed static Skill.
//! A decision here never reads resources, activates a Skill, or grants tools.
use crate::{
    approval_workbench,
    cipher::ResultCipher,
    error::{Error, Result},
    governance::{self, Tx},
    human::{self, Human},
    mcp_catalog,
    store::Identity,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub request_id: Uuid,
    pub turn_id: Uuid,
    pub server_id: Uuid,
    pub uri: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Query {
    pub turn_id: Option<Uuid>,
    pub pending_only: bool,
    pub limit: i64,
    pub before_created_at: Option<DateTime<Utc>>,
    pub before_id: Option<Uuid>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Review {
    pub request_id: Uuid,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Decide {
    pub request_id: Uuid,
    pub review_id: Uuid,
    pub acknowledged_scope: bool,
    pub decision: String,
    pub feedback: Option<String>,
}

#[derive(sqlx::FromRow)]
struct Observation {
    id: Uuid,
    server_config_revision: i64,
    entry: Value,
    approval_fingerprint: String,
    requests_tool_grant: bool,
    authorization_context_hash: Option<Vec<u8>>,
}

#[derive(sqlx::FromRow)]
struct Consent {
    id: Uuid,
    turn_id: Uuid,
    user_id: Uuid,
    server_id: Uuid,
    observation_id: Uuid,
    server_config_revision: i64,
    uri: String,
    approval_fingerprint: String,
    manifest_hash: Vec<u8>,
    authorization_context_hash: Option<Vec<u8>>,
    state: String,
    decision_hash: Option<Vec<u8>>,
    feedback_ciphertext: Option<Vec<u8>>,
    expires_at: DateTime<Utc>,
}

type ReviewBinding = (Vec<u8>, String, Option<Vec<u8>>, DateTime<Utc>);

async fn begin<'a>(pool: &'a PgPool, who: &Human) -> Result<Tx<'a>> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL statement_timeout='5s'")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SET LOCAL lock_timeout='2s'")
        .execute(&mut *tx)
        .await?;
    human::authorize(&mut tx, who).await?;
    Ok(tx)
}

async fn current(tx: &mut Tx<'_>, tenant: Uuid, server: Uuid, uri: &str) -> Result<Observation> {
    // The latest direct lookup supersedes earlier observations even if it has expired.
    // Tenant locking serializes this read with catalog publication and revocation.
    let observed: Observation = sqlx::query_as(
        "SELECT o.id,o.server_config_revision,o.entry,o.approval_fingerprint,o.requests_tool_grant,o.authorization_context_hash FROM mcp_servers s JOIN LATERAL (SELECT * FROM mcp_skill_get_observations o WHERE o.tenant_id=s.tenant_id AND o.server_id=s.id AND o.server_config_revision=s.config_revision AND o.uri=$3 ORDER BY o.observed_at DESC,o.id DESC LIMIT 1) o ON true WHERE s.tenant_id=$1 AND s.id=$2 AND s.active AND o.expires_at>clock_timestamp()",
    )
    .bind(tenant)
    .bind(server)
    .bind(uri)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(Error::PolicyChanged)?;
    Ok(observed)
}

fn same_observation(request: &Consent, observed: &Observation) -> Result<()> {
    // A direct refresh may change the observation ID without changing the
    // human-reviewed bytes or the upstream authorization context.
    if request.server_config_revision != observed.server_config_revision
        || request.approval_fingerprint != observed.approval_fingerprint
        || request.manifest_hash != governance::digest(&observed.entry)?
        || request.authorization_context_hash != observed.authorization_context_hash
    {
        return Err(Error::PolicyChanged);
    }
    Ok(())
}

async fn consent(tx: &mut Tx<'_>, tenant: Uuid, id: Uuid, user: Option<Uuid>) -> Result<Consent> {
    sqlx::query_as("SELECT r.id,r.turn_id,r.user_id,r.server_id,r.observation_id,r.server_config_revision,r.uri,r.approval_fingerprint,r.manifest_hash,r.authorization_context_hash,r.state,r.decision_hash,r.feedback_ciphertext,r.expires_at FROM mcp_skill_approval_requests r WHERE r.tenant_id=$1 AND r.id=$2 AND ($3::uuid IS NULL OR r.user_id=$3) FOR UPDATE")
        .bind(tenant).bind(id).bind(user).fetch_optional(&mut **tx).await?.ok_or(Error::NotFound)
}

async fn binding(
    tx: &mut Tx<'_>,
    tenant: Uuid,
    id: Uuid,
    user: Option<Uuid>,
) -> Result<(Uuid, Uuid)> {
    sqlx::query_as("SELECT turn_id,key_id FROM mcp_skill_approval_requests WHERE tenant_id=$1 AND id=$2 AND ($3::uuid IS NULL OR user_id=$3)")
        .bind(tenant).bind(id).bind(user).fetch_optional(&mut **tx).await?.ok_or(Error::NotFound)
}

fn view(request: &Consent, state: &str, feedback: Option<String>) -> Value {
    json!({
        "request_id":request.id,"turn_id":request.turn_id,"server_id":request.server_id,
        "observation_id":request.observation_id,"server_revision":request.server_config_revision.to_string(),
        "uri":request.uri,"approval_fingerprint":request.approval_fingerprint,
        "manifest_sha256":governance::hex(&request.manifest_hash),"state":state,
        "feedback":feedback,"expires_at":request.expires_at,
        "activation":"disabled_until_verified_resource_read_and_explicit_tool_grant"
    })
}

pub async fn request(pool: &PgPool, who: &Identity, input: Request) -> Result<Value> {
    mcp_catalog::validate_skill_lookup_uri(&input.uri)?;
    let mut tx = pool.begin().await?;
    governance::tenant_lock(&mut tx, who.tenant_id).await?;
    let turn = governance::turn_locked(&mut tx, who, input.turn_id, true).await?;
    let existing: Option<(Uuid, Uuid, String)> = sqlx::query_as(
        "SELECT turn_id,server_id,uri FROM mcp_skill_approval_requests WHERE tenant_id=$1 AND id=$2",
    )
    .bind(who.tenant_id).bind(input.request_id).fetch_optional(&mut *tx).await?;
    if let Some((turn_id, server_id, uri)) = existing {
        if turn_id != input.turn_id || server_id != input.server_id || uri != input.uri {
            return Err(Error::Conflict);
        }
        let request = consent(&mut tx, who.tenant_id, input.request_id, Some(turn.user_id)).await?;
        let observed = current(&mut tx, who.tenant_id, input.server_id, &input.uri).await?;
        same_observation(&request, &observed)?;
        tx.commit().await?;
        return Ok(view(&request, &request.state, None));
    }
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM mcp_skill_approval_requests WHERE tenant_id=$1 AND turn_id=$2",
    )
    .bind(who.tenant_id)
    .bind(input.turn_id)
    .fetch_one(&mut *tx)
    .await?;
    if count >= 128 {
        return Err(Error::Busy);
    }
    let observed = current(&mut tx, who.tenant_id, input.server_id, &input.uri).await?;
    let manifest_hash = governance::digest(&observed.entry)?;
    // An observation can expire before a person responds. Re-observation of
    // exactly the same manifest is required at review and decision time.
    let expires_at = turn.expires_at;
    sqlx::query("INSERT INTO mcp_skill_approval_requests(tenant_id,id,turn_id,key_id,user_id,server_id,observation_id,server_config_revision,uri,approval_fingerprint,manifest_hash,authorization_context_hash,expires_at) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13)")
        .bind(who.tenant_id).bind(input.request_id).bind(input.turn_id).bind(who.key_id)
        .bind(turn.user_id).bind(input.server_id).bind(observed.id)
        .bind(observed.server_config_revision).bind(&input.uri).bind(&observed.approval_fingerprint)
        .bind(&manifest_hash).bind(&observed.authorization_context_hash).bind(expires_at)
        .execute(&mut *tx).await?;
    governance::event(&mut tx, &turn, "skill.approval_requested", json!({"request_id":input.request_id,"server_id":input.server_id,"uri":input.uri,"observation_id":observed.id,"manifest_sha256":governance::hex(&manifest_hash)})).await?;
    tx.commit().await?;
    Ok(
        json!({"request_id":input.request_id,"turn_id":turn.id,"server_id":input.server_id,"observation_id":observed.id,"server_revision":observed.server_config_revision.to_string(),"uri":input.uri,"approval_fingerprint":observed.approval_fingerprint,"manifest_sha256":governance::hex(&manifest_hash),"state":"pending","feedback":null,"expires_at":expires_at,"activation":"disabled_until_verified_resource_read_and_explicit_tool_grant"}),
    )
}

pub async fn status(
    pool: &PgPool,
    cipher: &ResultCipher,
    who: &Identity,
    id: Uuid,
) -> Result<Value> {
    let mut tx = pool.begin().await?;
    governance::tenant_lock(&mut tx, who.tenant_id).await?;
    let (turn_id, key_id) = binding(&mut tx, who.tenant_id, id, None).await?;
    if key_id != who.key_id {
        return Err(Error::NotFound);
    }
    let turn = governance::turn_locked(&mut tx, who, turn_id, false).await?;
    let request = consent(&mut tx, who.tenant_id, id, Some(turn.user_id)).await?;
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut *tx)
        .await?;
    let mut state = request.state.as_str();
    if matches!(request.state.as_str(), "pending" | "approved")
        && (turn.state != "active" || turn.expires_at <= now || request.expires_at <= now)
    {
        state = "expired";
    } else if matches!(request.state.as_str(), "pending" | "approved") {
        match current(&mut tx, who.tenant_id, request.server_id, &request.uri).await {
            Ok(observed) if same_observation(&request, &observed).is_ok() => {}
            _ => state = "stale",
        }
    }
    let feedback = if request.state == "feedback" {
        request
            .feedback_ciphertext
            .as_ref()
            .map(|bytes| {
                let plain = cipher.decrypt(who.tenant_id, id, bytes)?;
                serde_json::from_slice::<String>(&plain).map_err(|_| Error::ResultUnavailable)
            })
            .transpose()?
    } else {
        None
    };
    tx.commit().await?;
    Ok(view(&request, state, feedback))
}

pub async fn query(pool: &PgPool, who: &Human, q: Query) -> Result<Value> {
    if !(1..=50).contains(&q.limit) || q.before_created_at.is_some() != q.before_id.is_some() {
        return Err(Error::Invalid("invalid Skill approval pagination"));
    }
    let mut tx = begin(pool, who).await?;
    let rows: Vec<(Value,DateTime<Utc>,Uuid)> = sqlx::query_as(
        "SELECT jsonb_build_object('request_id',r.id,'turn_id',r.turn_id,'server_id',r.server_id,'uri',r.uri,'state',CASE WHEN r.state='pending' AND (t.state<>'active' OR t.expires_at<=clock_timestamp() OR r.expires_at<=clock_timestamp()) THEN 'expired' ELSE r.state END,'expires_at',r.expires_at,'created_at',r.created_at),r.created_at,r.id FROM mcp_skill_approval_requests r JOIN governance_turns t ON t.tenant_id=r.tenant_id AND t.id=r.turn_id WHERE r.tenant_id=$1 AND r.user_id=$2 AND ($3::uuid IS NULL OR r.turn_id=$3) AND (NOT $4 OR (r.state='pending' AND t.state='active' AND t.expires_at>clock_timestamp() AND r.expires_at>clock_timestamp())) AND ($5::timestamptz IS NULL OR (r.created_at,r.id)<($5,$6)) ORDER BY r.created_at DESC,r.id DESC LIMIT $7",
    ).bind(who.tenant_id).bind(who.id).bind(q.turn_id).bind(q.pending_only)
        .bind(q.before_created_at).bind(q.before_id).bind(q.limit+1).fetch_all(&mut *tx).await?;
    let next = if rows.len() > q.limit as usize {
        let (_, created_at, id) = &rows[q.limit as usize - 1];
        json!({"before_created_at":created_at,"before_id":id})
    } else {
        Value::Null
    };
    approval_workbench::fresh(&mut tx, who).await?;
    tx.commit().await?;
    Ok(
        json!({"data":rows.into_iter().take(q.limit as usize).map(|r|r.0).collect::<Vec<_>>(),"next_after":next}),
    )
}

pub async fn review(pool: &PgPool, who: &Human, input: Review) -> Result<Value> {
    let mut tx = begin(pool, who).await?;
    let (turn_id, key_id) = binding(&mut tx, who.tenant_id, input.request_id, Some(who.id)).await?;
    let identity = Identity {
        tenant_id: who.tenant_id,
        key_id,
    };
    let turn = governance::turn_locked(&mut tx, &identity, turn_id, true).await?;
    let request = consent(&mut tx, who.tenant_id, input.request_id, Some(who.id)).await?;
    if request.user_id != turn.user_id {
        return Err(Error::NotFound);
    }
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut *tx)
        .await?;
    if request.state != "pending" || request.expires_at <= now {
        return Err(Error::ActionRejected);
    }
    let observed = current(&mut tx, who.tenant_id, request.server_id, &request.uri).await?;
    same_observation(&request, &observed)?;
    let review_id = Uuid::new_v4();
    let expires_at = request.expires_at.min(now + chrono::Duration::minutes(5));
    sqlx::query("INSERT INTO mcp_skill_approval_reviews(tenant_id,id,request_id,user_id,manifest_hash,approval_fingerprint,authorization_context_hash,created_at,expires_at) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9)")
        .bind(who.tenant_id).bind(review_id).bind(request.id).bind(who.id)
        .bind(&request.manifest_hash).bind(&request.approval_fingerprint)
        .bind(&request.authorization_context_hash).bind(now).bind(expires_at)
        .execute(&mut *tx).await?;
    sqlx::query("INSERT INTO audit_events(tenant_id,action,object_id,details) VALUES($1,'mcp.skill_approval_reviewed',$2,$3)")
        .bind(who.tenant_id).bind(request.id.to_string())
        .bind(json!({"user_id":who.id,"review_id":review_id,"observation_id":observed.id,"manifest_sha256":governance::hex(&request.manifest_hash)}))
        .execute(&mut *tx).await?;
    approval_workbench::fresh(&mut tx, who).await?;
    tx.commit().await?;
    Ok(
        json!({"request":view(&request,"pending",None),"review_id":review_id,"review_expires_at":expires_at,"entry":observed.entry,"requests_tool_grant":observed.requests_tool_grant,"scope":"this exact manifest in this turn only; no file read, activation or tool grant"}),
    )
}

pub async fn decide(
    pool: &PgPool,
    cipher: &ResultCipher,
    who: &Human,
    input: Decide,
) -> Result<Value> {
    if !input.acknowledged_scope
        || !matches!(
            input.decision.as_str(),
            "approve_this_turn" | "reject" | "feedback"
        )
        || (input.decision == "feedback"
            && input
                .feedback
                .as_deref()
                .is_none_or(|s| !governance::valid_text(s, 4000)))
        || (input.decision != "feedback" && input.feedback.is_some())
    {
        return Err(Error::Invalid("invalid explicit Skill decision"));
    }
    let hash = governance::digest(
        &serde_json::to_value(&input).map_err(|_| Error::Invalid("invalid Skill decision"))?,
    )?;
    let mut tx = begin(pool, who).await?;
    let (turn_id, key_id) = binding(&mut tx, who.tenant_id, input.request_id, Some(who.id)).await?;
    let identity = Identity {
        tenant_id: who.tenant_id,
        key_id,
    };
    let turn = governance::turn_locked(&mut tx, &identity, turn_id, false).await?;
    let request = consent(&mut tx, who.tenant_id, input.request_id, Some(who.id)).await?;
    if request.user_id != turn.user_id {
        return Err(Error::NotFound);
    }
    if request.state != "pending" {
        if request.decision_hash.as_ref() != Some(&hash) {
            return Err(Error::Conflict);
        }
        tx.commit().await?;
        return Ok(view(&request, &request.state, None));
    }
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut *tx)
        .await?;
    if turn.state != "active" || turn.expires_at <= now || request.expires_at <= now {
        return Err(Error::TurnClosed);
    }
    let review: Option<ReviewBinding> = sqlx::query_as(
        "SELECT manifest_hash,approval_fingerprint,authorization_context_hash,expires_at FROM mcp_skill_approval_reviews WHERE tenant_id=$1 AND id=$2 AND request_id=$3 AND user_id=$4",
    ).bind(who.tenant_id).bind(input.review_id).bind(request.id).bind(who.id)
        .fetch_optional(&mut *tx).await?;
    let (manifest_hash, fingerprint, auth_hash, review_expires) = review.ok_or(Error::NotFound)?;
    if review_expires <= now
        || manifest_hash != request.manifest_hash
        || fingerprint != request.approval_fingerprint
        || auth_hash != request.authorization_context_hash
    {
        return Err(Error::PolicyChanged);
    }
    let observed = current(&mut tx, who.tenant_id, request.server_id, &request.uri).await?;
    same_observation(&request, &observed)?;
    let state = match input.decision.as_str() {
        "approve_this_turn" => "approved",
        "reject" => "rejected",
        _ => "feedback",
    };
    let encrypted = input
        .feedback
        .as_ref()
        .map(|feedback| {
            cipher.encrypt(
                who.tenant_id,
                request.id,
                &serde_json::to_vec(feedback).map_err(|_| Error::Configuration)?,
            )
        })
        .transpose()?;
    sqlx::query("UPDATE mcp_skill_approval_requests SET state=$3,decision_hash=$4,decided_by=$5,feedback_ciphertext=$6,decided_at=clock_timestamp() WHERE tenant_id=$1 AND id=$2 AND state='pending'")
        .bind(who.tenant_id).bind(request.id).bind(state).bind(&hash).bind(who.id).bind(encrypted)
        .execute(&mut *tx).await?;
    governance::event(&mut tx, &turn, "skill.approval_decided", json!({"request_id":request.id,"server_id":request.server_id,"observation_id":request.observation_id,"manifest_sha256":governance::hex(&request.manifest_hash),"state":state})).await?;
    if state == "feedback" {
        sqlx::query("UPDATE governance_turns SET state='cancelled' WHERE tenant_id=$1 AND id=$2")
            .bind(who.tenant_id)
            .bind(turn.id)
            .execute(&mut *tx)
            .await?;
        governance::event(
            &mut tx,
            &turn,
            "turn.closed",
            json!({"state":"cancelled","reason":"skill_feedback_requires_new_turn"}),
        )
        .await?;
    }
    sqlx::query("INSERT INTO audit_events(tenant_id,action,object_id,details) VALUES($1,'mcp.skill_approval_decided',$2,$3)")
        .bind(who.tenant_id).bind(request.id.to_string())
        .bind(json!({"user_id":who.id,"review_id":input.review_id,"turn_id":turn.id,"state":state,"manifest_sha256":governance::hex(&request.manifest_hash)}))
        .execute(&mut *tx).await?;
    approval_workbench::fresh(&mut tx, who).await?;
    tx.commit().await?;
    Ok(view(&request, state, input.feedback))
}
