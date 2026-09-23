//! Authenticated, stateless downstream MCP tool edge.

use crate::{
    cipher::ResultCipher,
    error::Error,
    mcp_calls, mcp_catalog, mcp_oauth,
    store::{self, Identity},
};
use axum::{
    Json, Router,
    body::Body,
    http::{HeaderMap, Request, StatusCode, header, request::Parts},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post_service},
};
use base64::{Engine, prelude::BASE64_STANDARD};
use chrono::Utc;
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler,
    model::{
        CacheScope, CallToolRequestParams, CallToolResponse, Implementation,
        InitializeRequestParams, InitializeResult, ListToolsResult, PaginatedRequestParams,
        ProtocolVersion, ServerCapabilities, ServerConfig, Tool,
    },
    service::RequestContext,
    transport::{
        StreamableHttpServerConfig, StreamableHttpService,
        streamable_http_server::session::never::NeverSessionManager,
    },
};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::{borrow::Cow, collections::HashSet, sync::Arc};
use url::Url;

const MAX_REQUEST_BODY_BYTES: usize = 1024 * 1024;
const PAGE_SIZE: i64 = 100;

#[derive(Clone, Debug)]
pub struct HttpConfig {
    allowed_hosts: Vec<String>,
    allowed_origins: Vec<String>,
}

#[derive(Clone)]
struct Boundary {
    allowed_hosts: Arc<HashSet<String>>,
    allowed_origins: Arc<HashSet<String>>,
}

#[derive(Clone)]
struct AuthState {
    pool: PgPool,
    oauth: Option<mcp_oauth::Verifier>,
}

#[derive(Clone)]
struct Authenticated {
    identity: Identity,
    /// `None` is the existing first-party virtual-key flow. OAuth tokens carry
    /// an explicit, authorization-server-issued scope set.
    scopes: Option<Arc<HashSet<String>>>,
}

impl Authenticated {
    fn permits(&self, scope: &str) -> bool {
        self.scopes
            .as_ref()
            .is_none_or(|scopes| scopes.contains(scope))
    }
}

impl HttpConfig {
    pub fn from_env() -> Result<Self, Error> {
        let hosts =
            std::env::var("BROKERROUTER_MCP_ALLOWED_HOSTS").map_err(|_| Error::Configuration)?;
        let origins =
            std::env::var("BROKERROUTER_MCP_ALLOWED_ORIGINS").unwrap_or_else(|_| "[]".to_owned());
        Self::new(parse_string_array(&hosts)?, parse_string_array(&origins)?)
    }

    pub fn new(allowed_hosts: Vec<String>, allowed_origins: Vec<String>) -> Result<Self, Error> {
        if allowed_hosts.is_empty() || allowed_hosts.len() > 64 || allowed_origins.len() > 64 {
            return Err(Error::Configuration);
        }
        let mut hosts = HashSet::new();
        for value in allowed_hosts {
            let normalized = normalize_authority(&value).ok_or(Error::Configuration)?;
            if !hosts.insert(normalized) {
                return Err(Error::Configuration);
            }
        }
        let mut origins = HashSet::new();
        for value in allowed_origins {
            let normalized = normalize_origin(&value).ok_or(Error::Configuration)?;
            if !origins.insert(normalized) {
                return Err(Error::Configuration);
            }
        }
        Ok(Self {
            allowed_hosts: hosts.into_iter().collect(),
            allowed_origins: origins.into_iter().collect(),
        })
    }
}

fn parse_string_array(raw: &str) -> Result<Vec<String>, Error> {
    let values: Vec<String> = serde_json::from_str(raw).map_err(|_| Error::Configuration)?;
    if values
        .iter()
        .any(|value| value.is_empty() || value.len() > 512)
    {
        return Err(Error::Configuration);
    }
    Ok(values)
}

fn normalize_authority(value: &str) -> Option<String> {
    let authority = value.parse::<http::uri::Authority>().ok()?;
    let host = authority
        .host()
        .trim_matches(['[', ']'])
        .to_ascii_lowercase();
    if host.is_empty() || host.contains('*') {
        return None;
    }
    Some(match authority.port_u16() {
        Some(port) => format!("{host}:{port}"),
        None => host,
    })
}

