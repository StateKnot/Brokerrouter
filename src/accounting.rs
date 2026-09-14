use crate::{
    error::{Error, Result},
    money::{PriceSnapshot, UnitPriceSnapshot, UnitUsage},
    store::Identity,
};
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::{FromRow, PgPool, Postgres, Transaction};
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, FromRow)]
pub struct Reservation {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub key_id: Uuid,
    pub model_id: String,
    pub status: String,
    pub reserve_micros: i64,
    pub final_micros: Option<i64>,
    pub price_snapshot: Value,
    #[serde(skip)]
    pub request_hash: Vec<u8>,
    #[serde(skip)]
    pub result_ciphertext: Option<Vec<u8>>,
    pub error_code: Option<String>,
    pub routing_snapshot: Option<Value>,
    pub dispatch_deadline: Option<chrono::DateTime<chrono::Utc>>,
    pub result_expires_at: Option<chrono::DateTime<chrono::Utc>>,
    pub governance_turn_id: Option<Uuid>,
    pub guardrail_evaluation_id: Option<Uuid>,
}

pub struct Admission<'a, P = PriceSnapshot> {
    pub identity: &'a Identity,
    pub model: &'a str,
    pub idempotency_key: &'a str,
    pub request_hash: &'a [u8],
    pub maximum_micros: i64,
    pub snapshot: P,
    pub routing: Option<Value>,
    pub timeout_seconds: i32,
    pub governance_turn_id: Option<Uuid>,
}
impl<'a, P: Serialize> Admission<'a, P> {
    fn encoded(self) -> Result<Admission<'a, Value>> {
        Ok(Admission {
            identity: self.identity,
            model: self.model,
            idempotency_key: self.idempotency_key,
            request_hash: self.request_hash,
            maximum_micros: self.maximum_micros,
            snapshot: serde_json::to_value(self.snapshot).map_err(|_| Error::Configuration)?,
            routing: self.routing,
            timeout_seconds: self.timeout_seconds,
            governance_turn_id: self.governance_turn_id,
        })
    }
}

pub(crate) const COLUMNS: &str = "id,tenant_id,key_id,model_id,status,reserve_micros,final_micros,price_snapshot,request_hash,result_ciphertext,error_code,routing_snapshot,dispatch_deadline,result_expires_at,governance_turn_id,guardrail_evaluation_id";

pub(crate) async fn lock_accounts(
    tx: &mut Transaction<'_, Postgres>,
    who: &Identity,
) -> Result<()> {
    // Every money transaction uses the same tenant -> key -> request lock order.
    let tenant: Option<bool> =
        sqlx::query_scalar("SELECT active FROM tenants WHERE id=$1 FOR UPDATE")
            .bind(who.tenant_id)
            .fetch_optional(&mut **tx)
            .await?;
    if tenant.is_none() {
        return Err(Error::NotFound);
    }
    let key: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM virtual_keys WHERE tenant_id=$1 AND id=$2 FOR UPDATE")
            .bind(who.tenant_id)
            .bind(who.key_id)
            .fetch_optional(&mut **tx)
            .await?;
    if key.is_none() {
        return Err(Error::NotFound);
    }
    Ok(())
}

pub async fn reserve(pool: &PgPool, a: Admission<'_>) -> Result<(Reservation, bool)> {
    a.snapshot.validate()?;
    let fx = (a.snapshot.fx_version, a.snapshot.cny_per_unit);
    reserve_inner(pool, a.encoded()?, None, fx).await
}

pub async fn reserve_units(
    pool: &PgPool,
    a: Admission<'_, UnitPriceSnapshot>,
) -> Result<(Reservation, bool)> {
    if a.maximum_micros != a.snapshot.maximum_cost()? {
        return Err(Error::Invalid(
            "unit reservation must equal the maximum frozen quote",
        ));
    }
    let fx = (a.snapshot.fx_version, a.snapshot.cny_per_unit);
    reserve_inner(pool, a.encoded()?, None, fx).await
}

