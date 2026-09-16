//! Read-only monitoring uses deduplicated facts, never decrypted risk material.
use crate::{
    console::{self, Permission, Timezone},
    error::{Error, Result},
    governance::{self, Guardrail},
    human::Human,
};
use chrono::{DateTime, Days, NaiveDate, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{FromRow, PgPool, Postgres, QueryBuilder, Transaction};
use uuid::Uuid;

pub(crate) async fn observe(
    tx: &mut Transaction<'_, Postgres>,
    tenant: Uuid,
    operation: Uuid,
    rail: &Guardrail,
    outcome: &str,
    duration: Option<i64>,
) -> Result<()> {
    sqlx::query("INSERT INTO guardrail_observations(tenant_id,operation_id,guardrail_id,version,first_outcome,inline_duration_us,source,approval_skipped_at) VALUES($1,$2,$3,$4,$5,$6,'live',CASE WHEN $5='skipped_by_user_approval' THEN clock_timestamp() END) ON CONFLICT(tenant_id,operation_id,guardrail_id,version) DO UPDATE SET approval_skipped_at=EXCLUDED.approval_skipped_at WHERE guardrail_observations.approval_skipped_at IS NULL AND EXCLUDED.approval_skipped_at IS NOT NULL")
        .bind(tenant).bind(operation).bind(&rail.id).bind(rail.version).bind(outcome).bind(duration).execute(&mut **tx).await?;
    Ok(())
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Filter {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub guardrail_id: Option<String>,
    pub model_id: Option<String>,
    pub key_id: Option<Uuid>,
    pub stage: Option<String>,
}
impl Filter {
    fn validate(&self) -> Result<()> {
        if self.start >= self.end
            || self.end - self.start > chrono::Duration::days(31)
            || self
                .guardrail_id
                .as_ref()
                .is_some_and(|v| !governance::valid_text(v, 100))
            || self
                .model_id
                .as_ref()
                .is_some_and(|v| !governance::valid_text(v, 200))
            || self.stage.as_deref().is_some_and(|v| {
                !matches!(
                    v,
                    "pre" | "post" | "tool_input" | "tool_output" | "artifact" | "media_output"
                )
            })
        {
            return Err(Error::Invalid("invalid monitoring filter; maximum 31 days"));
        }
        Ok(())
    }
    fn apply<'a>(&'a self, q: &mut QueryBuilder<'a, Postgres>, tenant: Uuid) {
        q.push(" WHERE tenant_id=")
            .push_bind(tenant)
            .push(" AND created_at>=")
            .push_bind(self.start)
            .push(" AND created_at<")
            .push_bind(self.end);
        if let Some(v) = &self.guardrail_id {
            q.push(" AND guardrail_id=").push_bind(v);
        }
        if let Some(v) = &self.model_id {
            q.push(" AND model_id=").push_bind(v);
        }
        if let Some(v) = self.key_id {
            q.push(" AND key_id=").push_bind(v);
        }
        if let Some(v) = &self.stage {
            q.push(" AND stage=").push_bind(v);
        }
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Query {
    pub filter: Filter,
    #[serde(default)]
    pub timezone: Timezone,
}

const AGGREGATE: &str = "count(*) AS checks,count(*) FILTER(WHERE outcome='passed') AS passed,count(*) FILTER(WHERE outcome='risk') AS risk,count(*) FILTER(WHERE outcome='failed') AS failed,count(*) FILTER(WHERE outcome='pending') AS pending,count(*) FILTER(WHERE outcome='running') AS running,count(*) FILTER(WHERE outcome='running' AND judge_deadline<=transaction_timestamp()) AS overdue_running,count(*) FILTER(WHERE outcome='skipped_by_user_approval') AS skipped_initial,count(approval_skipped_at) AS approval_skipped,count(*) FILTER(WHERE source<>'live') AS legacy_checks,count(inline_duration_us) AS inline_samples,avg(inline_duration_us)::numeric/1000 AS mean_inline_ms,count(judge_lifecycle_ms) AS judge_samples,avg(judge_lifecycle_ms)::numeric AS mean_judge_lifecycle_ms,count(request_id) AS judge_requests,count(cost_micros) AS known_cost_requests,COALESCE(sum(cost_micros),0)::numeric AS cost_micros,COALESCE(sum(reserved_micros),0)::numeric AS reserved_micros";
#[derive(Default, FromRow)]
struct Totals {
    checks: i64,
    passed: i64,
    risk: i64,
    failed: i64,
    pending: i64,
    running: i64,
    overdue_running: i64,
    skipped_initial: i64,
    approval_skipped: i64,
    legacy_checks: i64,
    inline_samples: i64,
    mean_inline_ms: Option<Decimal>,
    judge_samples: i64,
    mean_judge_lifecycle_ms: Option<Decimal>,
    judge_requests: i64,
    known_cost_requests: i64,
    cost_micros: Decimal,
    reserved_micros: Decimal,
}
impl Totals {
    fn json(&self) -> Value {
        let classified = self.passed + self.risk;
        json!({"checks":self.checks.to_string(),"passed":self.passed.to_string(),"risk":self.risk.to_string(),"failed":self.failed.to_string(),"pending":self.pending.to_string(),"running":self.running.to_string(),"overdue_running":self.overdue_running.to_string(),"skipped_initial":self.skipped_initial.to_string(),"approval_skipped":self.approval_skipped.to_string(),"legacy_checks":self.legacy_checks.to_string(),"classified_pass_percent":if classified==0 {None} else {Some((Decimal::from(self.passed)*Decimal::from(100)/Decimal::from(classified)).round_dp(4).to_string())},"inline_samples":self.inline_samples.to_string(),"mean_inline_ms":self.mean_inline_ms.map(|v| v.round_dp(6).to_string()),"judge_samples":self.judge_samples.to_string(),"mean_judge_lifecycle_ms":self.mean_judge_lifecycle_ms.map(|v| v.round_dp(6).to_string()),"judge_requests":self.judge_requests.to_string(),"known_cost_requests":self.known_cost_requests.to_string(),"unsettled_cost_requests":(self.judge_requests-self.known_cost_requests).to_string(),"known_cost_cny":format!("{:.6}",self.cost_micros/Decimal::from(1_000_000)),"reserved_cny":format!("{:.6}",self.reserved_micros/Decimal::from(1_000_000))})
    }
}
#[derive(FromRow)]
struct Daily {
    day: NaiveDate,
    #[sqlx(flatten)]
    totals: Totals,
}
pub async fn summary(pool: &PgPool, who: &Human, query: Query) -> Result<Value> {
    query.filter.validate()?;
    let mut tx = console::begin(pool, who, Permission::MonitoringRead).await?;
    let (observed_at, recorded_from): (DateTime<Utc>, DateTime<Utc>) = sqlx::query_as(
        "SELECT transaction_timestamp(),recorded_from FROM monitoring_coverage WHERE singleton",
    )
    .fetch_one(&mut *tx)
    .await?;
    let mut q = QueryBuilder::new(format!("SELECT {AGGREGATE} FROM console_guardrail_facts"));
    query.filter.apply(&mut q, who.tenant_id);
    let totals = q.build_query_as::<Totals>().fetch_one(&mut *tx).await?;
    let mut q = QueryBuilder::new("SELECT timezone(");
    q.push_bind(query.timezone.name()).push(format!(
        ",created_at)::date AS day,{AGGREGATE} FROM console_guardrail_facts"
    ));
    query.filter.apply(&mut q, who.tenant_id);
    q.push(" GROUP BY day ORDER BY day");
    let rows = q.build_query_as::<Daily>().fetch_all(&mut *tx).await?;
    let mut days: std::collections::BTreeMap<_, _> =
        rows.into_iter().map(|r| (r.day, r.totals)).collect();
    let (mut day,last):(NaiveDate,NaiveDate)=sqlx::query_as("SELECT timezone($1,$2::timestamptz)::date,timezone($1,$3::timestamptz-interval '1 microsecond')::date").bind(query.timezone.name()).bind(query.filter.start).bind(query.filter.end).fetch_one(&mut *tx).await?;
    let mut daily = Vec::new();
    while day <= last {
        daily.push(json!({"date":day,"totals":days.remove(&day).unwrap_or_default().json()}));
        day = day
            .checked_add_days(Days::new(1))
            .ok_or(Error::Invalid("date overflow"))?;
    }
    // Count each matched operation once; approval rows form a separate denominator.
    let mut q = QueryBuilder::new(
        "WITH matched AS (SELECT DISTINCT tenant_id,operation_id,operation_state,execution_deadline,turn_state,turn_expires_at FROM console_guardrail_facts",
    );
    query.filter.apply(&mut q, who.tenant_id);
    q.push(") SELECT CASE WHEN operation_state='executing' AND execution_deadline<=transaction_timestamp() THEN 'execution_unknown' ELSE operation_state END AS state,count(*) AS count,count(*) FILTER(WHERE turn_state<>'active' OR turn_expires_at<=transaction_timestamp()) AS closed_turn FROM matched GROUP BY 1 ORDER BY 1");
    let states: Vec<(String, i64, i64)> = q.build_query_as().fetch_all(&mut *tx).await?;
    let operations:Vec<_>=states.into_iter().map(|(state,count,closed)|json!({"state":state,"count":count.to_string(),"closed_turn":closed.to_string()})).collect();
    let mut q = QueryBuilder::new(
        "WITH matched AS (SELECT DISTINCT tenant_id,operation_id FROM console_guardrail_facts",
    );
    query.filter.apply(&mut q, who.tenant_id);
    q.push(") SELECT CASE WHEN a.state='pending' AND a.expires_at<=transaction_timestamp() THEN 'expired_pending' ELSE a.state END AS state,count(*) AS count FROM console_approval_facts a JOIN matched m ON m.tenant_id=a.tenant_id AND m.operation_id=a.operation_id GROUP BY 1 ORDER BY 1");
    let states: Vec<(String, i64)> = q.build_query_as().fetch_all(&mut *tx).await?;
    let approvals: Vec<_> = states
        .into_iter()
        .map(|(state, count)| json!({"state":state,"count":count.to_string()}))
        .collect();
    let active: i64 =
        sqlx::query_scalar("SELECT count(*) FROM console_active_guardrails WHERE tenant_id=$1")
            .bind(who.tenant_id)
            .fetch_one(&mut *tx)
            .await?;
    console::finish(tx,pool,who,Permission::MonitoringRead,json!({"currency":"CNY","cost_basis":"configured_price_ledger_not_provider_invoice","time_basis":"first_observation_at","observed_at":observed_at,"recorded_from":recorded_from,"history_may_be_partial":query.filter.start<recorded_from,"timezone":query.timezone,"filter":query.filter,"totals":totals.json(),"daily":daily,"operations":operations,"approvals":approvals,"tenant_active_guardrails":active.to_string()})).await
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Cursor {
    pub guardrail_id: String,
    pub version: i32,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RailsQuery {
    pub filter: Filter,
    pub after: Option<Cursor>,
    pub limit: i64,
}
#[derive(FromRow)]
struct Rail {
    guardrail_id: String,
    version: i32,
    name: String,
    stage: String,
    kind: String,
    current_active: bool,
    #[sqlx(flatten)]
    totals: Totals,
}
pub async fn rails(pool: &PgPool, who: &Human, query: RailsQuery) -> Result<Value> {
    query.filter.validate()?;
    if !(1..=100).contains(&query.limit)
        || query
            .after
            .as_ref()
            .is_some_and(|v| !governance::valid_text(&v.guardrail_id, 100) || v.version < 1)
    {
        return Err(Error::Invalid("invalid rail page"));
    }
    let mut tx = console::begin(pool, who, Permission::MonitoringRead).await?;
    let observed_at: DateTime<Utc> = sqlx::query_scalar("SELECT transaction_timestamp()")
        .fetch_one(&mut *tx)
        .await?;
    let mut q = QueryBuilder::new(format!(
        "SELECT guardrail_id,version,name,stage,kind,current_active,{AGGREGATE} FROM console_guardrail_facts"
    ));
    query.filter.apply(&mut q, who.tenant_id);
    if let Some(after) = &query.after {
        q.push(" AND (guardrail_id COLLATE \"C\",version)>(")
            .push_bind(&after.guardrail_id)
            .push(",")
            .push_bind(after.version)
            .push(")");
    }
    q.push(" GROUP BY guardrail_id,version,name,stage,kind,current_active ORDER BY guardrail_id COLLATE \"C\",version LIMIT ").push_bind(query.limit+1);
    let mut rows = q.build_query_as::<Rail>().fetch_all(&mut *tx).await?;
    let more = rows.len() > query.limit as usize;
    rows.truncate(query.limit as usize);
    let next = if more {
        rows.last()
            .map(|r| json!({"guardrail_id":r.guardrail_id,"version":r.version}))
    } else {
        None
    };
    let data:Vec<_>=rows.into_iter().map(|r|json!({"guardrail_id":r.guardrail_id,"version":r.version,"name":r.name,"stage":r.stage,"kind":r.kind,"current_active":r.current_active,"totals":r.totals.json()})).collect();
    console::finish(
        tx,
        pool,
        who,
        Permission::MonitoringRead,
        json!({"observed_at":observed_at,"data":data,"next_after":next}),
    )
    .await
}
