use crate::{
    api,
    error::{Error, Result},
    output_scans::{self, Report, Scanner, Scans},
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
use serde_json::json;
use uuid::Uuid;

pub fn router(scans: Scans) -> Router {
    Router::new()
        .route("/health/live", get(|| async { StatusCode::NO_CONTENT }))
        .route("/health/ready", get(ready))
        .route("/v1/output-scans/claim", post(claim))
        .route("/v1/output-scans/attempts/{id}/renew", post(renew))
        .route(
            "/v1/output-scans/attempts/{id}/chunks/{ordinal}",
            get(chunk),
        )
        .route("/v1/output-scans/attempts/{id}/report", post(report))
        .with_state(scans)
        .layer(axum::extract::DefaultBodyLimit::max(16384))
        .layer(axum::middleware::from_fn_with_state(
            std::sync::Arc::new(tokio::sync::Semaphore::new(16)),
            api::http_capacity,
        ))
        .layer(axum::middleware::from_fn(api::request_id))
        .layer(axum::middleware::from_fn(
            |r: axum::http::Request<Body>, n: axum::middleware::Next| async move {
                let mut r = n.run(r).await;
                r.headers_mut()
                    .insert("cache-control", "private, no-store".parse().unwrap());
                r.headers_mut()
                    .insert("x-content-type-options", "nosniff".parse().unwrap());
                r.headers_mut().insert(
                    "content-security-policy",
                    "default-src 'none'; sandbox".parse().unwrap(),
                );
                r
            },
        ))
}
async fn ready(State(s): State<Scans>) -> Result<StatusCode> {
    crate::check_schema(&s.assets.pool).await?;
    Ok(StatusCode::NO_CONTENT)
}
async fn identity(s: &Scans, h: &HeaderMap) -> Result<Scanner> {
    let token = h
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or(Error::ScannerUnauthorized)?;
    output_scans::authenticate(&s.assets.pool, token).await
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Claim {
    claim_id: Uuid,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}
async fn claim(
    State(s): State<Scans>,
    h: HeaderMap,
    Json(c): Json<Claim>,
) -> Result<Json<serde_json::Value>> {
    let who = identity(&s, &h).await?;
    Ok(Json(
        s.claim(&who, c.claim_id)
            .await?
            .unwrap_or(json!({"state":"no_assignment"})),
    ))
}
async fn renew(
    State(s): State<Scans>,
    Path(id): Path<Uuid>,
    h: HeaderMap,
    Json(_): Json<Empty>,
) -> Result<Json<serde_json::Value>> {
    let who = identity(&s, &h).await?;
    Ok(Json(s.renew(&who, id).await?))
}
async fn report(
    State(s): State<Scans>,
    Path(id): Path<Uuid>,
    h: HeaderMap,
    Json(r): Json<Report>,
) -> Result<Json<serde_json::Value>> {
    let who = identity(&s, &h).await?;
    Ok(Json(s.report(&who, id, r).await?))
}
async fn chunk(
    State(s): State<Scans>,
    Path((id, n)): Path<(Uuid, i32)>,
    h: HeaderMap,
) -> Result<Response> {
    if h.contains_key("range") {
        return Err(Error::Invalid(
            "use exact numbered scanner chunks, not HTTP ranges",
        ));
    }
    let who = identity(&s, &h).await?;
    let permit = s
        .assets
        .capacity
        .clone()
        .try_acquire_owned()
        .map_err(|_| Error::Busy)?;
    let bytes = s.chunk(&who, id, n).await?;
    // Hold bounded capacity until the response body is consumed or disconnected.
    let stream =
        futures_util::stream::try_unfold((Some(bytes), permit), |(bytes, permit)| async move {
            Ok::<_, Error>(bytes.map(|bytes| (bytes, (None, permit))))
        });
    Ok((
        [
            ("content-type", "application/octet-stream"),
            ("content-disposition", "attachment"),
        ],
        Body::from_stream(stream),
    )
        .into_response())
}
