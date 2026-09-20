use axum::{body::Body, http::Request};
use brokerrouter::{mcp_server, store};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

fn request(method: &str, token: Option<&str>, origin: Option<&str>) -> Request<Body> {
    let body = json!({
        "jsonrpc":"2.0",
        "id":1,
        "method":method,
        "params":{
            "_meta":{
                "io.modelcontextprotocol/protocolVersion":"2026-07-28",
                "io.modelcontextprotocol/clientInfo":{"name":"fixture","version":"1.0.0"},
                "io.modelcontextprotocol/clientCapabilities":{}
            }
        }
    });
    let mut builder = Request::post("/mcp")
        .header("host", "gateway.example")
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", "2026-07-28")
        .header("mcp-method", method);
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    if let Some(origin) = origin {
        builder = builder.header("origin", origin);
    }
    builder.body(Body::from(body.to_string())).unwrap()
}

#[sqlx::test(migrations = "./migrations")]
async fn downstream_mcp_is_stateless_authenticated_and_origin_bound(pool: PgPool) {
    let tenant = store::create_tenant(&pool, "mcp-edge", 0).await.unwrap();
    store::put_model(&pool, tenant, "test/model", "Test model")
        .await
        .unwrap();
    let (_, token) = store::create_key(&pool, tenant, "MCP edge", 0, &["test/model".into()], None)
        .await
        .unwrap();
    let config = mcp_server::HttpConfig::new(
        vec!["gateway.example".into()],
        vec!["https://console.example".into()],
    )
    .unwrap();
    let app = mcp_server::router(pool, config);

    let denied = app
        .clone()
        .oneshot(request(
            "server/discover",
            None,
            Some("https://attacker.example"),
        ))
        .await
        .unwrap();
    assert_eq!(denied.status(), 403);

    let unauthorized = app
        .clone()
        .oneshot(request("server/discover", None, None))
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), 401);
    assert!(unauthorized.headers().contains_key("www-authenticate"));

    let discovered = app
        .clone()
        .oneshot(request(
            "server/discover",
            Some(&token),
            Some("https://console.example"),
        ))
        .await
        .unwrap();
    assert_eq!(discovered.status(), 200);
    assert!(!discovered.headers().contains_key("mcp-session-id"));
    let body = discovered.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["result"]["supportedVersions"][0], "2026-07-28");

    let mut legacy = request("tools/list", Some(&token), None);
    legacy
        .headers_mut()
        .insert("mcp-session-id", "legacy".parse().unwrap());
    assert_eq!(app.clone().oneshot(legacy).await.unwrap().status(), 400);

    let listed = app
        .oneshot(request("tools/list", Some(&token), None))
        .await
        .unwrap();
    assert_eq!(listed.status(), 200);
    let body = listed.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["result"]["tools"], json!([]));
    assert_eq!(body["result"]["cacheScope"], "private");
}
