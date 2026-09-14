//! Tenant-scoped, ledger-backed operational reports. No decryption or execution.
use crate::{
    error::{Error, Result},
    governance,
    human::Human,
};
use chrono::{DateTime, Days, NaiveDate, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{FromRow, PgPool, Postgres, QueryBuilder, Transaction};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    UsageRead,
    AuditRead,
}
impl Permission {
    fn name(self) -> &'static str {
        match self {
            Self::UsageRead => "usage_read",
            Self::AuditRead => "audit_read",
        }
    }
}

pub async fn set_permission(
    pool: &PgPool,
    tenant: Uuid,
    user: Uuid,
    permission: Permission,
    enabled: bool,
    reason: &str,
) -> Result<()> {
    if !governance::valid_text(reason, 500) {
        return Err(Error::Invalid("permission change reason required"));
    }
    let mut tx = pool.begin().await?;
    let active: Option<bool> = sqlx::query_scalar(
        "SELECT active FROM governance_users WHERE tenant_id=$1 AND id=$2 FOR UPDATE",
    )
    .bind(tenant)
    .bind(user)
    .fetch_optional(&mut *tx)
    .await?;
    if active.is_none() || (enabled && active != Some(true)) {
        return Err(Error::NotFound);
    }
    let old: Option<bool> = sqlx::query_scalar("SELECT enabled FROM console_permissions WHERE tenant_id=$1 AND user_id=$2 AND permission=$3")
        .bind(tenant).bind(user).bind(permission.name()).fetch_optional(&mut *tx).await?;
    if old != Some(enabled) {
        sqlx::query("INSERT INTO console_permissions(tenant_id,user_id,permission,enabled) VALUES($1,$2,$3,$4) ON CONFLICT(tenant_id,user_id,permission) DO UPDATE SET enabled=EXCLUDED.enabled")
            .bind(tenant).bind(user).bind(permission.name()).bind(enabled).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO audit_events(tenant_id,action,object_id,details) VALUES($1,'console.permission_changed',$2,$3)")
            .bind(tenant).bind(user.to_string()).bind(json!({"permission":permission,"previous":old,"enabled":enabled,"reason":reason})).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(())
}

pub(crate) async fn authorize(pool: &PgPool, who: &Human, permission: Permission) -> Result<()> {
    let allowed: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM console_permissions p JOIN governance_users u ON u.tenant_id=p.tenant_id AND u.id=p.user_id JOIN tenants t ON t.id=u.tenant_id WHERE p.tenant_id=$1 AND p.user_id=$2 AND p.permission=$3 AND p.enabled AND u.active AND t.active) AND clock_timestamp()<to_timestamp($4::double precision)")
        .bind(who.tenant_id).bind(who.id).bind(permission.name()).bind(who.expires_at() as f64).fetch_one(pool).await?;
    if allowed {
        Ok(())
    } else {
        Err(Error::ConsoleForbidden)
    }
}
async fn begin(
    pool: &PgPool,
    who: &Human,
    permission: Permission,
) -> Result<Transaction<'static, Postgres>> {
    authorize(pool, who, permission).await?;
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SET LOCAL statement_timeout='5s'")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SET LOCAL lock_timeout='1s'")
        .execute(&mut *tx)
        .await?;
    Ok(tx)
}
async fn finish(
    tx: Transaction<'_, Postgres>,
    pool: &PgPool,
    who: &Human,
    permission: Permission,
    body: Value,
) -> Result<Value> {
    tx.commit().await?;
    // A new snapshot checks revocation/expiry after the potentially slow report.
    authorize(pool, who, permission).await?;
    Ok(body)
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Filter {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub model_id: Option<String>,
    pub key_id: Option<Uuid>,
    pub status: Option<String>,
    pub purpose: Option<String>,
    pub request_id: Option<Uuid>,
}
impl Filter {
    fn validate(&self) -> Result<()> {
        if self.start >= self.end
            || self.end - self.start > chrono::Duration::days(31)
            || self
                .model_id
                .as_ref()
                .is_some_and(|s| !governance::valid_text(s, 200))
            || self.status.as_deref().is_some_and(|s| {
                !matches!(
                    s,
                    "reserved"
                        | "submitting"
                        | "submission_unknown"
                        | "succeeded"
                        | "failed"
                        | "reconciled"
                )
            })
            || self
                .purpose
                .as_deref()
                .is_some_and(|s| !matches!(s, "model" | "guardrail"))
        {
            return Err(Error::Invalid(
                "invalid report filter; time window must be within 31 days",
            ));
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
        if let Some(v) = &self.model_id {
            q.push(" AND model_id=").push_bind(v);
        }
        if let Some(v) = self.key_id {
            q.push(" AND key_id=").push_bind(v);
        }
        if let Some(v) = &self.status {
            q.push(" AND status=").push_bind(v);
        }
        if let Some(v) = &self.purpose {
            q.push(" AND purpose=").push_bind(v);
        }
        if let Some(v) = self.request_id {
            q.push(" AND id=").push_bind(v);
        }
    }
}

#[derive(Clone, Copy, Default, Deserialize, Serialize)]
pub enum Timezone {
    #[default]
    #[serde(rename = "Asia/Shanghai")]
    Shanghai,
    UTC,
}
impl Timezone {
    fn name(self) -> &'static str {
        match self {
            Self::Shanghai => "Asia/Shanghai",
            Self::UTC => "UTC",
        }
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageQuery {
    pub filter: Filter,
    #[serde(default)]
    pub timezone: Timezone,
}
const AGGREGATE: &str = "count(*) AS requests,count(*) FILTER(WHERE status='succeeded') AS succeeded,count(*) FILTER(WHERE status='failed') AS failed,count(*) FILTER(WHERE status='reconciled') AS reconciled,count(*) FILTER(WHERE status IN ('reserved','submitting')) AS in_progress,count(*) FILTER(WHERE status='submission_unknown') AS submission_unknown,count(cost_micros) AS known_cost_requests,COALESCE(sum(cost_micros),0)::numeric AS cost_micros,COALESCE(sum(reserved_micros),0)::numeric AS reserved_micros,count(*) FILTER(WHERE input_tokens IS NOT NULL AND output_tokens IS NOT NULL) AS token_usage_requests,COALESCE(sum(input_tokens),0)::numeric AS input_tokens,COALESCE(sum(output_tokens),0)::numeric AS output_tokens";
#[derive(Default, FromRow)]
struct Totals {
    requests: i64,
    succeeded: i64,
    failed: i64,
    reconciled: i64,
    in_progress: i64,
    submission_unknown: i64,
    known_cost_requests: i64,
    cost_micros: Decimal,
    reserved_micros: Decimal,
    token_usage_requests: i64,
    input_tokens: Decimal,
    output_tokens: Decimal,
}
fn cny(micros: Decimal) -> String {
    format!("{:.6}", micros / Decimal::from(1_000_000))
}
impl Totals {
    fn json(&self) -> Value {
        json!({"requests":self.requests.to_string(),"succeeded":self.succeeded.to_string(),"failed":self.failed.to_string(),"reconciled":self.reconciled.to_string(),"in_progress":self.in_progress.to_string(),"submission_unknown":self.submission_unknown.to_string(),"known_cost_requests":self.known_cost_requests.to_string(),"unsettled_cost_requests":(self.requests-self.known_cost_requests).to_string(),"known_cost_cny":cny(self.cost_micros),"reserved_cny":cny(self.reserved_micros),"mean_known_request_cost_cny":if self.known_cost_requests==0 {None} else {Some((self.cost_micros/Decimal::from(1_000_000)/Decimal::from(self.known_cost_requests)).round_dp(12).to_string())},"token_usage_requests":self.token_usage_requests.to_string(),"input_tokens":self.input_tokens.to_string(),"output_tokens":self.output_tokens.to_string(),"total_tokens":(self.input_tokens+self.output_tokens).to_string()})
    }
}
#[derive(FromRow)]
struct Daily {
    day: NaiveDate,
    #[sqlx(flatten)]
    totals: Totals,
}
pub async fn usage(pool: &PgPool, who: &Human, query: UsageQuery) -> Result<Value> {
    query.filter.validate()?;
    let mut tx = begin(pool, who, Permission::UsageRead).await?;
    let observed_at: DateTime<Utc> = sqlx::query_scalar("SELECT transaction_timestamp()")
        .fetch_one(&mut *tx)
        .await?;
    let mut q = QueryBuilder::new(format!("SELECT {AGGREGATE} FROM console_request_facts"));
    query.filter.apply(&mut q, who.tenant_id);
    let total = q.build_query_as::<Totals>().fetch_one(&mut *tx).await?;
    let mut q = QueryBuilder::new("SELECT timezone(");
    q.push_bind(query.timezone.name()).push(format!(
        ",created_at)::date AS day,{AGGREGATE} FROM console_request_facts"
    ));
    query.filter.apply(&mut q, who.tenant_id);
    q.push(" GROUP BY day ORDER BY day");
    let daily = q.build_query_as::<Daily>().fetch_all(&mut *tx).await?;
    let mut daily: std::collections::BTreeMap<_, _> =
        daily.into_iter().map(|d| (d.day, d.totals)).collect();
    let (mut day,last):(NaiveDate,NaiveDate)=sqlx::query_as("SELECT timezone($1,$2::timestamptz)::date,timezone($1,$3::timestamptz-interval '1 microsecond')::date")
        .bind(query.timezone.name()).bind(query.filter.start).bind(query.filter.end).fetch_one(&mut *tx).await?;
    let mut series = Vec::new();
    while day <= last {
        series.push(json!({"date":day,"totals":daily.remove(&day).unwrap_or_default().json()}));
        day = day
            .checked_add_days(Days::new(1))
            .ok_or(Error::Invalid("date overflow"))?;
    }
    finish(tx,pool,who,Permission::UsageRead,json!({"currency":"CNY","cost_basis":"configured_price_ledger_not_provider_invoice","time_basis":"request_created_at","observed_at":observed_at,"filter":query.filter,"timezone":query.timezone,"totals":total.json(),"daily":series})).await
}

fn default_limit() -> i64 {
    50
}
fn limit(n: i64) -> Result<()> {
    if (1..=100).contains(&n) {
        Ok(())
    } else {
        Err(Error::Invalid("limit must be 1 to 100"))
    }
}
#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Dimension {
    Model,
    Key,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GroupQuery {
    pub filter: Filter,
    pub dimension: Dimension,
    pub after: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: i64,
}
#[derive(FromRow)]
struct Group {
    object_id: String,
    label: String,
    #[sqlx(flatten)]
    totals: Totals,
}
pub async fn groups(pool: &PgPool, who: &Human, query: GroupQuery) -> Result<Value> {
    query.filter.validate()?;
    limit(query.limit)?;
    if query
        .after
        .as_ref()
        .is_some_and(|s| !governance::valid_text(s, 200))
    {
        return Err(Error::Invalid("invalid group cursor"));
    }
    let mut tx = begin(pool, who, Permission::UsageRead).await?;
    let (id, label) = match query.dimension {
        Dimension::Model => ("model_id", "model_name"),
        Dimension::Key => ("key_id::text", "key_alias"),
    };
    let mut q = QueryBuilder::new(format!(
        "SELECT {id} AS object_id,{label} AS label,{AGGREGATE} FROM console_request_facts"
    ));
    query.filter.apply(&mut q, who.tenant_id);
    if let Some(after) = &query.after {
        q.push(format!(" AND {id} COLLATE \"C\"> "))
            .push_bind(after);
    }
    q.push(format!(
        " GROUP BY {id},{label} ORDER BY {id} COLLATE \"C\" LIMIT "
    ))
    .push_bind(query.limit + 1);
    let mut rows = q.build_query_as::<Group>().fetch_all(&mut *tx).await?;
    let more = rows.len() > query.limit as usize;
    rows.truncate(query.limit as usize);
    let next = if more {
        rows.last().map(|r| r.object_id.clone())
    } else {
        None
    };
    let data: Vec<_> = rows
        .into_iter()
        .map(|r| json!({"object_id":r.object_id,"label":r.label,"totals":r.totals.json()}))
        .collect();
    finish(
        tx,
        pool,
        who,
        Permission::UsageRead,
        json!({"currency":"CNY","dimension":query.dimension,"data":data,"next_after":next}),
    )
    .await
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RequestCursor {
    pub created_at: DateTime<Utc>,
    pub id: Uuid,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestQuery {
    pub filter: Filter,
    pub before: Option<RequestCursor>,
    #[serde(default = "default_limit")]
    pub limit: i64,
}
#[derive(FromRow)]
struct RequestRow {
    id: Uuid,
    key_id: Uuid,
    key_alias: String,
    model_id: String,
    model_name: String,
    status: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    error_code: Option<String>,
    purpose: String,
    cost_micros: Option<i64>,
    reserved_micros: i64,
    input_tokens: Option<Decimal>,
    output_tokens: Option<Decimal>,
}
pub async fn requests(pool: &PgPool, who: &Human, query: RequestQuery) -> Result<Value> {
    query.filter.validate()?;
    limit(query.limit)?;
    let mut tx = begin(pool, who, Permission::UsageRead).await?;
    let mut q = QueryBuilder::new(
        "SELECT id,key_id,key_alias,model_id,model_name,status,created_at,updated_at,error_code,purpose,cost_micros,reserved_micros,input_tokens,output_tokens FROM console_request_facts",
    );
    query.filter.apply(&mut q, who.tenant_id);
    if let Some(c) = &query.before {
        q.push(" AND (created_at,id)<(")
            .push_bind(c.created_at)
            .push(",")
            .push_bind(c.id)
            .push(")");
    }
    q.push(" ORDER BY created_at DESC,id DESC LIMIT ")
        .push_bind(query.limit + 1);
    let mut rows = q.build_query_as::<RequestRow>().fetch_all(&mut *tx).await?;
    let more = rows.len() > query.limit as usize;
    rows.truncate(query.limit as usize);
    let next = if more {
        rows.last().map(|r| RequestCursor {
            created_at: r.created_at,
            id: r.id,
        })
    } else {
        None
    };
    let data:Vec<_>=rows.into_iter().map(|r|json!({"id":r.id,"key_id":r.key_id,"key_alias":r.key_alias,"model_id":r.model_id,"model_name":r.model_name,"status":r.status,"created_at":r.created_at,"updated_at":r.updated_at,"error_code":r.error_code,"purpose":r.purpose,"cost_cny":r.cost_micros.map(crate::money::format_micros),"reserved_cny":crate::money::format_micros(r.reserved_micros),"input_tokens":r.input_tokens.map(|v|v.to_string()),"output_tokens":r.output_tokens.map(|v|v.to_string())})).collect();
    finish(
        tx,
        pool,
        who,
        Permission::UsageRead,
        json!({"currency":"CNY","data":data,"next_before":next}),
    )
    .await
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AuditCursor {
    pub created_at: DateTime<Utc>,
    pub id: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditQuery {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub action: Option<String>,
    pub object_id: Option<String>,
    pub before: Option<AuditCursor>,
    #[serde(default = "default_limit")]
    pub limit: i64,
}
#[derive(FromRow)]
struct AuditRow {
    id: i64,
    action: String,
    object_id: String,
    created_at: DateTime<Utc>,
    database_actor: String,
}
pub async fn audit(pool: &PgPool, who: &Human, query: AuditQuery) -> Result<Value> {
    limit(query.limit)?;
    if query.start >= query.end
        || query.end - query.start > chrono::Duration::days(31)
        || [&query.action, &query.object_id]
            .iter()
            .any(|s| s.as_ref().is_some_and(|s| !governance::valid_text(s, 500)))
    {
        return Err(Error::Invalid("invalid audit filter"));
    }
    let mut tx = begin(pool, who, Permission::AuditRead).await?;
    let mut q = QueryBuilder::new(
        "SELECT id,action,object_id,created_at,database_actor FROM console_audit_facts WHERE tenant_id=",
    );
    q.push_bind(who.tenant_id)
        .push(" AND created_at>=")
        .push_bind(query.start)
        .push(" AND created_at<")
        .push_bind(query.end);
    if let Some(v) = &query.action {
        q.push(" AND action=").push_bind(v);
    }
    if let Some(v) = &query.object_id {
        q.push(" AND object_id=").push_bind(v);
    }
    if let Some(c) = &query.before {
        let id =
            c.id.parse::<i64>()
                .ok()
                .filter(|id| *id > 0)
                .ok_or(Error::Invalid("invalid audit cursor"))?;
        q.push(" AND (created_at,id)<(")
            .push_bind(c.created_at)
            .push(",")
            .push_bind(id)
            .push(")");
    }
    q.push(" ORDER BY created_at DESC,id DESC LIMIT ")
        .push_bind(query.limit + 1);
    let mut rows = q.build_query_as::<AuditRow>().fetch_all(&mut *tx).await?;
    let more = rows.len() > query.limit as usize;
    rows.truncate(query.limit as usize);
    let next = if more {
        rows.last()
            .map(|r| json!({"created_at":r.created_at,"id":r.id.to_string()}))
    } else {
        None
    };
    let data:Vec<_>=rows.into_iter().map(|r|json!({"id":r.id.to_string(),"action":r.action,"object_id":r.object_id,"created_at":r.created_at,"database_actor":r.database_actor})).collect();
    finish(
        tx,
        pool,
        who,
        Permission::AuditRead,
        json!({"data":data,"next_before":next}),
    )
    .await
}
