//! Human-owned inbox and explicit, audited JSON review. No agent execution authority.
use crate::{
    cipher::ResultCipher,
    error::{Error, Result},
    governance::{self, Approval, Operation, Tx},
    human::{self, Human},
    store::Identity,
};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cursor {
    pub created_at: DateTime<Utc>,
    pub id: Uuid,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Query {
    pub after: Option<Cursor>,
    pub turn_id: Option<Uuid>,
    pub pending_only: bool,
    pub limit: i64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Review {
    pub approval_id: Uuid,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Decide {
    pub approval_id: Uuid,
    pub review_id: Uuid,
    pub acknowledged_scope: bool,
    pub decision: human::Decision,
}

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
pub(crate) async fn fresh(tx: &mut Tx<'_>, who: &Human) -> Result<()> {
    // Tenant lock serializes revocation; check proof deadline again after any lock wait/work.
    let live: bool =
        sqlx::query_scalar("SELECT clock_timestamp()<to_timestamp($1::double precision)")
            .bind(who.expires_at() as f64)
            .fetch_one(&mut **tx)
            .await?;
    if !live {
        return Err(Error::HumanUnauthorized);
    }
    Ok(())
}
pub async fn query(pool: &PgPool, who: &Human, q: Query) -> Result<Value> {
    if !(1..=50).contains(&q.limit) {
        return Err(Error::Invalid("limit must be 1..50"));
    }
    let mut tx = begin(pool, who).await?;
    // Never select payload/feedback/rule/summary JSON in the inbox.
    let rows: Vec<(Value,DateTime<Utc>,Uuid)> = sqlx::query_as(
        "SELECT jsonb_build_object('id',a.id,'operation_id',o.id,'turn_id',t.id,'stage',o.stage,'model_id',o.model_id,'tool_name',o.tool_name,'state',CASE WHEN a.state='pending' AND (t.state<>'active' OR t.expires_at<=clock_timestamp() OR a.expires_at<=clock_timestamp()) THEN 'expired' ELSE a.state END,'operation_state',CASE WHEN o.state='executing' AND o.execution_deadline<=clock_timestamp() THEN 'execution_unknown' ELSE o.state END,'expires_at',a.expires_at,'created_at',a.created_at),a.created_at,a.id FROM approvals a JOIN governance_turns t ON t.tenant_id=a.tenant_id AND t.id=a.turn_id JOIN governance_operations o ON o.tenant_id=a.tenant_id AND o.id=a.operation_id WHERE a.tenant_id=$1 AND t.user_id=$2 AND ($3::uuid IS NULL OR t.id=$3) AND (NOT $4 OR (a.state='pending' AND t.state='active' AND t.expires_at>clock_timestamp() AND a.expires_at>clock_timestamp())) AND ($5::timestamptz IS NULL OR (a.created_at,a.id)<($5,$6)) ORDER BY a.created_at DESC,a.id DESC LIMIT $7")
        .bind(who.tenant_id).bind(who.id).bind(q.turn_id).bind(q.pending_only)
        .bind(q.after.as_ref().map(|c| c.created_at)).bind(q.after.as_ref().map(|c| c.id))
        .bind(q.limit+1).fetch_all(&mut *tx).await?;
    let next = if rows.len() > q.limit as usize {
        let (_, created_at, id) = &rows[q.limit as usize - 1];
        json!({"created_at":created_at,"id":id})
    } else {
        Value::Null
    };
    fresh(&mut tx, who).await?;
    tx.commit().await?;
    Ok(
        json!({"data":rows.into_iter().take(q.limit as usize).map(|r|r.0).collect::<Vec<_>>(),"next_after":next}),
    )
}

// A JSON operation review is not a binary media review. Text chat is recognized by structure;
// tool JSON authorizes precisely the displayed invocation/result, never linked asset contents.
fn reviewable(stage: &str, payload: &Value) -> bool {
    let text_message = |m: &Value| {
        m.as_object().is_some_and(|o| {
            o.keys().all(|k| {
                matches!(
                    k.as_str(),
                    "role"
                        | "content"
                        | "name"
                        | "tool_calls"
                        | "tool_call_id"
                        | "refusal"
                        | "reasoning_content"
                )
            }) && m.get("content").map_or_else(
                || m.get("tool_calls").is_some_and(Value::is_array),
                |v| v.is_null() || v.is_string(),
            )
        })
    };
    match stage {
        "tool_input" | "tool_output" => true,
        "pre" => payload
            .get("messages")
            .and_then(Value::as_array)
            .is_some_and(|m| !m.is_empty() && m.iter().all(text_message)),
        "post" => payload
            .get("choices")
            .and_then(Value::as_array)
            .is_some_and(|c| !c.is_empty() && c.iter().all(|c| text_message(&c["message"]))),
        _ => false,
    }
}
pub async fn review(pool: &PgPool, cipher: &ResultCipher, who: &Human, q: Review) -> Result<Value> {
    let mut tx = begin(pool, who).await?;
    let a: Approval=sqlx::query_as("SELECT a.* FROM approvals a JOIN governance_turns t ON t.tenant_id=a.tenant_id AND t.id=a.turn_id WHERE a.tenant_id=$1 AND a.id=$2 AND t.user_id=$3 FOR UPDATE OF a")
        .bind(who.tenant_id).bind(q.approval_id).bind(who.id).fetch_optional(&mut *tx).await?.ok_or(Error::NotFound)?;
    let op: Operation = sqlx::query_as(
        "SELECT * FROM governance_operations WHERE tenant_id=$1 AND id=$2 FOR UPDATE",
    )
    .bind(who.tenant_id)
    .bind(a.operation_id)
    .fetch_one(&mut *tx)
    .await?;
    let identity = Identity {
        tenant_id: who.tenant_id,
        key_id: op.key_id,
    };
    let turn = governance::turn_locked(&mut tx, &identity, a.turn_id, true).await?;
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut *tx)
        .await?;
    if a.state != "pending"
        || a.expires_at <= now
        || !matches!(op.state.as_str(), "ready" | "checking" | "waiting_approval")
    {
        return Err(Error::ActionRejected);
    }
    let current = governance::selected(
        &mut tx,
        &identity,
        &op.stage,
        op.model_id.as_deref(),
        op.tool_name.as_deref(),
    )
    .await?;
    if serde_json::to_value(current).map_err(|_| Error::Configuration)? != a.policy_snapshot {
        return Err(Error::PolicyChanged);
    }
    let mut payload = Value::Null;
    let mut can_continue = false;
    let mut unavailable = "binary_review_not_supported";
    if !matches!(op.stage.as_str(), "artifact" | "media_output") {
        unavailable = "content_unavailable";
        if let Some(bytes) = &op.payload_ciphertext {
            let plain = cipher.decrypt(who.tenant_id, op.id, bytes)?;
            // No truncation: the whole reviewed content must fit the review contract.
            if plain.len() <= 131072 {
                let value: Value =
                    serde_json::from_slice(&plain).map_err(|_| Error::ResultUnavailable)?;
                if governance::digest(
                    &json!({"turn":op.turn_id,"model":op.model_id,"tool":op.tool_name,"payload":value}),
                )? != op.payload_hash
                {
                    return Err(Error::ResultUnavailable);
                }
                can_continue = if op
                    .operation_key
                    .starts_with(crate::runtime_publications::OUTPUT_PREFIX)
                {
                    crate::runtime_publications::review_payload(
                        &mut tx, &identity, &op, &turn, &value,
                    )
                    .await?;
                    true
                } else {
                    reviewable(&op.stage, &value)
                };
                if can_continue {
                    payload = Value::String(
                        String::from_utf8(plain).map_err(|_| Error::ResultUnavailable)?,
                    );
                    unavailable = "";
                } else {
                    unavailable = "binary_review_not_supported";
                }
            } else {
                unavailable = "content_too_large";
            }
        }
    }
    let id = Uuid::new_v4();
    let expires_at = a
        .expires_at
        .min(turn.expires_at)
        .min(now + chrono::Duration::minutes(5));
    sqlx::query("INSERT INTO approval_reviews(tenant_id,id,approval_id,user_id,payload_hash,policy_hash,can_continue,created_at,expires_at) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9)")
        .bind(who.tenant_id).bind(id).bind(a.id).bind(who.id).bind(&op.payload_hash)
        .bind(governance::digest(&a.policy_snapshot)?).bind(can_continue).bind(now).bind(expires_at).execute(&mut *tx).await?;
    sqlx::query("INSERT INTO audit_events(tenant_id,action,object_id,details) VALUES($1,'approval.reviewed',$2,$3)")
        .bind(who.tenant_id).bind(a.id.to_string()).bind(json!({"user_id":who.id,"review_id":id,"operation_id":op.id,"operation_sha256":governance::hex(&op.payload_hash),"content_delivered":can_continue,"expires_at":expires_at})).execute(&mut *tx).await?;
    fresh(&mut tx, who).await?;
    tx.commit().await?;
    Ok(
        json!({"approval":governance::approval_view(&a),"review_id":id,"review_expires_at":expires_at,"stage":op.stage,"model_id":op.model_id,"tool_name":op.tool_name,"operation_state":op.state,"payload_text":payload,"operation_sha256":governance::hex(&op.payload_hash),"can_continue":can_continue,"unavailable_reason":unavailable}),
    )
}
pub async fn decide(pool: &PgPool, cipher: &ResultCipher, who: &Human, q: Decide) -> Result<Value> {
    if !q.acknowledged_scope {
        return Err(Error::Invalid("explicit scope acknowledgement required"));
    }
    human::resolve_reviewed(pool, cipher, who, q.approval_id, q.decision, q.review_id).await
}
pub(crate) async fn validate_review(
    tx: &mut Tx<'_>,
    who: &Human,
    id: Uuid,
    a: &Approval,
    op: &Operation,
    decision: &human::Decision,
) -> Result<()> {
    if op.stage == "media_output" && decision.decision == "continue_this_turn" {
        let completed:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM media_review_completions c JOIN media_review_sessions s ON s.tenant_id=c.tenant_id AND s.id=c.review_id WHERE c.tenant_id=$1 AND c.review_id=$2 AND s.approval_id=$3 AND s.user_id=$4 AND s.payload_hash=$5 AND s.policy_hash=$6)")
            .bind(who.tenant_id).bind(id).bind(a.id).bind(who.id).bind(&op.payload_hash).bind(governance::digest(&a.policy_snapshot)?).fetch_one(&mut **tx).await?;
        if !completed {
            return Err(Error::Invalid(
                "binary media decisions require a completed isolated review",
            ));
        }
    }
    type StoredReview = (Vec<u8>, Vec<u8>, bool, DateTime<Utc>);
    let row:Option<StoredReview>=sqlx::query_as("SELECT payload_hash,policy_hash,can_continue,expires_at FROM approval_reviews WHERE tenant_id=$1 AND id=$2 AND approval_id=$3 AND user_id=$4")
        .bind(who.tenant_id).bind(id).bind(a.id).bind(who.id).fetch_optional(&mut **tx).await?;
    let (payload, policy, can_continue, expires) = row.ok_or(Error::NotFound)?;
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut **tx)
        .await?;
    if expires <= now
        || payload != op.payload_hash
        || policy != governance::digest(&a.policy_snapshot)?
    {
        return Err(Error::PolicyChanged);
    }
    if decision.decision == "continue_this_turn"
        && (!can_continue || (op.stage != "media_output" && op.payload_ciphertext.is_none()))
    {
        return Err(Error::ResultUnavailable);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn binary_references_are_not_a_text_chat_review() {
        assert!(reviewable("pre", &json!({"messages":[{"content":"text"}]})));
        assert!(!reviewable(
            "pre",
            &json!({"messages":[{"content":[{"type":"image_url"}]}]})
        ));
        assert!(!reviewable(
            "pre",
            &json!({"messages":[{"content":[{"type":"image_url"}],"tool_calls":[]}]})
        ));
        assert!(!reviewable(
            "artifact",
            &json!({"text":"not binary evidence"})
        ));
        assert!(!reviewable(
            "media_output",
            &json!({"asset_id":Uuid::new_v4()})
        ));
        assert!(!reviewable(
            "post",
            &json!({"choices":[{"message":{"content":null,"audio":{"id":"unreviewed"}}}]})
        ));
    }
}
