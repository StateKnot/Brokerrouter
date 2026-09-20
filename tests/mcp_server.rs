use axum::{body::Body, http::Request};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use brokerrouter::{mcp_catalog, mcp_oauth, mcp_server, store};
use http_body_util::BodyExt;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use rsa::{
    pkcs8::{EncodePrivateKey, LineEnding},
    traits::PublicKeyParts,
};
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::sync::OnceLock;
use tower::ServiceExt;
use uuid::Uuid;

const OAUTH_ISSUER: &str = "https://auth.example";
const OAUTH_RESOURCE: &str = "https://gateway.example/mcp";

fn oauth_keys() -> &'static (EncodingKey, jsonwebtoken::jwk::JwkSet) {
    static KEYS: OnceLock<(EncodingKey, jsonwebtoken::jwk::JwkSet)> = OnceLock::new();
    KEYS.get_or_init(|| {
        let private = rsa::RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 2048).unwrap();
        let public = private.to_public_key();
        let encoding =
            EncodingKey::from_rsa_pem(private.to_pkcs8_pem(LineEnding::LF).unwrap().as_bytes())
                .unwrap();
        let jwks = serde_json::from_value(json!({"keys":[{
            "kty":"RSA",
            "kid":"oauth-fixture",
            "use":"sig",
            "alg":"RS256",
            "n":URL_SAFE_NO_PAD.encode(public.n().to_bytes_be()),
            "e":URL_SAFE_NO_PAD.encode(public.e().to_bytes_be())
        }]}))
        .unwrap();
        (encoding, jwks)
    })
}

#[derive(Serialize)]
struct OAuthClaims<'a> {
    tenant_id: Uuid,
    iss: &'a str,
    sub: &'a str,
    aud: &'a str,
    exp: i64,
    nbf: i64,
    iat: i64,
    scope: &'a str,
}

fn oauth_token(tenant_id: Uuid, audience: &str, scope: &str, jku: bool) -> String {
    let now = chrono::Utc::now().timestamp();
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some("oauth-fixture".into());
    header.typ = Some("at+jwt".into());
    if jku {
        header.jku = Some("https://attacker.example/jwks".into());
    }
    jsonwebtoken::encode(
        &header,
        &OAuthClaims {
            tenant_id,
            iss: OAUTH_ISSUER,
            sub: "runtime-1",
            aud: audience,
            exp: now + 300,
            nbf: now - 1,
            iat: now,
            scope,
        },
        &oauth_keys().0,
    )
    .unwrap()
}

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
    let app = mcp_server::router(pool, config, None).unwrap();

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

#[sqlx::test(migrations = "./migrations")]
async fn oauth_metadata_jwt_scope_mapping_and_revocation_fail_closed(pool: PgPool) {
    let tenant = store::create_tenant(&pool, "mcp-oauth", 0).await.unwrap();
    store::put_model(&pool, tenant, "test/model", "Test model")
        .await
        .unwrap();
    let (key_id, _) =
        store::create_key(&pool, tenant, "OAuth MCP", 0, &["test/model".into()], None)
            .await
            .unwrap();
    mcp_catalog::set_oauth_principal(
        &pool,
        mcp_catalog::SetOauthPrincipal {
            tenant_id: tenant,
            key_id,
            operation_id: Uuid::new_v4(),
            actor: "oauth-test".into(),
            reason: "bind runtime subject".into(),
            issuer: OAUTH_ISSUER.into(),
            subject: "runtime-1".into(),
            active: true,
        },
    )
    .await
    .unwrap();
    let oauth = mcp_oauth::Verifier::new(
        OAUTH_RESOURCE,
        vec![mcp_oauth::IssuerConfig {
            issuer: OAUTH_ISSUER.into(),
            jwks_uri: None,
            jwks: Some(oauth_keys().1.clone()),
            allow_private: false,
        }],
        3600,
    )
    .await
    .unwrap();
    let config = mcp_server::HttpConfig::new(
        vec!["gateway.example".into()],
        vec!["https://console.example".into()],
    )
    .unwrap();
    let app = mcp_server::router(pool.clone(), config, Some(oauth)).unwrap();

    let metadata = app
        .clone()
        .oneshot(
            Request::get("/.well-known/oauth-protected-resource/mcp")
                .header("host", "gateway.example")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(metadata.status(), 200);
    assert_eq!(metadata.headers()["cache-control"], "public, max-age=300");
    let metadata: Value =
        serde_json::from_slice(&metadata.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(metadata["resource"], OAUTH_RESOURCE);
    assert_eq!(metadata["resource_name"], "Brokerrouter MCP Gateway");
    assert_eq!(metadata["authorization_servers"][0], OAUTH_ISSUER);
    assert!(
        metadata
            .get("resource_signing_alg_values_supported")
            .is_none()
    );

    let missing = app
        .clone()
        .oneshot(request("tools/list", None, None))
        .await
        .unwrap();
    assert_eq!(missing.status(), 401);
    assert!(
        missing.headers()["www-authenticate"]
            .to_str()
            .unwrap()
            .contains("resource_metadata=\"https://gateway.example/.well-known/oauth-protected-resource/mcp\"")
    );

    let read = oauth_token(tenant, OAUTH_RESOURCE, mcp_oauth::READ_SCOPE, false);
    let listed = app
        .clone()
        .oneshot(request("tools/list", Some(&read), None))
        .await
        .unwrap();
    assert_eq!(listed.status(), 200);

    let call_denied = app
        .clone()
        .oneshot(request("tools/call", Some(&read), None))
        .await
        .unwrap();
    assert_eq!(call_denied.status(), 403);
    assert!(
        call_denied.headers()["www-authenticate"]
            .to_str()
            .unwrap()
            .contains("error=\"insufficient_scope\"")
    );

    let wrong_audience = oauth_token(
        tenant,
        "https://other.example/mcp",
        mcp_oauth::READ_SCOPE,
        false,
    );
    assert_eq!(
        app.clone()
            .oneshot(request("tools/list", Some(&wrong_audience), None))
            .await
            .unwrap()
            .status(),
        401
    );
    let remote_key_header = oauth_token(tenant, OAUTH_RESOURCE, mcp_oauth::READ_SCOPE, true);
    assert_eq!(
        app.clone()
            .oneshot(request("tools/list", Some(&remote_key_header), None))
            .await
            .unwrap()
            .status(),
        401
    );

    store::revoke_key(&pool, tenant, key_id).await.unwrap();
    assert_eq!(
        app.oneshot(request("tools/list", Some(&read), None))
            .await
            .unwrap()
            .status(),
        401
    );
}