pub(crate) async fn reserve_evaluation(
    pool: &PgPool,
    a: Admission<'_>,
    evaluation: Uuid,
) -> Result<(Reservation, bool)> {
    a.snapshot.validate()?;
    let fx = (a.snapshot.fx_version, a.snapshot.cny_per_unit);
    reserve_inner(pool, a.encoded()?, Some(evaluation), fx).await
}

async fn reserve_inner(
    pool: &PgPool,
    a: Admission<'_, Value>,
    evaluation: Option<Uuid>,
    fx: (Option<i64>, rust_decimal::Decimal),
) -> Result<(Reservation, bool)> {
    let mut tx = pool.begin().await?;
    let result = admit(&mut tx, a, evaluation, fx).await?;
    tx.commit().await?;
    Ok(result)
}

pub(crate) async fn reserve_units_in(
    tx: &mut Transaction<'_, Postgres>,
    a: Admission<'_, UnitPriceSnapshot>,
) -> Result<(Reservation, bool)> {
    if a.maximum_micros != a.snapshot.maximum_cost()? {
        return Err(Error::Invalid("unit reservation must equal frozen quote"));
    }
    let fx = (a.snapshot.fx_version, a.snapshot.cny_per_unit);
    admit(tx, a.encoded()?, None, fx).await
}

async fn admit(
    tx: &mut Transaction<'_, Postgres>,
    a: Admission<'_, Value>,
    evaluation: Option<Uuid>,
    fx: (Option<i64>, rust_decimal::Decimal),
) -> Result<(Reservation, bool)> {
    if a.maximum_micros < 0
        || !(1..=600).contains(&a.timeout_seconds)
        || a.idempotency_key.is_empty()
        || a.idempotency_key.len() > 200
        || a.request_hash.len() != 32
    {
        return Err(Error::Invalid("invalid admission parameters"));
    }
    let who = a.identity;
    lock_accounts(tx, who).await?;
    let allowed:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM virtual_keys k JOIN tenants t ON t.id=k.tenant_id JOIN key_models g ON g.tenant_id=k.tenant_id AND g.key_id=k.id JOIN models m ON m.tenant_id=g.tenant_id AND m.id=g.model_id WHERE k.tenant_id=$1 AND k.id=$2 AND t.active AND k.revoked_at IS NULL AND (k.expires_at IS NULL OR k.expires_at>now()) AND g.model_id=$3 AND m.active)")
        .bind(who.tenant_id).bind(who.key_id).bind(a.model).fetch_one(&mut **tx).await?;
    if !allowed {
        return Err(Error::Unauthorized);
    }
    if let Some(turn) = a.governance_turn_id {
        crate::governance::turn_locked(tx, who, turn, true).await?;
    }
    if let Some(id) = evaluation {
        let rule = crate::judge::authority_locked(tx, who, id).await?;
        if rule.model != a.model || a.maximum_micros > rule.maximum_micros()? {
            return Err(Error::Invalid(
                "judge request exceeds its configured cost or model boundary",
            ));
        }
    }
    if let Some(existing) = sqlx::query_as::<_, Reservation>(&format!(
        "SELECT {COLUMNS} FROM requests WHERE tenant_id=$1 AND key_id=$2 AND idempotency_key=$3"
    ))
    .bind(who.tenant_id)
    .bind(who.key_id)
    .bind(a.idempotency_key)
    .fetch_optional(&mut **tx)
    .await?
    {
        if existing.request_hash != a.request_hash
            || existing.model_id != a.model
            || existing.guardrail_evaluation_id != evaluation
            || existing.price_snapshot.get("schema") != a.snapshot.get("schema")
        {
            return Err(Error::Conflict);
        }
        return Ok((existing, false));
    }
    if let Some(version) = fx.0 {
        let valid: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM fx_rates WHERE tenant_id=$1 AND id=$2 AND usd_cny=$3 AND effective_at<=now())")
            .bind(who.tenant_id).bind(version).bind(fx.1).fetch_one(&mut **tx).await?;
        if !valid {
            return Err(Error::Invalid(
                "exchange-rate snapshot is not an effective tenant version",
            ));
        }
    }
    let tenant=sqlx::query("UPDATE tenants SET reserved_micros=reserved_micros+$2 WHERE id=$1 AND budget_micros-spent_micros-reserved_micros >= $2").bind(who.tenant_id).bind(a.maximum_micros).execute(&mut **tx).await?;
    let key=sqlx::query("UPDATE virtual_keys SET reserved_micros=reserved_micros+$3 WHERE tenant_id=$1 AND id=$2 AND budget_micros-spent_micros-reserved_micros >= $3").bind(who.tenant_id).bind(who.key_id).bind(a.maximum_micros).execute(&mut **tx).await?;
    if tenant.rows_affected() != 1 || key.rows_affected() != 1 {
        return Err(Error::Budget);
    }
    let id = Uuid::new_v4();
    let row=sqlx::query_as::<_,Reservation>(&format!("INSERT INTO requests(id,tenant_id,key_id,model_id,idempotency_key,request_hash,status,reserve_micros,price_snapshot,routing_snapshot,dispatch_deadline,governance_turn_id,guardrail_evaluation_id) VALUES($1,$2,$3,$4,$5,$6,'reserved',$7,$8,$9,now()+make_interval(secs=>$10),$11,$12) RETURNING {COLUMNS}"))
        .bind(id).bind(who.tenant_id).bind(who.key_id).bind(a.model).bind(a.idempotency_key).bind(a.request_hash).bind(a.maximum_micros).bind(sqlx::types::Json(a.snapshot)).bind(a.routing).bind(f64::from(a.timeout_seconds)).bind(a.governance_turn_id).bind(evaluation).fetch_one(&mut **tx).await?;
    sqlx::query("INSERT INTO ledger_entries(tenant_id,request_id,kind,amount_micros) VALUES($1,$2,'reserve',$3)").bind(who.tenant_id).bind(id).bind(a.maximum_micros).execute(&mut **tx).await?;
    Ok((row, true))
}

