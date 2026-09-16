use super::*;
use brokerrouter::governance::{self, Check};
const MONITOR: &str = "/console/v1/governance/monitor/query";
const RAILS: &str = "/console/v1/governance/monitor/rails/query";
pub(super) async fn runtime(pool: &PgPool, f: &Fixture) -> Uuid {
    human::bind_runtime(
        pool,
        f.who.tenant_id,
        f.who.key_id,
        f.user,
        "hermes",
        "agent",
    )
    .await
    .unwrap();
    let path = "/v1/governance/turns";
    let body = json!({"key_id":f.who.key_id,"session_id":"session","message_id":"message","run_id":"run","generation":0,"ttl_seconds":900});
    let p = verifier()
        .verify(
            pool,
            &sign(&claims(f.who.tenant_id, "alice", path, &body)),
            "POST",
            path,
            &serde_json::to_vec(&body).unwrap(),
        )
        .await
        .unwrap();
    let r = human::create_turn(pool, &p, serde_json::from_value(body).unwrap())
        .await
        .unwrap();
    serde_json::from_value(r["id"].clone()).unwrap()
}
fn check(t: Uuid, key: &str) -> Check {
    Check {
        turn_id: t,
        operation_key: key.into(),
        stage: "tool_input".into(),
        model_id: Some("alpha".into()),
        tool_name: Some("send".into()),
        payload: json!({"text":"private-raw-material"}),
    }
}
pub(super) async fn sample(pool: &PgPool, f: &Fixture, t: Uuid, key: &str) -> Value {
    governance::check(pool, &cipher(), &f.who, check(t, key))
        .await
        .unwrap()
}

