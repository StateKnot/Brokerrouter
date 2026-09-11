use crate::{
    error::{Error, Result},
    store,
};
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, Request, StatusCode},
    middleware::{self, Next},
    response::Response,
    routing::get,
};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

pub fn router(pool: PgPool) -> Router {
    Router::new()
        .route("/health/live", get(|| async { StatusCode::NO_CONTENT }))
        .route("/health/ready", get(ready))
        .route("/v1/models", get(models))
        .route("/v1/exchange-rates", get(exchange_rate))
        .fallback(|| async { Error::NotFound })
        .layer(axum::extract::DefaultBodyLimit::max(2 * 1024 * 1024))
        .layer(middleware::from_fn(request_id))
        .with_state(pool)
}

async fn request_id(mut request: Request<axum::body::Body>, next: Next) -> Response {
    let id = Uuid::new_v4().to_string();
    request
        .headers_mut()
        .insert("x-request-id", id.parse().expect("uuid header"));
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert("x-request-id", id.parse().expect("uuid header"));
    response
}

async fn ready(State(pool): State<PgPool>) -> Result<StatusCode> {
    crate::check_schema(&pool).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn identity(pool: &PgPool, headers: &HeaderMap) -> Result<store::Identity> {
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .ok_or(Error::Unauthorized)?;
    store::authenticate(pool, token).await
}

async fn models(State(pool): State<PgPool>, headers: HeaderMap) -> Result<Json<Value>> {
    let who = identity(&pool, &headers).await?;
    let models:Vec<String>=sqlx::query_scalar("SELECT m.id FROM models m JOIN key_models k ON k.tenant_id=m.tenant_id AND k.model_id=m.id WHERE k.tenant_id=$1 AND k.key_id=$2 AND m.active AND EXISTS (SELECT 1 FROM endpoints e WHERE e.tenant_id=m.tenant_id AND e.model_id=m.id AND e.active) ORDER BY m.id").bind(who.tenant_id).bind(who.key_id).fetch_all(&pool).await?;
    Ok(Json(
        json!({"object":"list","data":models.into_iter().map(|id|json!({"id":id,"object":"model","owned_by":"brokerrouter"})).collect::<Vec<_>>()}),
    ))
}

async fn exchange_rate(State(pool): State<PgPool>, headers: HeaderMap) -> Result<Json<Value>> {
    let who = identity(&pool, &headers).await?;
    Ok(Json(
        json!({"primary_currency":"CNY","quote":"CNY per 1 USD","effective":store::current_fx(&pool,who.tenant_id).await?}),
    ))
}
