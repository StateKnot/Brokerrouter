use crate::{
    api,
    error::{Error, Result},
    human,
    output_delivery::Delivery,
};
use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{OriginalUri, Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::Value;
use std::sync::Arc;
use uuid::Uuid;

#[derive(Clone)]
pub struct Service {
    pub delivery: Delivery,
    pub verifier: Arc<human::Verifier>,
}
pub fn router(service: Service) -> Router {
    Router::new()
        .route("/health/live", get(|| async { StatusCode::NO_CONTENT }))
        .route("/health/ready", get(ready))
        .route("/v1/media/jobs/{id}/output/check", post(check))
        .route("/v1/media/jobs/{id}/output", get(manifest))
        .route("/v1/media/jobs/{id}/output/chunks/{n}", get(chunk))
        .route("/v1/approvals/{id}/output/review", post(review_manifest))
        .route(
            "/v1/approvals/{id}/output/review/chunks/{n}",
            post(review_chunk),
        )
        .with_state(service)
        .layer(axum::extract::DefaultBodyLimit::max(1024))
        .layer(axum::middleware::from_fn_with_state(
            Arc::new(tokio::sync::Semaphore::new(16)),
            api::http_capacity,
        ))
        .layer(axum::middleware::from_fn(api::request_id))
        .layer(axum::middleware::from_fn(
            |r: axum::http::Request<Body>, n: axum::middleware::Next| async move {
                let mut r = n.run(r).await;
                for (key, value) in [
                    ("cache-control", "private, no-store"),
                    ("x-content-type-options", "nosniff"),
                    ("content-security-policy", "default-src 'none'; sandbox"),
                    ("referrer-policy", "no-referrer"),
                ] {
                    r.headers_mut().insert(key, value.parse().unwrap());
                }
                r
            },
        ))
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}
async fn ready(State(s): State<Service>) -> Result<StatusCode> {
    crate::check_schema(&s.delivery.assets.pool).await?;
    Ok(StatusCode::NO_CONTENT)
}
async fn check(
    State(s): State<Service>,
    Path(id): Path<Uuid>,
    h: HeaderMap,
    Json(_): Json<Empty>,
) -> Result<Json<Value>> {
    let who = api::identity(&s.delivery.assets.pool, &h).await?;
    Ok(Json(s.delivery.check(&who, id).await?))
}
async fn manifest(
    State(s): State<Service>,
    Path(id): Path<Uuid>,
    h: HeaderMap,
) -> Result<Json<Value>> {
    let who = api::identity(&s.delivery.assets.pool, &h).await?;
    Ok(Json(s.delivery.manifest(&who, id).await?))
}
async fn human(s: &Service, h: &HeaderMap, path: &str, body: &Bytes) -> Result<human::Human> {
    serde_json::from_slice::<Empty>(body)
        .map_err(|_| Error::Invalid("expected empty review object"))?;
    let token = h
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or(Error::HumanUnauthorized)?;
    s.verifier
        .verify(&s.delivery.assets.pool, token, "POST", path, body)
        .await
}
async fn review_manifest(
    State(s): State<Service>,
    Path(id): Path<Uuid>,
    OriginalUri(uri): OriginalUri,
    h: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>> {
    let human = human(&s, &h, &uri.to_string(), &body).await?;
    Ok(Json(s.delivery.review_manifest(&human, id).await?))
}
async fn chunk(
    State(s): State<Service>,
    Path((id, n)): Path<(Uuid, i32)>,
    h: HeaderMap,
) -> Result<Response> {
    no_range(&h)?;
    let who = api::identity(&s.delivery.assets.pool, &h).await?;
    let permit = capacity(&s)?;
    let bytes = s.delivery.chunk(&who, id, n).await?;
    Ok(binary(bytes, permit))
}
async fn review_chunk(
    State(s): State<Service>,
    Path((id, n)): Path<(Uuid, i32)>,
    OriginalUri(uri): OriginalUri,
    h: HeaderMap,
    body: Bytes,
) -> Result<Response> {
    no_range(&h)?;
    let human = human(&s, &h, &uri.to_string(), &body).await?;
    let permit = capacity(&s)?;
    let bytes = s.delivery.review_chunk(&human, id, n).await?;
    Ok(binary(bytes, permit))
}
fn no_range(h: &HeaderMap) -> Result<()> {
    if h.contains_key("range") {
        return Err(Error::Invalid(
            "use exact numbered output chunks, not HTTP ranges",
        ));
    }
    Ok(())
}
fn capacity(s: &Service) -> Result<tokio::sync::OwnedSemaphorePermit> {
    s.delivery
        .assets
        .capacity
        .clone()
        .try_acquire_owned()
        .map_err(|_| Error::Busy)
}
fn binary(bytes: Bytes, permit: tokio::sync::OwnedSemaphorePermit) -> Response {
    let length = bytes.len().to_string();
    let stream =
        futures_util::stream::try_unfold((Some(bytes), permit), |(bytes, permit)| async move {
            Ok::<_, Error>(bytes.map(|bytes| (bytes, (None, permit))))
        });
    (
        [
            ("content-type", "application/octet-stream".to_owned()),
            ("content-disposition", "attachment".to_owned()),
            ("content-length", length),
        ],
        Body::from_stream(stream),
    )
        .into_response()
}