fn normalize_origin(value: &str) -> Option<String> {
    if value == "null" {
        return Some(value.to_owned());
    }
    let parsed = Url::parse(value).ok()?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || parsed.path() != "/"
    {
        return None;
    }
    let host = parsed.host_str()?.to_ascii_lowercase();
    Some(match parsed.port() {
        Some(port) => format!("{}://{host}:{port}", parsed.scheme()),
        None => format!("{}://{host}", parsed.scheme()),
    })
}

pub fn router(
    pool: PgPool,
    config: HttpConfig,
    oauth: Option<mcp_oauth::Verifier>,
    cipher: Arc<ResultCipher>,
) -> Result<Router, Error> {
    if oauth.as_ref().is_some_and(|oauth| {
        !config
            .allowed_hosts
            .iter()
            .any(|host| host == oauth.resource_authority())
    }) {
        return Err(Error::Configuration);
    }
    let boundary = Boundary {
        allowed_hosts: Arc::new(config.allowed_hosts.iter().cloned().collect()),
        allowed_origins: Arc::new(config.allowed_origins.iter().cloned().collect()),
    };
    let sdk = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_allowed_hosts(config.allowed_hosts)
        .with_allowed_origins(config.allowed_origins)
        .enforce_origin_validation()
        .with_max_request_body_bytes(MAX_REQUEST_BODY_BYTES)
        .with_stateless_protocol_metadata_required(true);
    let handler_pool = pool.clone();
    let handler_cipher = cipher;
    let service = StreamableHttpService::new(
        move || {
            Ok(CatalogServer {
                pool: handler_pool.clone(),
                calls: mcp_calls::Edge::new(handler_pool.clone(), handler_cipher.clone()),
            })
        },
        Arc::new(NeverSessionManager::default()),
        sdk,
    );
    let protected = Router::new()
        .route("/mcp", post_service(service))
        .layer(middleware::from_fn_with_state(
            AuthState {
                pool,
                oauth: oauth.clone(),
            },
            authenticate,
        ))
        .layer(middleware::from_fn(reject_legacy_headers));
    let mut router = Router::new().merge(protected);
    if let Some(oauth) = oauth {
        let path = oauth.metadata_path().to_owned();
        let metadata = oauth.metadata();
        router = router.route(
            &path,
            get(move || {
                let metadata = metadata.clone();
                async move {
                    (
                        [(header::CACHE_CONTROL, "public, max-age=300")],
                        Json(metadata),
                    )
                }
            }),
        );
    }
    Ok(router
        .fallback(|| async { Error::NotFound })
        .layer(middleware::from_fn_with_state(boundary, validate_boundary)))
}

async fn reject_legacy_headers(request: Request<Body>, next: Next) -> Response {
    if request.headers().contains_key("mcp-session-id")
        || request.headers().contains_key("last-event-id")
    {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({"error":{"code":"legacy_mcp_session_rejected","message":"MCP 2026-07-28 is stateless"}})),
        )
            .into_response();
    }
    next.run(request).await
}

async fn validate_boundary(
    axum::extract::State(boundary): axum::extract::State<Boundary>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .and_then(normalize_authority);
    let host_allowed = host.as_ref().is_some_and(|host| {
        boundary.allowed_hosts.contains(host)
            || host
                .rsplit_once(':')
                .is_some_and(|(without_port, _)| boundary.allowed_hosts.contains(without_port))
    });
    let origin_allowed = match request.headers().get(header::ORIGIN) {
        None => true,
        Some(value) => value
            .to_str()
            .ok()
            .and_then(normalize_origin)
            .is_some_and(|origin| boundary.allowed_origins.contains(&origin)),
    };
    if !host_allowed || !origin_allowed {
        return StatusCode::FORBIDDEN.into_response();
    }
    next.run(request).await
}

