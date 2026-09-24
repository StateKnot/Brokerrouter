//! Tenant-scoped MCP connection registry and immutable discovery observations.
//!
//! This module owns host-assigned server identity. Remote `serverInfo` is
//! retained only as untrusted display metadata and never participates in
//! routing, authorization, cache partitioning, or approval identity.
use crate::{
    cipher::ResultCipher,
    error::{Error, Result},
    governance,
    mcp::{SKILLS_EXTENSION, SkillEntry},
    store,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, QueryBuilder, Transaction};
use std::collections::{HashMap, HashSet};
use url::Url;
use uuid::Uuid;

pub const PROTOCOL_VERSION: &str = "2026-07-28";
const MAX_CAPABILITIES_BYTES: usize = 256 * 1024;
const MAX_SERVER_INFO_BYTES: usize = 16 * 1024;
const MAX_TOOL_DESCRIPTOR_BYTES: usize = 512 * 1024;
const MAX_TOOL_CATALOG_BYTES: usize = 4 * 1024 * 1024;
const MAX_TOOLS: usize = 1024;
const MAX_SKILLS: usize = 1024;
const MAX_SKILL_CATALOG_BYTES: usize = 8 * 1024 * 1024;
const MAX_SKILL_ENTRY_BYTES: usize = 512 * 1024;
const MAX_JSON_DEPTH: usize = 32;
type ObservedTool = (String, String, Value, Vec<u8>);
type ReviewToolRow = (String, String, Value, Vec<u8>, DateTime<Utc>);
type ObservedSkill = (String, String, Value, String, bool);

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
    /// Complete, bounded `tools/list` result observed in the same authenticated
    /// lifecycle. Descriptors retain their upstream names; downstream names
    /// are host-assigned while recording the immutable snapshot.
    #[serde(default)]
    pub tools: Vec<Value>,
    pub tools_ttl_ms: Option<i32>,
    pub tools_cache_scope: Option<CacheScope>,
    /// Complete static Skills list. Never exposed to agents merely by discovery.
    #[serde(default)]
    pub skills: Vec<Value>,
    pub skills_ttl_ms: Option<i32>,
    pub skills_cache_scope: Option<CacheScope>,
    /// Lower-hex SHA-256 of the effective upstream auth context. The client
    /// derives it from the resolved credential/token; the credential itself is
    /// never persisted in this request or snapshot.
    pub authorization_context_fingerprint: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RecordSkillGet {
    pub tenant_id: Uuid,
    pub server_id: Uuid,
    pub operation_id: Uuid,
    pub actor: String,
    pub expected_server_revision: i64,
    pub protocol_version: String,
    pub result_type: String,
    pub supported_versions: Vec<String>,
    pub capabilities: Value,
    pub discovery_ttl_ms: i32,
    pub discovery_cache_scope: CacheScope,
    pub requested_uri: String,
    pub entry: Value,
    pub skill_ttl_ms: i32,
    pub skill_cache_scope: CacheScope,
    pub authorization_context_fingerprint: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SetToolGrant {
    pub tenant_id: Uuid,
    pub key_id: Uuid,
    pub server_id: Uuid,
    pub operation_id: Uuid,
    pub actor: String,
    pub reason: String,
    pub downstream_name: String,
    /// Lower-hex descriptor digest copied from the operator review view. A
    /// changed descriptor therefore fails closed instead of inheriting access.
    pub descriptor_sha256: String,
    pub active: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SetOauthPrincipal {
    pub tenant_id: Uuid,
    pub key_id: Uuid,
    pub operation_id: Uuid,
    pub actor: String,
    pub reason: String,
    /// Exact authorization-server issuer. It is compared byte-for-byte with
    /// the verified JWT `iss` claim; no URL normalization occurs at runtime.
    pub issuer: String,
    pub subject: String,
    pub active: bool,
}

#[derive(Clone, Debug)]
pub struct AuthorizedTool {
    pub server_id: Uuid,
    pub server_config_revision: i64,
    pub discovery_snapshot_id: Uuid,
    pub upstream_name: String,
    pub downstream_name: String,
    pub descriptor_hash: Vec<u8>,
    pub descriptor: Value,
    pub expires_at: DateTime<Utc>,
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

fn valid_tool_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

fn downstream_tool_name(server: Uuid, upstream: &str) -> String {
    let digest = store::hash(upstream.as_bytes());
    let suffix = digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let readable = upstream.chars().take(60).collect::<String>();
    format!("br.{}.{suffix}.{readable}", server.simple())
}

fn nested_header_annotation(value: &Value) -> bool {
    value
        .get("properties")
        .and_then(Value::as_object)
        .is_some_and(|properties| {
            properties
                .values()
                .any(|value| value.get("x-mcp-header").is_some() || nested_header_annotation(value))
        })
}

fn valid_param_header_annotations(schema: &Value) -> bool {
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return true;
    };
    let mut names = HashSet::new();
    properties.iter().all(|(_, value)| {
        if nested_header_annotation(value) {
            return false;
        }
        let Some(annotation) = value.get("x-mcp-header") else {
            return true;
        };
        annotation.as_str().is_some_and(|annotation| {
            !annotation.is_empty()
                && annotation.chars().all(|character| {
                    character.is_ascii_alphanumeric()
                        || matches!(
                            character,
                            '!' | '#'
                                | '$'
                                | '%'
                                | '&'
                                | '\''
                                | '*'
                                | '+'
                                | '-'
                                | '.'
                                | '^'
                                | '_'
                                | '`'
                                | '|'
                                | '~'
                        )
                })
                && names.insert(annotation.to_ascii_lowercase())
                && matches!(
                    value.get("type").and_then(Value::as_str),
                    Some("string" | "integer" | "boolean")
                )
        })
    })
}

fn validate_tools(input: &RecordDiscovery) -> Result<Vec<ObservedTool>> {
    let advertised = input.capabilities.get("tools").is_some();
    if input.tools.len() > MAX_TOOLS
        || advertised != input.tools_ttl_ms.is_some()
        || advertised != input.tools_cache_scope.is_some()
        || (!advertised && !input.tools.is_empty())
        || input
            .tools_ttl_ms
            .is_some_and(|ttl| !(0..=86_400_000).contains(&ttl))
    {
        return Err(Error::Invalid("invalid MCP tool catalog metadata"));
    }
    let mut total = 0usize;
    let mut names = HashSet::with_capacity(input.tools.len());
    let mut downstream = HashSet::with_capacity(input.tools.len());
    let mut tools = Vec::with_capacity(input.tools.len());
    for descriptor in &input.tools {
        bounded_json(descriptor, MAX_TOOL_DESCRIPTOR_BYTES, true)?;
        let bytes = serde_json::to_vec(descriptor)
            .map_err(|_| Error::Invalid("invalid MCP tool descriptor"))?;
        total = total
            .checked_add(bytes.len())
            .ok_or(Error::Invalid("MCP tool catalog is too large"))?;
        if total > MAX_TOOL_CATALOG_BYTES {
            return Err(Error::Invalid("MCP tool catalog is too large"));
        }
        let name = descriptor
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| valid_tool_name(name))
            .ok_or(Error::Invalid("invalid MCP upstream tool name"))?;
        if !descriptor.get("inputSchema").is_some_and(Value::is_object)
            || descriptor
                .get("outputSchema")
                .is_some_and(|value| !value.is_object())
            || !valid_param_header_annotations(&descriptor["inputSchema"])
            || !names.insert(name.to_owned())
        {
            return Err(Error::Invalid("invalid MCP tool descriptor"));
        }
        let exposed = downstream_tool_name(input.server_id, name);
        if !valid_tool_name(&exposed) || !downstream.insert(exposed.clone()) {
            return Err(Error::Invalid("invalid MCP downstream tool identity"));
        }
        tools.push((
            name.to_owned(),
            exposed,
            descriptor.clone(),
            governance::digest(descriptor)?,
        ));
    }
    tools.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(tools)
}

fn validate_skills(input: &RecordDiscovery) -> Result<Vec<ObservedSkill>> {
    let advertised = input
        .capabilities
        .get("extensions")
        .and_then(Value::as_object)
        .is_some_and(|extensions| extensions.contains_key(SKILLS_EXTENSION));
    if input.skills.len() > MAX_SKILLS
        || advertised != input.skills_ttl_ms.is_some()
        || advertised != input.skills_cache_scope.is_some()
        || (!advertised && !input.skills.is_empty())
        || (advertised
            && !input
                .capabilities
                .get("resources")
                .is_some_and(Value::is_object))
        || input
            .skills_ttl_ms
            .is_some_and(|ttl| !(0..=86_400_000).contains(&ttl))
    {
        return Err(Error::Invalid("invalid MCP Skills catalog metadata"));
    }
    let mut total = 0usize;
    let mut uris = HashSet::new();
    let mut skills = Vec::with_capacity(input.skills.len());
    for descriptor in &input.skills {
        bounded_json(descriptor, MAX_SKILL_ENTRY_BYTES, true)?;
        let bytes = serde_json::to_vec(descriptor)
            .map_err(|_| Error::Invalid("invalid MCP Skill entry"))?;
        total = total
            .checked_add(bytes.len())
            .ok_or(Error::Invalid("MCP Skills catalog is too large"))?;
        if total > MAX_SKILL_CATALOG_BYTES {
            return Err(Error::Invalid("MCP Skills catalog is too large"));
        }
        let entry: SkillEntry = serde_json::from_value(descriptor.clone())
            .map_err(|_| Error::Invalid("invalid MCP Skill entry"))?;
        let validated = entry
            .validate_static(input.server_id)
            .map_err(|_| Error::Invalid("invalid or unsupported MCP Skill manifest"))?;
        let uri = validated.identity().uri.clone();
        if !uris.insert(uri.clone()) {
            return Err(Error::Invalid("duplicate MCP Skill URI"));
        }
        let name = descriptor["frontmatter"]["name"]
            .as_str()
            .expect("validated Skill name")
            .to_owned();
        skills.push((
            uri,
            name,
            descriptor.clone(),
            validated.approval_fingerprint().to_owned(),
            validated.requests_tool_grant(),
        ));
    }
    skills.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(skills)
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
    if transport_name == "stdio" && auth_mode != "none" {
        return Err(Error::Invalid(
            "stdio credentials must be injected by the deployment launcher profile",
        ));
    }
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

/// A URI is an upstream protocol identifier, never a URL to fetch directly.
pub fn validate_skill_lookup_uri(uri: &str) -> Result<()> {
    if uri.is_empty() || uri.len() > 4096 || uri.chars().any(|c| c.is_control()) {
        return Err(Error::Invalid("invalid MCP Skill URI"));
    }
    let parsed = Url::parse(uri).map_err(|_| Error::Invalid("invalid MCP Skill URI"))?;
    if parsed.scheme().is_empty()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || !parsed.path().ends_with("/SKILL.md")
    {
        return Err(Error::Invalid("invalid MCP Skill URI"));
    }
    Ok(())
}

pub fn validate_skill_lookup_request(actor: &str, uri: &str) -> Result<()> {
    validate_common(actor, None, None)?;
    validate_skill_lookup_uri(uri)
}

pub async fn skill_get_receipt(
    pool: &PgPool,
    tenant: Uuid,
    server: Uuid,
    operation: Uuid,
    uri: &str,
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
    if kind != "skill_get_record"
        || response.get("server_id").and_then(Value::as_str) != Some(&server.to_string())
        || response.get("uri").and_then(Value::as_str) != Some(uri)
    {
        return Err(Error::Conflict);
    }
    Ok(Some(response))
}

pub async fn record_skill_get(pool: &PgPool, input: RecordSkillGet) -> Result<Value> {
    validate_skill_lookup_request(&input.actor, &input.requested_uri)?;
    if input.expected_server_revision < 1
        || input.protocol_version != PROTOCOL_VERSION
        || input.result_type != "complete"
        || input.supported_versions.is_empty()
        || input.supported_versions.len() > 16
        || !input
            .supported_versions
            .iter()
            .any(|v| v == PROTOCOL_VERSION)
        || input.supported_versions.iter().any(|v| !valid_text(v, 32))
        || !(0..=86_400_000).contains(&input.discovery_ttl_ms)
        || !(0..=86_400_000).contains(&input.skill_ttl_ms)
    {
        return Err(Error::Invalid("invalid MCP Skill lookup result"));
    }
    let mut versions = HashSet::new();
    if input
        .supported_versions
        .iter()
        .any(|v| !versions.insert(v.as_str()))
    {
        return Err(Error::Invalid("duplicate MCP protocol version"));
    }
    bounded_json(&input.capabilities, MAX_CAPABILITIES_BYTES, true)?;
    bounded_json(&input.entry, MAX_SKILL_ENTRY_BYTES, true)?;
    if !input
        .capabilities
        .get("resources")
        .is_some_and(Value::is_object)
        || !input
            .capabilities
            .get("extensions")
            .and_then(Value::as_object)
            .is_some_and(|extensions| {
                extensions
                    .get(SKILLS_EXTENSION)
                    .is_some_and(Value::is_object)
            })
    {
        return Err(Error::Invalid("MCP Skills capability is not advertised"));
    }
    let entry: SkillEntry = serde_json::from_value(input.entry.clone())
        .map_err(|_| Error::Invalid("invalid MCP Skill entry"))?;
    let validated = entry
        .validate_static(input.server_id)
        .map_err(|_| Error::Invalid("invalid or unsupported MCP Skill manifest"))?;
    if validated.identity().uri != input.requested_uri {
        return Err(Error::Invalid("MCP Skill URI mismatch"));
    }
    let private = matches!(input.discovery_cache_scope, CacheScope::Private)
        || matches!(input.skill_cache_scope, CacheScope::Private);
    match (private, &input.authorization_context_fingerprint) {
        (true, Some(value)) if valid_fingerprint(value) => {}
        (false, None) => {}
        _ => return Err(Error::Invalid("invalid MCP cache authorization context")),
    }
    let value = serde_json::to_value(&input)
        .map_err(|_| Error::Invalid("invalid MCP Skill lookup result"))?;
    let body_hash = governance::digest(&value)?;
    let capability_hash = governance::digest(&json!({
        "supportedVersions":input.supported_versions,
        "capabilities":input.capabilities,
    }))?;
    let mut tx = begin(pool, input.tenant_id, input.operation_id).await?;
    if let Some(response) =
        previous(&mut tx, input.tenant_id, input.operation_id, &body_hash).await?
    {
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
    let server: Option<(i64, Vec<u8>)> = sqlx::query_as(
        "SELECT config_revision,coalesce(credential_ref_ciphertext,'') FROM mcp_servers WHERE tenant_id=$1 AND id=$2 AND active",
    ).bind(input.tenant_id).bind(input.server_id).fetch_optional(&mut *tx).await?;
    let (revision, credential_envelope) = server.ok_or(Error::NotFound)?;
    if revision != input.expected_server_revision {
        return Err(Error::ConfigConflict);
    }
    let auth_context = if private {
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
    let ttl_ms = input.discovery_ttl_ms.min(input.skill_ttl_ms);
    let effective_scope = if private { "private" } else { "public" };
    let name = input.entry["frontmatter"]["name"]
        .as_str()
        .expect("validated Skill name");
    let expires_at: DateTime<Utc> = sqlx::query_scalar(
        "INSERT INTO mcp_skill_get_observations(tenant_id,id,server_id,server_config_revision,operation_id,uri,name,entry,approval_fingerprint,requests_tool_grant,capability_hash,discovery_ttl_ms,skill_ttl_ms,cache_scope,authorization_context_hash,expires_at) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,clock_timestamp()+($16::bigint*interval '1 millisecond')) RETURNING expires_at",
    )
    .bind(input.tenant_id).bind(id).bind(input.server_id).bind(revision).bind(input.operation_id)
    .bind(&input.requested_uri).bind(name).bind(&input.entry)
    .bind(validated.approval_fingerprint()).bind(validated.requests_tool_grant())
    .bind(&capability_hash).bind(input.discovery_ttl_ms).bind(input.skill_ttl_ms)
    .bind(effective_scope).bind(auth_context).bind(ttl_ms)
    .fetch_one(&mut *tx).await?;
    let response = json!({
        "operation_id":input.operation_id,"observation_id":id,"server_id":input.server_id,
        "server_revision":revision.to_string(),"uri":input.requested_uri,"name":name,
        "approval_fingerprint":validated.approval_fingerprint(),
        "requests_tool_grant":validated.requests_tool_grant(),
        "capability_hash":governance::hex(&capability_hash),
        "ttl_ms":ttl_ms,"cache_scope":effective_scope,"expires_at":expires_at,
        "activation":"disabled_until_approval_and_read_path"
    });
    sqlx::query("INSERT INTO audit_events(tenant_id,action,object_id,details) VALUES($1,'mcp.skill_get_recorded',$2,$3)")
        .bind(input.tenant_id).bind(input.server_id.to_string())
        .bind(json!({"actor":input.actor,"operation_id":input.operation_id,
            "observation_id":id,"uri":input.requested_uri,
            "approval_fingerprint":validated.approval_fingerprint(),
            "capability_hash":governance::hex(&capability_hash),"cache_scope":effective_scope}))
        .execute(&mut *tx).await?;
    receipt(
        &mut tx,
        input.tenant_id,
        input.operation_id,
        &input.actor,
        "skill_get_record",
        body_hash,
        &response,
    )
    .await?;
    tx.commit().await?;
    Ok(response)
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
    if input
        .capabilities
        .get("tools")
        .is_some_and(|value| !value.is_object())
        || input
            .capabilities
            .get("extensions")
            .is_some_and(|value| !value.is_object())
        || input
            .capabilities
            .get("extensions")
            .and_then(Value::as_object)
            .and_then(|extensions| extensions.get(SKILLS_EXTENSION))
            .is_some_and(|value| !value.is_object())
    {
        return Err(Error::Invalid("invalid MCP tools capability"));
    }
    let tools = validate_tools(&input)?;
    let skills = validate_skills(&input)?;
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
    let effective_private = matches!(input.cache_scope, CacheScope::Private)
        || matches!(input.tools_cache_scope, Some(CacheScope::Private))
        || matches!(input.skills_cache_scope, Some(CacheScope::Private));
    match (effective_private, &input.authorization_context_fingerprint) {
        (true, Some(value)) if valid_fingerprint(value) => {}
        (false, None) => {}
        _ => return Err(Error::Invalid("invalid MCP cache authorization context")),
    }
    let value =
        serde_json::to_value(&input).map_err(|_| Error::Invalid("invalid MCP discovery result"))?;
    let hash = governance::digest(&value)?;
    let capability_hash = governance::digest(
        &json!({"supportedVersions":input.supported_versions,"capabilities":input.capabilities,"serverInfo":input.server_info,"instructions":input.instructions}),
    )?;
    let tool_catalog_hash = governance::digest(&Value::Array(
        tools.iter().map(|tool| tool.2.clone()).collect(),
    ))?;
    let skill_catalog_hash = governance::digest(&Value::Array(
        skills.iter().map(|skill| skill.2.clone()).collect(),
    ))?;
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
    let auth_context = if effective_private {
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
    let effective_scope = if effective_private {
        "private"
    } else {
        "public"
    };
    let id = Uuid::new_v4();
    let snapshot_expires_at:DateTime<Utc>=sqlx::query_scalar("INSERT INTO mcp_discovery_snapshots(tenant_id,id,server_id,server_config_revision,protocol_version,result_type,supported_versions,capabilities,server_info,instructions,capability_hash,ttl_ms,cache_scope,authorization_context_hash,skills_ttl_ms,skills_cache_scope,expires_at) VALUES($1,$2,$3,$4,$5,'complete',$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,clock_timestamp()+($11::bigint*interval '1 millisecond')) RETURNING expires_at")
        .bind(input.tenant_id).bind(id).bind(input.server_id).bind(revision).bind(PROTOCOL_VERSION).bind(json!(input.supported_versions)).bind(&input.capabilities).bind(&input.server_info).bind(&input.instructions).bind(&capability_hash).bind(input.ttl_ms).bind(effective_scope).bind(auth_context).bind(input.skills_ttl_ms).bind(input.skills_cache_scope.map(CacheScope::name)).fetch_one(&mut *tx).await?;
    if !tools.is_empty() {
        let tools_ttl_ms = input.tools_ttl_ms.expect("validated tools TTL");
        let tools_expires_at: DateTime<Utc> = sqlx::query_scalar(
            "SELECT LEAST($1::timestamptz,clock_timestamp()+($2::bigint*interval '1 millisecond'))",
        )
        .bind(snapshot_expires_at)
        .bind(tools_ttl_ms)
        .fetch_one(&mut *tx)
        .await?;
        let effective_scope = if matches!(input.cache_scope, CacheScope::Private)
            || matches!(input.tools_cache_scope, Some(CacheScope::Private))
        {
            "private"
        } else {
            "public"
        };
        let mut query = QueryBuilder::<Postgres>::new(
            "INSERT INTO mcp_discovered_tools(tenant_id,discovery_snapshot_id,server_id,ordinal,upstream_name,downstream_name,descriptor,descriptor_hash,cache_scope,expires_at) ",
        );
        query.push_values(tools.iter().enumerate(), |mut row, (ordinal, tool)| {
            row.push_bind(input.tenant_id)
                .push_bind(id)
                .push_bind(input.server_id)
                .push_bind(i32::try_from(ordinal).expect("bounded tool ordinal"))
                .push_bind(&tool.0)
                .push_bind(&tool.1)
                .push_bind(&tool.2)
                .push_bind(&tool.3)
                .push_bind(effective_scope)
                .push_bind(tools_expires_at);
        });
        query.build().execute(&mut *tx).await?;
    }
    if !skills.is_empty() {
        let skill_ttl_ms = input.skills_ttl_ms.expect("validated Skills TTL");
        let skills_expires_at: DateTime<Utc> = sqlx::query_scalar(
            "SELECT LEAST($1::timestamptz,clock_timestamp()+($2::bigint*interval '1 millisecond'))",
        )
        .bind(snapshot_expires_at)
        .bind(skill_ttl_ms)
        .fetch_one(&mut *tx)
        .await?;
        let effective_scope = if matches!(input.cache_scope, CacheScope::Private)
            || matches!(input.skills_cache_scope, Some(CacheScope::Private))
        {
            "private"
        } else {
            "public"
        };
        let mut query = QueryBuilder::<Postgres>::new(
            "INSERT INTO mcp_discovered_skills(tenant_id,discovery_snapshot_id,server_id,ordinal,uri,name,entry,approval_fingerprint,requests_tool_grant,cache_scope,expires_at) ",
        );
        query.push_values(skills.iter().enumerate(), |mut row, (ordinal, skill)| {
            row.push_bind(input.tenant_id)
                .push_bind(id)
                .push_bind(input.server_id)
                .push_bind(i32::try_from(ordinal).expect("bounded Skill ordinal"))
                .push_bind(&skill.0)
                .push_bind(&skill.1)
                .push_bind(&skill.2)
                .push_bind(&skill.3)
                .push_bind(skill.4)
                .push_bind(effective_scope)
                .push_bind(skills_expires_at);
        });
        query.build().execute(&mut *tx).await?;
    }
    let response = json!({"operation_id":input.operation_id,"snapshot_id":id,"server_id":input.server_id,"server_revision":revision.to_string(),"protocol_version":PROTOCOL_VERSION,"capability_hash":governance::hex(&capability_hash),"tool_catalog_hash":governance::hex(&tool_catalog_hash),"tool_count":tools.len(),"skill_catalog_hash":governance::hex(&skill_catalog_hash),"skill_count":skills.len(),"ttl_ms":input.ttl_ms,"cache_scope":effective_scope});
    sqlx::query("INSERT INTO audit_events(tenant_id,action,object_id,details) VALUES($1,'mcp.discovery_recorded',$2,$3)")
        .bind(input.tenant_id).bind(input.server_id.to_string()).bind(json!({"actor":input.actor,"operation_id":input.operation_id,"snapshot_id":id,"server_revision":revision.to_string(),"protocol_version":PROTOCOL_VERSION,"capability_hash":governance::hex(&capability_hash),"tool_catalog_hash":governance::hex(&tool_catalog_hash),"tool_count":tools.len(),"skill_catalog_hash":governance::hex(&skill_catalog_hash),"skill_count":skills.len(),"ttl_ms":input.ttl_ms,"cache_scope":effective_scope})).execute(&mut *tx).await?;
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

pub async fn review_tools(pool: &PgPool, tenant: Uuid, server: Uuid) -> Result<Value> {
    let rows:Vec<ReviewToolRow>=sqlx::query_as("SELECT t.upstream_name,t.downstream_name,t.descriptor,t.descriptor_hash,t.expires_at FROM mcp_servers s JOIN tenants x ON x.id=s.tenant_id AND x.active JOIN LATERAL(SELECT d.id,d.expires_at FROM mcp_discovery_snapshots d WHERE d.tenant_id=s.tenant_id AND d.server_id=s.id AND d.server_config_revision=s.config_revision ORDER BY d.observed_at DESC,d.id DESC LIMIT 1)d ON d.expires_at>clock_timestamp() JOIN mcp_discovered_tools t ON t.tenant_id=s.tenant_id AND t.discovery_snapshot_id=d.id AND t.server_id=s.id AND t.expires_at>clock_timestamp() WHERE s.tenant_id=$1 AND s.id=$2 AND s.active ORDER BY t.upstream_name")
        .bind(tenant).bind(server).fetch_all(pool).await?;
    if rows.is_empty() {
        return Err(Error::NotFound);
    }
    Ok(
        json!({"server_id":server,"tools":rows.into_iter().map(|(upstream_name,downstream_name,descriptor,hash,expires_at)|json!({"upstream_name":upstream_name,"downstream_name":downstream_name,"descriptor":descriptor,"descriptor_sha256":governance::hex(&hash),"expires_at":expires_at})).collect::<Vec<_>>() }),
    )
}

/// Operator-only review of fresh Skill metadata. This never grants a Skill to
/// a virtual key or reads any resource bytes.
pub async fn review_skills(pool: &PgPool, tenant: Uuid, server: Uuid) -> Result<Value> {
    type Row = (
        Uuid,
        i64,
        Option<String>,
        Option<String>,
        Option<Value>,
        Option<String>,
        Option<bool>,
        Option<DateTime<Utc>>,
    );
    let rows: Vec<Row> = sqlx::query_as("SELECT d.id,d.server_config_revision,k.uri,k.name,k.entry,k.approval_fingerprint,k.requests_tool_grant,k.expires_at FROM mcp_servers s JOIN tenants x ON x.id=s.tenant_id AND x.active JOIN LATERAL(SELECT d.id,d.server_config_revision,d.skills_ttl_ms,d.observed_at,d.expires_at FROM mcp_discovery_snapshots d WHERE d.tenant_id=s.tenant_id AND d.server_id=s.id AND d.server_config_revision=s.config_revision ORDER BY d.observed_at DESC,d.id DESC LIMIT 1)d ON true LEFT JOIN mcp_discovered_skills k ON k.tenant_id=s.tenant_id AND k.discovery_snapshot_id=d.id AND k.server_id=s.id AND k.expires_at>clock_timestamp() WHERE s.tenant_id=$1 AND s.id=$2 AND s.active AND d.skills_ttl_ms IS NOT NULL AND d.expires_at>clock_timestamp() AND d.observed_at+(d.skills_ttl_ms::bigint*interval '1 millisecond')>clock_timestamp() ORDER BY k.uri")
        .bind(tenant).bind(server).fetch_all(pool).await?;
    let Some((snapshot_id, revision, ..)) = rows.first() else {
        return Err(Error::NotFound);
    };
    let (snapshot_id, revision) = (*snapshot_id, *revision);
    let mut names = HashMap::<String, usize>::new();
    for (_, _, _, name, _, _, _, _) in &rows {
        if let Some(name) = name {
            *names.entry(name.clone()).or_default() += 1;
        }
    }
    let skills = rows
        .into_iter()
        .filter_map(
            |(_, _, uri, name, entry, fingerprint, requests_tool_grant, expires_at)| {
                Some(json!({
                    "uri":uri?,
                    "name":name.as_ref()?,
                    "entry":entry?,
                    "approval_fingerprint":fingerprint?,
                    "requests_tool_grant":requests_tool_grant?,
                    "expires_at":expires_at?,
                    "name_collision":names.get(name.as_ref()?).copied().unwrap_or(0)>1,
                "activation":"disabled_until_approval_and_read_path",
                }))
            },
        )
        .collect::<Vec<_>>();
    Ok(
        json!({"server_id":server,"snapshot_id":snapshot_id,"server_revision":revision.to_string(),"skills":skills}),
    )
}

/// Operator-only view of a fresh direct lookup; never exposed by the MCP server.
pub async fn review_skill_get(
    pool: &PgPool,
    tenant: Uuid,
    server: Uuid,
    uri: &str,
) -> Result<Value> {
    validate_skill_lookup_uri(uri)?;
    type Row = (
        Uuid,
        i64,
        String,
        Value,
        String,
        bool,
        Vec<u8>,
        String,
        DateTime<Utc>,
    );
    let row: Option<Row> = sqlx::query_as(
        "SELECT o.id,o.server_config_revision,o.name,o.entry,o.approval_fingerprint,o.requests_tool_grant,o.capability_hash,o.cache_scope,o.expires_at FROM mcp_servers s JOIN tenants t ON t.id=s.tenant_id AND t.active JOIN LATERAL(SELECT * FROM mcp_skill_get_observations o WHERE o.tenant_id=s.tenant_id AND o.server_id=s.id AND o.server_config_revision=s.config_revision AND o.uri=$3 ORDER BY o.observed_at DESC,o.id DESC LIMIT 1)o ON true WHERE s.tenant_id=$1 AND s.id=$2 AND s.active AND o.expires_at>clock_timestamp()",
    ).bind(tenant).bind(server).bind(uri).fetch_optional(pool).await?;
    let (
        id,
        revision,
        name,
        entry,
        fingerprint,
        requests_tool_grant,
        capability_hash,
        scope,
        expires_at,
    ) = row.ok_or(Error::NotFound)?;
    Ok(json!({
        "server_id":server,"observation_id":id,"server_revision":revision.to_string(),
        "uri":uri,"name":name,"entry":entry,"approval_fingerprint":fingerprint,
        "requests_tool_grant":requests_tool_grant,"capability_hash":governance::hex(&capability_hash),
        "cache_scope":scope,"expires_at":expires_at,
        "activation":"disabled_until_approval_and_read_path"
    }))
}

pub async fn set_tool_grant(pool: &PgPool, input: SetToolGrant) -> Result<Value> {
    validate_common(&input.actor, Some(&input.reason), None)?;
    if !valid_tool_name(&input.downstream_name) || !valid_fingerprint(&input.descriptor_sha256) {
        return Err(Error::Invalid("invalid MCP tool grant"));
    }
    let value =
        serde_json::to_value(&input).map_err(|_| Error::Invalid("invalid MCP tool grant"))?;
    let body_hash = governance::digest(&value)?;
    let mut tx = begin(pool, input.tenant_id, input.operation_id).await?;
    if let Some(response) =
        previous(&mut tx, input.tenant_id, input.operation_id, &body_hash).await?
    {
        tx.commit().await?;
        return Ok(response);
    }
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
        .bind(format!(
            "mcp-tool-grant:{}:{}:{}",
            input.tenant_id, input.key_id, input.downstream_name
        ))
        .execute(&mut *tx)
        .await?;
    let key_exists:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM virtual_keys WHERE tenant_id=$1 AND id=$2 AND revoked_at IS NULL AND (expires_at IS NULL OR expires_at>clock_timestamp()))")
        .bind(input.tenant_id).bind(input.key_id).fetch_one(&mut *tx).await?;
    if !key_exists {
        return Err(Error::NotFound);
    }
    let observed:Option<(String,Vec<u8>,i64)>=sqlx::query_as("SELECT t.upstream_name,t.descriptor_hash,s.config_revision FROM mcp_servers s JOIN LATERAL(SELECT d.id,d.expires_at FROM mcp_discovery_snapshots d WHERE d.tenant_id=s.tenant_id AND d.server_id=s.id AND d.server_config_revision=s.config_revision ORDER BY d.observed_at DESC,d.id DESC LIMIT 1)d ON d.expires_at>clock_timestamp() JOIN mcp_discovered_tools t ON t.tenant_id=s.tenant_id AND t.discovery_snapshot_id=d.id AND t.server_id=s.id AND t.expires_at>clock_timestamp() WHERE s.tenant_id=$1 AND s.id=$2 AND s.active AND t.downstream_name=$3")
        .bind(input.tenant_id).bind(input.server_id).bind(&input.downstream_name).fetch_optional(&mut *tx).await?;
    let (upstream_name, descriptor_hash, server_revision) = observed.ok_or(Error::NotFound)?;
    if governance::hex(&descriptor_hash) != input.descriptor_sha256 {
        return Err(Error::ConfigConflict);
    }
    let revision:i64=sqlx::query_scalar("INSERT INTO mcp_key_tool_grants(tenant_id,key_id,server_id,server_config_revision,upstream_name,descriptor_hash,active) VALUES($1,$2,$3,$4,$5,$6,$7) ON CONFLICT(tenant_id,key_id,server_id,upstream_name) DO UPDATE SET server_config_revision=EXCLUDED.server_config_revision,descriptor_hash=EXCLUDED.descriptor_hash,active=EXCLUDED.active RETURNING revision")
        .bind(input.tenant_id).bind(input.key_id).bind(input.server_id).bind(server_revision).bind(&upstream_name).bind(&descriptor_hash).bind(input.active).fetch_one(&mut *tx).await?;
    let response = json!({"operation_id":input.operation_id,"key_id":input.key_id,"server_id":input.server_id,"server_revision":server_revision.to_string(),"upstream_name":upstream_name,"downstream_name":input.downstream_name,"descriptor_sha256":input.descriptor_sha256,"active":input.active,"revision":revision.to_string()});
    sqlx::query("INSERT INTO audit_events(tenant_id,action,object_id,details) VALUES($1,'mcp.tool_grant_set',$2,$3)")
        .bind(input.tenant_id).bind(input.key_id.to_string()).bind(json!({"actor":input.actor,"reason":input.reason,"operation_id":input.operation_id,"server_id":input.server_id,"server_revision":server_revision.to_string(),"upstream_name":upstream_name,"downstream_name":input.downstream_name,"descriptor_sha256":input.descriptor_sha256,"active":input.active,"revision":revision.to_string()})).execute(&mut *tx).await?;
    receipt(
        &mut tx,
        input.tenant_id,
        input.operation_id,
        &input.actor,
        "tool_grant_set",
        body_hash,
        &response,
    )
    .await?;
    tx.commit().await?;
    Ok(response)
}

pub async fn set_oauth_principal(pool: &PgPool, input: SetOauthPrincipal) -> Result<Value> {
    validate_common(&input.actor, Some(&input.reason), None)?;
    let issuer =
        Url::parse(&input.issuer).map_err(|_| Error::Invalid("invalid MCP OAuth issuer"))?;
    if input.issuer.len() > 2048
        || issuer.scheme() != "https"
        || issuer.host_str().is_none()
        || !issuer.username().is_empty()
        || issuer.password().is_some()
        || issuer.query().is_some()
        || issuer.fragment().is_some()
        || !valid_text(&input.subject, 512)
    {
        return Err(Error::Invalid("invalid MCP OAuth principal"));
    }
    let value =
        serde_json::to_value(&input).map_err(|_| Error::Invalid("invalid MCP OAuth principal"))?;
    let body_hash = governance::digest(&value)?;
    let mut tx = begin(pool, input.tenant_id, input.operation_id).await?;
    if let Some(response) =
        previous(&mut tx, input.tenant_id, input.operation_id, &body_hash).await?
    {
        tx.commit().await?;
        return Ok(response);
    }
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
        .bind(format!(
            "mcp-oauth-principal:{}:{}:{}",
            input.tenant_id, input.issuer, input.subject
        ))
        .execute(&mut *tx)
        .await?;
    let key_exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM virtual_keys WHERE tenant_id=$1 AND id=$2 AND revoked_at IS NULL AND (expires_at IS NULL OR expires_at>clock_timestamp()))")
        .bind(input.tenant_id)
        .bind(input.key_id)
        .fetch_one(&mut *tx)
        .await?;
    if !key_exists {
        return Err(Error::NotFound);
    }
    let before: Option<(Uuid, bool, i64)> = sqlx::query_as(
        "SELECT key_id,active,revision FROM mcp_oauth_principals WHERE tenant_id=$1 AND issuer=$2 AND subject=$3 FOR UPDATE",
    )
    .bind(input.tenant_id)
    .bind(&input.issuer)
    .bind(&input.subject)
    .fetch_optional(&mut *tx)
    .await?;
    let revision: i64 = sqlx::query_scalar("INSERT INTO mcp_oauth_principals(tenant_id,issuer,subject,key_id,active) VALUES($1,$2,$3,$4,$5) ON CONFLICT(tenant_id,issuer,subject) DO UPDATE SET key_id=EXCLUDED.key_id,active=EXCLUDED.active RETURNING revision")
        .bind(input.tenant_id)
        .bind(&input.issuer)
        .bind(&input.subject)
        .bind(input.key_id)
        .bind(input.active)
        .fetch_one(&mut *tx)
        .await?;
    let response = json!({"operation_id":input.operation_id,"tenant_id":input.tenant_id,"key_id":input.key_id,"issuer":input.issuer,"subject":input.subject,"active":input.active,"revision":revision.to_string()});
    sqlx::query("INSERT INTO audit_events(tenant_id,action,object_id,details) VALUES($1,'mcp.oauth_principal_set',$2,$3)")
        .bind(input.tenant_id)
        .bind(input.key_id.to_string())
        .bind(json!({"actor":input.actor,"reason":input.reason,"operation_id":input.operation_id,"issuer":input.issuer,"subject":input.subject,"before":before.map(|(key_id,active,revision)|json!({"key_id":key_id,"active":active,"revision":revision.to_string()})),"after":{"key_id":input.key_id,"active":input.active,"revision":revision.to_string()}}))
        .execute(&mut *tx)
        .await?;
    receipt(
        &mut tx,
        input.tenant_id,
        input.operation_id,
        &input.actor,
        "oauth_principal_set",
        body_hash,
        &response,
    )
    .await?;
    tx.commit().await?;
    Ok(response)
}

pub async fn authorized_tools(
    pool: &PgPool,
    who: &store::Identity,
    after: Option<&str>,
    limit: i64,
) -> Result<Vec<AuthorizedTool>> {
    if !(1..=101).contains(&limit) || after.is_some_and(|value| !valid_tool_name(value)) {
        return Err(Error::Invalid("invalid MCP tool cursor"));
    }
    type Row = (
        Uuid,
        i64,
        Uuid,
        String,
        String,
        Vec<u8>,
        Value,
        DateTime<Utc>,
    );
    let rows:Vec<Row>=sqlx::query_as("SELECT s.id,d.server_config_revision,d.id,t.upstream_name,t.downstream_name,t.descriptor_hash,t.descriptor,t.expires_at FROM virtual_keys k JOIN tenants x ON x.id=k.tenant_id AND x.active JOIN mcp_servers s ON s.tenant_id=k.tenant_id AND s.active JOIN LATERAL(SELECT d.id,d.server_config_revision,d.authorization_context_hash,d.expires_at FROM mcp_discovery_snapshots d WHERE d.tenant_id=s.tenant_id AND d.server_id=s.id AND d.server_config_revision=s.config_revision ORDER BY d.observed_at DESC,d.id DESC LIMIT 1)d ON d.expires_at>clock_timestamp() JOIN mcp_discovered_tools t ON t.tenant_id=s.tenant_id AND t.discovery_snapshot_id=d.id AND t.server_id=s.id AND t.expires_at>clock_timestamp() JOIN mcp_key_tool_grants g ON g.tenant_id=t.tenant_id AND g.key_id=k.id AND g.server_id=t.server_id AND g.server_config_revision=d.server_config_revision AND g.upstream_name=t.upstream_name AND g.descriptor_hash=t.descriptor_hash AND g.active WHERE k.tenant_id=$1 AND k.id=$2 AND k.revoked_at IS NULL AND (k.expires_at IS NULL OR k.expires_at>clock_timestamp()) AND (t.cache_scope='public' OR d.authorization_context_hash IS NOT NULL) AND ($3::text IS NULL OR t.downstream_name>$3) ORDER BY t.downstream_name LIMIT $4")
        .bind(who.tenant_id).bind(who.key_id).bind(after).bind(limit).fetch_all(pool).await?;
    Ok(rows
        .into_iter()
        .map(
            |(
                server_id,
                server_config_revision,
                discovery_snapshot_id,
                upstream_name,
                downstream_name,
                descriptor_hash,
                descriptor,
                expires_at,
            )| AuthorizedTool {
                server_id,
                server_config_revision,
                discovery_snapshot_id,
                upstream_name,
                downstream_name,
                descriptor_hash,
                descriptor,
                expires_at,
            },
        )
        .collect())
}

pub async fn authorized_tool(
    pool: &PgPool,
    who: &store::Identity,
    name: &str,
) -> Result<Option<AuthorizedTool>> {
    if !valid_tool_name(name) {
        return Ok(None);
    }
    type Row = (
        Uuid,
        i64,
        Uuid,
        String,
        String,
        Vec<u8>,
        Value,
        DateTime<Utc>,
    );
    let row:Option<Row>=sqlx::query_as("SELECT s.id,d.server_config_revision,d.id,t.upstream_name,t.downstream_name,t.descriptor_hash,t.descriptor,t.expires_at FROM virtual_keys k JOIN tenants x ON x.id=k.tenant_id AND x.active JOIN mcp_servers s ON s.tenant_id=k.tenant_id AND s.active JOIN LATERAL(SELECT d.id,d.server_config_revision,d.authorization_context_hash,d.expires_at FROM mcp_discovery_snapshots d WHERE d.tenant_id=s.tenant_id AND d.server_id=s.id AND d.server_config_revision=s.config_revision ORDER BY d.observed_at DESC,d.id DESC LIMIT 1)d ON d.expires_at>clock_timestamp() JOIN mcp_discovered_tools t ON t.tenant_id=s.tenant_id AND t.discovery_snapshot_id=d.id AND t.server_id=s.id AND t.expires_at>clock_timestamp() JOIN mcp_key_tool_grants g ON g.tenant_id=t.tenant_id AND g.key_id=k.id AND g.server_id=t.server_id AND g.server_config_revision=d.server_config_revision AND g.upstream_name=t.upstream_name AND g.descriptor_hash=t.descriptor_hash AND g.active WHERE k.tenant_id=$1 AND k.id=$2 AND k.revoked_at IS NULL AND (k.expires_at IS NULL OR k.expires_at>clock_timestamp()) AND (t.cache_scope='public' OR d.authorization_context_hash IS NOT NULL) AND t.downstream_name=$3")
        .bind(who.tenant_id).bind(who.key_id).bind(name).fetch_optional(pool).await?;
    Ok(row.map(
        |(
            server_id,
            server_config_revision,
            discovery_snapshot_id,
            upstream_name,
            downstream_name,
            descriptor_hash,
            descriptor,
            expires_at,
        )| AuthorizedTool {
            server_id,
            server_config_revision,
            discovery_snapshot_id,
            upstream_name,
            downstream_name,
            descriptor_hash,
            descriptor,
            expires_at,
        },
    ))
}
