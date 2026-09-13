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
    let (_, scanner, _) = scanner(&pool, &f, 1).await;
    let scans = Scans::new(o.assets.clone());
    let task = scans
        .claim(&scanner, Uuid::new_v4())
        .await
        .unwrap()
        .unwrap();
    let mut data = Vec::new();
    for n in 0..task["chunk_count"].as_i64().unwrap() as i32 {
        data.extend_from_slice(&scans.chunk(&scanner, attempt_id(&task), n).await.unwrap());
    }
    assert_eq!(data.as_slice(), f.r.data.as_ref());
    scans
        .report(&scanner, attempt_id(&task), scan_report(&task))
        .await
        .unwrap();
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

use brokerrouter::output_scans::{self as scan, Report, Scanner, Scans};
async fn scanner(pool: &PgPool, f: &Fixture, version: i32) -> (scan::Profile, Scanner, String) {
    let profile = scan::Profile {
        tenant_id: f.who.tenant_id,
        id: Uuid::new_v4(),
        model_id: f.e.model_id.clone(),
        version,
        engine_revision: common::hex(&store::hash(
            format!("fixture-contract-v{version}").as_bytes(),
        )),
    };
    scan::publish(pool, profile.clone()).await.unwrap();
    let (_, token) = scan::create_token(
        pool,
        f.who.tenant_id,
        profile.id,
        "isolated fixture",
        chrono::Utc::now() + chrono::Duration::hours(1),
    )
    .await
    .unwrap();
    let who = scan::authenticate(pool, &token).await.unwrap();
    (profile, who, token)
}
fn scan_report(v: &Value) -> Report {
    // Synthetic attestations exercise the gateway contract, not malware/format detection quality.
    serde_json::from_value(json!({"sha256":v["sha256"],"byte_length":v["byte_length"],"profile_version":v["profile_version"],"engine_revision":v["engine_revision"],"technical":"clean","content":"risk","media_type":"video/mp4","reason_codes":["fixture_risk"],"evidence_sha256":"a".repeat(64),"scanned_at":chrono::Utc::now()})).unwrap()
}
fn attempt_id(v: &Value) -> Uuid {
    serde_json::from_value(v["attempt_id"].clone()).unwrap()
}
async fn expire_scan_attempt(pool: &PgPool, id: Uuid) {
    // Time travel only in this isolated test database; production cannot rewrite attempt facts.
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("ALTER TABLE output_scan_attempts DISABLE TRIGGER output_scan_attempt_immutable")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("UPDATE output_scan_attempts SET created_at=clock_timestamp()-interval '10 minutes',lease_until=clock_timestamp()-interval '1 second',deadline=clock_timestamp()+interval '10 minutes' WHERE id=$1").bind(id).execute(&mut *tx).await.unwrap();
    sqlx::query("ALTER TABLE output_scan_attempts ENABLE TRIGGER output_scan_attempt_immutable")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

#[sqlx::test(migrations = "./migrations")]
async fn scanner_claim_read_receipt_is_durable_and_never_grants_delivery(pool: PgPool) {
    let f = setup(&pool).await;
    let job = generate(&f, &pool, None).await;
    f.o.tick().await.unwrap();
    let (_, who, _) = scanner(&pool, &f, 1).await;
    let s = Scans::new(f.o.assets.clone());
    let claim = Uuid::new_v4();
    let v = s.claim(&who, claim).await.unwrap().unwrap();
    let id = attempt_id(&v);
    assert_eq!(s.claim(&who, claim).await.unwrap().unwrap(), v);
    let mut bytes = Vec::new();
    for n in 0..v["chunk_count"].as_i64().unwrap() as i32 {
        bytes.extend_from_slice(&s.chunk(&who, id, n).await.unwrap());
    }
    assert_eq!(bytes.as_slice(), f.r.data.as_ref());
    assert_eq!(common::hex(&store::hash(&bytes)), v["sha256"]);
    let report = scan_report(&v);
    let done = s.report(&who, id, report.clone()).await.unwrap();
    assert_eq!(done["state"], "complete");
    assert_eq!(done["delivery"], "not_released");
    assert_eq!(done["approval"], "not_granted");
    assert_eq!(s.report(&who, id, report).await.unwrap(), done);
    assert!(s.chunk(&who, id, 0).await.is_err());
    assert!(s.renew(&who, id).await.is_err());
    let status = f.m.get(&f.who, job).await.unwrap();
    assert_eq!(status["output_scan"]["content"], "risk");
    assert_eq!(status["artifacts"], json!([]));
    assert_eq!(status["state"], "output_pending");
    assert!(
        f.o.assets
            .content_chunk(&f.who, asset_id(&pool, job).await, 0)
            .await
            .is_err()
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM guardrail_grants")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM audit_events WHERE action='output_scan.report_recorded'"
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        1
    );
    assert_eq!(balances(&pool, &f.who).await.0, 500000);
    assert_eq!(f.r.posts.load(Ordering::SeqCst), 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn scanner_concurrent_claims_have_one_persistent_owner(pool: PgPool) {
    let f = setup(&pool).await;
    generate(&f, &pool, None).await;
    f.o.tick().await.unwrap();
    let (_, who, _) = scanner(&pool, &f, 1).await;
    let s = Scans::new(f.o.assets.clone());
    let key = Uuid::new_v4();
    let (a, b) = tokio::join!(s.claim(&who, key), s.claim(&who, key));
    assert_eq!(a.unwrap(), b.unwrap());
    assert!(s.claim(&who, Uuid::new_v4()).await.unwrap().is_none());
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM output_scan_attempts")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, i32>("SELECT attempts FROM output_scans")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn scanner_stale_owner_cannot_read_renew_or_publish_and_retries_are_bounded(pool: PgPool) {
    let f = setup(&pool).await;
    generate(&f, &pool, None).await;
    f.o.tick().await.unwrap();
    let (_, who, _) = scanner(&pool, &f, 1).await;
    let s = Scans::new(f.o.assets.clone());
    let old_claim = Uuid::new_v4();
    let first = s.claim(&who, old_claim).await.unwrap().unwrap();
    let old = attempt_id(&first);
    expire_scan_attempt(&pool, old).await;
    assert!(s.claim(&who, old_claim).await.is_err());
    let second = s.claim(&who, Uuid::new_v4()).await.unwrap().unwrap();
    assert_ne!(attempt_id(&second), old);
    assert_eq!(second["scan_id"], first["scan_id"]);
    assert!(s.chunk(&who, old, 0).await.is_err());
    assert!(s.renew(&who, old).await.is_err());
    assert!(s.report(&who, old, scan_report(&first)).await.is_err());
    expire_scan_attempt(&pool, attempt_id(&second)).await;
    let third = s.claim(&who, Uuid::new_v4()).await.unwrap().unwrap();
    expire_scan_attempt(&pool, attempt_id(&third)).await;
    assert!(s.claim(&who, Uuid::new_v4()).await.unwrap().is_none());
    s.maintain().await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT state FROM output_scans")
            .fetch_one(&pool)
            .await
            .unwrap(),
        "closed"
    );
    assert_eq!(f.r.posts.load(Ordering::SeqCst), 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn scanner_receipt_must_bind_exact_bytes_profile_revision_and_strict_outcomes(pool: PgPool) {
    let f = setup(&pool).await;
    generate(&f, &pool, None).await;
    f.o.tick().await.unwrap();
    let (_, who, _) = scanner(&pool, &f, 1).await;
    let s = Scans::new(f.o.assets.clone());
    let v = s.claim(&who, Uuid::new_v4()).await.unwrap().unwrap();
    let id = attempt_id(&v);
    for (field, value) in [
        ("sha256", json!("0".repeat(64))),
        ("byte_length", json!(1)),
        ("profile_version", json!(2)),
        ("engine_revision", json!("unknown")),
        ("evidence_sha256", json!("https://private.example")),
        ("media_type", json!("text/html")),
        ("reason_codes", json!(["untrusted text, not code"])),
        ("scanned_at", json!("2000-01-01T00:00:00Z")),
    ] {
        let mut data = serde_json::to_value(scan_report(&v)).unwrap();
        data[field] = value;
        assert!(
            s.report(&who, id, serde_json::from_value(data).unwrap())
                .await
                .is_err(),
            "{field}"
        );
    }
    let mut r = scan_report(&v);
    r.technical = scan::Technical::Unsafe;
    assert!(s.report(&who, id, r.clone()).await.is_err());
    r.content = scan::Content::Unknown;
    r.media_type = None;
    let done = s.report(&who, id, r.clone()).await.unwrap();
    assert_eq!(done["report"]["technical"], "unsafe");
    r.reason_codes = vec!["other".into()];
    assert!(matches!(s.report(&who, id, r).await, Err(Error::Conflict)));
    let mut bad = serde_json::to_value(scan_report(&v)).unwrap();
    bad["approved"] = json!(true);
    assert!(serde_json::from_value::<Report>(bad).is_err());
    assert!(
        sqlx::query("UPDATE output_scans SET report='{}'")
            .execute(&pool)
            .await
            .is_err()
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn scanner_credential_rotation_and_profile_upgrade_fence_prior_work(pool: PgPool) {
    let f = setup(&pool).await;
    generate(&f, &pool, None).await;
    f.o.tick().await.unwrap();
    let (p, who, token) = scanner(&pool, &f, 1).await;
    let s = Scans::new(f.o.assets.clone());
    let old = s.claim(&who, Uuid::new_v4()).await.unwrap().unwrap();
    let renewed = s.renew(&who, attempt_id(&old)).await.unwrap();
    assert_eq!(renewed["deadline"], old["deadline"]);
    let (_, peer_token) = scan::create_token(
        &pool,
        f.who.tenant_id,
        p.id,
        "peer",
        chrono::Utc::now() + chrono::Duration::hours(1),
    )
    .await
    .unwrap();
    let peer = scan::authenticate(&pool, &peer_token).await.unwrap();
    assert!(s.chunk(&peer, attempt_id(&old), 0).await.is_err());
    assert!(s.claim(&peer, Uuid::new_v4()).await.unwrap().is_none());
    let id: Uuid = sqlx::query_scalar("SELECT id FROM output_scanner_tokens WHERE token_hash=$1")
        .bind(store::hash(token.as_bytes()))
        .fetch_one(&pool)
        .await
        .unwrap();
    scan::revoke_token(&pool, f.who.tenant_id, id)
        .await
        .unwrap();
    assert!(scan::authenticate(&pool, &token).await.is_err());
    assert!(s.chunk(&who, attempt_id(&old), 0).await.is_err());
    let (_, new_token) = scan::create_token(
        &pool,
        f.who.tenant_id,
        p.id,
        "rotated",
        chrono::Utc::now() + chrono::Duration::hours(1),
    )
    .await
    .unwrap();
    let who2 = scan::authenticate(&pool, &new_token).await.unwrap();
    let new = s.claim(&who2, Uuid::new_v4()).await.unwrap().unwrap();
    assert_eq!(new["scan_id"], old["scan_id"]);
    let (_, who3, _) = scanner(&pool, &f, 2).await;
    assert!(s.chunk(&who2, attempt_id(&new), 0).await.is_err());
    let latest = s.claim(&who3, Uuid::new_v4()).await.unwrap().unwrap();
    assert_eq!(latest["profile_version"], 2);
    assert_ne!(latest["scan_id"], new["scan_id"]);
    assert!(
        s.report(&who3, attempt_id(&latest), scan_report(&new))
            .await
            .is_err()
    );
    assert!(
        sqlx::query("UPDATE output_scan_profiles SET active=true WHERE id=$1")
            .bind(p.id)
            .execute(&pool)
            .await
            .is_err()
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn scanner_cancellation_key_and_turn_revocation_stop_reads_and_keep_model_cost(pool: PgPool) {
    for mode in 0..3 {
        let f = setup(&pool).await;
        let turn = if mode == 2 {
            common::provision(&pool, &f.who, "scan-user").await;
            Some(common::turn(&pool, &f.who, "scan-user", 0).await)
        } else {
            None
        };
        let job = generate(&f, &pool, turn).await;
        f.o.tick().await.unwrap();
        let (_, who, _) = scanner(&pool, &f, 1).await;
        let s = Scans::new(f.o.assets.clone());
        let v = s.claim(&who, Uuid::new_v4()).await.unwrap().unwrap();
        let id = attempt_id(&v);
        assert_eq!(s.chunk(&who, id, 0).await.unwrap().len(), CHUNK_BYTES);
        match mode {
            0 => {
                f.m.cancel(&f.who, job).await.unwrap();
            }
            1 => {
                store::revoke_key(&pool, f.who.tenant_id, f.who.key_id)
                    .await
                    .unwrap();
            }
            _ => {
                brokerrouter::governance::close_runtime(&pool, &f.who, turn.unwrap(), "completed")
                    .await
                    .unwrap();
            }
        }
        assert!(s.chunk(&who, id, 1).await.is_err());
        assert!(s.report(&who, id, scan_report(&v)).await.is_err());
        s.maintain().await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT state FROM output_scans WHERE id=$1")
                .bind(serde_json::from_value::<Uuid>(v["scan_id"].clone()).unwrap())
                .fetch_one(&pool)
                .await
                .unwrap(),
            "closed"
        );
        assert_eq!(balances(&pool, &f.who).await.0, 500000);
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn scanner_report_transaction_failure_recovers_without_new_generation(pool: PgPool) {
    let f = setup(&pool).await;
    generate(&f, &pool, None).await;
    f.o.tick().await.unwrap();
    let (_, who, _) = scanner(&pool, &f, 1).await;
    let s = Scans::new(f.o.assets.clone());
    let v = s.claim(&who, Uuid::new_v4()).await.unwrap().unwrap();
    let r = scan_report(&v);
    sqlx::raw_sql("CREATE FUNCTION fail_scan_audit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.action='output_scan.report_recorded' THEN RAISE EXCEPTION 'injected'; END IF; RETURN NEW; END; $$; CREATE TRIGGER injected_scan BEFORE INSERT ON audit_events FOR EACH ROW EXECUTE FUNCTION fail_scan_audit();").execute(&pool).await.unwrap();
    assert!(s.report(&who, attempt_id(&v), r.clone()).await.is_err());
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT state FROM output_scans")
            .fetch_one(&pool)
            .await
            .unwrap(),
        "scanning"
    );
    sqlx::query("DROP TRIGGER injected_scan ON audit_events")
        .execute(&pool)
        .await
        .unwrap();
    s.report(&who, attempt_id(&v), r).await.unwrap();
    assert_eq!(f.r.posts.load(Ordering::SeqCst), 1);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM output_scan_attempts")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn scanner_actual_database_role_cannot_read_inputs_change_money_or_approve(pool: PgPool) {
    let f = setup(&pool).await;
    generate(&f, &pool, None).await;
    f.o.tick().await.unwrap();
    let (_, who, _) = scanner(&pool, &f, 1).await;
    let input =
        f.o.assets
            .create(
                &f.who,
                "scan-hidden-input",
                assets::CreateAsset {
                    byte_length: 1,
                    sha256: common::hex(&store::hash(b"a")),
                    media_type: "application/octet-stream".into(),
                    ttl_seconds: 60,
                },
            )
            .await
            .unwrap();
    sqlx::raw_sql(include_str!("../deploy/permissions.sql"))
        .execute(&pool)
        .await
        .unwrap();
    let p = sqlx::postgres::PgPoolOptions::new()
        .max_connections(3)
        .after_connect(|c, _| {
            Box::pin(async move {
                sqlx::query("SET SESSION AUTHORIZATION brokerrouter_output_scan")
                    .execute(c)
                    .await?;
                Ok(())
            })
        })
        .connect_with(pool.connect_options().as_ref().clone())
        .await
        .unwrap();
    let s = Scans::new(Assets::new(p.clone(), f.objects.clone(), common::cipher(), 2).unwrap());
    let v = s.claim(&who, Uuid::new_v4()).await.unwrap().unwrap();
    s.chunk(&who, attempt_id(&v), 0).await.unwrap();
    s.renew(&who, attempt_id(&v)).await.unwrap();
    s.report(&who, attempt_id(&v), scan_report(&v))
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM assets WHERE id=$1")
            .bind(serde_json::from_value::<Uuid>(input["id"].clone()).unwrap())
            .fetch_one(&p)
            .await
            .unwrap(),
        0
    );
    for q in [
        "UPDATE tenants SET spent_micros=0",
        "UPDATE tenants SET asset_used_bytes=0",
        "UPDATE virtual_keys SET budget_micros=0",
        "UPDATE approvals SET state='approved'",
        "UPDATE media_jobs SET cancel_requested=true",
        "UPDATE assets SET state='deleted'",
        "UPDATE output_scan_profiles SET active=false",
        "UPDATE output_scanner_tokens SET revoked_at=NULL",
        "SET ROLE brokerrouter_assets",
        "SET ROLE brokerrouter_media_outputs",
        "SET ROLE brokerrouter_runtime",
        "SET ROLE brokerrouter_governance_control",
    ] {
        assert!(sqlx::query(q).execute(&p).await.is_err(), "{q}");
    }
    s.maintain().await.unwrap();
    p.close().await;
}

#[sqlx::test(migrations = "./migrations")]
async fn scanner_http_auth_scope_and_binary_boundary_are_enforced(pool: PgPool) {
    let f = setup(&pool).await;
    let job = generate(&f, &pool, None).await;
    f.o.tick().await.unwrap();
    let (_, who, token) = scanner(&pool, &f, 1).await;
    let other = setup(&pool).await;
    let (_, who2, token2) = scanner(&pool, &other, 1).await;
    let s = Scans::new(f.o.assets.clone());
    assert!(s.claim(&who2, Uuid::new_v4()).await.unwrap().is_none());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let root = format!("http://{}", listener.local_addr().unwrap());
    let app = brokerrouter::output_scan_api::router(s.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = reqwest::Client::new();
    for credential in [&f.key, "not-a-credential"] {
        assert_eq!(
            client
                .post(format!("{root}/v1/output-scans/claim"))
                .bearer_auth(credential)
                .json(&json!({"claim_id":Uuid::new_v4()}))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
    }
    assert!(store::authenticate(&pool, &token).await.is_err());
    let v: Value = client
        .post(format!("{root}/v1/output-scans/claim"))
        .bearer_auth(&token)
        .json(&json!({"claim_id":Uuid::new_v4()}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = attempt_id(&v);
    let url = format!("{root}/v1/output-scans/attempts/{id}/chunks/0");
    assert_eq!(
        client
            .get(&url)
            .bearer_auth(&token2)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        client
            .get(&url)
            .bearer_auth(&token)
            .header("range", "bytes=0-1")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    let r = client.get(&url).bearer_auth(&token).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.headers()["content-type"], "application/octet-stream");
    assert_eq!(r.headers()["cache-control"], "private, no-store");
    assert_eq!(r.headers()["x-content-type-options"], "nosniff");
    assert_eq!(r.bytes().await.unwrap(), f.r.data.slice(..CHUNK_BYTES));
    assert!(s.chunk(&who, id, -1).await.is_err());
    assert!(s.chunk(&who, id, 999).await.is_err());
    let mut r = serde_json::to_value(scan_report(&v)).unwrap();
    r["approved"] = json!(true);
    assert_eq!(
        client
            .post(format!("{root}/v1/output-scans/attempts/{id}/report"))
            .bearer_auth(&token)
            .json(&r)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(f.m.get(&f.who, job).await.unwrap()["artifacts"], json!([]));
    server.abort();
}