async fn authenticate(
    axum::extract::State(state): axum::extract::State<AuthState>,
    mut request: Request<Body>,
    next: Next,
) -> Response {
    let mut values = request.headers().get_all(header::AUTHORIZATION).iter();
    let token = values
        .next()
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    if values.next().is_some()
        || token.is_none_or(|token| token.is_empty() || token.contains(char::is_whitespace))
    {
        return unauthorized(state.oauth.as_ref(), token.is_some());
    }
    let token = token.expect("validated bearer token");
    let principal = if token.starts_with("sk_br_") {
        store::authenticate(&state.pool, token)
            .await
            .map(|identity| Authenticated {
                identity,
                scopes: None,
            })
            .map_err(|error| match error {
                Error::Unauthorized => mcp_oauth::VerifyError::Invalid,
                _ => mcp_oauth::VerifyError::Unavailable,
            })
    } else if let Some(oauth) = &state.oauth {
        match oauth.verify(token).await {
            Ok(access) => oauth
                .resolve_identity(&state.pool, &access)
                .await
                .map(|identity| Authenticated {
                    identity,
                    scopes: Some(Arc::new(access.scopes)),
                }),
            Err(error) => Err(error),
        }
    } else {
        Err(mcp_oauth::VerifyError::Invalid)
    };
    match principal {
        Ok(principal) => {
            let required = if request
                .headers()
                .get("mcp-method")
                .and_then(|value| value.to_str().ok())
                == Some("tools/call")
            {
                mcp_oauth::CALL_SCOPE
            } else {
                mcp_oauth::READ_SCOPE
            };
            if !principal.permits(required) {
                return insufficient_scope(state.oauth.as_ref(), required);
            }
            request.extensions_mut().insert(principal);
            next.run(request).await
        }
        Err(mcp_oauth::VerifyError::Invalid) => unauthorized(state.oauth.as_ref(), true),
        Err(mcp_oauth::VerifyError::Unavailable) => (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::CACHE_CONTROL, "no-store")],
            Json(mcp_oauth::unavailable_body()),
        )
            .into_response(),
    }
}

fn unauthorized(oauth: Option<&mcp_oauth::Verifier>, invalid: bool) -> Response {
    let code = if invalid {
        "invalid_token"
    } else {
        "authentication_required"
    };
    let mut response = (
        StatusCode::UNAUTHORIZED,
        Json(json!({"error":{"code":code,"message":"A valid MCP bearer credential is required","type":"gateway_error"}})),
    )
        .into_response();
    let challenge = oauth.map_or_else(
        || match invalid {
            true => "Bearer realm=\"brokerrouter-mcp\", error=\"invalid_token\"".to_owned(),
            false => "Bearer realm=\"brokerrouter-mcp\"".to_owned(),
        },
        |oauth| oauth.challenge(invalid.then_some("invalid_token"), mcp_oauth::READ_SCOPE),
    );
    response.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        challenge.parse().expect("validated OAuth challenge"),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        "no-store".parse().expect("static header"),
    );
    response
}

fn insufficient_scope(oauth: Option<&mcp_oauth::Verifier>, scope: &str) -> Response {
    let mut response = (
        StatusCode::FORBIDDEN,
        Json(json!({"error":{"code":"insufficient_scope","message":"The access token lacks the scope required for this MCP operation","type":"gateway_error"}})),
    )
        .into_response();
    let challenge = oauth.map_or_else(
        || format!("Bearer error=\"insufficient_scope\", scope=\"{scope}\""),
        |oauth| oauth.challenge(Some("insufficient_scope"), scope),
    );
    response.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        challenge.parse().expect("validated OAuth challenge"),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        "no-store".parse().expect("static header"),
    );
    response
}

#[derive(Clone)]
struct CatalogServer {
    pool: PgPool,
    calls: mcp_calls::Edge,
}

fn identity(context: &RequestContext<RoleServer>) -> Result<Identity, McpError> {
    context
        .extensions
        .get::<Parts>()
        .and_then(|parts| parts.extensions.get::<Authenticated>())
        .map(|principal| principal.identity.clone())
        .ok_or_else(|| McpError::internal_error("Authenticated identity is unavailable", None))
}

fn require_scope(context: &RequestContext<RoleServer>, scope: &str) -> Result<(), McpError> {
    let permitted = context
        .extensions
        .get::<Parts>()
        .and_then(|parts| parts.extensions.get::<Authenticated>())
        .is_some_and(|principal| principal.permits(scope));
    permitted
        .then_some(())
        .ok_or_else(|| McpError::invalid_request("OAuth scope is insufficient", None))
}

