//! Additional Phase C PostgreSQL regressions. Included as a child of the four
//! service-seam tests, reusing their actual gateway source and capture fixtures.
//! Synthetic scan reports are contract facts, not real detector certification.
use super::*;
use chrono::{DateTime, Utc};
use sqlx::{PgConnection, postgres::PgPoolOptions};
use std::time::Duration;

async fn apply_permissions(pool: &PgPool) {
    sqlx::raw_sql(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/deploy/permissions.sql"
    )))
    .execute(pool)
    .await
    .unwrap();
}
async fn purpose(pool: &PgPool, role: &'static str) -> PgPool {
    assert!(matches!(
        role,
        "brokerrouter_runtime"
            | "brokerrouter_runtime_assets"
            | "brokerrouter_approvals"
            | "brokerrouter_governance_control"
    ));
    PgPoolOptions::new()
        .max_connections(3)
        .after_connect(move |connection, _| {
            Box::pin(async move {
                sqlx::query(&format!("SET SESSION AUTHORIZATION {role}"))
                    .execute(connection)
                    .await?;
                Ok(())
            })
        })
        .connect_with(pool.connect_options().as_ref().clone())
        .await
        .unwrap()
}
fn sql_code(error: &sqlx::Error) -> &str {
    // PostgreSQL's code Cow is borrowed by PgDatabaseError; callers use this
    // helper only through an owned comparison below to avoid borrowing a Cow.
    if let Some(code) = error.as_database_error().and_then(|e| e.code()) {
        match code.as_ref() {
            "P0001" => "P0001",
            "40001" => "40001",
            "42501" => "42501",
            _ => "other",
        }
    } else {
        "not_database"
    }
}
async fn source(pool: &PgPool, f: &Fixture, p: &Pair) -> (Value, DateTime<Utc>) {
    sqlx::query_as("SELECT files,source_deadline FROM runtime_artifact_source_facts WHERE tenant_id=$1 AND input_operation_id=$2")
        .bind(f.who.tenant_id).bind(p.input).fetch_one(pool).await.unwrap()
}
async fn input(pool: &PgPool, f: &Fixture, key: &str, model: Option<&str>) -> Reserve {
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
            model_id: model.map(str::to_string),
            tool_name: Some("fixed_mp4_producer".into()),
            payload: json!({"duration_seconds":1}),
        },
    )
    .await
    .unwrap();
    let operation = serde_json::from_value(checked["id"].clone()).unwrap();
    let token = "77".repeat(32);
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
        original_payload_hash: "88".repeat(32),
        maximum_bytes: 1024,
        maximum_artifacts: 2,
    }
}
async fn extra_batch(
    pool: &PgPool,
    f: &Fixture,
    runtime: &RuntimeArtifacts,
    key: &str,
    model: Option<&str>,
) -> Pair {
    let r = input(pool, f, key, model).await;
    let saved = runtime.reserve(&f.who, r.clone()).await.unwrap();
    let batch = serde_json::from_value(saved["id"].clone()).unwrap();
    capture(f, runtime, batch, r).await
}
async fn capture(f: &Fixture, runtime: &RuntimeArtifacts, batch: Uuid, r: Reserve) -> Pair {
    let data = vec![Bytes::from_static(
        b"another isolated synthetic native file",
    )];
    let stored = runtime
        .record_manifest(
            &f.who,
            batch,
            Manifest {
                claim_token: r.claim_token.clone(),
                evidence_ref: format!("runtime://coupled/{}/{batch}", r.input_operation_id),
                artifacts: data
                    .iter()
                    .map(|bytes| File {
                        sha256: common::hex(&store::hash(bytes)),
                        byte_length: bytes.len() as i64,
                        media_type: "video/mp4".into(),
                        producer_revision: "44".repeat(32),
                    })
                    .collect(),
            },
        )
        .await
        .unwrap();
    let assets = stored["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| serde_json::from_value(v["id"].clone()).unwrap())
        .collect();
    for (ordinal, bytes) in data.iter().enumerate() {
        runtime
            .put_chunk(
                &f.who,
                batch,
                ordinal as i32,
                0,
                &r.claim_token,
                bytes.clone(),
            )
            .await
            .unwrap();
    }
    runtime
        .complete(&f.who, batch, &r.claim_token)
        .await
        .unwrap();
    Pair {
        runtime: runtime.clone(),
        batch,
        input: r.input_operation_id,
        token: r.claim_token,
        assets,
        data,
    }
}

// Deliberately direct purpose SQL: correct source fact INSERTs must work, while
// even a caller with table INSERT cannot choose a subset or alternate hash.
async fn direct_tool(
    connection: &mut PgConnection,
    f: &Fixture,
    p: &Pair,
    set: Option<&Value>,
    hash: Option<&[u8]>,
    expiry: DateTime<Utc>,
) -> Result<Uuid, sqlx::Error> {
    let id = Uuid::new_v4();
    let text = p.text();
    let ciphertext = common::cipher()
        .encrypt(f.who.tenant_id, id, text.text.as_bytes())
        .unwrap();
    sqlx::query_scalar("INSERT INTO runtime_tool_results(tenant_id,id,key_id,turn_id,input_operation_id,model_id,tool_name,claim_hash,receipt_hash,evidence_ref,text_sha256,bytes,result_ciphertext,created_at,expires_at,artifact_set,artifact_set_hash) SELECT o.tenant_id,$3,o.key_id,o.turn_id,o.id,o.model_id,o.tool_name,o.claim_hash,o.receipt_hash,$4,$5,$6,$7,clock_timestamp(),$8,$9,$10 FROM governance_operations o WHERE o.tenant_id=$1 AND o.id=$2 RETURNING id")
        .bind(f.who.tenant_id).bind(p.input).bind(id).bind(text.evidence_ref)
        .bind(store::hash(text.text.as_bytes())).bind(text.text.len() as i32).bind(ciphertext)
        .bind(expiry).bind(set).bind(hash).fetch_one(connection).await
}

#[sqlx::test(migrations = "./migrations")]
async fn fixed_flat_set_digest_matches_rust_and_actual_complete_source_order(pool: PgPool) {
    let f = setup(&pool).await;
    let p = pair(&pool, &f, true).await;
    let (set, _) = source(&pool, &f, &p).await;
    assert_eq!(set.as_array().unwrap().len(), 2);
    let sql_hash: Vec<u8> = sqlx::query_scalar("SELECT runtime_artifact_set_digest($1)")
        .bind(&set)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(sql_hash, governance::digest(&set).unwrap());
    for (ordinal, file) in set.as_array().unwrap().iter().enumerate() {
        assert_eq!(file.as_object().unwrap().len(), 9);
        assert_eq!(file["ordinal"], ordinal as i32);
        assert_eq!(file["batch_id"], p.batch.to_string());
        assert_eq!(file["input_operation_id"], p.input.to_string());
        let timestamp = file["expires_at"].as_str().unwrap();
        assert!(timestamp.ends_with('Z'));
        assert_eq!(timestamp.split('.').nth(1).unwrap().len(), 7);
    }
    let mut maximum = set[0].clone();
    maximum["byte_length"] = json!(67_108_864_i64);
    maximum["expires_at"] = json!("2026-10-11T08:12:13.000000Z");
    let dense = Value::Array(
        (0..64)
            .map(|n| {
                let mut file = maximum.clone();
                file["ordinal"] = json!(n % 8);
                file["asset_id"] = json!(Uuid::from_u128(n as u128 + 1));
                file["input_operation_id"] = json!(Uuid::from_u128((n / 8) as u128 + 100));
                file
            })
            .collect(),
    );
    let sql_hash: Vec<u8> = sqlx::query_scalar("SELECT runtime_artifact_set_digest($1)")
        .bind(&dense)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(sql_hash, governance::digest(&dense).unwrap());
    let mut float = set.clone();
    float[0]["ordinal"] = serde_json::from_str("0.0").unwrap();
    assert_ne!(
        governance::digest(&float).unwrap(),
        governance::digest(&set).unwrap()
    );
    let mut offset = set.clone();
    let original = offset[0]["expires_at"].as_str().unwrap().to_string();
    offset[0]["expires_at"] = json!(original.replace('Z', "+00:00"));
    assert_ne!(
        governance::digest(&offset).unwrap(),
        governance::digest(&set).unwrap()
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn runtime_direct_insert_cannot_choose_subset_reordered_or_noncanonical_set(pool: PgPool) {
    let f = setup(&pool).await;
    let p = pair(&pool, &f, true).await;
    let (set, expiry) = source(&pool, &f, &p).await;
    apply_permissions(&pool).await;
    let runtime = purpose(&pool, "brokerrouter_runtime").await;
    let mut variants = vec![json!([set[0]]), json!([set[1], set[0]])];
    let mut float = set.clone();
    float[0]["ordinal"] = serde_json::from_str("0.0").unwrap();
    variants.push(float);
    let mut extra = set.clone();
    extra[0]["chosen_by_caller"] = json!(true);
    variants.push(extra);
    let mut alternate = set.clone();
    alternate[0]["asset_id"] = json!(set[0]["asset_id"].as_str().unwrap().replace('-', ""));
    variants.push(alternate);
    for candidate in variants {
        let hash = governance::digest(&candidate).unwrap();
        let mut connection = runtime.acquire().await.unwrap();
        let error = direct_tool(
            &mut connection,
            &f,
            &p,
            Some(&candidate),
            Some(&hash),
            expiry,
        )
        .await
        .unwrap_err();
        assert_eq!(sql_code(&error), "P0001");
        assert!(
            error
                .as_database_error()
                .unwrap()
                .message()
                .contains("entire original artifact batch")
        );
    }
    let mut connection = runtime.acquire().await.unwrap();
    let bad = vec![0_u8; 32];
    assert_eq!(
        sql_code(
            &direct_tool(&mut connection, &f, &p, Some(&set), Some(&bad), expiry)
                .await
                .unwrap_err()
        ),
        "P0001"
    );
    assert_eq!(
        sql_code(
            &direct_tool(&mut connection, &f, &p, None, None, expiry)
                .await
                .unwrap_err()
        ),
        "P0001"
    );
    let hash = governance::digest(&set).unwrap();
    let saved = direct_tool(&mut connection, &f, &p, Some(&set), Some(&hash), expiry)
        .await
        .unwrap();
    let frozen:(Value,Vec<u8>)=sqlx::query_as("SELECT artifact_set,artifact_set_hash FROM runtime_tool_results WHERE tenant_id=$1 AND id=$2")
        .bind(f.who.tenant_id).bind(saved).fetch_one(&runtime).await.unwrap();
    assert_eq!(frozen, (set, hash));
    assert_eq!(f.calls.load(Ordering::SeqCst), 1);
    drop(connection);
    runtime.close().await;
}

#[sqlx::test(migrations = "./migrations")]
async fn whole_turn_derivation_does_not_hide_different_or_null_model_batches(pool: PgPool) {
    for model in [Some("other-model"), None] {
        let f = setup(&pool).await;
        let p = pair(&pool, &f, true).await;
        runtime_results::record(&pool, &common::cipher(), &f.who, p.text())
            .await
            .unwrap();
        store::put_model(&pool, f.who.tenant_id, "other-model", "Other model")
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO key_models(tenant_id,key_id,model_id) VALUES($1,$2,'other-model')",
        )
        .bind(f.who.tenant_id)
        .bind(f.who.key_id)
        .execute(&pool)
        .await
        .unwrap();
        let second = extra_batch(&pool, &f, &p.runtime, "other-model-original", model).await;
        runtime_results::record(&pool, &common::cipher(), &f.who, second.text())
            .await
            .unwrap();
        let error = runtime_publications::record(&pool, &common::cipher(), &f.who, f.record())
            .await
            .unwrap_err();
        assert!(
            matches!(error, Error::Conflict),
            "all original batches must be compared before model filtering"
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM runtime_final_publications WHERE tenant_id=$1"
            )
            .bind(f.who.tenant_id)
            .fetch_one(&pool)
            .await
            .unwrap(),
            0
        );
    }
}

async fn direct_batch(
    connection: &mut PgConnection,
    tenant: Uuid,
    input: Uuid,
    lifetime: i32,
    backdate: i32,
) -> Result<Uuid, sqlx::Error> {
    sqlx::query_scalar("INSERT INTO runtime_artifact_batches(tenant_id,id,key_id,turn_id,input_operation_id,model_id,tool_name,claim_hash,input_payload_hash,original_payload_hash,maximum_bytes,maximum_artifacts,reserved_bytes,created_at,upload_expires_at,expires_at) SELECT o.tenant_id,$3,o.key_id,o.turn_id,o.id,o.model_id,o.tool_name,o.claim_hash,o.payload_hash,decode(repeat('88',32),'hex'),1024,2,1024,s.ts-make_interval(secs=>$5),s.ts+make_interval(secs=>$4),s.ts+make_interval(secs=>$4) FROM governance_operations o CROSS JOIN (SELECT clock_timestamp() ts) s WHERE o.tenant_id=$1 AND o.id=$2 RETURNING id")
        .bind(tenant).bind(input).bind(Uuid::new_v4()).bind(lifetime).bind(backdate).fetch_one(connection).await
}

#[sqlx::test(migrations = "./migrations")]
async fn wrong_key_batch_in_actual_turn_is_visible_and_cannot_be_omitted(pool: PgPool) {
    let f = setup(&pool).await;
    let (other, _) = store::create_key(
        &pool,
        f.who.tenant_id,
        "other-runtime",
        1_000_000,
        &["model".into()],
        None,
    )
    .await
    .unwrap();
    let op = Uuid::new_v4();
    sqlx::query("INSERT INTO governance_operations(tenant_id,id,key_id,turn_id,operation_key,stage,model_id,tool_name,payload_hash,state,claim_hash,execution_deadline) VALUES($1,$2,$3,$4,'wrong-key-source','tool_input','model','fixed_mp4_producer',decode(repeat('88',32),'hex'),'executing',decode(repeat('77',32),'hex'),clock_timestamp()+interval '5 minutes')")
        .bind(f.who.tenant_id).bind(op).bind(other).bind(f.turn).execute(&pool).await.unwrap();
    let mut connection = pool.acquire().await.unwrap();
    direct_batch(&mut connection, f.who.tenant_id, op, 600, 0)
        .await
        .unwrap();
    let facts:(Uuid,bool)=sqlx::query_as("SELECT key_id,source_matches FROM runtime_artifact_source_facts WHERE tenant_id=$1 AND turn_id=$2")
        .bind(f.who.tenant_id).bind(f.turn).fetch_one(&pool).await.unwrap();
    assert_eq!(facts, (other, false));
    assert!(
        runtime_publications::record(&pool, &common::cipher(), &f.who, f.record())
            .await
            .is_err()
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM runtime_final_publications")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn final_freeze_rejects_later_reservation_tool_result_and_new_input_http(pool: PgPool) {
    let f = setup(&pool).await;
    let r = input(&pool, &f, "before-final-reserve", Some("model")).await;
    let late = input(&pool, &f, "known-effect-before-final", Some("model")).await;
    let ready = governance::check(
        &pool,
        &common::cipher(),
        &f.who,
        governance::Check {
            turn_id: f.turn,
            operation_key: "ready-but-never-executed-before-final".into(),
            stage: "tool_input".into(),
            model_id: Some("model".into()),
            tool_name: Some("fixed_mp4_producer".into()),
            payload: json!({}),
        },
    )
    .await
    .unwrap();
    let ready_input: Uuid = serde_json::from_value(ready["id"].clone()).unwrap();
    let text = runtime_results::Record {
        input_operation_id: late.input_operation_id,
        claim_token: late.claim_token.clone(),
        evidence_ref: "runtime://known-effect-before-final".into(),
        text: "late private text".into(),
    };
    governance::receipt(
        &pool,
        &common::cipher(),
        &f.who,
        late.input_operation_id,
        governance::Receipt {
            token: late.claim_token,
            outcome: "completed".into(),
            evidence_ref: text.evidence_ref.clone(),
        },
    )
    .await
    .unwrap();
    let saved = runtime_publications::record(&pool, &common::cipher(), &f.who, f.record())
        .await
        .unwrap();
    assert!(saved.get("artifact_set").is_none());
    let runtime = RuntimeArtifacts::new(
        Assets::new(pool.clone(), Arc::new(InMemory::new()), common::cipher(), 2).unwrap(),
        common::cipher(),
    );
    assert!(runtime.reserve(&f.who, r).await.is_err());
    assert!(
        runtime_results::record(&pool, &common::cipher(), &f.who, text)
            .await
            .is_err()
    );
    assert!(
        governance::claim(
            &pool,
            &common::cipher(),
            &f.who,
            ready_input,
            governance::Claim {
                token: "99".repeat(32),
                payload_hash: ready["payload_hash"].as_str().unwrap().into(),
            }
        )
        .await
        .is_err(),
        "an old Ready input cannot acquire a new execution after final freeze"
    );
    let unexecuted: (String, bool) = sqlx::query_as(
        "SELECT state,claim_hash IS NULL FROM governance_operations WHERE tenant_id=$1 AND id=$2",
    )
    .bind(f.who.tenant_id)
    .bind(ready_input)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(unexecuted, ("ready".into(), true));
    let app = api::gateway_router(f.gateway.clone());
    let (status, body) = super::super::http(
        &app,
        &f.token,
        "POST",
        "/v1/governance/checks",
        json!({"turn_id":f.turn,
        "operation_key":"after-final-new-effect","stage":"tool_input","model_id":"model",
        "tool_name":"fixed_mp4_producer","payload":{"must_not_execute":true}}),
    )
    .await;
    assert!(
        matches!(status, StatusCode::CONFLICT | StatusCode::GONE),
        "new input after immutable final must fail before a caller can execute"
    );
    assert!(body.get("text").is_none());
    let counts:(i64,i64)=sqlx::query_as("SELECT (SELECT count(*) FROM runtime_artifact_batches),(SELECT count(*) FROM runtime_tool_results)")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(counts, (0, 0));
    assert_eq!(f.calls.load(Ordering::SeqCst), 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn whole_source_deadline_under_sixty_seconds_is_immutable_and_expiry_retains_facts(
    pool: PgPool,
) {
    let f = setup(&pool).await;
    let r = input(&pool, &f, "short-original-source", Some("model")).await;
    let runtime = RuntimeArtifacts::new(
        Assets::new(pool.clone(), Arc::new(InMemory::new()), common::cipher(), 2).unwrap(),
        common::cipher(),
    );
    let mut connection = pool.acquire().await.unwrap();
    // Backdate only this fixture's new reservation INSERT, never mutate an
    // existing identity or override the production API's server-owned TTL.
    let batch = direct_batch(
        &mut connection,
        f.who.tenant_id,
        r.input_operation_id,
        4,
        120,
    )
    .await
    .unwrap();
    for sql in [
        "UPDATE tenants SET asset_used_bytes=asset_used_bytes+1024 WHERE id=$1",
        "UPDATE virtual_keys SET asset_used_bytes=asset_used_bytes+1024 WHERE tenant_id=$1",
    ] {
        sqlx::query(sql)
            .bind(f.who.tenant_id)
            .execute(&pool)
            .await
            .unwrap();
    }
    let p = capture(&f, &runtime, batch, r).await;
    let saved = runtime_results::record(&pool, &common::cipher(), &f.who, p.text())
        .await
        .unwrap();
    let result = serde_json::from_value(saved["id"].clone()).unwrap();
    let facts:(DateTime<Utc>,DateTime<Utc>,Value,Vec<u8>)=sqlx::query_as("SELECT created_at,expires_at,artifact_set,artifact_set_hash FROM runtime_tool_results WHERE tenant_id=$1 AND id=$2")
        .bind(f.who.tenant_id).bind(result).fetch_one(&pool).await.unwrap();
    assert!(facts.1 > facts.0 && facts.1 - facts.0 < chrono::Duration::seconds(60));
    assert_eq!(
        saved,
        runtime_results::record(&pool, &common::cipher(), &f.who, p.text())
            .await
            .unwrap()
    );
    for sql in [
        "UPDATE runtime_tool_results SET expires_at=expires_at+interval '1 second' WHERE id=$1",
        "UPDATE runtime_tool_results SET artifact_set_hash=decode(repeat('00',32),'hex') WHERE id=$1",
        "UPDATE runtime_tool_results SET artifact_set='[]' WHERE id=$1",
        "UPDATE runtime_tool_results SET result_ciphertext=NULL WHERE id=$1",
    ] {
        assert!(sqlx::query(sql).bind(result).execute(&pool).await.is_err());
    }
    sqlx::query(
        "SELECT pg_sleep(GREATEST(0,extract(epoch FROM $1::timestamptz-clock_timestamp()))+0.02)",
    )
    .bind(facts.1)
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(runtime_results::maintain(&pool).await.unwrap(), 1);
    let retained:(bool,Value,Vec<u8>,i64)=sqlx::query_as("SELECT result_ciphertext IS NULL,artifact_set,artifact_set_hash,(SELECT count(*) FROM runtime_artifacts WHERE tenant_id=$1) FROM runtime_tool_results WHERE tenant_id=$1 AND id=$2")
        .bind(f.who.tenant_id).bind(result).fetch_one(&pool).await.unwrap();
    assert_eq!(retained, (true, facts.2, facts.3, 1));
    assert!(
        runtime_results::release(&f.gateway, &f.who, result)
            .await
            .is_err()
    );
}

async fn direct_final(
    connection: &mut PgConnection,
    tenant: Uuid,
    key: Uuid,
    turn: Uuid,
    request: Uuid,
) -> Result<Uuid, sqlx::Error> {
    let id = Uuid::new_v4();
    let text = b"direct final transaction fixture";
    let ciphertext = common::cipher().encrypt(tenant, id, text).unwrap();
    sqlx::query_scalar("INSERT INTO runtime_final_publications(tenant_id,id,key_id,turn_id,model_id,source_requests,text_sha256,bytes,result_ciphertext,created_at,expires_at,artifact_set,artifact_set_hash) SELECT r.tenant_id,$5,r.key_id,r.governance_turn_id,r.model_id,jsonb_build_array(jsonb_build_object('id',r.id::text,'request_hash',encode(r.request_hash,'hex'),'ciphertext_sha256',encode(sha256(r.result_ciphertext),'hex'),'expires_at',r.result_expires_at)),$6,$7,$8,clock_timestamp(),LEAST(r.result_expires_at,t.expires_at),NULL,NULL FROM requests r JOIN governance_turns t ON t.tenant_id=r.tenant_id AND t.id=r.governance_turn_id WHERE r.tenant_id=$1 AND r.key_id=$2 AND r.governance_turn_id=$3 AND r.id=$4 RETURNING id")
        .bind(tenant).bind(key).bind(turn).bind(request).bind(id).bind(store::hash(text))
        .bind(text.len() as i32).bind(ciphertext).fetch_one(connection).await
}
async fn wait_for_tenant_lock(pool: &PgPool, pid: i32) {
    let end = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let waiting:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE pid=$1 AND wait_event_type='Lock' AND state='active')")
            .bind(pid).fetch_one(pool).await.unwrap();
        if waiting {
            return;
        }
        assert!(
            tokio::time::Instant::now() < end,
            "direct INSERT did not reach the actual tenant fence"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn insertion_race(pool: &PgPool, isolation: &str, final_first: bool) {
    let f = setup(pool).await;
    let r = input(pool, &f, "competing-reservation", Some("model")).await;
    apply_permissions(pool).await;
    let runtime = purpose(pool, "brokerrouter_runtime").await;
    let storage = purpose(pool, "brokerrouter_runtime_assets").await;
    let (first_pool, second_pool) = if final_first {
        (&runtime, &storage)
    } else {
        (&storage, &runtime)
    };
    let mut first = first_pool.begin().await.unwrap();
    if final_first {
        direct_final(&mut first, f.who.tenant_id, f.who.key_id, f.turn, f.request)
            .await
            .unwrap();
    } else {
        direct_batch(&mut first, f.who.tenant_id, r.input_operation_id, 600, 0)
            .await
            .unwrap();
    }
    let mut second = second_pool.begin().await.unwrap();
    assert!(matches!(isolation, "READ COMMITTED" | "REPEATABLE READ"));
    sqlx::query(&format!("SET TRANSACTION ISOLATION LEVEL {isolation}"))
        .execute(&mut *second)
        .await
        .unwrap();
    sqlx::query("SET LOCAL statement_timeout='6s'")
        .execute(&mut *second)
        .await
        .unwrap();
    // Establish a real old snapshot before the first tuple UPDATE commits.
    let seen: i64 = sqlx::query_scalar("SELECT count(*) FROM tenants WHERE id=$1")
        .bind(f.who.tenant_id)
        .fetch_one(&mut *second)
        .await
        .unwrap();
    assert_eq!(seen, 1);
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *second)
        .await
        .unwrap();
    let tenant = f.who.tenant_id;
    let key = f.who.key_id;
    let turn = f.turn;
    let request = f.request;
    let task = tokio::spawn(async move {
        let result = if final_first {
            direct_batch(&mut second, tenant, r.input_operation_id, 600, 0).await
        } else {
            direct_final(&mut second, tenant, key, turn, request).await
        };
        second.rollback().await.unwrap();
        result
    });
    wait_for_tenant_lock(pool, pid).await;
    first.commit().await.unwrap();
    let error = tokio::time::timeout(Duration::from_secs(7), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert_eq!(
        sql_code(&error),
        if isolation == "REPEATABLE READ" {
            "40001"
        } else {
            "P0001"
        }
    );
    let counts:(i64,i64)=sqlx::query_as("SELECT (SELECT count(*) FROM runtime_final_publications WHERE tenant_id=$1),(SELECT count(*) FROM runtime_artifact_batches WHERE tenant_id=$1)")
        .bind(tenant).fetch_one(pool).await.unwrap();
    assert_eq!(counts, if final_first { (1, 0) } else { (0, 1) });
    runtime.close().await;
    storage.close().await;
}

#[sqlx::test(migrations = "./migrations")]
async fn direct_rc_final_insert_fences_concurrent_original_batch(pool: PgPool) {
    insertion_race(&pool, "READ COMMITTED", true).await;
}
#[sqlx::test(migrations = "./migrations")]
async fn direct_rr_old_snapshot_cannot_append_batch_after_final_freeze(pool: PgPool) {
    insertion_race(&pool, "REPEATABLE READ", true).await;
}
#[sqlx::test(migrations = "./migrations")]
async fn direct_rc_original_batch_insert_cannot_be_omitted_by_concurrent_final(pool: PgPool) {
    insertion_race(&pool, "READ COMMITTED", false).await;
}
#[sqlx::test(migrations = "./migrations")]
async fn direct_rr_old_snapshot_cannot_freeze_final_omitting_concurrent_batch(pool: PgPool) {
    insertion_race(&pool, "REPEATABLE READ", false).await;
}

#[sqlx::test(migrations = "./migrations")]
async fn actual_storage_purpose_can_reserve_through_the_whole_set_tenant_fence(pool: PgPool) {
    let f = setup(&pool).await;
    let r = input(&pool, &f, "storage-purpose-fence", Some("model")).await;
    apply_permissions(&pool).await;
    let scoped = purpose(&pool, "brokerrouter_runtime_assets").await;
    let runtime = RuntimeArtifacts::new(
        Assets::new(
            scoped.clone(),
            Arc::new(InMemory::new()),
            common::cipher(),
            2,
        )
        .unwrap(),
        common::cipher(),
    );
    // This is the isolated storage identity, not a privileged fixture calling
    // the new trigger. Missing SELECT(governance_lock) must remain a real RED.
    let saved = runtime.reserve(&f.who, r).await.unwrap();
    assert_eq!(saved["state"], "reserved");
    assert!(saved.get("text").is_none());
    let identity: String = sqlx::query_scalar("SELECT session_user::text")
        .fetch_one(&scoped)
        .await
        .unwrap();
    assert_eq!(identity, "brokerrouter_runtime_assets");
    drop(runtime);
    scoped.close().await;
}

#[sqlx::test(migrations = "./migrations")]
async fn actual_purpose_metadata_is_readonly_and_contains_no_runtime_cipher_or_body(pool: PgPool) {
    let f = setup(&pool).await;
    let p = pair(&pool, &f, true).await;
    let saved = runtime_results::record(&pool, &common::cipher(), &f.who, p.text())
        .await
        .unwrap();
    apply_permissions(&pool).await;
    for role in [
        "brokerrouter_runtime",
        "brokerrouter_approvals",
        "brokerrouter_governance_control",
    ] {
        let scoped = purpose(&pool, role).await;
        let visible:Value=sqlx::query_scalar("SELECT to_jsonb(f) FROM runtime_tool_result_review_facts f WHERE tenant_id=$1 AND input_operation_id=$2")
            .bind(f.who.tenant_id).bind(p.input).fetch_one(&scoped).await.unwrap();
        assert_eq!(visible["id"], saved["id"]);
        assert_eq!(visible["artifact_set"].as_array().unwrap().len(), 2);
        assert!(visible.get("result_ciphertext").is_none() && visible.get("text").is_none());
        assert!(
            !serde_json::to_string(&visible)
                .unwrap()
                .contains(&p.text().text)
        );
        sqlx::query("SELECT files,source_matches,files_match,complete,available FROM runtime_artifact_source_facts WHERE tenant_id=$1")
            .bind(f.who.tenant_id).fetch_all(&scoped).await.unwrap();
        for statement in [
            "UPDATE runtime_tool_result_review_facts SET evidence_ref='forged'",
            "UPDATE runtime_artifact_source_facts SET files='[]'",
        ] {
            assert!(
                sqlx::query(statement).execute(&scoped).await.is_err(),
                "{role}: {statement}"
            );
        }
        if role != "brokerrouter_runtime" {
            for statement in [
                "SELECT result_ciphertext FROM runtime_tool_results",
                "SELECT result_ciphertext FROM runtime_final_publications",
                "INSERT INTO guardrail_evaluations SELECT * FROM guardrail_evaluations WHERE false",
                "UPDATE guardrail_evaluations SET state=state",
            ] {
                assert_eq!(
                    sql_code(&sqlx::query(statement).execute(&scoped).await.unwrap_err()),
                    "42501",
                    "{role}: {statement}"
                );
            }
        } else {
            let raw: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM assets WHERE tenant_id=$1 AND kind='runtime_output'",
            )
            .bind(f.who.tenant_id)
            .fetch_one(&scoped)
            .await
            .unwrap();
            assert_eq!(raw, 0, "runtime must not gain raw native asset reads");
            let facts:i64=sqlx::query_scalar("SELECT count(*) FROM runtime_artifact_asset_facts WHERE tenant_id=$1 AND kind='runtime_output'")
                .bind(f.who.tenant_id).fetch_one(&scoped).await.unwrap();
            assert_eq!(
                facts, 2,
                "current file authority must read only the narrow asset facts view"
            );
            for statement in [
                "SELECT * FROM asset_chunks",
                "INSERT INTO runtime_artifact_grants SELECT * FROM runtime_artifact_grants WHERE false",
                "UPDATE runtime_tool_results SET artifact_set='[]'",
                "UPDATE runtime_tool_results SET artifact_set_hash=decode(repeat('00',32),'hex')",
            ] {
                assert_eq!(
                    sql_code(&sqlx::query(statement).execute(&scoped).await.unwrap_err()),
                    "42501",
                    "{statement}"
                );
            }
        }
        scoped.close().await;
    }
    let storage = purpose(&pool, "brokerrouter_runtime_assets").await;
    let freezes: i64 =
        sqlx::query_scalar("SELECT count(*) FROM runtime_output_set_freezes WHERE tenant_id=$1")
            .bind(f.who.tenant_id)
            .fetch_one(&storage)
            .await
            .unwrap();
    assert_eq!(freezes, 1);
    for statement in [
        "SELECT result_ciphertext FROM runtime_tool_results",
        "SELECT result_ciphertext FROM runtime_final_publications",
    ] {
        assert_eq!(
            sql_code(&sqlx::query(statement).execute(&storage).await.unwrap_err()),
            "42501"
        );
    }
    storage.close().await;
}

mod late_authorization;

mod typed_body;
