//! Browser-facing OIDC BFF with independent, fixed-path purpose credentials; no model keys.
mod api;
mod oidc;
pub use api::router;

use crate::{
    cipher::ResultCipher,
    error::{Error, Result},
    governance, human, store,
};
use chrono::{DateTime, Utc};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use rand::RngCore;
use rsa::{pkcs1::DecodeRsaPrivateKey, pkcs8::DecodePrivateKey, traits::PublicKeyParts};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::{sync::Arc, time::Duration};
use url::Url;
use uuid::Uuid;

struct Config {
    origin: String,
    oidc_issuer: String,
    oidc_origins: Vec<String>,
    client_id: String,
    client_secret: String,
    console_origin: String,
    audience: String,
    kid: String,
}
struct CatalogTarget {
    origin: String,
    audience: String,
    kid: String,
    signer: EncodingKey,
}
pub struct Bff {
    pool: PgPool,
    config: Config,
    config_hash: Vec<u8>,
    cipher: ResultCipher,
    signer: EncodingKey,
    catalog: Option<CatalogTarget>,
    settings: Option<CatalogTarget>,
    approvals: Option<CatalogTarget>,
    media_review: Option<CatalogTarget>,
    preview_origin: Option<String>,
    http: reqwest::Client,
}
impl Bff {
    pub fn from_env(pool: PgPool) -> Result<Arc<Self>> {
        let get = |name| std::env::var(name).map_err(|_| Error::Configuration);
        let issuer = get("BROKERROUTER_OIDC_ISSUER")?;
        let issuer_url = https_url(&issuer)?;
        let origins: Vec<String> = match std::env::var("BROKERROUTER_OIDC_ALLOWED_ORIGINS") {
            Ok(value) => serde_json::from_str(&value).map_err(|_| Error::Configuration)?,
            Err(std::env::VarError::NotPresent) => vec![issuer_url.origin().ascii_serialization()],
            Err(_) => return Err(Error::Configuration),
        };
        if origins.is_empty() || origins.len() > 8 {
            return Err(Error::Configuration);
        }
        for origin in &origins {
            https_origin(origin)?;
        }
        if !origins.contains(&issuer_url.origin().ascii_serialization()) {
            return Err(Error::Configuration);
        }
        let config = Config {
            origin: get("BROKERROUTER_BFF_ORIGIN")?,
            oidc_issuer: issuer,
            oidc_origins: origins,
            client_id: get("BROKERROUTER_OIDC_CLIENT_ID")?,
            client_secret: get("BROKERROUTER_OIDC_CLIENT_SECRET")?,
            console_origin: get("BROKERROUTER_BFF_CONSOLE_ORIGIN")?,
            audience: get("BROKERROUTER_CONSOLE_AUDIENCE")?,
            kid: get("BROKERROUTER_BFF_SIGNING_KID")?,
        };
        https_origin(&config.origin)?;
        https_origin(&config.console_origin)?;
        if !governance::valid_text(&config.client_id, 200)
            || config.client_secret.is_empty()
            || config.client_secret.len() > 4096
            || !governance::valid_text(&config.audience, 200)
            || !governance::valid_text(&config.kid, 100)
        {
            return Err(Error::Configuration);
        }
        let pem = get("BROKERROUTER_BFF_SIGNING_PRIVATE_KEY")?;
        let signer = signing_key(&pem)?;
        let catalog_names = [
            "BROKERROUTER_BFF_CATALOG_ORIGIN",
            "BROKERROUTER_CATALOG_AUDIENCE",
            "BROKERROUTER_BFF_CATALOG_SIGNING_KID",
            "BROKERROUTER_BFF_CATALOG_SIGNING_PRIVATE_KEY",
        ];
        let catalog = if catalog_names
            .iter()
            .all(|name| std::env::var_os(name).is_none())
        {
            None
        } else {
            let origin = get(catalog_names[0])?;
            https_origin(&origin)?;
            let audience = get(catalog_names[1])?;
            let kid = get(catalog_names[2])?;
            let catalog_pem = get(catalog_names[3])?;
            if !governance::valid_text(&audience, 200)
                || !governance::valid_text(&kid, 100)
                || audience == config.audience
                || private_key(&catalog_pem)?.n() == private_key(&pem)?.n()
            {
                return Err(Error::Configuration);
            }
            Some(CatalogTarget {
                origin,
                audience,
                kid,
                signer: signing_key(&catalog_pem)?,
            })
        };
        let settings_names = [
            "BROKERROUTER_BFF_SETTINGS_ORIGIN",
            "BROKERROUTER_SETTINGS_AUDIENCE",
            "BROKERROUTER_BFF_SETTINGS_SIGNING_KID",
            "BROKERROUTER_BFF_SETTINGS_SIGNING_PRIVATE_KEY",
        ];
        let settings = if settings_names
            .iter()
            .all(|name| std::env::var_os(name).is_none())
        {
            None
        } else {
            let origin = get(settings_names[0])?;
            https_origin(&origin)?;
            let audience = get(settings_names[1])?;
            let kid = get(settings_names[2])?;
            let settings_pem = get(settings_names[3])?;
            let modulus = private_key(&settings_pem)?.n().clone();
            if !governance::valid_text(&audience, 200)
                || !governance::valid_text(&kid, 100)
                || audience == config.audience
                || modulus == *private_key(&pem)?.n()
                || catalog.as_ref().is_some_and(|c| c.audience == audience)
                || (catalog.is_some() && modulus == *private_key(&get(catalog_names[3])?)?.n())
            {
                return Err(Error::Configuration);
            }
            Some(CatalogTarget {
                origin,
                audience,
                kid,
                signer: signing_key(&settings_pem)?,
            })
        };
        let approval_names = [
            "BROKERROUTER_BFF_APPROVALS_ORIGIN",
            "BROKERROUTER_APPROVALS_AUDIENCE",
            "BROKERROUTER_BFF_APPROVALS_SIGNING_KID",
            "BROKERROUTER_BFF_APPROVALS_SIGNING_PRIVATE_KEY",
        ];
        let approvals = if approval_names.iter().all(|n| std::env::var_os(n).is_none()) {
            None
        } else {
            let origin = get(approval_names[0])?;
            https_origin(&origin)?;
            let audience = get(approval_names[1])?;
            let kid = get(approval_names[2])?;
            let approval_pem = get(approval_names[3])?;
            let modulus = private_key(&approval_pem)?.n().clone();
            if !governance::valid_text(&audience, 200)
                || !governance::valid_text(&kid, 100)
                || audience == config.audience
                || modulus == *private_key(&pem)?.n()
                || catalog.as_ref().is_some_and(|c| c.audience == audience)
                || settings.as_ref().is_some_and(|c| c.audience == audience)
                || (catalog.is_some() && modulus == *private_key(&get(catalog_names[3])?)?.n())
                || (settings.is_some() && modulus == *private_key(&get(settings_names[3])?)?.n())
            {
                return Err(Error::Configuration);
            }
            Some(CatalogTarget {
                origin,
                audience,
                kid,
                signer: signing_key(&approval_pem)?,
            })
        };
        let media_names = [
            "BROKERROUTER_BFF_MEDIA_REVIEW_ORIGIN",
            "BROKERROUTER_MEDIA_REVIEW_AUDIENCE",
            "BROKERROUTER_BFF_MEDIA_REVIEW_SIGNING_KID",
            "BROKERROUTER_BFF_MEDIA_REVIEW_SIGNING_PRIVATE_KEY",
            "BROKERROUTER_BFF_MEDIA_PREVIEW_ORIGIN",
        ];
        let (media_review, preview_origin) = if media_names
            .iter()
            .all(|n| std::env::var_os(n).is_none())
        {
            (None, None)
        } else {
            let origin = get(media_names[0])?;
            https_origin(&origin)?;
            let audience = get(media_names[1])?;
            let kid = get(media_names[2])?;
            let media_pem = get(media_names[3])?;
            let preview = get(media_names[4])?;
            https_origin(&preview)?;
            let modulus = private_key(&media_pem)?.n().clone();
            if approvals.is_none()
                || !governance::valid_text(&audience, 200)
                || !governance::valid_text(&kid, 100)
                || audience == config.audience
                || modulus == *private_key(&pem)?.n()
                || [&catalog, &settings, &approvals]
                    .iter()
                    .any(|c| c.as_ref().is_some_and(|c| c.audience == audience))
                || https_url(&preview)?.host_str() == https_url(&config.origin)?.host_str()
                || (catalog.is_some() && modulus == *private_key(&get(catalog_names[3])?)?.n())
                || (settings.is_some() && modulus == *private_key(&get(settings_names[3])?)?.n())
                || (approvals.is_some() && modulus == *private_key(&get(approval_names[3])?)?.n())
            {
                return Err(Error::Configuration);
            }
            (
                Some(CatalogTarget {
                    origin,
                    audience,
                    kid,
                    signer: signing_key(&media_pem)?,
                }),
                Some(preview),
            )
        };
        let cipher = ResultCipher::new(
            get("BROKERROUTER_BFF_ACTIVE_KEY")?,
            serde_json::from_str(&get("BROKERROUTER_BFF_KEYS")?)
                .map_err(|_| Error::Configuration)?,
        )?;
        let config_hash = governance::digest(
            &json!({"origin":config.origin,"issuer":config.oidc_issuer,
            "origins":config.oidc_origins,"client":config.client_id,"console":config.console_origin,
            "audience":config.audience,"kid":config.kid,"catalog":catalog.as_ref().map(|c|json!({"origin":c.origin,"audience":c.audience,"kid":c.kid})),"settings":settings.as_ref().map(|c|json!({"origin":c.origin,"audience":c.audience,"kid":c.kid})),"approvals":approvals.as_ref().map(|c|json!({"origin":c.origin,"audience":c.audience,"kid":c.kid})),"media_review":media_review.as_ref().map(|c|json!({"origin":c.origin,"audience":c.audience,"kid":c.kid})),"preview_origin":preview_origin}),
        )?;
        Ok(Arc::new(Self {
            pool,
            config,
            config_hash,
            cipher,
            signer,
            catalog,
            settings,
            approvals,
            media_review,
            preview_origin,
            http: http_client()?,
        }))
    }
    pub(super) async fn login(
        &self,
        tenant: Uuid,
        previous_cookie: Option<&str>,
    ) -> Result<(String, String)> {
        // Check before any IdP traffic; knowing a tenant UUID does not grant membership.
        let active: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM tenants WHERE id=$1 AND active)")
                .bind(tenant)
                .fetch_one(&self.pool)
                .await?;
        if !active {
            return Err(Error::SessionUnauthorized);
        }
        let (url, state, pending) = self.authorization().await?;
        let cookie = secret();
        let id = Uuid::new_v4();
        let payload = self.cipher.encrypt(
            tenant,
            id,
            &serde_json::to_vec(&pending).map_err(|_| Error::Configuration)?,
        )?;
        let mut tx = self.pool.begin().await?;
        // Fixed advisory lock only for short DB admission, never held during network calls.
        sqlx::query("SELECT pg_advisory_xact_lock(4819247750012)")
            .execute(&mut *tx)
            .await?;
        if let Some(previous) = previous_cookie {
            sqlx::query("DELETE FROM bff_logins WHERE cookie_hash=$1")
                .bind(store::hash(previous.as_bytes()))
                .execute(&mut *tx)
                .await?;
        }
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM bff_logins WHERE expires_at>clock_timestamp()",
        )
        .fetch_one(&mut *tx)
        .await?;
        if count >= 1000 {
            return Err(Error::Busy);
        }
        sqlx::query("INSERT INTO bff_logins(id,tenant_id,cookie_hash,state_hash,config_hash,payload,created_at,expires_at) VALUES($1,$2,$3,$4,$5,$6,now(),now()+interval '5 minutes')")
            .bind(id).bind(tenant).bind(store::hash(cookie.as_bytes())).bind(store::hash(state.as_bytes()))
            .bind(&self.config_hash).bind(payload).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok((url, cookie))
    }
    pub(super) async fn callback(
        &self,
        cookie: &str,
        state: &str,
        code: Option<String>,
        previous: Option<&str>,
    ) -> Result<(String, String, i64)> {
        if state.len() > 256 {
            return Err(Error::SessionUnauthorized);
        }
        // Delete/commit BEFORE exchange. A crash or ambiguous token exchange requires a new login,
        // never replay of an authorization code. The browser cookie is independent of state.
        let row: Option<(Uuid, Uuid, Vec<u8>, DateTime<Utc>)> = sqlx::query_as("DELETE FROM bff_logins WHERE cookie_hash=$1 AND state_hash=$2 AND config_hash=$3 AND expires_at>clock_timestamp() RETURNING id,tenant_id,payload,created_at")
            .bind(store::hash(cookie.as_bytes())).bind(store::hash(state.as_bytes())).bind(&self.config_hash).fetch_optional(&self.pool).await?;
        let (id, tenant, ciphertext, started_at) = row.ok_or(Error::SessionUnauthorized)?;
        let code = code
            .filter(|v| governance::valid_text(v, 4096))
            .ok_or(Error::SessionUnauthorized)?;
        let pending = serde_json::from_slice(
            &self
                .cipher
                .decrypt(tenant, id, &ciphertext)
                .map_err(|_| Error::SessionUnauthorized)?,
        )
        .map_err(|_| Error::SessionUnauthorized)?;
        let (subject, auth_time, expires_at) = self.exchange(code, pending).await?;
        if auth_time < started_at - chrono::Duration::seconds(5) {
            return Err(Error::SessionUnauthorized);
        }
        let token = secret();
        let csrf = secret();
        let mut tx = self.pool.begin().await?;
        // Session stores the exact generation; a concurrent revoke invalidates it on every read.
        let binding: Option<(Uuid, i64, Uuid, i64, i64)> = sqlx::query_as("SELECT b.id,b.generation,b.user_id,u.bff_epoch,t.bff_epoch FROM bff_identity_bindings b JOIN governance_users u ON u.tenant_id=b.tenant_id AND u.id=b.user_id JOIN tenants t ON t.id=b.tenant_id WHERE b.tenant_id=$1 AND b.oidc_issuer=$2 AND b.oidc_subject=$3 AND b.active AND u.active AND t.active AND u.issuer=$4")
            .bind(tenant).bind(&self.config.oidc_issuer).bind(subject).bind(&self.config.origin).fetch_optional(&mut *tx).await?;
        let (binding, generation, user, user_epoch, tenant_epoch) =
            binding.ok_or(Error::SessionUnauthorized)?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
            .bind(binding.to_string())
            .execute(&mut *tx)
            .await?;
        // A successful re-login rotates and revokes the existing browser session atomically.
        if let Some(previous) = previous {
            sqlx::query("UPDATE bff_sessions SET revoked_at=COALESCE(revoked_at,clock_timestamp()) WHERE token_hash=$1")
                .bind(store::hash(previous.as_bytes())).execute(&mut *tx).await?;
        }
        // Bound active sessions per identity; old sessions are revoked, never silently reused.
        sqlx::query("UPDATE bff_sessions SET revoked_at=clock_timestamp() WHERE id IN (SELECT id FROM bff_sessions WHERE tenant_id=$1 AND binding_id=$2 AND revoked_at IS NULL ORDER BY expires_at DESC OFFSET 9)")
            .bind(tenant).bind(binding).execute(&mut *tx).await?;
        let session_id = Uuid::new_v4();
        sqlx::query("INSERT INTO bff_sessions(id,tenant_id,binding_id,binding_generation,token_hash,csrf_hash,config_hash,auth_time,expires_at,user_epoch,tenant_epoch) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)")
            .bind(session_id).bind(tenant).bind(binding).bind(generation).bind(store::hash(token.as_bytes())).bind(store::hash(csrf.as_bytes()))
            .bind(&self.config_hash).bind(auth_time).bind(expires_at).bind(user_epoch).bind(tenant_epoch).execute(&mut *tx).await?;
        audit(
            &mut tx,
            tenant,
            "bff.login",
            session_id,
            json!({"user_id":user,"binding_id":binding}),
        )
        .await?;
        tx.commit().await?;
        Ok((token, csrf, (expires_at - Utc::now()).num_seconds().max(0)))
    }
    pub(super) async fn session(&self, token: &str, csrf: Option<&str>) -> Result<Session> {
        let session: Session = sqlx::query_as("SELECT s.id,s.tenant_id,u.id AS user_id,u.subject,s.csrf_hash,s.auth_time,s.expires_at FROM bff_sessions s JOIN bff_identity_bindings b ON b.tenant_id=s.tenant_id AND b.id=s.binding_id JOIN governance_users u ON u.tenant_id=b.tenant_id AND u.id=b.user_id JOIN tenants t ON t.id=b.tenant_id WHERE s.token_hash=$1 AND s.config_hash=$2 AND s.revoked_at IS NULL AND s.expires_at>clock_timestamp() AND s.auth_time>clock_timestamp()-interval '1 hour' AND b.active AND b.generation=s.binding_generation AND b.oidc_issuer=$3 AND u.issuer=$4 AND u.active AND t.active AND u.bff_epoch=s.user_epoch AND t.bff_epoch=s.tenant_epoch")
            .bind(store::hash(token.as_bytes())).bind(&self.config_hash).bind(&self.config.oidc_issuer).bind(&self.config.origin)
            .fetch_optional(&self.pool).await?.ok_or(Error::SessionUnauthorized)?;
        if csrf.is_some_and(|v| store::hash(v.as_bytes()) != session.csrf_hash) {
            return Err(Error::CsrfRejected);
        }
        Ok(session)
    }
    pub(super) async fn logout(&self, token: &str, csrf: &str) -> Result<()> {
        let session = self.session(token, Some(csrf)).await?;
        let mut tx = self.pool.begin().await?;
        let changed = sqlx::query("UPDATE bff_sessions SET revoked_at=clock_timestamp() WHERE id=$1 AND revoked_at IS NULL")
            .bind(session.id).execute(&mut *tx).await?;
        if changed.rows_affected() == 1 {
            audit(
                &mut tx,
                session.tenant_id,
                "bff.logout",
                session.id,
                json!({"user_id":session.user_id}),
            )
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }
    pub(super) async fn query(
        &self,
        token: &str,
        csrf: &str,
        path: &str,
        body: &[u8],
    ) -> Result<(axum::http::StatusCode, Vec<u8>)> {
        let (origin, audience, kid, signer, token_type) = if matches!(
            path,
            "/catalog/v1/models/query" | "/catalog/v1/endpoints/query" | "/catalog/v1/apply"
        ) {
            let c = self.catalog.as_ref().ok_or(Error::Configuration)?;
            (&c.origin, &c.audience, &c.kid, &c.signer, "br-catalog+jwt")
        } else if matches!(
            path,
            "/media-review/v1/start" | "/media-review/v1/chunk" | "/media-review/v1/complete"
        ) {
            let c = self.media_review.as_ref().ok_or(Error::Configuration)?;
            (
                &c.origin,
                &c.audience,
                &c.kid,
                &c.signer,
                "br-media-review+jwt",
            )
        } else if matches!(
            path,
            "/approvals/v1/query"
                | "/approvals/v1/review"
                | "/approvals/v1/decide"
                | "/approvals/v1/skills/query"
                | "/approvals/v1/skills/review"
                | "/approvals/v1/skills/decide"
        ) {
            let c = self.approvals.as_ref().ok_or(Error::Configuration)?;
            (
                &c.origin,
                &c.audience,
                &c.kid,
                &c.signer,
                "br-approvals+jwt",
            )
        } else if matches!(
            path,
            "/settings/v1/keys/query"
                | "/settings/v1/keys/detail"
                | "/settings/v1/models/query"
                | "/settings/v1/fx/query"
                | "/settings/v1/guardrails/query"
                | "/settings/v1/guardrails/history"
                | "/settings/v1/policies/query"
                | "/settings/v1/policies/preview"
                | "/settings/v1/governance/targets"
                | "/settings/v1/apply"
        ) {
            let c = self.settings.as_ref().ok_or(Error::Configuration)?;
            (&c.origin, &c.audience, &c.kid, &c.signer, "br-settings+jwt")
        } else if matches!(
            path,
            "/console/v1/usage/query"
                | "/console/v1/usage/groups/query"
                | "/console/v1/requests/query"
                | "/console/v1/audit/query"
                | "/console/v1/traces/turns/query"
                | "/console/v1/traces/turns/detail"
                | "/console/v1/traces/turns/items"
                | "/console/v1/governance/monitor/query"
                | "/console/v1/governance/monitor/rails/query"
        ) {
            (
                &self.config.console_origin,
                &self.config.audience,
                &self.config.kid,
                &self.signer,
                "br-console+jwt",
            )
        } else {
            return Err(Error::NotFound);
        };
        let session = self.session(token, Some(csrf)).await?;
        let now = Utc::now().timestamp();
        let claims = human::Claims {
            tenant_id: session.tenant_id,
            iss: self.config.origin.clone(),
            sub: session.subject,
            aud: audience.clone(),
            exp: (now + 30).min(session.expires_at.timestamp()),
            nbf: now,
            iat: now,
            auth_time: session.auth_time.timestamp(),
            jti: Uuid::new_v4().to_string(),
            method: "POST".into(),
            path: path.into(),
            body_sha256: governance::hex(&store::hash(body)),
            interactive: true,
        };
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some(kid.clone());
        header.typ = Some(token_type.into());
        let proof =
            jsonwebtoken::encode(&header, &claims, signer).map_err(|_| Error::Configuration)?;
        let response = self
            .http
            .post(format!("{origin}{path}"))
            .bearer_auth(proof)
            .header("content-type", "application/json")
            .body(body.to_vec())
            .send()
            .await
            .map_err(|_| Error::BffUpstream)?;
        let status = response.status();
        if !matches!(
            status.as_u16(),
            200 | 400 | 401 | 403 | 404 | 409 | 410 | 413 | 422 | 429 | 503
        ) {
            return Err(Error::BffUpstream);
        }
        let binary = path == "/media-review/v1/chunk" && status == axum::http::StatusCode::OK;
        if binary
            && response
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                != Some("application/octet-stream")
        {
            return Err(Error::BffUpstream);
        }
        let bytes = bounded_response(response, if binary { 4194304 } else { 2097152 }).await?;
        if binary {
            if bytes.is_empty() {
                return Err(Error::BffUpstream);
            }
        } else {
            serde_json::from_slice::<Value>(&bytes).map_err(|_| Error::BffUpstream)?;
        }
        // Recheck fresh DB state after upstream work; concurrent logout must suppress delivery.
        self.session(token, Some(csrf)).await?;
        Ok((status, bytes))
    }
}

