use crate::{
    error::{Error, Result},
    gateway::Gateway,
    store,
};
use axum::{
    Json, Router,
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderMap, Request, StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::convert::Infallible;
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
        .route("/", get(landing))
        .route("/gateway.css", get(landing_css))
        .route("/health/live", get(|| async { StatusCode::NO_CONTENT }))
        .route("/health/ready", get(ready))
        .route("/v1/models", get(models))
        .route("/v1/exchange-rates", get(exchange_rate))
        .fallback(|| async { Error::NotFound })
        .with_state(pool)
}

const LANDING_HTML: &str = r#"<!doctype html>
<html lang="zh-CN">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <meta name="robots" content="noindex,nofollow">
  <title>Brokerrouter AI 网关</title>
  <link rel="stylesheet" href="gateway.css">
</head>
<body>
  <main>
    <p class="eyebrow">STATEKNOT · BROKERROUTER</p>
    <h1>AI 网关已上线</h1>
    <p class="lead">统一模型调用、人民币账本与智能体审批治理的生产核心数据面。</p>
    <section aria-labelledby="service-status">
      <div class="status"><span aria-hidden="true"></span>服务运行中</div>
      <h2 id="service-status">API 接入地址</h2>
      <code>https://stknot.com/ai-gateway/v1</code>
      <p>模型与业务接口需要 Brokerrouter 虚拟密钥；此入口不会公开租户、模型或用量数据。</p>
      <nav aria-label="服务检查">
        <a href="health/ready">就绪状态</a>
        <a href="health/live">存活状态</a>
      </nav>
    </section>
  </main>
</body>
</html>
"#;

const LANDING_CSS: &str = r#":root {
  color-scheme: light;
  font-family: Inter, ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
  background: #f5f7fb;
  color: #14213d;
}
* { box-sizing: border-box; }
body { margin: 0; min-height: 100vh; display: grid; place-items: center; padding: 24px; }
main { width: min(720px, 100%); }
.eyebrow { color: #5f6fff; font-size: 13px; font-weight: 750; letter-spacing: .14em; margin: 0 0 18px; }
h1 { color: #101828; font-size: clamp(42px, 8vw, 72px); letter-spacing: -.055em; line-height: .98; margin: 0; }
.lead { color: #667085; font-size: clamp(18px, 3vw, 22px); line-height: 1.65; margin: 24px 0 34px; }
section { background: #fff; border: 1px solid #e4e7ec; border-radius: 22px; box-shadow: 0 20px 60px rgba(16, 24, 40, .08); padding: clamp(24px, 5vw, 40px); }
.status { align-items: center; color: #087443; display: flex; font-size: 14px; font-weight: 700; gap: 10px; }
.status span { background: #12b76a; border-radius: 50%; box-shadow: 0 0 0 5px #d1fadf; height: 9px; width: 9px; }
h2 { color: #344054; font-size: 15px; margin: 28px 0 12px; }
code { background: #f2f4f7; border-radius: 9px; color: #101828; display: block; font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: 14px; overflow-wrap: anywhere; padding: 14px 16px; }
section p { color: #667085; font-size: 14px; line-height: 1.7; margin: 18px 0 24px; }
nav { display: flex; flex-wrap: wrap; gap: 12px; }
a { background: #eef0ff; border-radius: 9px; color: #3448d8; font-size: 14px; font-weight: 700; padding: 10px 14px; text-decoration: none; }
a:focus-visible, a:hover { background: #e0e4ff; outline: 2px solid #7180ff; outline-offset: 2px; }
"#;

async fn landing() -> impl IntoResponse {
    ([(header::CACHE_CONTROL, "no-store")], Html(LANDING_HTML))
}

async fn landing_css() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/css; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=3600"),
        ],
        Body::from(LANDING_CSS),
    )
}

async fn chat(State(g): State<Gateway>, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    if body.get("stream") == Some(&Value::Bool(true)) {
        stream_chat(g, headers, body).await
    } else {
        execute(g, headers, body, "chat").await
    }
}
async fn embedding(
    State(g): State<Gateway>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    execute(g, headers, body, "embedding").await
}
async fn execute(g: Gateway, headers: HeaderMap, body: Value, operation: &'static str) -> Response {
    let (who, key, turn) = match execution_context(&g, &headers).await {
        Ok(context) => context,
        Err(e) => return e.into_response(),
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

async fn execution_context(
    g: &Gateway,
    headers: &HeaderMap,
) -> Result<(store::Identity, String, Option<Uuid>)> {
    let who = identity(&g.pool, headers).await?;
    let key = headers
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
        .ok_or(Error::Invalid(
            "Idempotency-Key header required for billable requests",
        ))?;
    let turn = match headers.get("x-brokerrouter-turn-id") {
        Some(value) => Some(
            value
                .to_str()
                .ok()
                .and_then(|value| value.parse::<Uuid>().ok())
                .ok_or(Error::Invalid("invalid trusted turn ID"))?,
        ),
        None => None,
    };
    Ok((who, key, turn))
}

async fn stream_chat(g: Gateway, headers: HeaderMap, body: Value) -> Response {
    let (who, key, turn) = match execution_context(&g, &headers).await {
        Ok(context) => context,
        Err(error) => return error.into_response(),
    };
    let session = match g.execute_stream_in_turn(&who, &key, body, turn).await {
        Ok(session) => session,
        Err(error) => return error.into_response(),
    };
    let id = session.id;
    let stream = futures_util::stream::unfold(session.receiver, |mut receiver| async move {
        receiver
            .recv()
            .await
            .map(|bytes| (Ok::<_, Infallible>(bytes), receiver))
    });
    let mut response = axum::body::Body::from_stream(stream).into_response();
    response.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        "text/event-stream; charset=utf-8"
            .parse()
            .expect("static header"),
    );
    response.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        "no-store".parse().expect("static header"),
    );
    response
        .headers_mut()
        .insert("x-accel-buffering", "no".parse().expect("static header"));
    response.headers_mut().insert(
        "x-brokerrouter-request-id",
        id.to_string().parse().expect("uuid header"),
    );
    response
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use sqlx::postgres::PgPoolOptions;
    use tower::ServiceExt;

    fn app() -> Router {
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://unused:unused@127.0.0.1/unused")
            .expect("valid lazy database URL");
        base_routes(pool)
    }

    #[tokio::test]
    async fn browser_entrypoint_is_public_but_discloses_no_catalog_data() {
        let response = app()
            .oneshot(Request::get("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert!(
            response.headers()[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("text/html")
        );
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body = std::str::from_utf8(&body).unwrap();
        assert!(body.contains("网关已上线"));
        assert!(body.contains("https://stknot.com/ai-gateway/v1"));
        assert!(!body.contains("Authorization"));
    }

    #[tokio::test]
    async fn browser_styles_are_self_hosted_and_unknown_routes_still_fail_closed() {
        let response = app()
            .oneshot(Request::get("/gateway.css").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/css; charset=utf-8"
        );
        assert!(
            std::str::from_utf8(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap()
                .contains("color-scheme")
        );

        let response = app()
            .oneshot(Request::get("/unknown").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body: Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(body["error"]["code"], "not_found");
    }
}
