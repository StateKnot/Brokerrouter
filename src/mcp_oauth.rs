//! OAuth 2.1 protected-resource authentication for the downstream MCP edge.
//!
//! Authorization-server locations and JWKS endpoints are deployment-owned
//! configuration. Untrusted token headers can select only a preconfigured
//! issuer and key id; `jku`, embedded `jwk` and other remote-key indirections
//! are rejected. JWKS downloads use the gateway's DNS/IP-pinned egress client.

use crate::{egress, error::Error, store::Identity};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::Utc;
use futures_util::StreamExt;
use jsonwebtoken::{
    Algorithm, DecodingKey, Validation, decode, decode_header,
    jwk::{AlgorithmParameters, JwkSet, KeyAlgorithm, KeyOperations, PublicKeyUse},
};
use reqwest::header::{CACHE_CONTROL, CONTENT_LENGTH, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::{
    collections::{HashMap, HashSet},
    io::Read,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, RwLock};
use url::Url;
use uuid::Uuid;

pub const READ_SCOPE: &str = "mcp:tools:read";
pub const CALL_SCOPE: &str = "mcp:tools:call";
const MAX_TOKEN_BYTES: usize = 16 * 1024;
const MAX_JWKS_BYTES: usize = 512 * 1024;
const MAX_JWKS_KEYS: usize = 32;
const MAX_CA_BUNDLE_BYTES: usize = 1024 * 1024;
const DEFAULT_JWKS_TTL: Duration = Duration::from_secs(300);
const MAX_JWKS_TTL: Duration = Duration::from_secs(900);
const UNKNOWN_KID_REFRESH_INTERVAL: Duration = Duration::from_secs(10);
const JWKS_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IssuerConfig {
    pub issuer: String,
    /// HTTPS JWKS endpoint. Exactly one of `jwks_uri` or `jwks` is required.
    #[serde(default)]
    pub jwks_uri: Option<String>,
    /// Deployment-pinned public JWKS for air-gapped installations. Rotation is
    /// performed by an atomic config rollout/restart; no private key belongs here.
    #[serde(default)]
    pub jwks: Option<JwkSet>,
    #[serde(default)]
    pub allow_private: bool,
}

#[derive(Clone)]
pub struct Verifier {
    inner: Arc<Inner>,
}

struct Inner {
    resource: String,
    resource_authority: String,
    metadata_url: String,
    metadata_path: String,
    metadata: Value,
    issuers: HashMap<String, Arc<Issuer>>,
    max_token_seconds: i64,
}

struct Issuer {
    name: String,
    jwks_uri: Option<Url>,
    allow_private: bool,
    static_keys: bool,
    roots: Arc<Vec<reqwest::Certificate>>,
    cache: RwLock<KeyCache>,
    refresh: Mutex<()>,
}

#[derive(Clone)]
struct CachedKey {
    algorithm: Algorithm,
    decoding: DecodingKey,
}

struct KeyCache {
    keys: HashMap<String, CachedKey>,
    expires_at: Instant,
    last_unknown_refresh: Option<Instant>,
}

#[derive(Debug)]
pub enum VerifyError {
    Invalid,
    Unavailable,
}

#[derive(Clone, Debug)]
pub struct Access {
    pub tenant_id: Uuid,
    pub issuer: String,
    pub subject: String,
    pub scopes: HashSet<String>,
}

#[derive(Deserialize)]
struct UnverifiedIssuer {
    iss: String,
}

#[derive(Deserialize)]
struct Claims {
    tenant_id: Uuid,
    iss: String,
    sub: String,
    aud: Value,
    exp: i64,
    nbf: i64,
    iat: i64,
    scope: String,
}

#[derive(Serialize)]
struct ProtectedResourceMetadata<'a> {
    resource: &'a str,
    resource_name: &'static str,
    authorization_servers: Vec<&'a str>,
    scopes_supported: [&'static str; 2],
    bearer_methods_supported: [&'static str; 1],
}

impl Verifier {
    pub async fn optional_from_env() -> Result<Option<Self>, Error> {
        let resource = std::env::var("BROKERROUTER_MCP_RESOURCE").ok();
        let issuers = std::env::var("BROKERROUTER_MCP_OAUTH_ISSUERS").ok();
        match (resource, issuers) {
            (None, None) => Ok(None),
            (Some(resource), Some(issuers)) => {
                let issuers: Vec<IssuerConfig> =
                    serde_json::from_str(&issuers).map_err(|_| Error::Configuration)?;
                let max_token_seconds = std::env::var("BROKERROUTER_MCP_OAUTH_MAX_TOKEN_SECONDS")
                    .ok()
                    .map(|value| value.parse::<i64>().map_err(|_| Error::Configuration))
                    .transpose()?
                    .unwrap_or(3600);
                Self::build(
                    &resource,
                    issuers,
                    max_token_seconds,
                    Arc::new(tls_roots_from_env()?),
                )
                .await
                .map(Some)
            }
            _ => Err(Error::Configuration),
        }
    }

    pub async fn new(
        resource: &str,
        configs: Vec<IssuerConfig>,
        max_token_seconds: i64,
    ) -> Result<Self, Error> {
        Self::build(resource, configs, max_token_seconds, Arc::new(Vec::new())).await
    }

    async fn build(
        resource: &str,
        configs: Vec<IssuerConfig>,
        max_token_seconds: i64,
        roots: Arc<Vec<reqwest::Certificate>>,
    ) -> Result<Self, Error> {
        let resource_url = canonical_resource(resource)?;
        if configs.is_empty() || configs.len() > 8 || !(60..=86_400).contains(&max_token_seconds) {
            return Err(Error::Configuration);
        }
        let resource_authority = resource_url
            .host_str()
            .map(|host| match resource_url.port() {
                Some(port) => format!("{}:{port}", host.to_ascii_lowercase()),
                None => host.to_ascii_lowercase(),
            })
            .ok_or(Error::Configuration)?;
        let metadata_url = format!(
            "{}://{}/.well-known/oauth-protected-resource/mcp",
            resource_url.scheme(),
            resource_url.authority()
        );
        let metadata_path = "/.well-known/oauth-protected-resource/mcp".to_owned();
        let mut issuer_names = HashSet::new();
        let mut issuers = HashMap::new();
        for config in configs {
            let issuer = canonical_issuer(&config.issuer)?;
            if config.jwks_uri.is_some() == config.jwks.is_some() {
                return Err(Error::Configuration);
            }
            let jwks_uri = config
                .jwks_uri
                .as_deref()
                .map(Url::parse)
                .transpose()
                .map_err(|_| Error::Configuration)?;
            if !issuer_names.insert(issuer.clone())
                || jwks_uri.as_ref().is_some_and(|jwks_uri| {
                    jwks_uri.scheme() != "https"
                        || jwks_uri.host_str().is_none()
                        || !jwks_uri.username().is_empty()
                        || jwks_uri.password().is_some()
                        || jwks_uri.fragment().is_some()
                        || jwks_uri.as_str().len() > 2048
                })
            {
                return Err(Error::Configuration);
            }
            let static_keys = config
                .jwks
                .map(|set| {
                    if serde_json::to_vec(&set).map_err(|_| ())?.len() > MAX_JWKS_BYTES {
                        return Err(());
                    }
                    parse_jwks(set)
                })
                .transpose()
                .map_err(|_| Error::Configuration)?;
            let static_mode = static_keys.is_some();
            issuers.insert(
                issuer.clone(),
                Arc::new(Issuer {
                    name: issuer,
                    jwks_uri,
                    allow_private: config.allow_private,
                    static_keys: static_mode,
                    roots: roots.clone(),
                    cache: RwLock::new(KeyCache {
                        keys: static_keys.unwrap_or_default(),
                        expires_at: if static_mode {
                            Instant::now() + DEFAULT_JWKS_TTL
                        } else {
                            Instant::now()
                        },
                        last_unknown_refresh: None,
                    }),
                    refresh: Mutex::new(()),
                }),
            );
        }
        let mut authorization_servers: Vec<&str> =
            issuer_names.iter().map(String::as_str).collect();
        authorization_servers.sort_unstable();
        let metadata = serde_json::to_value(ProtectedResourceMetadata {
            resource,
            resource_name: "Brokerrouter MCP Gateway",
            authorization_servers,
            scopes_supported: [READ_SCOPE, CALL_SCOPE],
            bearer_methods_supported: ["header"],
        })
        .map_err(|_| Error::Configuration)?;
        let verifier = Self {
            inner: Arc::new(Inner {
                resource: resource.to_owned(),
                resource_authority,
                metadata_url,
                metadata_path,
                metadata,
                issuers,
                max_token_seconds,
            }),
        };
        futures_util::future::try_join_all(
            verifier
                .inner
                .issuers
                .values()
                .filter(|issuer| !issuer.static_keys)
                .map(|issuer| issuer.refresh(false)),
        )
        .await
        .map_err(|_| Error::Configuration)?;
        Ok(verifier)
    }

    pub fn resource_authority(&self) -> &str {
        &self.inner.resource_authority
    }

    pub fn metadata_path(&self) -> &str {
        &self.inner.metadata_path
    }

    pub fn metadata_url(&self) -> &str {
        &self.inner.metadata_url
    }

    pub fn metadata(&self) -> Value {
        self.inner.metadata.clone()
    }

    pub fn challenge(&self, error: Option<&str>, scope: &str) -> String {
        match error {
            Some(error) => format!(
                "Bearer error=\"{error}\", scope=\"{scope}\", resource_metadata=\"{}\"",
                self.inner.metadata_url
            ),
            None => format!(
                "Bearer resource_metadata=\"{}\", scope=\"{scope}\"",
                self.inner.metadata_url
            ),
        }
    }

    pub async fn verify(&self, token: &str) -> Result<Access, VerifyError> {
        if token.len() > MAX_TOKEN_BYTES {
            return Err(VerifyError::Invalid);
        }
        let unverified = unverified_issuer(token)?;
        let issuer = self
            .inner
            .issuers
            .get(&unverified.iss)
            .ok_or(VerifyError::Invalid)?;
        let header = decode_header(token).map_err(|_| VerifyError::Invalid)?;
        if !supported_algorithm(header.alg)
            || header.typ.as_deref() != Some("at+jwt")
            || header.crit.is_some()
            || header.jku.is_some()
            || header.jwk.is_some()
            || header.x5u.is_some()
            || header.x5c.is_some()
            || header.x5t.is_some()
            || header.x5t_s256.is_some()
            || header.enc.is_some()
            || header.zip.is_some()
            || header.url.is_some()
            || header.nonce.is_some()
            || !header.extras.is_empty()
        {
            return Err(VerifyError::Invalid);
        }
        let kid = header
            .kid
            .as_deref()
            .filter(|kid| valid_text(kid, 128))
            .ok_or(VerifyError::Invalid)?;
        let key = issuer.key(kid, header.alg).await?;
        let mut validation = Validation::new(header.alg);
        validation.set_audience(&[self.inner.resource.as_str()]);
        validation.set_issuer(&[issuer.name.as_str()]);
        validation.set_required_spec_claims(&[
            "exp",
            "nbf",
            "iat",
            "iss",
            "aud",
            "sub",
            "tenant_id",
            "scope",
        ]);
        validation.leeway = 5;
        validation.validate_nbf = true;
        let claims = decode::<Claims>(token, &key.decoding, &validation)
            .map_err(|_| VerifyError::Invalid)?
            .claims;
        let now = Utc::now().timestamp();
        if claims.iss != issuer.name
            || !exact_audience(&claims.aud, &self.inner.resource)
            || !valid_text(&claims.sub, 512)
            || claims.exp <= claims.iat
            || claims.nbf > claims.exp
            || claims.exp.saturating_sub(claims.iat) > self.inner.max_token_seconds
            || claims.iat > now + 5
            || claims.iat < now.saturating_sub(self.inner.max_token_seconds)
        {
            return Err(VerifyError::Invalid);
        }
        let scopes = parse_scopes(&claims.scope).ok_or(VerifyError::Invalid)?;
        Ok(Access {
            tenant_id: claims.tenant_id,
            issuer: claims.iss,
            subject: claims.sub,
            scopes,
        })
    }

    pub async fn resolve_identity(
        &self,
        pool: &PgPool,
        access: &Access,
    ) -> Result<Identity, VerifyError> {
        sqlx::query_as::<_, Identity>(
            "SELECT p.tenant_id,p.key_id FROM mcp_oauth_principals p JOIN tenants t ON t.id=p.tenant_id AND t.active JOIN virtual_keys k ON k.tenant_id=p.tenant_id AND k.id=p.key_id WHERE p.tenant_id=$1 AND p.issuer=$2 AND p.subject=$3 AND p.active AND k.revoked_at IS NULL AND (k.expires_at IS NULL OR k.expires_at>clock_timestamp())",
        )
        .bind(access.tenant_id)
        .bind(&access.issuer)
        .bind(&access.subject)
        .fetch_optional(pool)
        .await
        .map_err(|_| VerifyError::Unavailable)?
        .ok_or(VerifyError::Invalid)
    }
}

impl Issuer {
    async fn key(&self, kid: &str, algorithm: Algorithm) -> Result<CachedKey, VerifyError> {
        {
            let cache = self.cache.read().await;
            if (self.static_keys || cache.expires_at > Instant::now())
                && let Some(key) = cache.keys.get(kid)
            {
                return (key.algorithm == algorithm)
                    .then(|| key.clone())
                    .ok_or(VerifyError::Invalid);
            }
        }
        if self.static_keys {
            return Err(VerifyError::Invalid);
        }
        let expired = self.cache.read().await.expires_at <= Instant::now();
        if expired {
            self.refresh(false).await?;
        } else {
            let should_refresh = {
                let mut cache = self.cache.write().await;
                let now = Instant::now();
                let allowed = cache
                    .last_unknown_refresh
                    .is_none_or(|last| now.duration_since(last) >= UNKNOWN_KID_REFRESH_INTERVAL);
                if allowed {
                    cache.last_unknown_refresh = Some(now);
                }
                allowed
            };
            if should_refresh {
                self.refresh(true).await?;
            }
        }
        let cache = self.cache.read().await;
        let key = cache.keys.get(kid).ok_or(VerifyError::Invalid)?;
        (key.algorithm == algorithm)
            .then(|| key.clone())
            .ok_or(VerifyError::Invalid)
    }

    async fn refresh(&self, force: bool) -> Result<(), VerifyError> {
        let _refresh = self.refresh.lock().await;
        if !force && self.cache.read().await.expires_at > Instant::now() {
            return Ok(());
        }
        let jwks_uri = self.jwks_uri.as_ref().ok_or(VerifyError::Unavailable)?;
        let client = egress::pinned_client_with_roots(
            jwks_uri,
            self.allow_private,
            JWKS_TIMEOUT,
            &self.roots,
        )
        .await
        .map_err(|_| VerifyError::Unavailable)?;
        let response = client
            .get(jwks_uri.clone())
            .header("accept", "application/json")
            .send()
            .await
            .map_err(|_| VerifyError::Unavailable)?;
        if response.status() != reqwest::StatusCode::OK
            || response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .is_none_or(|value| !value.to_ascii_lowercase().starts_with("application/json"))
            || response
                .headers()
                .get(CONTENT_LENGTH)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<usize>().ok())
                .is_some_and(|length| length > MAX_JWKS_BYTES)
        {
            return Err(VerifyError::Unavailable);
        }
        let ttl = jwks_ttl(
            response
                .headers()
                .get(CACHE_CONTROL)
                .and_then(|v| v.to_str().ok()),
        );
        let mut bytes = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| VerifyError::Unavailable)?;
            if bytes.len().saturating_add(chunk.len()) > MAX_JWKS_BYTES {
                return Err(VerifyError::Unavailable);
            }
            bytes.extend_from_slice(&chunk);
        }
        let set: JwkSet = serde_json::from_slice(&bytes).map_err(|_| VerifyError::Unavailable)?;
        let keys = parse_jwks(set).map_err(|_| VerifyError::Unavailable)?;
        let mut cache = self.cache.write().await;
        cache.keys = keys;
        cache.expires_at = Instant::now() + ttl;
        Ok(())
    }
}