#[derive(sqlx::FromRow)]
pub(super) struct Session {
    id: Uuid,
    tenant_id: Uuid,
    user_id: Uuid,
    subject: String,
    csrf_hash: Vec<u8>,
    auth_time: DateTime<Utc>,
    expires_at: DateTime<Utc>,
}
fn https_url(value: &str) -> Result<Url> {
    let url = Url::parse(value).map_err(|_| Error::Configuration)?;
    if value.len() > 500
        || url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(Error::Configuration);
    }
    Ok(url)
}
fn signing_key(pem: &str) -> Result<EncodingKey> {
    private_key(pem)?;
    EncodingKey::from_rsa_pem(pem.as_bytes()).map_err(|_| Error::Configuration)
}
fn private_key(pem: &str) -> Result<rsa::RsaPrivateKey> {
    if pem.len() > 16384 {
        return Err(Error::Configuration);
    }
    let key = rsa::RsaPrivateKey::from_pkcs8_pem(pem)
        .or_else(|_| rsa::RsaPrivateKey::from_pkcs1_pem(pem))
        .map_err(|_| Error::Configuration)?;
    if !(2048..=8192).contains(&key.n().bits()) || key.validate().is_err() {
        return Err(Error::Configuration);
    }
    Ok(key)
}
fn https_origin(value: &str) -> Result<()> {
    if https_url(value)?.origin().ascii_serialization() != value {
        return Err(Error::Configuration);
    }
    Ok(())
}
fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(12))
        .build()
        .map_err(|_| Error::Configuration)
}
fn secret() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    governance::hex(&bytes)
}
async fn bounded_response(mut response: reqwest::Response, limit: usize) -> Result<Vec<u8>> {
    if response.content_length().is_some_and(|n| n > limit as u64) {
        return Err(Error::BffUpstream);
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| Error::BffUpstream)? {
        if bytes.len() + chunk.len() > limit {
            return Err(Error::BffUpstream);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}
async fn audit(
    tx: &mut governance::Tx<'_>,
    tenant: Uuid,
    action: &str,
    id: Uuid,
    details: Value,
) -> Result<()> {
    sqlx::query("INSERT INTO audit_events(tenant_id,action,object_id,details) VALUES($1,$2,$3,$4)")
        .bind(tenant)
        .bind(action)
        .bind(id.to_string())
        .bind(details)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Operator-only: immutable identity binding. Enabling after revocation starts a new generation.
pub async fn bind_identity(
    pool: &PgPool,
    tenant: Uuid,
    user: Uuid,
    issuer: &str,
    subject: &str,
) -> Result<Uuid> {
    https_url(issuer)?;
    if !governance::valid_text(subject, 500) {
        return Err(Error::Invalid("invalid OIDC subject"));
    }
    let mut tx = pool.begin().await?;
    let active: bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM governance_users u JOIN tenants t ON t.id=u.tenant_id WHERE u.tenant_id=$1 AND u.id=$2 AND u.active AND t.active)")
        .bind(tenant).bind(user).fetch_one(&mut *tx).await?;
    if !active {
        return Err(Error::NotFound);
    }
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO bff_identity_bindings(tenant_id,id,user_id,oidc_issuer,oidc_subject) VALUES($1,$2,$3,$4,$5)")
        .bind(tenant).bind(id).bind(user).bind(issuer).bind(subject).execute(&mut *tx).await?;
    audit(
        &mut tx,
        tenant,
        "bff.identity_bound",
        id,
        json!({"user_id":user}),
    )
    .await?;
    tx.commit().await?;
    Ok(id)
}
pub async fn set_identity_active(
    pool: &PgPool,
    tenant: Uuid,
    id: Uuid,
    active: bool,
    reason: &str,
) -> Result<()> {
    if !governance::valid_text(reason, 1000) {
        return Err(Error::Invalid("reason required"));
    }
    let mut tx = pool.begin().await?;
    let old: bool = sqlx::query_scalar(
        "SELECT active FROM bff_identity_bindings WHERE tenant_id=$1 AND id=$2 FOR UPDATE",
    )
    .bind(tenant)
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(Error::NotFound)?;
    if old != active {
        sqlx::query("UPDATE bff_identity_bindings SET active=$3,generation=generation+1 WHERE tenant_id=$1 AND id=$2")
            .bind(tenant).bind(id).bind(active).execute(&mut *tx).await?;
        audit(
            &mut tx,
            tenant,
            "bff.identity_changed",
            id,
            json!({"active":active,"reason":reason}),
        )
        .await?;
    }
    tx.commit().await?;
    Ok(())
}
pub async fn maintain(pool: &PgPool) -> Result<()> {
    sqlx::query("WITH b AS (SELECT id FROM bff_logins WHERE expires_at<=clock_timestamp() ORDER BY expires_at LIMIT 1000) DELETE FROM bff_logins s USING b WHERE s.id=b.id").execute(pool).await?;
    sqlx::query("WITH b AS (SELECT id FROM bff_sessions WHERE expires_at<=clock_timestamp() OR revoked_at IS NOT NULL ORDER BY expires_at LIMIT 1000 FOR UPDATE SKIP LOCKED) DELETE FROM bff_sessions s USING b WHERE s.id=b.id").execute(pool).await?;
    Ok(())
}

#[cfg(test)]
mod tests;
