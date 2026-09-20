//! Tenant-scoped MCP connection registry and immutable discovery observations.
//!
//! This module owns host-assigned server identity. Remote `serverInfo` is
//! retained only as untrusted display metadata and never participates in
//! routing, authorization, cache partitioning, or approval identity.
use crate::{
    cipher::ResultCipher,
    error::{Error, Result},
    governance, store,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, Transaction};
use std::collections::HashSet;
use url::Url;
use uuid::Uuid;

pub const PROTOCOL_VERSION: &str = "2026-07-28";
const MAX_CAPABILITIES_BYTES: usize = 256 * 1024;
const MAX_SERVER_INFO_BYTES: usize = 16 * 1024;
const MAX_JSON_DEPTH: usize = 32;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Transport {
    StreamableHttp {
        endpoint_url: String,
        #[serde(default)]
        allow_private: bool,
    },
    /// A deployment-owned launcher profile, never a caller supplied command.
    Stdio { launcher_profile: String },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum CredentialReference {
    Environment {
        name: String,
    },
    /// URI understood by the deployment secret resolver. Userinfo, query and
    /// fragment are forbidden so a secret value cannot be smuggled in here.
    SecretManager {
        uri: String,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Authentication {
    None,
    Bearer {
        credential_reference: CredentialReference,
    },
    OauthClientCredentials {
        credential_reference: CredentialReference,
        issuer: String,
        resource: String,
        audience: String,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RegisterServer {
    pub tenant_id: Uuid,
    pub operation_id: Uuid,
    pub actor: String,
    pub reason: String,
    pub display_name: String,
    pub transport: Transport,
    pub authentication: Authentication,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateServer {
    pub tenant_id: Uuid,
    pub server_id: Uuid,
    pub operation_id: Uuid,
    pub actor: String,
    pub reason: String,
    pub expected_revision: i64,
    pub display_name: String,
    pub transport: Transport,
    pub authentication: Authentication,
    pub active: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RecordDiscovery {
    pub tenant_id: Uuid,
    pub server_id: Uuid,
    pub operation_id: Uuid,
    pub actor: String,
    pub expected_server_revision: i64,
    pub protocol_version: String,
    pub result_type: String,
    pub supported_versions: Vec<String>,
    pub capabilities: Value,
    /// Value of result `_meta.io.modelcontextprotocol/serverInfo`.
    pub server_info: Option<Value>,
    pub instructions: Option<String>,
    pub ttl_ms: i32,
    pub cache_scope: CacheScope,
    /// Lower-hex SHA-256 of the effective upstream auth context. The client
    /// derives it from the resolved credential/token; the credential itself is
    /// never persisted in this request or snapshot.
    pub authorization_context_fingerprint: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheScope {
    Private,
    Public,
}
impl CacheScope {
    fn name(self) -> &'static str {
        match self {
            Self::Private => "private",
            Self::Public => "public",
        }
    }
}

/// Internal connector view. The credential reference is decrypted only by a
/// process with both this DB role and the dedicated MCP configuration key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectorServer {
    pub tenant_id: Uuid,
    pub id: Uuid,
    pub config_revision: i64,
    pub transport: String,
    pub endpoint_url: Option<String>,
    pub stdio_profile: Option<String>,
    pub allow_private: bool,
    pub auth_mode: String,
    pub credential_reference: Option<CredentialReference>,
    pub oauth_issuer: Option<String>,
    pub oauth_resource: Option<String>,
    pub oauth_audience: Option<String>,
}

struct ValidatedConfig {
    transport: &'static str,
    endpoint_url: Option<String>,
    stdio_profile: Option<String>,
    allow_private: bool,
    auth_mode: &'static str,
    credential: Option<CredentialReference>,
    credential_kind: Option<&'static str>,
    issuer: Option<String>,
    resource: Option<String>,
    audience: Option<String>,
}

fn valid_text(value: &str, max: usize) -> bool {
    governance::valid_text(value, max)
}

fn https_url(value: &str) -> Result<String> {
    if value.len() > 2048 {
        return Err(Error::Invalid("MCP URL is too long"));
    }
    let parsed = Url::parse(value).map_err(|_| Error::Invalid("invalid MCP HTTPS URL"))?;
    if parsed.scheme() != "https"
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(Error::Invalid("invalid MCP HTTPS URL"));
    }
    Ok(parsed.to_string())
}

fn credential(reference: &CredentialReference) -> Result<&'static str> {
    match reference {
        CredentialReference::Environment { name } => {
            if name.is_empty()
                || name.len() > 128
                || !name.as_bytes()[0].is_ascii_uppercase()
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
            {
                return Err(Error::Invalid("invalid credential environment reference"));
            }
            Ok("environment")
        }
        CredentialReference::SecretManager { uri } => {
            if uri.len() > 1024 {
                return Err(Error::Invalid("invalid secret manager reference"));
            }
            let parsed =
                Url::parse(uri).map_err(|_| Error::Invalid("invalid secret manager reference"))?;
            if !matches!(
                parsed.scheme(),
                "vault" | "aws-sm" | "gcp-sm" | "aliyun-kms"
            ) || parsed.host_str().is_none()
                || !parsed.username().is_empty()
                || parsed.password().is_some()
                || parsed.query().is_some()
                || parsed.fragment().is_some()
                || parsed.path().is_empty()
                || parsed.path().split('/').any(|p| p == "..")
            {
                return Err(Error::Invalid("invalid secret manager reference"));
            }
            Ok("secret_manager")
        }
    }
}

fn validate_config(transport: &Transport, auth: &Authentication) -> Result<ValidatedConfig> {
    let (transport_name, endpoint_url, stdio_profile, allow_private) = match transport {
        Transport::StreamableHttp {
            endpoint_url,
            allow_private,
        } => (
            "streamable_http",
            Some(https_url(endpoint_url)?),
            None,
            *allow_private,
        ),
        Transport::Stdio { launcher_profile } => {
            if !valid_text(launcher_profile, 200)
                || launcher_profile.starts_with('/')
                || launcher_profile
                    .split('/')
                    .any(|p| p.is_empty() || p == "..")
                || !launcher_profile.bytes().all(|b| {
                    b.is_ascii_lowercase()
                        || b.is_ascii_digit()
                        || matches!(b, b'-' | b'_' | b'.' | b'/')
                })
            {
                return Err(Error::Invalid("invalid stdio launcher profile"));
            }
            ("stdio", None, Some(launcher_profile.clone()), false)
        }
    };
    let (auth_mode, reference, kind, issuer, resource, audience) = match auth {
        Authentication::None => ("none", None, None, None, None, None),
        Authentication::Bearer {
            credential_reference,
        } => (
            "bearer",
            Some(credential_reference.clone()),
            Some(credential(credential_reference)?),
            None,
            None,
            None,
        ),
        Authentication::OauthClientCredentials {
            credential_reference,
            issuer,
            resource,
            audience,
        } => {
            let issuer = https_url(issuer)?;
            let resource = https_url(resource)?;
            if audience != &resource {
                return Err(Error::Invalid(
                    "OAuth audience must equal the canonical resource identifier",
                ));
            }
            (
                "oauth_client_credentials",
                Some(credential_reference.clone()),
                Some(credential(credential_reference)?),
                Some(issuer),
                Some(resource.clone()),
                Some(resource),
            )
        }
    };
    Ok(ValidatedConfig {
        transport: transport_name,
        endpoint_url,
        stdio_profile,
        allow_private,
        auth_mode,
        credential: reference,
        credential_kind: kind,
        issuer,
        resource,
        audience,
    })
}

fn json_depth(value: &Value, depth: usize) -> bool {
    if depth > MAX_JSON_DEPTH {
        return false;
    }
    match value {
        Value::Array(values) => values.iter().all(|v| json_depth(v, depth + 1)),
        Value::Object(values) => values.values().all(|v| json_depth(v, depth + 1)),
        _ => true,
    }
}

fn bounded_json(value: &Value, max: usize, object: bool) -> Result<()> {
    if (object && !value.is_object())
        || !json_depth(value, 0)
        || serde_json::to_vec(value)
            .map_err(|_| Error::Invalid("invalid MCP metadata"))?
            .len()
            > max
    {
        return Err(Error::Invalid("invalid or oversized MCP metadata"));
    }
    Ok(())
}

fn validate_common(actor: &str, reason: Option<&str>, display_name: Option<&str>) -> Result<()> {
    if !valid_text(actor, 200)
        || reason.is_some_and(|v| !valid_text(v, 500))
        || display_name.is_some_and(|v| !valid_text(v, 200))
    {
        return Err(Error::Invalid("invalid MCP catalog audit fields"));
    }
    Ok(())
}

fn valid_fingerprint(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

type Tx<'a> = Transaction<'a, Postgres>;

async fn begin(pool: &PgPool, tenant: Uuid, operation: Uuid) -> Result<Tx<'static>> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL statement_timeout='5s'")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SET LOCAL lock_timeout='1s'")
        .execute(&mut *tx)
        .await?;
    let active: Option<bool> =
        sqlx::query_scalar("SELECT active FROM tenants WHERE id=$1 FOR SHARE")
            .bind(tenant)
            .fetch_optional(&mut *tx)
            .await?;
    if active != Some(true) {
        return Err(Error::NotFound);
    }
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
        .bind(format!("mcp-catalog:{tenant}:{operation}"))
        .execute(&mut *tx)
        .await?;
    Ok(tx)
}

async fn previous(
    tx: &mut Tx<'_>,
    tenant: Uuid,
    operation: Uuid,
    hash: &[u8],
) -> Result<Option<Value>> {
    let row: Option<(Vec<u8>, Value)> = sqlx::query_as(
        "SELECT body_hash,response FROM mcp_catalog_mutations WHERE tenant_id=$1 AND operation_id=$2",
    )
    .bind(tenant)
    .bind(operation)
    .fetch_optional(&mut **tx)
    .await?;
    match row {
        Some((old, _)) if old != hash => Err(Error::Conflict),
        Some((_, response)) => Ok(Some(response)),
        None => Ok(None),
    }
}

async fn receipt(
    tx: &mut Tx<'_>,
    tenant: Uuid,
    operation: Uuid,
    actor: &str,
    kind: &str,
    hash: Vec<u8>,
    response: &Value,
) -> Result<()> {
    sqlx::query("INSERT INTO mcp_catalog_mutations(tenant_id,operation_id,actor,kind,body_hash,response) VALUES($1,$2,$3,$4,$5,$6)")
        .bind(tenant).bind(operation).bind(actor).bind(kind).bind(hash).bind(response).execute(&mut **tx).await?;
    Ok(())
}

fn encrypted_reference(
    cipher: &ResultCipher,
    tenant: Uuid,
    server: Uuid,
    config: &ValidatedConfig,
) -> Result<Option<Vec<u8>>> {
    config
        .credential
        .as_ref()
        .map(|r| {
            let bytes = serde_json::to_vec(r).map_err(|_| Error::Configuration)?;
            cipher.encrypt(tenant, server, &bytes)
        })
        .transpose()
}

pub async fn register_server(
    pool: &PgPool,
    cipher: &ResultCipher,
    input: RegisterServer,
) -> Result<Value> {
    validate_common(&input.actor, Some(&input.reason), Some(&input.display_name))?;
    let config = validate_config(&input.transport, &input.authentication)?;
    let value = serde_json::to_value(&input).map_err(|_| Error::Invalid("invalid MCP server"))?;
    let hash = governance::digest(&value)?;
    let mut tx = begin(pool, input.tenant_id, input.operation_id).await?;
    if let Some(response) = previous(&mut tx, input.tenant_id, input.operation_id, &hash).await? {
        tx.commit().await?;
        return Ok(response);
    }
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
        .bind(format!("mcp-server-names:{}", input.tenant_id))
        .execute(&mut *tx)
        .await?;
    let id = Uuid::new_v4();
    let encrypted = encrypted_reference(cipher, input.tenant_id, id, &config)?;
    let inserted = sqlx::query("INSERT INTO mcp_servers(tenant_id,id,display_name,transport,endpoint_url,stdio_profile,allow_private,auth_mode,credential_ref_kind,credential_ref_ciphertext,oauth_issuer,oauth_resource,oauth_audience) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13) ON CONFLICT DO NOTHING")
        .bind(input.tenant_id).bind(id).bind(&input.display_name).bind(config.transport).bind(&config.endpoint_url).bind(&config.stdio_profile).bind(config.allow_private).bind(config.auth_mode).bind(config.credential_kind).bind(encrypted).bind(&config.issuer).bind(&config.resource).bind(&config.audience).execute(&mut *tx).await?;
    if inserted.rows_affected() != 1 {
        return Err(Error::ConfigConflict);
    }
    let response =
        json!({"operation_id":input.operation_id,"server_id":id,"revision":"1","active":false});
    sqlx::query("INSERT INTO audit_events(tenant_id,action,object_id,details) VALUES($1,'mcp.server_registered',$2,$3)")
        .bind(input.tenant_id).bind(id.to_string()).bind(json!({"actor":input.actor,"operation_id":input.operation_id,"reason":input.reason,"transport":config.transport,"auth_mode":config.auth_mode,"revision":"1"})).execute(&mut *tx).await?;
    receipt(
        &mut tx,
        input.tenant_id,
        input.operation_id,
        &input.actor,
        "server_register",
        hash,
        &response,
    )
    .await?;
    tx.commit().await?;
    Ok(response)
}

pub async fn update_server(
    pool: &PgPool,
    cipher: &ResultCipher,
    input: UpdateServer,
) -> Result<Value> {
    validate_common(&input.actor, Some(&input.reason), Some(&input.display_name))?;
    if input.expected_revision < 1 {
        return Err(Error::Invalid("invalid MCP server revision"));
    }
    let config = validate_config(&input.transport, &input.authentication)?;
    let value = serde_json::to_value(&input).map_err(|_| Error::Invalid("invalid MCP server"))?;
    let hash = governance::digest(&value)?;
    let mut tx = begin(pool, input.tenant_id, input.operation_id).await?;
    if let Some(response) = previous(&mut tx, input.tenant_id, input.operation_id, &hash).await? {
        tx.commit().await?;
        return Ok(response);
    }
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
        .bind(format!(
            "mcp-server:{}:{}",
            input.tenant_id, input.server_id
        ))
        .execute(&mut *tx)
        .await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
        .bind(format!("mcp-server-names:{}", input.tenant_id))
        .execute(&mut *tx)
        .await?;
    let duplicate: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM mcp_servers WHERE tenant_id=$1 AND id<>$2 AND lower(display_name)=lower($3))")
        .bind(input.tenant_id).bind(input.server_id).bind(&input.display_name).fetch_one(&mut *tx).await?;
    if duplicate {
        return Err(Error::ConfigConflict);
    }
    let old:Option<(String,String,String,bool,i64)>=sqlx::query_as("SELECT display_name,transport,auth_mode,active,config_revision FROM mcp_servers WHERE tenant_id=$1 AND id=$2 FOR UPDATE")
        .bind(input.tenant_id).bind(input.server_id).fetch_optional(&mut *tx).await?;
    let (old_name, old_transport, old_auth, old_active, revision) = old.ok_or(Error::NotFound)?;
    if revision != input.expected_revision {
        return Err(Error::ConfigConflict);
    }
    let encrypted = encrypted_reference(cipher, input.tenant_id, input.server_id, &config)?;
    let revision:i64=sqlx::query_scalar("UPDATE mcp_servers SET display_name=$3,transport=$4,endpoint_url=$5,stdio_profile=$6,allow_private=$7,auth_mode=$8,credential_ref_kind=$9,credential_ref_ciphertext=$10,oauth_issuer=$11,oauth_resource=$12,oauth_audience=$13,active=$14 WHERE tenant_id=$1 AND id=$2 RETURNING config_revision")
        .bind(input.tenant_id).bind(input.server_id).bind(&input.display_name).bind(config.transport).bind(&config.endpoint_url).bind(&config.stdio_profile).bind(config.allow_private).bind(config.auth_mode).bind(config.credential_kind).bind(encrypted).bind(&config.issuer).bind(&config.resource).bind(&config.audience).bind(input.active).fetch_one(&mut *tx).await?;
    let response = json!({"operation_id":input.operation_id,"server_id":input.server_id,"revision":revision.to_string(),"active":input.active});
    sqlx::query("INSERT INTO audit_events(tenant_id,action,object_id,details) VALUES($1,'mcp.server_updated',$2,$3)")
        .bind(input.tenant_id).bind(input.server_id.to_string()).bind(json!({"actor":input.actor,"operation_id":input.operation_id,"reason":input.reason,"before":{"display_name":old_name,"transport":old_transport,"auth_mode":old_auth,"active":old_active,"revision":input.expected_revision.to_string()},"after":{"display_name":input.display_name,"transport":config.transport,"auth_mode":config.auth_mode,"active":input.active,"revision":revision.to_string()}})).execute(&mut *tx).await?;
    receipt(
        &mut tx,
        input.tenant_id,
        input.operation_id,
        &input.actor,
        "server_update",
        hash,
        &response,
    )
    .await?;
    tx.commit().await?;
    Ok(response)
}

pub async fn connector_server(
    pool: &PgPool,
    cipher: &ResultCipher,
    tenant: Uuid,
    server: Uuid,
) -> Result<ConnectorServer> {
    type Row = (
        Uuid,
        Uuid,
        i64,
        String,
        Option<String>,
        Option<String>,
        bool,
        String,
        Option<Vec<u8>>,
        Option<String>,
        Option<String>,
        Option<String>,
    );
    let row:Row=sqlx::query_as("SELECT s.tenant_id,s.id,s.config_revision,s.transport,s.endpoint_url,s.stdio_profile,s.allow_private,s.auth_mode,s.credential_ref_ciphertext,s.oauth_issuer,s.oauth_resource,s.oauth_audience FROM mcp_servers s JOIN tenants t ON t.id=s.tenant_id AND t.active WHERE s.tenant_id=$1 AND s.id=$2 AND s.active")
        .bind(tenant).bind(server).fetch_optional(pool).await?.ok_or(Error::NotFound)?;
    let credential_reference = row
        .8
        .map(|encrypted| {
            let plain = cipher.decrypt(tenant, server, &encrypted)?;
            serde_json::from_slice(&plain).map_err(|_| Error::Configuration)
        })
        .transpose()?;
    Ok(ConnectorServer {
        tenant_id: row.0,
        id: row.1,
        config_revision: row.2,
        transport: row.3,
        endpoint_url: row.4,
        stdio_profile: row.5,
        allow_private: row.6,
        auth_mode: row.7,
        credential_reference,
        oauth_issuer: row.9,
        oauth_resource: row.10,
        oauth_audience: row.11,
    })
}

/// Return a completed discovery receipt before any retry performs network I/O.
/// The operation must already belong to the same tenant, server and mutation
/// kind; otherwise it is an idempotency conflict rather than a cache hit.
pub async fn discovery_receipt(
    pool: &PgPool,
    tenant: Uuid,
    server: Uuid,
    operation: Uuid,
) -> Result<Option<Value>> {
    let row: Option<(String, Value)> = sqlx::query_as(
        "SELECT m.kind,m.response FROM mcp_catalog_mutations m JOIN tenants t ON t.id=m.tenant_id AND t.active WHERE m.tenant_id=$1 AND m.operation_id=$2",
    )
    .bind(tenant)
    .bind(operation)
    .fetch_optional(pool)
    .await?;
    let Some((kind, response)) = row else {
        return Ok(None);
    };
    if kind != "discovery_record"
        || response
            .get("server_id")
            .and_then(Value::as_str)
            .and_then(|value| Uuid::parse_str(value).ok())
            != Some(server)
    {
        return Err(Error::Conflict);
    }
    Ok(Some(response))
}

pub async fn record_discovery(pool: &PgPool, input: RecordDiscovery) -> Result<Value> {
    validate_common(&input.actor, None, None)?;
    if input.expected_server_revision < 1
        || input.protocol_version != PROTOCOL_VERSION
        || input.result_type != "complete"
        || !(0..=86_400_000).contains(&input.ttl_ms)
        || input.supported_versions.is_empty()
        || input.supported_versions.len() > 16
    {
        return Err(Error::Invalid("invalid MCP discovery result"));
    }
    let mut versions = HashSet::new();
    if input
        .supported_versions
        .iter()
        .any(|v| !valid_text(v, 32) || !versions.insert(v.as_str()))
        || !versions.contains(PROTOCOL_VERSION)
    {
        return Err(Error::Invalid("unsupported MCP protocol version"));
    }
    bounded_json(&input.capabilities, MAX_CAPABILITIES_BYTES, true)?;
    if let Some(info) = &input.server_info {
        bounded_json(info, MAX_SERVER_INFO_BYTES, true)?;
    }
    if input
        .instructions
        .as_ref()
        .is_some_and(|v| v.len() > 16_384 || v.chars().any(|c| c == '\0'))
    {
        return Err(Error::Invalid("invalid MCP server instructions"));
    }
    match (input.cache_scope, &input.authorization_context_fingerprint) {
        (CacheScope::Private, Some(value)) if valid_fingerprint(value) => {}
        (CacheScope::Public, None) => {}
        _ => return Err(Error::Invalid("invalid MCP cache authorization context")),
    }
    let value =
        serde_json::to_value(&input).map_err(|_| Error::Invalid("invalid MCP discovery result"))?;
    let hash = governance::digest(&value)?;
    let capability_hash = governance::digest(
        &json!({"supportedVersions":input.supported_versions,"capabilities":input.capabilities,"serverInfo":input.server_info,"instructions":input.instructions}),
    )?;
    let mut tx = begin(pool, input.tenant_id, input.operation_id).await?;
    if let Some(response) = previous(&mut tx, input.tenant_id, input.operation_id, &hash).await? {
        tx.commit().await?;
        return Ok(response);
    }
    // Serialize with application-owned connection updates without granting the
    // network client any UPDATE privilege over catalog rows.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
        .bind(format!(
            "mcp-server:{}:{}",
            input.tenant_id, input.server_id
        ))
        .execute(&mut *tx)
        .await?;
    let server:Option<(i64,Vec<u8>)>=sqlx::query_as("SELECT config_revision,coalesce(credential_ref_ciphertext,'') FROM mcp_servers WHERE tenant_id=$1 AND id=$2 AND active")
        .bind(input.tenant_id).bind(input.server_id).fetch_optional(&mut *tx).await?;
    let (revision, credential_envelope) = server.ok_or(Error::NotFound)?;
    if revision != input.expected_server_revision {
        return Err(Error::ConfigConflict);
    }
    let auth_context = if matches!(input.cache_scope, CacheScope::Private) {
        let mut material = Vec::with_capacity(64 + credential_envelope.len());
        material.extend_from_slice(input.tenant_id.as_bytes());
        material.extend_from_slice(input.server_id.as_bytes());
        material.extend_from_slice(&revision.to_be_bytes());
        material.extend_from_slice(&credential_envelope);
        material.extend_from_slice(
            input
                .authorization_context_fingerprint
                .as_deref()
                .expect("validated private cache context")
                .as_bytes(),
        );
        Some(store::hash(&material))
    } else {
        None
    };
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO mcp_discovery_snapshots(tenant_id,id,server_id,server_config_revision,protocol_version,result_type,supported_versions,capabilities,server_info,instructions,capability_hash,ttl_ms,cache_scope,authorization_context_hash,expires_at) VALUES($1,$2,$3,$4,$5,'complete',$6,$7,$8,$9,$10,$11,$12,$13,clock_timestamp()+($11::bigint*interval '1 millisecond'))")
        .bind(input.tenant_id).bind(id).bind(input.server_id).bind(revision).bind(PROTOCOL_VERSION).bind(json!(input.supported_versions)).bind(&input.capabilities).bind(&input.server_info).bind(&input.instructions).bind(&capability_hash).bind(input.ttl_ms).bind(input.cache_scope.name()).bind(auth_context).execute(&mut *tx).await?;
    let response = json!({"operation_id":input.operation_id,"snapshot_id":id,"server_id":input.server_id,"server_revision":revision.to_string(),"protocol_version":PROTOCOL_VERSION,"capability_hash":governance::hex(&capability_hash),"ttl_ms":input.ttl_ms,"cache_scope":input.cache_scope.name()});
    sqlx::query("INSERT INTO audit_events(tenant_id,action,object_id,details) VALUES($1,'mcp.discovery_recorded',$2,$3)")
        .bind(input.tenant_id).bind(input.server_id.to_string()).bind(json!({"actor":input.actor,"operation_id":input.operation_id,"snapshot_id":id,"server_revision":revision.to_string(),"protocol_version":PROTOCOL_VERSION,"capability_hash":governance::hex(&capability_hash),"ttl_ms":input.ttl_ms,"cache_scope":input.cache_scope.name()})).execute(&mut *tx).await?;
    receipt(
        &mut tx,
        input.tenant_id,
        input.operation_id,
        &input.actor,
        "discovery_record",
        hash,
        &response,
    )
    .await?;
    tx.commit().await?;
    Ok(response)
}

pub async fn current_discovery(
    pool: &PgPool,
    tenant: Uuid,
    server: Uuid,
    authorization_context_fingerprint: Option<&str>,
) -> Result<Value> {
    type Row = (Value, String, Option<Vec<u8>>, i64, Vec<u8>);
    let row:Option<Row>=sqlx::query_as("SELECT jsonb_build_object('snapshot_id',d.id,'server_id',d.server_id,'server_revision',d.server_config_revision::text,'protocol_version',d.protocol_version,'result_type',d.result_type,'supported_versions',d.supported_versions,'capabilities',d.capabilities,'server_info',d.server_info,'instructions',d.instructions,'capability_hash',encode(d.capability_hash,'hex'),'ttl_ms',d.ttl_ms,'cache_scope',d.cache_scope,'observed_at',d.observed_at,'expires_at',d.expires_at,'fresh',d.expires_at>clock_timestamp()),d.cache_scope,d.authorization_context_hash,s.config_revision,coalesce(s.credential_ref_ciphertext,'') FROM mcp_servers s JOIN tenants t ON t.id=s.tenant_id AND t.active JOIN LATERAL(SELECT * FROM mcp_discovery_snapshots d WHERE d.tenant_id=s.tenant_id AND d.server_id=s.id AND d.server_config_revision=s.config_revision ORDER BY d.observed_at DESC,d.id DESC LIMIT 1)d ON true WHERE s.tenant_id=$1 AND s.id=$2 AND s.active")
        .bind(tenant).bind(server).fetch_optional(pool).await?;
    let (view, scope, stored_context, revision, credential_envelope) =
        row.ok_or(Error::NotFound)?;
    if scope == "private" {
        let fingerprint = authorization_context_fingerprint
            .filter(|value| valid_fingerprint(value))
            .ok_or(Error::NotFound)?;
        let mut material = Vec::with_capacity(128 + credential_envelope.len());
        material.extend_from_slice(tenant.as_bytes());
        material.extend_from_slice(server.as_bytes());
        material.extend_from_slice(&revision.to_be_bytes());
        material.extend_from_slice(&credential_envelope);
        material.extend_from_slice(fingerprint.as_bytes());
        let expected = store::hash(&material);
        if stored_context.as_deref() != Some(expected.as_slice()) {
            return Err(Error::NotFound);
        }
    }
    Ok(view)
}
