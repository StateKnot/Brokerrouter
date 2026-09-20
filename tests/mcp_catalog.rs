use brokerrouter::{
    cipher::ResultCipher,
    error::Error,
    mcp_catalog::{
        self, Authentication, CacheScope, CredentialReference, RecordDiscovery, RegisterServer,
        SetToolGrant, Transport, UpdateServer,
    },
    store,
};
use serde_json::json;
use sqlx::{PgPool, postgres::PgPoolOptions};
use std::collections::HashMap;
use uuid::Uuid;
const AUTH_CONTEXT: &str = "1111111111111111111111111111111111111111111111111111111111111111";

fn cipher() -> ResultCipher {
    ResultCipher::new(
        "mcp-v1".into(),
        HashMap::from([("mcp-v1".into(), "73".repeat(32))]),
    )
    .unwrap()
}

async fn tenant(pool: &PgPool, name: &str) -> Uuid {
    store::create_tenant(pool, name, 0).await.unwrap()
}

fn registration(tenant_id: Uuid) -> RegisterServer {
    RegisterServer {
        tenant_id,
        operation_id: Uuid::new_v4(),
        actor: "operator:alice".into(),
        reason: "reviewed initial MCP connection".into(),
        display_name: "Primary tools".into(),
        transport: Transport::StreamableHttp {
            endpoint_url: "https://mcp.example/mcp".into(),
            allow_private: false,
        },
        authentication: Authentication::Bearer {
            credential_reference: CredentialReference::Environment {
                name: "MCP_PRIMARY_TOKEN".into(),
            },
        },
    }
}

fn update(tenant_id: Uuid, server_id: Uuid, revision: i64) -> UpdateServer {
    UpdateServer {
        tenant_id,
        server_id,
        operation_id: Uuid::new_v4(),
        actor: "operator:alice".into(),
        reason: "reviewed activation with OAuth binding".into(),
        expected_revision: revision,
        display_name: "Primary tools".into(),
        transport: Transport::StreamableHttp {
            endpoint_url: "https://mcp.example/mcp".into(),
            allow_private: false,
        },
        authentication: Authentication::OauthClientCredentials {
            credential_reference: CredentialReference::SecretManager {
                uri: "vault://production/mcp/primary".into(),
            },
            issuer: "https://identity.example/tenant".into(),
            resource: "https://mcp.example/mcp".into(),
            audience: "https://mcp.example/mcp".into(),
        },
        active: true,
    }
}

