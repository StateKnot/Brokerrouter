use crate::{
    api,
    cipher::ResultCipher,
    error::{Error, Result},
    gateway::Gateway,
    governance, human, mcp_skill_approvals, mcp_skill_resources, runtime_results,
};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{OriginalUri, Path, Query, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use serde_json::Value;
use sqlx::PgPool;
use std::sync::Arc;
use uuid::Uuid;

pub fn runtime_routes(gateway: Gateway) -> Router {
    Router::new()
        .route("/v1/governance/checks", post(check))
        .route("/v1/governance/tool-results", post(record_tool_result))
        .route(
            "/v1/governance/tool-results/{id}/release",
            post(release_tool_result),
        )
        .route("/v1/approvals/{id}", get(approval))
        .route("/v1/governance/operations/{id}", get(operation))
        .route("/v1/governance/operations/{id}/claim", post(claim))
        .route("/v1/governance/operations/{id}/receipt", post(receipt))
        .route("/v1/governance/turns/{id}/approvals", get(approvals))
        .route("/v1/governance/turns/{id}/bind", post(bind_runtime_turn))
        .route("/v1/governance/turns/{id}/events", get(events))
        .route("/v1/governance/turns/{id}/ack", post(ack))
        .route("/v1/governance/turns/{id}/close", post(runtime_close))
        .route("/v1/governance/skills/requests", post(skill_request))
        .route("/v1/governance/skills/requests/{id}", get(skill_status))
        .route(
            "/v1/governance/skills/requests/{id}/resources",
            post(skill_resource_request),
        )
        .route(
            "/v1/governance/skills/resources/{id}",
            get(skill_resource_status),
        )
        .with_state(gateway)
}
async fn skill_resource_request(
    State(g): State<Gateway>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(body): Json<mcp_skill_resources::ReadRequest>,
) -> Result<Json<Value>> {
    let who = api::identity(&g.pool, &headers).await?;
    Ok(Json(
        mcp_skill_resources::request(&g.pool, &who, id, body).await?,
    ))
}
async fn skill_resource_status(
    State(g): State<Gateway>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>> {
    let who = api::identity(&g.pool, &headers).await?;
    Ok(Json(
        mcp_skill_resources::status(&g.pool, &g.cipher, &who, id).await?,
    ))
}
async fn skill_request(
    State(g): State<Gateway>,
    headers: HeaderMap,
    Json(body): Json<mcp_skill_approvals::Request>,
) -> Result<Json<Value>> {
    let who = api::identity(&g.pool, &headers).await?;
    Ok(Json(
        mcp_skill_approvals::request(&g.pool, &who, body).await?,
    ))
}
async fn skill_status(
    State(g): State<Gateway>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>> {
    let who = api::identity(&g.pool, &headers).await?;
    Ok(Json(
        mcp_skill_approvals::status(&g.pool, &g.cipher, &who, id).await?,
    ))
}
async fn check(
    State(g): State<Gateway>,
    headers: HeaderMap,
    Json(check): Json<governance::Check>,
) -> Result<Json<Value>> {
    if check
        .operation_key
        .starts_with(runtime_results::OUTPUT_PREFIX)
    {
        return Err(Error::Invalid("reserved tool result operation key"));
    }
    if !matches!(
        check.stage.as_str(),
        "tool_input" | "tool_output" | "artifact"
    ) || check.tool_name.is_none()
    {
        return Err(Error::Invalid(
            "runtime checks require a named tool or artifact action; model checks are automatic",
        ));
    }
    let who = api::identity(&g.pool, &headers).await?;
    // HTTP disconnect must not abandon an owned, potentially billable check.
    let worker = g.clone();
    let task = g
        .tasks
        .spawn(async move { worker.check(&who, check).await });
    Ok(Json(task.await.map_err(|_| Error::Configuration)??))
}
async fn record_tool_result(
    State(g): State<Gateway>,
    headers: HeaderMap,
    Json(body): Json<runtime_results::Record>,
) -> Result<Json<Value>> {
    let who = api::identity(&g.pool, &headers).await?;
    let worker = g.clone();
    let task = g.tasks.spawn(async move {
        runtime_results::record(&worker.pool, &worker.cipher, &who, body).await
    });
    Ok(Json(task.await.map_err(|_| Error::Configuration)??))
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ReleaseToolResult {}
async fn release_tool_result(
    State(g): State<Gateway>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(_): Json<ReleaseToolResult>,
) -> Result<Json<Value>> {
    let who = api::identity(&g.pool, &headers).await?;
    let worker = g.clone();
    // A lost response must not cancel a billable semantic check or its settlement.
    let task = g
        .tasks
        .spawn(async move { runtime_results::release(&worker, &who, id).await });
    Ok(Json(task.await.map_err(|_| Error::Configuration)??))
}
async fn generic_operation(g: &Gateway, who: &crate::store::Identity, id: Uuid) -> Result<()> {
    let key: String = sqlx::query_scalar(
        "SELECT operation_key FROM governance_operations WHERE tenant_id=$1 AND key_id=$2 AND id=$3",
    )
    .bind(who.tenant_id)
    .bind(who.key_id)
    .bind(id)
    .fetch_optional(&g.pool)
    .await?
    .ok_or(Error::NotFound)?;
    if key.starts_with(runtime_results::OUTPUT_PREFIX) {
        return Err(Error::Invalid(
            "tool result outputs are released without runtime claims or receipts",
        ));
    }
    Ok(())
}
async fn operation(
    State(g): State<Gateway>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>> {
    let who = api::identity(&g.pool, &headers).await?;
    Ok(Json(
        governance::operation(&g.pool, &g.cipher, &who, id).await?,
    ))
}
async fn claim(
    State(g): State<Gateway>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(body): Json<governance::Claim>,
) -> Result<Json<Value>> {
    let who = api::identity(&g.pool, &headers).await?;
    generic_operation(&g, &who, id).await?;
    Ok(Json(
        governance::claim(&g.pool, &g.cipher, &who, id, body).await?,
    ))
}
async fn receipt(
    State(g): State<Gateway>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(body): Json<governance::Receipt>,
) -> Result<Json<Value>> {
    let who = api::identity(&g.pool, &headers).await?;
    generic_operation(&g, &who, id).await?;
    Ok(Json(
        governance::receipt(&g.pool, &g.cipher, &who, id, body).await?,
    ))
}
async fn approvals(
    State(g): State<Gateway>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>> {
    let who = api::identity(&g.pool, &headers).await?;
    Ok(Json(
        governance::approvals(&g.pool, &g.cipher, &who, id).await?,
    ))
}
async fn bind_runtime_turn(
    State(g): State<Gateway>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(body): Json<governance::RuntimeTurnBinding>,
) -> Result<Json<Value>> {
    let who = api::identity(&g.pool, &headers).await?;
    Ok(Json(
        governance::bind_runtime_turn(&g.pool, &who, id, body).await?,
    ))
}
#[derive(serde::Deserialize)]
struct Cursor {
    #[serde(default)]
    after: i64,
}
async fn events(
    State(g): State<Gateway>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Query(q): Query<Cursor>,
) -> Result<Json<Value>> {
    let who = api::identity(&g.pool, &headers).await?;
    Ok(Json(
        governance::events(&g.pool, &who, id, q.after, None).await?,
    ))
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Ack {
    sequence: i64,
}
async fn ack(
    State(g): State<Gateway>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(q): Json<Ack>,
) -> Result<Json<Value>> {
    let who = api::identity(&g.pool, &headers).await?;
    Ok(Json(
        governance::events(&g.pool, &who, id, q.sequence, Some(q.sequence)).await?,
    ))
}

#[derive(Clone)]
pub struct Control {
    pub pool: PgPool,
    pub verifier: Arc<human::Verifier>,
    pub cipher: Arc<ResultCipher>,
}
pub fn control_router(control: Control) -> Router {
    Router::new()
        .route("/health/live", get(|| async { StatusCode::NO_CONTENT }))
        .route("/health/ready", get(control_ready))
        .route("/v1/governance/turns", post(create_turn))
        .route("/v1/governance/turns/{id}/close", post(close_turn))
        .route("/v1/approvals/{id}/resolve", post(resolve))
        .route("/v1/approvals/query", post(query_approvals))
        .route("/v1/governance/operations/{id}/reconcile", post(reconcile))
        .with_state(control)
        .layer(axum::extract::DefaultBodyLimit::max(65536))
        .layer(axum::middleware::from_fn_with_state(
            Arc::new(tokio::sync::Semaphore::new(32)),
            api::http_capacity,
        ))
        .layer(axum::middleware::from_fn(api::request_id))
}
async fn control_ready(State(c): State<Control>) -> Result<StatusCode> {
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        crate::check_schema(&c.pool),
    )
    .await
    .map_err(|_| Error::Database(sqlx::Error::PoolTimedOut))??;
    Ok(StatusCode::NO_CONTENT)
}
async fn auth(c: &Control, headers: &HeaderMap, path: &str, body: &Bytes) -> Result<human::Human> {
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or(Error::HumanUnauthorized)?;
    c.verifier.verify(&c.pool, token, "POST", path, body).await
}
fn decode<T: serde::de::DeserializeOwned>(body: &Bytes) -> Result<T> {
    serde_json::from_slice(body).map_err(|_| Error::Invalid("invalid control request"))
}
async fn create_turn(
    State(c): State<Control>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>> {
    let who = auth(&c, &headers, &uri.to_string(), &body).await?;
    Ok(Json(
        human::create_turn(&c.pool, &who, decode(&body)?).await?,
    ))
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Close {
    state: String,
}
async fn runtime_close(
    State(g): State<Gateway>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    Json(body): Json<Close>,
) -> Result<Json<Value>> {
    let who = api::identity(&g.pool, &headers).await?;
    Ok(Json(
        governance::close_runtime(&g.pool, &who, id, &body.state).await?,
    ))
}
async fn close_turn(
    State(c): State<Control>,
    Path(id): Path<Uuid>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>> {
    let who = auth(&c, &headers, &uri.to_string(), &body).await?;
    let close: Close = decode(&body)?;
    Ok(Json(
        human::close_turn(&c.pool, &who, id, &close.state).await?,
    ))
}
async fn resolve(
    State(c): State<Control>,
    Path(id): Path<Uuid>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>> {
    let who = auth(&c, &headers, &uri.to_string(), &body).await?;
    Ok(Json(
        human::resolve(&c.pool, &c.cipher, &who, id, decode(&body)?).await?,
    ))
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ApprovalQuery {
    turn_id: Uuid,
}
async fn query_approvals(
    State(c): State<Control>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>> {
    let who = auth(&c, &headers, &uri.to_string(), &body).await?;
    let q: ApprovalQuery = decode(&body)?;
    Ok(Json(
        human::list_approvals(&c.pool, &c.cipher, &who, q.turn_id).await?,
    ))
}
async fn approval(
    State(g): State<Gateway>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>> {
    let who = api::identity(&g.pool, &headers).await?;
    Ok(Json(
        governance::approval(&g.pool, &g.cipher, &who, id).await?,
    ))
}
async fn reconcile(
    State(c): State<Control>,
    Path(id): Path<Uuid>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>> {
    let who = auth(&c, &headers, &uri.to_string(), &body).await?;
    Ok(Json(
        human::reconcile_operation(&c.pool, &c.cipher, &who, id, decode(&body)?).await?,
    ))
}
