//! Independent fixed-purpose listener; no decisions, execution or arbitrary asset paths.
use crate::{
    error::{Error, Result},
    human, media_review,
    output_delivery::Delivery,
};
use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{OriginalUri, State},
    http::{HeaderMap, StatusCode},
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use std::{sync::Arc, time::Duration};
#[derive(Clone)]
pub struct Service {
    pub delivery: Delivery,
    pub verifier: Arc<human::Verifier>,
}
pub fn router(service: Service) -> Router {
    Router::new()
        .route("/health/live", get(|| async { StatusCode::NO_CONTENT }))
        .route("/health/ready", get(ready))
        .route("/media-review/v1/start", post(handle))
        .route("/media-review/v1/chunk", post(handle))
        .route("/media-review/v1/complete", post(handle))
        .fallback(|| async { Error::NotFound })
        .with_state(service)
        .layer(axum::extract::DefaultBodyLimit::max(1024))
        .layer(middleware::from_fn_with_state(
            Arc::new(tokio::sync::Semaphore::new(8)),
            crate::api::http_capacity,
        ))
        .layer(middleware::from_fn(boundary))
}
async fn ready(State(s): State<Service>) -> Result<StatusCode> {
    crate::check_schema(&s.delivery.assets.pool).await?;
    Ok(StatusCode::NO_CONTENT)
}
async fn boundary(r: axum::extract::Request, n: middleware::Next) -> Response {
    let mut r = match tokio::time::timeout(Duration::from_secs(15), n.run(r)).await {
        Ok(r) => r,
        Err(_) => Error::ConsoleTimeout.into_response(),
    };
    for (k, v) in [
        ("cache-control", "no-store"),
        ("x-content-type-options", "nosniff"),
        ("referrer-policy", "no-referrer"),
        (
            "content-security-policy",
            "default-src 'none'; frame-ancestors 'none'; sandbox",
        ),
    ] {
        r.headers_mut().insert(k, v.parse().unwrap());
    }
    r
}
fn decode<T: serde::de::DeserializeOwned>(b: &[u8]) -> Result<T> {
    serde_json::from_slice(b).map_err(|_| Error::Invalid("invalid media review request"))
}
async fn handle(
    State(s): State<Service>,
    OriginalUri(uri): OriginalUri,
    h: HeaderMap,
    body: Bytes,
) -> Result<Response> {
    if uri.query().is_some() || h.contains_key("range") {
        return Err(Error::Invalid("no query strings or byte ranges"));
    }
    let token = h
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or(Error::HumanUnauthorized)?;
    let who = s
        .verifier
        .verify(&s.delivery.assets.pool, token, "POST", uri.path(), &body)
        .await?;
    match uri.path() {
        "/media-review/v1/start" => {
            Ok(Json(media_review::start(&s.delivery, &who, decode(&body)?).await?).into_response())
        }
        "/media-review/v1/complete" => Ok(Json(
            media_review::complete(&s.delivery, &who, decode(&body)?).await?,
        )
        .into_response()),
        "/media-review/v1/chunk" => {
            let permit = s
                .delivery
                .assets
                .capacity
                .clone()
                .try_acquire_owned()
                .map_err(|_| Error::Busy)?;
            let bytes = media_review::chunk(&s.delivery, &who, decode(&body)?).await?;
            let length = bytes.len().to_string();
            let stream =
                futures_util::stream::try_unfold((Some(bytes), permit), |(b, p)| async move {
                    Ok::<_, Error>(b.map(|b| (b, (None, p))))
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
        _ => Err(Error::NotFound),
    }
}
