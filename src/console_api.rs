//! Read-only operational API. Browser sessions and signing keys belong in the BFF.
use crate::{
    api, console,
    error::{Error, Result},
    human,
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
use sqlx::PgPool;
use std::sync::Arc;

#[derive(Clone)]
pub struct Console {
    pub pool: PgPool,
    pub verifier: Arc<human::Verifier>,
}
pub fn router(service: Console) -> Router {
    Router::new()
        .route("/health/live", get(|| async { StatusCode::NO_CONTENT }))
        .route("/health/ready", get(ready))
        .route("/console/v1/usage/query", post(usage))
        .route("/console/v1/usage/groups/query", post(groups))
        .route("/console/v1/requests/query", post(requests))
        .route("/console/v1/audit/query", post(audit))
        .route("/console/v1/governance/monitor/query", post(monitor))
        .route(
            "/console/v1/governance/monitor/rails/query",
            post(monitor_rails),
        )
        .fallback(|| async { Error::NotFound })
        .with_state(service)
        .layer(axum::extract::DefaultBodyLimit::max(16384))
        .layer(middleware::from_fn_with_state(
            Arc::new(tokio::sync::Semaphore::new(8)),
            api::http_capacity,
        ))
        .layer(middleware::from_fn(query_deadline))
        .layer(middleware::from_fn(api::request_id))
        .layer(middleware::from_fn(no_store))
}
async fn query_deadline(request: axum::extract::Request, next: middleware::Next) -> Response {
    match tokio::time::timeout(std::time::Duration::from_secs(20), next.run(request)).await {
        Ok(response) => response,
        Err(_) => Error::ConsoleTimeout.into_response(),
    }
}
async fn no_store(request: axum::extract::Request, next: middleware::Next) -> Response {
    let mut response = next.run(request).await;
    for (name, value) in [
        ("cache-control", "no-store"),
        ("x-content-type-options", "nosniff"),
        (
            "content-security-policy",
            "default-src 'none'; frame-ancestors 'none'",
        ),
        ("referrer-policy", "no-referrer"),
    ] {
        response
            .headers_mut()
            .insert(name, value.parse().expect("static header"));
    }
    response
}
async fn ready(State(c): State<Console>) -> Result<StatusCode> {
    crate::check_schema(&c.pool).await?;
    Ok(StatusCode::NO_CONTENT)
}
async fn auth(c: &Console, headers: &HeaderMap, path: &str, body: &Bytes) -> Result<human::Human> {
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or(Error::HumanUnauthorized)?;
    c.verifier.verify(&c.pool, token, "POST", path, body).await
}
fn decode<T: serde::de::DeserializeOwned>(body: &Bytes) -> Result<T> {
    serde_json::from_slice(body).map_err(|_| Error::Invalid("invalid console query"))
}
async fn usage(
    State(c): State<Console>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>> {
    let who = auth(&c, &headers, &uri.to_string(), &body).await?;
    Ok(Json(console::usage(&c.pool, &who, decode(&body)?).await?))
}
async fn groups(
    State(c): State<Console>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>> {
    let who = auth(&c, &headers, &uri.to_string(), &body).await?;
    Ok(Json(console::groups(&c.pool, &who, decode(&body)?).await?))
}
async fn requests(
    State(c): State<Console>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>> {
    let who = auth(&c, &headers, &uri.to_string(), &body).await?;
    Ok(Json(
        console::requests(&c.pool, &who, decode(&body)?).await?,
    ))
}
async fn audit(
    State(c): State<Console>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>> {
    let who = auth(&c, &headers, &uri.to_string(), &body).await?;
    Ok(Json(console::audit(&c.pool, &who, decode(&body)?).await?))
}
async fn monitor(
    State(c): State<Console>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>> {
    let who = auth(&c, &headers, &uri.to_string(), &body).await?;
    Ok(Json(
        crate::monitoring::summary(&c.pool, &who, decode(&body)?).await?,
    ))
}
async fn monitor_rails(
    State(c): State<Console>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>> {
    let who = auth(&c, &headers, &uri.to_string(), &body).await?;
    Ok(Json(
        crate::monitoring::rails(&c.pool, &who, decode(&body)?).await?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;
    #[tokio::test]
    async fn console_deadline_cancels_a_stalled_request() {
        let app = Router::new()
            .route(
                "/",
                get(|| async { std::future::pending::<StatusCode>().await }),
            )
            .layer(middleware::from_fn(query_deadline))
            .layer(middleware::from_fn(no_store));
        let response = app
            .oneshot(Request::get("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()["cache-control"], "no-store");
    }
}
