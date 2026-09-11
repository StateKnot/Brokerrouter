use crate::{
    error::{Error, Result},
    money::PriceSnapshot,
};
use chrono::{DateTime, Utc};
use rand::RngCore;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{FromRow, PgPool, Postgres, Transaction};
use uuid::Uuid;

pub fn hash(value: &[u8]) -> Vec<u8> {
    Sha256::digest(value).to_vec()
}

#[derive(Clone, Debug, FromRow)]
pub struct Identity {
    pub tenant_id: Uuid,
    pub key_id: Uuid,
}

pub async fn authenticate(pool: &PgPool, token: &str) -> Result<Identity> {
    if !token.starts_with("sk_br_") || token.len() != 70 {
        return Err(Error::Unauthorized);
    }
    sqlx::query_as::<_,Identity>("SELECT k.tenant_id,k.id AS key_id FROM virtual_keys k JOIN tenants t ON t.id=k.tenant_id WHERE k.key_hash=$1 AND k.revoked_at IS NULL AND (k.expires_at IS NULL OR k.expires_at>now()) AND t.active")
        .bind(hash(token.as_bytes())).fetch_optional(pool).await?.ok_or(Error::Unauthorized)
}

async fn audit(
    tx: &mut Transaction<'_, Postgres>,
    tenant: Uuid,
    action: &str,
    object: &str,
    details: Value,
) -> Result<()> {
    sqlx::query("INSERT INTO audit_events(tenant_id,action,object_id,details) VALUES($1,$2,$3,$4)")
        .bind(tenant)
        .bind(action)
        .bind(object)
        .bind(details)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

pub async fn create_tenant(pool: &PgPool, name: &str, budget: i64) -> Result<Uuid> {
    let id = Uuid::new_v4();
    let mut tx = pool.begin().await?;
    sqlx::query("INSERT INTO tenants(id,name,budget_micros) VALUES($1,$2,$3)")
        .bind(id)
        .bind(name)
        .bind(budget)
        .execute(&mut *tx)
        .await?;
    audit(
        &mut tx,
        id,
        "tenant.created",
        &id.to_string(),
        json!({"actor":"operator_cli"}),
    )
    .await?;
    tx.commit().await?;
    Ok(id)
}

pub async fn put_model(pool: &PgPool, tenant: Uuid, id: &str, name: &str) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("INSERT INTO models(tenant_id,id,name) VALUES($1,$2,$3) ON CONFLICT(tenant_id,id) DO UPDATE SET name=EXCLUDED.name").bind(tenant).bind(id).bind(name).execute(&mut *tx).await?;
    audit(
        &mut tx,
        tenant,
        "model.updated",
        id,
        json!({"actor":"operator_cli"}),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn create_key(
    pool: &PgPool,
    tenant: Uuid,
    name: &str,
    budget: i64,
    models: &[String],
    expires: Option<DateTime<Utc>>,
) -> Result<(Uuid, String)> {
    if models.is_empty() {
        return Err(Error::Invalid("at least one model grant is required"));
    }
    if expires.is_some_and(|e| e <= Utc::now()) {
        return Err(Error::Invalid("expiry must be in the future"));
    }
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    let token = format!(
        "sk_br_{}",
        bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
    );
    let id = Uuid::new_v4();
    let mut tx = pool.begin().await?;
    sqlx::query("INSERT INTO virtual_keys(id,tenant_id,name,key_hash,key_prefix,budget_micros,expires_at) VALUES($1,$2,$3,$4,$5,$6,$7)").bind(id).bind(tenant).bind(name).bind(hash(token.as_bytes())).bind(&token[..14]).bind(budget).bind(expires).execute(&mut *tx).await?;
    for model in models {
        sqlx::query("INSERT INTO key_models(tenant_id,key_id,model_id) VALUES($1,$2,$3)")
            .bind(tenant)
            .bind(id)
            .bind(model)
            .execute(&mut *tx)
            .await?;
    }
    audit(
        &mut tx,
        tenant,
        "key.created",
        &id.to_string(),
        json!({"actor":"operator_cli","models":models}),
    )
    .await?;
    tx.commit().await?;
    Ok((id, token))
}

pub async fn revoke_key(pool: &PgPool, tenant: Uuid, id: Uuid) -> Result<()> {
    let mut tx = pool.begin().await?;
    if sqlx::query("UPDATE virtual_keys SET revoked_at=coalesce(revoked_at,now()) WHERE tenant_id=$1 AND id=$2").bind(tenant).bind(id).execute(&mut *tx).await?.rows_affected()==0 { return Err(Error::NotFound); }
    audit(
        &mut tx,
        tenant,
        "key.revoked",
        &id.to_string(),
        json!({"actor":"operator_cli"}),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn set_fx(
    pool: &PgPool,
    tenant: Uuid,
    rate: Decimal,
    effective: DateTime<Utc>,
    source: &str,
) -> Result<i64> {
    if rate <= Decimal::ZERO || rate > Decimal::from(1_000_000) || rate.scale() > 10 {
        return Err(Error::Invalid(
            "USD/CNY rate must be >0, <=1e6, with at most 10 decimal places",
        ));
    }
    if source.trim().is_empty() || source.chars().count() > 200 {
        return Err(Error::Invalid(
            "exchange-rate source must contain 1 to 200 characters",
        ));
    }
    let mut tx = pool.begin().await?;
    let id: i64=sqlx::query_scalar("INSERT INTO fx_rates(tenant_id,usd_cny,effective_at,source) VALUES($1,$2,$3,$4) RETURNING id").bind(tenant).bind(rate).bind(effective).bind(source).fetch_one(&mut *tx).await?;
    audit(
        &mut tx,
        tenant,
        "fx.published",
        &id.to_string(),
        json!({"actor":"operator_cli","usd_cny":rate.to_string(),"effective_at":effective}),
    )
    .await?;
    tx.commit().await?;
    Ok(id)
}

#[derive(Debug, Serialize, FromRow)]
pub struct FxRate {
    pub id: i64,
    #[serde(with = "rust_decimal::serde::str")]
    pub usd_cny: Decimal,
    pub source: String,
    pub effective_at: DateTime<Utc>,
}
pub async fn current_fx(pool: &PgPool, tenant: Uuid) -> Result<Option<FxRate>> {
    Ok(sqlx::query_as("SELECT id,usd_cny,source,effective_at FROM fx_rates WHERE tenant_id=$1 AND effective_at<=now() ORDER BY effective_at DESC LIMIT 1").bind(tenant).fetch_optional(pool).await?)
}

pub async fn fx_history(
    pool: &PgPool,
    tenant: Uuid,
    before: Option<i64>,
    limit: i64,
) -> Result<Vec<FxRate>> {
    if !(1..=100).contains(&limit) {
        return Err(Error::Invalid("history limit must be between 1 and 100"));
    }
    Ok(sqlx::query_as("SELECT id,usd_cny,source,effective_at FROM fx_rates WHERE tenant_id=$1 AND ($2::bigint IS NULL OR id<$2) ORDER BY id DESC LIMIT $3")
        .bind(tenant).bind(before).bind(limit).fetch_all(pool).await?)
}

#[derive(Clone, Debug, Serialize, Deserialize, FromRow)]
pub struct Endpoint {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub model_id: String,
    pub name: String,
    pub upstream_model: String,
    pub base_url: String,
    pub credential_env: String,
    pub allow_private: bool,
    pub currency: String,
    #[serde(with = "rust_decimal::serde::str")]
    pub input_per_million: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub output_per_million: Decimal,
    pub context_tokens: i32,
    pub max_output_tokens: i32,
    pub priority: i32,
    pub operation: String,
    pub supports_tools: bool,
    pub embedding_dimension: Option<i32>,
}

impl Endpoint {
    pub fn validate(&self) -> Result<()> {
        let url =
            url::Url::parse(&self.base_url).map_err(|_| Error::Invalid("invalid base URL"))?;
        if url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(Error::Invalid(
                "base URL must have a host and no credentials, query or fragment",
            ));
        }
        if url.scheme() != "https" && !(self.allow_private && url.scheme() == "http") {
            return Err(Error::Invalid(
                "HTTPS required; HTTP only for explicitly approved private endpoints",
            ));
        }
        if self.currency != "CNY" && self.currency != "USD" {
            return Err(Error::Invalid("currency must be CNY or USD"));
        }
        if self.input_per_million.is_sign_negative()
            || self.output_per_million.is_sign_negative()
            || self.context_tokens <= 0
            || self.max_output_tokens <= 0
            || self.max_output_tokens > self.context_tokens
        {
            return Err(Error::Invalid("invalid pricing or token limits"));
        }
        crate::money::nonnegative(&self.input_per_million.to_string())?;
        crate::money::nonnegative(&self.output_per_million.to_string())?;
        if !matches!(self.operation.as_str(), "chat" | "embedding")
            || (self.operation == "chat" && self.embedding_dimension.is_some())
            || (self.operation == "embedding"
                && (self.supports_tools
                    || !self
                        .embedding_dimension
                        .is_some_and(|n| (1..=65536).contains(&n))))
        {
            return Err(Error::Invalid(
                "invalid endpoint operation or embedding dimension",
            ));
        }
        Ok(())
    }
    pub async fn price(&self, pool: &PgPool) -> Result<PriceSnapshot> {
        let (rate, version) = if self.currency == "USD" {
            let fx = current_fx(pool, self.tenant_id)
                .await?
                .ok_or(Error::MissingFx)?;
            (fx.usd_cny, Some(fx.id))
        } else {
            (Decimal::ONE, None)
        };
        Ok(PriceSnapshot {
            currency: self.currency.clone(),
            input_per_million: self.input_per_million,
            output_per_million: self.output_per_million,
            cny_per_unit: rate,
            fx_version: version,
        })
    }
}

pub async fn add_endpoint(pool: &PgPool, e: &Endpoint) -> Result<()> {
    e.validate()?;
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT id FROM models WHERE tenant_id=$1 AND id=$2 FOR UPDATE")
        .bind(e.tenant_id)
        .bind(&e.model_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Error::NotFound)?;
    let conflict:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM endpoints WHERE tenant_id=$1 AND model_id=$2 AND (operation<>$3 OR embedding_dimension IS DISTINCT FROM $4))").bind(e.tenant_id).bind(&e.model_id).bind(&e.operation).bind(e.embedding_dimension).fetch_one(&mut *tx).await?;
    if conflict {
        return Err(Error::Invalid(
            "logical model operation and embedding space dimension must be consistent",
        ));
    }
    sqlx::query("INSERT INTO endpoints(id,tenant_id,model_id,name,upstream_model,base_url,credential_env,allow_private,currency,input_per_million,output_per_million,context_tokens,max_output_tokens,priority,operation,supports_tools,embedding_dimension) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17)")
        .bind(e.id).bind(e.tenant_id).bind(&e.model_id).bind(&e.name).bind(&e.upstream_model).bind(&e.base_url).bind(&e.credential_env).bind(e.allow_private).bind(&e.currency).bind(e.input_per_million).bind(e.output_per_million).bind(e.context_tokens).bind(e.max_output_tokens).bind(e.priority).bind(&e.operation).bind(e.supports_tools).bind(e.embedding_dimension).execute(&mut *tx).await?;
    audit(
        &mut tx,
        e.tenant_id,
        "endpoint.created",
        &e.id.to_string(),
        json!({"actor":"operator_cli","model":e.model_id}),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn endpoints(
    pool: &PgPool,
    identity: &Identity,
    model: &str,
    max_output: i32,
) -> Result<Vec<Endpoint>> {
    Ok(sqlx::query_as("SELECT e.id,e.tenant_id,e.model_id,e.name,e.upstream_model,e.base_url,e.credential_env,e.allow_private,e.currency,e.input_per_million,e.output_per_million,e.context_tokens,e.max_output_tokens,e.priority,e.operation,e.supports_tools,e.embedding_dimension FROM endpoints e JOIN models m ON m.tenant_id=e.tenant_id AND m.id=e.model_id JOIN key_models k ON k.tenant_id=e.tenant_id AND k.model_id=e.model_id WHERE e.tenant_id=$1 AND k.key_id=$2 AND e.model_id=$3 AND e.active AND m.active AND e.max_output_tokens>=$4 ORDER BY e.priority,e.id")
        .bind(identity.tenant_id).bind(identity.key_id).bind(model).bind(max_output).fetch_all(pool).await?)
}

pub async fn authorize_model(pool: &PgPool, who: &Identity, model: &str) -> Result<()> {
    let allowed:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM virtual_keys k JOIN tenants t ON t.id=k.tenant_id JOIN key_models g ON g.tenant_id=k.tenant_id AND g.key_id=k.id JOIN models m ON m.tenant_id=g.tenant_id AND m.id=g.model_id WHERE k.tenant_id=$1 AND k.id=$2 AND t.active AND k.revoked_at IS NULL AND (k.expires_at IS NULL OR k.expires_at>now()) AND g.model_id=$3 AND m.active)")
        .bind(who.tenant_id).bind(who.key_id).bind(model).fetch_one(pool).await?;
    if allowed {
        Ok(())
    } else {
        Err(Error::Unauthorized)
    }
}
