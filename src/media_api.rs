use crate::{
    api,
    error::{Error, Result},
    media::Media,
    media_protocol::CreateJob,
};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;
pub fn router(m: Media) -> Router {
    Router::new()
        .route("/health/live", get(|| async { StatusCode::NO_CONTENT }))
        .route("/health/ready", get(ready))
        .route("/v1/media/capabilities", get(capabilities))
        .route("/v1/media/jobs", post(create))
        .route("/v1/media/jobs/{id}", get(read))
        .route("/v1/media/jobs/{id}/events", get(events))
        .route("/v1/media/jobs/{id}/cancel", post(cancel))
        .with_state(m)
        .layer(axum::extract::DefaultBodyLimit::max(32768))
        .layer(axum::middleware::from_fn_with_state(
            std::sync::Arc::new(tokio::sync::Semaphore::new(32)),
            api::http_capacity,
        ))
        .layer(axum::middleware::from_fn(api::request_id))
        .layer(axum::middleware::from_fn(
            |r: axum::http::Request<axum::body::Body>, n: axum::middleware::Next| async move {
                let mut r = n.run(r).await;
                r.headers_mut()
                    .insert("cache-control", "private, no-store".parse().unwrap());
                r
            },
        ))
}
async fn ready(State(m): State<Media>) -> Result<StatusCode> {
    crate::check_schema(&m.gateway.pool).await?;
    Ok(StatusCode::NO_CONTENT)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelQuery {
    model: String,
}
async fn capabilities(
    State(m): State<Media>,
    h: HeaderMap,
    Query(q): Query<ModelQuery>,
) -> Result<Json<Value>> {
    let who = api::identity(&m.gateway.pool, &h).await?;
    Ok(Json(m.capabilities(&who, &q.model).await?))
}
async fn create(
    State(m): State<Media>,
    h: HeaderMap,
    Json(body): Json<CreateJob>,
) -> Result<(StatusCode, Json<Value>)> {
    let who = api::identity(&m.gateway.pool, &h).await?;
    let idem = h
        .get("idempotency-key")
        .and_then(|s| s.to_str().ok())
        .ok_or(Error::Invalid("Idempotency-Key required"))?;
    let turn = h
        .get("x-brokerrouter-turn-id")
        .map(|s| {
            s.to_str()
                .ok()
                .and_then(|s| s.parse::<Uuid>().ok())
                .ok_or(Error::GovernanceContext)
        })
        .transpose()?;
    Ok((
        StatusCode::ACCEPTED,
        Json(m.create(&who, idem, body, turn).await?),
    ))
}
async fn read(State(m): State<Media>, h: HeaderMap, Path(id): Path<Uuid>) -> Result<Json<Value>> {
    let who = api::identity(&m.gateway.pool, &h).await?;
    Ok(Json(m.get(&who, id).await?))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    #[serde(default)]
    after: i64,
}
async fn events(
    State(m): State<Media>,
    h: HeaderMap,
    Path(id): Path<Uuid>,
    Query(q): Query<Cursor>,
) -> Result<Json<Value>> {
    let who = api::identity(&m.gateway.pool, &h).await?;
    Ok(Json(m.events(&who, id, q.after).await?))
}
async fn cancel(State(m): State<Media>, h: HeaderMap, Path(id): Path<Uuid>) -> Result<Json<Value>> {
    let who = api::identity(&m.gateway.pool, &h).await?;
    Ok(Json(m.cancel(&who, id).await?))
}