fn canonical_resource(value: &str) -> Result<Url, Error> {
    let parsed = Url::parse(value).map_err(|_| Error::Configuration)?;
    if parsed.scheme() != "https"
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || parsed.path() != "/mcp"
        || parsed.as_str() != value
        || value.len() > 2048
    {
        return Err(Error::Configuration);
    }
    Ok(parsed)
}

fn tls_roots_from_env() -> Result<Vec<reqwest::Certificate>, Error> {
    let path = match std::env::var("BROKERROUTER_MCP_CA_BUNDLE") {
        Ok(path) if !path.is_empty() => path,
        Ok(_) | Err(std::env::VarError::NotUnicode(_)) => return Err(Error::Configuration),
        Err(std::env::VarError::NotPresent) => return Ok(Vec::new()),
    };
    let metadata = std::fs::metadata(&path).map_err(|_| Error::Configuration)?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_CA_BUNDLE_BYTES as u64 {
        return Err(Error::Configuration);
    }
    let mut pem = Vec::with_capacity(metadata.len() as usize);
    std::fs::File::open(path)
        .map_err(|_| Error::Configuration)?
        .take((MAX_CA_BUNDLE_BYTES + 1) as u64)
        .read_to_end(&mut pem)
        .map_err(|_| Error::Configuration)?;
    if pem.is_empty() || pem.len() > MAX_CA_BUNDLE_BYTES {
        return Err(Error::Configuration);
    }
    let roots = reqwest::Certificate::from_pem_bundle(&pem).map_err(|_| Error::Configuration)?;
    if roots.is_empty() || roots.len() > 128 {
        return Err(Error::Configuration);
    }
    Ok(roots)
}

