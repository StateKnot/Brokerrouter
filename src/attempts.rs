use crate::{
    accounting::{self, COLUMNS, Reservation},
    error::{Error, Result},
    gateway::Route,
    store::Identity,
};
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

async fn locked(
    tx: &mut Transaction<'_, Postgres>,
    who: &Identity,
    id: Uuid,
) -> Result<Reservation> {
    accounting::lock_accounts(tx, who).await?;
    sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM requests WHERE tenant_id=$1 AND key_id=$2 AND id=$3 FOR UPDATE"
    ))
    .bind(who.tenant_id)
    .bind(who.key_id)
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(Error::NotFound)
}

pub async fn begin(pool: &PgPool, who: &Identity, id: Uuid) -> Result<(Uuid, Route)> {
    let mut tx = pool.begin().await?;
    let row = locked(&mut tx, who, id).await?;
    if row.status != "reserved" {
        return Err(Error::InProgress);
    }
    let now: chrono::DateTime<chrono::Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut *tx)
        .await?;
    if row.dispatch_deadline.is_none_or(|d| d <= now) {
        return Err(Error::NotSubmitted);
    }
    let routes: Vec<Route> =
        serde_json::from_value(row.routing_snapshot.ok_or(Error::Configuration)?)
            .map_err(|_| Error::Configuration)?;
    let ordinal: i64 =
        sqlx::query_scalar("SELECT count(*) FROM attempts WHERE tenant_id=$1 AND request_id=$2")
            .bind(who.tenant_id)
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
    let route = routes
        .get(ordinal as usize)
        .ok_or(Error::NotSubmitted)?
        .clone();
    let allowed:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM virtual_keys k JOIN tenants t ON t.id=k.tenant_id JOIN key_models g ON g.tenant_id=k.tenant_id AND g.key_id=k.id JOIN models m ON m.tenant_id=g.tenant_id AND m.id=g.model_id JOIN endpoints e ON e.tenant_id=m.tenant_id AND e.model_id=m.id WHERE k.tenant_id=$1 AND k.id=$2 AND k.revoked_at IS NULL AND (k.expires_at IS NULL OR k.expires_at>clock_timestamp()) AND t.active AND m.active AND g.model_id=$3 AND e.id=$4 AND e.active)")
        .bind(who.tenant_id).bind(who.key_id).bind(&row.model_id).bind(route.endpoint.id).fetch_one(&mut *tx).await?;
    if !allowed {
        return Err(Error::Unauthorized);
    }
    let attempt = Uuid::new_v4();
    sqlx::query("INSERT INTO attempts(id,tenant_id,request_id,endpoint_id,ordinal,status) VALUES($1,$2,$3,$4,$5,'submitting')").bind(attempt).bind(who.tenant_id).bind(id).bind(route.endpoint.id).bind(ordinal as i32).execute(&mut *tx).await?;
    sqlx::query("UPDATE requests SET status='submitting',updated_at=now() WHERE id=$1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok((attempt, route))
}

