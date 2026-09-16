//! Governance configuration only: no payloads, turn creation, grants, or model dispatch.
use crate::{
    console::Permission,
    error::{Error, Result},
    governance::{self, Guardrail, Tx},
    human::{self, Human},
    settings::{self, Change},
    store::Identity,
};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Query {
    pub after: Option<String>,
    pub search: Option<String>,
    pub limit: i64,
}
fn text(s: &str, max: usize) -> Result<()> {
    if !governance::valid_text(s, max) {
        return Err(Error::Invalid("invalid governance configuration text"));
    }
    Ok(())
}
fn validate_query(q: &Query) -> Result<()> {
    if !(1..=100).contains(&q.limit) {
        return Err(Error::Invalid("invalid page size"));
    }
    for s in [&q.after, &q.search].into_iter().flatten() {
        text(s, 200)?;
    }
    Ok(())
}
async fn page(mut tx: Tx<'_>, who: &Human, mut rows: Vec<Value>, limit: i64) -> Result<Value> {
    let more = rows.len() > limit as usize;
    rows.truncate(limit as usize);
    let next = if more {
        rows.last().map(|r| r["id"].clone())
    } else {
        None
    };
    let write = settings::can_write(&mut tx, who, Permission::GovernanceWrite).await?;
    settings::finish(
        tx,
        who,
        json!({"data":rows,"next_after":next,"can_write":write}),
    )
    .await
}
pub async fn guardrails(pool: &PgPool, who: &Human, q: Query) -> Result<Value> {
    validate_query(&q)?;
    if q.limit > 20 {
        return Err(Error::Invalid(
            "rule pages are limited to 20 bounded full rules",
        ));
    }
    let mut tx = settings::begin(pool, who, Permission::GovernanceRead, None).await?;
    let rows = sqlx::query_scalar("SELECT jsonb_build_object('id',g.id,'revision',g.config_revision::text,'version',g.version,'active',g.active,'name',v.name,'stage',v.stage,'rule',v.rule) FROM guardrails g JOIN guardrail_versions v ON v.tenant_id=g.tenant_id AND v.id=g.id AND v.version=g.version WHERE g.tenant_id=$1 AND ($2::text IS NULL OR g.id>$2) AND ($3::text IS NULL OR strpos(lower(g.id),lower($3))>0 OR strpos(lower(v.name),lower($3))>0) ORDER BY g.id LIMIT $4")
        .bind(who.tenant_id).bind(q.after).bind(q.search).bind(q.limit+1).fetch_all(&mut *tx).await?;
    page(tx, who, rows, q.limit).await
}
pub async fn policies(pool: &PgPool, who: &Human, q: Query) -> Result<Value> {
    validate_query(&q)?;
    let after = q
        .after
        .as_deref()
        .map(Uuid::parse_str)
        .transpose()
        .map_err(|_| Error::Invalid("invalid policy cursor"))?;
    let mut tx = settings::begin(pool, who, Permission::GovernanceRead, None).await?;
    let rows = sqlx::query_scalar("SELECT (to_jsonb(p)-'tenant_id'-'config_revision')||jsonb_build_object('revision',p.config_revision::text) FROM guardrail_policies p WHERE tenant_id=$1 AND ($2::uuid IS NULL OR id>$2) AND ($3::text IS NULL OR strpos(lower(guardrail_id),lower($3))>0 OR strpos(lower(model_id),lower($3))>0 OR id::text=$3 OR key_id::text=$3 OR strpos(lower(runtime_id),lower($3))>0 OR strpos(lower(tool_name),lower($3))>0) ORDER BY id LIMIT $4")
        .bind(who.tenant_id).bind(after).bind(q.search).bind(q.limit+1).fetch_all(&mut *tx).await?;
    page(tx, who, rows, q.limit).await
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct History {
    pub id: String,
    pub before: Option<i32>,
    pub limit: i64,
}
pub async fn history(pool: &PgPool, who: &Human, q: History) -> Result<Value> {
    text(&q.id, 100)?;
    if !(1..=20).contains(&q.limit) || q.before.is_some_and(|v| v < 1) {
        return Err(Error::Invalid("invalid history page"));
    }
    let mut tx = settings::begin(pool, who, Permission::GovernanceRead, None).await?;
    let mut rows:Vec<Value> = sqlx::query_scalar("SELECT jsonb_build_object('id',id,'version',version,'name',name,'stage',stage,'rule',rule) FROM guardrail_versions WHERE tenant_id=$1 AND id=$2 AND ($3::int IS NULL OR version<$3) ORDER BY version DESC LIMIT $4")
        .bind(who.tenant_id).bind(q.id).bind(q.before).bind(q.limit+1).fetch_all(&mut *tx).await?;
    let more = rows.len() > q.limit as usize;
    rows.truncate(q.limit as usize);
    let next = if more {
        rows.last().map(|r| r["version"].clone())
    } else {
        None
    };
    settings::finish(tx, who, json!({"data":rows,"next_before":next})).await
}
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetKind {
    Model,
    Key,
    Runtime,
    ScanProfile,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Targets {
    pub kind: TargetKind,
    pub after: Option<String>,
    pub search: Option<String>,
    pub limit: i64,
}
pub async fn targets(pool: &PgPool, who: &Human, q: Targets) -> Result<Value> {
    validate_query(&Query {
        after: q.after.clone(),
        search: q.search.clone(),
        limit: q.limit,
    })?;
    let mut tx = settings::begin(pool, who, Permission::GovernanceRead, None).await?;
    // Only identifiers and lifecycle hints, never key prefixes, owner identities or credentials.
    let source = match q.kind {
        TargetKind::Model => "SELECT id,name,active FROM models WHERE tenant_id=$1",
        TargetKind::Key => {
            "SELECT id::text,name,(revoked_at IS NULL AND (expires_at IS NULL OR expires_at>statement_timestamp())) AS active FROM virtual_keys WHERE tenant_id=$1"
        }
        TargetKind::Runtime => {
            "SELECT runtime_id AS id,runtime_id AS name,bool_or(active) AS active FROM runtime_bindings WHERE tenant_id=$1 GROUP BY runtime_id"
        }
        TargetKind::ScanProfile => {
            "SELECT id::text,model_id||' · v'||version::text AS name,active FROM output_scan_profiles WHERE tenant_id=$1"
        }
    };
    let sql = format!(
        "SELECT to_jsonb(t) FROM ({source}) t WHERE ($2::text IS NULL OR id>$2) AND ($3::text IS NULL OR strpos(lower(id),lower($3))>0 OR strpos(lower(name),lower($3))>0) ORDER BY id LIMIT $4"
    );
    let rows = sqlx::query_scalar(&sql)
        .bind(who.tenant_id)
        .bind(q.after)
        .bind(q.search)
        .bind(q.limit + 1)
        .fetch_all(&mut *tx)
        .await?;
    page(tx, who, rows, q.limit).await
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Preview {
    pub key_id: Uuid,
    pub model_id: Option<String>,
    pub tool_name: Option<String>,
    pub stage: String,
}
pub async fn preview(pool: &PgPool, who: &Human, q: Preview) -> Result<Value> {
    for s in [&q.model_id, &q.tool_name].into_iter().flatten() {
        text(s, 200)?;
    }
    human::validate_guardrail(&Guardrail {
        id: "preview".into(),
        version: 1,
        name: "preview".into(),
        stage: q.stage.clone(),
        rule: json!({"kind":"always"}),
    })?;
    let mut tx = settings::begin(pool, who, Permission::GovernanceRead, None).await?;
    let runtime:Option<String>=sqlx::query_scalar("SELECT b.runtime_id FROM runtime_bindings b JOIN virtual_keys k ON k.tenant_id=b.tenant_id AND k.id=b.key_id JOIN governance_users u ON u.tenant_id=b.tenant_id AND u.id=b.user_id WHERE b.tenant_id=$1 AND b.key_id=$2 AND b.active AND u.active AND k.revoked_at IS NULL AND (k.expires_at IS NULL OR k.expires_at>clock_timestamp())")
        .bind(who.tenant_id).bind(q.key_id).fetch_optional(&mut *tx).await?;
    let runtime = runtime.ok_or(Error::Invalid(
        "preview requires an active bound runtime key",
    ))?;
    let rails = governance::selected(
        &mut tx,
        &Identity {
            tenant_id: who.tenant_id,
            key_id: q.key_id,
        },
        &q.stage,
        q.model_id.as_deref(),
        q.tool_name.as_deref(),
    )
    .await?;
    let judges = rails
        .iter()
        .filter(|g| g.rule["kind"] == "model_judge")
        .count();
    let summaries:Vec<Value>=rails.into_iter().map(|r|json!({"id":r.id,"version":r.version,"name":r.name,"stage":r.stage,"kind":r.rule["kind"]})).collect();
    settings::finish(tx,who,json!({"data":summaries,"runtime_id":runtime,"judge_limit_exceeded":judges>8,"evaluated_payload":false})).await
}

async fn references(tx: &mut Tx<'_>, tenant: Uuid, rule: &Value, active: bool) -> Result<()> {
    let valid = match rule["kind"].as_str() {
        Some("model_judge") => sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM models m WHERE tenant_id=$1 AND id=$2 AND (NOT $3 OR (active AND EXISTS(SELECT 1 FROM endpoints e WHERE e.tenant_id=m.tenant_id AND e.model_id=m.id AND e.active AND e.operation='chat'))))")
            .bind(tenant).bind(rule["model"].as_str()).bind(active).fetch_one(&mut **tx).await?,
        Some("asset_scan") => sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM output_scan_profiles WHERE tenant_id=$1 AND id=$2 AND (NOT $3 OR active))")
            .bind(tenant).bind(Uuid::parse_str(rule["profile_id"].as_str().ok_or(Error::Configuration)?).map_err(|_|Error::Configuration)?).bind(active).fetch_one(&mut **tx).await?,
        _ => true,
    };
    if !valid {
        return Err(Error::Invalid("unknown or unavailable guardrail reference"));
    }
    Ok(())
}
type AuditChange = (&'static str, String, i64, Value, Value);
pub(crate) async fn apply_locked(
    tx: &mut Tx<'_>,
    who: &Human,
    change: &Change,
) -> Result<AuditChange> {
    match change {
        Change::GuardrailPublish {
            id,
            expected_revision,
            name,
            stage,
            rule,
            active,
        } => {
            human::validate_guardrail(&Guardrail {
                id: id.clone(),
                version: 1,
                name: name.clone(),
                stage: stage.clone(),
                rule: rule.clone(),
            })?;
            let old:Option<(i64,i32,Value)>=sqlx::query_as("SELECT g.config_revision,g.version,jsonb_build_object('name',v.name,'stage',v.stage,'rule',v.rule,'active',g.active,'version',g.version) FROM guardrails g JOIN guardrail_versions v ON v.tenant_id=g.tenant_id AND v.id=g.id AND v.version=g.version WHERE g.tenant_id=$1 AND g.id=$2")
                .bind(who.tenant_id).bind(id).fetch_optional(&mut **tx).await?;
            if old.as_ref().map(|r| r.0)
                != expected_revision
                    .as_deref()
                    .map(settings::revision)
                    .transpose()?
            {
                return Err(Error::ConfigConflict);
            }
            if old.is_none() && *active {
                return Err(Error::Invalid("new guardrails must be disabled"));
            }
            if old.as_ref().is_some_and(|r| r.2["stage"] != *stage) {
                return Err(Error::Invalid(
                    "stage is immutable; create a new guardrail ID",
                ));
            }
            references(tx, who.tenant_id, rule, *active).await?;
            if *active && rule["kind"] == "asset_scan" {
                let mismatch:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM guardrail_policies p JOIN output_scan_profiles s ON s.tenant_id=p.tenant_id AND s.id=$3 WHERE p.tenant_id=$1 AND p.guardrail_id=$2 AND p.active AND p.model_id IS DISTINCT FROM s.model_id)")
                    .bind(who.tenant_id).bind(id).bind(Uuid::parse_str(rule["profile_id"].as_str().ok_or(Error::Configuration)?).map_err(|_|Error::Configuration)?).fetch_one(&mut **tx).await?;
                if mismatch {
                    return Err(Error::Invalid(
                        "active policies must select the scan profile model",
                    ));
                }
            }
            let version = old
                .as_ref()
                .map_or(Some(1), |r| r.1.checked_add(1))
                .ok_or(Error::ConfigConflict)?;
            let revision:i64=sqlx::query_scalar("INSERT INTO guardrails(tenant_id,id,version,active) VALUES($1,$2,$3,$4) ON CONFLICT(tenant_id,id) DO UPDATE SET version=EXCLUDED.version,active=EXCLUDED.active RETURNING config_revision")
                .bind(who.tenant_id).bind(id).bind(version).bind(active).fetch_one(&mut **tx).await?;
            sqlx::query("INSERT INTO guardrail_versions(tenant_id,id,version,name,stage,rule) VALUES($1,$2,$3,$4,$5,$6)").bind(who.tenant_id).bind(id).bind(version).bind(name).bind(stage).bind(rule).execute(&mut **tx).await?;
            Ok((
                "settings.guardrail_published",
                id.clone(),
                revision,
                old.map_or(Value::Null, |r| r.2),
                json!({"name":name,"stage":stage,"rule":rule,"active":active,"version":version}),
            ))
        }
        Change::PolicyPut {
            id,
            expected_revision,
            guardrail_id,
            model_id,
            key_id,
            runtime_id,
            tool_name,
            active,
        } => {
            text(guardrail_id, 100)?;
            for s in [model_id, runtime_id, tool_name].into_iter().flatten() {
                text(s, 200)?;
            }
            let old:Option<(i64,Value)>=sqlx::query_as("SELECT config_revision,to_jsonb(p)-'tenant_id'-'config_revision' FROM guardrail_policies p WHERE tenant_id=$1 AND id=$2")
                .bind(who.tenant_id).bind(id).fetch_optional(&mut **tx).await?;
            if old.as_ref().map(|r| r.0)
                != expected_revision
                    .as_deref()
                    .map(settings::revision)
                    .transpose()?
            {
                return Err(Error::ConfigConflict);
            }
            if old.is_none() && *active {
                return Err(Error::Invalid("new policies must be disabled"));
            }
            let rail:Option<(String,Value,bool)>=sqlx::query_as("SELECT v.stage,v.rule,g.active FROM guardrails g JOIN guardrail_versions v ON v.tenant_id=g.tenant_id AND v.id=g.id AND v.version=g.version WHERE g.tenant_id=$1 AND g.id=$2")
                .bind(who.tenant_id).bind(guardrail_id).fetch_optional(&mut **tx).await?;
            let (stage, rule, rail_active) = rail.ok_or(Error::Invalid("unknown guardrail"))?;
            // Disabling must remain possible even after a referenced model or scanner is retired.
            if *active {
                if !rail_active {
                    return Err(Error::Invalid("enable the guardrail before its policy"));
                }
                references(tx, who.tenant_id, &rule, true).await?;
            }
            let validate_links = *active || old.is_none();
            if validate_links
                && tool_name.is_some()
                && !matches!(stage.as_str(), "tool_input" | "tool_output")
            {
                return Err(Error::Invalid("tool selector requires a tool stage"));
            }
            if validate_links && rule["kind"] == "asset_scan" {
                let profile_model: String = sqlx::query_scalar(
                    "SELECT model_id FROM output_scan_profiles WHERE tenant_id=$1 AND id=$2",
                )
                .bind(who.tenant_id)
                .bind(
                    Uuid::parse_str(rule["profile_id"].as_str().ok_or(Error::Configuration)?)
                        .map_err(|_| Error::Configuration)?,
                )
                .fetch_optional(&mut **tx)
                .await?
                .ok_or(Error::Invalid("unknown scan profile"))?;
                if model_id.as_ref() != Some(&profile_model) {
                    return Err(Error::Invalid(
                        "asset policy requires the exact scan profile model",
                    ));
                }
            }
            let known:bool=sqlx::query_scalar("SELECT ($2::text IS NULL OR EXISTS(SELECT 1 FROM models WHERE tenant_id=$1 AND id=$2 AND (NOT $5 OR active))) AND ($3::uuid IS NULL OR EXISTS(SELECT 1 FROM virtual_keys WHERE tenant_id=$1 AND id=$3 AND (NOT $5 OR (revoked_at IS NULL AND (expires_at IS NULL OR expires_at>clock_timestamp()))))) AND ($4::text IS NULL OR EXISTS(SELECT 1 FROM runtime_bindings WHERE tenant_id=$1 AND runtime_id=$4 AND ($3::uuid IS NULL OR key_id=$3) AND (NOT $5 OR active)))")
                .bind(who.tenant_id).bind(model_id).bind(key_id).bind(runtime_id).bind(active).fetch_one(&mut **tx).await?;
            if validate_links && !known {
                return Err(Error::Invalid(
                    "unknown, inactive or incompatible policy selector",
                ));
            }
            let revision:i64=sqlx::query_scalar("INSERT INTO guardrail_policies(tenant_id,id,guardrail_id,model_id,key_id,runtime_id,tool_name,active) VALUES($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT(tenant_id,id) DO UPDATE SET guardrail_id=EXCLUDED.guardrail_id,model_id=EXCLUDED.model_id,key_id=EXCLUDED.key_id,runtime_id=EXCLUDED.runtime_id,tool_name=EXCLUDED.tool_name,active=EXCLUDED.active RETURNING config_revision")
                .bind(who.tenant_id).bind(id).bind(guardrail_id).bind(model_id).bind(key_id).bind(runtime_id).bind(tool_name).bind(active).fetch_one(&mut **tx).await?;
            Ok((
                "settings.policy_updated",
                id.to_string(),
                revision,
                old.map_or(Value::Null, |r| r.1),
                json!({"guardrail_id":guardrail_id,"model_id":model_id,"key_id":key_id,"runtime_id":runtime_id,"tool_name":tool_name,"active":active}),
            ))
        }
        _ => Err(Error::Configuration),
    }
}
