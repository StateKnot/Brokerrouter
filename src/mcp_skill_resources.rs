//! On-demand, approval-bound Skill resource reads. Resource bytes never enter
//! the public edge before the isolated worker verifies the complete manifest.
use crate::{
    cipher::ResultCipher,
    error::{Error, Result},
    governance,
    mcp_client::{self, InjectedCredentialResolver},
    mcp_skill_approvals,
    store::{self, Identity},
};
use base64::{Engine, prelude::BASE64_STANDARD};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::sync::Arc;
use uuid::Uuid;

const MAX_TURN_READS: i64 = 1024;
const MAX_TURN_REQUESTED_BYTES: i64 = 64 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadRequest {
    pub read_id: Uuid,
    pub turn_id: Uuid,
    pub uri: String,
}

#[derive(Clone, sqlx::FromRow)]
struct ReadRow {
    tenant_id: Uuid,
    id: Uuid,
    request_id: Uuid,
    turn_id: Uuid,
    key_id: Uuid,
    server_id: Uuid,
    server_config_revision: i64,
    skill_uri: String,
    resource_uri: String,
    expected_size: i64,
    manifest_hash: Vec<u8>,
    authorization_context_hash: Option<Vec<u8>>,
    state: String,
    claim_id: Option<Uuid>,
    content_sha256: Option<Vec<u8>>,
    content_ciphertext: Option<Vec<u8>>,
    mime_type: Option<String>,
    content_kind: Option<String>,
    resource_valid_until: Option<DateTime<Utc>>,
    failure_code: Option<String>,
    expires_at: DateTime<Utc>,
}

const READ_COLUMNS: &str = "tenant_id,id,request_id,turn_id,key_id,server_id,server_config_revision,skill_uri,resource_uri,expected_size,manifest_hash,authorization_context_hash,state,claim_id,content_sha256,content_ciphertext,mime_type,content_kind,resource_valid_until,failure_code,expires_at";

async fn row(pool: &PgPool, tenant: Uuid, id: Uuid) -> Result<Option<ReadRow>> {
    let sql =
        format!("SELECT {READ_COLUMNS} FROM mcp_skill_resource_reads WHERE tenant_id=$1 AND id=$2");
    Ok(sqlx::query_as(&sql)
        .bind(tenant)
        .bind(id)
        .fetch_optional(pool)
        .await?)
}

fn same_binding(row: &ReadRow, who: &Identity, request_id: Uuid, body: &ReadRequest) -> Result<()> {
    if row.key_id != who.key_id
        || row.request_id != request_id
        || row.turn_id != body.turn_id
        || row.resource_uri != body.uri
    {
        return Err(Error::Conflict);
    }
    Ok(())
}

fn view(row: &ReadRow) -> Value {
    let expired = row.expires_at <= Utc::now()
        || (row.state == "ready"
            && row
                .resource_valid_until
                .is_none_or(|until| until <= Utc::now()));
    json!({
        "read_id":row.id,"request_id":row.request_id,"turn_id":row.turn_id,
        "server_id":row.server_id,"skill_uri":row.skill_uri,"uri":row.resource_uri,
        "state":if expired {"expired"} else {row.state.as_str()},
        "failure_code":row.failure_code,"expires_at":row.expires_at,
        "resource_valid_until":row.resource_valid_until,
        "content_sha256":row.content_sha256.as_ref().map(|hash| governance::hex(hash)),
        "activation":"disabled","tool_grant":"none","content_trust":"untrusted"
    })
}

