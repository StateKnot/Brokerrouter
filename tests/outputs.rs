mod common;
use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::{HeaderMap, Request, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use brokerrouter::{
    asset_api,
    assets::{self, Assets, CHUNK_BYTES},
    error::Error,
    gateway::Gateway,
    media::{self, Media},
    media_protocol::{CreateJob, Endpoint, Profile},
    outputs::{self, Outputs},
    store,
};
use bytes::Bytes;
use http_body_util::BodyExt;
use object_store::{ObjectStore, ObjectStoreExt, memory::InMemory, path::Path};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tower::ServiceExt;
use uuid::Uuid;

#[derive(Clone)]
struct Remote {
    root: String,
    data: Bytes,
    gets: Arc<AtomicUsize>,
    posts: Arc<AtomicUsize>,
    mode: Arc<AtomicUsize>,
    headers: Arc<Mutex<Vec<HeaderMap>>>,
    tail: Arc<tokio::sync::Notify>,
}
async fn submit(State(r): State<Remote>) -> Json<Value> {
    r.posts.fetch_add(1, Ordering::SeqCst);
    Json(json!({"output":{"task_id":"task-one","task_status":"PENDING"}}))
}
async fn poll(State(r): State<Remote>) -> Json<Value> {
    Json(
        json!({"output":{"task_id":"task-one","task_status":"SUCCEEDED","video_url":format!("{}/file?signature=private-test-reference",r.root)},"usage":{"duration":5.0,"input_video_duration":0,"output_video_duration":5,"SR":720,"ratio":"16:9","video_count":1}}),
    )
}
async fn download(State(r): State<Remote>, h: HeaderMap) -> Response {
    let n = r.gets.fetch_add(1, Ordering::SeqCst);
    assert!(
        !h.contains_key("authorization") && !h.contains_key("cookie") && !h.contains_key("referer")
    );
    r.headers.lock().unwrap().push(h.clone());
    let mode = r.mode.load(Ordering::SeqCst);
    if mode == 1 {
        return (
            StatusCode::TEMPORARY_REDIRECT,
            [("location", format!("{}/forbidden", r.root))],
        )
            .into_response();
    }
    let etag = if mode == 2 {
        "\"version-two\""
    } else {
        "\"version-one\""
    };
    if h.get("if-match").is_some_and(|old| old != etag) {
        return StatusCode::PRECONDITION_FAILED.into_response();
    }
    if mode == 3 && n == 0 {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let body = if mode == 4 && n == 0 {
        let source = r.clone();
        Body::from_stream(futures_util::stream::try_unfold(0, move |part| {
            let source = source.clone();
            async move {
                match part {
                    0 => Ok::<_, std::io::Error>(Some((source.data.slice(..CHUNK_BYTES), 1))),
                    1 => {
                        source.tail.notified().await;
                        Ok(Some((source.data.slice(CHUNK_BYTES..), 2)))
                    }
                    _ => Ok(None),
                }
            }
        }))
    } else {
        Body::from(r.data.clone())
    };
    let mut response = body.into_response();
    response
        .headers_mut()
        .insert("content-length", r.data.len().to_string().parse().unwrap());
    response.headers_mut().insert("etag", etag.parse().unwrap());
    if mode == 5 {
        response.headers_mut().remove("etag");
    }
    if mode == 6 {
        response
            .headers_mut()
            .insert("etag", "W/\"weak\"".parse().unwrap());
    }
    if mode == 7 {
        response
            .headers_mut()
            .insert("content-encoding", "gzip".parse().unwrap());
    }
    if mode == 8 {
        *response.status_mut() = StatusCode::PARTIAL_CONTENT;
    }
    response
}
struct Fixture {
    m: Media,
    o: Outputs,
    who: store::Identity,
    key: String,
    e: Endpoint,
    r: Remote,
    objects: Arc<InMemory>,
    server: tokio::task::JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.r.tail.notify_waiters();
        self.server.abort();
    }
}
fn request() -> CreateJob {
    serde_json::from_value(json!({"model":"video/output","task":"video.generate","input":{"prompt":"fixture video"},"output":{"resolution":"720P","aspect_ratio":"16:9","duration_seconds":5}})).unwrap()
}
fn media_service(pool: &PgPool) -> Media {
    Media::new(
        Gateway::new(pool.clone(), common::cipher(), HashMap::new(), 8, 1, 60).unwrap(),
        HashMap::from([(
            "OUTPUT_TEST_KEY".into(),
            "fixture-secret-never-forward".into(),
        )]),
        1,
    )
    .unwrap()
}
async fn setup(pool: &PgPool) -> Fixture {
    let tenant = store::create_tenant(pool, "output", 100_000_000)
        .await
        .unwrap();
    store::put_model(pool, tenant, "video/output", "Video")
        .await
        .unwrap();
    let (key_id, key) = store::create_key(
        pool,
        tenant,
        "output",
        100_000_000,
        &["video/output".into()],
        None,
    )
    .await
    .unwrap();
    let who = store::Identity {
        tenant_id: tenant,
        key_id,
    };
    assets::set_quota(pool, tenant, None, 20_000_000)
        .await
        .unwrap();
    assets::set_quota(pool, tenant, Some(key_id), 20_000_000)
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let root = format!("http://{}", listener.local_addr().unwrap());
    let r = Remote {
        root: root.clone(),
        data: Bytes::from(vec![63; CHUNK_BYTES + 173]),
        gets: Arc::new(AtomicUsize::new(0)),
        posts: Arc::new(AtomicUsize::new(0)),
        mode: Arc::new(AtomicUsize::new(0)),
        headers: Arc::new(Mutex::new(Vec::new())),
        tail: Arc::new(tokio::sync::Notify::new()),
    };
    let app = Router::new()
        .route(
            "/api/v1/services/aigc/video-generation/video-synthesis",
            post(submit),
        )
        .route("/api/v1/tasks/{id}", get(poll))
        .route("/file", get(download))
        .route(
            "/forbidden",
            get(|| async {
                panic!("redirect must not be followed");
                #[allow(unreachable_code)]
                StatusCode::OK
            }),
        )
        .with_state(r.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let e = Endpoint {
        id: Uuid::new_v4(),
        tenant_id: tenant,
        model_id: "video/output".into(),
        name: "output fixture".into(),
        protocol: "dashscope-wan27-v1".into(),
        upstream_model: "wan2.7-t2v".into(),
        origin: root.clone(),
        credential_env: "OUTPUT_TEST_KEY".into(),
        allow_private: true,
        currency: "CNY".into(),
        priority: 0,
        profiles: vec![Profile {
            resolution: "720P".into(),
            aspect_ratio: "16:9".into(),
            durations: vec![5],
            price_per_second: "0.1".parse().unwrap(),
        }],
        maximum_output_bytes: 8_000_000,
        output_origins: vec![root],
    };
    media::put_endpoint(pool, e.clone()).await.unwrap();
    let objects = Arc::new(InMemory::new());
    let a = Assets::new(pool.clone(), objects.clone(), common::cipher(), 4).unwrap();
    Fixture {
        m: media_service(pool),
        o: Outputs::new(a, common::cipher()),
        who,
        key,
        e,
        r,
        objects,
        server,
    }
}
async fn generate(f: &Fixture, pool: &PgPool, turn: Option<Uuid>) -> Uuid {
    let v =
        f.m.create(&f.who, &Uuid::new_v4().to_string(), request(), turn)
            .await
            .unwrap();
    let id = serde_json::from_value(v["id"].clone()).unwrap();
    f.m.tick().await.unwrap();
    sqlx::query(
        "UPDATE media_jobs SET next_poll_at=clock_timestamp()-interval '1 second' WHERE id=$1",
    )
    .bind(id)
    .execute(pool)
    .await
    .unwrap();
    f.m.tick().await.unwrap();
    assert_eq!(
        f.m.get(&f.who, id).await.unwrap()["state"],
        "output_pending"
    );
    id
}
async fn balances(pool: &PgPool, who: &store::Identity) -> (i64, i64, i64) {
    sqlx::query_as("SELECT spent_micros,reserved_micros,asset_used_bytes FROM tenants WHERE id=$1")
        .bind(who.tenant_id)
        .fetch_one(pool)
        .await
        .unwrap()
}
async fn asset_id(pool: &PgPool, id: Uuid) -> Uuid {
    sqlx::query_scalar("SELECT asset_id FROM media_output_imports WHERE job_id=$1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}
async fn state(pool: &PgPool, id: Uuid) -> String {
    sqlx::query_scalar("SELECT state FROM media_output_imports WHERE job_id=$1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}
async fn due(pool: &PgPool, id: Uuid) {
    sqlx::query("UPDATE media_output_imports SET next_attempt_at=clock_timestamp()-interval '1 second' WHERE job_id=$1").bind(id).execute(pool).await.unwrap();
}
async fn expire_owner(pool: &PgPool, id: Uuid) {
    sqlx::query("UPDATE media_output_imports SET lease_until=clock_timestamp()-interval '1 second',next_attempt_at=clock_timestamp()-interval '1 second' WHERE job_id=$1").bind(id).execute(pool).await.unwrap();
}
async fn first_stored(pool: &PgPool, id: Uuid) {
    tokio::time::timeout(Duration::from_secs(15),async {
        loop {
            let n:i64=sqlx::query_scalar("SELECT count(*) FROM asset_chunks c JOIN media_output_imports i ON i.tenant_id=c.tenant_id AND i.asset_id=c.asset_id WHERE i.job_id=$1 AND c.state='stored'").bind(id).fetch_one(pool).await.unwrap();
            if n>0 {break;}tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.unwrap();
}

#[sqlx::test(migrations = "./migrations")]
async fn private_output_is_encrypted_and_all_input_api_bypasses_are_closed(pool: PgPool) {
    let f = setup(&pool).await;
    let job = generate(&f, &pool, None).await;
    assert_eq!(balances(&pool, &f.who).await, (500000, 0, 8_000_000));
    assert!(f.o.tick().await.unwrap());
    assert_eq!(state(&pool, job).await, "quarantined");
    assert_eq!(
        balances(&pool, &f.who).await,
        (500000, 0, f.r.data.len() as i64)
    );
    let id = asset_id(&pool, job).await;
    let digest: Vec<u8> = sqlx::query_scalar("SELECT sha256 FROM assets WHERE id=$1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(digest, store::hash(&f.r.data));
    let chunk: Uuid =
        sqlx::query_scalar("SELECT id FROM asset_chunks WHERE asset_id=$1 AND ordinal=0")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    let bytes = f
        .objects
        .get(&Path::from(format!(
            "assets-v1/{}/{}/{}",
            f.who.tenant_id, id, chunk
        )))
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_ne!(bytes, f.r.data.slice(..CHUNK_BYTES));
    assert_eq!(
        common::cipher()
            .decrypt(f.who.tenant_id, chunk, &bytes)
            .unwrap(),
        f.r.data[..CHUNK_BYTES]
    );
    let app = asset_api::router(f.o.assets.clone());
    for (method, path) in [
        ("GET", format!("/v1/assets/{id}")),
        ("GET", format!("/v1/assets/{id}/content")),
        ("GET", format!("/v1/assets/{id}/chunks/0")),
        ("PUT", format!("/v1/assets/{id}/chunks/0")),
        ("POST", format!("/v1/assets/{id}/complete")),
        ("DELETE", format!("/v1/assets/{id}")),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header("authorization", format!("Bearer {}", f.key))
                    .body(Body::from("a"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(
            !String::from_utf8_lossy(&response.into_body().collect().await.unwrap().to_bytes())
                .contains("signature")
        );
    }
    let v = f.m.get(&f.who, job).await.unwrap();
    assert_eq!(v["output_storage"]["state"], "quarantined");
    assert_eq!(v["artifacts"], json!([]));
    assert!(!v.to_string().contains("signature"));
    let remaining: bool =
        sqlx::query_scalar("SELECT output_ciphertext IS NOT NULL FROM media_jobs WHERE id=$1")
            .bind(job)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!remaining);
    assert_eq!(f.r.posts.load(Ordering::SeqCst), 1);
    assert_eq!(f.r.gets.load(Ordering::SeqCst), 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn output_capacity_and_money_admit_atomically_and_cancel_releases_once(pool: PgPool) {
    let f = setup(&pool).await;
    assets::set_quota(&pool, f.who.tenant_id, Some(f.who.key_id), 0)
        .await
        .unwrap();
    assert!(matches!(
        f.m.create(&f.who, "quota", request(), None).await,
        Err(Error::AssetQuota)
    ));
    assert_eq!(balances(&pool, &f.who).await, (0, 0, 0));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM requests")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    assets::set_quota(&pool, f.who.tenant_id, Some(f.who.key_id), 8_000_000)
        .await
        .unwrap();
    let (a, b) = tokio::join!(
        f.m.create(&f.who, "a", request(), None),
        f.m.create(&f.who, "b", request(), None)
    );
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
    let job: Uuid = serde_json::from_value(a.or(b).unwrap()["id"].clone()).unwrap();
    f.m.cancel(&f.who, job).await.unwrap();
    f.m.cancel(&f.who, job).await.unwrap();
    f.o.maintain().await.unwrap();
    assert_eq!(balances(&pool, &f.who).await, (0, 0, 0));
    assert_eq!(f.r.posts.load(Ordering::SeqCst), 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn object_written_before_database_failure_recovers_with_same_etag_and_no_generation(
    pool: PgPool,
) {
    let f = setup(&pool).await;
    let job = generate(&f, &pool, None).await;
    sqlx::raw_sql("CREATE FUNCTION fail_output_chunk() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected after object write'; END; $$; CREATE TRIGGER injected BEFORE UPDATE ON asset_chunks FOR EACH ROW EXECUTE FUNCTION fail_output_chunk();").execute(&pool).await.unwrap();
    assert!(f.o.tick().await.is_err());
    assert_eq!(state(&pool, job).await, "copying");
    let asset = asset_id(&pool, job).await;
    sqlx::query("DROP TRIGGER injected ON asset_chunks")
        .execute(&pool)
        .await
        .unwrap();
    expire_owner(&pool, job).await;
    f.o.tick().await.unwrap();
    assert_eq!(asset_id(&pool, job).await, asset);
    assert_eq!(state(&pool, job).await, "quarantined");
    assert_eq!(
        f.r.headers.lock().unwrap()[1]["if-match"],
        "\"version-one\""
    );
    assert_eq!(f.r.posts.load(Ordering::SeqCst), 1);
    assert_eq!(f.r.gets.load(Ordering::SeqCst), 2);
}

#[sqlx::test(migrations = "./migrations")]
async fn completed_asset_recovers_final_commit_without_refetching_expired_source(pool: PgPool) {
    let f = setup(&pool).await;
    let job = generate(&f, &pool, None).await;
    sqlx::raw_sql("CREATE FUNCTION fail_output_publish() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.state='quarantined' THEN RAISE EXCEPTION 'injected final publication'; END IF; RETURN NEW; END; $$; CREATE TRIGGER injected BEFORE UPDATE ON media_output_imports FOR EACH ROW EXECUTE FUNCTION fail_output_publish();").execute(&pool).await.unwrap();
    assert!(f.o.tick().await.is_err());
    let asset = asset_id(&pool, job).await;
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT state FROM assets WHERE id=$1")
            .bind(asset)
            .fetch_one(&pool)
            .await
            .unwrap(),
        "available"
    );
    sqlx::query("DROP TRIGGER injected ON media_output_imports")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE media_jobs SET output_ciphertext=NULL WHERE id=$1")
        .bind(job)
        .execute(&pool)
        .await
        .unwrap();
    expire_owner(&pool, job).await;
    f.o.tick().await.unwrap();
    assert_eq!(state(&pool, job).await, "quarantined");
    assert_eq!(f.r.gets.load(Ordering::SeqCst), 1);
    assert_eq!(
        balances(&pool, &f.who).await,
        (500000, 0, f.r.data.len() as i64)
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn changed_source_is_never_spliced_or_refunded_as_unpaid_generation(pool: PgPool) {
    let f = setup(&pool).await;
    let job = generate(&f, &pool, None).await;
    sqlx::raw_sql("CREATE FUNCTION fail_output_chunk() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected after object write'; END; $$; CREATE TRIGGER injected BEFORE UPDATE ON asset_chunks FOR EACH ROW EXECUTE FUNCTION fail_output_chunk();").execute(&pool).await.unwrap();
    assert!(f.o.tick().await.is_err());
    sqlx::query("DROP TRIGGER injected ON asset_chunks")
        .execute(&pool)
        .await
        .unwrap();
    f.r.mode.store(2, Ordering::SeqCst);
    expire_owner(&pool, job).await;
    f.o.tick().await.unwrap();
    assert_eq!(state(&pool, job).await, "failed");
    assert_eq!(balances(&pool, &f.who).await, (500000, 0, 0));
    assert_eq!(f.r.posts.load(Ordering::SeqCst), 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn cancellation_during_copy_fences_late_writes_and_gc_does_not_refund_model_cost(
    pool: PgPool,
) {
    let f = setup(&pool).await;
    let job = generate(&f, &pool, None).await;
    f.r.mode.store(4, Ordering::SeqCst);
    let o = f.o.clone();
    let running = tokio::spawn(async move { o.tick().await });
    first_stored(&pool, job).await;
    f.m.cancel(&f.who, job).await.unwrap();
    f.o.maintain().await.unwrap();
    f.r.tail.notify_one();
    let _ = running.await.unwrap();
    assert_eq!(state(&pool, job).await, "expired");
    assert_eq!(balances(&pool, &f.who).await, (500000, 0, 0));
    let id = asset_id(&pool, job).await;
    sqlx::query("UPDATE assets SET gc_after=clock_timestamp()-interval '1 second' WHERE id=$1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    f.o.assets.maintain().await.unwrap();
    let count = f
        .objects
        .list_with_delimiter(Some(&Path::from(format!(
            "assets-v1/{}/{id}",
            f.who.tenant_id
        ))))
        .await
        .unwrap()
        .objects
        .len();
    assert_eq!(count, 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn stale_copy_owner_cannot_publish_after_another_worker_completes(pool: PgPool) {
    let f = setup(&pool).await;
    let job = generate(&f, &pool, None).await;
    f.r.mode.store(4, Ordering::SeqCst);
    let o = f.o.clone();
    let old = tokio::spawn(async move { o.tick().await });
    first_stored(&pool, job).await;
    expire_owner(&pool, job).await;
    f.o.tick().await.unwrap();
    f.r.tail.notify_one();
    assert!(old.await.unwrap().is_err());
    assert_eq!(state(&pool, job).await, "quarantined");
    assert_eq!(f.r.posts.load(Ordering::SeqCst), 1);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM audit_events WHERE action='output.quarantined'"
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        1
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn invalid_transport_contracts_fail_closed_and_transient_get_errors_retry(pool: PgPool) {
    for mode in [1, 5, 6, 7, 8] {
        let f = setup(&pool).await;
        let job = generate(&f, &pool, None).await;
        f.r.mode.store(mode, Ordering::SeqCst);
        f.o.tick().await.unwrap();
        assert_eq!(state(&pool, job).await, "failed");
        assert_eq!(balances(&pool, &f.who).await, (500000, 0, 0));
        assert_eq!(f.r.gets.load(Ordering::SeqCst), 1);
    }
    let f = setup(&pool).await;
    let job = generate(&f, &pool, None).await;
    f.r.mode.store(3, Ordering::SeqCst);
    f.o.tick().await.unwrap();
    assert_eq!(state(&pool, job).await, "pending");
    due(&pool, job).await;
    f.o.tick().await.unwrap();
    assert_eq!(state(&pool, job).await, "quarantined");
    assert_eq!(f.r.posts.load(Ordering::SeqCst), 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn source_expiry_and_closed_turn_clear_private_capacity_without_fetch_or_fee_reversal(
    pool: PgPool,
) {
    let f = setup(&pool).await;
    let job = generate(&f, &pool, None).await;
    sqlx::query(
        "UPDATE media_jobs SET output_expires_at=clock_timestamp()-interval '1 second' WHERE id=$1",
    )
    .bind(job)
    .execute(&pool)
    .await
    .unwrap();
    f.o.maintain().await.unwrap();
    assert_eq!(state(&pool, job).await, "expired");
    assert_eq!(f.r.gets.load(Ordering::SeqCst), 0);
    assert_eq!(balances(&pool, &f.who).await, (500000, 0, 0));
    let f = setup(&pool).await;
    common::provision(&pool, &f.who, "output-user").await;
    let turn = common::turn(&pool, &f.who, "output-user", 0).await;
    let job = generate(&f, &pool, Some(turn)).await;
    f.o.tick().await.unwrap();
    brokerrouter::governance::close_runtime(&pool, &f.who, turn, "completed")
        .await
        .unwrap();
    f.o.maintain().await.unwrap();
    assert_eq!(state(&pool, job).await, "expired");
    assert_eq!(balances(&pool, &f.who).await, (500000, 0, 0));
}

#[sqlx::test(migrations = "./migrations")]
async fn output_database_role_can_import_but_cannot_generate_change_money_or_approve(pool: PgPool) {
    let f = setup(&pool).await;
    let job = generate(&f, &pool, None).await;
    sqlx::raw_sql(include_str!("../deploy/permissions.sql"))
        .execute(&pool)
        .await
        .unwrap();
    let p = sqlx::postgres::PgPoolOptions::new()
        .max_connections(5)
        .after_connect(|c, _| {
            Box::pin(async move {
                // SET ROLE alone would retain the fixture superuser's ability to switch roles.
                sqlx::query("SET SESSION AUTHORIZATION brokerrouter_media_outputs")
                    .execute(c)
                    .await?;
                Ok(())
            })
        })
        .connect_with(pool.connect_options().as_ref().clone())
        .await
        .unwrap();
    let o = Outputs::new(
        Assets::new(p.clone(), f.objects.clone(), common::cipher(), 4).unwrap(),
        common::cipher(),
    );
    o.tick().await.unwrap();
    assert_eq!(state(&pool, job).await, "quarantined");
    for query in [
        "UPDATE tenants SET spent_micros=0",
        "UPDATE requests SET status='succeeded'",
        "INSERT INTO ledger_entries(tenant_id,request_id,kind,amount_micros) SELECT tenant_id,id,'finalize',0 FROM requests",
        "UPDATE approvals SET state='approved'",
        "UPDATE media_jobs SET cancel_requested=true",
        "UPDATE media_output_imports SET maximum_bytes=1",
        "SET ROLE brokerrouter_assets",
        "SET ROLE brokerrouter_runtime",
    ] {
        assert!(sqlx::query(query).execute(&p).await.is_err(), "{query}");
    }
    assert!(
        media_service(&p)
            .create(&f.who, "must-not-generate", request(), None)
            .await
            .is_err()
    );
    let input_role = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .after_connect(|c, _| {
            Box::pin(async move {
                sqlx::query("SET SESSION AUTHORIZATION brokerrouter_assets")
                    .execute(c)
                    .await?;
                Ok(())
            })
        })
        .connect_with(pool.connect_options().as_ref().clone())
        .await
        .unwrap();
    assert!(
        sqlx::query("SET ROLE brokerrouter_media_outputs")
            .execute(&input_role)
            .await
            .is_err()
    );
    let asset = asset_id(&pool, job).await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM assets WHERE id=$1")
            .bind(asset)
            .fetch_one(&input_role)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM asset_chunks WHERE asset_id=$1")
            .bind(asset)
            .fetch_one(&input_role)
            .await
            .unwrap(),
        0
    );
    let input = Assets::new(input_role.clone(), f.objects.clone(), common::cipher(), 2).unwrap();
    let payload = assets::CreateAsset {
        byte_length: 1,
        sha256: common::hex(&store::hash(b"a")),
        media_type: "application/octet-stream".into(),
        ttl_seconds: 60,
    };
    let uploaded: Uuid = serde_json::from_value(
        input.create(&f.who, "input-role", payload).await.unwrap()["id"].clone(),
    )
    .unwrap();
    input
        .put_chunk(&f.who, uploaded, 0, Bytes::from_static(b"a"))
        .await
        .unwrap();
    input.complete(&f.who, uploaded).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM assets WHERE id=$1")
            .bind(uploaded)
            .fetch_one(&p)
            .await
            .unwrap(),
        0
    );
    assert!(input.content_chunk(&f.who, asset, 0).await.is_err());
    input_role.close().await;
    p.close().await;
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires isolated S3 fixture and BROKERROUTER_ASSET_* configuration"]
async fn s3_output_import_quarantine_and_cleanup(pool: PgPool) {
    let f = setup(&pool).await;
    let job = generate(&f, &pool, None).await;
    let o = Outputs::new(Assets::from_env(pool.clone()).unwrap(), common::cipher());
    o.tick().await.unwrap();
    assert_eq!(state(&pool, job).await, "quarantined");
    f.m.cancel(&f.who, job).await.unwrap();
    o.maintain().await.unwrap();
    let id = asset_id(&pool, job).await;
    sqlx::query("UPDATE assets SET gc_after=clock_timestamp()-interval '1 second' WHERE id=$1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    o.assets.maintain().await.unwrap();
    assert!(
        sqlx::query_scalar::<_, bool>("SELECT gc_checked_at IS NOT NULL FROM assets WHERE id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap()
    );
    assert_eq!(balances(&pool, &f.who).await, (500000, 0, 0));
}

#[sqlx::test(migrations = "./migrations")]
async fn output_origin_is_exact_and_frozen_endpoint_config_cannot_be_mutated(pool: PgPool) {
    let f = setup(&pool).await;
    assert!(outputs::source_url(&f.e, &format!("{}/file?a=1", f.r.root)).is_ok());
    for url in [
        "https://different.example/file",
        "http://user:password@127.0.0.1/file",
        "file:///etc/passwd",
        "data:text/plain,secret",
    ] {
        assert!(outputs::source_url(&f.e, url).is_err());
    }
    assert!(outputs::source_url(&f.e, &format!("{}/file#fragment", f.r.root)).is_err());
    let mut invalid = f.e.clone();
    invalid.maximum_output_bytes = 0;
    assert!(invalid.validate_output_contract().is_err());
    assert!(sqlx::query("UPDATE media_endpoints SET config=jsonb_set(config,'{output_origins}','[\"https://attacker.example\"]')").execute(&pool).await.is_err());
}
