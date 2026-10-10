//! The server-derived whole set also fixes the first trusted producer's body.
//! PostgreSQL capture/scan fixtures here do not attest a producer or codec.
use super::*;

const FIXED_TOOL: &str = "brokerrouter_fixed_mp4";

fn fixed_body(count: usize) -> String {
    format!("{{\"artifact_count\":{count},\"contract\":\"brokerrouter-fixed-mp4-result-v1\"}}")
}

// Create the fixed tool identity before claim/capture. Never UPDATE the identity
// of a sealed batch or an existing runtime result merely to satisfy a fixture.
async fn fixed_input(pool: &PgPool, f: &Fixture, key: &str) -> Reserve {
    brokerrouter::assets::set_quota(pool, f.who.tenant_id, None, 64 * 1024 * 1024)
        .await
        .unwrap();
    brokerrouter::assets::set_quota(pool, f.who.tenant_id, Some(f.who.key_id), 64 * 1024 * 1024)
        .await
        .unwrap();
    let checked = governance::check(
        pool,
        &common::cipher(),
        &f.who,
        governance::Check {
            turn_id: f.turn,
            operation_key: key.into(),
            stage: "tool_input".into(),
            model_id: Some("model".into()),
            tool_name: Some(FIXED_TOOL.into()),
            payload: json!({"width":16,"height":16,"seconds":1,"fps":1,"clips":1,"color":0}),
        },
    )
    .await
    .unwrap();
    let operation = id(&checked);
    let token = "99".repeat(32);
    governance::claim(
        pool,
        &common::cipher(),
        &f.who,
        operation,
        governance::Claim {
            token: token.clone(),
            payload_hash: checked["payload_hash"].as_str().unwrap().into(),
        },
    )
    .await
    .unwrap();
    Reserve {
        input_operation_id: operation,
        claim_token: token,
        input_payload_hash: checked["payload_hash"].as_str().unwrap().into(),
        original_payload_hash: "aa".repeat(32),
        maximum_bytes: 1024,
        maximum_artifacts: 1,
    }
}

async fn fixed_pair(pool: &PgPool, f: &Fixture) -> Pair {
    let request = fixed_input(pool, f, "fixed-producer-original-source").await;
    let runtime = RuntimeArtifacts::new(
        Assets::new(pool.clone(), Arc::new(InMemory::new()), common::cipher(), 2).unwrap(),
        common::cipher(),
    );
    let reserved = runtime.reserve(&f.who, request.clone()).await.unwrap();
    capture(f, &runtime, id(&reserved), request).await
}

fn fixed_record(p: &Pair) -> runtime_results::Record {
    runtime_results::Record {
        text: fixed_body(p.assets.len()),
        ..p.text()
    }
}

