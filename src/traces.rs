//! Read-only execution metadata. Never decrypt, approve, acknowledge or resume.
use crate::{
    console::{self, Permission},
    error::{Error, Result},
    governance,
    human::Human,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Filter {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub user_id: Option<Uuid>,
    pub key_id: Option<Uuid>,
    pub turn_id: Option<Uuid>,
    pub runtime_id: Option<String>,
    pub agent_id: Option<String>,
    pub session_id: Option<String>,
    pub run_id: Option<String>,
    pub state: Option<String>,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TimeCursor {
    pub created_at: DateTime<Utc>,
    pub id: Uuid,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnsQuery {
    pub filter: Filter,
    pub before: Option<TimeCursor>,
    pub limit: i64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnQuery {
    pub turn_id: Uuid,
}
fn limit(value: i64) -> Result<()> {
    if (1..=50).contains(&value) {
        Ok(())
    } else {
        Err(Error::Invalid("trace page limit must be 1..50"))
    }
}
const TURN_FIELDS: &str = "id,key_id,user_id,session_id,run_id,generation,recorded_state,state,created_at,expires_at,event_clock::text,acknowledged_sequence::text,runtime_id,agent_id,key_alias,identity_active";
pub async fn turns(pool: &PgPool, who: &Human, q: TurnsQuery) -> Result<Value> {
    limit(q.limit)?;
    let f = &q.filter;
    if f.start >= f.end
        || f.end - f.start > chrono::Duration::days(31)
        || [&f.runtime_id, &f.agent_id, &f.session_id, &f.run_id]
            .iter()
            .any(|v| v.as_ref().is_some_and(|v| !governance::valid_text(v, 200)))
        || f.state.as_deref().is_some_and(|s| {
            !matches!(
                s,
                "active" | "expired" | "completed" | "cancelled" | "failed"
            )
        })
    {
        return Err(Error::Invalid("invalid trace filter; maximum 31 days"));
    }
    let mut tx = console::begin(pool, who, Permission::TracesRead).await?;
    let observed: DateTime<Utc> = sqlx::query_scalar("SELECT transaction_timestamp()")
        .fetch_one(&mut *tx)
        .await?;
    let mut sql = QueryBuilder::<Postgres>::new(format!(
        "SELECT to_jsonb(v) FROM (SELECT {TURN_FIELDS} FROM console_trace_turns WHERE tenant_id="
    ));
    sql.push_bind(who.tenant_id)
        .push(" AND created_at>=")
        .push_bind(f.start)
        .push(" AND created_at<")
        .push_bind(f.end);
    for (name, value) in [
        ("user_id", f.user_id),
        ("key_id", f.key_id),
        ("id", f.turn_id),
    ] {
        if let Some(v) = value {
            sql.push(format!(" AND {name}=")).push_bind(v);
        }
    }
    for (name, value) in [
        ("runtime_id", &f.runtime_id),
        ("agent_id", &f.agent_id),
        ("session_id", &f.session_id),
        ("run_id", &f.run_id),
        ("state", &f.state),
    ] {
        if let Some(v) = value {
            sql.push(format!(" AND {name}=")).push_bind(v);
        }
    }
    if let Some(c) = &q.before {
        time_cursor(&mut sql, c);
    }
    sql.push(" ORDER BY created_at DESC,id DESC LIMIT ")
        .push_bind(q.limit + 1)
        .push(") v");
    let mut data: Vec<Value> = sql.build_query_scalar().fetch_all(&mut *tx).await?;
    let next = paginate(
        &mut data,
        q.limit,
        |r| json!({"created_at":r["created_at"],"id":r["id"]}),
    );
    console::finish(
        tx,
        pool,
        who,
        Permission::TracesRead,
        json!({"observed_at":observed,"data":data,"next_before":next}),
    )
    .await
}
fn time_cursor<'a>(sql: &mut QueryBuilder<'a, Postgres>, c: &TimeCursor) {
    sql.push(" AND (created_at,id)<(")
        .push_bind(c.created_at)
        .push(",")
        .push_bind(c.id)
        .push(")");
}
fn paginate(
    data: &mut Vec<Value>,
    limit: i64,
    cursor: impl FnOnce(&Value) -> Value,
) -> Option<Value> {
    let more = data.len() > limit as usize;
    data.truncate(limit as usize);
    if more { data.last().map(cursor) } else { None }
}
async fn owned_turn(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    tenant: Uuid,
    turn: Uuid,
) -> Result<Value> {
    sqlx::query_scalar(&format!("SELECT to_jsonb(v) FROM (SELECT {TURN_FIELDS} FROM console_trace_turns WHERE tenant_id=$1 AND id=$2) v"))
        .bind(tenant).bind(turn).fetch_optional(&mut **tx).await?.ok_or(Error::NotFound)
}
pub async fn detail(pool: &PgPool, who: &Human, q: TurnQuery) -> Result<Value> {
    let mut tx = console::begin(pool, who, Permission::TracesRead).await?;
    let turn = owned_turn(&mut tx, who.tenant_id, q.turn_id).await?;
    let observed: DateTime<Utc> = sqlx::query_scalar("SELECT transaction_timestamp()")
        .fetch_one(&mut *tx)
        .await?;
    let costs:Vec<Value>=sqlx::query_scalar("SELECT to_jsonb(v) FROM (SELECT purpose,count(*)::text AS requests,count(cost_micros)::text AS known_cost_requests,(count(*)-count(cost_micros))::text AS unsettled_cost_requests,round(COALESCE(sum(cost_micros),0)/1000000,6)::text AS known_cost_cny,round(COALESCE(sum(reserved_micros),0)/1000000,6)::text AS reserved_cny,count(input_tokens)::text AS token_usage_requests,COALESCE(sum(input_tokens),0)::text AS input_tokens,COALESCE(sum(output_tokens),0)::text AS output_tokens FROM console_trace_requests WHERE tenant_id=$1 AND turn_id=$2 GROUP BY purpose ORDER BY purpose) v")
        .bind(who.tenant_id).bind(q.turn_id).fetch_all(&mut *tx).await?;
    let states:Vec<Value>=sqlx::query_scalar("SELECT to_jsonb(v) FROM (SELECT state,count(*)::text AS count FROM console_trace_nodes WHERE tenant_id=$1 AND turn_id=$2 GROUP BY state ORDER BY state) v")
        .bind(who.tenant_id).bind(q.turn_id).fetch_all(&mut *tx).await?;
    let approvals:Vec<Value>=sqlx::query_scalar("SELECT to_jsonb(v) FROM (SELECT state,count(*)::text AS count FROM console_trace_approvals WHERE tenant_id=$1 AND turn_id=$2 GROUP BY state ORDER BY state) v")
        .bind(who.tenant_id).bind(q.turn_id).fetch_all(&mut *tx).await?;
    console::finish(tx,pool,who,Permission::TracesRead,json!({"observed_at":observed,"currency":"CNY","cost_basis":"configured_price_ledger_not_provider_invoice","turn":turn,"costs":costs,"nodes":states,"approvals":approvals})).await
}

#[derive(Deserialize, Serialize, Clone, Copy, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Section {
    Nodes,
    Requests,
    Approvals,
    Events,
    Checks,
}
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Cursor {
    Time { created_at: DateTime<Utc>, id: Uuid },
    Event { sequence: String, through: String },
    Check { guardrail_id: String, version: i32 },
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ItemsQuery {
    pub turn_id: Uuid,
    pub section: Section,
    pub operation_id: Option<Uuid>,
    pub after: Option<Cursor>,
    pub limit: i64,
}
fn sequence(s: &str) -> Result<i64> {
    s.parse::<i64>()
        .ok()
        .filter(|v| *v >= 0 && v.to_string() == s)
        .ok_or(Error::Invalid("invalid trace event cursor"))
}
pub async fn items(pool: &PgPool, who: &Human, q: ItemsQuery) -> Result<Value> {
    limit(q.limit)?;
    if q.section == Section::Approvals && q.limit > 5 {
        return Err(Error::Invalid("approval trace page limit must be 1..5"));
    }
    if (q.section == Section::Checks && q.operation_id.is_none())
        || (q.section == Section::Nodes && q.operation_id.is_some())
    {
        return Err(Error::Invalid("invalid trace operation filter"));
    }
    let mut tx = console::begin(pool, who, Permission::TracesRead).await?;
    let turn = owned_turn(&mut tx, who.tenant_id, q.turn_id).await?;
    if let Some(id) = q.operation_id {
        let exists:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM console_trace_nodes WHERE tenant_id=$1 AND turn_id=$2 AND id=$3)")
            .bind(who.tenant_id).bind(q.turn_id).bind(id).fetch_one(&mut *tx).await?;
        if !exists {
            return Err(Error::NotFound);
        }
    }
    let observed: DateTime<Utc> = sqlx::query_scalar("SELECT transaction_timestamp()")
        .fetch_one(&mut *tx)
        .await?;
    let (fields, source) = match q.section {
        Section::Nodes => (
            "id,turn_id,stage,model_id,tool_name,created_at,execution_deadline,recorded_state,state,operation_sha256",
            "console_trace_nodes",
        ),
        Section::Requests => (
            "id,turn_id,model_id,model_name,purpose,status,created_at,updated_at,error_code,round(cost_micros::numeric/1000000,6)::text AS cost_cny,round(reserved_micros::numeric/1000000,6)::text AS reserved_cny,input_tokens::text,output_tokens::text,judge_operation_id,media_state,media_endpoint_id,cancel_requested,dispatch_deadline,result_expires_at",
            "console_trace_requests",
        ),
        Section::Approvals => (
            "id,turn_id,operation_id,recorded_state,state,created_at,expires_at,decided_at,decided_by",
            "console_trace_approvals",
        ),
        Section::Events => (
            "sequence::text,created_at,event_type,operation_id,approval_id,reconciliation",
            "console_trace_events",
        ),
        Section::Checks => (
            "guardrail_id,version,name,stage,kind AS rule_kind,outcome,current_active,created_at,approval_skipped_at,source,request_id,judge_deadline",
            "console_guardrail_facts",
        ),
    };
    let mut sql = QueryBuilder::<Postgres>::new(format!(
        "SELECT to_jsonb(v) FROM (SELECT {fields} FROM {source} WHERE tenant_id="
    ));
    sql.push_bind(who.tenant_id)
        .push(" AND turn_id=")
        .push_bind(q.turn_id);
    if let Some(id) = q.operation_id {
        if q.section == Section::Requests {
            sql.push(" AND EXISTS(SELECT 1 FROM console_trace_links l WHERE l.tenant_id=console_trace_requests.tenant_id AND l.request_id=console_trace_requests.id AND l.turn_id=console_trace_requests.turn_id AND l.operation_id=").push_bind(id).push(")");
        } else {
            sql.push(" AND operation_id=").push_bind(id);
        }
    }
    let mut fence = sequence(turn["event_clock"].as_str().ok_or(Error::Configuration)?)?;
    match (&q.section, &q.after) {
        (
            Section::Events,
            Some(Cursor::Event {
                sequence: s,
                through,
            }),
        ) => {
            let after = sequence(s)?;
            let upper = sequence(through)?;
            if after > upper || upper > fence {
                return Err(Error::Invalid("event cursor is ahead of committed events"));
            }
            fence = upper;
            sql.push(" AND sequence>").push_bind(after);
        }
        (
            Section::Checks,
            Some(Cursor::Check {
                guardrail_id,
                version,
            }),
        ) => {
            if !governance::valid_text(guardrail_id, 100) || *version < 1 {
                return Err(Error::Invalid("invalid check cursor"));
            }
            sql.push(" AND (guardrail_id COLLATE \"C\",version)>(")
                .push_bind(guardrail_id)
                .push(",")
                .push_bind(*version)
                .push(")");
        }
        (
            Section::Nodes | Section::Requests | Section::Approvals,
            Some(Cursor::Time { created_at, id }),
        ) => time_cursor(
            &mut sql,
            &TimeCursor {
                created_at: *created_at,
                id: *id,
            },
        ),
        (_, None) => {}
        _ => return Err(Error::Invalid("cursor does not match trace section")),
    }
    match q.section {
        Section::Events => {
            sql.push(" AND sequence<=")
                .push_bind(fence)
                .push(" ORDER BY sequence");
        }
        Section::Checks => {
            sql.push(" ORDER BY guardrail_id COLLATE \"C\",version");
        }
        _ => {
            sql.push(" ORDER BY created_at DESC,id DESC");
        }
    }
    sql.push(" LIMIT ").push_bind(q.limit + 1).push(") v");
    let mut data: Vec<Value> = sql.build_query_scalar().fetch_all(&mut *tx).await?;
    let next = paginate(&mut data, q.limit, |r| match q.section {
        Section::Events => {
            json!({"kind":"event","sequence":r["sequence"],"through":fence.to_string()})
        }
        Section::Checks => {
            json!({"kind":"check","guardrail_id":r["guardrail_id"],"version":r["version"]})
        }
        _ => json!({"kind":"time","created_at":r["created_at"],"id":r["id"]}),
    });
    // One bounded child query, never one database round-trip per row.
    if matches!(q.section, Section::Requests | Section::Approvals) && !data.is_empty() {
        let ids: Vec<Uuid> = data
            .iter()
            .map(|r| serde_json::from_value(r["id"].clone()).map_err(|_| Error::Configuration))
            .collect::<Result<_>>()?;
        let (query, property, maximum) = if q.section == Section::Requests {
            (
                "SELECT request_id,to_jsonb(v)-'request_id' FROM (SELECT request_id,id,endpoint_id,endpoint_name,ordinal,status,http_status,error_code,round(cost_micros::numeric/1000000,6)::text AS cost_cny,created_at,finished_at FROM console_trace_attempts WHERE tenant_id=$1 AND request_id=ANY($2) ORDER BY request_id,ordinal LIMIT 151) v",
                "attempts",
                3,
            )
        } else {
            (
                "SELECT approval_id,to_jsonb(v)-'approval_id' FROM (SELECT approval_id,guardrail_id,version,expires_at FROM console_trace_grants WHERE tenant_id=$1 AND approval_id=ANY($2) ORDER BY approval_id,guardrail_id COLLATE \"C\",version LIMIT 1281) v",
                "grants",
                256,
            )
        };
        let rows: Vec<(Uuid, Value)> = sqlx::query_as(query)
            .bind(who.tenant_id)
            .bind(&ids)
            .fetch_all(&mut *tx)
            .await?;
        let mut children = std::collections::HashMap::<Uuid, Vec<Value>>::new();
        for (id, row) in rows {
            let group = children.entry(id).or_default();
            group.push(row);
            if group.len() > maximum {
                return Err(Error::Configuration);
            }
        }
        for (row, id) in data.iter_mut().zip(ids) {
            row[property] = json!(children.remove(&id).unwrap_or_default());
        }
    }
    for row in &mut data {
        row["kind"] = json!(q.section);
    }
    console::finish(tx,pool,who,Permission::TracesRead,json!({"section":q.section,"observed_at":observed,"data":data,"next_after":next,"event_fence":if q.section==Section::Events {Some(fence.to_string())}else{None}})).await
}