pub async fn get(pool: &PgPool, who: &Identity, id: Uuid) -> Result<Reservation> {
    sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM requests WHERE tenant_id=$1 AND key_id=$2 AND id=$3"
    ))
    .bind(who.tenant_id)
    .bind(who.key_id)
    .bind(id)
    .fetch_optional(pool)
    .await?
    .ok_or(Error::NotFound)
}

pub async fn mark_submitting(pool: &PgPool, who: &Identity, id: Uuid) -> Result<()> {
    // Recheck live key authority at the dispatch boundary, not only at HTTP admission.
    let mut tx = pool.begin().await?;
    lock_accounts(&mut tx, who).await?;
    let rows=sqlx::query("UPDATE requests r SET status='submitting',updated_at=now() FROM virtual_keys k,tenants t,key_models g,models m WHERE r.tenant_id=$1 AND r.key_id=$2 AND r.id=$3 AND r.status='reserved' AND k.tenant_id=r.tenant_id AND k.id=r.key_id AND k.revoked_at IS NULL AND (k.expires_at IS NULL OR k.expires_at>now()) AND t.id=r.tenant_id AND t.active AND g.tenant_id=r.tenant_id AND g.key_id=r.key_id AND g.model_id=r.model_id AND m.tenant_id=g.tenant_id AND m.id=g.model_id AND m.active")
        .bind(who.tenant_id).bind(who.key_id).bind(id).execute(&mut *tx).await?;
    if rows.rows_affected() != 1 {
        return Err(Error::InProgress);
    }
    tx.commit().await?;
    Ok(())
}

pub async fn mark_unknown(pool: &PgPool, who: &Identity, id: Uuid, code: &str) -> Result<()> {
    sqlx::query("UPDATE requests SET status='submission_unknown',error_code=$4,updated_at=now() WHERE tenant_id=$1 AND key_id=$2 AND id=$3 AND status='submitting'").bind(who.tenant_id).bind(who.key_id).bind(id).bind(code).execute(pool).await?;
    Ok(())
}

