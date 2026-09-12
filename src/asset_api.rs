use crate::{
    api,
    assets::{Assets, CHUNK_BYTES, CreateAsset},
    error::{Error, Result},
};
use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use serde_json::Value;
use uuid::Uuid;

pub fn router(assets: Assets) -> Router {
    Router::new()
        .route("/health/live", get(|| async { StatusCode::NO_CONTENT }))
        .route("/health/ready", get(ready))
        .route("/v1/assets", post(create))
        .route("/v1/assets/{id}", get(metadata).delete(delete))
        .route(
            "/v1/assets/{id}/chunks/{ordinal}",
            put(upload).get(download_chunk),
        )
        .route("/v1/assets/{id}/complete", post(complete))
        .route("/v1/assets/{id}/content", get(content))
        .with_state(assets)
        .layer(axum::extract::DefaultBodyLimit::max(CHUNK_BYTES))
        .layer(axum::middleware::from_fn_with_state(
            std::sync::Arc::new(tokio::sync::Semaphore::new(32)),
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
async fn ready(State(a): State<Assets>) -> Result<StatusCode> {
    crate::check_schema(&a.pool).await?;
    Ok(StatusCode::NO_CONTENT)
}
async fn create(
    State(a): State<Assets>,
    headers: HeaderMap,
    Json(body): Json<CreateAsset>,
) -> Result<Json<Value>> {
    let who = api::identity(&a.pool, &headers).await?;
    let idem = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .ok_or(Error::Invalid("Idempotency-Key required"))?;
    Ok(Json(a.create(&who, idem, body).await?))
}
async fn metadata(
    State(a): State<Assets>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>> {
    let who = api::identity(&a.pool, &headers).await?;
    Ok(Json(a.metadata(&who, id).await?))
}
async fn delete(
    State(a): State<Assets>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>> {
    let who = api::identity(&a.pool, &headers).await?;
    Ok(Json(a.delete(&who, id).await?))
}
async fn upload(
    State(a): State<Assets>,
    headers: HeaderMap,
    Path((id, ordinal)): Path<(Uuid, i32)>,
    bytes: Bytes,
) -> Result<Json<Value>> {
    let who = api::identity(&a.pool, &headers).await?;
    let permit = a
        .capacity
        .clone()
        .try_acquire_owned()
        .map_err(|_| Error::Busy)?;
    let worker = a.clone();
    let task = a.tasks.spawn(async move {
        let _permit = permit;
        worker.put_chunk(&who, id, ordinal, bytes).await
    });
    Ok(Json(task.await.map_err(|_| Error::Storage)??))
}
async fn complete(
    State(a): State<Assets>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>> {
    let who = api::identity(&a.pool, &headers).await?;
    let permit = a
        .capacity
        .clone()
        .try_acquire_owned()
        .map_err(|_| Error::Busy)?;
    let worker = a.clone();
    let task = a.tasks.spawn(async move {
        let _permit = permit;
        tokio::time::timeout(
            std::time::Duration::from_secs(300),
            worker.complete(&who, id),
        )
        .await
        .map_err(|_| Error::Storage)?
    });
    Ok(Json(task.await.map_err(|_| Error::Storage)??))
}
async fn download_chunk(
    State(a): State<Assets>,
    headers: HeaderMap,
    Path((id, ordinal)): Path<(Uuid, i32)>,
) -> Result<Response> {
    let who = api::identity(&a.pool, &headers).await?;
    let _permit = a
        .capacity
        .clone()
        .try_acquire_owned()
        .map_err(|_| Error::Busy)?;
    Ok((
        [
            ("content-type", "application/octet-stream"),
            ("content-disposition", "attachment"),
        ],
        a.content_chunk(&who, id, ordinal).await?,
    )
        .into_response())
}
async fn content(
    State(a): State<Assets>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Response> {
    if headers.contains_key("range") {
        return Err(Error::Invalid(
            "HTTP ranges are not supported; use numbered chunk downloads",
        ));
    }
    let who = api::identity(&a.pool, &headers).await?;
    let permit = a
        .capacity
        .clone()
        .try_acquire_owned()
        .map_err(|_| Error::Busy)?;
    let meta = a.metadata(&who, id).await?;
    if meta["state"] != "available" {
        return Err(Error::ResultUnavailable);
    }
    let count = meta["chunk_count"].as_i64().ok_or(Error::Configuration)? as i32;
    let length = meta["byte_length"].as_i64().ok_or(Error::Configuration)?;
    // The owned permit lives in the response body, not just the handler.
    let first = a.content_chunk(&who, id, 0).await?;
    let stream = futures_util::stream::try_unfold(
        (a, who, id, 0, count, Some(first), permit),
        |(a, who, id, n, count, first, permit)| async move {
            if n >= count {
                return Ok::<_, Error>(None);
            }
            let bytes = if let Some(bytes) = first {
                bytes
            } else {
                a.content_chunk(&who, id, n).await?
            };
            Ok(Some((bytes, (a, who, id, n + 1, count, None, permit))))
        },
    );
    let mut response = Body::from_stream(stream).into_response();
    response
        .headers_mut()
        .insert("content-type", "application/octet-stream".parse().unwrap());
    response
        .headers_mut()
        .insert("content-disposition", "attachment".parse().unwrap());
    response.headers_mut().insert(
        "content-length",
        length
            .to_string()
            .parse()
            .map_err(|_| Error::Configuration)?,
    );
    Ok(response)
}
