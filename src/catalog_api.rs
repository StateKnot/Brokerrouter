//! Separate listener, verifier and database role for catalog management.
use crate::{
    api, catalog,
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
pub struct Catalog {
    pub pool: PgPool,
    pub verifier: Arc<human::Verifier>,
    pub connections: Arc<Vec<catalog::Connection>>,
}
pub fn router(service: Catalog) -> Router {
    Router::new()
        .route("/health/live", get(|| async { StatusCode::NO_CONTENT }))
        .route("/health/ready", get(ready))
        .route("/catalog/v1/models/query", post(query))
        .route("/catalog/v1/endpoints/query", post(query))
        .route("/catalog/v1/apply", post(query))
        .fallback(|| async { Error::NotFound })
        .with_state(service)
        .layer(axum::extract::DefaultBodyLimit::max(16384))
        .layer(middleware::from_fn_with_state(
            Arc::new(tokio::sync::Semaphore::new(8)),
            api::http_capacity,
        ))
        .layer(middleware::from_fn(api::request_id))
        .layer(middleware::from_fn(boundary))
}
async fn ready(State(c): State<Catalog>) -> Result<StatusCode> {
    crate::check_schema(&c.pool).await?;
    Ok(StatusCode::NO_CONTENT)
}
async fn boundary(request: axum::extract::Request, next: middleware::Next) -> Response {
    let mut response =
        match tokio::time::timeout(std::time::Duration::from_secs(10), next.run(request)).await {
            Ok(r) => r,
            Err(_) => Error::ConsoleTimeout.into_response(),
        };
    for (k, v) in [
        ("cache-control", "no-store"),
        ("x-content-type-options", "nosniff"),
        ("referrer-policy", "no-referrer"),
        (
            "content-security-policy",
            "default-src 'none'; frame-ancestors 'none'",
        ),
    ] {
        response
            .headers_mut()
            .insert(k, v.parse().expect("static header"));
    }
    response
}
async fn query(
    State(c): State<Catalog>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>> {
    if headers.get_all("authorization").iter().count() != 1 {
        return Err(Error::HumanUnauthorized);
    }
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or(Error::HumanUnauthorized)?;
    let who = c
        .verifier
        .verify(&c.pool, token, "POST", &uri.to_string(), &body)
        .await?;
    fn decode<T: serde::de::DeserializeOwned>(body: &Bytes) -> Result<T> {
        serde_json::from_slice(body).map_err(|_| Error::Invalid("invalid catalog request"))
    }
    Ok(Json(match uri.path() {
        "/catalog/v1/models/query" => catalog::models(&c.pool, &who, decode(&body)?).await?,
        "/catalog/v1/endpoints/query" => {
            catalog::endpoints(&c.pool, &who, decode(&body)?, &c.connections).await?
        }
        "/catalog/v1/apply" => {
            catalog::apply(&c.pool, &who, decode(&body)?, &c.connections).await?
        }
        _ => return Err(Error::NotFound),
    }))
}
