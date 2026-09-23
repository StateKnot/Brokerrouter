mod common;

use brokerrouter::{
    error::Error,
    mcp_catalog::{self, CacheScope, RecordSkillGet},
    mcp_client::InjectedCredentialResolver,
    mcp_skill_approvals::{self, Decide, Request as ConsentRequest, Review},
    mcp_skill_resources::{self, ReadRequest, Worker},
    store::{self, Identity},
};
use common::*;
use serde_json::{Value, json};
use sqlx::{PgPool, postgres::PgPoolOptions};
use std::sync::Arc;
use uuid::Uuid;

const URI: &str = "skill://reviewed/SKILL.md";
const CONTENT: &str = "---\nname: reviewed\ndescription: A reviewed workflow\n---\nRead only.";

async fn human_proof(
    pool: &PgPool,
    tenant: Uuid,
    path: &str,
    body: Value,
) -> brokerrouter::human::Human {
    let token = approvals_sign(claims(tenant, "alice", path, &body));
    approvals_verifier()
        .verify(
            pool,
            &token,
            "POST",
            path,
            &serde_json::to_vec(&body).unwrap(),
        )
        .await
        .unwrap()
}

#[sqlx::test(migrations = "./migrations")]
async fn approved_resource_is_verified_encrypted_and_revoked_with_manifest(pool: PgPool) {
    let tenant = store::create_tenant(&pool, "Skill read", 1_000_000)
        .await
        .unwrap();
    store::put_model(&pool, tenant, "model", "model")
        .await
        .unwrap();
    let (key_id, _) =
        store::create_key(&pool, tenant, "runtime", 1_000_000, &["model".into()], None)
            .await
            .unwrap();
    let who = Identity {
        tenant_id: tenant,
        key_id,
    };
    provision(&pool, &who, "alice").await;
    let turn_id = turn(&pool, &who, "alice", 0).await;
    let server_id = Uuid::new_v4();
    sqlx::query("INSERT INTO mcp_servers(tenant_id,id,display_name,transport,endpoint_url,allow_private,auth_mode,active) VALUES($1,$2,'fixture','streamable_http',$3,true,'none',true)")
        .bind(tenant).bind(server_id).bind("https://example.org/mcp").execute(&pool).await.unwrap();
    let entry = json!({"uri":URI,"frontmatter":{"name":"reviewed","description":"A reviewed workflow"},
        "resources":[{"uri":URI,"digest":format!("sha256:{}",hex(&store::hash(CONTENT.as_bytes()))),"size":CONTENT.len()}]});
    let record = |entry: Value| RecordSkillGet {
        tenant_id: tenant,
        server_id,
        operation_id: Uuid::new_v4(),
        actor: "mcp-connector:test".into(),
        expected_server_revision: 1,
        protocol_version: mcp_catalog::PROTOCOL_VERSION.into(),
        result_type: "complete".into(),
        supported_versions: vec![mcp_catalog::PROTOCOL_VERSION.into()],
        capabilities: json!({"resources":{},"extensions":{"io.modelcontextprotocol/skills":{}}}),
        discovery_ttl_ms: 120_000,
        discovery_cache_scope: CacheScope::Public,
        requested_uri: URI.into(),
        entry,
        skill_ttl_ms: 120_000,
        skill_cache_scope: CacheScope::Public,
        authorization_context_fingerprint: None,
    };
    mcp_catalog::record_skill_get(&pool, record(entry.clone()))
        .await
        .unwrap();
    let request_id = Uuid::new_v4();
    mcp_skill_approvals::request(
        &pool,
        &who,
        ConsentRequest {
            request_id,
            turn_id,
            server_id,
            uri: URI.into(),
        },
    )
    .await
    .unwrap();
    let read_id = Uuid::new_v4();
    assert!(matches!(
        mcp_skill_resources::request(
            &pool,
            &who,
            request_id,
            ReadRequest {
                read_id,
                turn_id,
                uri: URI.into(),
            }
        )
        .await,
        Err(Error::ActionRejected)
    ));
    let review_body = json!({"request_id":request_id});
    let reviewed = mcp_skill_approvals::review(
        &pool,
        &human_proof(&pool, tenant, "/approvals/v1/skills/review", review_body).await,
        Review { request_id },
    )
    .await
    .unwrap();
    let decision = Decide {
        request_id,
        review_id: Uuid::parse_str(reviewed["review_id"].as_str().unwrap()).unwrap(),
        acknowledged_scope: true,
        decision: "approve_this_turn".into(),
        feedback: None,
    };
    let decision_body = serde_json::to_value(&decision).unwrap();
    mcp_skill_approvals::decide(
        &pool,
        &cipher(),
        &human_proof(&pool, tenant, "/approvals/v1/skills/decide", decision_body).await,
        decision,
    )
    .await
    .unwrap();
    sqlx::raw_sql(include_str!("../deploy/permissions.sql"))
        .execute(&pool)
        .await
        .unwrap();
    let runtime = PgPoolOptions::new()
        .max_connections(1)
        .after_connect(|c, _| {
            Box::pin(async move {
                sqlx::query("SET SESSION AUTHORIZATION brokerrouter_runtime")
                    .execute(c)
                    .await?;
                Ok(())
            })
        })
        .connect_with(pool.connect_options().as_ref().clone())
        .await
        .unwrap();
    let worker_pool = PgPoolOptions::new()
        .max_connections(1)
        .after_connect(|c, _| {
            Box::pin(async move {
                sqlx::query("SET SESSION AUTHORIZATION brokerrouter_mcp_worker")
                    .execute(c)
                    .await?;
                Ok(())
            })
        })
        .connect_with(pool.connect_options().as_ref().clone())
        .await
        .unwrap();
    brokerrouter::check_schema(&runtime).await.unwrap();
    brokerrouter::check_schema(&worker_pool).await.unwrap();
    assert_eq!(
        mcp_skill_approvals::status(&runtime, &cipher(), &who, request_id)
            .await
            .unwrap()["state"],
        "approved"
    );
    let queued = mcp_skill_resources::request(
        &runtime,
        &who,
        request_id,
        ReadRequest {
            read_id,
            turn_id,
            uri: URI.into(),
        },
    )
    .await
    .unwrap();
    assert_eq!(queued["state"], "queued");
    assert!(queued.get("text").is_none());
    assert!(matches!(
        mcp_skill_resources::request(
            &runtime,
            &who,
            request_id,
            ReadRequest {
                read_id: Uuid::new_v4(),
                turn_id,
                uri: "skill://reviewed/missing".into()
            }
        )
        .await,
        Err(Error::Invalid(_))
    ));
    // Model the isolated worker's two legal transitions without weakening the
    // production HTTPS database constraint for an in-process HTTP fixture.
    let claim = Uuid::new_v4();
    sqlx::query("UPDATE mcp_skill_resource_reads SET state='reading',claim_id=$3,lease_until=clock_timestamp()+interval '60 seconds' WHERE tenant_id=$1 AND id=$2")
        .bind(tenant).bind(read_id).bind(claim).execute(&pool).await.unwrap();
    sqlx::query("UPDATE mcp_skill_resource_reads SET state='ready',claim_id=NULL,lease_until=NULL,content_sha256=$3,content_ciphertext=$4,content_kind='text',mime_type='text/markdown',resource_valid_until=clock_timestamp()+interval '60 seconds',completed_at=clock_timestamp() WHERE tenant_id=$1 AND id=$2")
        .bind(tenant).bind(read_id).bind(store::hash(CONTENT.as_bytes()))
        .bind(cipher().encrypt(tenant,read_id,CONTENT.as_bytes()).unwrap()).execute(&pool).await.unwrap();
    let ready = mcp_skill_resources::status(&runtime, &cipher(), &who, read_id)
        .await
        .unwrap();
    assert_eq!(ready["state"], "ready");
    assert_eq!(ready["text"], CONTENT);
    assert_eq!(ready["activation"], "disabled");
    assert!(
        sqlx::query("UPDATE mcp_skill_resource_reads SET state='failed'")
            .execute(&runtime)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("SELECT feedback_ciphertext FROM mcp_skill_approval_requests")
            .execute(&worker_pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("INSERT INTO mcp_skill_resource_reads DEFAULT VALUES")
            .execute(&worker_pool)
            .await
            .is_err()
    );
    let encrypted: Vec<u8> = sqlx::query_scalar(
        "SELECT content_ciphertext FROM mcp_skill_resource_reads WHERE tenant_id=$1 AND id=$2",
    )
    .bind(tenant)
    .bind(read_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        !encrypted
            .windows(CONTENT.len())
            .any(|part| part == CONTENT.as_bytes())
    );
    assert!(
        sqlx::query(
            "UPDATE mcp_skill_resource_reads SET content_ciphertext=$3 WHERE tenant_id=$1 AND id=$2"
        )
        .bind(tenant)
        .bind(read_id)
        .bind(vec![1, 2, 3])
        .execute(&pool)
        .await
        .is_err()
    );
    for (bytes, ttl, expected_error) in [
        (b"tampered".as_slice(), "60 seconds", "content"),
        (CONTENT.as_bytes(), "-1 second", "ttl"),
    ] {
        let id = Uuid::new_v4();
        mcp_skill_resources::request(
            &runtime,
            &who,
            request_id,
            ReadRequest {
                read_id: id,
                turn_id,
                uri: URI.into(),
            },
        )
        .await
        .unwrap();
        sqlx::query("UPDATE mcp_skill_resource_reads SET state='reading',claim_id=$3,lease_until=clock_timestamp()+interval '60 seconds' WHERE tenant_id=$1 AND id=$2")
            .bind(tenant).bind(id).bind(Uuid::new_v4()).execute(&pool).await.unwrap();
        sqlx::query("UPDATE mcp_skill_resource_reads SET state='ready',claim_id=NULL,lease_until=NULL,content_sha256=$3,content_ciphertext=$4,content_kind='text',resource_valid_until=clock_timestamp()+($5::text)::interval,completed_at=clock_timestamp() WHERE tenant_id=$1 AND id=$2")
            .bind(tenant).bind(id).bind(store::hash(bytes))
            .bind(cipher().encrypt(tenant,id,bytes).unwrap()).bind(ttl).execute(&pool).await.unwrap();
        assert!(
            matches!(
                mcp_skill_resources::status(&runtime, &cipher(), &who, id).await,
                Err(Error::ResultUnavailable)
            ),
            "{expected_error}"
        );
    }
    let denied_read_id = Uuid::new_v4();
    mcp_skill_resources::request(
        &runtime,
        &who,
        request_id,
        ReadRequest {
            read_id: denied_read_id,
            turn_id,
            uri: URI.into(),
        },
    )
    .await
    .unwrap();
    let mut changed = entry;
    changed["resources"][0]["digest"] = json!(format!("sha256:{}", "a".repeat(64)));
    mcp_catalog::record_skill_get(&pool, record(changed))
        .await
        .unwrap();
    assert!(matches!(
        mcp_skill_resources::status(&runtime, &cipher(), &who, read_id).await,
        Err(Error::PolicyChanged)
    ));
    let worker = Worker::new(
        worker_pool.clone(),
        Arc::new(cipher()),
        Arc::new(cipher()),
        InjectedCredentialResolver::from_env().unwrap(),
    );
    assert!(worker.tick().await.unwrap());
    let failed: String = sqlx::query_scalar(
        "SELECT state FROM mcp_skill_resource_reads WHERE tenant_id=$1 AND id=$2",
    )
    .bind(tenant)
    .bind(denied_read_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(failed, "failed");
}