fn canonical_issuer(value: &str) -> Result<String, Error> {
    let parsed = Url::parse(value).map_err(|_| Error::Configuration)?;
    if parsed.scheme() != "https"
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || value.len() > 2048
    {
        return Err(Error::Configuration);
    }
    Ok(value.to_owned())
}

fn unverified_issuer(token: &str) -> Result<UnverifiedIssuer, VerifyError> {
    let mut segments = token.split('.');
    let _header = segments.next().ok_or(VerifyError::Invalid)?;
    let payload = segments.next().ok_or(VerifyError::Invalid)?;
    let _signature = segments.next().ok_or(VerifyError::Invalid)?;
    if segments.next().is_some() || payload.len() > MAX_TOKEN_BYTES {
        return Err(VerifyError::Invalid);
    }
    let bytes = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| VerifyError::Invalid)?;
    if bytes.len() > MAX_TOKEN_BYTES {
        return Err(VerifyError::Invalid);
    }
    serde_json::from_slice(&bytes).map_err(|_| VerifyError::Invalid)
}

fn supported_algorithm(algorithm: Algorithm) -> bool {
    matches!(
        algorithm,
        Algorithm::RS256 | Algorithm::PS256 | Algorithm::ES256 | Algorithm::EdDSA
    )
}

fn jwk_algorithm(algorithm: KeyAlgorithm) -> Option<Algorithm> {
    match algorithm {
        KeyAlgorithm::RS256 => Some(Algorithm::RS256),
        KeyAlgorithm::PS256 => Some(Algorithm::PS256),
        KeyAlgorithm::ES256 => Some(Algorithm::ES256),
        KeyAlgorithm::EdDSA => Some(Algorithm::EdDSA),
        _ => None,
    }
}

