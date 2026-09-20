//! Production boundary for outbound MCP `server/discover` over Streamable HTTP.
//!
//! The catalog owns configuration and identity. This module alone resolves the
//! referenced credential, establishes a pinned outbound connection, executes
//! the official RMCP 2026-07-28 lifecycle, and persists the observed result.
//! It never accepts a credential value from a caller or forwards a downstream
//! identity token upstream.

use crate::{
    cipher::ResultCipher,
    egress,
    error::Error as GatewayError,
    mcp_catalog::{self, ConnectorServer, CredentialReference, RecordDiscovery},
};
use futures_util::{StreamExt, stream::BoxStream};
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use reqwest::Client;
use rmcp::{
    ClientLifecycleMode, ClientServiceExt,
    model::{
        CacheScope as RmcpCacheScope, ClientCapabilities, ClientConfig, DiscoverResult,
        Implementation, ProtocolVersion, RequestMetaObject, ServerJsonRpcMessage,
    },
    transport::{
        StreamableHttpClientTransport,
        auth::{
            AuthClient, AuthorizationManager, AuthorizationMetadataSource, ClientCredentialsConfig,
            OAuthHttpClient, OAuthHttpClientError, OAuthHttpClientFuture, OAuthHttpRedirectPolicy,
            OAuthHttpRequest,
        },
        streamable_http_client::{
            AuthRequiredError, InsufficientScopeError, SseError, StreamableHttpClient,
            StreamableHttpClientTransportConfig, StreamableHttpError, StreamableHttpPostResponse,
        },
    },
};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    io::Read,
    sync::Arc,
    time::Duration,
};
use url::Url;
use uuid::Uuid;
use zeroize::Zeroizing;

