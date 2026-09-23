use super::*;
use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{OriginalUri, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
};
const LOGIN: &str = "__Host-br-login";
const SESSION: &str = "__Host-br-session";
const CSRF: &str = "__Host-br-csrf";

pub fn router(service: Arc<Bff>) -> Router {
    Router::new()
        .route("/health/live", get(|| async { StatusCode::NO_CONTENT }))
        .route("/health/ready", get(ready))
        .route("/bff/v1/login", post(login))
        .route("/bff/v1/callback", get(callback))
        .route("/bff/v1/session", get(session))
        .route("/bff/v1/logout", post(logout))
        .route("/bff/v1/approvals/query", post(query))
        .route("/bff/v1/approvals/review", post(query))
        .route("/bff/v1/approvals/decide", post(query))
        .route("/bff/v1/approvals/skills/query", post(query))
        .route("/bff/v1/approvals/skills/review", post(query))
        .route("/bff/v1/approvals/skills/decide", post(query))
        .route("/bff/v1/media-review/start", post(query))
        .route("/bff/v1/media-review/chunk", post(query))
        .route("/bff/v1/media-review/complete", post(query))
        .route("/bff/v1/usage/query", post(query))
        .route("/bff/v1/usage/groups/query", post(query))
        .route("/bff/v1/requests/query", post(query))
        .route("/bff/v1/audit/query", post(query))
        .route("/bff/v1/traces/turns/query", post(query))
        .route("/bff/v1/traces/turns/detail", post(query))
        .route("/bff/v1/traces/turns/items", post(query))
        .route("/bff/v1/governance/monitor/query", post(query))
        .route("/bff/v1/governance/monitor/rails/query", post(query))
        .route("/bff/v1/catalog/models/query", post(query))
        .route("/bff/v1/catalog/endpoints/query", post(query))
        .route("/bff/v1/catalog/apply", post(query))
        .route("/bff/v1/settings/keys/query", post(query))
        .route("/bff/v1/settings/keys/detail", post(query))
        .route("/bff/v1/settings/models/query", post(query))
        .route("/bff/v1/settings/fx/query", post(query))
        .route("/bff/v1/settings/guardrails/query", post(query))
        .route("/bff/v1/settings/guardrails/history", post(query))
        .route("/bff/v1/settings/policies/query", post(query))
        .route("/bff/v1/settings/policies/preview", post(query))
        .route("/bff/v1/settings/governance/targets", post(query))
        .route("/bff/v1/settings/apply", post(query))
        .fallback(|| async { Error::NotFound })
        .with_state(service)
        .layer(axum::extract::DefaultBodyLimit::max(16384))
        .layer(middleware::from_fn_with_state(
            Arc::new(tokio::sync::Semaphore::new(8)),
            crate::api::http_capacity,
        ))
        .layer(middleware::from_fn(boundary))
}
async fn boundary(request: axum::extract::Request, next: middleware::Next) -> Response {
    let mut response = if request.uri().to_string().len() > 8192 {
        Error::Invalid("URI too long").into_response()
    } else {
        match tokio::time::timeout(Duration::from_secs(40), next.run(request)).await {
            Ok(v) => v,
            Err(_) => Error::BffUpstream.into_response(),
        }
    };
    for (name, value) in [
        ("cache-control", "no-store"),
        ("pragma", "no-cache"),
        ("x-content-type-options", "nosniff"),
        ("referrer-policy", "no-referrer"),
        (
            "content-security-policy",
            "default-src 'none'; frame-ancestors 'none'; base-uri 'none'",
        ),
        ("strict-transport-security", "max-age=31536000"),
        ("x-frame-options", "DENY"),
    ] {
        response
            .headers_mut()
            .insert(name, HeaderValue::from_static(value));
    }
    response
}
async fn ready(State(b): State<Arc<Bff>>) -> Result<StatusCode> {
    crate::check_schema(&b.pool).await?;
    Ok(StatusCode::NO_CONTENT)
}
fn origin(b: &Bff, headers: &HeaderMap) -> Result<()> {
    if headers.get_all(header::ORIGIN).iter().count() != 1
        || headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) != Some(&b.config.origin)
        || headers
            .get("sec-fetch-site")
            .is_some_and(|v| v != "same-origin")
        || headers.contains_key(header::AUTHORIZATION)
        || headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(';').next())
            != Some("application/json")
    {
        return Err(Error::CsrfRejected);
    }
    Ok(())
}
fn cookie(headers: &HeaderMap, name: &str) -> Result<Option<String>> {
    let mut found = None;
    for value in headers.get_all(header::COOKIE) {
        let text = value.to_str().map_err(|_| Error::SessionUnauthorized)?;
        if text.len() > 8192 {
            return Err(Error::SessionUnauthorized);
        }
        for pair in text.split(';') {
            if let Some((key, value)) = pair.trim().split_once('=')
                && key == name
            {
                if found.is_some()
                    || value.len() != 64
                    || !value.bytes().all(|v| v.is_ascii_hexdigit())
                {
                    return Err(Error::SessionUnauthorized);
                }
                found = Some(value.to_string());
            }
        }
    }
    Ok(found)
}
fn auth(headers: &HeaderMap) -> Result<(String, String)> {
    let session = cookie(headers, SESSION)?.ok_or(Error::SessionUnauthorized)?;
    let csrf = cookie(headers, CSRF)?.ok_or(Error::CsrfRejected)?;
    if headers.get_all("x-br-csrf").iter().count() != 1
        || headers.get("x-br-csrf").and_then(|v| v.to_str().ok()) != Some(&csrf)
    {
        return Err(Error::CsrfRejected);
    }
    Ok((session, csrf))
}
fn set_cookie(response: &mut Response, name: &str, value: &str, seconds: i64, http_only: bool) {
    let text = format!(
        "{name}={value}; Path=/; Secure; SameSite=Lax; Max-Age={seconds}{}",
        if http_only { "; HttpOnly" } else { "" }
    );
    response.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(&text).expect("bounded hex cookie"),
    );
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Login {
    tenant_id: Uuid,
}
async fn login(State(b): State<Arc<Bff>>, headers: HeaderMap, body: Bytes) -> Result<Response> {
    origin(&b, &headers)?;
    let input: Login =
        serde_json::from_slice(&body).map_err(|_| Error::Invalid("tenant_id required"))?;
    let (url, token) = b
        .login(input.tenant_id, cookie(&headers, LOGIN)?.as_deref())
        .await?;
    let mut response = Json(json!({"authorization_url":url})).into_response();
    set_cookie(&mut response, LOGIN, &token, 300, true);
    Ok(response)
}
#[derive(Deserialize)]
struct Callback {
    state: String,
    code: Option<String>,
    error: Option<String>,
    iss: Option<String>,
}
async fn callback(
    State(b): State<Arc<Bff>>,
    headers: HeaderMap,
    input: std::result::Result<Query<Callback>, axum::extract::rejection::QueryRejection>,
) -> Response {
    // This changes presentation only, never authentication or the redirect destination.
    let browser = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(',').any(|part| {
                part.split(';')
                    .next()
                    .is_some_and(|mime| mime.trim() == "text/html")
            })
        });
    let result = async {
        let Query(input) = input.map_err(|_| Error::Invalid("invalid OIDC callback"))?;
        let token = cookie(&headers, LOGIN)?.ok_or(Error::SessionUnauthorized)?;
        let code = if input.error.is_some()
            || input
                .iss
                .as_deref()
                .is_some_and(|v| v != b.config.oidc_issuer)
        {
            None
        } else {
            input.code
        };
        b.callback(
            &token,
            &input.state,
            code,
            cookie(&headers, SESSION)?.as_deref(),
        )
        .await
    }
    .await;
    let mut response = match result {
        Ok((token, csrf, seconds)) => {
            // Fixed destination. No return_to / redirect_uri from the browser is ever accepted.
            let mut response = (StatusCode::SEE_OTHER, [(header::LOCATION, "/")]).into_response();
            set_cookie(&mut response, SESSION, &token, seconds, true);
            set_cookie(&mut response, CSRF, &csrf, seconds, false);
            response
        }
        Err(_) if browser => (
            StatusCode::SEE_OTHER,
            [(header::LOCATION, "/?login=failed")],
        )
            .into_response(),
        Err(error) => error.into_response(),
    };
    set_cookie(&mut response, LOGIN, "", 0, true);
    response
}
async fn session(State(b): State<Arc<Bff>>, headers: HeaderMap) -> Result<Json<Value>> {
    let token = cookie(&headers, SESSION)?.ok_or(Error::SessionUnauthorized)?;
    let s = b.session(&token, None).await?;
    Ok(Json(
        json!({"tenant_id":s.tenant_id,"user_id":s.user_id,"expires_at":s.expires_at,"currency":"CNY","media_preview_origin":b.preview_origin}),
    ))
}
async fn logout(State(b): State<Arc<Bff>>, headers: HeaderMap, body: Bytes) -> Result<Response> {
    origin(&b, &headers)?;
    if serde_json::from_slice::<Value>(&body).ok() != Some(json!({})) {
        return Err(Error::Invalid("empty JSON object required"));
    }
    let (token, csrf) = auth(&headers)?;
    b.logout(&token, &csrf).await?;
    let mut response = StatusCode::NO_CONTENT.into_response();
    for (name, only) in [(SESSION, true), (CSRF, false), (LOGIN, true)] {
        set_cookie(&mut response, name, "", 0, only);
    }
    Ok(response)
}
async fn query(
    State(b): State<Arc<Bff>>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response> {
    origin(&b, &headers)?;
    if uri.query().is_some() {
        return Err(Error::Invalid("query string is not supported"));
    }
    let (token, csrf) = auth(&headers)?;
    let path = if let Some(suffix) = uri.path().strip_prefix("/bff/v1/catalog/") {
        format!("/catalog/v1/{suffix}")
    } else if let Some(suffix) = uri.path().strip_prefix("/bff/v1/settings/") {
        format!("/settings/v1/{suffix}")
    } else if let Some(suffix) = uri.path().strip_prefix("/bff/v1/approvals/") {
        format!("/approvals/v1/{suffix}")
    } else if let Some(suffix) = uri.path().strip_prefix("/bff/v1/media-review/") {
        format!("/media-review/v1/{suffix}")
    } else {
        uri.path().replacen("/bff/", "/console/", 1)
    };
    let (status, bytes) = b.query(&token, &csrf, &path, &body).await?;
    if path == "/media-review/v1/chunk" && status == StatusCode::OK {
        return Ok((
            status,
            [
                (header::CONTENT_TYPE, "application/octet-stream"),
                (header::CONTENT_DISPOSITION, "attachment"),
            ],
            Body::from(bytes),
        )
            .into_response());
    }
    Ok((
        status,
        [(header::CONTENT_TYPE, "application/json")],
        Body::from(bytes),
    )
        .into_response())
}