fn parse_jwks(set: JwkSet) -> Result<HashMap<String, CachedKey>, ()> {
    if set.keys.is_empty() || set.keys.len() > MAX_JWKS_KEYS {
        return Err(());
    }
    let mut seen = HashSet::new();
    let mut keys = HashMap::new();
    for jwk in set.keys {
        let kid = jwk
            .common
            .key_id
            .as_deref()
            .filter(|kid| valid_text(kid, 128))
            .ok_or(())?;
        if !seen.insert(kid.to_owned()) {
            return Err(());
        }
        match jwk.common.public_key_use {
            None | Some(PublicKeyUse::Signature) => {}
            Some(PublicKeyUse::Encryption | PublicKeyUse::Other(_)) => continue,
        }
        if jwk.common.public_key_use.is_some() && jwk.common.key_operations.is_some() {
            return Err(());
        }
        if jwk
            .common
            .key_operations
            .as_ref()
            .is_some_and(|operations| {
                operations.is_empty()
                    || operations
                        .iter()
                        .any(|operation| *operation != KeyOperations::Verify)
            })
        {
            continue;
        }
        let Some(algorithm) = jwk.common.key_algorithm.and_then(jwk_algorithm) else {
            continue;
        };
        let shape_matches = matches!(
            (algorithm, &jwk.algorithm),
            (
                Algorithm::RS256 | Algorithm::PS256,
                AlgorithmParameters::RSA(_)
            ) | (Algorithm::ES256, AlgorithmParameters::EllipticCurve(_))
                | (Algorithm::EdDSA, AlgorithmParameters::OctetKeyPair(_))
        );
        if !shape_matches || matches!(jwk.algorithm, AlgorithmParameters::OctetKey(_)) {
            return Err(());
        }
        let decoding = DecodingKey::from_jwk(&jwk).map_err(|_| ())?;
        keys.insert(
            kid.to_owned(),
            CachedKey {
                algorithm,
                decoding,
            },
        );
    }
    if keys.is_empty() {
        return Err(());
    }
    Ok(keys)
}