pub async fn finish(
    pool: &PgPool,
    who: &Identity,
    id: Uuid,
    actual: i64,
    result: Option<&[u8]>,
    details: Value,
) -> Result<()> {
    if actual < 0 {
        return Err(Error::Invalid("negative cost"));
    }
    let mut tx = pool.begin().await?;
    lock_accounts(&mut tx, who).await?;
    let row = sqlx::query_as::<_, Reservation>(&format!(
        "SELECT {COLUMNS} FROM requests WHERE tenant_id=$1 AND key_id=$2 AND id=$3 FOR UPDATE"
    ))
    .bind(who.tenant_id)
    .bind(who.key_id)
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(Error::NotFound)?;
    finish_locked(&mut tx, who, &row, actual, result, details).await?;
    tx.commit().await?;
    Ok(())
}

pub(crate) async fn finish_locked(
    tx: &mut Transaction<'_, Postgres>,
    who: &Identity,
    row: &Reservation,
    actual: i64,
    result: Option<&[u8]>,
    details: Value,
) -> Result<()> {
    if actual < 0 {
        return Err(Error::Invalid("negative cost"));
    }
    let id = row.id;
    if row.status == "succeeded" {
        if row.final_micros != Some(actual) {
            return Err(Error::Conflict);
        }
        return Ok(());
    }
    if row.status != "submitting" && row.status != "submission_unknown" {
        return Err(Error::InProgress);
    }
    if actual > row.reserve_micros {
        return Err(Error::Invalid(
            "actual cost exceeds reservation; reconciliation required",
        ));
    }
    sqlx::query("UPDATE tenants SET reserved_micros=reserved_micros-$2,spent_micros=spent_micros+$3 WHERE id=$1").bind(who.tenant_id).bind(row.reserve_micros).bind(actual).execute(&mut **tx).await?;
    sqlx::query("UPDATE virtual_keys SET reserved_micros=reserved_micros-$3,spent_micros=spent_micros+$4 WHERE tenant_id=$1 AND id=$2").bind(who.tenant_id).bind(who.key_id).bind(row.reserve_micros).bind(actual).execute(&mut **tx).await?;
    sqlx::query("UPDATE requests SET status='succeeded',final_micros=$4,result_ciphertext=$5,error_code=NULL,updated_at=now() WHERE tenant_id=$1 AND key_id=$2 AND id=$3").bind(who.tenant_id).bind(who.key_id).bind(id).bind(actual).bind(result).execute(&mut **tx).await?;
    sqlx::query("INSERT INTO ledger_entries(tenant_id,request_id,kind,amount_micros,details) VALUES($1,$2,'finalize',$3,$4),($1,$2,'release',$5,'{}')").bind(who.tenant_id).bind(id).bind(actual).bind(details).bind(row.reserve_micros-actual).execute(&mut **tx).await?;
    Ok(())
}

pub async fn settle_usage(
    pool: &PgPool,
    who: &Identity,
    id: Uuid,
    input_tokens: u64,
    output_tokens: u64,
    encrypted_result: Option<&[u8]>,
) -> Result<()> {
    let request = get(pool, who, id).await?;
    let snapshot: PriceSnapshot = serde_json::from_value(request.price_snapshot)
        .map_err(|_| Error::Invalid("invalid persisted price snapshot; reconciliation required"))?;
    let cost = snapshot.breakdown(input_tokens, output_tokens)?;
    finish(
        pool,
        who,
        id,
        cost.cny_micros,
        encrypted_result,
        json!({
            "input_tokens":input_tokens,
            "output_tokens":output_tokens,
            "cost":cost,
            "pricing_basis":"configured_endpoint_price"
        }),
    )
    .await
}