fn primitive(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Bool(value) => Some(value.to_string()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

fn decoded_header(value: &str) -> Option<String> {
    match value
        .strip_prefix("=?base64?")
        .and_then(|value| value.strip_suffix("?="))
    {
        Some(value) => String::from_utf8(BASE64_STANDARD.decode(value).ok()?).ok(),
        None => Some(value.to_owned()),
    }
}

fn validate_param_headers(
    headers: &HeaderMap,
    schema: &serde_json::Map<String, Value>,
    arguments: &serde_json::Map<String, Value>,
) -> Result<(), String> {
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return Ok(());
    };
    let mut names = HashSet::new();
    for (property, schema) in properties {
        let Some(annotation) = schema.get("x-mcp-header") else {
            continue;
        };
        let annotation = annotation
            .as_str()
            .filter(|value| {
                !value.is_empty()
                    && value.chars().all(|character| {
                        character.is_ascii_alphanumeric()
                            || matches!(
                                character,
                                '!' | '#'
                                    | '$'
                                    | '%'
                                    | '&'
                                    | '\''
                                    | '*'
                                    | '+'
                                    | '-'
                                    | '.'
                                    | '^'
                                    | '_'
                                    | '`'
                                    | '|'
                                    | '~'
                            )
                    })
            })
            .ok_or_else(|| format!("invalid x-mcp-header annotation for `{property}`"))?;
        if !names.insert(annotation.to_ascii_lowercase())
            || !matches!(
                schema.get("type").and_then(Value::as_str),
                Some("string" | "integer" | "boolean")
            )
        {
            return Err(format!("invalid x-mcp-header annotation for `{property}`"));
        }
        let full = format!("mcp-param-{annotation}");
        let supplied = headers.get(&full).and_then(|value| value.to_str().ok());
        let expected = arguments
            .get(property)
            .filter(|value| !value.is_null())
            .and_then(primitive);
        match (supplied, expected) {
            (None, None) => {}
            (Some(_), None) => return Err(format!("unexpected {full} header")),
            (None, Some(_)) => return Err(format!("missing {full} header")),
            (Some(supplied), Some(expected)) => {
                if decoded_header(supplied).as_deref() != Some(expected.as_str()) {
                    return Err(format!("{full} header does not match body"));
                }
            }
        }
    }
    Ok(())
}

impl ServerHandler for CatalogServer {
    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Owned(vec![ProtocolVersion::V_2026_07_28])
    }

    async fn initialize(
        &self,
        _request: InitializeRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, McpError> {
        Err(McpError::invalid_request(
            "initialize is not supported by the stateless 2026-07-28 endpoint",
            None,
        ))
    }

    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_protocol_version(ProtocolVersion::V_2026_07_28)
            .with_server_info(Implementation::new(
                "brokerrouter",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "Tool calls require io.brokerrouter/turnId and io.brokerrouter/callId UUID metadata. Reuse callId on retries. Approval is resolved through the bound runtime's governance workbench.",
            )
    }

    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        require_scope(&context, mcp_oauth::READ_SCOPE)?;
        let who = identity(&context)?;
        let cursor = request.and_then(|request| request.cursor);
        let mut rows =
            mcp_catalog::authorized_tools(&self.pool, &who, cursor.as_deref(), PAGE_SIZE + 1)
                .await
                .map_err(|_| McpError::internal_error("Tool catalog is unavailable", None))?;
        let has_more = rows.len() > PAGE_SIZE as usize;
        if has_more {
            rows.truncate(PAGE_SIZE as usize);
        }
        let next_cursor = has_more
            .then(|| rows.last().map(|tool| tool.downstream_name.clone()))
            .flatten();
        let ttl_ms = rows
            .iter()
            .map(|tool| (tool.expires_at - Utc::now()).num_milliseconds().max(0) as u64)
            .min()
            .unwrap_or(0);
        let mut tools = Vec::with_capacity(rows.len());
        for row in rows {
            let mut tool: Tool = serde_json::from_value(row.descriptor)
                .map_err(|_| McpError::internal_error("Stored tool descriptor is invalid", None))?;
            tool.name = Cow::Owned(row.downstream_name);
            tools.push(tool);
        }
        let mut result = ListToolsResult::with_all_items(tools)
            .with_ttl_ms(ttl_ms)
            .with_cache_scope(CacheScope::Private);
        result.next_cursor = next_cursor;
        Ok(result)
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        require_scope(&context, mcp_oauth::CALL_SCOPE)?;
        let who = identity(&context)?;
        let tool = mcp_catalog::authorized_tool(&self.pool, &who, request.name.as_ref())
            .await
            .map_err(|_| McpError::internal_error("Tool authorization is unavailable", None))?
            .ok_or_else(|| McpError::invalid_params("Unknown or unauthorized tool", None))?;
        let arguments = request.arguments.unwrap_or_default();
        let headers = context
            .extensions
            .get::<Parts>()
            .map(|parts| &parts.headers)
            .ok_or_else(|| {
                McpError::internal_error("HTTP request metadata is unavailable", None)
            })?;
        let input_schema = tool
            .descriptor
            .get("inputSchema")
            .and_then(Value::as_object)
            .ok_or_else(|| McpError::internal_error("Stored tool schema is invalid", None))?;
        validate_param_headers(headers, input_schema, &arguments)
            .map_err(|reason| McpError::header_mismatch(reason, None))?;
        if request.input_responses.is_some() || request.request_state.is_some() {
            return Err(McpError::invalid_params(
                "This tool call has no Brokerrouter MRTR request to resume",
                None,
            ));
        }
        let turn_id = context
            .meta
            .get("io.brokerrouter/turnId")
            .and_then(Value::as_str)
            .and_then(|value| uuid::Uuid::parse_str(value).ok())
            .ok_or_else(|| {
                McpError::invalid_params("A valid io.brokerrouter/turnId is required", None)
            })?;
        let call_id = context
            .meta
            .get("io.brokerrouter/callId")
            .and_then(Value::as_str)
            .and_then(|value| uuid::Uuid::parse_str(value).ok())
            .ok_or_else(|| {
                McpError::invalid_params("A valid io.brokerrouter/callId is required", None)
            })?;
        self.calls
            .call(&who, turn_id, call_id, tool, Value::Object(arguments))
            .await
            .map_err(|error| {
                let (code, message) = match &error {
                    Error::ApprovalRequired(_) => {
                        ("approval_required", "Human approval is required")
                    }
                    Error::InProgress => ("tool_call_pending", "Tool call is pending"),
                    Error::UpstreamUnknown => (
                        "execution_unknown",
                        "The upstream outcome needs reconciliation",
                    ),
                    Error::NotSubmitted => ("tool_not_submitted", "Tool call was not submitted"),
                    Error::Conflict => (
                        "idempotency_conflict",
                        "callId is already bound to a different tool call",
                    ),
                    Error::NotFound | Error::TurnClosed | Error::Unauthorized => (
                        "authorization_ended",
                        "Tool call is not authorized in this turn",
                    ),
                    Error::ResultUnavailable => {
                        ("result_unavailable", "Stored tool result is unavailable")
                    }
                    _ => ("tool_call_unavailable", "Tool execution is unavailable"),
                };
                let mut data = json!({"code":code,"callId":call_id});
                if let Error::ApprovalRequired(approval) = error {
                    data["approval"] = approval;
                }
                McpError::invalid_request(message, Some(data))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configuration_requires_exact_hosts_and_origins() {
        assert!(HttpConfig::new(vec![], vec![]).is_err());
        assert!(HttpConfig::new(vec!["*.example.com".into()], vec![]).is_err());
        assert!(
            HttpConfig::new(
                vec!["Gateway.Example:8443".into()],
                vec!["https://console.example".into()]
            )
            .is_ok()
        );
        assert!(
            HttpConfig::new(
                vec!["gateway.example".into()],
                vec!["https://console.example/path".into()]
            )
            .is_err()
        );
    }

    #[test]
    fn annotated_parameter_headers_must_match_body() {
        let schema = json!({
            "type":"object",
            "properties":{"id":{"type":"string","x-mcp-header":"Record-Id"}}
        });
        let arguments = json!({"id":"记录-1"});
        let mut headers = HeaderMap::new();
        headers.insert(
            "mcp-param-record-id",
            format!("=?base64?{}?=", BASE64_STANDARD.encode("记录-1"))
                .parse()
                .unwrap(),
        );
        assert!(
            validate_param_headers(
                &headers,
                schema.as_object().unwrap(),
                arguments.as_object().unwrap()
            )
            .is_ok()
        );
        headers.insert("mcp-param-record-id", "different".parse().unwrap());
        assert!(
            validate_param_headers(
                &headers,
                schema.as_object().unwrap(),
                arguments.as_object().unwrap()
            )
            .is_err()
        );
    }
}