fn jwks_ttl(cache_control: Option<&str>) -> Duration {
    if cache_control.is_some_and(|value| {
        value
            .split(',')
            .any(|directive| directive.trim().eq_ignore_ascii_case("no-store"))
    }) {
        return Duration::ZERO;
    }
    cache_control
        .and_then(|value| {
            value.split(',').find_map(|directive| {
                directive
                    .trim()
                    .strip_prefix("max-age=")
                    .and_then(|value| value.parse::<u64>().ok())
            })
        })
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_JWKS_TTL)
        .min(MAX_JWKS_TTL)
}

fn exact_audience(value: &Value, resource: &str) -> bool {
    match value {
        Value::String(value) => value == resource,
        Value::Array(values) if values.len() == 1 => values[0].as_str() == Some(resource),
        _ => false,
    }
}

fn parse_scopes(value: &str) -> Option<HashSet<String>> {
    if value.is_empty() || value.len() > 4096 {
        return None;
    }
    let scopes: HashSet<String> = value.split(' ').map(str::to_owned).collect();
    if scopes.is_empty()
        || scopes.len() > 64
        || scopes.iter().any(|scope| {
            scope.is_empty()
                || scope.len() > 200
                || !scope.bytes().all(|byte| {
                    byte == 0x21 || (0x23..=0x5b).contains(&byte) || (0x5d..=0x7e).contains(&byte)
                })
        })
    {
        return None;
    }
    Some(scopes)
}

