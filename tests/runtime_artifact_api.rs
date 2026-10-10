mod common;
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use brokerrouter::{
    assets::{Assets, CHUNK_BYTES},
    governance::{self, Check},
    runtime_artifact_api::{self, CLAIM_HEADER},
    runtime_artifacts::{File, Manifest, Reserve, RuntimeArtifacts},
    store,
};
use futures_util::stream::BoxStream;
use http_body_util::BodyExt;
use object_store::*;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::sync::Notify;
use tower::ServiceExt;
use uuid::Uuid;

async fn fixture(
    pool: &PgPool,
    objects: Arc<dyn ObjectStore>,
) -> (store::Identity, String, Reserve, RuntimeArtifacts) {
    let tenant = store::create_tenant(pool, "runtime-artifact-http", 1_000_000)
        .await
        .unwrap();
    store::put_model(pool, tenant, "model", "Model")
        .await
        .unwrap();
    let (key_id, key) =
        store::create_key(pool, tenant, "runtime", 1_000_000, &["model".into()], None)
            .await
            .unwrap();
    let who = store::Identity {
        tenant_id: tenant,
        key_id,
    };
    common::provision(pool, &who, "alice").await;
    let turn = common::turn(pool, &who, "alice", 0).await;
    brokerrouter::assets::set_quota(pool, tenant, None, 2 * CHUNK_BYTES as i64)
        .await
        .unwrap();
    brokerrouter::assets::set_quota(pool, tenant, Some(key_id), 2 * CHUNK_BYTES as i64)
        .await
        .unwrap();
    let op = governance::check(
        pool,
        &common::cipher(),
        &who,
        Check {
            turn_id: turn,
            operation_key: "native-http".into(),
            stage: "tool_input".into(),
            model_id: Some("model".into()),
            tool_name: Some("fixture_tool".into()),
            payload: json!({"final":"arguments"}),
        },
    )
    .await
    .unwrap();
    let input_operation_id = serde_json::from_value(op["id"].clone()).unwrap();
    let claim_token = "11".repeat(32);
    governance::claim(
        pool,
        &common::cipher(),
        &who,
        input_operation_id,
        governance::Claim {
            token: claim_token.clone(),
            payload_hash: op["payload_hash"].as_str().unwrap().into(),
        },
    )
    .await
    .unwrap();
    let reserve = Reserve {
        input_operation_id,
        claim_token,
        input_payload_hash: op["payload_hash"].as_str().unwrap().into(),
        original_payload_hash: "22".repeat(32),
        maximum_bytes: 2 * CHUNK_BYTES as i64,
        maximum_artifacts: 1,
    };
    let a = RuntimeArtifacts::new(
        Assets::new(pool.clone(), objects, common::cipher(), 1).unwrap(),
        common::cipher(),
    );
    (who, key, reserve, a)
}
fn manifest(r: &Reserve, bytes: &[u8]) -> Manifest {
    Manifest {
        claim_token: r.claim_token.clone(),
        evidence_ref: "runtime://http-fixture".into(),
        artifacts: vec![File {
            sha256: common::hex(&store::hash(bytes)),
            byte_length: bytes.len() as i64,
            media_type: "video/mp4".into(),
            producer_revision: "33".repeat(32),
        }],
    }
}
fn request(
    method: &str,
    path: &str,
    key: &str,
    body: impl Into<Body>,
    content_type: &str,
    claim: Option<&str>,
) -> Request<Body> {
    let mut r = Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", format!("Bearer {key}"))
        .header("content-type", content_type);
    if let Some(token) = claim {
        r = r.header(CLAIM_HEADER, token);
    }
    r.body(body.into()).unwrap()
}
async fn json_request(
    app: &Router,
    method: &str,
    path: &str,
    key: &str,
    value: &Value,
    claim: Option<&str>,
) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(request(
            method,
            path,
            key,
            serde_json::to_vec(value).unwrap(),
            "application/json",
            claim,
        ))
        .await
        .unwrap();
    let status = response.status();
    assert_eq!(response.headers()["cache-control"], "private, no-store");
    assert_eq!(response.headers()["x-content-type-options"], "nosniff");
    let data = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&data).unwrap_or(Value::Null))
}
#[sqlx::test(migrations = "./migrations")]
async fn http_capture_exact_boundaries_claim_and_no_binary_or_approval_read_routes(pool: PgPool) {
    let (who, key, r, a) = fixture(&pool, Arc::new(memory::InMemory::new())).await;
    let app = runtime_artifact_api::router(a.clone());
    let (status, first) =
        json_request(&app, "POST", "/v1/runtime-artifacts", &key, &json!(r), None).await;
    assert_eq!(status, StatusCode::OK);
    let b: Uuid = serde_json::from_value(first["id"].clone()).unwrap();
    let (status, retry) =
        json_request(&app, "POST", "/v1/runtime-artifacts", &key, &json!(r), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(retry, first);
    let route = format!("/v1/runtime-artifacts/{b}/manifest");
    let m = manifest(&r, b"abc");
    let (status, value) = json_request(&app, "POST", &route, &key, &json!(m), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(value["state"], "capturing");
    let path = format!("/v1/runtime-artifacts/{b}/artifacts/0/chunks/0");
    let missing = app
        .clone()
        .oneshot(request(
            "PUT",
            &path,
            &key,
            "abc",
            "application/octet-stream",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::BAD_REQUEST);
    let wrong = app
        .clone()
        .oneshot(request(
            "PUT",
            &path,
            &key,
            "abc",
            "application/octet-stream",
            Some(&"44".repeat(32)),
        ))
        .await
        .unwrap();
    assert_eq!(wrong.status(), StatusCode::NOT_FOUND);
    let duplicate = request(
        "PUT",
        &path,
        &key,
        "abc",
        "application/octet-stream",
        Some(&r.claim_token),
    );
    let mut duplicate = duplicate;
    duplicate
        .headers_mut()
        .append(CLAIM_HEADER, r.claim_token.parse().unwrap());
    assert_eq!(
        app.clone().oneshot(duplicate).await.unwrap().status(),
        StatusCode::BAD_REQUEST
    );
    let oversized = app
        .clone()
        .oneshot(request(
            "PUT",
            &path,
            &key,
            vec![0; CHUNK_BYTES + 1],
            "application/octet-stream",
            Some(&r.claim_token),
        ))
        .await
        .unwrap();
    assert_eq!(oversized.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let large_metadata = app
        .clone()
        .oneshot(request(
            "POST",
            "/v1/runtime-artifacts",
            &key,
            vec![b' '; 64 * 1024 + 1],
            "application/json",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(large_metadata.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let mut encoding = request(
        "PUT",
        &path,
        &key,
        "abc",
        "application/octet-stream",
        Some(&r.claim_token),
    );
    encoding
        .headers_mut()
        .insert("content-encoding", "gzip".parse().unwrap());
    assert_eq!(
        app.clone().oneshot(encoding).await.unwrap().status(),
        StatusCode::BAD_REQUEST
    );
    let uploaded = app
        .clone()
        .oneshot(request(
            "PUT",
            &path,
            &key,
            "abc",
            "application/octet-stream",
            Some(&r.claim_token),
        ))
        .await
        .unwrap();
    assert_eq!(uploaded.status(), StatusCode::OK);
    let (status, done) = json_request(
        &app,
        "POST",
        &format!("/v1/runtime-artifacts/{b}/complete"),
        &key,
        &json!({"claim_token":r.claim_token}),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(done["state"], "quarantined");
    for path in [
        format!("/v1/runtime-artifacts/{b}/content"),
        format!("/v1/runtime-artifacts/{b}/artifacts/0/chunks/0"),
        format!(
            "/v1/assets/{}",
            value["artifacts"][0]["id"].as_str().unwrap()
        ),
        "/v1/governance/approvals".into(),
    ] {
        assert!(matches!(
            app.clone()
                .oneshot(request(
                    "GET",
                    &path,
                    &key,
                    Body::empty(),
                    "application/json",
                    Some(&r.claim_token)
                ))
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
        ));
    }
    assert!(
        brokerrouter::assets::Assets::metadata(
            &a.assets,
            &who,
            serde_json::from_value(value["artifacts"][0]["id"].clone()).unwrap()
        )
        .await
        .is_err()
    );
    let mut forged = json!(m);
    forged["artifacts"][0]["path"] = json!("/untrusted/path");
    assert_eq!(
        json_request(&app, "POST", &route, &key, &forged, None)
            .await
            .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
}

#[derive(Debug)]
struct HeldStore {
    inner: Arc<memory::InMemory>,
    entered: Notify,
    release: Notify,
    puts: AtomicUsize,
}
impl std::fmt::Display for HeldStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HeldRuntimeArtifactFixture")
    }
}
#[async_trait::async_trait]
impl ObjectStore for HeldStore {
    async fn get_opts(&self, p: &path::Path, o: GetOptions) -> object_store::Result<GetResult> {
        self.inner.get_opts(p, o).await
    }
    async fn put_opts(
        &self,
        p: &path::Path,
        b: PutPayload,
        o: PutOptions,
    ) -> object_store::Result<PutResult> {
        self.puts.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_one();
        self.release.notified().await;
        self.inner.put_opts(p, b, o).await
    }
    async fn put_multipart_opts(
        &self,
        p: &path::Path,
        o: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn MultipartUpload>> {
        self.inner.put_multipart_opts(p, o).await
    }
    fn delete_stream(
        &self,
        p: BoxStream<'static, object_store::Result<path::Path>>,
    ) -> BoxStream<'static, object_store::Result<path::Path>> {
        self.inner.delete_stream(p)
    }
    fn list(&self, p: Option<&path::Path>) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        self.inner.list(p)
    }
    async fn list_with_delimiter(
        &self,
        p: Option<&path::Path>,
    ) -> object_store::Result<ListResult> {
        self.inner.list_with_delimiter(p).await
    }
    async fn copy_opts(
        &self,
        a: &path::Path,
        b: &path::Path,
        o: CopyOptions,
    ) -> object_store::Result<()> {
        self.inner.copy_opts(a, b, o).await
    }
}
#[sqlx::test(migrations = "./migrations")]
async fn canceled_http_handler_keeps_owned_write_capacity_and_settles_original_chunk(pool: PgPool) {
    let held = Arc::new(HeldStore {
        inner: Arc::new(memory::InMemory::new()),
        entered: Notify::new(),
        release: Notify::new(),
        puts: AtomicUsize::new(0),
    });
    let (who, key, r, a) = fixture(&pool, held.clone()).await;
    let batch = a.reserve(&who, r.clone()).await.unwrap();
    let b: Uuid = serde_json::from_value(batch["id"].clone()).unwrap();
    a.record_manifest(&who, b, manifest(&r, b"abc"))
        .await
        .unwrap();
    let app = runtime_artifact_api::router(a.clone());
    let path = format!("/v1/runtime-artifacts/{b}/artifacts/0/chunks/0");
    let response = tokio::spawn(app.clone().oneshot(request(
        "PUT",
        &path,
        &key,
        "abc",
        "application/octet-stream",
        Some(&r.claim_token),
    )));
    tokio::time::timeout(std::time::Duration::from_secs(5), held.entered.notified())
        .await
        .unwrap();
    response.abort();
    assert!(response.await.unwrap_err().is_cancelled());
    let competing = app
        .clone()
        .oneshot(request(
            "PUT",
            &path,
            &key,
            "abc",
            "application/octet-stream",
            Some(&r.claim_token),
        ))
        .await
        .unwrap();
    assert_eq!(competing.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = competing.into_body().collect().await.unwrap().to_bytes();
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["error"]["code"], "gateway_busy");
    assert_eq!(held.puts.load(Ordering::SeqCst), 1);
    held.release.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(5), a.assets.drain())
        .await
        .unwrap();
    let states: Vec<String> =
        sqlx::query_scalar("SELECT state FROM asset_chunks WHERE tenant_id=$1")
            .bind(who.tenant_id)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(states, vec!["stored"]);
    assert_eq!(held.puts.load(Ordering::SeqCst), 1);
    assert_eq!(
        a.complete(&who, b, &r.claim_token).await.unwrap()["state"],
        "quarantined"
    );
}
