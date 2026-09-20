//! Human-only configuration. No provider calls, credentials, approval or budget authority.
use crate::{
    console::Permission,
    error::{Error, Result},
    governance::{self, Tx},
    human::Human,
    money, store,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

/// Operator-owned allowlist. Values are secret references, never secret values.
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Connection {
    pub tenant_id: Uuid,
    pub id: String,
    pub name: String,
    pub base_url: String,
    pub credential_env: String,
    pub allow_private: bool,
}
impl Connection {
    fn revision(&self) -> Result<String> {
        Ok(governance::hex(&governance::digest(
            &serde_json::to_value(self).map_err(|_| Error::Configuration)?,
        )?))
    }
    fn endpoint(&self) -> store::Endpoint {
        store::Endpoint {
            id: Uuid::new_v4(),
            tenant_id: self.tenant_id,
            model_id: "validation".into(),
            name: self.name.clone(),
            upstream_model: "validation".into(),
            base_url: self.base_url.clone(),
            credential_env: self.credential_env.clone(),
            allow_private: self.allow_private,
            currency: "CNY".into(),
            input_per_million: 0.into(),
            output_per_million: 0.into(),
            context_tokens: 1,
            max_output_tokens: 1,
            priority: 100,
            operation: "chat".into(),
            supports_tools: false,
            supports_streaming: false,
            embedding_dimension: None,
        }
    }
}
pub fn connections_from_env() -> Result<Vec<Connection>> {
    let text =
        std::env::var("BROKERROUTER_CATALOG_CONNECTIONS").map_err(|_| Error::Configuration)?;
    if text.len() > 1_048_576 {
        return Err(Error::Configuration);
    }
    let connections: Vec<Connection> =
        serde_json::from_str(&text).map_err(|_| Error::Configuration)?;
    validate_connections(&connections)?;
    Ok(connections)
}
pub fn validate_connections(connections: &[Connection]) -> Result<()> {
    let mut ids = std::collections::HashSet::new();
    let mut counts = std::collections::HashMap::new();
    if connections.len() > 2000 {
        return Err(Error::Configuration);
    }
    for c in connections {
        let count = counts.entry(c.tenant_id).or_insert(0);
        *count += 1;
        if *count > 100 || !governance::valid_text(&c.id, 100) || !ids.insert((c.tenant_id, &c.id))
        {
            return Err(Error::Configuration);
        }
        c.endpoint().validate().map_err(|_| Error::Configuration)?;
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelQuery {
    pub after: Option<String>,
    pub search: Option<String>,
    pub limit: i64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndpointQuery {
    pub model_id: String,
    pub after: Option<Uuid>,
    pub limit: i64,
}

async fn begin(pool: &PgPool, who: &Human, permission: Permission) -> Result<Tx<'static>> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL statement_timeout='5s'")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SET LOCAL lock_timeout='1s'")
        .execute(&mut *tx)
        .await?;
    // Serialize with permission / user / tenant revocation until this transaction commits.
    let enabled: Vec<bool> = sqlx::query_scalar("SELECT p.enabled FROM console_permissions p JOIN governance_users u ON u.tenant_id=p.tenant_id AND u.id=p.user_id JOIN tenants t ON t.id=u.tenant_id WHERE p.tenant_id=$1 AND p.user_id=$2 AND (p.permission=$3 OR ($3='catalog_write' AND p.permission='catalog_read')) AND p.enabled AND u.active AND t.active AND clock_timestamp()<to_timestamp($4::double precision) FOR SHARE OF p,u,t")
        .bind(who.tenant_id).bind(who.id).bind(permission.name()).bind(who.expires_at() as f64).fetch_all(&mut *tx).await?;
    if enabled.len()
        != if permission.name() == "catalog_write" {
            2
        } else {
            1
        }
    {
        return Err(Error::ConsoleForbidden);
    }
    Ok(tx)
}
async fn finish(mut tx: Tx<'_>, who: &Human, value: Value) -> Result<Value> {
    let fresh: bool =
        sqlx::query_scalar("SELECT clock_timestamp()<to_timestamp($1::double precision)")
            .bind(who.expires_at() as f64)
            .fetch_one(&mut *tx)
            .await?;
    if !fresh {
        return Err(Error::HumanUnauthorized);
    }
    tx.commit().await?;
    Ok(value)
}
fn text(value: &str) -> Result<()> {
    if !governance::valid_text(value, 200) {
        return Err(Error::Invalid("invalid configuration text"));
    }
    Ok(())
}
fn revision(value: &str) -> Result<i64> {
    let n = value
        .parse::<i64>()
        .map_err(|_| Error::Invalid("invalid configuration revision"))?;
    if n < 1 || n.to_string() != value {
        return Err(Error::Invalid("invalid configuration revision"));
    }
    Ok(n)
}
async fn can_write(tx: &mut Tx<'_>, who: &Human) -> Result<bool> {
    Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM console_permissions WHERE tenant_id=$1 AND user_id=$2 AND permission='catalog_write' AND enabled)").bind(who.tenant_id).bind(who.id).fetch_one(&mut **tx).await?)
}
pub async fn models(pool: &PgPool, who: &Human, q: ModelQuery) -> Result<Value> {
    if !(1..=100).contains(&q.limit) {
        return Err(Error::Invalid("invalid page size"));
    }
    for s in [&q.after, &q.search].into_iter().flatten() {
        text(s)?;
    }
    let mut tx = begin(pool, who, Permission::CatalogRead).await?;
    let mut rows:Vec<Value>=sqlx::query_scalar("SELECT jsonb_build_object('id',id,'name',name,'active',active,'revision',config_revision::text) FROM models WHERE tenant_id=$1 AND ($2::text IS NULL OR id>$2) AND ($3::text IS NULL OR strpos(lower(id),lower($3))>0 OR strpos(lower(name),lower($3))>0) ORDER BY id LIMIT $4")
        .bind(who.tenant_id).bind(&q.after).bind(&q.search).bind(q.limit+1).fetch_all(&mut *tx).await?;
    let more = rows.len() > q.limit as usize;
    rows.truncate(q.limit as usize);
    let next = if more {
        rows.last().map(|v| v["id"].clone())
    } else {
        None
    };
    let value = json!({"data":rows,"next_after":next,"can_write":can_write(&mut tx,who).await?});
    finish(tx, who, value).await
}
pub async fn endpoints(
    pool: &PgPool,
    who: &Human,
    q: EndpointQuery,
    connections: &[Connection],
) -> Result<Value> {
    text(&q.model_id)?;
    if !(1..=100).contains(&q.limit) {
        return Err(Error::Invalid("invalid page size"));
    }
    let mut tx = begin(pool, who, Permission::CatalogRead).await?;
    let model:Value=sqlx::query_scalar("SELECT jsonb_build_object('id',id,'name',name,'active',active,'revision',config_revision::text) FROM models WHERE tenant_id=$1 AND id=$2").bind(who.tenant_id).bind(&q.model_id).fetch_optional(&mut *tx).await?.ok_or(Error::NotFound)?;
    // Deliberately excludes URL, environment references and arbitrary JSON.
    let mut rows:Vec<Value>=sqlx::query_scalar("SELECT jsonb_build_object('id',id,'name',name,'model_id',model_id,'upstream_model',upstream_model,'active',active,'revision',config_revision::text,'currency',currency,'input_per_million',input_per_million::text,'output_per_million',output_per_million::text,'context_tokens',context_tokens,'max_output_tokens',max_output_tokens,'priority',priority,'operation',operation,'supports_tools',supports_tools,'supports_streaming',supports_streaming,'embedding_dimension',embedding_dimension,'connection',(SELECT jsonb_build_object('id',c.connection_id,'name',c.connection_name,'revision',c.connection_revision) FROM catalog_endpoint_connections c WHERE c.tenant_id=endpoints.tenant_id AND c.endpoint_id=endpoints.id)) FROM endpoints WHERE tenant_id=$1 AND model_id=$2 AND ($3::uuid IS NULL OR id>$3) ORDER BY id LIMIT $4")
        .bind(who.tenant_id).bind(&q.model_id).bind(q.after).bind(q.limit+1).fetch_all(&mut *tx).await?;
    let more = rows.len() > q.limit as usize;
    rows.truncate(q.limit as usize);
    let next = if more {
        rows.last().map(|v| v["id"].clone())
    } else {
        None
    };
    let connections: Vec<Value> = connections
        .iter()
        .filter(|c| c.tenant_id == who.tenant_id)
        .map(|c| Ok(json!({"id":c.id,"name":c.name,"revision":c.revision()?})))
        .collect::<Result<_>>()?;
    let value = json!({"model":model,"data":rows,"next_after":next,"connections":connections,"can_write":can_write(&mut tx,who).await?});
    finish(tx, who, value).await
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Mutation {
    pub operation_id: Uuid,
    pub reason: String,
    pub change: Change,
}
#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Change {
    ModelCreate {
        id: String,
        name: String,
    },
    ModelUpdate {
        id: String,
        expected_revision: String,
        name: String,
        active: bool,
    },
    EndpointCreate {
        model_id: String,
        connection_id: String,
        connection_revision: String,
        name: String,
        upstream_model: String,
        currency: String,
        input_per_million: String,
        output_per_million: String,
        context_tokens: i32,
        max_output_tokens: i32,
        priority: i32,
        operation: String,
        supports_tools: bool,
        supports_streaming: bool,
        embedding_dimension: Option<i32>,
    },
    EndpointUpdate {
        id: Uuid,
        expected_revision: String,
        name: String,
        active: bool,
        priority: i32,
        currency: String,
        input_per_million: String,
        output_per_million: String,
    },
}

pub async fn apply(
    pool: &PgPool,
    who: &Human,
    mutation: Mutation,
    connections: &[Connection],
) -> Result<Value> {
    if !governance::valid_text(&mutation.reason, 500) {
        return Err(Error::Invalid("configuration change reason required"));
    }
    // Write permission never implicitly reveals the catalog; both grants are explicit.
    let mut tx = begin(pool, who, Permission::CatalogWrite).await?;
    let hash = governance::digest(
        &serde_json::to_value(&mutation).map_err(|_| Error::Invalid("invalid mutation"))?,
    )?;
    // One short DB lock, no network calls or upstream retries inside configuration transactions.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
        .bind(format!(
            "catalog:{}:{}:{}",
            who.tenant_id, who.id, mutation.operation_id
        ))
        .execute(&mut *tx)
        .await?;
    let previous:Option<(Vec<u8>,Value)>=sqlx::query_as("SELECT body_hash,response FROM catalog_mutations WHERE tenant_id=$1 AND user_id=$2 AND operation_id=$3").bind(who.tenant_id).bind(who.id).bind(mutation.operation_id).fetch_optional(&mut *tx).await?;
    if let Some((old, result)) = previous {
        if old != hash {
            return Err(Error::Conflict);
        }
        return finish(tx, who, result).await;
    }
    let (action, object_id, version, before, after) = match &mutation.change {
        Change::ModelCreate { id, name } => {
            text(id)?;
            text(name)?;
            let inserted=sqlx::query("INSERT INTO models(tenant_id,id,name,active) VALUES($1,$2,$3,false) ON CONFLICT DO NOTHING").bind(who.tenant_id).bind(id).bind(name).execute(&mut *tx).await?;
            if inserted.rows_affected() != 1 {
                return Err(Error::ConfigConflict);
            }
            (
                "catalog.model_created",
                id.clone(),
                1,
                Value::Null,
                json!({"name":name,"active":false}),
            )
        }
        Change::ModelUpdate {
            id,
            expected_revision,
            name,
            active,
        } => {
            text(id)?;
            text(name)?;
            let old:Option<(String,bool,i64)>=sqlx::query_as("SELECT name,active,config_revision FROM models WHERE tenant_id=$1 AND id=$2 FOR UPDATE").bind(who.tenant_id).bind(id).fetch_optional(&mut *tx).await?;
            let (old_name, old_active, v) = old.ok_or(Error::NotFound)?;
            if v != revision(expected_revision)? {
                return Err(Error::ConfigConflict);
            }
            let version:i64=sqlx::query_scalar("UPDATE models SET name=$3,active=$4 WHERE tenant_id=$1 AND id=$2 RETURNING config_revision").bind(who.tenant_id).bind(id).bind(name).bind(active).fetch_one(&mut *tx).await?;
            (
                "catalog.model_updated",
                id.clone(),
                version,
                json!({"name":old_name,"active":old_active,"revision":v.to_string()}),
                json!({"name":name,"active":active}),
            )
        }
        Change::EndpointCreate {
            model_id,
            connection_id,
            connection_revision,
            name,
            upstream_model,
            currency,
            input_per_million,
            output_per_million,
            context_tokens,
            max_output_tokens,
            priority,
            operation,
            supports_tools,
            supports_streaming,
            embedding_dimension,
        } => {
            let c = connections
                .iter()
                .find(|c| c.tenant_id == who.tenant_id && c.id == *connection_id)
                .ok_or(Error::NotFound)?;
            if c.revision()? != *connection_revision {
                return Err(Error::ConfigConflict);
            }
            let mut e = c.endpoint();
            e.model_id = model_id.clone();
            e.name = name.clone();
            e.upstream_model = upstream_model.clone();
            e.currency = currency.clone();
            e.input_per_million = money::nonnegative(input_per_million)?;
            e.output_per_million = money::nonnegative(output_per_million)?;
            e.context_tokens = *context_tokens;
            e.max_output_tokens = *max_output_tokens;
            e.priority = *priority;
            e.operation = operation.clone();
            e.supports_tools = *supports_tools;
            e.supports_streaming = *supports_streaming;
            e.embedding_dimension = *embedding_dimension;
            store::insert_endpoint(&mut tx, &e).await?;
            sqlx::query("INSERT INTO catalog_endpoint_connections(tenant_id,endpoint_id,connection_id,connection_name,connection_revision) VALUES($1,$2,$3,$4,$5)").bind(who.tenant_id).bind(e.id).bind(&c.id).bind(&c.name).bind(connection_revision).execute(&mut *tx).await?;
            // Draft endpoints cannot receive traffic until a separate reviewed activation.
            let version:i64=sqlx::query_scalar("UPDATE endpoints SET active=false WHERE tenant_id=$1 AND id=$2 RETURNING config_revision").bind(who.tenant_id).bind(e.id).fetch_one(&mut *tx).await?;
            (
                "catalog.endpoint_created",
                e.id.to_string(),
                version,
                Value::Null,
                json!({"model_id":model_id,"connection_id":connection_id,"connection_revision":connection_revision,"name":name,"upstream_model":upstream_model,"currency":currency,"input_per_million":input_per_million,"output_per_million":output_per_million,"context_tokens":context_tokens,"max_output_tokens":max_output_tokens,"priority":priority,"operation":operation,"supports_tools":supports_tools,"supports_streaming":supports_streaming,"embedding_dimension":embedding_dimension,"active":false}),
            )
        }
        Change::EndpointUpdate {
            id,
            expected_revision,
            name,
            active,
            priority,
            currency,
            input_per_million,
            output_per_million,
        } => {
            text(name)?;
            if *priority < 0 || !matches!(currency.as_str(), "CNY" | "USD") {
                return Err(Error::Invalid("invalid price currency or priority"));
            }
            let input = money::nonnegative(input_per_million)?;
            let output = money::nonnegative(output_per_million)?;
            let old:Option<(i64,Value)>=sqlx::query_as("SELECT config_revision,jsonb_build_object('name',name,'active',active,'priority',priority,'currency',currency,'input_per_million',input_per_million::text,'output_per_million',output_per_million::text,'revision',config_revision::text) FROM endpoints WHERE tenant_id=$1 AND id=$2 FOR UPDATE").bind(who.tenant_id).bind(id).fetch_optional(&mut *tx).await?;
            let (v, before) = old.ok_or(Error::NotFound)?;
            if v != revision(expected_revision)? {
                return Err(Error::ConfigConflict);
            }
            let version:i64=sqlx::query_scalar("UPDATE endpoints SET name=$3,active=$4,priority=$5,currency=$6,input_per_million=$7,output_per_million=$8 WHERE tenant_id=$1 AND id=$2 RETURNING config_revision").bind(who.tenant_id).bind(id).bind(name).bind(active).bind(priority).bind(currency).bind(input).bind(output).fetch_one(&mut *tx).await?;
            (
                "catalog.endpoint_updated",
                id.to_string(),
                version,
                before,
                json!({"name":name,"active":active,"priority":priority,"currency":currency,"input_per_million":input_per_million,"output_per_million":output_per_million}),
            )
        }
    };
    let response = json!({"operation_id":mutation.operation_id,"object_id":object_id,"revision":version.to_string()});
    sqlx::query("INSERT INTO audit_events(tenant_id,action,object_id,details) VALUES($1,$2,$3,$4)").bind(who.tenant_id).bind(action).bind(&object_id).bind(json!({"actor":"human_catalog","user_id":who.id,"operation_id":mutation.operation_id,"reason":mutation.reason,"before":before,"after":after,"revision":version.to_string()})).execute(&mut *tx).await?;
    sqlx::query("INSERT INTO catalog_mutations(tenant_id,user_id,operation_id,body_hash,response) VALUES($1,$2,$3,$4,$5)").bind(who.tenant_id).bind(who.id).bind(mutation.operation_id).bind(hash).bind(&response).execute(&mut *tx).await?;
    finish(tx, who, response).await
}
