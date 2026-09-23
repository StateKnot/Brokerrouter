//! Fixed-path, purpose-isolated human approval listener. Never expose to Agent tools.
use crate::{
    approval_workbench as workbench,
    error::{Error, Result},
    governance_api::Control,
    mcp_skill_approvals,
};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{OriginalUri, State},
    http::{HeaderMap, StatusCode},
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::Value;
use std::{sync::Arc, time::Duration};

pub fn router(c: Control) -> Router {
    Router::new()
        .route("/health/live", get(|| async { StatusCode::NO_CONTENT }))
        .route("/health/ready", get(ready))
        .route("/approvals/v1/query", post(handle))
        .route("/approvals/v1/review", post(handle))
        .route("/approvals/v1/decide", post(handle))
        .route("/approvals/v1/skills/query", post(handle))
        .route("/approvals/v1/skills/review", post(handle))
        .route("/approvals/v1/skills/decide", post(handle))
        .fallback(|| async { Error::NotFound })
        .with_state(c)
        .layer(axum::extract::DefaultBodyLimit::max(16384))
        .layer(middleware::from_fn_with_state(
            Arc::new(tokio::sync::Semaphore::new(8)),
            crate::api::http_capacity,
        ))
        .layer(middleware::from_fn(boundary))
}
async fn ready(State(c): State<Control>) -> Result<StatusCode> {
    crate::check_schema(&c.pool).await?;
    Ok(StatusCode::NO_CONTENT)
}
async fn boundary(request: axum::extract::Request, next: middleware::Next) -> Response {
    let mut response = match tokio::time::timeout(Duration::from_secs(15), next.run(request)).await
    {
        Ok(r) => r,
        Err(_) => Error::ConsoleTimeout.into_response(),
    };
    for (k, v) in [
        ("cache-control", "no-store"),
        ("x-content-type-options", "nosniff"),
        ("referrer-policy", "no-referrer"),
        (
            "content-security-policy",
            "default-src 'none'; frame-ancestors 'none'; base-uri 'none'",
        ),
    ] {
        response
            .headers_mut()
            .insert(k, v.parse().expect("static header"));
    }
    response
}
fn decode<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T> {
    serde_json::from_slice(body).map_err(|_| Error::Invalid("invalid approval request"))
}
async fn handle(
    State(c): State<Control>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>> {
    if uri.query().is_some() {
        return Err(Error::Invalid("query string not supported"));
    }
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or(Error::HumanUnauthorized)?;
    let who = c
        .verifier
        .verify(&c.pool, token, "POST", uri.path(), &body)
        .await?;
    let value = match uri.path() {
        "/approvals/v1/query" => workbench::query(&c.pool, &who, decode(&body)?).await?,
        "/approvals/v1/review" => {
            workbench::review(&c.pool, &c.cipher, &who, decode(&body)?).await?
        }
        "/approvals/v1/decide" => {
            workbench::decide(&c.pool, &c.cipher, &who, decode(&body)?).await?
        }
        "/approvals/v1/skills/query" => {
            mcp_skill_approvals::query(&c.pool, &who, decode(&body)?).await?
        }
        "/approvals/v1/skills/review" => {
            mcp_skill_approvals::review(&c.pool, &who, decode(&body)?).await?
        }
        "/approvals/v1/skills/decide" => {
            mcp_skill_approvals::decide(&c.pool, &c.cipher, &who, decode(&body)?).await?
        }
        _ => return Err(Error::NotFound),
    };
    Ok(Json(value))
}