async fn result_count(pool: &PgPool, f: &Fixture) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM runtime_tool_results WHERE tenant_id=$1")
        .bind(f.who.tenant_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[sqlx::test(migrations = "./migrations")]
async fn fixed_tool_http_rejects_arbitrary_body_and_wrong_count_before_first_record(pool: PgPool) {
    let f = setup(&pool).await;
    let p = fixed_pair(&pool, &f).await;
    let (files, _) = source(&pool, &f, &p).await;
    let app = api::gateway_router(f.gateway.clone());
    for body in ["untrusted Agent body".into(), fixed_body(2)] {
        let request = runtime_results::Record {
            text: body,
            ..p.text()
        };
        let (status, rejected) = http(
            &app,
            &f.token,
            "POST",
            "/v1/governance/tool-results",
            json!(request),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(rejected["error"]["code"], "invalid_request");
        assert_eq!(result_count(&pool, &f).await, 0);
    }
    let (status, recorded) = http(
        &app,
        &f.token,
        "POST",
        "/v1/governance/tool-results",
        json!(fixed_record(&p)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(recorded["state"], "recorded");
    assert_eq!(recorded["artifact_set"]["files"], files);
    assert_eq!(
        recorded["text_sha256"],
        common::hex(&store::hash(fixed_body(1).as_bytes()))
    );
    assert_eq!(recorded["bytes"], fixed_body(1).len());
    assert!(recorded.get("text").is_none());
    let (_, retry) = http(
        &app,
        &f.token,
        "POST",
        "/v1/governance/tool-results",
        json!(fixed_record(&p)),
    )
    .await;
    assert_eq!(retry, recorded);
    assert_eq!(result_count(&pool, &f).await, 1);
    let state: String =
        sqlx::query_scalar("SELECT state FROM governance_operations WHERE tenant_id=$1 AND id=$2")
            .bind(f.who.tenant_id)
            .bind(p.input)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(state, "completed");
    assert_eq!(f.calls.load(Ordering::SeqCst), 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn fixed_tool_http_requires_nonempty_original_set_and_rolls_back_receipt_on_denial(
    pool: PgPool,
) {
    let f = setup(&pool).await;
    let r = fixed_input(&pool, &f, "fixed-producer-without-source").await;
    let app = api::gateway_router(f.gateway.clone());
    let (status, rejected) = http(
        &app,
        &f.token,
        "POST",
        "/v1/governance/tool-results",
        json!(runtime_results::Record {
            input_operation_id: r.input_operation_id,
            claim_token: r.claim_token,
            evidence_ref: "runtime://no-original-artifact-set".into(),
            text: fixed_body(0),
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(rejected["error"]["code"], "invalid_request");
    assert_eq!(result_count(&pool, &f).await, 0);
    let facts: (String, Option<Vec<u8>>, i64) = sqlx::query_as("SELECT state,receipt_hash,(SELECT count(*) FROM runtime_artifact_batches WHERE tenant_id=$1) FROM governance_operations WHERE tenant_id=$1 AND id=$2")
        .bind(f.who.tenant_id).bind(r.input_operation_id).fetch_one(&pool).await.unwrap();
    assert_eq!(facts, ("executing".into(), None, 0));
    assert_eq!(f.calls.load(Ordering::SeqCst), 1);
}

// Purpose-level SQL bypasses the Rust body validator. The immutable insertion
// trigger must independently verify both template digest and exact byte count.
async fn direct_fixed(
    connection: &mut PgConnection,
    f: &Fixture,
    p: &Pair,
    files: &Value,
    expiry: DateTime<Utc>,
    text_fact: (&[u8], i32),
) -> Result<Uuid, sqlx::Error> {
    let result = Uuid::new_v4();
    let request = fixed_record(p);
    let ciphertext = common::cipher()
        .encrypt(f.who.tenant_id, result, request.text.as_bytes())
        .unwrap();
    let set_hash = governance::digest(files).unwrap();
    sqlx::query_scalar("INSERT INTO runtime_tool_results(tenant_id,id,key_id,turn_id,input_operation_id,model_id,tool_name,claim_hash,receipt_hash,evidence_ref,text_sha256,bytes,result_ciphertext,created_at,expires_at,artifact_set,artifact_set_hash) SELECT o.tenant_id,$3,o.key_id,o.turn_id,o.id,o.model_id,o.tool_name,o.claim_hash,o.receipt_hash,$4,$5,$6,$7,clock_timestamp(),$8,$9,$10 FROM governance_operations o WHERE o.tenant_id=$1 AND o.id=$2 RETURNING id")
        .bind(f.who.tenant_id).bind(p.input).bind(result).bind(request.evidence_ref)
        .bind(text_fact.0).bind(text_fact.1).bind(ciphertext).bind(expiry).bind(files).bind(set_hash)
        .fetch_one(connection).await
}

#[sqlx::test(migrations = "./migrations")]
async fn fixed_tool_runtime_sql_rejects_wrong_template_hash_and_length(pool: PgPool) {
    let f = setup(&pool).await;
    let p = fixed_pair(&pool, &f).await;
    let (files, expiry) = source(&pool, &f, &p).await;
    apply_permissions(&pool).await;
    let runtime = purpose(&pool, "brokerrouter_runtime").await;
    let correct = store::hash(fixed_body(1).as_bytes());
    let wrong = store::hash(b"untrusted Agent body");
    for (hash, bytes) in [
        (&wrong, fixed_body(1).len() as i32),
        (&correct, fixed_body(1).len() as i32 + 1),
    ] {
        let mut connection = runtime.acquire().await.unwrap();
        let error = direct_fixed(&mut connection, &f, &p, &files, expiry, (hash, bytes))
            .await
            .unwrap_err();
        assert_eq!(sql_code(&error), "P0001");
        assert_eq!(result_count(&pool, &f).await, 0);
    }
    let mut connection = runtime.acquire().await.unwrap();
    let recorded = direct_fixed(
        &mut connection,
        &f,
        &p,
        &files,
        expiry,
        (&correct, fixed_body(1).len() as i32),
    )
    .await
    .unwrap();
    let facts: (Vec<u8>, i32, Value, Vec<u8>) = sqlx::query_as("SELECT text_sha256,bytes,artifact_set,result_ciphertext FROM runtime_tool_results WHERE tenant_id=$1 AND id=$2")
        .bind(f.who.tenant_id).bind(recorded).fetch_one(&runtime).await.unwrap();
    assert_eq!(facts.0, correct);
    assert_eq!(facts.1 as usize, fixed_body(1).len());
    assert_eq!(facts.2, files);
    assert_eq!(
        common::cipher()
            .decrypt(f.who.tenant_id, recorded, &facts.3)
            .unwrap(),
        fixed_body(1).as_bytes()
    );
    assert_eq!(result_count(&pool, &f).await, 1);
    assert_eq!(f.calls.load(Ordering::SeqCst), 1);
    drop(connection);
    runtime.close().await;
}

#[sqlx::test(migrations = "./migrations")]
async fn legacy_tool_body_is_unchanged_by_fixed_producer_template_binding(pool: PgPool) {
    let f = setup(&pool).await;
    let p = super::super::pair(&pool, &f, true).await;
    let delivery = super::super::scan_pair(&pool, &f, &p).await;
    let saved = runtime_results::record(&pool, &common::cipher(), &f.who, p.text())
        .await
        .unwrap();
    for ordinal in 0..2 {
        super::super::approve_file(&pool, &f, &p, &delivery, ordinal).await;
    }
    let released = runtime_results::release(&f.gateway, &f.who, id(&saved))
        .await
        .unwrap();
    assert_eq!(released["state"], "ready");
    assert_eq!(released["text"], p.text().text);
    let second = extra_batch(
        &pool,
        &f,
        &p.runtime,
        "second-legacy-producer",
        Some("model"),
    )
    .await;
    let other = runtime_results::record(&pool, &common::cipher(), &f.who, second.text())
        .await
        .unwrap();
    assert_eq!(other["bytes"], second.text().text.len());
    assert_eq!(result_count(&pool, &f).await, 2);
    assert_eq!(f.calls.load(Ordering::SeqCst), 1);
}
