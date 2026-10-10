//! A purpose-specific private capture listener. It has no download/scan/approval
//! route; runtime artifact bytes cannot be reached through input asset routes.
use crate::{
    api,
    assets::CHUNK_BYTES,
    error::{Error, Result},
    runtime_artifacts::{Manifest, Reserve, RuntimeArtifacts},
};
use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, Path, State},
    http::{HeaderMap, StatusCode},
    response::Response,
    routing::{get, post, put},
};
use serde::Deserialize;
use serde_json::Value;
use std::{sync::Arc, time::Duration};
use uuid::Uuid;

pub const CLAIM_HEADER: &str = "x-brokerrouter-claim-token";
const METADATA_BYTES: usize = 64 * 1024;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Complete {
    claim_token: String,
}

pub fn router(artifacts: RuntimeArtifacts) -> Router {
    let metadata = Router::new()
        .route("/v1/runtime-artifacts", post(reserve))
        .route("/v1/runtime-artifacts/{id}", get(metadata))
        .route("/v1/runtime-artifacts/{id}/manifest", post(manifest))
        .route("/v1/runtime-artifacts/{id}/complete", post(complete))
        .layer(DefaultBodyLimit::max(METADATA_BYTES));
    let chunks = Router::new()
        .route(
            "/v1/runtime-artifacts/{id}/artifacts/{ordinal}/chunks/{chunk}",
            put(upload),
        )
        .layer(DefaultBodyLimit::max(CHUNK_BYTES));
    Router::new()
        .route("/health/live", get(|| async { StatusCode::NO_CONTENT }))
        .route("/health/ready", get(ready))
        .merge(metadata)
        .merge(chunks)
        .with_state(artifacts)
        .layer(axum::middleware::from_fn_with_state(
            Arc::new(tokio::sync::Semaphore::new(32)),
            api::http_capacity,
        ))
        .layer(axum::middleware::from_fn(api::request_id))
        .layer(axum::middleware::from_fn(private_response))
}
async fn private_response(
    request: axum::http::Request<Body>,
    next: axum::middleware::Next,
) -> Response {
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert("cache-control", "private, no-store".parse().unwrap());
    response
        .headers_mut()
        .insert("x-content-type-options", "nosniff".parse().unwrap());
    response
}
async fn ready(State(a): State<RuntimeArtifacts>) -> Result<StatusCode> {
    crate::check_schema(&a.assets.pool).await?;
    Ok(StatusCode::NO_CONTENT)
}
async fn reserve(
    State(a): State<RuntimeArtifacts>,
    headers: HeaderMap,
    Json(body): Json<Reserve>,
) -> Result<Json<Value>> {
    let who = api::identity(&a.assets.pool, &headers).await?;
    Ok(Json(a.reserve(&who, body).await?))
}
async fn metadata(
    State(a): State<RuntimeArtifacts>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>> {
    let who = api::identity(&a.assets.pool, &headers).await?;
    Ok(Json(a.metadata(&who, id, claim(&headers)?).await?))
}
async fn manifest(
    State(a): State<RuntimeArtifacts>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(body): Json<Manifest>,
) -> Result<Json<Value>> {
    let who = api::identity(&a.assets.pool, &headers).await?;
    Ok(Json(a.record_manifest(&who, id, body).await?))
}
async fn upload(
    State(a): State<RuntimeArtifacts>,
    headers: HeaderMap,
    Path((id, ordinal, chunk)): Path<(Uuid, i32, i32)>,
    bytes: Bytes,
) -> Result<Json<Value>> {
    if headers.get("content-type").and_then(|v| v.to_str().ok()) != Some("application/octet-stream")
        || headers.contains_key("content-range")
        || headers.contains_key("content-encoding")
    {
        return Err(Error::Invalid(
            "runtime chunks require exact raw application/octet-stream bytes",
        ));
    }
    let token = claim(&headers)?.to_owned();
    let who = api::identity(&a.assets.pool, &headers).await?;
    let permit = a
        .assets
        .capacity
        .clone()
        .try_acquire_owned()
        .map_err(|_| Error::Busy)?;
    // The owned task and permit survive HTTP cancellation until its immutable
    // write fact settles. Shutdown must drain the same Assets TaskTracker.
    let tracker = a.assets.tasks.clone();
    let task = tracker.spawn(async move {
        let _permit = permit;
        a.put_chunk(&who, id, ordinal, chunk, &token, bytes).await
    });
    Ok(Json(task.await.map_err(|_| Error::Storage)??))
}
async fn complete(
    State(a): State<RuntimeArtifacts>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(body): Json<Complete>,
) -> Result<Json<Value>> {
    let who = api::identity(&a.assets.pool, &headers).await?;
    let permit = a
        .assets
        .capacity
        .clone()
        .try_acquire_owned()
        .map_err(|_| Error::Busy)?;
    let tracker = a.assets.tasks.clone();
    let task = tracker.spawn(async move {
        let _permit = permit;
        tokio::time::timeout(
            Duration::from_secs(300),
            a.complete(&who, id, &body.claim_token),
        )
        .await
        .map_err(|_| Error::Storage)?
    });
    Ok(Json(task.await.map_err(|_| Error::Storage)??))
}
fn claim(headers: &HeaderMap) -> Result<&str> {
    if headers.get_all(CLAIM_HEADER).iter().count() != 1 {
        return Err(Error::Invalid(
            "exactly one runtime artifact claim header is required",
        ));
    }
    headers
        .get(CLAIM_HEADER)
        .and_then(|v| v.to_str().ok())
        .ok_or(Error::Invalid("runtime artifact claim header is required"))
}
