use crate::{
    error::{Error, Result},
    gateway::Gateway,
    store,
};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, Request, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

pub fn router(pool: PgPool) -> Router {
    common(base_routes(pool))
}

pub fn gateway_router(gateway: Gateway) -> Router {
    common(
        base_routes(gateway.pool.clone())
            .merge(crate::governance_api::runtime_routes(gateway.clone()))
            .merge(
                Router::new()
                    .route("/v1/chat/completions", post(chat))
                    .route("/v1/embeddings", post(embedding))
                    .route("/v1/requests", get(request_logs))
                    .route("/v1/requests/{id}", get(request_details))
                    .route("/v1/requests/{id}/result", get(request_result))
                    .route("/v1/requests/{id}/check-result", post(check_result))
                    .with_state(gateway),
            ),
    )
}

fn common(router: Router) -> Router {
    router
        .layer(axum::extract::DefaultBodyLimit::max(2 * 1024 * 1024))
        .layer(middleware::from_fn_with_state(
            std::sync::Arc::new(tokio::sync::Semaphore::new(64)),
            http_capacity,
        ))
        .layer(middleware::from_fn(request_id))
}
pub(crate) async fn http_capacity(
    State(capacity): State<std::sync::Arc<tokio::sync::Semaphore>>,
    request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let Ok(_permit) = capacity.try_acquire() else {
        return Error::Busy.into_response();
    };
    next.run(request).await
}

fn base_routes(pool: PgPool) -> Router {
    Router::new()
        .route("/health/live", get(|| async { StatusCode::NO_CONTENT }))
        .route("/health/ready", get(ready))
        .route("/v1/models", get(models))
        .route("/v1/exchange-rates", get(exchange_rate))
        .fallback(|| async { Error::NotFound })
        .with_state(pool)
}

async fn chat(State(g): State<Gateway>, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    execute(g, headers, body, "chat").await
}
async fn embedding(
    State(g): State<Gateway>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    execute(g, headers, body, "embedding").await
}
async fn execute(g: Gateway, headers: HeaderMap, body: Value, operation: &'static str) -> Response {
    let who = match identity(&g.pool, &headers).await {
        Ok(who) => who,
        Err(e) => return e.into_response(),
    };
    let Some(key) = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
    else {
        return Error::Invalid("Idempotency-Key header required for billable requests")
            .into_response();
    };
    let turn = match headers.get("x-brokerrouter-turn-id") {
        Some(value) => match value.to_str().ok().and_then(|s| s.parse::<Uuid>().ok()) {
            Some(id) => Some(id),
            None => return Error::Invalid("invalid trusted turn ID").into_response(),
        },
        None => None,
    };
    // Detached from client disconnect; bounded by Gateway capacity and its persisted deadline.
    let worker = g.clone();
    let completed = g
        .tasks
        .spawn(async move {
            worker
                .execute_in_turn(&who, &key, body, operation, turn)
                .await
        })
        .await;
    match completed {
        Ok(Ok((id, result))) => {
            let mut response = match result {
                Ok(body) => Json(body).into_response(),
                Err(error) => error.into_response(),
            };
            response.headers_mut().insert(
                "x-brokerrouter-request-id",
                id.to_string().parse().expect("uuid header"),
            );
            response
        }
        Ok(Err(error)) => error.into_response(),
        Err(_) => Error::UpstreamUnknown.into_response(),
    }
}
async fn request_details(
    State(g): State<Gateway>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>> {
    let who = identity(&g.pool, &headers).await?;
    Ok(Json(g.request(&who, id).await?))
}

#[derive(serde::Deserialize)]
struct LogQuery {
    before: Option<Uuid>,
    #[serde(default = "default_log_limit")]
    limit: i64,
}
fn default_log_limit() -> i64 {
    20
}
async fn request_logs(
    State(g): State<Gateway>,
    headers: HeaderMap,
    Query(query): Query<LogQuery>,
) -> Result<Json<Value>> {
    let who = identity(&g.pool, &headers).await?;
    Ok(Json(g.logs(&who, query.before, query.limit).await?))
}
async fn request_result(
    State(g): State<Gateway>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>> {
    let who = identity(&g.pool, &headers).await?;
    Ok(Json(g.result(&who, id).await?))
}
async fn check_result(
    State(g): State<Gateway>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>> {
    let who = identity(&g.pool, &headers).await?;
    // Explicit POST authorizes a newly required, potentially billable check.
    let worker = g.clone();
    let task = g
        .tasks
        .spawn(async move { worker.check_result(&who, id).await });
    Ok(Json(task.await.map_err(|_| Error::Configuration)??))
}

pub(crate) async fn request_id(mut request: Request<axum::body::Body>, next: Next) -> Response {
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
    let models:Vec<String>=sqlx::query_scalar("SELECT m.id FROM models m JOIN key_models k ON k.tenant_id=m.tenant_id AND k.model_id=m.id WHERE k.tenant_id=$1 AND k.key_id=$2 AND m.active AND (EXISTS (SELECT 1 FROM endpoints e WHERE e.tenant_id=m.tenant_id AND e.model_id=m.id AND e.active) OR EXISTS(SELECT 1 FROM media_endpoints e WHERE e.tenant_id=m.tenant_id AND e.model_id=m.id AND e.active)) ORDER BY m.id").bind(who.tenant_id).bind(who.key_id).fetch_all(&pool).await?;
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