#[sqlx::test(migrations = "./migrations")]
async fn monitoring_preserves_original_risk_deduplicates_polls_grants_and_versions(pool: PgPool) {
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
    guardrail(
        &pool,
        f.who.tenant_id,
        "pass",
        1,
        "tool_input",
        json!({"kind":"contains_any","terms":["not-present"],"case_sensitive":true}),
    )
    .await;
    let a = sample(&pool, &f, t, "one").await;
    for _ in 0..3 {
        assert_eq!(sample(&pool, &f, t, "one").await, a);
    }
    decide(&pool, "alice", approval_id(&a), approve(&["risk"]))
        .await
        .unwrap();
    assert_eq!(sample(&pool, &f, t, "one").await["state"], "ready");
    sample(&pool, &f, t, "two").await;
    grant(&pool, &f, Permission::MonitoringRead, true).await;
    let (status, r) = call(&pool, &f, MONITOR, &query()).await;
    assert_eq!(status, 200, "{r}");
    for (k, v) in [
        ("checks", "4"),
        ("passed", "2"),
        ("risk", "1"),
        ("failed", "0"),
        ("skipped_initial", "1"),
        ("approval_skipped", "2"),
        ("judge_requests", "0"),
        ("inline_samples", "3"),
    ] {
        assert_eq!(r["totals"][k], v, "{k}: {r}");
    }
    assert_eq!(r["totals"]["classified_pass_percent"], "66.6667");
    assert_eq!(r["totals"]["mean_judge_lifecycle_ms"], Value::Null);
    assert_eq!(r["approvals"], json!([{"state":"approved","count":"1"}]));
    assert_eq!(
        r["operations"],
        json!([{"state":"ready","count":"2","closed_turn":"0"}])
    );
    assert!(!r.to_string().contains("private-raw-material"));
    guardrail(
        &pool,
        f.who.tenant_id,
        "risk",
        2,
        "tool_input",
        json!({"kind":"always"}),
    )
    .await;
    sample(&pool, &f, t, "one").await;
    let r = call(&pool, &f, MONITOR, &query()).await.1;
    assert_eq!(r["totals"]["checks"], "5");
    assert_eq!(r["totals"]["risk"], "2");
    let mut q = json!({"filter":filter(),"limit":1});
    let mut versions = Vec::new();
    loop {
        let (s, r) = call(&pool, &f, RAILS, &q).await;
        assert_eq!(s, 200, "{r}");
        versions.push((
            r["data"][0]["guardrail_id"].clone(),
            r["data"][0]["version"].clone(),
            r["data"][0]["current_active"].clone(),
        ));
        if r["next_after"].is_null() {
            break;
        }
        q["after"] = r["next_after"].clone();
    }
    assert_eq!(
        versions,
        vec![
            (json!("pass"), json!(1), json!(true)),
            (json!("risk"), json!(1), json!(false)),
            (json!("risk"), json!(2), json!(true))
        ]
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn monitoring_filters_permissions_tenant_separation_and_bounds(pool: PgPool) {
    let f = setup(&pool).await;
    let other = setup(&pool).await;
    let t = runtime(&pool, &other).await;
    guardrail(
        &pool,
        other.who.tenant_id,
        "other",
        1,
        "tool_input",
        json!({"kind":"always"}),
    )
    .await;
    sample(&pool, &other, t, "other").await;
    for p in [
        Permission::UsageRead,
        Permission::GovernanceRead,
        Permission::GovernanceWrite,
    ] {
        grant(&pool, &f, p, true).await;
    }
    assert_eq!(call(&pool, &f, MONITOR, &query()).await.0, 403);
    grant(&pool, &f, Permission::MonitoringRead, true).await;
    let (s, r) = call(&pool, &f, MONITOR, &query()).await;
    assert_eq!(s, 200, "{r}");
    assert_eq!(r["totals"]["checks"], "0");
    assert_eq!(r["tenant_active_guardrails"], "0");
    assert!(
        r["daily"]
            .as_array()
            .unwrap()
            .iter()
            .all(|d| d["totals"]["checks"] == "0")
    );
    assert_eq!(r["totals"]["classified_pass_percent"], Value::Null);
    for bad in [
        json!({"filter":{"start":Utc::now(),"end":Utc::now()+Duration::days(32)}}),
        json!({"filter":{"start":Utc::now(),"end":Utc::now()-Duration::seconds(1)}}),
        json!({"filter":filter(),"timezone":"Europe/Paris"}),
        json!({"filter":filter(),"tenant_id":other.who.tenant_id}),
    ] {
        assert_eq!(call(&pool, &f, MONITOR, &bad).await.0, 400);
    }
    for bad in [
        json!({"filter":filter(),"limit":101}),
        json!({"filter":filter(),"limit":1,"after":{"guardrail_id":"other","version":0}}),
    ] {
        assert_eq!(call(&pool, &f, RAILS, &bad).await.0, 400);
    }
    let mut q = query();
    q["filter"]["key_id"] = json!(other.who.key_id);
    assert_eq!(
        call(&pool, &f, MONITOR, &q).await.1["totals"]["checks"],
        "0"
    );
    grant(&pool, &f, Permission::MonitoringRead, false).await;
    assert_eq!(call(&pool, &f, MONITOR, &query()).await.0, 403);
}

#[sqlx::test(migrations = "./migrations")]
async fn monitoring_semantic_status_cost_and_late_settlement_are_not_multiplied(pool: PgPool) {
    let f = setup(&pool).await;
    let t = runtime(&pool, &f).await;
    guardrail(&pool,f.who.tenant_id,"judge",1,"tool_input",json!({"kind":"model_judge","model":"alpha","instructions":"test","input_format":"text_json","max_input_bytes":1000,"max_output_tokens":16,"maximum_cny":"0.001"})).await;
    let a = sample(&pool, &f, t, "semantic").await;
    assert_eq!(a["state"], "checking");
    let evaluation: Uuid = serde_json::from_value(a["evaluations"][0].clone()).unwrap();
    sqlx::query("UPDATE guardrail_evaluations SET state='running',deadline=clock_timestamp()-interval '1 second' WHERE id=$1").bind(evaluation).execute(&pool).await.unwrap();
    grant(&pool, &f, Permission::MonitoringRead, true).await;
    let r = call(&pool, &f, MONITOR, &query()).await.1;
    assert_eq!(r["totals"]["running"], "1");
    assert_eq!(r["totals"]["overdue_running"], "1");
    assert_eq!(r["totals"]["failed"], "0");
    // A fixture request is linked at creation, just like the server-owned judge admission.
    let request = Uuid::new_v4();
    sqlx::query("INSERT INTO requests(tenant_id,id,key_id,model_id,idempotency_key,request_hash,status,reserve_micros,price_snapshot,guardrail_evaluation_id) SELECT tenant_id,$1,key_id,model_id,'judge-fixture',request_hash,'submission_unknown',reserve_micros,price_snapshot,$2 FROM requests WHERE id=$3")
        .bind(request).bind(evaluation).bind(reservation(&pool,&f,"alpha").await).execute(&pool).await.unwrap();
    // The fixture copy is not money admission; only insert matching immutable ledger evidence.
    let r = call(&pool, &f, MONITOR, &query()).await.1;
    assert_eq!(r["totals"]["unsettled_cost_requests"], "1");
    assert_eq!(r["totals"]["reserved_cny"], "0.001000");
    sqlx::query("INSERT INTO ledger_entries(tenant_id,request_id,kind,amount_micros,details) VALUES($1,$2,'finalize',408,'{}')").bind(f.who.tenant_id).bind(request).execute(&pool).await.unwrap();
    sqlx::query("UPDATE requests SET status='reconciled',final_micros=408 WHERE id=$1")
        .bind(request)
        .execute(&pool)
        .await
        .unwrap();
    let a = sample(&pool, &f, t, "semantic").await;
    assert_eq!(a["state"], "waiting_approval");
    let r = call(&pool, &f, MONITOR, &query()).await.1;
    assert_eq!(r["totals"]["checks"], "1");
    assert_eq!(r["totals"]["failed"], "1");
    assert_eq!(r["totals"]["judge_samples"], "1");
    assert_eq!(r["totals"]["known_cost_cny"], "0.000408");
    for _ in 0..3 {
        sample(&pool, &f, t, "semantic").await;
    }
    let r = call(&pool, &f, MONITOR, &query()).await.1;
    assert_eq!(r["totals"]["judge_requests"], "1");
    assert_eq!(r["totals"]["known_cost_cny"], "0.000408");
}

#[sqlx::test(migrations = "./migrations")]
async fn monitoring_roles_observation_immutability_and_safe_projection(pool: PgPool) {
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
    let runtime = scoped(&pool, "brokerrouter_runtime").await;
    sample(&runtime, &f, t, "role").await;
    let operator = scoped(&pool, "brokerrouter_operator").await;
    grant(&operator, &f, Permission::MonitoringRead, true).await;
    let reader = scoped(&pool, "brokerrouter_console").await;
    for (path, q) in [
        (MONITOR, query()),
        (RAILS, json!({"filter":filter(),"limit":20})),
    ] {
        let (s, r) = call(&reader, &f, path, &q).await;
        assert_eq!(s, 200, "{r}");
    }
    for sql in [
        "SELECT * FROM guardrail_observations",
        "SELECT * FROM guardrail_evaluations",
        "SELECT data FROM governance_events",
        "SELECT policy_snapshot FROM approvals",
        "SELECT payload_ciphertext FROM governance_operations",
        "UPDATE approvals SET state='approved'",
        "UPDATE console_permissions SET enabled=true",
        "SET ROLE brokerrouter_runtime",
    ] {
        assert!(sqlx::query(sql).execute(&reader).await.is_err(), "{sql}");
    }
    for sql in [
        "UPDATE guardrail_observations SET first_outcome='passed'",
        "DELETE FROM guardrail_observations",
        "TRUNCATE guardrail_observations",
        "UPDATE guardrail_observations SET created_at=now()",
    ] {
        assert!(sqlx::query(sql).execute(&pool).await.is_err(), "{sql}");
    }
    runtime.close().await;
    operator.close().await;
    reader.close().await;
}

#[sqlx::test(migrations = false)]
async fn monitoring_migration_deduplicates_legacy_events_without_fabricating_latency(pool: PgPool) {
    let migrator = sqlx::migrate::Migrator::new(std::path::Path::new("./migrations"))
        .await
        .unwrap();
    for m in migrator.iter().filter(|m| m.version < 16) {
        sqlx::raw_sql(&m.sql).execute(&pool).await.unwrap();
    }
    let f = setup(&pool).await;
    let t = runtime(&pool, &f).await;
    guardrail(
        &pool,
        f.who.tenant_id,
        "legacy",
        1,
        "tool_input",
        json!({"kind":"always"}),
    )
    .await;
    let op = Uuid::new_v4();
    sqlx::query("INSERT INTO governance_operations(tenant_id,id,turn_id,key_id,operation_key,stage,payload_hash) VALUES($1,$2,$3,$4,'legacy','tool_input',$5)").bind(f.who.tenant_id).bind(op).bind(t).bind(f.who.key_id).bind(vec![0_u8;32]).execute(&pool).await.unwrap();
    for (n, outcome) in [(2, "risk"), (3, "risk"), (4, "skipped_by_user_approval")] {
        sqlx::query("INSERT INTO governance_events(tenant_id,turn_id,sequence,event_type,data) VALUES($1,$2,$3,'guardrails.evaluated',$4)").bind(f.who.tenant_id).bind(t).bind(n as i64).bind(json!({"operation_id":op,"results":[{"id":"legacy","version":1,"outcome":outcome}]})).execute(&pool).await.unwrap();
    }
    sqlx::raw_sql(include_str!(
        "../../migrations/0016_guardrail_monitoring.sql"
    ))
    .execute(&pool)
    .await
    .unwrap();
    grant(&pool, &f, Permission::MonitoringRead, true).await;
    let (s, r) = call(&pool, &f, MONITOR, &query()).await;
    assert_eq!(s, 200, "{r}");
    assert_eq!(r["totals"]["checks"], "1");
    assert_eq!(r["totals"]["risk"], "1");
    assert_eq!(r["totals"]["approval_skipped"], "1");
    assert_eq!(r["totals"]["legacy_checks"], "1");
    assert_eq!(r["totals"]["mean_inline_ms"], Value::Null);
    assert_eq!(r["history_may_be_partial"], true);
}

#[sqlx::test(migrations = "./migrations")]
async fn monitoring_microsecond_window_and_all_selectors_are_conjunctive(pool: PgPool) {
    let f = setup(&pool).await;
    let t = runtime(&pool, &f).await;
    guardrail(
        &pool,
        f.who.tenant_id,
        "boundary",
        1,
        "tool_input",
        json!({"kind":"always"}),
    )
    .await;
    sample(&pool, &f, t, "boundary").await;
    grant(&pool, &f, Permission::MonitoringRead, true).await;
    let at: DateTime<Utc> =
        sqlx::query_scalar("SELECT created_at FROM guardrail_observations WHERE tenant_id=$1")
            .bind(f.who.tenant_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    let mut q = json!({"filter":{"start":at,"end":at+Duration::microseconds(1),"guardrail_id":"boundary","model_id":"alpha","key_id":f.who.key_id,"stage":"tool_input"},"timezone":"UTC"});
    let (s, r) = call(&pool, &f, MONITOR, &q).await;
    assert_eq!(s, 200, "{r}");
    assert_eq!(r["totals"]["checks"], "1");
    assert_eq!(r["daily"].as_array().unwrap().len(), 1);
    for (field, value) in [
        ("guardrail_id", json!("missing")),
        ("model_id", json!("beta")),
        ("key_id", json!(Uuid::new_v4())),
        ("stage", json!("pre")),
    ] {
        let mut wrong = q.clone();
        wrong["filter"][field] = value;
        assert_eq!(
            call(&pool, &f, MONITOR, &wrong).await.1["totals"]["checks"],
            "0"
        );
    }
    q["filter"]["start"] = json!(at - Duration::microseconds(1));
    q["filter"]["end"] = json!(at);
    assert_eq!(
        call(&pool, &f, MONITOR, &q).await.1["totals"]["checks"],
        "0"
    );
}
