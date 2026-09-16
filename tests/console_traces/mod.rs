use super::monitoring::{runtime, sample};
use super::*;
use brokerrouter::governance::{self, Check, Claim};
const TURNS: &str = "/console/v1/traces/turns/query";
const DETAIL: &str = "/console/v1/traces/turns/detail";
const ITEMS: &str = "/console/v1/traces/turns/items";
fn page(t: Uuid, section: &str) -> Value {
    json!({"turn_id":t,"section":section,"limit":2})
}
async fn get(pool: &PgPool, f: &Fixture, path: &str, q: Value) -> Value {
    let (s, r) = call(pool, f, path, &q).await;
    assert_eq!(s, 200, "{r}");
    r
}

#[sqlx::test(migrations = "./migrations")]
async fn traces_permission_scope_and_validation(pool: PgPool) {
    let f = setup(&pool).await;
    let other = setup(&pool).await;
    let t = runtime(&pool, &f).await;
    let foreign = runtime(&pool, &other).await;
    let q = json!({"filter":filter(),"limit":20});
    grant(&pool, &f, Permission::MonitoringRead, true).await;
    for (path, q) in [
        (TURNS, q.clone()),
        (DETAIL, json!({"turn_id":t})),
        (ITEMS, page(t, "nodes")),
    ] {
        assert_eq!(call(&pool, &f, path, &q).await.0, 403);
        for token in [&f.key, &sign(&claims(f.who.tenant_id, "alice", path, &q))] {
            assert_eq!(send(&app(&pool), token, path, &q).await.0, 401);
        }
    }
    grant(&pool, &f, Permission::TracesRead, true).await;
    let r = get(&pool, &f, TURNS, q.clone()).await;
    assert_eq!(r["data"].as_array().unwrap().len(), 1);
    assert_eq!(r["data"][0]["id"], json!(t));
    for (path, q) in [
        (DETAIL, json!({"turn_id":foreign})),
        (ITEMS, page(foreign, "nodes")),
    ] {
        assert_eq!(call(&pool, &f, path, &q).await.0, 404);
    }
    for bad in [
        json!({"filter":filter(),"limit":51}),
        json!({"filter":filter(),"limit":1,"tenant_id":other.who.tenant_id}),
        json!({"filter":{"start":Utc::now(),"end":Utc::now()+Duration::days(32)},"limit":1}),
        json!({"filter":{"start":Utc::now(),"end":Utc::now()+Duration::days(1),"state":"approved"},"limit":1}),
    ] {
        assert_eq!(call(&pool, &f, TURNS, &bad).await.0, 400);
    }
    for bad in [
        json!({"turn_id":t,"section":"checks","limit":1}),
        json!({"turn_id":t,"section":"nodes","operation_id":Uuid::new_v4(),"limit":1}),
        json!({"turn_id":t,"section":"approvals","limit":6}),
        json!({"turn_id":t,"section":"events","after":{"kind":"event","sequence":"01","through":"1"},"limit":1}),
        json!({"turn_id":t,"section":"requests","after":{"kind":"event","sequence":"0","through":"1"},"limit":1}),
    ] {
        assert_eq!(call(&pool, &f, ITEMS, &bad).await.0, 400, "{bad}");
    }
    let mut exact = q.clone();
    exact["filter"]["agent_id"] = json!("missing");
    assert_eq!(get(&pool, &f, TURNS, exact).await["data"], json!([]));
    grant(&pool, &f, Permission::TracesRead, false).await;
    assert_eq!(call(&pool, &f, TURNS, &q).await.0, 403);
}

