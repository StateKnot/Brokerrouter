// Protocol fixtures below are local test IdPs, not deployment certification.
use super::*;
use axum::{
    Form, Json, Router,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rsa::{
    pkcs8::{EncodePrivateKey, EncodePublicKey, LineEnding},
    traits::PublicKeyParts,
};
use std::{
    collections::HashMap,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio::task::JoinHandle;

fn keys() -> &'static (EncodingKey, String, Value) {
    static KEYS: OnceLock<(EncodingKey, String, Value)> = OnceLock::new();
    KEYS.get_or_init(||{
        let k=rsa::RsaPrivateKey::new(&mut rsa::rand_core::OsRng,2048).unwrap();
        let p=k.to_public_key();
        (EncodingKey::from_rsa_pem(k.to_pkcs8_pem(LineEnding::LF).unwrap().as_bytes()).unwrap(),
            p.to_public_key_pem(LineEnding::LF).unwrap(),
            json!({"keys":[{"kty":"RSA","kid":"fixture","use":"sig","alg":"RS256","n":URL_SAFE_NO_PAD.encode(p.n().to_bytes_be()),"e":URL_SAFE_NO_PAD.encode(p.e().to_bytes_be())}]}))
    })
}
struct Idp {
    issuer: String,
    codes: Mutex<HashMap<String, HashMap<String, String>>>,
    mode: Mutex<String>,
    exchanged: AtomicUsize,
}
async fn discovery(State(idp): State<Arc<Idp>>) -> Json<Value> {
    Json(
        json!({"issuer":idp.issuer,"authorization_endpoint":format!("{}/authorize",idp.issuer),
        "token_endpoint":format!("{}/token",idp.issuer),"jwks_uri":format!("{}/jwks",idp.issuer),
        "response_types_supported":["code"],"subject_types_supported":["public"],
        "id_token_signing_alg_values_supported":["RS256"],"token_endpoint_auth_methods_supported":["client_secret_basic"]}),
    )
}
async fn authorize(
    State(idp): State<Arc<Idp>>,
    Query(q): Query<HashMap<String, String>>,
) -> axum::response::Response {
    assert_eq!(q["client_id"], "client");
    assert_eq!(q["response_type"], "code");
    assert_eq!(q["code_challenge_method"], "S256");
    assert_eq!(q["max_age"], "0");
    assert_eq!(q["scope"], "openid");
    let code = Uuid::new_v4().to_string();
    let mut url = Url::parse(&q["redirect_uri"]).unwrap();
    url.query_pairs_mut()
        .append_pair("code", &code)
        .append_pair("state", &q["state"])
        .append_pair("iss", &idp.issuer);
    idp.codes.lock().unwrap().insert(code, q);
    (StatusCode::SEE_OTHER, [("location", url.to_string())]).into_response()
}
async fn token(
    State(idp): State<Arc<Idp>>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> axum::response::Response {
    idp.exchanged.fetch_add(1, Ordering::SeqCst);
    assert_eq!(headers["authorization"], "Basic Y2xpZW50OnNlY3JldA==");
    let Some(q) = idp.codes.lock().unwrap().remove(&form["code"]) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    assert_eq!(form["redirect_uri"], q["redirect_uri"]);
    let challenge = openidconnect::PkceCodeChallenge::from_code_verifier_sha256(
        &openidconnect::PkceCodeVerifier::new(form["code_verifier"].clone()),
    );
    if challenge.as_str() != q["code_challenge"] {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let now = Utc::now().timestamp();
    let mut claims = json!({"iss":idp.issuer,"sub":"external-user","aud":"client","exp":now+1800,"iat":now,"auth_time":now,"nonce":q["nonce"]});
    let mode = idp.mode.lock().unwrap().clone();
    match mode.as_str() {
        "nonce" => claims["nonce"] = json!("other"),
        "issuer" => claims["iss"] = json!("https://evil.invalid"),
        "audience" => claims["aud"] = json!("other-client"),
        "azp" => claims["azp"] = json!("other-client"),
        "multi-audience" => claims["aud"] = json!(["client", "other"]),
        "expired" => claims["exp"] = json!(now - 1),
        "stale" => claims["auth_time"] = json!(now - 301),
        "future" => claims["iat"] = json!(now + 30),
        "missing-auth" => {
            claims.as_object_mut().unwrap().remove("auth_time");
        }
        "unknown-user" => claims["sub"] = json!("not-bound"),
        "hash" => claims["at_hash"] = json!("bad-hash"),
        "oversized" => {
            return (
                StatusCode::OK,
                [("content-type", "application/json")],
                "x".repeat(262145),
            )
                .into_response();
        }
        "redirect" => {
            return (
                StatusCode::TEMPORARY_REDIRECT,
                [("location", "http://127.0.0.1:9/stolen")],
            )
                .into_response();
        }
        _ => {}
    }
    let mut h = Header::new(Algorithm::RS256);
    h.kid = Some("fixture".into());
    let mut jwt = jsonwebtoken::encode(&h, &claims, &keys().0).unwrap();
    if mode == "signature" {
        jwt.push('x');
    }
    Json(json!({"access_token":"unused-test-token","token_type":"Bearer","id_token":jwt}))
        .into_response()
}
async fn serve(app: Router) -> (String, JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (origin, task)
}
struct Fixture {
    bff: Arc<Bff>,
    idp: Arc<Idp>,
    tenant: Uuid,
    user: Uuid,
    binding: Uuid,
    tasks: Vec<JoinHandle<()>>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}
async fn fixture(pool: &PgPool) -> Fixture {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let idp = Arc::new(Idp {
        issuer: format!("http://{}", listener.local_addr().unwrap()),
        codes: Mutex::default(),
        mode: Mutex::new(String::new()),
        exchanged: AtomicUsize::new(0),
    });
    let app = Router::new()
        .route("/.well-known/openid-configuration", get(discovery))
        .route("/jwks", get(|| async { Json(keys().2.clone()) }))
        .route("/authorize", get(authorize))
        .route("/token", post(token))
        .with_state(idp.clone());
    let idp_task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let origin = "https://console.example".to_string();
    let tenant = Uuid::new_v4();
    let user = Uuid::new_v4();
    let binding = Uuid::new_v4();
    sqlx::query("INSERT INTO tenants(id,name,budget_micros) VALUES($1,'bff test',20000000)")
        .bind(tenant)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO governance_users(tenant_id,id,issuer,subject) VALUES($1,$2,$3,'internal-user')").bind(tenant).bind(user).bind(&origin).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO bff_identity_bindings(tenant_id,id,user_id,oidc_issuer,oidc_subject) VALUES($1,$2,$3,$4,'external-user')").bind(tenant).bind(binding).bind(user).bind(&idp.issuer).execute(pool).await.unwrap();
    let verifier = human::Verifier::new_console(
        &origin,
        "console-test",
        HashMap::from([("fixture".into(), keys().1.clone())]),
    )
    .unwrap();
    let (console_origin, console_task) =
        serve(crate::console_api::router(crate::console_api::Console {
            pool: pool.clone(),
            verifier: Arc::new(verifier),
        }))
        .await;
    // Only this private cfg(test) fixture permits loopback HTTP. from_env has no insecure switch.
    let bff = Arc::new(Bff {
        catalog: None,
        pool: pool.clone(),
        config: Config {
            origin,
            oidc_issuer: idp.issuer.clone(),
            oidc_origins: vec![idp.issuer.clone()],
            client_id: "client".into(),
            client_secret: "secret".into(),
            console_origin,
            audience: "console-test".into(),
            kid: "fixture".into(),
        },
        config_hash: vec![1; 32],
        cipher: ResultCipher::new(
            "fixture".into(),
            HashMap::from([("fixture".into(), "12".repeat(32))]),
        )
        .unwrap(),
        signer: keys().0.clone(),
        http: http_client().unwrap(),
    });
    Fixture {
        bff,
        idp,
        tenant,
        user,
        binding,
        tasks: vec![idp_task, console_task],
    }
}
async fn pending(f: &Fixture) -> (String, String, String) {
    let (url, cookie) = f.bff.login(f.tenant, None).await.unwrap();
    let response = f.bff.http.get(url).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let u = Url::parse(response.headers()["location"].to_str().unwrap()).unwrap();
    let q: HashMap<_, _> = u.query_pairs().into_owned().collect();
    (cookie, q["state"].clone(), q["code"].clone())
}
async fn logged_in(f: &Fixture) -> (String, String) {
    let (cookie, state, code) = pending(f).await;
    let (token, csrf, ttl) = f
        .bff
        .callback(&cookie, &state, Some(code), None)
        .await
        .unwrap();
    assert!((1..=3600).contains(&ttl));
    (token, csrf)
}
fn query_body() -> Vec<u8> {
    serde_json::to_vec(
        &json!({"filter":{"start":"2026-09-01T00:00:00Z","end":"2026-09-02T00:00:00Z"}}),
    )
    .unwrap()
}

#[sqlx::test(migrations = "./migrations")]
async fn browser_callback_errors_have_fixed_safe_landing_and_boundary_headers(pool: PgPool) {
    let f = fixture(&pool).await;
    let (base, task) = serve(router(f.bff.clone())).await;
    let _abort = Task(task);
    let client = http_client().unwrap();
    for suffix in [
        "?state=test&error=access_denied",
        "?code=not-a-real-code",
        "?state=test&state=duplicate",
    ] {
        let response = client
            .get(format!("{base}/bff/v1/callback{suffix}"))
            .header("accept", "text/html,application/xhtml+xml")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 303);
        assert_eq!(response.headers()["location"], "/?login=failed");
        assert_eq!(response.headers()["cache-control"], "no-store");
        assert_eq!(response.headers()["referrer-policy"], "no-referrer");
        assert!(
            response.headers()["set-cookie"]
                .to_str()
                .unwrap()
                .contains("Max-Age=0")
        );
        assert!(!response.text().await.unwrap().contains("not-a-real-code"));
    }
    let response = client
        .get(format!("{base}/bff/v1/callback?state=test"))
        .header("accept", "application/json")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
    assert!(!response.headers().contains_key("location"));
    let response = client
        .get(format!("{base}/bff/v1/callback?state={}", "a".repeat(8200)))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert_eq!(response.headers()["referrer-policy"], "no-referrer");
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM bff_sessions")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn real_http_oidc_pkce_to_console_and_logout(pool: PgPool) {
    let f = fixture(&pool).await;
    let (base, task) = serve(router(f.bff.clone())).await;
    let _abort = Task(task);
    let client = http_client().unwrap();
    let login = client
        .post(format!("{base}/bff/v1/login"))
        .header("origin", &f.bff.config.origin)
        .json(&json!({"tenant_id":f.tenant}))
        .send()
        .await
        .unwrap();
    assert_eq!(login.status(), 200);
    let c = login.headers()["set-cookie"].to_str().unwrap().to_string();
    assert!(c.contains("Secure; SameSite=Lax"));
    assert!(c.contains("HttpOnly"));
    assert!(!c.contains("Domain="));
    let login_cookie = c.split(';').next().unwrap().to_string();
    let url = login.json::<Value>().await.unwrap()["authorization_url"]
        .as_str()
        .unwrap()
        .to_string();
    let redirect = client.get(url).send().await.unwrap();
    let callback_url = Url::parse(redirect.headers()["location"].to_str().unwrap()).unwrap();
    let callback = client
        .get(format!(
            "{base}/bff/v1/callback?{}",
            callback_url.query().unwrap()
        ))
        .header("cookie", login_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(callback.status(), 303);
    assert_eq!(callback.headers()["location"], "/");
    let cookies: Vec<String> = callback
        .headers()
        .get_all("set-cookie")
        .iter()
        .map(|v| v.to_str().unwrap().split(';').next().unwrap().to_string())
        .collect();
    let session = cookies
        .iter()
        .find(|v| v.starts_with("__Host-br-session="))
        .unwrap();
    let csrf_cookie = cookies
        .iter()
        .find(|v| v.starts_with("__Host-br-csrf="))
        .unwrap();
    let csrf = csrf_cookie.split_once('=').unwrap().1;
    let cookie = format!("{session}; {csrf_cookie}");
    let info = client
        .get(format!("{base}/bff/v1/session"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(info.status(), 200);
    assert_eq!(info.headers()["cache-control"], "no-store");
    assert!(!info.headers().contains_key("access-control-allow-origin"));
    let info = info.json::<Value>().await.unwrap();
    assert_eq!(info["user_id"], f.user.to_string());
    assert!(!info.to_string().contains("token"));
    let request = || {
        client
            .post(format!("{base}/bff/v1/usage/query"))
            .header("origin", &f.bff.config.origin)
            .header("content-type", "application/json")
            .header("cookie", &cookie)
            .header("x-br-csrf", csrf)
            .body(query_body())
    };
    assert_eq!(request().send().await.unwrap().status(), 403);
    crate::console::set_permission(
        &pool,
        f.tenant,
        f.user,
        crate::console::Permission::UsageRead,
        true,
        "test",
    )
    .await
    .unwrap();
    let usage = request().send().await.unwrap();
    assert_eq!(usage.status(), 200);
    let usage = usage.json::<Value>().await.unwrap();
    assert_eq!(usage["currency"], "CNY");
    assert_eq!(usage["totals"]["known_cost_cny"], "0.000000");
    let logout = client
        .post(format!("{base}/bff/v1/logout"))
        .header("origin", &f.bff.config.origin)
        .header("cookie", &cookie)
        .header("x-br-csrf", csrf)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(logout.status(), 204);
    assert_eq!(request().send().await.unwrap().status(), 401);
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events WHERE action IN ('bff.login','bff.logout')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 2);
}
struct Task(JoinHandle<()>);
impl Drop for Task {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn invalid_oidc_tokens_never_create_sessions(pool: PgPool) {
    let f = fixture(&pool).await;
    for mode in [
        "nonce",
        "issuer",
        "audience",
        "azp",
        "multi-audience",
        "expired",
        "stale",
        "future",
        "missing-auth",
        "unknown-user",
        "hash",
        "signature",
        "oversized",
        "redirect",
    ] {
        *f.idp.mode.lock().unwrap() = mode.into();
        let (cookie, state, code) = pending(&f).await;
        assert!(
            f.bff
                .callback(&cookie, &state, Some(code.clone()), None)
                .await
                .is_err(),
            "{mode}"
        );
        let exchanges = f.idp.exchanged.load(Ordering::SeqCst);
        assert!(
            f.bff
                .callback(&cookie, &state, Some(code), None)
                .await
                .is_err()
        );
        assert_eq!(f.idp.exchanged.load(Ordering::SeqCst), exchanges);
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM bff_sessions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}
#[sqlx::test(migrations = "./migrations")]
async fn cookie_state_replay_concurrency_and_encrypted_pending(pool: PgPool) {
    let f = fixture(&pool).await;
    let (cookie, state, code) = pending(&f).await;
    let payload: Vec<u8> = sqlx::query_scalar("SELECT payload FROM bff_logins")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(serde_json::from_slice::<Value>(&payload).is_err());
    assert!(
        f.bff
            .callback(&secret(), &state, Some(code.clone()), None)
            .await
            .is_err()
    );
    assert!(
        f.bff
            .callback(&cookie, "wrong-state", Some(code.clone()), None)
            .await
            .is_err()
    );
    assert_eq!(f.idp.exchanged.load(Ordering::SeqCst), 0);
    let (a, b) = tokio::join!(
        f.bff.callback(&cookie, &state, Some(code.clone()), None),
        f.bff.callback(&cookie, &state, Some(code), None)
    );
    assert_ne!(a.is_ok(), b.is_ok());
    assert_eq!(f.idp.exchanged.load(Ordering::SeqCst), 1);
}
#[sqlx::test(migrations = "./migrations")]
async fn revoked_reenabled_binding_expiry_and_rotated_login(pool: PgPool) {
    let f = fixture(&pool).await;
    let (old, old_csrf) = logged_in(&f).await;
    let (cookie, state, code) = pending(&f).await;
    let (new, csrf, _) = f
        .bff
        .callback(&cookie, &state, Some(code), Some(&old))
        .await
        .unwrap();
    assert!(f.bff.session(&old, Some(&old_csrf)).await.is_err());
    assert!(f.bff.session(&new, Some(&csrf)).await.is_ok());
    set_identity_active(&pool, f.tenant, f.binding, false, "offboard")
        .await
        .unwrap();
    set_identity_active(&pool, f.tenant, f.binding, true, "re-enroll")
        .await
        .unwrap();
    assert!(f.bff.session(&new, Some(&csrf)).await.is_err());
    let (new, csrf) = logged_in(&f).await;
    sqlx::query("UPDATE bff_sessions SET auth_time=now()-interval '2 hours',expires_at=now()-interval '1 hour' WHERE token_hash=$1").bind(store::hash(new.as_bytes())).execute(&pool).await.unwrap();
    assert!(f.bff.session(&new, Some(&csrf)).await.is_err());
    maintain(&pool).await.unwrap();
}
#[sqlx::test(migrations = "./migrations")]
async fn tenant_and_principal_binding_is_not_email_or_browser_input(pool: PgPool) {
    let f = fixture(&pool).await;
    let other = Uuid::new_v4();
    sqlx::query("INSERT INTO tenants(id,name,budget_micros) VALUES($1,'other',0)")
        .bind(other)
        .execute(&pool)
        .await
        .unwrap();
    let (url, cookie) = f.bff.login(other, None).await.unwrap();
    let response = f.bff.http.get(url).send().await.unwrap();
    let u = Url::parse(response.headers()["location"].to_str().unwrap()).unwrap();
    let q: HashMap<_, _> = u.query_pairs().into_owned().collect();
    assert!(
        f.bff
            .callback(&cookie, &q["state"], Some(q["code"].clone()), None)
            .await
            .is_err()
    );
    let (token, csrf) = logged_in(&f).await;
    sqlx::query("UPDATE governance_users SET active=false WHERE tenant_id=$1 AND id=$2")
        .bind(f.tenant)
        .bind(f.user)
        .execute(&pool)
        .await
        .unwrap();
    assert!(f.bff.session(&token, Some(&csrf)).await.is_err());
    sqlx::query("UPDATE governance_users SET active=true WHERE tenant_id=$1 AND id=$2")
        .bind(f.tenant)
        .bind(f.user)
        .execute(&pool)
        .await
        .unwrap();
    assert!(f.bff.session(&token, Some(&csrf)).await.is_err());
    let (token, csrf) = logged_in(&f).await;
    sqlx::query("UPDATE tenants SET active=false WHERE id=$1")
        .bind(f.tenant)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE tenants SET active=true WHERE id=$1")
        .bind(f.tenant)
        .execute(&pool)
        .await
        .unwrap();
    assert!(f.bff.session(&token, Some(&csrf)).await.is_err());
}
#[sqlx::test(migrations = "./migrations")]
async fn csrf_cookies_and_arbitrary_proxy_paths_are_rejected(pool: PgPool) {
    let f = fixture(&pool).await;
    let (token, csrf) = logged_in(&f).await;
    let (base, task) = serve(router(f.bff.clone())).await;
    let _abort = Task(task);
    let client = http_client().unwrap();
    let cookie = format!("__Host-br-session={token}; __Host-br-csrf={csrf}");
    for (origin, header, ct, cookies) in [
        (
            "https://evil.example",
            csrf.as_str(),
            "application/json",
            cookie.clone(),
        ),
        ("null", csrf.as_str(), "application/json", cookie.clone()),
        (
            "https://console.example",
            "wrong",
            "application/json",
            cookie.clone(),
        ),
        (
            "https://console.example",
            csrf.as_str(),
            "text/plain",
            cookie.clone(),
        ),
        (
            "https://console.example",
            csrf.as_str(),
            "application/json",
            format!("{cookie}; __Host-br-session={token}"),
        ),
    ] {
        let response = client
            .post(format!("{base}/bff/v1/usage/query"))
            .header("origin", origin)
            .header("x-br-csrf", header)
            .header("content-type", ct)
            .header("cookie", cookies)
            .body(query_body())
            .send()
            .await
            .unwrap();
        assert!(matches!(response.status().as_u16(), 401 | 403));
    }
    assert!(
        f.bff
            .query(&token, &csrf, "/human/v1/approve", b"{}")
            .await
            .is_err()
    );
    let response = client
        .post(format!("{base}/bff/v1/usage/query"))
        .header("origin", "https://console.example")
        .header("x-br-csrf", &csrf)
        .header("cookie", cookie)
        .header("content-type", "application/json")
        .body("x".repeat(16385))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 413);
}

#[sqlx::test(migrations = "./migrations")]
async fn catalog_bff_uses_separate_signer_csrf_whitelist_and_idempotent_mutations(pool: PgPool) {
    let mut f = fixture(&pool).await;
    let (token, csrf) = logged_in(&f).await;
    assert!(matches!(
        f.bff
            .query(
                &token,
                &csrf,
                "/catalog/v1/models/query",
                br#"{"limit":20}"#
            )
            .await,
        Err(Error::Configuration)
    ));
    let private = rsa::RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 2048).unwrap();
    let verifier = human::Verifier::new_catalog(
        &f.bff.config.origin,
        "catalog-test",
        HashMap::from([(
            "catalog-fixture".into(),
            private
                .to_public_key()
                .to_public_key_pem(LineEnding::LF)
                .unwrap(),
        )]),
    )
    .unwrap();
    let (origin, task) = serve(crate::catalog_api::router(crate::catalog_api::Catalog {
        pool: pool.clone(),
        verifier: Arc::new(verifier),
        connections: Arc::new(vec![]),
    }))
    .await;
    let _abort = Task(task);
    Arc::get_mut(&mut f.bff).unwrap().catalog = Some(CatalogTarget {
        origin,
        audience: "catalog-test".into(),
        kid: "catalog-fixture".into(),
        signer: EncodingKey::from_rsa_pem(private.to_pkcs8_pem(LineEnding::LF).unwrap().as_bytes())
            .unwrap(),
    });
    for permission in [
        crate::console::Permission::CatalogRead,
        crate::console::Permission::CatalogWrite,
    ] {
        crate::console::set_permission(&pool, f.tenant, f.user, permission, true, "test catalog")
            .await
            .unwrap();
    }
    let (base, task) = serve(router(f.bff.clone())).await;
    let _bff = Task(task);
    let client = http_client().unwrap();
    let body = json!({"operation_id":Uuid::new_v4(),"reason":"OIDC configuration test","change":{"kind":"model_create","id":"catalog/model","name":"Catalog model"}});
    let cookie = format!("__Host-br-session={token}; __Host-br-csrf={csrf}");
    for path in ["/bff/v1/catalog/models/query", "/bff/v1/catalog/apply"] {
        let response = client
            .post(format!("{base}{path}"))
            .header("origin", "https://evil.example")
            .header("cookie", &cookie)
            .header("x-br-csrf", &csrf)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 403);
    }
    let mut receipts = vec![];
    for _ in 0..2 {
        let response = client
            .post(format!("{base}/bff/v1/catalog/apply"))
            .header("origin", &f.bff.config.origin)
            .header("cookie", &cookie)
            .header("x-br-csrf", &csrf)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["cache-control"], "no-store");
        receipts.push(response.json::<Value>().await.unwrap());
    }
    assert_eq!(receipts[0], receipts[1]);
    assert!(
        f.bff
            .query(&token, &csrf, "/catalog/v1/permissions/apply", b"{}")
            .await
            .is_err()
    );
    f.bff.logout(&token, &csrf).await.unwrap();
    assert!(
        f.bff
            .query(
                &token,
                &csrf,
                "/catalog/v1/apply",
                &serde_json::to_vec(&body).unwrap()
            )
            .await
            .is_err()
    );
}
#[sqlx::test(migrations = "./migrations")]
async fn login_and_logout_audit_failures_are_atomic(pool: PgPool) {
    let f = fixture(&pool).await;
    sqlx::raw_sql("CREATE FUNCTION bff_fail_audit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.action LIKE 'bff.%' THEN RAISE EXCEPTION 'injected'; END IF; RETURN NEW; END $$; CREATE TRIGGER bff_fail BEFORE INSERT ON audit_events FOR EACH ROW EXECUTE FUNCTION bff_fail_audit();").execute(&pool).await.unwrap();
    let (cookie, state, code) = pending(&f).await;
    assert!(
        f.bff
            .callback(&cookie, &state, Some(code), None)
            .await
            .is_err()
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM bff_sessions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    sqlx::query("DROP TRIGGER bff_fail ON audit_events")
        .execute(&pool)
        .await
        .unwrap();
    let (token, csrf) = logged_in(&f).await;
    sqlx::query("CREATE TRIGGER bff_fail BEFORE INSERT ON audit_events FOR EACH ROW EXECUTE FUNCTION bff_fail_audit()").execute(&pool).await.unwrap();
    assert!(f.bff.logout(&token, &csrf).await.is_err());
    assert!(f.bff.session(&token, Some(&csrf)).await.is_ok());
}

#[test]
fn production_configuration_requires_exact_https_origins() {
    for url in [
        "http://localhost",
        "https://u:p@example.com",
        "https://example.com/path",
        "https://example.com?x=1",
        "https://example.com#fragment",
        "https://example.com/",
    ] {
        assert!(https_origin(url).is_err(), "{url}");
    }
    assert!(https_origin("https://console.example").is_ok());
    let weak = rsa::RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 1024).unwrap();
    assert!(signing_key(&weak.to_pkcs8_pem(LineEnding::LF).unwrap()).is_err());
    assert!(signing_key("not a private key").is_err());
}

async fn scoped(pool: &PgPool, role: &'static str) -> PgPool {
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .after_connect(move |c, _| {
            Box::pin(async move {
                sqlx::query(&format!("SET SESSION AUTHORIZATION {role}"))
                    .execute(&mut *c)
                    .await?;
                crate::configure_connection(c).await?;
                Ok(())
            })
        })
        .connect_with((*pool.connect_options()).clone())
        .await
        .unwrap()
}
#[sqlx::test(migrations = "./migrations")]
async fn actual_bff_role_cannot_grant_approve_or_read_model_secrets(pool: PgPool) {
    sqlx::raw_sql(include_str!("../../deploy/permissions.sql"))
        .execute(&pool)
        .await
        .unwrap();
    let mut f = fixture(&pool).await;
    let runtime = scoped(&pool, "brokerrouter_bff").await;
    Arc::get_mut(&mut f.bff).unwrap().pool = runtime.clone();
    crate::check_schema(&runtime).await.unwrap();
    let (token, csrf) = logged_in(&f).await;
    assert!(f.bff.session(&token, Some(&csrf)).await.is_ok());
    f.bff.logout(&token, &csrf).await.unwrap();
    maintain(&runtime).await.unwrap();
    for sql in [
        "SELECT key_hash FROM virtual_keys",
        "SELECT result_ciphertext FROM requests",
        "SELECT * FROM ledger_entries",
        "SELECT details FROM audit_events",
        "SELECT * FROM endpoints",
        "SELECT * FROM console_request_facts",
        "UPDATE bff_identity_bindings SET active=true",
        "UPDATE governance_users SET active=true",
        "UPDATE tenants SET budget_micros=1",
        "UPDATE console_permissions SET enabled=true",
        "SELECT * FROM human_assertions",
        "SET ROLE brokerrouter_runtime",
        "SET ROLE brokerrouter_console",
        "SET ROLE brokerrouter_governance_control",
        "SET ROLE brokerrouter_operator",
    ] {
        assert!(sqlx::query(sql).execute(&runtime).await.is_err(), "{sql}");
    }
    let operator = scoped(&pool, "brokerrouter_operator").await;
    set_identity_active(&operator, f.tenant, f.binding, false, "test revoke")
        .await
        .unwrap();
    set_identity_active(&operator, f.tenant, f.binding, true, "test enroll")
        .await
        .unwrap();
    let bound = bind_identity(
        &operator,
        f.tenant,
        f.user,
        "https://other-idp.example",
        "external-user",
    )
    .await
    .unwrap();
    assert_ne!(bound, f.binding);
    sqlx::query("UPDATE governance_users SET active=false WHERE tenant_id=$1 AND id=$2")
        .bind(f.tenant)
        .bind(f.user)
        .execute(&operator)
        .await
        .unwrap();
    // Column-limited operator update fires the epoch trigger without granting direct epoch writes.
    assert!(
        sqlx::query("UPDATE governance_users SET bff_epoch=1")
            .execute(&operator)
            .await
            .is_err()
    );
    runtime.close().await;
    operator.close().await;
}
#[sqlx::test(migrations = "./migrations")]
async fn concurrent_logout_suppresses_completed_report(pool: PgPool) {
    let f = fixture(&pool).await;
    let (token, csrf) = logged_in(&f).await;
    crate::console::set_permission(
        &pool,
        f.tenant,
        f.user,
        crate::console::Permission::UsageRead,
        true,
        "test",
    )
    .await
    .unwrap();
    let mut lock = pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE requests IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *lock)
        .await
        .unwrap();
    let b = f.bff.clone();
    let t = token.clone();
    let c = csrf.clone();
    let task = tokio::spawn(async move {
        b.query(&t, &c, "/console/v1/usage/query", &query_body())
            .await
    });
    let mut waiting = false;
    for _ in 0..100 {
        waiting=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND wait_event_type='Lock' AND query LIKE 'SELECT count(*) AS requests%')").fetch_one(&pool).await.unwrap();
        if waiting {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(waiting);
    f.bff.logout(&token, &csrf).await.unwrap();
    lock.commit().await.unwrap();
    assert!(matches!(
        task.await.unwrap(),
        Err(Error::SessionUnauthorized)
    ));
}
#[sqlx::test(migrations = "./migrations")]
async fn expired_pending_configuration_drift_and_cleanup(pool: PgPool) {
    let mut f = fixture(&pool).await;
    let (cookie, state, code) = pending(&f).await;
    sqlx::query("UPDATE bff_logins SET created_at=now()-interval '10 minutes',expires_at=now()-interval '5 minutes'").execute(&pool).await.unwrap();
    assert!(
        f.bff
            .callback(&cookie, &state, Some(code), None)
            .await
            .is_err()
    );
    assert_eq!(f.idp.exchanged.load(Ordering::SeqCst), 0);
    maintain(&pool).await.unwrap();
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM bff_logins")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    let (token, csrf) = logged_in(&f).await;
    let (cookie, state, code) = pending(&f).await;
    Arc::get_mut(&mut f.bff).unwrap().config_hash = vec![2; 32];
    assert!(f.bff.session(&token, Some(&csrf)).await.is_err());
    assert!(
        f.bff
            .callback(&cookie, &state, Some(code), None)
            .await
            .is_err()
    );
    assert_eq!(f.idp.exchanged.load(Ordering::SeqCst), 1);
}
#[sqlx::test(migrations = "./migrations")]
async fn identity_revoke_and_generation_audit_roll_back_together(pool: PgPool) {
    let f = fixture(&pool).await;
    let (token, csrf) = logged_in(&f).await;
    sqlx::raw_sql("CREATE FUNCTION bff_identity_audit_fail() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.action='bff.identity_changed' THEN RAISE EXCEPTION 'injected'; END IF; RETURN NEW; END $$; CREATE TRIGGER bff_identity_fail BEFORE INSERT ON audit_events FOR EACH ROW EXECUTE FUNCTION bff_identity_audit_fail();").execute(&pool).await.unwrap();
    assert!(
        set_identity_active(&pool, f.tenant, f.binding, false, "test")
            .await
            .is_err()
    );
    assert!(f.bff.session(&token, Some(&csrf)).await.is_ok());
    assert!(
        set_identity_active(&pool, f.tenant, f.binding, false, "")
            .await
            .is_err()
    );
}