const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(20);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_DISCOVERY_RESPONSE_BYTES: usize = 512 * 1024;
const MAX_OAUTH_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_CREDENTIAL_BYTES: usize = 64 * 1024;
const MAX_OAUTH_REDIRECTS: usize = 5;
const MAX_CA_BUNDLE_BYTES: usize = 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum DiscoveryError {
    #[error("MCP catalog operation failed")]
    Catalog(#[source] GatewayError),
    #[error("this operation requires an active Streamable HTTP MCP server")]
    UnsupportedTransport,
    #[error("the configured MCP credential reference could not be resolved")]
    CredentialUnavailable,
    #[error("the resolved MCP credential has an invalid format")]
    InvalidCredential,
    #[error("the MCP outbound network policy rejected the connection")]
    NetworkPolicy,
    #[error("MCP server discovery failed")]
    Protocol,
    #[error("MCP OAuth discovery or token exchange failed")]
    OAuth,
    #[error("MCP OAuth metadata did not match the pinned issuer/resource/audience")]
    OAuthBinding,
}

impl From<GatewayError> for DiscoveryError {
    fn from(value: GatewayError) -> Self {
        Self::Catalog(value)
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SecretEnvironmentBinding {
    reference: String,
    environment: String,
}

/// Resolves credentials already injected into this isolated connector process.
///
/// Direct environment references name the injected variable. Secret Manager
/// references use an explicit URI-to-variable binding supplied in
/// `BROKERROUTER_MCP_SECRET_ENV_BINDINGS`; the binding contains no secret value
/// and works with Vault Agent, CSI Secret Store, External Secrets and analogous
/// production injectors.
#[derive(Clone, Debug, Default)]
pub struct InjectedCredentialResolver {
    secret_environments: HashMap<String, String>,
}

#[derive(Clone)]
struct McpTlsRoots {
    roots: Arc<Vec<reqwest::Certificate>>,
    trust_fingerprint: [u8; 32],
}

impl Default for McpTlsRoots {
    fn default() -> Self {
        Self {
            roots: Arc::new(Vec::new()),
            trust_fingerprint: Sha256::digest(b"brokerrouter:mcp:platform-trust:v1").into(),
        }
    }
}

impl McpTlsRoots {
    fn from_env() -> Result<Self, DiscoveryError> {
        let path = match std::env::var("BROKERROUTER_MCP_CA_BUNDLE") {
            Ok(path) if !path.is_empty() => path,
            Ok(_) | Err(std::env::VarError::NotUnicode(_)) => {
                return Err(DiscoveryError::NetworkPolicy);
            }
            Err(std::env::VarError::NotPresent) => return Ok(Self::default()),
        };
        let metadata = std::fs::metadata(&path).map_err(|_| DiscoveryError::NetworkPolicy)?;
        if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_CA_BUNDLE_BYTES as u64
        {
            return Err(DiscoveryError::NetworkPolicy);
        }
        let mut pem = Vec::with_capacity(metadata.len() as usize);
        std::fs::File::open(path)
            .map_err(|_| DiscoveryError::NetworkPolicy)?
            .take((MAX_CA_BUNDLE_BYTES + 1) as u64)
            .read_to_end(&mut pem)
            .map_err(|_| DiscoveryError::NetworkPolicy)?;
        if pem.is_empty() || pem.len() > MAX_CA_BUNDLE_BYTES {
            return Err(DiscoveryError::NetworkPolicy);
        }
        let roots = reqwest::Certificate::from_pem_bundle(&pem)
            .map_err(|_| DiscoveryError::NetworkPolicy)?;
        if roots.is_empty() || roots.len() > 128 {
            return Err(DiscoveryError::NetworkPolicy);
        }
        Ok(Self {
            roots: Arc::new(roots),
            trust_fingerprint: Sha256::digest(&pem).into(),
        })
    }
}

impl InjectedCredentialResolver {
    pub fn from_env() -> Result<Self, DiscoveryError> {
        let raw = match std::env::var("BROKERROUTER_MCP_SECRET_ENV_BINDINGS") {
            Ok(value) => value,
            Err(std::env::VarError::NotPresent) => "[]".to_owned(),
            Err(std::env::VarError::NotUnicode(_)) => {
                return Err(DiscoveryError::InvalidCredential);
            }
        };
        Self::from_bindings_json(&raw)
    }

    fn from_bindings_json(raw: &str) -> Result<Self, DiscoveryError> {
        let bindings: Vec<SecretEnvironmentBinding> =
            serde_json::from_str(raw).map_err(|_| DiscoveryError::InvalidCredential)?;
        if bindings.len() > 1024 {
            return Err(DiscoveryError::InvalidCredential);
        }
        let mut secret_environments = HashMap::with_capacity(bindings.len());
        for binding in bindings {
            if !valid_environment_name(&binding.environment)
                || !valid_secret_reference(&binding.reference)
                || secret_environments
                    .insert(binding.reference, binding.environment)
                    .is_some()
            {
                return Err(DiscoveryError::InvalidCredential);
            }
        }
        Ok(Self {
            secret_environments,
        })
    }

    pub fn resolve(
        &self,
        reference: &CredentialReference,
    ) -> Result<Zeroizing<String>, DiscoveryError> {
        let environment = match reference {
            CredentialReference::Environment { name } if valid_environment_name(name) => name,
            CredentialReference::SecretManager { uri } if valid_secret_reference(uri) => self
                .secret_environments
                .get(uri)
                .ok_or(DiscoveryError::CredentialUnavailable)?,
            _ => return Err(DiscoveryError::InvalidCredential),
        };
        let value = std::env::var(environment)
            .map_err(|_| DiscoveryError::CredentialUnavailable)
            .map(Zeroizing::new)?;
        if value.is_empty() || value.len() > MAX_CREDENTIAL_BYTES || value.contains('\0') {
            return Err(DiscoveryError::InvalidCredential);
        }
        Ok(value)
    }
}

fn valid_environment_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.as_bytes()[0].is_ascii_uppercase()
        && value
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
}

fn valid_secret_reference(value: &str) -> bool {
    let Ok(url) = Url::parse(value) else {
        return false;
    };
    matches!(url.scheme(), "vault" | "aws-sm" | "gcp-sm" | "aliyun-kms")
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
        && !url.path().is_empty()
        && !url.path().split('/').any(|part| part == "..")
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OAuthClientSecret {
    client_id: String,
    client_secret: String,
    #[serde(default)]
    scopes: Vec<String>,
}

fn parse_oauth_secret(raw: &str) -> Result<OAuthClientSecret, DiscoveryError> {
    let secret: OAuthClientSecret =
        serde_json::from_str(raw).map_err(|_| DiscoveryError::InvalidCredential)?;
    let mut scopes = HashSet::new();
    if secret.client_id.is_empty()
        || secret.client_id.len() > 512
        || secret.client_id.chars().any(char::is_control)
        || secret.client_secret.is_empty()
        || secret.client_secret.len() > MAX_CREDENTIAL_BYTES
        || secret.client_secret.contains('\0')
        || secret.scopes.len() > 64
        || secret.scopes.iter().any(|scope| {
            scope.is_empty()
                || scope.len() > 256
                || !scope
                    .bytes()
                    .all(|b| b == 0x21 || (0x23..=0x5b).contains(&b) || (0x5d..=0x7e).contains(&b))
                || !scopes.insert(scope.as_str())
        })
    {
        return Err(DiscoveryError::InvalidCredential);
    }
    Ok(secret)
}

fn authorization_fingerprint(parts: &[&[u8]]) -> String {
    let mut digest = Sha256::new();
    digest.update(b"brokerrouter:mcp:authorization-context:v1\0");
    for part in parts {
        digest.update((part.len() as u64).to_be_bytes());
        digest.update(part);
    }
    format!("{:x}", digest.finalize())
}

#[derive(Debug, thiserror::Error)]
enum OAuthNetworkError {
    #[error("invalid OAuth request URL")]
    InvalidUrl,
    #[error("OAuth request rejected by network policy")]
    NetworkPolicy,
    #[error("OAuth request failed")]
    Request,
    #[error("OAuth response exceeded the configured size limit")]
    ResponseTooLarge,
    #[error("OAuth redirect was rejected")]
    Redirect,
    #[error("invalid OAuth response")]
    InvalidResponse,
}

#[derive(Clone)]
struct PinnedOAuthHttpClient {
    allow_private: bool,
    timeout: Duration,
    roots: Arc<Vec<reqwest::Certificate>>,
}

fn same_origin(left: &Url, right: &Url) -> bool {
    left.scheme() == right.scheme()
        && left.host_str() == right.host_str()
        && left.port_or_known_default() == right.port_or_known_default()
}

async fn bounded_response_body(
    response: reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>, OAuthNetworkError> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(OAuthNetworkError::ResponseTooLarge);
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| OAuthNetworkError::Request)?;
        if chunk.len() > limit.saturating_sub(body.len()) {
            return Err(OAuthNetworkError::ResponseTooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

impl OAuthHttpClient for PinnedOAuthHttpClient {
    fn execute(&self, operation: OAuthHttpRequest) -> OAuthHttpClientFuture<'_> {
        Box::pin(async move {
            let OAuthHttpRequest {
                request,
                redirect_policy,
                timeout,
                ..
            } = operation;
            let (parts, body) = request.into_parts();
            let mut url = Url::parse(&parts.uri.to_string())
                .map_err(|_| Box::new(OAuthNetworkError::InvalidUrl) as OAuthHttpClientError)?;
            if url.scheme() != "https"
                || url.host_str().is_none()
                || !url.username().is_empty()
                || url.password().is_some()
                || url.fragment().is_some()
            {
                return Err(Box::new(OAuthNetworkError::InvalidUrl) as OAuthHttpClientError);
            }
            let origin = url.clone();
            let timeout = timeout.unwrap_or(self.timeout).min(self.timeout);
            let follow = matches!(redirect_policy, OAuthHttpRedirectPolicy::Follow);
            for redirect_count in 0..=MAX_OAUTH_REDIRECTS {
                let client = egress::pinned_client_with_roots(
                    &url,
                    self.allow_private,
                    timeout,
                    &self.roots,
                )
                .await
                .map_err(|_| Box::new(OAuthNetworkError::NetworkPolicy) as OAuthHttpClientError)?;
                let mut request = client
                    .request(parts.method.clone(), url.clone())
                    .headers(parts.headers.clone())
                    .body(body.clone());
                request = request.timeout(timeout);
                let response = request
                    .send()
                    .await
                    .map_err(|_| Box::new(OAuthNetworkError::Request) as OAuthHttpClientError)?;
                if follow && response.status().is_redirection() {
                    if redirect_count == MAX_OAUTH_REDIRECTS
                        || !matches!(parts.method, Method::GET | Method::HEAD)
                    {
                        return Err(Box::new(OAuthNetworkError::Redirect) as OAuthHttpClientError);
                    }
                    let location = response
                        .headers()
                        .get(header::LOCATION)
                        .and_then(|value| value.to_str().ok())
                        .ok_or_else(|| {
                            Box::new(OAuthNetworkError::Redirect) as OAuthHttpClientError
                        })?;
                    let next = url.join(location).map_err(|_| {
                        Box::new(OAuthNetworkError::Redirect) as OAuthHttpClientError
                    })?;
                    if next.scheme() != "https"
                        || !same_origin(&origin, &next)
                        || !next.username().is_empty()
                        || next.password().is_some()
                        || next.fragment().is_some()
                    {
                        return Err(Box::new(OAuthNetworkError::Redirect) as OAuthHttpClientError);
                    }
                    url = next;
                    continue;
                }
                let status = response.status();
                let version = response.version();
                let headers = response.headers().clone();
                let body = bounded_response_body(response, MAX_OAUTH_RESPONSE_BYTES)
                    .await
                    .map_err(|error| Box::new(error) as OAuthHttpClientError)?;
                let mut response = http::Response::builder().status(status).version(version);
                for (name, value) in &headers {
                    response = response.header(name, value);
                }
                return response.body(body).map_err(|_| {
                    Box::new(OAuthNetworkError::InvalidResponse) as OAuthHttpClientError
                });
            }
            Err(Box::new(OAuthNetworkError::Redirect) as OAuthHttpClientError)
        })
    }
}

#[derive(Debug, thiserror::Error)]
enum DiscoveryBodyError {
    #[error("MCP response read failed")]
    Request(#[source] reqwest::Error),
    #[error("MCP discovery response exceeded {0} bytes")]
    TooLarge(usize),
}

#[derive(Clone, Debug)]
struct BoundedDiscoveryHttpClient {
    inner: Client,
}

fn session_id(headers: &HeaderMap) -> Result<Option<String>, StreamableHttpError<reqwest::Error>> {
    headers
        .get("mcp-session-id")
        .map(|value| {
            value.to_str().map(str::to_owned).map_err(|_| {
                StreamableHttpError::UnexpectedServerResponse(Cow::Borrowed(
                    "invalid MCP session header",
                ))
            })
        })
        .transpose()
}

fn apply_protocol_headers(
    mut request: reqwest::RequestBuilder,
    headers: HashMap<HeaderName, HeaderValue>,
) -> Result<reqwest::RequestBuilder, StreamableHttpError<reqwest::Error>> {
    for (name, value) in headers {
        if matches!(
            name.as_str(),
            "accept"
                | "authorization"
                | "connection"
                | "content-length"
                | "content-type"
                | "cookie"
                | "host"
                | "last-event-id"
                | "mcp-session-id"
                | "proxy-authenticate"
                | "proxy-authorization"
                | "referer"
                | "te"
                | "trailer"
                | "transfer-encoding"
                | "upgrade"
        ) {
            return Err(StreamableHttpError::ReservedHeaderConflict(
                name.to_string(),
            ));
        }
        request = request.header(name, value);
    }
    Ok(request)
}

fn has_content_type(headers: &HeaderMap, expected: &str) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case(expected))
}

async fn bounded_json_body(
    response: reqwest::Response,
) -> Result<Vec<u8>, StreamableHttpError<reqwest::Error>> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_DISCOVERY_RESPONSE_BYTES as u64)
    {
        return Err(StreamableHttpError::UnexpectedServerResponse(
            Cow::Borrowed("MCP discovery response is too large"),
        ));
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(StreamableHttpError::Client)?;
        if chunk.len() > MAX_DISCOVERY_RESPONSE_BYTES.saturating_sub(body.len()) {
            return Err(StreamableHttpError::UnexpectedServerResponse(
                Cow::Borrowed("MCP discovery response is too large"),
            ));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn bounded_sse_response(
    response: reqwest::Response,
) -> BoxStream<'static, Result<sse_stream::Sse, SseError>> {
    let stream = response
        .bytes_stream()
        .scan((0usize, false), |(observed, failed), item| {
            futures_util::future::ready({
                if *failed {
                    None
                } else {
                    match item {
                        Ok(chunk)
                            if chunk.len()
                                <= MAX_DISCOVERY_RESPONSE_BYTES.saturating_sub(*observed) =>
                        {
                            *observed += chunk.len();
                            Some(Ok(chunk))
                        }
                        Ok(_) => {
                            *failed = true;
                            Some(Err(DiscoveryBodyError::TooLarge(
                                MAX_DISCOVERY_RESPONSE_BYTES,
                            )))
                        }
                        Err(error) => {
                            *failed = true;
                            Some(Err(DiscoveryBodyError::Request(error)))
                        }
                    }
                }
            })
        });
    sse_stream::SseStream::from_bytes_stream(stream).boxed()
}

impl StreamableHttpClient for BoundedDiscoveryHttpClient {
    type Error = reqwest::Error;

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: rmcp::model::ClientJsonRpcMessage,
        session: Option<Arc<str>>,
        auth_token: Option<String>,
        headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        let mut request = self
            .inner
            .post(uri.as_ref())
            .header(header::ACCEPT, "application/json, text/event-stream");
        if let Some(token) = auth_token {
            request = request.bearer_auth(token);
        }
        if let Some(session) = &session {
            request = request.header("mcp-session-id", session.as_ref());
        }
        request = apply_protocol_headers(request, headers)?;
        let response = request
            .json(&message)
            .send()
            .await
            .map_err(StreamableHttpError::Client)?;

        if response.status() == StatusCode::UNAUTHORIZED
            && let Some(challenge) = response.headers().get(header::WWW_AUTHENTICATE)
        {
            let challenge = challenge.to_str().map_err(|_| {
                StreamableHttpError::UnexpectedServerResponse(Cow::Borrowed(
                    "invalid WWW-Authenticate header",
                ))
            })?;
            return Err(StreamableHttpError::AuthRequired(AuthRequiredError::new(
                challenge.to_owned(),
            )));
        }
        if response.status() == StatusCode::FORBIDDEN
            && let Some(challenge) = response.headers().get(header::WWW_AUTHENTICATE)
        {
            let challenge = challenge.to_str().map_err(|_| {
                StreamableHttpError::UnexpectedServerResponse(Cow::Borrowed(
                    "invalid WWW-Authenticate header",
                ))
            })?;
            return Err(StreamableHttpError::InsufficientScope(
                InsufficientScopeError::new(challenge.to_owned(), None),
            ));
        }
        if response.status() == StatusCode::ACCEPTED {
            return Ok(StreamableHttpPostResponse::Accepted);
        }
        if !response.status().is_success()
            && !has_content_type(response.headers(), "application/json")
        {
            return Err(StreamableHttpError::Client(
                response.error_for_status().expect_err("non-success status"),
            ));
        }

        let response_session = session_id(response.headers())?;
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        if has_content_type(response.headers(), "application/json") {
            let body = bounded_json_body(response).await?;
            let message: ServerJsonRpcMessage = serde_json::from_slice(&body)?;
            Ok(StreamableHttpPostResponse::Json(message, response_session))
        } else if has_content_type(response.headers(), "text/event-stream") {
            Ok(StreamableHttpPostResponse::Sse(
                bounded_sse_response(response),
                response_session,
            ))
        } else {
            Err(StreamableHttpError::UnexpectedContentType(content_type))
        }
    }

    async fn post_message_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        message: rmcp::model::ClientJsonRpcMessage,
        session: Option<Arc<str>>,
        auth_token: Option<String>,
        headers: HashMap<HeaderName, HeaderValue>,
        _max_sse_event_size: usize,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        self.post_message(uri, message, session, auth_token, headers)
            .await
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session: Arc<str>,
        auth_token: Option<String>,
        headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), StreamableHttpError<Self::Error>> {
        let _ = (uri, session, auth_token, headers);
        // The 2026-07-28 lifecycle is stateless and never deletes a session.
        Ok(())
    }

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth_token: Option<String>,
        headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<
        BoxStream<'static, Result<sse_stream::Sse, SseError>>,
        StreamableHttpError<Self::Error>,
    > {
        let _ = (uri, session, last_event_id, auth_token, headers);
        // 2026-07-28 does not use the legacy standalone GET stream.
        Err(StreamableHttpError::ServerDoesNotSupportSse)
    }

    async fn get_stream_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        session: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth_token: Option<String>,
        headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> Result<
        BoxStream<'static, Result<sse_stream::Sse, SseError>>,
        StreamableHttpError<Self::Error>,
    > {
        let _ = max_sse_event_size;
        self.get_stream(uri, session, last_event_id, auth_token, headers)
            .await
    }
}

#[derive(Clone, Debug)]
pub struct DiscoverServer {
    pub tenant_id: Uuid,
    pub server_id: Uuid,
    pub operation_id: Uuid,
    pub actor: String,
}

fn client_config() -> ClientConfig {
    ClientConfig::new(
        ClientCapabilities::default(),
        Implementation::new("brokerrouter", env!("CARGO_PKG_VERSION")),
    )
    .with_protocol_version(ProtocolVersion::V_2026_07_28)
}

async fn discover_wire<C>(
    client: C,
    endpoint: &str,
    auth_header: Option<String>,
) -> Result<DiscoverResult, DiscoveryError>
where
    C: StreamableHttpClient + Sync,
{
    let mut transport_config = StreamableHttpClientTransportConfig::with_uri(endpoint.to_owned())
        .max_concurrent_requests(1)
        .control_request_timeout(Duration::from_secs(3))
        .session_recovery_timeout(Duration::from_secs(3))
        .max_sse_event_size(MAX_DISCOVERY_RESPONSE_BYTES)
        .reinit_on_expired_session(false);
    transport_config.allow_stateless = true;
    transport_config.auth_header = auth_header;
    let transport = StreamableHttpClientTransport::with_client(client, transport_config);
    let config = client_config();
    let mut running = tokio::time::timeout(
        DISCOVERY_TIMEOUT,
        config.clone().serve_with_lifecycle(
            transport,
            ClientLifecycleMode::Discover {
                preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            },
        ),
    )
    .await
    .map_err(|_| DiscoveryError::Protocol)?
    .map_err(|_| DiscoveryError::Protocol)?;

    // The lifecycle's first response establishes the selected version. A
    // second idempotent discovery call returns the complete cache metadata that
    // RunningService intentionally reduces in its peer view.
    let meta = RequestMetaObject::with_client_context(
        ProtocolVersion::V_2026_07_28,
        config.client_info,
        config.capabilities,
    );
    let result = tokio::time::timeout(DISCOVERY_TIMEOUT, running.discover(meta))
        .await
        .map_err(|_| DiscoveryError::Protocol)?
        .map_err(|_| DiscoveryError::Protocol);
    let _ = running.close_with_timeout(SHUTDOWN_TIMEOUT).await;
    result
}

fn discovery_record(
    input: &DiscoverServer,
    server: &ConnectorServer,
    result: DiscoverResult,
    fingerprint: String,
) -> Result<RecordDiscovery, DiscoveryError> {
    let ttl_ms = i32::try_from(result.ttl_ms).map_err(|_| DiscoveryError::Protocol)?;
    let server_info = result
        .server_info()
        .map(serde_json::to_value)
        .transpose()
        .map_err(|_| DiscoveryError::Protocol)?;
    let capabilities =
        serde_json::to_value(result.capabilities).map_err(|_| DiscoveryError::Protocol)?;
    let cache_scope = match result.cache_scope {
        RmcpCacheScope::Private => mcp_catalog::CacheScope::Private,
        RmcpCacheScope::Public => mcp_catalog::CacheScope::Public,
        _ => return Err(DiscoveryError::Protocol),
    };
    Ok(RecordDiscovery {
        tenant_id: input.tenant_id,
        server_id: input.server_id,
        operation_id: input.operation_id,
        actor: input.actor.clone(),
        expected_server_revision: server.config_revision,
        protocol_version: mcp_catalog::PROTOCOL_VERSION.to_owned(),
        result_type: result.result_type.as_str().to_owned(),
        supported_versions: result
            .supported_versions
            .iter()
            .map(ToString::to_string)
            .collect(),
        capabilities,
        server_info,
        instructions: result.instructions,
        ttl_ms,
        cache_scope,
        authorization_context_fingerprint: matches!(cache_scope, mcp_catalog::CacheScope::Private)
            .then_some(fingerprint),
    })
}

/// Discover an active catalog server and persist the immutable observation.
pub async fn discover_and_record(
    pool: &PgPool,
    cipher: &ResultCipher,
    resolver: &InjectedCredentialResolver,
    input: DiscoverServer,
) -> Result<Value, DiscoveryError> {
    if let Some(receipt) =
        mcp_catalog::discovery_receipt(pool, input.tenant_id, input.server_id, input.operation_id)
            .await?
    {
        return Ok(receipt);
    }
    let server =
        mcp_catalog::connector_server(pool, cipher, input.tenant_id, input.server_id).await?;
    if server.transport != "streamable_http" || server.stdio_profile.is_some() {
        return Err(DiscoveryError::UnsupportedTransport);
    }
    let endpoint = server
        .endpoint_url
        .as_deref()
        .ok_or(DiscoveryError::UnsupportedTransport)?;
    let endpoint_url = Url::parse(endpoint).map_err(|_| DiscoveryError::NetworkPolicy)?;
    let tls = McpTlsRoots::from_env()?;
    let http = egress::pinned_client_with_roots(
        &endpoint_url,
        server.allow_private,
        DISCOVERY_TIMEOUT,
        &tls.roots,
    )
    .await
    .map_err(|_| DiscoveryError::NetworkPolicy)?;
    let http = BoundedDiscoveryHttpClient { inner: http };

    let (result, fingerprint) = match server.auth_mode.as_str() {
        "none" if server.credential_reference.is_none() => {
            let fingerprint = authorization_fingerprint(&[b"none", &tls.trust_fingerprint]);
            (discover_wire(http, endpoint, None).await?, fingerprint)
        }
        "bearer" => {
            let reference = server
                .credential_reference
                .as_ref()
                .ok_or(DiscoveryError::InvalidCredential)?;
            let token = resolver.resolve(reference)?;
            let fingerprint =
                authorization_fingerprint(&[b"bearer", &tls.trust_fingerprint, token.as_bytes()]);
            (
                discover_wire(http, endpoint, Some(token.to_string())).await?,
                fingerprint,
            )
        }
        "oauth_client_credentials" => {
            let reference = server
                .credential_reference
                .as_ref()
                .ok_or(DiscoveryError::InvalidCredential)?;
            let raw = resolver.resolve(reference)?;
            let secret = parse_oauth_secret(&raw)?;
            let issuer = server
                .oauth_issuer
                .as_deref()
                .ok_or(DiscoveryError::OAuthBinding)?;
            let resource = server
                .oauth_resource
                .as_deref()
                .ok_or(DiscoveryError::OAuthBinding)?;
            let audience = server
                .oauth_audience
                .as_deref()
                .ok_or(DiscoveryError::OAuthBinding)?;
            if audience != resource || resource != endpoint {
                return Err(DiscoveryError::OAuthBinding);
            }
            let oauth_http = Arc::new(PinnedOAuthHttpClient {
                allow_private: server.allow_private,
                timeout: DISCOVERY_TIMEOUT,
                roots: tls.roots.clone(),
            });
            let mut manager =
                AuthorizationManager::new_with_oauth_http_client(endpoint, oauth_http)
                    .await
                    .map_err(|_| DiscoveryError::OAuth)?;
            let resolution = manager
                .resolve_metadata()
                .await
                .map_err(|_| DiscoveryError::OAuth)?;
            if !resolution.source.is_discovered()
                || matches!(
                    resolution.source,
                    AuthorizationMetadataSource::LegacyEndpointFallback
                )
                || resolution.metadata.issuer.as_deref() != Some(issuer)
            {
                return Err(DiscoveryError::OAuthBinding);
            }
            manager.set_metadata(resolution.metadata);
            let credentials = ClientCredentialsConfig::ClientSecret {
                client_id: secret.client_id.clone(),
                client_secret: secret.client_secret.clone(),
                scopes: secret.scopes,
                resource: Some(resource.to_owned()),
            };
            manager
                .validate_client_credentials_metadata(&credentials)
                .and_then(|_| manager.configure_client_credentials(&credentials))
                .map_err(|_| DiscoveryError::OAuth)?;
            manager
                .exchange_client_credentials(&credentials)
                .await
                .map_err(|_| DiscoveryError::OAuth)?;
            let access_token = manager
                .get_access_token()
                .await
                .map_err(|_| DiscoveryError::OAuth)?;
            let fingerprint = authorization_fingerprint(&[
                b"oauth_client_credentials",
                issuer.as_bytes(),
                resource.as_bytes(),
                audience.as_bytes(),
                secret.client_id.as_bytes(),
                secret.client_secret.as_bytes(),
                access_token.as_bytes(),
                &tls.trust_fingerprint,
            ]);
            let auth_client = AuthClient::new(http, manager);
            (
                discover_wire(auth_client, endpoint, None).await?,
                fingerprint,
            )
        }
        _ => return Err(DiscoveryError::InvalidCredential),
    };
    let record = discovery_record(&input, &server, result, fingerprint)?;
    mcp_catalog::record_discovery(pool, record)
        .await
        .map_err(DiscoveryError::Catalog)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, extract::State, routing::post};
    use serde_json::json;
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    #[derive(Clone)]
    struct WireFixture {
        calls: Arc<AtomicUsize>,
        requests: Arc<Mutex<Vec<Value>>>,
        oversized: bool,
    }

    async fn discovery_fixture(
        State(state): State<WireFixture>,
        headers: HeaderMap,
        Json(request): Json<Value>,
    ) -> (StatusCode, HeaderMap, Json<Value>) {
        state.calls.fetch_add(1, Ordering::SeqCst);
        state.requests.lock().unwrap().push(request.clone());
        assert_eq!(
            headers
                .get("mcp-protocol-version")
                .and_then(|value| value.to_str().ok()),
            Some(mcp_catalog::PROTOCOL_VERSION)
        );
        assert_eq!(
            headers
                .get("mcp-method")
                .and_then(|value| value.to_str().ok()),
            Some("server/discover")
        );
        assert_eq!(
            headers
                .get(header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok()),
            Some("Bearer fixture-token")
        );
        let instructions = state
            .oversized
            .then(|| "x".repeat(MAX_DISCOVERY_RESPONSE_BYTES + 1));
        let response = json!({
            "jsonrpc":"2.0",
            "id":request["id"],
            "result":{
                "resultType":"complete",
                "supportedVersions":[mcp_catalog::PROTOCOL_VERSION],
                "capabilities":{"tools":{"listChanged":true}},
                "instructions":instructions,
                "ttlMs":30000,
                "cacheScope":"private",
                "_meta":{"io.modelcontextprotocol/serverInfo":{"name":"fixture","version":"1.0.0"}}
            }
        });
        (StatusCode::OK, HeaderMap::new(), Json(response))
    }

    async fn fixture_endpoint(
        oversized: bool,
    ) -> (String, WireFixture, tokio::task::JoinHandle<()>) {
        let fixture = WireFixture {
            calls: Arc::new(AtomicUsize::new(0)),
            requests: Arc::new(Mutex::new(Vec::new())),
            oversized,
        };
        let app = Router::new()
            .route("/mcp", post(discovery_fixture))
            .with_state(fixture.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{address}/mcp"), fixture, server)
    }

    #[test]
    fn binding_document_rejects_duplicates_and_secret_values() {
        let resolver = InjectedCredentialResolver::from_bindings_json(
            r#"[{"reference":"vault://prod/mcp/a","environment":"MCP_A"}]"#,
        )
        .unwrap();
        assert_eq!(
            resolver.secret_environments.get("vault://prod/mcp/a"),
            Some(&"MCP_A".to_owned())
        );
        assert!(
            InjectedCredentialResolver::from_bindings_json(
                r#"[{"reference":"vault://prod/mcp/a","environment":"MCP_A"},{"reference":"vault://prod/mcp/a","environment":"MCP_B"}]"#
            )
            .is_err()
        );
        assert!(
            InjectedCredentialResolver::from_bindings_json(
                r#"[{"reference":"vault://prod/mcp/a","environment":"actual-secret-value"}]"#
            )
            .is_err()
        );
    }

    #[test]
    fn oauth_secret_and_fingerprint_are_strict_and_unambiguous() {
        let secret = parse_oauth_secret(
            r#"{"client_id":"brokerrouter","client_secret":"secret","scopes":["mcp:read"]}"#,
        )
        .unwrap();
        assert_eq!(secret.client_id, "brokerrouter");
        assert!(
            parse_oauth_secret(r#"{"client_id":"a","client_secret":"b","scopes":["x","x"]}"#)
                .is_err()
        );
        assert!(
            parse_oauth_secret(r#"{"client_id":"a","client_secret":"b","unknown":true}"#).is_err()
        );
        assert_ne!(
            authorization_fingerprint(&[b"ab", b"c"]),
            authorization_fingerprint(&[b"a", b"bc"])
        );
        assert_eq!(authorization_fingerprint(&[b"none"]).len(), 64);
    }

    #[test]
    fn content_type_requires_an_exact_media_type() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("Application/JSON; charset=utf-8"),
        );
        assert!(has_content_type(&headers, "application/json"));
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/jsonx"),
        );
        assert!(!has_content_type(&headers, "application/json"));
    }

    #[tokio::test]
    async fn official_discover_lifecycle_sends_modern_metadata_and_standard_headers() {
        let (endpoint, fixture, server) = fixture_endpoint(false).await;
        let client = BoundedDiscoveryHttpClient {
            inner: Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
        };
        let result = discover_wire(client, &endpoint, Some("fixture-token".into()))
            .await
            .unwrap();
        server.abort();
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            result.supported_versions,
            vec![ProtocolVersion::V_2026_07_28]
        );
        assert_eq!(result.ttl_ms, 30_000);
        assert_eq!(result.cache_scope, RmcpCacheScope::Private);
        let requests = fixture.requests.lock().unwrap();
        assert!(
            requests
                .iter()
                .all(|request| request["method"] == "server/discover")
        );
        assert!(requests.iter().all(|request| {
            request["params"]["_meta"]["io.modelcontextprotocol/protocolVersion"]
                == mcp_catalog::PROTOCOL_VERSION
                && request["params"]["_meta"]["io.modelcontextprotocol/clientInfo"]["name"]
                    == "brokerrouter"
        }));
    }

    #[tokio::test]
    async fn discovery_rejects_oversized_json_before_deserialization() {
        let (endpoint, _, server) = fixture_endpoint(true).await;
        let client = BoundedDiscoveryHttpClient {
            inner: Client::new(),
        };
        let result = discover_wire(client, &endpoint, Some("fixture-token".into())).await;
        server.abort();
        assert!(matches!(result, Err(DiscoveryError::Protocol)));
    }
}