#[sqlx::test(migrations = "./migrations")]
async fn traces_approval_is_not_execution_and_events_have_a_committed_fence(pool: PgPool) {
    let f = setup(&pool).await;
    let t = runtime(&pool, &f).await;
    guardrail(
        &pool,
        f.who.tenant_id,
        "risk",
        1,
        "tool_input",
        json!({"kind":"always"}),
    )
    .await;
    let a = sample(&pool, &f, t, "private-idempotency").await;
    let op: Uuid = serde_json::from_value(a["id"].clone()).unwrap();
    decide(&pool, "alice", approval_id(&a), approve(&["risk"]))
        .await
        .unwrap();
    sample(&pool, &f, t, "private-idempotency").await;
    grant(&pool, &f, Permission::TracesRead, true).await;
    let r = get(&pool, &f, DETAIL, json!({"turn_id":t})).await;
    assert_eq!(r["nodes"], json!([{"state":"ready","count":"1"}]));
    assert_eq!(r["approvals"], json!([{"state":"approved","count":"1"}]));
    let r = get(&pool, &f, ITEMS, page(t, "approvals")).await;
    assert_eq!(r["data"][0]["grants"][0]["guardrail_id"], "risk");
    assert_eq!(r["data"][0]["grants"][0]["version"], 1);
    let q = json!({"turn_id":t,"section":"checks","operation_id":op,"limit":1});
    let r = get(&pool, &f, ITEMS, q).await;
    assert_eq!(r["data"][0]["outcome"], "risk");
    assert_eq!(r["data"][0]["rule_kind"], "always");
    assert!(!r["data"][0]["approval_skipped_at"].is_null());
    let mut q = page(t, "events");
    q["limit"] = json!(1);
    let first = get(&pool, &f, ITEMS, q.clone()).await;
    let fence = first["event_fence"].clone();
    let mut next = first["next_after"].clone();
    sample(&pool, &f, t, "later-node").await;
    let mut sequences = vec![
        first["data"][0]["sequence"]
            .as_str()
            .unwrap()
            .parse::<i64>()
            .unwrap(),
    ];
    while !next.is_null() {
        q["after"] = next;
        let r = get(&pool, &f, ITEMS, q.clone()).await;
        assert_eq!(r["event_fence"], fence);
        sequences.extend(
            r["data"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v["sequence"].as_str().unwrap().parse::<i64>().unwrap()),
        );
        next = r["next_after"].clone();
    }
    assert!(sequences.windows(2).all(|w| w[0] < w[1]));
    assert_eq!(
        *sequences.last().unwrap(),
        fence.as_str().unwrap().parse::<i64>().unwrap()
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM governance_event_acks WHERE turn_id=$1")
            .bind(t)
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    for section in ["nodes", "requests", "approvals", "events"] {
        let r = get(&pool, &f, ITEMS, page(t, section)).await;
        for secret in [
            "private-idempotency",
            "private-raw-material",
            "payload_ciphertext",
            "policy_snapshot",
        ] {
            assert!(!r.to_string().contains(secret));
        }
    }
    let r = get(
        &pool,
        &f,
        ITEMS,
        json!({"turn_id":t,"section":"events","operation_id":op,"limit":50}),
    )
    .await;
    assert!(
        r["data"]
            .as_array()
            .unwrap()
            .iter()
            .all(|v| v["operation_id"] == json!(op))
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn traces_business_and_judge_costs_are_unique_not_per_node(pool: PgPool) {
    let f = setup(&pool).await;
    let t = runtime(&pool, &f).await;
    let key = "business-idempotency";
    let (r, _) = accounting::reserve(
        &pool,
        Admission {
            identity: &f.who,
            model: "alpha",
            idempotency_key: key,
            request_hash: &store::hash(key.as_bytes()),
            maximum_micros: 1000,
            snapshot: PriceSnapshot {
                currency: "CNY".into(),
                input_per_million: Decimal::ONE,
                output_per_million: Decimal::from(2),
                cny_per_unit: Decimal::ONE,
                fx_version: None,
            },
            routing: None,
            timeout_seconds: 60,
            governance_turn_id: Some(t),
        },
    )
    .await
    .unwrap();
    accounting::mark_submitting(&pool, &f.who, r.id)
        .await
        .unwrap();
    let encrypted = cipher()
        .encrypt(f.who.tenant_id, r.id, br#"{"choices":[]}"#)
        .unwrap();
    accounting::settle_usage(&pool, &f.who, r.id, 10, 20, Some(&encrypted))
        .await
        .unwrap();
    // Gateway completion writes this separate encrypted-result lifetime too.
    sqlx::query(
        "UPDATE requests SET result_expires_at=clock_timestamp()+interval '10 minutes' WHERE id=$1",
    )
    .bind(r.id)
    .execute(&pool)
    .await
    .unwrap();
    guardrail(
        &pool,
        f.who.tenant_id,
        "post-risk",
        1,
        "post",
        json!({"kind":"always"}),
    )
    .await;
    let mut nodes = Vec::new();
    for (stage, key) in [("pre", key.to_owned()), ("post", r.id.to_string())] {
        let a = governance::check(
            &pool,
            &cipher(),
            &f.who,
            Check {
                turn_id: t,
                operation_key: key,
                stage: stage.into(),
                model_id: Some("alpha".into()),
                tool_name: None,
                payload: json!({"private":"raw"}),
            },
        )
        .await
        .unwrap();
        nodes.push(a["id"].clone());
    }
    guardrail(&pool,f.who.tenant_id,"judge",1,"tool_input",json!({"kind":"model_judge","model":"alpha","instructions":"test","input_format":"text_json","max_input_bytes":1000,"max_output_tokens":16,"maximum_cny":"0.001"})).await;
    let a = sample(&pool, &f, t, "judge-node").await;
    let evaluation: Uuid = serde_json::from_value(a["evaluations"][0].clone()).unwrap();
    let judge = Uuid::new_v4();
    let endpoint = Uuid::new_v4();
    sqlx::query("INSERT INTO endpoints(id,tenant_id,model_id,name,upstream_model,base_url,credential_env,currency,input_per_million,output_per_million,context_tokens,max_output_tokens) VALUES($1,$2,'alpha','safe endpoint label','alpha','https://private-provider.invalid','PRIVATE_SECRET_REF','CNY',1,2,10000,1000)").bind(endpoint).bind(f.who.tenant_id).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO attempts(id,tenant_id,request_id,endpoint_id,ordinal,status,http_status,cost_micros) VALUES($1,$2,$3,$4,0,'succeeded',200,50)").bind(Uuid::new_v4()).bind(f.who.tenant_id).bind(r.id).bind(endpoint).execute(&pool).await.unwrap();
    // Synthetic immutable attribution fixture, not a real supplier/admission test.
    sqlx::query("INSERT INTO requests(tenant_id,id,key_id,model_id,idempotency_key,request_hash,status,reserve_micros,price_snapshot,guardrail_evaluation_id) SELECT tenant_id,$1,key_id,model_id,'judge-fixture',request_hash,'submission_unknown',reserve_micros,price_snapshot,$2 FROM requests WHERE id=$3").bind(judge).bind(evaluation).bind(r.id).execute(&pool).await.unwrap();
    grant(&pool, &f, Permission::TracesRead, true).await;
    let detail = get(&pool, &f, DETAIL, json!({"turn_id":t})).await;
    assert_eq!(detail["costs"][0]["purpose"], "guardrail");
    assert_eq!(detail["costs"][0]["unsettled_cost_requests"], "1");
    assert_eq!(detail["costs"][1]["known_cost_cny"], "0.000050");
    assert_eq!(detail["costs"][1]["requests"], "1");
    for op in nodes {
        let rows = get(
            &pool,
            &f,
            ITEMS,
            json!({"turn_id":t,"section":"requests","operation_id":op,"limit":20}),
        )
        .await;
        assert_eq!(rows["data"].as_array().unwrap().len(), 1);
        assert_eq!(rows["data"][0]["id"], json!(r.id));
        assert_eq!(rows["data"][0]["status"], "succeeded");
        assert_eq!(
            rows["data"][0]["attempts"][0]["endpoint_name"],
            "safe endpoint label"
        );
        assert_eq!(rows["data"][0]["attempts"][0]["cost_cny"], "0.000050");
        assert!(!rows.to_string().contains("private-provider"));
        assert!(!rows.to_string().contains("PRIVATE_SECRET_REF"));
    }
    let rows = get(
        &pool,
        &f,
        ITEMS,
        json!({"turn_id":t,"section":"requests","operation_id":a["id"],"limit":20}),
    )
    .await;
    assert_eq!(rows["data"][0]["id"], json!(judge));
    assert_eq!(rows["data"][0]["purpose"], "guardrail");
    sqlx::query("INSERT INTO ledger_entries(tenant_id,request_id,kind,amount_micros,details) VALUES($1,$2,'finalize',408,'{}')").bind(f.who.tenant_id).bind(judge).execute(&pool).await.unwrap();
    sqlx::query("UPDATE requests SET status='reconciled',final_micros=408 WHERE id=$1")
        .bind(judge)
        .execute(&pool)
        .await
        .unwrap();
    let d = get(&pool, &f, DETAIL, json!({"turn_id":t})).await;
    assert_eq!(d["costs"][0]["known_cost_cny"], "0.000408");
    assert_eq!(d["costs"][0]["unsettled_cost_requests"], "0");
    let mut tx = pool.begin().await.unwrap();
    let seq: i64 = sqlx::query_scalar(
        "UPDATE governance_turns SET event_clock=event_clock+1 WHERE id=$1 RETURNING event_clock",
    )
    .bind(t)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    sqlx::query("INSERT INTO governance_events(tenant_id,turn_id,sequence,event_type,data) VALUES($1,$2,$3,'guardrail.finished',$4)").bind(f.who.tenant_id).bind(t).bind(seq).bind(json!({"evaluation_id":evaluation,"reason_code":"private-detection-detail"})).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    let events = get(
        &pool,
        &f,
        ITEMS,
        json!({"turn_id":t,"section":"events","operation_id":a["id"],"limit":50}),
    )
    .await;
    assert!(
        events["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["event_type"] == "guardrail.finished" && e["operation_id"] == a["id"])
    );
    assert!(!events.to_string().contains("private-detection-detail"));
}

#[sqlx::test(migrations = "./migrations")]
async fn traces_turn_microsecond_paging_closed_identity_and_read_revocation(pool: PgPool) {
    let f = setup(&pool).await;
    let t = runtime(&pool, &f).await;
    let newer = turn(&pool, &f.who, "alice", 1).await;
    grant(&pool, &f, Permission::TracesRead, true).await;
    let mut q = json!({"filter":filter(),"limit":1});
    let first = get(&pool, &f, TURNS, q.clone()).await;
    assert_eq!(first["data"][0]["id"], json!(newer));
    q["before"] = first["next_before"].clone();
    let second = get(&pool, &f, TURNS, q).await;
    assert_eq!(second["data"][0]["id"], json!(t));
    assert_eq!(second["data"][0]["state"], "cancelled");
    assert!(second["next_before"].is_null());
    sqlx::query("UPDATE runtime_bindings SET active=false WHERE key_id=$1")
        .bind(f.who.key_id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        get(&pool, &f, DETAIL, json!({"turn_id":newer})).await["turn"]["identity_active"],
        false
    );
    let q = json!({"turn_id":newer});
    let token = console_sign(claims(f.who.tenant_id, "alice", DETAIL, &q));
    let mut lock = pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE governance_operations IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *lock)
        .await
        .unwrap();
    let router = app(&pool);
    let task = tokio::spawn(async move { send(&router, &token, DETAIL, &q).await });
    let mut waiting = false;
    for _ in 0..100 {
        waiting=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND pid<>pg_backend_pid() AND wait_event_type='Lock' AND query LIKE '%console_trace_requests%')").fetch_one(&pool).await.unwrap();
        if waiting {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(waiting, "trace did not reach controlled wait");
    grant(&pool, &f, Permission::TracesRead, false).await;
    lock.rollback().await.unwrap();
    assert_eq!(task.await.unwrap().0, 403);
}

#[sqlx::test(migrations = "./migrations")]
async fn traces_keyset_pagination_and_expired_claim_are_read_only(pool: PgPool) {
    let f = setup(&pool).await;
    let t = runtime(&pool, &f).await;
    let a = sample(&pool, &f, t, "one").await;
    sample(&pool, &f, t, "two").await;
    sample(&pool, &f, t, "three").await;
    let op: Uuid = serde_json::from_value(a["id"].clone()).unwrap();
    governance::claim(
        &pool,
        &cipher(),
        &f.who,
        op,
        Claim {
            token: "11".repeat(32),
            payload_hash: a["payload_hash"].as_str().unwrap().into(),
        },
    )
    .await
    .unwrap();
    sqlx::query("UPDATE governance_operations SET execution_deadline=clock_timestamp()-interval '1 second' WHERE id=$1").bind(op).execute(&pool).await.unwrap();
    grant(&pool, &f, Permission::TracesRead, true).await;
    let mut q = page(t, "nodes");
    q["limit"] = json!(1);
    let mut ids = Vec::new();
    loop {
        let r = get(&pool, &f, ITEMS, q.clone()).await;
        let row = &r["data"][0];
        ids.push(row["id"].clone());
        if row["id"] == json!(op) {
            assert_eq!(row["recorded_state"], "executing");
            assert_eq!(row["state"], "execution_unknown");
        }
        if r["next_after"].is_null() {
            break;
        }
        q["after"] = r["next_after"].clone();
    }
    assert_eq!(ids.len(), 3);
    ids.sort_by_key(|v| v.to_string());
    ids.dedup();
    assert_eq!(ids.len(), 3);
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT state FROM governance_operations WHERE id=$1")
            .bind(op)
            .fetch_one(&pool)
            .await
            .unwrap(),
        "executing"
    );
    let foreign_op = Uuid::new_v4();
    assert_eq!(
        call(
            &pool,
            &f,
            ITEMS,
            &json!({"turn_id":t,"section":"requests","operation_id":foreign_op,"limit":1})
        )
        .await
        .0,
        404
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn traces_restricted_role_no_payload_no_execution_and_safe_unknown_events(pool: PgPool) {
    sqlx::raw_sql(include_str!("../../deploy/permissions.sql"))
        .execute(&pool)
        .await
        .unwrap();
    let f = setup(&pool).await;
    let t = runtime(&pool, &f).await;
    guardrail(
        &pool,
        f.who.tenant_id,
        "risk",
        1,
        "tool_input",
        json!({"kind":"always"}),
    )
    .await;
    let a = sample(&pool, &f, t, "one").await;
    let mut tx = pool.begin().await.unwrap();
    let seq: i64 = sqlx::query_scalar(
        "UPDATE governance_turns SET event_clock=event_clock+1 WHERE id=$1 RETURNING event_clock",
    )
    .bind(t)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    sqlx::query("INSERT INTO governance_events(tenant_id,turn_id,sequence,event_type,data) VALUES($1,$2,$3,'unknown-dangerous-text',$4)").bind(f.who.tenant_id).bind(t).bind(seq).bind(json!({"operation_id":"not-a-uuid","approval_id":"bad","payload":"do-not-expose"})).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    grant(&pool, &f, Permission::TracesRead, true).await;
    let reader = scoped(&pool, "brokerrouter_console").await;
    for (path, q) in [
        (TURNS, json!({"filter":filter(),"limit":20})),
        (DETAIL, json!({"turn_id":t})),
        (ITEMS, page(t, "nodes")),
        (ITEMS, page(t, "requests")),
        (ITEMS, page(t, "approvals")),
        (
            ITEMS,
            json!({"turn_id":t,"section":"checks","operation_id":a["id"],"limit":1}),
        ),
    ] {
        get(&reader, &f, path, q).await;
    }
    let r = get(
        &reader,
        &f,
        ITEMS,
        json!({"turn_id":t,"section":"events","limit":50}),
    )
    .await;
    let last = r["data"].as_array().unwrap().last().unwrap();
    assert_eq!(last["event_type"], "unrecognized");
    assert!(last["operation_id"].is_null());
    assert!(!r.to_string().contains("do-not-expose"));
    for sql in [
        "SELECT payload_ciphertext FROM governance_operations",
        "SELECT operation_key FROM governance_operations",
        "SELECT data FROM governance_events",
        "SELECT credential_env FROM endpoints",
        "UPDATE approvals SET state='approved'",
        "UPDATE governance_event_acks SET sequence=100",
        "SELECT operation_key FROM console_trace_nodes",
    ] {
        assert!(sqlx::query(sql).execute(&reader).await.is_err(), "{sql}");
    }
    reader.close().await;
}
