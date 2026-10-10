//! Separate authenticated native file reader. It never accepts an upload,
//! detector verdict, human decision or a filesystem/provider URL.
use crate::{
    api,
    error::{Error, Result},
    runtime_artifact_delivery::Delivery,
};
use axum::{
    Json, Router,
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;
use uuid::Uuid;

pub fn router(delivery: Delivery) -> Router {
    Router::new()
        .route("/health/live", get(|| async { StatusCode::NO_CONTENT }))
        .route("/health/ready", get(ready))
        .route("/v1/runtime-artifacts/{id}/release", post(release))
        .route("/v1/runtime-artifacts/{id}/manifest", get(manifest))
        .route("/v1/runtime-artifacts/{id}/chunks/{ordinal}", get(chunk))
        .with_state(delivery)
        .layer(axum::extract::DefaultBodyLimit::max(1024))
        .layer(axum::middleware::from_fn_with_state(
            Arc::new(tokio::sync::Semaphore::new(16)),
            api::http_capacity,
        ))
        .layer(axum::middleware::from_fn(api::request_id))
        .layer(axum::middleware::from_fn(strict_request))
        .layer(axum::middleware::from_fn(
            |r: axum::http::Request<Body>, next: axum::middleware::Next| async move {
                let mut response = next.run(r).await;
                for (name, value) in [
                    ("cache-control", "private, no-store"),
                    ("x-content-type-options", "nosniff"),
                    ("content-security-policy", "default-src 'none'; sandbox"),
                    ("referrer-policy", "no-referrer"),
                ] {
                    response.headers_mut().insert(name, value.parse().unwrap());
                }
                response
            },
        ))
}
async fn ready(State(d): State<Delivery>) -> Result<StatusCode> {
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        crate::check_schema(&d.assets.pool),
    )
    .await
    .map_err(|_| Error::SchemaMismatch)??;
    Ok(StatusCode::NO_CONTENT)
}
async fn strict_request(
    request: axum::http::Request<Body>,
    next: axum::middleware::Next,
) -> Response {
    if request.uri().query().is_some()
        || request.headers().contains_key("range")
        || request
            .headers()
            .get("content-encoding")
            .is_some_and(|v| v != "identity")
    {
        return Error::Invalid("native delivery rejects query, ranges and encoded request bodies")
            .into_response();
    }
    next.run(request).await
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}
async fn release(
    State(d): State<Delivery>,
    Path(id): Path<Uuid>,
    h: HeaderMap,
    Json(_): Json<Empty>,
) -> Result<Json<Value>> {
    let who = api::identity(&d.assets.pool, &h).await?;
    let permit = d
        .assets
        .capacity
        .clone()
        .try_acquire_owned()
        .map_err(|_| Error::Busy)?;
    // If the caller disconnects, the bounded worker finishes the durable
    // operation/approval transaction. A retry binds the same immutable scan.
    let worker = d.clone();
    let task = d.assets.tasks.spawn(async move {
        let _permit = permit;
        worker.release(&who, id).await
    });
    Ok(Json(task.await.map_err(|_| Error::Storage)??))
}
async fn manifest(
    State(d): State<Delivery>,
    Path(id): Path<Uuid>,
    h: HeaderMap,
) -> Result<Json<Value>> {
    let who = api::identity(&d.assets.pool, &h).await?;
    Ok(Json(d.manifest(&who, id).await?))
}
async fn chunk(
    State(d): State<Delivery>,
    Path((id, ordinal)): Path<(Uuid, i32)>,
    h: HeaderMap,
) -> Result<Response> {
    let who = api::identity(&d.assets.pool, &h).await?;
    let permit = d
        .assets
        .capacity
        .clone()
        .try_acquire_owned()
        .map_err(|_| Error::Busy)?;
    let bytes = d.chunk(&who, id, ordinal).await?;
    let length = bytes.len().to_string();
    let stream =
        futures_util::stream::try_unfold((Some(bytes), permit), |(bytes, permit)| async move {
            Ok::<_, Error>(bytes.map(|bytes| (bytes, (None, permit))))
        });
    Ok((
        [
            ("content-type", "application/octet-stream".to_owned()),
            ("content-disposition", "attachment".to_owned()),
            ("content-length", length),
        ],
        Body::from_stream(stream),
    )
        .into_response())
}