pub async fn failed(
    pool: &PgPool,
    who: &Identity,
    id: Uuid,
    attempt: Uuid,
    not_sent: bool,
    code: &str,
    http_status: Option<u16>,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    let row = locked(&mut tx, who, id).await?;
    if row.status != "submitting" && row.status != "submission_unknown" {
        return Err(Error::InProgress);
    }
    let state = if not_sent {
        "not_sent"
    } else {
        "submission_unknown"
    };
    let updated=sqlx::query("UPDATE attempts SET status=$4,error_code=$5,http_status=$6,finished_at=now() WHERE tenant_id=$1 AND request_id=$2 AND id=$3 AND status IN ('submitting','submission_unknown')")
        .bind(who.tenant_id).bind(id).bind(attempt).bind(state).bind(code).bind(http_status.map(i32::from)).execute(&mut *tx).await?;
    if updated.rows_affected() != 1 {
        return Err(Error::InProgress);
    }
    sqlx::query("UPDATE requests SET status=$2,error_code=$3,updated_at=now() WHERE id=$1")
        .bind(id)
        .bind(if not_sent {
            "reserved"
        } else {
            "submission_unknown"
        })
        .bind(code)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

pub struct Completion<'a> {
    pub attempt: Uuid,
    pub usage: Value,
    pub input: u64,
    pub output: u64,
    pub ciphertext: &'a [u8],
    pub ttl_seconds: i32,
}
pub async fn complete(pool: &PgPool, who: &Identity, id: Uuid, c: Completion<'_>) -> Result<()> {
    let mut tx = pool.begin().await?;
    let row = locked(&mut tx, who, id).await?;
    let (ordinal,status):(i32,String)=sqlx::query_as("SELECT ordinal,status FROM attempts WHERE tenant_id=$1 AND request_id=$2 AND id=$3 FOR UPDATE").bind(who.tenant_id).bind(id).bind(c.attempt).fetch_optional(&mut *tx).await?.ok_or(Error::NotFound)?;
    if status == "succeeded" && row.status == "succeeded" {
        tx.commit().await?;
        return Ok(());
    }
    if !["submitting", "submission_unknown"].contains(&status.as_str()) {
        return Err(Error::InProgress);
    }
    let routes: Vec<Route> =
        serde_json::from_value(row.routing_snapshot.clone().ok_or(Error::Configuration)?)
            .map_err(|_| Error::Configuration)?;
    let route = routes.get(ordinal as usize).ok_or(Error::Configuration)?;
    let cost = route.price.breakdown(c.input, c.output)?;
    accounting::finish_locked(&mut tx,who,&row,cost.cny_micros,Some(c.ciphertext),json!({"attempt_id":c.attempt,"price_snapshot":route.price,"cost":cost,"input_tokens":c.input,"output_tokens":c.output,"pricing_basis":"configured_endpoint_price","purpose":if row.guardrail_evaluation_id.is_some(){"guardrail"}else{"model"},"guardrail_evaluation_id":row.guardrail_evaluation_id})).await?;
    sqlx::query("UPDATE attempts SET status='succeeded',usage=$2,cost_micros=$3,http_status=200,finished_at=now(),error_code=NULL WHERE id=$1").bind(c.attempt).bind(c.usage).bind(cost.cny_micros).execute(&mut *tx).await?;
    sqlx::query("UPDATE requests SET result_expires_at=now()+make_interval(secs=>$2) WHERE id=$1")
        .bind(id)
        .bind(f64::from(c.ttl_seconds))
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn reconcile(
    pool: &PgPool,
    tenant: Uuid,
    id: Uuid,
    usage: Option<(u64, u64)>,
    evidence: &str,
) -> Result<()> {
    if evidence.trim().is_empty() || evidence.len() > 1000 {
        return Err(Error::Invalid(
            "a reconciliation evidence reference of 1 to 1000 bytes is required",
        ));
    }
    let key_id: Uuid =
        sqlx::query_scalar("SELECT key_id FROM requests WHERE tenant_id=$1 AND id=$2")
            .bind(tenant)
            .bind(id)
            .fetch_optional(pool)
            .await?
            .ok_or(Error::NotFound)?;
    let who = Identity {
        tenant_id: tenant,
        key_id,
    };
    let mut tx = pool.begin().await?;
    let row = locked(&mut tx, &who, id).await?;
    if row.status != "submission_unknown" && row.status != "reconciled" {
        return Err(Error::InProgress);
    }
    let (attempt,ordinal):(Uuid,i32)=sqlx::query_as("SELECT id,ordinal FROM attempts WHERE tenant_id=$1 AND request_id=$2 AND status IN ('submission_unknown','reconciled') ORDER BY ordinal DESC LIMIT 1 FOR UPDATE").bind(tenant).bind(id).fetch_optional(&mut *tx).await?.ok_or(Error::InProgress)?;
    let routes: Vec<Route> =
        serde_json::from_value(row.routing_snapshot.clone().ok_or(Error::Configuration)?)
            .map_err(|_| Error::Configuration)?;
    let route = routes.get(ordinal as usize).ok_or(Error::Configuration)?;
    let cost = usage
        .map(|(i, o)| route.price.breakdown(i, o))
        .transpose()?;
    let actual = cost.as_ref().map_or(0, |c| c.cny_micros);
    if row.status == "reconciled" {
        return if row.final_micros == Some(actual) {
            Ok(())
        } else {
            Err(Error::Conflict)
        };
    }
    accounting::finish_locked(&mut tx,&who,&row,actual,None,json!({"attempt_id":attempt,"price_snapshot":route.price,"cost":cost,"reconciliation":"operator_verified","evidence_reference":evidence,"purpose":if row.guardrail_evaluation_id.is_some(){"guardrail"}else{"model"},"guardrail_evaluation_id":row.guardrail_evaluation_id})).await?;
    sqlx::query("UPDATE requests SET status='reconciled',error_code='reconciled_without_result' WHERE id=$1").bind(id).execute(&mut *tx).await?;
    sqlx::query(
        "UPDATE attempts SET status='reconciled',cost_micros=$2,finished_at=now() WHERE id=$1",
    )
    .bind(attempt)
    .bind(actual)
    .execute(&mut *tx)
    .await?;
    sqlx::query("INSERT INTO audit_events(tenant_id,action,object_id,details) VALUES($1,'request.reconciled',$2,$3)").bind(tenant).bind(id.to_string()).bind(json!({"evidence_reference":evidence,"usage":usage,"cost_micros":actual})).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

pub async fn maintain(pool: &PgPool) -> Result<()> {
    // Never restart a POST after a process/connection loss. Only change its visibility.
    let mut tx = pool.begin().await?;
    let expired:Vec<Uuid>=sqlx::query_scalar("WITH batch AS (SELECT id FROM requests WHERE status='submitting' AND dispatch_deadline<now() AND NOT EXISTS(SELECT 1 FROM media_jobs j WHERE j.tenant_id=requests.tenant_id AND j.id=requests.id) ORDER BY dispatch_deadline LIMIT 100 FOR UPDATE SKIP LOCKED) UPDATE requests r SET status='submission_unknown',error_code='dispatch_deadline_elapsed',updated_at=now() FROM batch WHERE r.id=batch.id RETURNING r.id").fetch_all(&mut *tx).await?;
    sqlx::query("UPDATE attempts SET status='submission_unknown',error_code='dispatch_deadline_elapsed',finished_at=now() WHERE request_id=ANY($1) AND status='submitting'").bind(expired).execute(&mut *tx).await?;
    sqlx::query("WITH batch AS (SELECT id FROM requests WHERE result_expires_at<now() AND result_ciphertext IS NOT NULL ORDER BY result_expires_at LIMIT 100 FOR UPDATE SKIP LOCKED) UPDATE requests r SET result_ciphertext=NULL FROM batch WHERE r.id=batch.id").execute(&mut *tx).await?;
    tx.commit().await?;
    // An abandoned pre-dispatch reservation has no external side effects and can be released.
    let rows:Vec<(Uuid,Uuid,Uuid)>=sqlx::query_as("SELECT tenant_id,key_id,id FROM requests WHERE status='reserved' AND dispatch_deadline<now() AND NOT EXISTS(SELECT 1 FROM media_jobs j WHERE j.tenant_id=requests.tenant_id AND j.id=requests.id) LIMIT 100").fetch_all(pool).await?;
    for (tenant_id, key_id, id) in rows {
        match accounting::release_unsubmitted(pool, &Identity { tenant_id, key_id }, id).await {
            Ok(()) | Err(Error::InProgress) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