pub async fn request(
    pool: &PgPool,
    who: &Identity,
    request_id: Uuid,
    body: ReadRequest,
) -> Result<Value> {
    let mut tx = pool.begin().await?;
    governance::tenant_lock(&mut tx, who.tenant_id).await?;
    let approved = mcp_skill_approvals::approved_in_tx(&mut tx, who, request_id).await?;
    let transport: String = sqlx::query_scalar(
        "SELECT transport FROM mcp_servers WHERE tenant_id=$1 AND id=$2 AND active AND config_revision=$3",
    ).bind(who.tenant_id).bind(approved.server_id).bind(approved.server_config_revision)
        .fetch_optional(&mut *tx).await?.ok_or(Error::PolicyChanged)?;
    if transport != "streamable_http" {
        return Err(Error::UnsupportedCapability(
            "HTTPS MCP Skill resource reads",
        ));
    }
    let turn: Uuid = sqlx::query_scalar(
        "SELECT turn_id FROM mcp_skill_approval_requests WHERE tenant_id=$1 AND id=$2",
    )
    .bind(who.tenant_id)
    .bind(request_id)
    .fetch_one(&mut *tx)
    .await?;
    if body.turn_id != turn {
        return Err(Error::NotFound);
    }
    let expected_size = approved
        .validated
        .resource_size(&body.uri)
        .ok_or(Error::Invalid(
            "resource is not in the approved Skill manifest",
        ))? as i64;
    let existing: Option<ReadRow> = sqlx::query_as(&format!(
        "SELECT {READ_COLUMNS} FROM mcp_skill_resource_reads WHERE tenant_id=$1 AND id=$2"
    ))
    .bind(who.tenant_id)
    .bind(body.read_id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(existing) = existing {
        same_binding(&existing, who, request_id, &body)?;
        tx.commit().await?;
        return Ok(view(&existing));
    }
    let (count, total): (i64, i64) = sqlx::query_as(
        "SELECT count(*),coalesce(sum(expected_size),0)::bigint FROM mcp_skill_resource_reads WHERE tenant_id=$1 AND turn_id=$2",
    ).bind(who.tenant_id).bind(turn).fetch_one(&mut *tx).await?;
    if count >= MAX_TURN_READS || total.saturating_add(expected_size) > MAX_TURN_REQUESTED_BYTES {
        return Err(Error::Busy);
    }
    sqlx::query("INSERT INTO mcp_skill_resource_reads(tenant_id,id,request_id,turn_id,key_id,server_id,server_config_revision,skill_uri,resource_uri,expected_size,manifest_hash,authorization_context_hash,expires_at) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13)")
        .bind(who.tenant_id).bind(body.read_id).bind(request_id).bind(turn).bind(who.key_id)
        .bind(approved.server_id).bind(approved.server_config_revision)
        .bind(&approved.skill_uri).bind(&body.uri).bind(expected_size)
        .bind(&approved.manifest_hash).bind(&approved.authorization_context_hash)
        .bind(approved.expires_at).execute(&mut *tx).await?;
    let turn_row = governance::turn_locked(&mut tx, who, turn, true).await?;
    governance::event(&mut tx, &turn_row, "skill.resource_queued",
        json!({"read_id":body.read_id,"request_id":request_id,"server_id":approved.server_id,"uri":body.uri})).await?;
    tx.commit().await?;
    let created = row(pool, who.tenant_id, body.read_id)
        .await?
        .ok_or(Error::ResultUnavailable)?;
    Ok(view(&created))
}

pub async fn status(
    pool: &PgPool,
    cipher: &ResultCipher,
    who: &Identity,
    id: Uuid,
) -> Result<Value> {
    let mut tx = pool.begin().await?;
    governance::tenant_lock(&mut tx, who.tenant_id).await?;
    let read: ReadRow = sqlx::query_as(&format!("SELECT {READ_COLUMNS} FROM mcp_skill_resource_reads WHERE tenant_id=$1 AND key_id=$2 AND id=$3"))
        .bind(who.tenant_id).bind(who.key_id).bind(id).fetch_optional(&mut *tx).await?.ok_or(Error::NotFound)?;
    let approved = mcp_skill_approvals::approved_in_tx(&mut tx, who, read.request_id).await?;
    if approved.manifest_hash != read.manifest_hash
        || approved.server_id != read.server_id
        || approved.server_config_revision != read.server_config_revision
        || approved.authorization_context_hash != read.authorization_context_hash
        || !approved.validated.contains_resource(&read.resource_uri)
    {
        return Err(Error::PolicyChanged);
    }
    let mut response = view(&read);
    if read.state == "ready" {
        if read
            .resource_valid_until
            .is_none_or(|until| until <= Utc::now())
        {
            return Err(Error::ResultUnavailable);
        }
        let ciphertext = read
            .content_ciphertext
            .as_ref()
            .ok_or(Error::ResultUnavailable)?;
        let bytes = cipher.decrypt(who.tenant_id, id, ciphertext)?;
        if read.content_sha256.as_deref() != Some(store::hash(&bytes).as_slice())
            || approved
                .validated
                .verify_resource(&read.resource_uri, &bytes)
                .is_err()
        {
            return Err(Error::ResultUnavailable);
        }
        response["mime_type"] = json!(read.mime_type);
        match read.content_kind.as_deref() {
            Some("text") => {
                response["text"] =
                    json!(String::from_utf8(bytes).map_err(|_| Error::ResultUnavailable)?);
            }
            Some("blob") => response["blob"] = json!(BASE64_STANDARD.encode(bytes)),
            _ => return Err(Error::ResultUnavailable),
        }
    }
    tx.commit().await?;
    Ok(response)
}

#[derive(Clone)]
pub struct Worker {
    pool: PgPool,
    result_cipher: Arc<ResultCipher>,
    config_cipher: Arc<ResultCipher>,
    resolver: InjectedCredentialResolver,
}

impl Worker {
    pub fn new(
        pool: PgPool,
        result_cipher: Arc<ResultCipher>,
        config_cipher: Arc<ResultCipher>,
        resolver: InjectedCredentialResolver,
    ) -> Self {
        Self {
            pool,
            result_cipher,
            config_cipher,
            resolver,
        }
    }

    pub async fn tick(&self) -> Result<bool> {
        self.maintain().await?;
        let Some(read) = self.reserve().await? else {
            return Ok(false);
        };
        self.execute(read).await?;
        Ok(true)
    }

    async fn reserve(&self) -> Result<Option<ReadRow>> {
        let mut tx = self.pool.begin().await?;
        let found: Option<(Uuid,Uuid)> = sqlx::query_as(
            "SELECT tenant_id,id FROM mcp_skill_resource_reads WHERE state='queued' AND expires_at>clock_timestamp() ORDER BY created_at,id LIMIT 1 FOR UPDATE SKIP LOCKED",
        ).fetch_optional(&mut *tx).await?;
        let Some((tenant, id)) = found else {
            tx.commit().await?;
            return Ok(None);
        };
        let claim = Uuid::new_v4();
        sqlx::query("UPDATE mcp_skill_resource_reads SET state='reading',claim_id=$3,lease_until=LEAST(expires_at,clock_timestamp()+interval '90 seconds') WHERE tenant_id=$1 AND id=$2 AND state='queued'")
            .bind(tenant).bind(id).bind(claim).execute(&mut *tx).await?;
        tx.commit().await?;
        row(&self.pool, tenant, id)
            .await?
            .ok_or(Error::NotFound)
            .map(Some)
    }

    async fn execute(&self, read: ReadRow) -> Result<()> {
        let who = Identity {
            tenant_id: read.tenant_id,
            key_id: read.key_id,
        };
        let approved = match self.gate(&who, &read).await {
            Ok(value) => value,
            Err(_) => return self.fail(&read, "approval_or_manifest_changed").await,
        };
        let observed = match mcp_client::read_skill_resource_from_server(
            &self.pool,
            &self.config_cipher,
            &self.resolver,
            read.tenant_id,
            read.server_id,
            &read.resource_uri,
        )
        .await
        {
            Ok(value) => value,
            Err(_) => return self.fail(&read, "upstream_read_failed").await,
        };
        if observed.resource.ttl_ms == 0 {
            return self.fail(&read, "uncacheable_result").await;
        }
        if observed.server_revision != read.server_config_revision
            || observed.resource.bytes.len() as i64 != read.expected_size
            || approved
                .validated
                .verify_resource(&read.resource_uri, &observed.resource.bytes)
                .is_err()
        {
            return self.fail(&read, "integrity_mismatch").await;
        }
        let current_auth = match self
            .authorization_context(&read, &observed.authorization_fingerprint)
            .await
        {
            Ok(value) => value,
            Err(_) => return self.fail(&read, "authorization_context_changed").await,
        };
        let became_private = matches!(
            observed.discovery_cache_scope,
            rmcp::model::CacheScope::Private
        ) || matches!(
            observed.resource.cache_scope,
            rmcp::model::CacheScope::Private
        );
        if current_auth != read.authorization_context_hash
            || (became_private && current_auth.is_none())
        {
            return self.fail(&read, "authorization_context_changed").await;
        }
        let bytes = observed.resource.bytes;
        let ciphertext = self
            .result_cipher
            .encrypt(read.tenant_id, read.id, &bytes)?;
        let hash = store::hash(&bytes);
        let mut tx = self.pool.begin().await?;
        governance::tenant_lock(&mut tx, read.tenant_id).await?;
        if self.gate_in_tx(&mut tx, &who, &read).await.is_err() {
            drop(tx);
            return self.fail(&read, "approval_or_manifest_changed").await;
        }
        let changed = sqlx::query("UPDATE mcp_skill_resource_reads SET state='ready',claim_id=NULL,lease_until=NULL,content_sha256=$4,content_ciphertext=$5,mime_type=$6,content_kind=$7,resource_valid_until=LEAST(expires_at,clock_timestamp()+($8::bigint*interval '1 millisecond')),completed_at=clock_timestamp() WHERE tenant_id=$1 AND id=$2 AND state='reading' AND claim_id=$3 AND lease_until>clock_timestamp()")
            .bind(read.tenant_id).bind(read.id).bind(read.claim_id)
            .bind(&hash).bind(ciphertext).bind(observed.resource.mime_type)
            .bind(observed.resource.kind).bind(observed.resource.ttl_ms as i64)
            .execute(&mut *tx).await?;
        if changed.rows_affected() != 1 {
            return Err(Error::ResultUnavailable);
        }
        sqlx::query("INSERT INTO audit_events(tenant_id,action,object_id,details) VALUES($1,'mcp.skill_resource_verified',$2,$3)")
            .bind(read.tenant_id).bind(read.id.to_string())
            .bind(json!({"request_id":read.request_id,"uri":read.resource_uri,"sha256":governance::hex(&hash),"bytes":bytes.len()}))
            .execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn gate(
        &self,
        who: &Identity,
        read: &ReadRow,
    ) -> Result<mcp_skill_approvals::ApprovedSkill> {
        let mut tx = self.pool.begin().await?;
        governance::tenant_lock(&mut tx, who.tenant_id).await?;
        let approved = self.gate_in_tx(&mut tx, who, read).await?;
        tx.commit().await?;
        Ok(approved)
    }

    async fn gate_in_tx(
        &self,
        tx: &mut governance::Tx<'_>,
        who: &Identity,
        read: &ReadRow,
    ) -> Result<mcp_skill_approvals::ApprovedSkill> {
        let approved = mcp_skill_approvals::approved_in_tx(tx, who, read.request_id).await?;
        if approved.server_id != read.server_id
            || approved.server_config_revision != read.server_config_revision
            || approved.skill_uri != read.skill_uri
            || approved.manifest_hash != read.manifest_hash
            || approved.authorization_context_hash != read.authorization_context_hash
            || approved.validated.resource_size(&read.resource_uri)
                != Some(read.expected_size as u64)
        {
            return Err(Error::PolicyChanged);
        }
        Ok(approved)
    }

    async fn authorization_context(
        &self,
        read: &ReadRow,
        fingerprint: &str,
    ) -> Result<Option<Vec<u8>>> {
        if read.authorization_context_hash.is_none() {
            return Ok(None);
        }
        let credential_envelope: Vec<u8> = sqlx::query_scalar(
            "SELECT coalesce(credential_ref_ciphertext,'') FROM mcp_servers WHERE tenant_id=$1 AND id=$2 AND active AND config_revision=$3",
        ).bind(read.tenant_id).bind(read.server_id).bind(read.server_config_revision)
            .fetch_optional(&self.pool).await?.ok_or(Error::PolicyChanged)?;
        let mut material = Vec::with_capacity(64 + credential_envelope.len());
        material.extend_from_slice(read.tenant_id.as_bytes());
        material.extend_from_slice(read.server_id.as_bytes());
        material.extend_from_slice(&read.server_config_revision.to_be_bytes());
        material.extend_from_slice(&credential_envelope);
        material.extend_from_slice(fingerprint.as_bytes());
        Ok(Some(store::hash(&material)))
    }

    async fn fail(&self, read: &ReadRow, code: &str) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let changed = sqlx::query("UPDATE mcp_skill_resource_reads SET state='failed',claim_id=NULL,lease_until=NULL,failure_code=$4,completed_at=clock_timestamp() WHERE tenant_id=$1 AND id=$2 AND state='reading' AND claim_id=$3")
            .bind(read.tenant_id).bind(read.id).bind(read.claim_id).bind(code)
            .execute(&mut *tx).await?;
        if changed.rows_affected() == 1 {
            sqlx::query("INSERT INTO audit_events(tenant_id,action,object_id,details) VALUES($1,'mcp.skill_resource_failed',$2,$3)")
                .bind(read.tenant_id).bind(read.id.to_string()).bind(json!({"reason":code}))
                .execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn maintain(&self) -> Result<()> {
        sqlx::query("WITH due AS (SELECT tenant_id,id FROM mcp_skill_resource_reads WHERE state='reading' AND lease_until<=clock_timestamp() ORDER BY lease_until,id LIMIT 100 FOR UPDATE SKIP LOCKED), changed AS (UPDATE mcp_skill_resource_reads r SET state='failed',claim_id=NULL,lease_until=NULL,failure_code='read_lease_expired',completed_at=clock_timestamp() FROM due WHERE r.tenant_id=due.tenant_id AND r.id=due.id RETURNING r.tenant_id,r.id) INSERT INTO audit_events(tenant_id,action,object_id,details) SELECT tenant_id,'mcp.skill_resource_failed',id::text,'{\"reason\":\"read_lease_expired\"}'::jsonb FROM changed")
            .execute(&self.pool).await?;
        sqlx::query("WITH due AS (SELECT tenant_id,id FROM mcp_skill_resource_reads WHERE state='queued' AND expires_at<=clock_timestamp() ORDER BY expires_at,id LIMIT 100 FOR UPDATE SKIP LOCKED), changed AS (UPDATE mcp_skill_resource_reads r SET state='failed',failure_code='turn_expired',completed_at=clock_timestamp() FROM due WHERE r.tenant_id=due.tenant_id AND r.id=due.id RETURNING r.tenant_id,r.id) INSERT INTO audit_events(tenant_id,action,object_id,details) SELECT tenant_id,'mcp.skill_resource_failed',id::text,'{\"reason\":\"turn_expired\"}'::jsonb FROM changed")
            .execute(&self.pool).await?;
        sqlx::query("WITH due AS (SELECT tenant_id,id FROM mcp_skill_resource_reads WHERE state='ready' AND (expires_at<=clock_timestamp() OR resource_valid_until<=clock_timestamp()) ORDER BY resource_valid_until,id LIMIT 100 FOR UPDATE SKIP LOCKED), changed AS (UPDATE mcp_skill_resource_reads r SET state='expired',content_sha256=NULL,content_ciphertext=NULL,content_kind=NULL,mime_type=NULL,completed_at=clock_timestamp() FROM due WHERE r.tenant_id=due.tenant_id AND r.id=due.id RETURNING r.tenant_id,r.id) INSERT INTO audit_events(tenant_id,action,object_id,details) SELECT tenant_id,'mcp.skill_resource_expired',id::text,'{}'::jsonb FROM changed")
            .execute(&self.pool).await?;
        Ok(())
    }
}
