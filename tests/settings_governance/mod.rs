use super::*;
use brokerrouter::{
    error::Error,
    governance::{self, Check},
};
const RAILS: &str = "/settings/v1/guardrails/query";
const POLICIES: &str = "/settings/v1/policies/query";
const HISTORY: &str = "/settings/v1/guardrails/history";
const PREVIEW: &str = "/settings/v1/policies/preview";
const TARGETS: &str = "/settings/v1/governance/targets";
async fn writer(pool: &PgPool, f: &Fixture) {
    for p in [Permission::GovernanceRead, Permission::GovernanceWrite] {
        grant(pool, f, p, true).await;
    }
}
fn rail(id: &str, revision: Option<&str>, active: bool) -> Value {
    mutation(
        json!({"kind":"guardrail_publish","id":id,"expected_revision":revision,"name":"Sensitive operation","stage":"tool_input","rule":{"kind":"always"},"active":active}),
    )
}
fn policy(rail: &str) -> Value {
    mutation(
        json!({"kind":"policy_put","id":Uuid::new_v4(),"expected_revision":null,"guardrail_id":rail,"model_id":null,"key_id":null,"runtime_id":null,"tool_name":null,"active":false}),
    )
}
async fn ok(pool: &PgPool, f: &Fixture, body: &Value) -> Value {
    let (s, r) = call(pool, f, APPLY, body).await;
    assert_eq!(s, 200, "{r}");
    r
}
fn next(m: &Value, revision: &str, active: bool) -> Value {
    let mut n = m.clone();
    n["operation_id"] = json!(Uuid::new_v4());
    n["change"]["expected_revision"] = json!(revision);
    n["change"]["active"] = json!(active);
    n
}
async fn enabled(pool: &PgPool, f: &Fixture, id: &str) -> Value {
    ok(pool, f, &rail(id, None, false)).await;
    ok(pool, f, &rail(id, Some("1"), true)).await;
    let p = policy(id);
    ok(pool, f, &p).await;
    let p = next(&p, "1", true);
    ok(pool, f, &p).await;
    p
}
async fn runtime(pool: &PgPool, f: &Fixture) -> store::Identity {
    let (key_id, _) = store::create_key(
        pool,
        f.tenant,
        "governed",
        1_000_000,
        &["test/model".into()],
        None,
    )
    .await
    .unwrap();
    human::bind_runtime(pool, f.tenant, key_id, f.user, "hermes-test", "agent-test")
        .await
        .unwrap();
    store::Identity {
        tenant_id: f.tenant,
        key_id,
    }
}
fn check(t: Uuid, operation: &str) -> Check {
    Check {
        turn_id: t,
        operation_key: operation.into(),
        stage: "tool_input".into(),
        model_id: Some("test/model".into()),
        tool_name: Some("send_message".into()),
        payload: json!({"text":"test-only untrusted content"}),
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn governance_configuration_has_separate_human_permissions_and_tenant_scope(pool: PgPool) {
    let f = setup(&pool).await;
    all(&pool, &f).await;
    let m = rail("risk", None, false);
    assert_eq!(call(&pool, &f, RAILS, &json!({"limit":20})).await.0, 403);
    assert_eq!(call(&pool, &f, APPLY, &m).await.0, 403);
    grant(&pool, &f, Permission::GovernanceWrite, true).await;
    assert_eq!(call(&pool, &f, APPLY, &m).await.0, 403);
    grant(&pool, &f, Permission::GovernanceRead, true).await;
    for token in [
        sign(&claims(f.tenant, "alice", APPLY, &m)),
        catalog_sign(claims(f.tenant, "alice", APPLY, &m)),
        console_sign(claims(f.tenant, "alice", APPLY, &m)),
    ] {
        assert_eq!(send(&app(&pool), APPLY, &m, token).await.0, 401);
    }
    ok(&pool, &f, &m).await;
    let other = setup(&pool).await;
    writer(&pool, &other).await;
    for (path, body) in [
        (RAILS, json!({"limit":20})),
        (POLICIES, json!({"limit":20})),
        (HISTORY, json!({"id":"risk","limit":20})),
        (TARGETS, json!({"kind":"runtime","limit":20})),
    ] {
        let (s, r) = call(&pool, &other, path, &body).await;
        assert_eq!(s, 200, "{r}");
        assert_eq!(r["data"], json!([]));
    }
    assert_eq!(
        call(&pool, &other, APPLY, &rail("risk", Some("1"), true))
            .await
            .0,
        409
    );
    assert_eq!(call(&pool, &other, APPLY, &policy("risk")).await.0, 400);
    grant(&pool, &f, Permission::GovernanceWrite, false).await;
    assert_eq!(call(&pool, &f, APPLY, &m).await.0, 403);
    assert_eq!(
        call(&pool, &f, RAILS, &json!({"limit":20})).await.1["can_write"],
        false
    );
    assert_eq!(call(&pool, &f, RAILS, &json!({"limit":101})).await.0, 400);
    assert_eq!(call(&pool, &f, RAILS, &json!({"limit":21})).await.0, 400);
    assert_eq!(
        call(
            &pool,
            &f,
            RAILS,
            &json!({"limit":20,"tenant_id":other.tenant})
        )
        .await
        .0,
        400
    );
}
#[sqlx::test(migrations = "./migrations")]
async fn publication_is_idempotent_monotonic_and_serializes_cli_and_browser(pool: PgPool) {
    let f = setup(&pool).await;
    writer(&pool, &f).await;
    let create = rail("risk", None, false);
    assert_eq!(
        call(&pool, &f, APPLY, &rail("risk", None, true)).await.0,
        400
    );
    let (a, b) = tokio::join!(
        call(&pool, &f, APPLY, &create),
        call(&pool, &f, APPLY, &create)
    );
    assert_eq!(a, b);
    assert_eq!(a.0, 200, "{a:?}");
    let update = rail("risk", Some("1"), true);
    let mut other = update.clone();
    other["operation_id"] = json!(Uuid::new_v4());
    other["change"]["name"] = json!("other edit");
    let (a, b) = tokio::join!(
        call(&pool, &f, APPLY, &update),
        call(&pool, &f, APPLY, &other)
    );
    assert!(
        a.0 == 200 && b.0 == 409 || a.0 == 409 && b.0 == 200,
        "{a:?} {b:?}"
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM guardrail_versions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 2);
    human::publish_guardrail(
        &pool,
        f.tenant,
        governance::Guardrail {
            id: "risk".into(),
            version: 5,
            name: "CLI version".into(),
            stage: "tool_input".into(),
            rule: json!({"kind":"always"}),
        },
    )
    .await
    .unwrap();
    assert_eq!(
        call(&pool, &f, APPLY, &rail("risk", Some("2"), false))
            .await
            .0,
        409
    );
    ok(&pool, &f, &rail("risk", Some("3"), false)).await;
    let rows = call(&pool, &f, HISTORY, &json!({"id":"risk","limit":2}))
        .await
        .1;
    assert_eq!(rows["data"][0]["version"], 6);
    assert_eq!(rows["next_before"], 5);
    assert_eq!(
        call(
            &pool,
            &f,
            HISTORY,
            &json!({"id":"risk","limit":2,"before":5})
        )
        .await
        .1["data"][0]["version"],
        2
    );
    for sql in [
        "UPDATE guardrails SET active=true",
        "UPDATE guardrails SET version=1",
        "UPDATE guardrail_versions SET name='mutated'",
        "DELETE FROM guardrail_versions",
    ] {
        assert!(sqlx::query(sql).execute(&pool).await.is_err(), "{sql}");
    }
    // Advancing to a nonexistent immutable version is rejected at commit.
    assert!(
        sqlx::query("UPDATE guardrails SET version=100")
            .execute(&pool)
            .await
            .is_err()
    );
    let audit:Value=sqlx::query_scalar("SELECT details FROM audit_events WHERE action='settings.guardrail_published' ORDER BY id DESC LIMIT 1").fetch_one(&pool).await.unwrap();
    assert_eq!(audit["before"]["version"], 5);
    assert_eq!(audit["after"]["version"], 6);
    assert_eq!(audit["user_id"], f.user.to_string());
}
#[sqlx::test(migrations = "./migrations")]
async fn changed_versions_invalidate_pending_approvals_and_never_revive_old_grants(pool: PgPool) {
    let f = setup(&pool).await;
    writer(&pool, &f).await;
    let who = runtime(&pool, &f).await;
    enabled(&pool, &f, "risk").await;
    let t = turn(&pool, &who, "alice", 0).await;
    let a = governance::check(&pool, &cipher(), &who, check(t, "pending"))
        .await
        .unwrap();
    ok(&pool, &f, &rail("risk", Some("2"), true)).await;
    assert!(matches!(
        decide(&pool, "alice", approval_id(&a), approve(&["risk"])).await,
        Err(Error::PolicyChanged)
    ));
    let b = governance::check(&pool, &cipher(), &who, check(t, "pending"))
        .await
        .unwrap();
    assert_ne!(approval_id(&a), approval_id(&b));
    decide(&pool, "alice", approval_id(&b), approve(&["risk"]))
        .await
        .unwrap();
    assert_eq!(
        governance::check(&pool, &cipher(), &who, check(t, "later"))
            .await
            .unwrap()["state"],
        "ready"
    );
    ok(&pool, &f, &rail("risk", Some("3"), false)).await;
    ok(&pool, &f, &rail("risk", Some("4"), true)).await;
    let c = governance::check(&pool, &cipher(), &who, check(t, "after-reenable"))
        .await
        .unwrap();
    assert_eq!(c["state"], "waiting_approval");
    assert_eq!(c["approval"]["guardrails"][0]["version"], 5);
    // A configuration proof cannot be used by the human decision listener.
    let path = format!("/v1/approvals/{}/resolve", approval_id(&c));
    let body = approve(&["risk"]);
    assert!(
        verifier()
            .verify(
                &pool,
                &settings_sign(claims(f.tenant, "alice", &path, &body)),
                "POST",
                &path,
                &serde_json::to_vec(&body).unwrap()
            )
            .await
            .is_err()
    );
}
#[sqlx::test(migrations = "./migrations")]
async fn selector_edits_recheck_snapshot_but_preserve_same_version_turn_grants(pool: PgPool) {
    let f = setup(&pool).await;
    writer(&pool, &f).await;
    let who = runtime(&pool, &f).await;
    let p = enabled(&pool, &f, "risk").await;
    let t = turn(&pool, &who, "alice", 0).await;
    let a = governance::check(&pool, &cipher(), &who, check(t, "first"))
        .await
        .unwrap();
    let off = next(&p, "2", false);
    ok(&pool, &f, &off).await;
    assert!(matches!(
        decide(&pool, "alice", approval_id(&a), approve(&["risk"])).await,
        Err(Error::PolicyChanged)
    ));
    let on = next(&off, "3", true);
    ok(&pool, &f, &on).await;
    // Same selected version again: pending approval remains semantically exact.
    decide(&pool, "alice", approval_id(&a), approve(&["risk"]))
        .await
        .unwrap();
    let mut exact = next(&on, "4", true);
    exact["change"]["model_id"] = json!("test/model");
    ok(&pool, &f, &exact).await;
    assert_eq!(
        governance::check(&pool, &cipher(), &who, check(t, "second"))
            .await
            .unwrap()["state"],
        "ready"
    );
    let t2 = turn(&pool, &who, "alice", 1).await;
    assert_eq!(
        governance::check(&pool, &cipher(), &who, check(t2, "next-turn"))
            .await
            .unwrap()["state"],
        "waiting_approval"
    );
    // CLI update participates in optimistic configuration revisions.
    human::put_policy(&pool,f.tenant,serde_json::from_value(json!({"id":p["change"]["id"],"guardrail_id":"risk","active":false,"model_id":null,"key_id":null,"runtime_id":null,"tool_name":null})).unwrap()).await.unwrap();
    assert_eq!(
        call(&pool, &f, APPLY, &next(&exact, "5", false)).await.0,
        409
    );
}
#[sqlx::test(migrations = "./migrations")]
async fn preview_is_exact_deduplicated_read_only_and_paginated(pool: PgPool) {
    let f = setup(&pool).await;
    writer(&pool, &f).await;
    let who = runtime(&pool, &f).await;
    let p = enabled(&pool, &f, "risk").await;
    let mut exact = next(&p, "2", true);
    for (k, v) in [
        ("model_id", json!("test/model")),
        ("key_id", json!(who.key_id)),
        ("runtime_id", json!("hermes-test")),
        ("tool_name", json!("send_message")),
    ] {
        exact["change"][k] = v;
    }
    ok(&pool, &f, &exact).await;
    let input = json!({"key_id":who.key_id,"model_id":"test/model","tool_name":"send_message","stage":"tool_input"});
    let r = call(&pool, &f, PREVIEW, &input).await;
    assert_eq!(r.0, 200, "{r:?}");
    assert_eq!(r.1["data"].as_array().unwrap().len(), 1);
    assert_eq!(r.1["evaluated_payload"], false);
    assert!(r.1["data"][0].get("rule").is_none());
    for (k, v) in [
        ("model_id", json!("test/other")),
        ("tool_name", json!("different")),
        ("stage", json!("pre")),
    ] {
        let mut mismatch = input.clone();
        mismatch[k] = v;
        assert_eq!(
            call(&pool, &f, PREVIEW, &mismatch).await.1["data"],
            json!([])
        );
    }
    let mut dup = policy("risk");
    dup["change"]["tool_name"] = json!("send_message");
    ok(&pool, &f, &dup).await;
    ok(&pool, &f, &next(&dup, "1", true)).await;
    assert_eq!(
        call(&pool, &f, PREVIEW, &input).await.1["data"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    for table in [
        "governance_operations",
        "guardrail_evaluations",
        "guardrail_grants",
        "requests",
    ] {
        let n: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, 0);
    }
    let r = call(&pool, &f, TARGETS, &json!({"kind":"model","limit":1}))
        .await
        .1;
    assert_eq!(r["data"].as_array().unwrap().len(), 1);
    assert!(!r["next_after"].is_null());
    let n = call(
        &pool,
        &f,
        TARGETS,
        &json!({"kind":"model","limit":1,"after":r["next_after"]}),
    )
    .await
    .1;
    assert_ne!(r["data"][0]["id"], n["data"][0]["id"]);
    let key = call(&pool, &f, TARGETS, &json!({"kind":"key","limit":20}))
        .await
        .1;
    assert!(!key.to_string().contains("prefix"));
    grant(&pool, &f, Permission::GovernanceRead, false).await;
    assert_eq!(call(&pool, &f, PREVIEW, &input).await.0, 403);
}
#[sqlx::test(migrations = "./migrations")]
async fn structured_rules_validate_media_references_and_allow_safe_retirement(pool: PgPool) {
    let f = setup(&pool).await;
    writer(&pool, &f).await;
    for rule in [
        json!({"kind":"contains_any","terms":[],"case_sensitive":false}),
        json!({"kind":"always","bypass":true}),
        json!({"kind":"regex","pattern":".*"}),
        json!({"kind":"asset_scan","profile_id":"bad"}),
    ] {
        let mut m = rail("invalid", None, false);
        m["change"]["rule"] = rule;
        assert_eq!(call(&pool, &f, APPLY, &m).await.0, 400);
    }
    let mut judge = rail("semantic", None, false);
    judge["change"]["rule"] = json!({"kind":"model_judge","model":"test/model","instructions":"Assess risk","input_format":"text_json","max_input_bytes":1024,"max_output_tokens":64,"maximum_cny":"0.010001"});
    ok(&pool, &f, &judge).await;
    assert_eq!(
        call(&pool, &f, APPLY, &next(&judge, "1", true)).await.0,
        400
    ); // no chat endpoint
    let profile = Uuid::new_v4();
    sqlx::query("INSERT INTO output_scan_profiles(tenant_id,id,model_id,version,engine_revision) VALUES($1,$2,'test/model',1,repeat('a',64))").bind(f.tenant).bind(profile).execute(&pool).await.unwrap();
    let mut media = rail("scan", None, false);
    media["change"]["stage"] = json!("media_output");
    media["change"]["rule"] = json!({"kind":"asset_scan","profile_id":profile});
    ok(&pool, &f, &media).await;
    ok(&pool, &f, &next(&media, "1", true)).await;
    let mut p = policy("scan");
    assert_eq!(call(&pool, &f, APPLY, &p).await.0, 400);
    p["change"]["model_id"] = json!("test/model");
    ok(&pool, &f, &p).await;
    ok(&pool, &f, &next(&p, "1", true)).await;
    sqlx::query("UPDATE output_scan_profiles SET active=false")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(call(&pool, &f, APPLY, &next(&p, "2", true)).await.0, 400);
    ok(&pool, &f, &next(&p, "2", false)).await;
    ok(&pool, &f, &next(&media, "2", false)).await;
    let mut stage = rail("scan", Some("3"), false);
    stage["change"]["stage"] = json!("pre");
    assert_eq!(call(&pool, &f, APPLY, &stage).await.0, 400);
    let f2 = setup(&pool).await;
    writer(&pool, &f2).await;
    let mut foreign = media.clone();
    foreign["operation_id"] = json!(Uuid::new_v4());
    assert_eq!(call(&pool, &f2, APPLY, &foreign).await.0, 400);
}
#[sqlx::test(migrations = "./migrations")]
async fn governance_configuration_receipts_and_audit_commit_atomically(pool: PgPool) {
    let f = setup(&pool).await;
    writer(&pool, &f).await;
    sqlx::raw_sql("CREATE FUNCTION reject_governance_audit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.action='settings.guardrail_published' THEN RAISE EXCEPTION 'injected'; END IF; RETURN NEW; END; $$; CREATE TRIGGER reject_governance_audit BEFORE INSERT ON audit_events FOR EACH ROW EXECUTE FUNCTION reject_governance_audit();").execute(&pool).await.unwrap();
    let m = rail("rollback", None, false);
    assert_eq!(call(&pool, &f, APPLY, &m).await.0, 503);
    for table in ["guardrails", "guardrail_versions", "settings_mutations"] {
        let n: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, 0);
    }
    sqlx::query("DROP TRIGGER reject_governance_audit ON audit_events")
        .execute(&pool)
        .await
        .unwrap();
    ok(&pool, &f, &m).await;
    let mut different = m.clone();
    different["reason"] = json!("changed reason");
    assert_eq!(call(&pool, &f, APPLY, &different).await.0, 409);
}
#[sqlx::test(migrations = "./migrations")]
async fn restricted_settings_role_can_configure_but_cannot_approve_or_access_runtime_content(
    pool: PgPool,
) {
    sqlx::raw_sql(include_str!("../../deploy/permissions.sql"))
        .execute(&pool)
        .await
        .unwrap();
    let f = setup(&pool).await;
    writer(&pool, &f).await;
    let who = runtime(&pool, &f).await;
    let restricted = scoped(&pool, "brokerrouter_settings").await;
    enabled(&restricted, &f, "risk").await;
    sqlx::query("INSERT INTO endpoints(id,tenant_id,model_id,name,upstream_model,base_url,credential_env,currency,input_per_million,output_per_million,context_tokens,max_output_tokens) VALUES($1,$2,'test/model','fixture','fixture','https://fixture.invalid/v1','TEST_ONLY_NOT_INJECTED','CNY',0,0,4096,512)").bind(Uuid::new_v4()).bind(f.tenant).execute(&pool).await.unwrap();
    let mut semantic = rail("semantic", None, false);
    semantic["change"]["rule"] = json!({"kind":"model_judge","model":"test/model","instructions":"Check risk","input_format":"text_json","max_input_bytes":1024,"max_output_tokens":64,"maximum_cny":"0.000001"});
    ok(&restricted, &f, &semantic).await;
    ok(&restricted, &f, &next(&semantic, "1", true)).await;
    let mut literal = rail("literal", None, false);
    literal["change"]["rule"] =
        json!({"kind":"contains_any","terms":["发送"],"case_sensitive":false});
    ok(&restricted, &f, &literal).await;
    for (path, body) in [
        (RAILS, json!({"limit":20})),
        (HISTORY, json!({"id":"risk","limit":10})),
        (POLICIES, json!({"limit":20})),
        (
            PREVIEW,
            json!({"key_id":who.key_id,"stage":"tool_input","model_id":null,"tool_name":null}),
        ),
    ] {
        let (s, r) = call(&restricted, &f, path, &body).await;
        assert_eq!(s, 200, "{path}: {r}");
    }
    for kind in ["model", "key", "runtime", "scan_profile"] {
        let (s, r) = call(&restricted, &f, TARGETS, &json!({"kind":kind,"limit":20})).await;
        assert_eq!(s, 200, "{r}");
    }
    for sql in [
        "SELECT * FROM approvals",
        "SELECT * FROM governance_operations",
        "SELECT * FROM guardrail_grants",
        "SELECT token_hash FROM output_scanner_tokens",
        "SELECT * FROM human_assertions",
        "SELECT * FROM output_scans",
        "SELECT credential_env FROM endpoints",
        "UPDATE console_permissions SET enabled=true",
        "UPDATE runtime_bindings SET active=true",
        "DELETE FROM guardrails",
        "UPDATE guardrail_versions SET name='changed'",
    ] {
        assert!(
            sqlx::query(sql).execute(&restricted).await.is_err(),
            "{sql}"
        );
    }
    let operator = scoped(&pool, "brokerrouter_operator").await;
    human::publish_guardrail(
        &operator,
        f.tenant,
        governance::Guardrail {
            id: "risk".into(),
            version: 3,
            name: "CLI".into(),
            stage: "tool_input".into(),
            rule: json!({"kind":"always"}),
        },
    )
    .await
    .unwrap();
    assert_eq!(
        call(&restricted, &f, APPLY, &rail("risk", Some("2"), false))
            .await
            .0,
        409
    );
    restricted.close().await;
    operator.close().await;
}