pub async fn settle_units(
    pool: &PgPool,
    who: &Identity,
    id: Uuid,
    usage: &[UnitUsage],
) -> Result<()> {
    let request = get(pool, who, id).await?;
    let (actual, details) = unit_bill(&request.price_snapshot, usage)?;
    let mut tx = pool.begin().await?;
    lock_accounts(&mut tx, who).await?;
    let row = sqlx::query_as::<_, Reservation>(&format!(
        "SELECT {COLUMNS} FROM requests WHERE tenant_id=$1 AND key_id=$2 AND id=$3 FOR UPDATE"
    ))
    .bind(who.tenant_id)
    .bind(who.key_id)
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    if row.status == "succeeded" {
        let previous: Value = sqlx::query_scalar("SELECT details FROM ledger_entries WHERE tenant_id=$1 AND request_id=$2 AND kind='finalize'")
            .bind(who.tenant_id).bind(id).fetch_one(&mut *tx).await?;
        if previous != details {
            return Err(Error::Conflict);
        }
    }
    finish_locked(&mut tx, who, &row, actual, None, details).await?;
    tx.commit().await?;
    Ok(())
}

pub(crate) fn unit_bill(snapshot: &Value, usage: &[UnitUsage]) -> Result<(i64, Value)> {
    let snapshot: UnitPriceSnapshot = serde_json::from_value(snapshot.clone()).map_err(|_| {
        Error::Invalid("invalid persisted unit price snapshot; reconciliation required")
    })?;
    let cost = snapshot.breakdown(usage)?;
    let mut canonical_usage = usage.to_vec();
    for used in &mut canonical_usage {
        used.quantity = used.quantity.normalize();
    }
    canonical_usage.sort_by(|a, b| a.unit.cmp(&b.unit));
    let details = json!({"usage_units":canonical_usage,"cost":cost,"price_version":snapshot.price_version,"pricing_basis":"configured_unit_price"});
    Ok((cost.cny_micros, details))
}

pub async fn release_unsubmitted(pool: &PgPool, who: &Identity, id: Uuid) -> Result<()> {
    let mut tx = pool.begin().await?;
    lock_accounts(&mut tx, who).await?;
    let row = sqlx::query_as::<_, Reservation>(&format!(
        "SELECT {COLUMNS} FROM requests WHERE tenant_id=$1 AND key_id=$2 AND id=$3 FOR UPDATE"
    ))
    .bind(who.tenant_id)
    .bind(who.key_id)
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(Error::NotFound)?;
    release_locked(&mut tx, who, &row).await?;
    tx.commit().await?;
    Ok(())
}

pub(crate) async fn release_locked(
    tx: &mut Transaction<'_, Postgres>,
    who: &Identity,
    row: &Reservation,
) -> Result<()> {
    let id = row.id;
    if row.status == "failed" {
        return Ok(());
    }
    if row.status != "reserved" {
        return Err(Error::InProgress);
    }
    sqlx::query("UPDATE tenants SET reserved_micros=reserved_micros-$2 WHERE id=$1")
        .bind(who.tenant_id)
        .bind(row.reserve_micros)
        .execute(&mut **tx)
        .await?;
    sqlx::query(
        "UPDATE virtual_keys SET reserved_micros=reserved_micros-$3 WHERE tenant_id=$1 AND id=$2",
    )
    .bind(who.tenant_id)
    .bind(who.key_id)
    .bind(row.reserve_micros)
    .execute(&mut **tx)
    .await?;
    sqlx::query("UPDATE requests SET status='failed',error_code='not_submitted',final_micros=0,updated_at=now() WHERE tenant_id=$1 AND key_id=$2 AND id=$3").bind(who.tenant_id).bind(who.key_id).bind(id).execute(&mut **tx).await?;
    sqlx::query("INSERT INTO ledger_entries(tenant_id,request_id,kind,amount_micros,details) VALUES($1,$2,'release',$3,$4)").bind(who.tenant_id).bind(id).bind(row.reserve_micros).bind(json!({"reason":"not_submitted"})).execute(&mut **tx).await?;
    Ok(())
}
