//! Read-only MP4 review channel. Complete delivery is not a human decision or execution.
use crate::{
    approval_workbench::fresh,
    error::{Error, Result},
    governance::{self, Approval, Tx},
    human::Human,
    output_delivery::{self, Delivery, Evidence},
    store::{self, Identity},
};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Start {
    pub approval_id: Uuid,
    pub session_id: Uuid,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Chunk {
    pub session_id: Uuid,
    pub ordinal: i32,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Complete {
    pub session_id: Uuid,
    pub sha256: String,
    pub reviewed: bool,
}
#[derive(sqlx::FromRow)]
struct Session {
    id: Uuid,
    approval_id: Uuid,
    payload_hash: Vec<u8>,
    policy_hash: Vec<u8>,
    sha256: Vec<u8>,
    byte_length: i64,
    chunk_count: i32,
    created_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
}
async fn still_live(tx: &mut Tx<'_>, who: &Human, s: &Session) -> Result<()> {
    if !sqlx::query_scalar::<_, bool>("SELECT clock_timestamp()<$1")
        .bind(s.expires_at)
        .fetch_one(&mut **tx)
        .await?
    {
        return Err(Error::ResultUnavailable);
    }
    fresh(tx, who).await
}
async fn begin(d: &Delivery) -> Result<Tx<'_>> {
    let mut tx = d.assets.pool.begin().await?;
    sqlx::query("SET LOCAL statement_timeout='5s'")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SET LOCAL lock_timeout='2s'")
        .execute(&mut *tx)
        .await?;
    Ok(tx)
}
async fn approval(tx: &mut Tx<'_>, who: &Human, id: Uuid) -> Result<Approval> {
    sqlx::query_as("SELECT * FROM approvals WHERE tenant_id=$1 AND id=$2")
        .bind(who.tenant_id)
        .bind(id)
        .fetch_one(&mut **tx)
        .await
        .map_err(Into::into)
}
async fn session(
    tx: &mut Tx<'_>,
    who: &Human,
    id: Uuid,
) -> Result<(Session, Identity, Evidence, Approval)> {
    crate::human::authorize(tx, who).await?;
    let s:Session=sqlx::query_as("SELECT * FROM media_review_sessions WHERE tenant_id=$1 AND id=$2 AND user_id=$3 AND expires_at>clock_timestamp()")
        .bind(who.tenant_id).bind(id).bind(who.id).fetch_optional(&mut **tx).await?.ok_or(Error::ResultUnavailable)?;
    let (identity, e) = output_delivery::review(tx, who, s.approval_id).await?;
    let a = approval(tx, who, s.approval_id).await?;
    if governance::digest(&e.view())? != s.payload_hash
        || governance::digest(&a.policy_snapshot)? != s.policy_hash
        || e.sha256 != s.sha256
        || e.byte_length != s.byte_length
        || e.chunk_count != s.chunk_count
    {
        return Err(Error::PolicyChanged);
    }
    still_live(tx, who, &s).await?;
    Ok((s, identity, e, a))
}
pub async fn start(d: &Delivery, who: &Human, q: Start) -> Result<Value> {
    let mut tx = begin(d).await?;
    let (_, e) = output_delivery::review(&mut tx, who, q.approval_id).await?;
    if e.byte_length > 67108864
        || e.byte_length <= 0
        || i64::from(e.chunk_count) != (e.byte_length + 4194303) / 4194304
    {
        return Err(Error::Invalid(
            "isolated MP4 review supports at most 64 MiB",
        ));
    }
    let a = approval(&mut tx, who, q.approval_id).await?;
    let existing: Option<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT approval_id,user_id FROM media_review_sessions WHERE tenant_id=$1 AND id=$2",
    )
    .bind(who.tenant_id)
    .bind(q.session_id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(pair) = existing {
        if pair != (q.approval_id, who.id) {
            return Err(Error::Conflict);
        }
    } else {
        let count:i64=sqlx::query_scalar("SELECT count(*) FROM media_review_sessions WHERE tenant_id=$1 AND user_id=$2 AND expires_at>clock_timestamp()")
            .bind(who.tenant_id).bind(who.id).fetch_one(&mut *tx).await?;
        if count >= 4 {
            return Err(Error::Busy);
        }
        let now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
            .fetch_one(&mut *tx)
            .await?;
        let expires = e
            .expires_at
            .min(a.expires_at)
            .min(now + chrono::Duration::minutes(5));
        if expires <= now {
            return Err(Error::ResultUnavailable);
        }
        sqlx::query("INSERT INTO media_review_sessions(tenant_id,id,approval_id,user_id,payload_hash,policy_hash,sha256,byte_length,chunk_count,created_at,expires_at) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)")
            .bind(who.tenant_id).bind(q.session_id).bind(q.approval_id).bind(who.id).bind(governance::digest(&e.view())?).bind(governance::digest(&a.policy_snapshot)?).bind(&e.sha256).bind(e.byte_length).bind(e.chunk_count).bind(now).bind(expires).execute(&mut *tx).await?;
        audit(
            &mut tx,
            who,
            q.approval_id,
            "media_review.started",
            json!({"review_id":q.session_id,"operation_id":a.operation_id}),
        )
        .await?;
    }
    let (s, _, e, _) = session(&mut tx, who, q.session_id).await?;
    still_live(&mut tx, who, &s).await?;
    tx.commit().await?;
    Ok(
        json!({"contract":"isolated-mp4-review-v1","session_id":s.id,"approval_id":s.approval_id,"expires_at":s.expires_at,"output":e.view()}),
    )
}
pub async fn chunk(d: &Delivery, who: &Human, q: Chunk) -> Result<Bytes> {
    let mut tx = begin(d).await?;
    let (s, identity, e, _) = session(&mut tx, who, q.session_id).await?;
    if q.ordinal < 0 || q.ordinal >= s.chunk_count {
        return Err(Error::NotFound);
    }
    tx.commit().await?;
    let bytes = d
        .assets
        .quarantined_chunk(&identity, e.asset_id, q.ordinal)
        .await?;
    let expected = (s.byte_length - i64::from(q.ordinal) * 4194304).min(4194304) as usize;
    if bytes.len() != expected {
        return Err(Error::ResultUnavailable);
    }
    let mut tx = begin(d).await?;
    session(&mut tx, who, q.session_id).await?;
    sqlx::query("INSERT INTO media_review_chunks(tenant_id,session_id,ordinal,byte_length,sha256) VALUES($1,$2,$3,$4,$5) ON CONFLICT DO NOTHING")
        .bind(who.tenant_id).bind(s.id).bind(q.ordinal).bind(bytes.len() as i32).bind(store::hash(&bytes)).execute(&mut *tx).await?;
    audit(
        &mut tx,
        who,
        s.approval_id,
        "media_review.chunk_served",
        json!({"review_id":s.id,"ordinal":q.ordinal,"byte_length":bytes.len()}),
    )
    .await?;
    still_live(&mut tx, who, &s).await?;
    tx.commit().await?;
    Ok(bytes)
}
pub async fn complete(d: &Delivery, who: &Human, q: Complete) -> Result<Value> {
    if !q.reviewed {
        return Err(Error::Invalid(
            "explicit media review confirmation required",
        ));
    }
    let mut tx = begin(d).await?;
    let (s, _, e, a) = session(&mut tx, who, q.session_id).await?;
    if q.sha256 != governance::hex(&s.sha256) {
        return Err(Error::Invalid("media digest mismatch"));
    }
    let (count,total):(i64,i64)=sqlx::query_as("SELECT count(*),COALESCE(sum(byte_length),0)::bigint FROM media_review_chunks WHERE tenant_id=$1 AND session_id=$2")
        .bind(who.tenant_id).bind(s.id).fetch_one(&mut *tx).await?;
    if count != i64::from(s.chunk_count) || total != s.byte_length {
        return Err(Error::ResultUnavailable);
    }
    let existed: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM media_review_completions WHERE tenant_id=$1 AND review_id=$2)",
    )
    .bind(who.tenant_id)
    .bind(s.id)
    .fetch_one(&mut *tx)
    .await?;
    if !existed {
        sqlx::query("INSERT INTO approval_reviews(tenant_id,id,approval_id,user_id,payload_hash,policy_hash,can_continue,created_at,expires_at) VALUES($1,$2,$3,$4,$5,$6,true,$7,$8)")
            .bind(who.tenant_id).bind(s.id).bind(s.approval_id).bind(who.id).bind(&s.payload_hash).bind(&s.policy_hash).bind(s.created_at).bind(s.expires_at).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO media_review_completions(tenant_id,review_id) VALUES($1,$2)")
            .bind(who.tenant_id)
            .bind(s.id)
            .execute(&mut *tx)
            .await?;
        audit(&mut tx,who,s.approval_id,"approval.reviewed",json!({"review_id":s.id,"operation_id":a.operation_id,"review_kind":"isolated_mp4","sha256":q.sha256,"byte_length":s.byte_length,"expires_at":s.expires_at,"human_attested":true})).await?;
    }
    let model: Option<String> = sqlx::query_scalar(
        "SELECT model_id FROM governance_operations WHERE tenant_id=$1 AND id=$2",
    )
    .bind(who.tenant_id)
    .bind(a.operation_id)
    .fetch_one(&mut *tx)
    .await?;
    still_live(&mut tx, who, &s).await?;
    tx.commit().await?;
    Ok(
        json!({"approval":governance::approval_view(&a),"review_id":s.id,"review_expires_at":s.expires_at,"stage":"media_output","model_id":model,"tool_name":null,"operation_state":"waiting_approval","payload_text":null,"operation_sha256":governance::hex(&s.payload_hash),"can_continue":true,"unavailable_reason":"","media":{"kind":"isolated_mp4","sha256":governance::hex(&e.sha256),"byte_length":e.byte_length}}),
    )
}
async fn audit(
    tx: &mut Tx<'_>,
    who: &Human,
    id: Uuid,
    action: &str,
    mut details: Value,
) -> Result<()> {
    details["user_id"] = json!(who.id);
    sqlx::query("INSERT INTO audit_events(tenant_id,action,object_id,details) VALUES($1,$2,$3,$4)")
        .bind(who.tenant_id)
        .bind(action)
        .bind(id.to_string())
        .bind(details)
        .execute(&mut **tx)
        .await?;
    Ok(())
}
pub async fn maintain(pool: &sqlx::PgPool) -> Result<()> {
    sqlx::query("WITH batch AS (SELECT issuer,jti FROM human_assertions WHERE expires_at<clock_timestamp() ORDER BY expires_at LIMIT 100 FOR UPDATE SKIP LOCKED) DELETE FROM human_assertions a USING batch b WHERE a.issuer=b.issuer AND a.jti=b.jti").execute(pool).await?;
    Ok(())
}