fn discovery(tenant_id: Uuid, server_id: Uuid, revision: i64) -> RecordDiscovery {
    RecordDiscovery {
        tenant_id,
        server_id,
        operation_id: Uuid::new_v4(),
        actor: "mcp-connector:fixture".into(),
        expected_server_revision: revision,
        protocol_version: mcp_catalog::PROTOCOL_VERSION.into(),
        result_type: "complete".into(),
        supported_versions: vec![mcp_catalog::PROTOCOL_VERSION.into()],
        capabilities: json!({"tools":{},"resources":{},"extensions":{"io.modelcontextprotocol/skills":{}}}),
        server_info: Some(json!({"name":"untrusted-name","version":"9.9"})),
        instructions: Some("Untrusted display instructions only.".into()),
        ttl_ms: 60_000,
        cache_scope: CacheScope::Private,
        tools: vec![json!({
            "name":"read_record",
            "description":"Read one reviewed record",
            "inputSchema":{"type":"object","properties":{"id":{"type":"string"}},"required":["id"],"additionalProperties":false}
        })],
        tools_ttl_ms: Some(30_000),
        tools_cache_scope: Some(CacheScope::Private),
        authorization_context_fingerprint: Some(AUTH_CONTEXT.into()),
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn host_identity_encrypted_reference_idempotency_and_tenant_boundary(pool: PgPool) {
    let tenant_id = tenant(&pool, "mcp-a").await;
    let input = registration(tenant_id);
    let encryption = cipher();
    let (left, right) = tokio::join!(
        mcp_catalog::register_server(&pool, &encryption, input.clone()),
        mcp_catalog::register_server(&pool, &encryption, input.clone())
    );
    let left = left.unwrap();
    assert_eq!(left, right.unwrap());
    assert_eq!(left["active"], false);
    let server_id: Uuid = serde_json::from_value(left["server_id"].clone()).unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM mcp_servers")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
    let envelope: Vec<u8> = sqlx::query_scalar(
        "SELECT credential_ref_ciphertext FROM mcp_servers WHERE tenant_id=$1 AND id=$2",
    )
    .bind(tenant_id)
    .bind(server_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(!String::from_utf8_lossy(&envelope).contains("MCP_PRIMARY_TOKEN"));
    assert!(matches!(
        mcp_catalog::connector_server(&pool, &cipher(), tenant_id, server_id).await,
        Err(Error::NotFound)
    ));
    let other = tenant(&pool, "mcp-b").await;
    assert!(matches!(
        mcp_catalog::connector_server(&pool, &cipher(), other, server_id).await,
        Err(Error::NotFound)
    ));
    let mut clash = input;
    clash.reason = "different body under same operation id".into();
    assert!(matches!(
        mcp_catalog::register_server(&pool, &cipher(), clash).await,
        Err(Error::Conflict)
    ));
}

#[sqlx::test(migrations = "./migrations")]
async fn oauth_revision_discovery_cache_and_self_report_are_not_identity(pool: PgPool) {
    let tenant_id = tenant(&pool, "mcp-discovery").await;
    let registered = mcp_catalog::register_server(&pool, &cipher(), registration(tenant_id))
        .await
        .unwrap();
    let server_id: Uuid = serde_json::from_value(registered["server_id"].clone()).unwrap();
    let activated = mcp_catalog::update_server(&pool, &cipher(), update(tenant_id, server_id, 1))
        .await
        .unwrap();
    assert_eq!(activated["revision"], "2");
    let server = mcp_catalog::connector_server(&pool, &cipher(), tenant_id, server_id)
        .await
        .unwrap();
    assert_eq!(server.id, server_id);
    assert_eq!(server.config_revision, 2);
    assert_eq!(server.oauth_audience, server.oauth_resource);
    assert_eq!(
        server.credential_reference,
        Some(CredentialReference::SecretManager {
            uri: "vault://production/mcp/primary".into()
        })
    );

    let observation = discovery(tenant_id, server_id, 2);
    let discovery_operation = observation.operation_id;
    let first = mcp_catalog::record_discovery(&pool, observation.clone())
        .await
        .unwrap();
    assert_eq!(
        first,
        mcp_catalog::record_discovery(&pool, observation)
            .await
            .unwrap()
    );
    assert_eq!(
        mcp_catalog::discovery_receipt(&pool, tenant_id, server_id, discovery_operation)
            .await
            .unwrap(),
        Some(first.clone())
    );
    assert!(matches!(
        mcp_catalog::discovery_receipt(&pool, tenant_id, Uuid::new_v4(), discovery_operation).await,
        Err(Error::Conflict)
    ));
    assert!(matches!(
        mcp_catalog::current_discovery(&pool, tenant_id, server_id, None).await,
        Err(Error::NotFound)
    ));
    assert!(matches!(
        mcp_catalog::current_discovery(
            &pool,
            tenant_id,
            server_id,
            Some("2222222222222222222222222222222222222222222222222222222222222222")
        )
        .await,
        Err(Error::NotFound)
    ));
    let current = mcp_catalog::current_discovery(&pool, tenant_id, server_id, Some(AUTH_CONTEXT))
        .await
        .unwrap();
    assert_eq!(current["server_id"], server_id.to_string());
    assert_eq!(current["server_info"]["name"], "untrusted-name");
    assert_ne!(current["server_info"]["name"], current["server_id"]);
    assert_eq!(current["result_type"], "complete");
    assert_eq!(current["cache_scope"], "private");
    let context: Option<Vec<u8>> = sqlx::query_scalar(
        "SELECT authorization_context_hash FROM mcp_discovery_snapshots WHERE tenant_id=$1 AND id=$2",
    )
    .bind(tenant_id)
    .bind(serde_json::from_value::<Uuid>(first["snapshot_id"].clone()).unwrap())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(context.unwrap().len(), 32);

    let review = mcp_catalog::review_tools(&pool, tenant_id, server_id)
        .await
        .unwrap();
    let downstream_name = review["tools"][0]["downstream_name"]
        .as_str()
        .unwrap()
        .to_owned();
    let descriptor_sha256 = review["tools"][0]["descriptor_sha256"]
        .as_str()
        .unwrap()
        .to_owned();
    store::put_model(&pool, tenant_id, "test/model", "Test model")
        .await
        .unwrap();
    let (key_id, _) = store::create_key(
        &pool,
        tenant_id,
        "MCP tool key",
        0,
        &["test/model".into()],
        None,
    )
    .await
    .unwrap();
    mcp_catalog::set_tool_grant(
        &pool,
        SetToolGrant {
            tenant_id,
            key_id,
            server_id,
            operation_id: Uuid::new_v4(),
            actor: "operator:alice".into(),
            reason: "reviewed exact tool descriptor".into(),
            downstream_name: downstream_name.clone(),
            descriptor_sha256,
            active: true,
        },
    )
    .await
    .unwrap();
    let who = store::Identity { tenant_id, key_id };
    assert_eq!(
        mcp_catalog::authorized_tools(&pool, &who, None, 101)
            .await
            .unwrap()
            .len(),
        1
    );

    let mut changed_descriptor = discovery(tenant_id, server_id, 2);
    changed_descriptor.tools[0]["description"] = json!("Changed upstream meaning");
    mcp_catalog::record_discovery(&pool, changed_descriptor)
        .await
        .unwrap();
    assert!(
        mcp_catalog::authorized_tools(&pool, &who, None, 101)
            .await
            .unwrap()
            .is_empty(),
        "a changed descriptor must not inherit the old grant"
    );

    let changed = UpdateServer {
        expected_revision: 2,
        reason: "disable after review".into(),
        active: false,
        ..update(tenant_id, server_id, 2)
    };
    let changed = mcp_catalog::update_server(&pool, &cipher(), changed)
        .await
        .unwrap();
    assert_eq!(changed["revision"], "3");
    assert!(matches!(
        mcp_catalog::current_discovery(&pool, tenant_id, server_id, Some(AUTH_CONTEXT)).await,
        Err(Error::NotFound)
    ));
}

#[sqlx::test(migrations = "./migrations")]
async fn invalid_transport_oauth_version_metadata_and_stale_revision_fail_closed(pool: PgPool) {
    let tenant_id = tenant(&pool, "mcp-validation").await;
    let mut invalid = registration(tenant_id);
    invalid.transport = Transport::StreamableHttp {
        endpoint_url: "http://127.0.0.1:3000/mcp".into(),
        allow_private: true,
    };
    assert!(matches!(
        mcp_catalog::register_server(&pool, &cipher(), invalid).await,
        Err(Error::Invalid(_))
    ));
    let mut invalid_stdio = registration(tenant_id);
    invalid_stdio.display_name = "Unsafe local process".into();
    invalid_stdio.transport = Transport::Stdio {
        launcher_profile: "../bin/sh".into(),
    };
    invalid_stdio.authentication = Authentication::None;
    assert!(matches!(
        mcp_catalog::register_server(&pool, &cipher(), invalid_stdio).await,
        Err(Error::Invalid(_))
    ));
    let mut stdio = registration(tenant_id);
    stdio.operation_id = Uuid::new_v4();
    stdio.display_name = "Approved local tools".into();
    stdio.transport = Transport::Stdio {
        launcher_profile: "approved/toolset-v1".into(),
    };
    stdio.authentication = Authentication::None;
    mcp_catalog::register_server(&pool, &cipher(), stdio)
        .await
        .unwrap();
    let mut stdio_with_http_auth = registration(tenant_id);
    stdio_with_http_auth.operation_id = Uuid::new_v4();
    stdio_with_http_auth.display_name = "Ambiguous stdio credentials".into();
    stdio_with_http_auth.transport = Transport::Stdio {
        launcher_profile: "approved/toolset-v1".into(),
    };
    assert!(matches!(
        mcp_catalog::register_server(&pool, &cipher(), stdio_with_http_auth).await,
        Err(Error::Invalid(_))
    ));
    let mut invalid = registration(tenant_id);
    invalid.authentication = Authentication::OauthClientCredentials {
        credential_reference: CredentialReference::Environment {
            name: "MCP_CLIENT_SECRET".into(),
        },
        issuer: "https://identity.example".into(),
        resource: "https://mcp.example/mcp".into(),
        audience: "https://different.example".into(),
    };
    assert!(matches!(
        mcp_catalog::register_server(&pool, &cipher(), invalid).await,
        Err(Error::Invalid(_))
    ));
    let registered = mcp_catalog::register_server(&pool, &cipher(), registration(tenant_id))
        .await
        .unwrap();
    let server_id: Uuid = serde_json::from_value(registered["server_id"].clone()).unwrap();
    mcp_catalog::update_server(&pool, &cipher(), update(tenant_id, server_id, 1))
        .await
        .unwrap();
    let mut wrong = discovery(tenant_id, server_id, 2);
    wrong.protocol_version = "2025-11-25".into();
    assert!(matches!(
        mcp_catalog::record_discovery(&pool, wrong).await,
        Err(Error::Invalid(_))
    ));
    let mut incomplete = discovery(tenant_id, server_id, 2);
    incomplete.result_type = "input_required".into();
    assert!(matches!(
        mcp_catalog::record_discovery(&pool, incomplete).await,
        Err(Error::Invalid(_))
    ));
    let mut oversized = discovery(tenant_id, server_id, 2);
    oversized.capabilities = json!({"tools":{"description":"x".repeat(300_000)}});
    assert!(matches!(
        mcp_catalog::record_discovery(&pool, oversized).await,
        Err(Error::Invalid(_))
    ));
    let mut invalid_header = discovery(tenant_id, server_id, 2);
    invalid_header.tools[0]["inputSchema"]["properties"]["id"]["x-mcp-header"] = json!("not valid");
    assert!(matches!(
        mcp_catalog::record_discovery(&pool, invalid_header).await,
        Err(Error::Invalid(_))
    ));
    let mut stale = discovery(tenant_id, server_id, 1);
    assert!(matches!(
        mcp_catalog::record_discovery(&pool, stale.clone()).await,
        Err(Error::ConfigConflict)
    ));
    stale.expected_server_revision = 2;
    stale.cache_scope = CacheScope::Public;
    assert!(matches!(
        mcp_catalog::record_discovery(&pool, stale.clone()).await,
        Err(Error::Invalid(_))
    ));
    stale.operation_id = Uuid::new_v4();
    stale.authorization_context_fingerprint = None;
    let public = mcp_catalog::record_discovery(&pool, stale).await.unwrap();
    let context: Option<Vec<u8>> = sqlx::query_scalar(
        "SELECT authorization_context_hash FROM mcp_discovery_snapshots WHERE tenant_id=$1 AND id=$2",
    )
    .bind(tenant_id)
    .bind(serde_json::from_value::<Uuid>(public["snapshot_id"].clone()).unwrap())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(context.is_none());
    sqlx::query("UPDATE tenants SET active=false WHERE id=$1")
        .bind(tenant_id)
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        mcp_catalog::connector_server(&pool, &cipher(), tenant_id, server_id).await,
        Err(Error::NotFound)
    ));
}

#[sqlx::test(migrations = "./migrations")]
async fn catalog_audit_failure_rolls_back_configuration_and_receipt(pool: PgPool) {
    let tenant_id = tenant(&pool, "mcp-rollback").await;
    sqlx::raw_sql("CREATE FUNCTION fail_mcp_audit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.action LIKE 'mcp.%' THEN RAISE EXCEPTION 'fixture'; END IF; RETURN NEW; END $$; CREATE TRIGGER fail_mcp_audit BEFORE INSERT ON audit_events FOR EACH ROW EXECUTE FUNCTION fail_mcp_audit();")
        .execute(&pool).await.unwrap();
    let input = registration(tenant_id);
    assert!(
        mcp_catalog::register_server(&pool, &cipher(), input.clone())
            .await
            .is_err()
    );
    for table in ["mcp_servers", "mcp_catalog_mutations"] {
        assert_eq!(
            sqlx::query_scalar::<_, i64>(&format!("SELECT count(*) FROM {table}"))
                .fetch_one(&pool)
                .await
                .unwrap(),
            0
        );
    }
    sqlx::query("DROP TRIGGER fail_mcp_audit ON audit_events")
        .execute(&pool)
        .await
        .unwrap();
    mcp_catalog::register_server(&pool, &cipher(), input)
        .await
        .unwrap();
}

async fn scoped(pool: &PgPool, role: &'static str) -> PgPool {
    PgPoolOptions::new()
        .max_connections(2)
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

#[sqlx::test(migrations = "./migrations")]
async fn deployment_roles_separate_configuration_discovery_and_other_authority(pool: PgPool) {
    sqlx::raw_sql(include_str!("../deploy/permissions.sql"))
        .execute(&pool)
        .await
        .unwrap();
    let tenant_id = tenant(&pool, "mcp-roles").await;
    store::put_model(&pool, tenant_id, "test/model", "Test model")
        .await
        .unwrap();
    let (key_id, token) = store::create_key(
        &pool,
        tenant_id,
        "MCP edge key",
        0,
        &["test/model".into()],
        None,
    )
    .await
    .unwrap();
    let catalog = scoped(&pool, "brokerrouter_mcp_catalog").await;
    brokerrouter::check_schema(&catalog).await.unwrap();
    let registered = mcp_catalog::register_server(&catalog, &cipher(), registration(tenant_id))
        .await
        .unwrap();
    let server_id: Uuid = serde_json::from_value(registered["server_id"].clone()).unwrap();
    mcp_catalog::update_server(&catalog, &cipher(), update(tenant_id, server_id, 1))
        .await
        .unwrap();
    assert!(
        sqlx::query("INSERT INTO mcp_discovery_snapshots(tenant_id,id,server_id,server_config_revision,protocol_version,result_type,supported_versions,capabilities,capability_hash,ttl_ms,cache_scope,authorization_context_hash,expires_at) VALUES($1,$2,$3,2,'2026-07-28','complete','[]','{}',$4,0,'private',$4,now())")
            .bind(tenant_id).bind(Uuid::new_v4()).bind(server_id).bind(vec![0_u8;32]).execute(&catalog).await.is_err()
    );
    for sql in [
        "SELECT key_hash FROM virtual_keys",
        "SELECT * FROM approvals",
        "SET ROLE brokerrouter_mcp_client",
    ] {
        assert!(sqlx::query(sql).execute(&catalog).await.is_err(), "{sql}");
    }
    let client = scoped(&pool, "brokerrouter_mcp_client").await;
    brokerrouter::check_schema(&client).await.unwrap();
    mcp_catalog::connector_server(&client, &cipher(), tenant_id, server_id)
        .await
        .unwrap();
    mcp_catalog::record_discovery(&client, discovery(tenant_id, server_id, 2))
        .await
        .unwrap();
    for sql in [
        "UPDATE mcp_servers SET active=false",
        "DELETE FROM mcp_discovery_snapshots",
        "SELECT result_ciphertext FROM requests",
        "SELECT * FROM approvals",
        "SET ROLE brokerrouter_mcp_catalog",
    ] {
        assert!(sqlx::query(sql).execute(&client).await.is_err(), "{sql}");
    }
    client.close().await;

    let reviewed = mcp_catalog::review_tools(&catalog, tenant_id, server_id)
        .await
        .unwrap();
    mcp_catalog::set_tool_grant(
        &catalog,
        SetToolGrant {
            tenant_id,
            key_id,
            server_id,
            operation_id: Uuid::new_v4(),
            actor: "operator:roles".into(),
            reason: "role boundary fixture".into(),
            downstream_name: reviewed["tools"][0]["downstream_name"]
                .as_str()
                .unwrap()
                .into(),
            descriptor_sha256: reviewed["tools"][0]["descriptor_sha256"]
                .as_str()
                .unwrap()
                .into(),
            active: true,
        },
    )
    .await
    .unwrap();
    catalog.close().await;

    let server_role = scoped(&pool, "brokerrouter_mcp_server").await;
    brokerrouter::check_schema(&server_role).await.unwrap();
    let who = store::authenticate(&server_role, &token).await.unwrap();
    assert_eq!(
        mcp_catalog::authorized_tools(&server_role, &who, None, 101)
            .await
            .unwrap()
            .len(),
        1
    );
    for sql in [
        "SELECT credential_ref_ciphertext FROM mcp_servers",
        "SELECT * FROM approvals",
        "UPDATE mcp_key_tool_grants SET active=false",
        "SET ROLE brokerrouter_mcp_client",
    ] {
        assert!(
            sqlx::query(sql).execute(&server_role).await.is_err(),
            "{sql}"
        );
    }
}