fn valid_text(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max && !value.chars().any(char::is_control)
}

pub fn unavailable_body() -> Value {
    json!({"error":{"code":"authentication_unavailable","message":"MCP authentication is temporarily unavailable","type":"gateway_error"}})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_and_audience_are_strict() {
        assert!(parse_scopes("mcp:tools:read mcp:tools:call").is_some());
        assert!(parse_scopes("mcp:tools:read  mcp:tools:call").is_none());
        assert!(exact_audience(
            &json!("https://gateway.example/mcp"),
            "https://gateway.example/mcp"
        ));
        assert!(!exact_audience(
            &json!(["https://gateway.example/mcp", "other"]),
            "https://gateway.example/mcp"
        ));
    }

    #[test]
    fn resource_and_issuer_require_canonical_https() {
        assert!(canonical_resource("https://gateway.example/mcp").is_ok());
        assert!(canonical_resource("http://gateway.example/mcp").is_err());
        assert!(canonical_resource("https://gateway.example/mcp?x=1").is_err());
        assert!(canonical_issuer("https://issuer.example/tenant").is_ok());
        assert!(canonical_issuer("https://issuer.example/tenant#x").is_err());
    }

    #[test]
    fn cache_control_is_bounded() {
        assert_eq!(
            jwks_ttl(Some("public, max-age=60")),
            Duration::from_secs(60)
        );
        assert_eq!(jwks_ttl(Some("max-age=999999")), MAX_JWKS_TTL);
        assert_eq!(jwks_ttl(Some("no-store")), Duration::ZERO);
    }
}
