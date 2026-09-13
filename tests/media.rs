mod common;
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use brokerrouter::{
    accounting, attempts,
    error::Error,
    gateway::Gateway,
    media::{self, Media},
    media_protocol::{self, CreateJob, Endpoint, Profile},
    money, store,
};
use rust_decimal::Decimal;
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
use uuid::Uuid;

#[derive(Clone)]
struct Remote {
    posts: Arc<AtomicUsize>,
    polls: Arc<AtomicUsize>,
    mode: Arc<AtomicUsize>,
    body: Arc<Mutex<Value>>,
}
impl Remote {
    fn new() -> Self {
        Self {
            posts: Arc::new(AtomicUsize::new(0)),
            polls: Arc::new(AtomicUsize::new(0)),
            mode: Arc::new(AtomicUsize::new(0)),
            body: Arc::new(Mutex::new(Value::Null)),
        }
    }
}
fn completed() -> Value {
    json!({"output":{"task_id":"task-123","task_status":"SUCCEEDED","video_url":"https://private-result.example/video.mp4?signature=not-public"},"usage":{"duration":5.0,"input_video_duration":0,"output_video_duration":5,"SR":720,"ratio":"16:9","video_count":1}})
}
async fn submit(State(s): State<Remote>, h: HeaderMap, Json(body): Json<Value>) -> Response {
    assert_eq!(h["authorization"], "Bearer fixture-only");
    assert_eq!(h["x-dashscope-async"], "enable");
    s.posts.fetch_add(1, Ordering::SeqCst);
    *s.body.lock().unwrap() = body;
    if s.mode.load(Ordering::SeqCst) == 1 {
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    if s.mode.load(Ordering::SeqCst) == 4 {
        return (
            StatusCode::TEMPORARY_REDIRECT,
            [("location", "http://127.0.0.1:1/secret")],
        )
            .into_response();
    }
    Json(json!({"output":{"task_id":"task-123","task_status":"PENDING"}})).into_response()
}
async fn poll(State(s): State<Remote>) -> Response {
    let n = s.polls.fetch_add(1, Ordering::SeqCst);
    if s.mode.load(Ordering::SeqCst) == 2 && n == 0 {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    if s.mode.load(Ordering::SeqCst) == 3 && n == 0 {
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    if s.mode.load(Ordering::SeqCst) == 5 {
        return Json(json!({"output":{"task_id":"task-123","task_status":"FAILED"}}))
            .into_response();
    }
    Json(completed()).into_response()
}
struct Fixture {
    m: Media,
    who: store::Identity,
    key: String,
    e: Endpoint,
    remote: Remote,
    server: tokio::task::JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}
async fn setup(pool: &PgPool, currency: &str) -> Fixture {
    let tenant = store::create_tenant(pool, "media", 100_000_000)
        .await
        .unwrap();
    store::put_model(pool, tenant, "video/model", "Video")
        .await
        .unwrap();
    let (key_id, key) = store::create_key(
        pool,
        tenant,
        "video",
        100_000_000,
        &["video/model".into()],
        None,
    )
    .await
    .unwrap();
    let who = store::Identity {
        tenant_id: tenant,
        key_id,
    };
    if currency == "USD" {
        store::set_fx(
            pool,
            tenant,
            Decimal::from(7),
            chrono::Utc::now() - chrono::Duration::seconds(2),
            "fixture",
        )
        .await
        .unwrap();
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let remote = Remote::new();
    let app = Router::new()
        .route(
            "/api/v1/services/aigc/video-generation/video-synthesis",
            post(submit),
        )
        .route("/api/v1/tasks/{task}", get(poll))
        .with_state(remote.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let e = Endpoint {
        id: Uuid::new_v4(),
        tenant_id: tenant,
        model_id: "video/model".into(),
        name: "fixture".into(),
        protocol: "dashscope-wan27-v1".into(),
        upstream_model: "wan2.7-t2v-2026-06-12".into(),
        origin: format!("http://{addr}"),
        credential_env: "MEDIA_FIXTURE_KEY".into(),
        allow_private: true,
        currency: currency.into(),
        priority: 10,
        profiles: vec![Profile {
            resolution: "720P".into(),
            aspect_ratio: "16:9".into(),
            durations: vec![5],
            price_per_second: money::nonnegative("0.1").unwrap(),
        }],
    };
    media::put_endpoint(pool, e.clone()).await.unwrap();
    let m = service(pool);
    Fixture {
        m,
        who,
        key,
        e,
        remote,
        server,
    }
}
fn service(pool: &PgPool) -> Media {
    Media::new(
        Gateway::new(pool.clone(), common::cipher(), HashMap::new(), 8, 1, 60).unwrap(),
        HashMap::from([("MEDIA_FIXTURE_KEY".into(), "fixture-only".into())]),
        1,
    )
    .unwrap()
}
fn body() -> CreateJob {
    serde_json::from_value(json!({"model":"video/model","task":"video.generate","input":{"prompt":"a safe video"},"output":{"resolution":"720P","aspect_ratio":"16:9","duration_seconds":5}})).unwrap()
}
fn id(v: &Value) -> Uuid {
    serde_json::from_value(v["id"].clone()).unwrap()
}
async fn due(pool: &PgPool, id: Uuid) {
    sqlx::query(
        "UPDATE media_jobs SET next_poll_at=clock_timestamp()-interval '1 second' WHERE id=$1",
    )
    .bind(id)
    .execute(pool)
    .await
    .unwrap();
}
async fn expire_lease(pool: &PgPool, id: Uuid) {
    sqlx::query("UPDATE media_jobs SET lease_until=clock_timestamp()-interval '1 second',next_poll_at=clock_timestamp()-interval '1 second' WHERE id=$1").bind(id).execute(pool).await.unwrap();
}
async fn balance(pool: &PgPool, who: &store::Identity) -> (i64, i64) {
    sqlx::query_as("SELECT spent_micros,reserved_micros FROM tenants WHERE id=$1")
        .bind(who.tenant_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[sqlx::test(migrations = "./migrations")]
async fn job_and_reservation_commit_atomically_and_idempotency_is_scoped(pool: PgPool) {
    let f = setup(&pool, "CNY").await;
    sqlx::raw_sql("CREATE FUNCTION fail_media() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected job failure'; END; $$; CREATE TRIGGER injected BEFORE INSERT ON media_jobs FOR EACH ROW EXECUTE FUNCTION fail_media();").execute(&pool).await.unwrap();
    assert!(f.m.create(&f.who, "atomic", body(), None).await.is_err());
    assert_eq!(balance(&pool, &f.who).await, (0, 0));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM requests")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    sqlx::query("DROP TRIGGER injected ON media_jobs")
        .execute(&pool)
        .await
        .unwrap();
    let (a, b) = tokio::join!(
        f.m.create(&f.who, "atomic", body(), None),
        f.m.create(&f.who, "atomic", body(), None)
    );
    let asset = id(&a.unwrap());
    assert_eq!(asset, id(&b.unwrap()));
    assert_eq!(balance(&pool, &f.who).await, (0, 500000));
    let mut changed = body();
    changed.input.prompt = "changed".into();
    assert!(matches!(
        f.m.create(&f.who, "atomic", changed, None).await,
        Err(Error::Conflict)
    ));
    let g = setup(&pool, "CNY").await;
    assert!(matches!(g.m.get(&g.who, asset).await, Err(Error::NotFound)));
    assert_eq!(f.remote.posts.load(Ordering::SeqCst), 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn real_http_submission_poll_and_frozen_cny_settlement_never_release_provider_url(
    pool: PgPool,
) {
    let f = setup(&pool, "USD").await;
    let job = id(&f.m.create(&f.who, "bill", body(), None).await.unwrap());
    store::set_fx(
        &pool,
        f.who.tenant_id,
        Decimal::from(8),
        chrono::Utc::now() - chrono::Duration::seconds(1),
        "new rate",
    )
    .await
    .unwrap();
    assert!(f.m.tick().await.unwrap());
    assert_eq!(f.m.get(&f.who, job).await.unwrap()["state"], "running");
    let sent = f.remote.body.lock().unwrap().clone();
    assert_eq!(sent["parameters"]["watermark"], true);
    assert_eq!(sent["parameters"]["prompt_extend"], false);
    assert!(!f.m.tick().await.unwrap());
    f.m.events(&f.who, job, 0).await.unwrap();
    assert_eq!(f.remote.polls.load(Ordering::SeqCst), 0);
    due(&pool, job).await;
    assert!(service(&pool).tick().await.unwrap());
    let result = f.m.get(&f.who, job).await.unwrap();
    assert_eq!(result["state"], "output_pending");
    assert_eq!(result["cost"]["final"], "3.500000");
    assert_eq!(result["delivery"], "not_released");
    assert!(!result.to_string().contains("signature"));
    assert_eq!(result["artifacts"], json!([]));
    assert_eq!(balance(&pool, &f.who).await, (3500000, 0));
    assert_eq!(f.remote.posts.load(Ordering::SeqCst), 1);
    assert_eq!(f.remote.polls.load(Ordering::SeqCst), 1);
    assert!(!f.m.tick().await.unwrap());
    let bytes: Vec<u8> = sqlx::query_scalar("SELECT output_ciphertext FROM media_jobs WHERE id=$1")
        .bind(job)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(!String::from_utf8_lossy(&bytes).contains("private-result"));
    assert!(
        common::cipher()
            .decrypt(f.who.tenant_id, job, &bytes)
            .is_ok()
    );
    assert!(
        common::cipher()
            .decrypt(Uuid::new_v4(), job, &bytes)
            .is_err()
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn accepted_timeout_never_resubmits_and_operator_can_attach_original_task(pool: PgPool) {
    let f = setup(&pool, "CNY").await;
    f.remote.mode.store(1, Ordering::SeqCst);
    let job = id(&f.m.create(&f.who, "timeout", body(), None).await.unwrap());
    f.m.tick().await.unwrap();
    assert_eq!(
        f.m.get(&f.who, job).await.unwrap()["state"],
        "submission_unknown"
    );
    assert!(!service(&pool).tick().await.unwrap());
    assert_eq!(balance(&pool, &f.who).await, (0, 500000));
    assert!(
        accounting::release_unsubmitted(&pool, &f.who, job)
            .await
            .is_err()
    );
    media::attach_task(
        &pool,
        f.who.tenant_id,
        job,
        "task-123",
        "fixture acceptance record",
    )
    .await
    .unwrap();
    media::attach_task(
        &pool,
        f.who.tenant_id,
        job,
        "task-123",
        "duplicate evidence",
    )
    .await
    .unwrap();
    assert!(
        media::attach_task(
            &pool,
            f.who.tenant_id,
            job,
            "task-other",
            "invalid replacement"
        )
        .await
        .is_err()
    );
    due(&pool, job).await;
    f.m.tick().await.unwrap();
    assert_eq!(f.remote.posts.load(Ordering::SeqCst), 1);
    assert_eq!(balance(&pool, &f.who).await, (500000, 0));
}

#[sqlx::test(migrations = "./migrations")]
async fn acceptance_persistence_failure_and_stale_submit_lease_do_not_resend(pool: PgPool) {
    let f = setup(&pool, "CNY").await;
    let job = id(&f.m.create(&f.who, "lost-db", body(), None).await.unwrap());
    sqlx::raw_sql("CREATE FUNCTION fail_task_id() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.provider_task_id IS NOT NULL THEN RAISE EXCEPTION 'injected task ID failure'; END IF; RETURN NEW; END; $$; CREATE TRIGGER injected BEFORE UPDATE ON media_jobs FOR EACH ROW EXECUTE FUNCTION fail_task_id();").execute(&pool).await.unwrap();
    assert!(f.m.tick().await.is_err());
    assert_eq!(f.remote.posts.load(Ordering::SeqCst), 1);
    sqlx::query("DROP TRIGGER injected ON media_jobs")
        .execute(&pool)
        .await
        .unwrap();
    expire_lease(&pool, job).await;
    service(&pool).tick().await.unwrap();
    assert_eq!(
        f.m.get(&f.who, job).await.unwrap()["state"],
        "submission_unknown"
    );
    assert!(!f.m.tick().await.unwrap());
    assert_eq!(f.remote.posts.load(Ordering::SeqCst), 1);
    let usage = vec![money::UnitUsage {
        unit: "video_second_720p".into(),
        quantity: Decimal::ZERO,
    }];
    media::reconcile(&pool, f.who.tenant_id, job, &usage, "confirmed not charged")
        .await
        .unwrap();
    media::reconcile(&pool, f.who.tenant_id, job, &usage, "confirmed not charged")
        .await
        .unwrap();
    assert_eq!(balance(&pool, &f.who).await, (0, 0));
}

#[sqlx::test(migrations = "./migrations")]
async fn poll_errors_retry_get_only_and_revocation_does_not_erase_paid_work(pool: PgPool) {
    let f = setup(&pool, "CNY").await;
    f.remote.mode.store(2, Ordering::SeqCst);
    let job = id(&f.m.create(&f.who, "poll", body(), None).await.unwrap());
    f.m.tick().await.unwrap();
    store::revoke_key(&pool, f.who.tenant_id, f.who.key_id)
        .await
        .unwrap();
    due(&pool, job).await;
    f.m.tick().await.unwrap();
    assert_eq!(balance(&pool, &f.who).await, (0, 500000));
    due(&pool, job).await;
    f.m.tick().await.unwrap();
    assert_eq!(balance(&pool, &f.who).await, (500000, 0));
    assert!(matches!(
        f.m.get(&f.who, job).await,
        Err(Error::Unauthorized)
    ));
    assert_eq!(f.remote.posts.load(Ordering::SeqCst), 1);
    assert_eq!(f.remote.polls.load(Ordering::SeqCst), 2);
}

#[sqlx::test(migrations = "./migrations")]
async fn cancellation_before_dispatch_refunds_but_after_dispatch_does_not_claim_success(
    pool: PgPool,
) {
    let f = setup(&pool, "CNY").await;
    let first = id(&f
        .m
        .create(&f.who, "cancel-first", body(), None)
        .await
        .unwrap());
    assert_eq!(
        f.m.cancel(&f.who, first).await.unwrap()["state"],
        "cancelled"
    );
    f.m.cancel(&f.who, first).await.unwrap();
    assert_eq!(balance(&pool, &f.who).await, (0, 0));
    assert!(!f.m.tick().await.unwrap());
    let second = id(&f
        .m
        .create(&f.who, "cancel-late", body(), None)
        .await
        .unwrap());
    f.m.tick().await.unwrap();
    let pending = f.m.cancel(&f.who, second).await.unwrap();
    assert_eq!(pending["state"], "running");
    assert_eq!(pending["cancel_requested"], true);
    assert_eq!(balance(&pool, &f.who).await, (0, 500000));
    due(&pool, second).await;
    f.m.tick().await.unwrap();
    assert_eq!(balance(&pool, &f.who).await, (500000, 0));
    assert_eq!(
        f.m.get(&f.who, second).await.unwrap()["delivery"],
        "not_released"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn input_guardrail_pauses_job_and_only_human_current_turn_grant_resumes(pool: PgPool) {
    let f = setup(&pool, "CNY").await;
    common::provision(&pool, &f.who, "alice").await;
    let turn = common::turn(&pool, &f.who, "alice", 0).await;
    common::guardrail(
        &pool,
        f.who.tenant_id,
        "video-risk",
        1,
        "pre",
        json!({"kind":"always"}),
    )
    .await;
    assert!(f.m.create(&f.who, "no-turn", body(), None).await.is_err());
    let job = id(&f
        .m
        .create(&f.who, "approval", body(), Some(turn))
        .await
        .unwrap());
    f.m.tick().await.unwrap();
    let waiting = f.m.get(&f.who, job).await.unwrap();
    assert_eq!(waiting["state"], "waiting_approval");
    assert_eq!(f.remote.posts.load(Ordering::SeqCst), 0);
    common::decide(
        &pool,
        "alice",
        common::approval_id(&waiting),
        common::approve(&["video-risk"]),
    )
    .await
    .unwrap();
    due(&pool, job).await;
    f.m.tick().await.unwrap();
    assert_eq!(f.remote.posts.load(Ordering::SeqCst), 1);
    let second = id(&f
        .m
        .create(&f.who, "same-turn", body(), Some(turn))
        .await
        .unwrap());
    f.m.tick().await.unwrap();
    assert_eq!(f.remote.posts.load(Ordering::SeqCst), 2);
    assert_eq!(f.m.get(&f.who, second).await.unwrap()["state"], "running");
}

#[sqlx::test(migrations = "./migrations")]
async fn late_poll_owner_is_fenced_from_duplicate_settlement(pool: PgPool) {
    let f = setup(&pool, "CNY").await;
    f.remote.mode.store(3, Ordering::SeqCst);
    let job = id(&f.m.create(&f.who, "fencing", body(), None).await.unwrap());
    f.m.tick().await.unwrap();
    due(&pool, job).await;
    let worker = f.m.clone();
    let old = tokio::spawn(async move { worker.tick().await });
    tokio::time::timeout(Duration::from_secs(2), async {
        while f.remote.polls.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    expire_lease(&pool, job).await;
    f.m.tick().await.unwrap();
    assert!(old.await.unwrap().is_err());
    assert_eq!(balance(&pool, &f.who).await, (500000, 0));
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM ledger_entries WHERE request_id=$1 AND kind='finalize'"
        )
        .bind(job)
        .fetch_one(&pool)
        .await
        .unwrap(),
        1
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn settlement_write_failure_rolls_back_money_and_recovers_with_get_only(pool: PgPool) {
    let f = setup(&pool, "CNY").await;
    let job = id(&f
        .m
        .create(&f.who, "settlement-failure", body(), None)
        .await
        .unwrap());
    f.m.tick().await.unwrap();
    sqlx::raw_sql("CREATE FUNCTION fail_media_output() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.output_ciphertext IS NOT NULL THEN RAISE EXCEPTION 'injected output persistence failure'; END IF; RETURN NEW; END; $$; CREATE TRIGGER injected BEFORE UPDATE ON media_jobs FOR EACH ROW EXECUTE FUNCTION fail_media_output();").execute(&pool).await.unwrap();
    due(&pool, job).await;
    assert!(f.m.tick().await.is_err());
    assert_eq!(balance(&pool, &f.who).await, (0, 500000));
    assert_eq!(f.m.get(&f.who, job).await.unwrap()["state"], "running");
    sqlx::query("DROP TRIGGER injected ON media_jobs")
        .execute(&pool)
        .await
        .unwrap();
    expire_lease(&pool, job).await;
    f.m.tick().await.unwrap();
    assert_eq!(balance(&pool, &f.who).await, (500000, 0));
    assert_eq!(f.remote.posts.load(Ordering::SeqCst), 1);
    assert_eq!(f.remote.polls.load(Ordering::SeqCst), 2);
    let evidence: Value = sqlx::query_scalar(
        "SELECT details FROM ledger_entries WHERE request_id=$1 AND kind='finalize'",
    )
    .bind(job)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(evidence["provider_usage"], completed()["usage"]);
    assert_eq!(evidence["provider_task_id"], "task-123");
}

#[sqlx::test(migrations = "./migrations")]
async fn real_media_http_is_authorized_and_dynamic_profiles_do_not_leak_credentials(pool: PgPool) {
    let f = setup(&pool, "CNY").await;
    let mut second = f.e.clone();
    second.id = Uuid::new_v4();
    second.priority = 0;
    second.profiles[0].durations = vec![8];
    media::put_endpoint(&pool, second.clone()).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let root = format!("http://{}", listener.local_addr().unwrap());
    let app = brokerrouter::media_api::router(f.m.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = reqwest::Client::new();
    assert_eq!(
        client
            .get(format!("{root}/v1/media/capabilities?model=video/model"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let cap = client
        .get(format!("{root}/v1/media/capabilities?model=video/model"))
        .bearer_auth(&f.key)
        .send()
        .await
        .unwrap();
    assert_eq!(cap.headers()["cache-control"], "private, no-store");
    let cap: Value = cap.json().await.unwrap();
    assert_eq!(cap["endpoints"].as_array().unwrap().len(), 2);
    assert_eq!(cap["endpoints"][0]["id"], json!(second.id));
    for secret in ["origin", "credential_env", "fixture-only", "upstream_model"] {
        assert!(!cap.to_string().contains(secret));
    }
    let response = client
        .post(format!("{root}/v1/media/jobs"))
        .bearer_auth(&f.key)
        .header("Idempotency-Key", "http-video")
        .json(&body())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert!(response.headers().contains_key("x-request-id"));
    let created: Value = response.json().await.unwrap();
    assert_eq!(created["endpoint_id"], json!(f.e.id)); // Higher priority endpoint lacks this profile.
    let job = id(&created);
    let read: Value = client
        .get(format!("{root}/v1/media/jobs/{job}"))
        .bearer_auth(&f.key)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(read["state"], "queued");
    assert_eq!(f.remote.posts.load(Ordering::SeqCst), 0);
    assert_eq!(f.remote.polls.load(Ordering::SeqCst), 0);
    let cancelled: Value = client
        .post(format!("{root}/v1/media/jobs/{job}/cancel"))
        .bearer_auth(&f.key)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(cancelled["state"], "cancelled");
    media::disable_endpoint(&pool, f.who.tenant_id, second.id)
        .await
        .unwrap();
    assert_eq!(
        f.m.capabilities(&f.who, "video/model").await.unwrap()["endpoints"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    server.abort();
}

#[sqlx::test(migrations = "./migrations")]
async fn disabled_endpoint_before_dispatch_releases_without_paid_post(pool: PgPool) {
    let f = setup(&pool, "CNY").await;
    let job = id(&f
        .m
        .create(&f.who, "disable-before-dispatch", body(), None)
        .await
        .unwrap());
    media::disable_endpoint(&pool, f.who.tenant_id, f.e.id)
        .await
        .unwrap();
    f.m.tick().await.unwrap();
    let read = f.m.get(&f.who, job).await.unwrap();
    assert_eq!(read["state"], "cancelled");
    assert_eq!(balance(&pool, &f.who).await, (0, 0));
    assert_eq!(f.remote.posts.load(Ordering::SeqCst), 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn runtime_can_process_jobs_but_cannot_reconcile_or_rewrite_admission(pool: PgPool) {
    let f = setup(&pool, "CNY").await;
    sqlx::raw_sql(include_str!("../deploy/permissions.sql"))
        .execute(&pool)
        .await
        .unwrap();
    let scoped = sqlx::postgres::PgPoolOptions::new()
        .max_connections(5)
        .after_connect(|c, _| {
            Box::pin(async move {
                sqlx::query("SET ROLE brokerrouter_runtime")
                    .execute(c)
                    .await?;
                Ok(())
            })
        })
        .connect_with(pool.connect_options().as_ref().clone())
        .await
        .unwrap();
    let m = service(&scoped);
    f.remote.mode.store(4, Ordering::SeqCst);
    let job = id(&m.create(&f.who, "role", body(), None).await.unwrap());
    m.tick().await.unwrap();
    assert_eq!(
        m.get(&f.who, job).await.unwrap()["state"],
        "submission_unknown"
    );
    assert!(
        media::attach_task(
            &scoped,
            f.who.tenant_id,
            job,
            "task-123",
            "should require separate operator"
        )
        .await
        .is_err()
    );
    assert!(
        media::reconcile(
            &scoped,
            f.who.tenant_id,
            job,
            &[money::UnitUsage {
                unit: "video_second_720p".into(),
                quantity: Decimal::ZERO
            }],
            "cannot close unknown"
        )
        .await
        .is_err()
    );
    assert_eq!(balance(&pool, &f.who).await, (0, 500000));
    assert!(
        sqlx::query("UPDATE media_jobs SET state='queued' WHERE id=$1")
            .bind(job)
            .execute(&scoped)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("UPDATE media_endpoints SET active=false")
            .execute(&scoped)
            .await
            .is_err()
    );
    attempts::maintain(&scoped).await.unwrap();
    scoped.close().await;
}

#[test]
fn strict_media_protocol_never_guesses_unknown_status_usage_or_task_identity() {
    let r = body();
    assert!(matches!(
        media_protocol::poll(&completed(), "task-123", &r).unwrap(),
        media_protocol::Poll::Complete { .. }
    ));
    for field in [
        "/output/task_id",
        "/usage/video_count",
        "/usage/output_video_duration",
        "/usage/SR",
        "/usage/ratio",
    ] {
        let mut v = completed();
        *v.pointer_mut(field).unwrap() = Value::Null;
        assert!(media_protocol::poll(&v, "task-123", &r).is_err());
    }
    let mut v = completed();
    v["output"]["task_status"] = json!("UNKNOWN");
    assert!(matches!(
        media_protocol::poll(&v, "task-123", &r).unwrap(),
        media_protocol::Poll::Review(_)
    ));
    v = completed();
    v["output"]["video_url"] = json!("http://127.0.0.1/unsafe");
    assert!(media_protocol::poll(&v, "task-123", &r).is_err());
    assert!(!media_protocol::task_id("../steal?key=x"));
}
