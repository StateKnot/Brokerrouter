//! Durable, turn-bound MCP tool calls. Only the isolated connector process
//! dispatches an upstream tool; the public edge handles authorization,
//! governance checks, idempotency and post-output release.

use crate::{
    cipher::ResultCipher,
    error::{Error, Result},
    governance::{self, Check},
    mcp_catalog::{self, AuthorizedTool},
    mcp_client::{self, InjectedCredentialResolver, ToolCallError, ToolCallWireResult},
    store::{self, Identity},
};
use rand::RngCore;
use rmcp::model::{CallToolResponse, CallToolResult};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

const MAX_RESULT_BYTES: usize = 4 * 1024 * 1024;
const INPUT_OPERATION_PREFIX: &str = "mcp-tool:";
const OUTPUT_OPERATION_PREFIX: &str = "mcp-output:";

#[derive(Clone)]
pub struct Edge {
    pool: PgPool,
    cipher: Arc<ResultCipher>,
}

impl Edge {
    pub fn new(pool: PgPool, cipher: Arc<ResultCipher>) -> Self {
        Self { pool, cipher }
    }

    pub async fn call(
        &self,
        who: &Identity,
        turn_id: Uuid,
        call_id: Uuid,
        tool: AuthorizedTool,
        arguments: Value,
    ) -> Result<CallToolResponse> {
        if !arguments.is_object() {
            return Err(Error::Invalid("MCP tool arguments must be an object"));
        }
        let arguments_hash = governance::digest(&arguments)?;
        let existing = call_row(&self.pool, who, call_id).await?;
        if let Some(row) = existing {
            verify_call_identity(&self.pool, &row, who, turn_id, &tool, &arguments_hash).await?;
            return self.release(who, row).await;
        }
        let operation_key = format!("{INPUT_OPERATION_PREFIX}{call_id}");
        let mut payload = json!({
            "call_id":call_id,
            "server_id":tool.server_id,
            "server_revision":tool.server_config_revision,
            "snapshot_id":tool.discovery_snapshot_id,
            "upstream_name":tool.upstream_name,
            "downstream_name":tool.downstream_name,
            "descriptor_sha256":governance::hex(&tool.descriptor_hash),
            "arguments":arguments,
        });
        // A discovery refresh is not a new action. Preserve the exact payload
        // a human reviewed, including its historical snapshot audit reference.
        let previous: Option<(Uuid, Uuid, Vec<u8>)> = sqlx::query_as(
            "SELECT id,turn_id,payload_ciphertext FROM governance_operations WHERE tenant_id=$1 AND key_id=$2 AND operation_key=$3 AND stage='tool_input'",
        )
        .bind(who.tenant_id)
        .bind(who.key_id)
        .bind(&operation_key)
        .fetch_optional(&self.pool)
        .await?;
        if let Some((id, original_turn, ciphertext)) = previous {
            if original_turn != turn_id {
                return Err(Error::Conflict);
            }
            let original: Value =
                serde_json::from_slice(&self.cipher.decrypt(who.tenant_id, id, &ciphertext)?)
                    .map_err(|_| Error::ResultUnavailable)?;
            let original_snapshot = parse_id(&original["snapshot_id"])?;
            if !same_authorization_context(
                &self.pool,
                who.tenant_id,
                original_snapshot,
                tool.discovery_snapshot_id,
            )
            .await?
            {
                return Err(Error::Conflict);
            }
            payload["snapshot_id"] = json!(original_snapshot);
            if original != payload {
                return Err(Error::Conflict);
            }
        }
        let original_snapshot = parse_id(&payload["snapshot_id"])?;
        let decision = governance::check(
            &self.pool,
            &self.cipher,
            who,
            Check {
                turn_id,
                operation_key,
                stage: "tool_input".into(),
                model_id: None,
                tool_name: Some(tool.downstream_name.clone()),
                payload,
            },
        )
        .await?;
        governance::require_ready(decision.clone())?;
        let input_operation_id = parse_id(&decision["id"])?;
        let mut tx = self.pool.begin().await?;
        governance::tenant_lock(&mut tx, who.tenant_id).await?;
        let turn = governance::turn_locked(&mut tx, who, turn_id, true).await?;
        let inserted = sqlx::query(
            "INSERT INTO mcp_tool_calls(tenant_id,id,key_id,turn_id,input_operation_id,server_id,server_config_revision,discovery_snapshot_id,upstream_name,downstream_name,descriptor_hash,arguments_hash,expires_at)
             VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13)
             ON CONFLICT(tenant_id,id) DO NOTHING",
        )
        .bind(who.tenant_id)
        .bind(call_id)
        .bind(who.key_id)
        .bind(turn_id)
        .bind(input_operation_id)
        .bind(tool.server_id)
        .bind(tool.server_config_revision)
        .bind(original_snapshot)
        .bind(&tool.upstream_name)
        .bind(&tool.downstream_name)
        .bind(&tool.descriptor_hash)
        .bind(&arguments_hash)
        .bind(turn.expires_at)
        .execute(&mut *tx)
        .await?;
        if inserted.rows_affected() == 1 {
            sqlx::query("INSERT INTO audit_events(tenant_id,action,object_id,details) VALUES($1,'mcp.tool_call_queued',$2,$3)")
            .bind(who.tenant_id).bind(call_id.to_string())
            .bind(json!({"key_id":who.key_id,"turn_id":turn_id,"tool":tool.downstream_name,"arguments_sha256":governance::hex(&arguments_hash)}))
            .execute(&mut *tx).await?;
        }
        tx.commit().await?;
        let row = call_row(&self.pool, who, call_id)
            .await?
            .ok_or(Error::ResultUnavailable)?;
        verify_call_identity(&self.pool, &row, who, turn_id, &tool, &arguments_hash).await?;
        self.release(who, row).await
    }

    async fn release(&self, who: &Identity, row: CallRow) -> Result<CallToolResponse> {
        let mut tx = self.pool.begin().await?;
        governance::tenant_lock(&mut tx, who.tenant_id).await?;
        governance::turn_locked(&mut tx, who, row.turn_id, true).await?;
        tx.commit().await?;
        match row.state.as_str() {
            "queued" | "authorizing" | "executing" => Err(Error::InProgress),
            "execution_unknown" => Err(Error::UpstreamUnknown),
            "failed" => Err(Error::NotSubmitted),
            "input_required" | "task" => Err(Error::InProgress),
            "expired" => Err(Error::ResultUnavailable),
            "output_pending" | "completed" => {
                let encrypted = row
                    .response_ciphertext
                    .as_deref()
                    .ok_or(Error::ResultUnavailable)?;
                let plaintext = self.cipher.decrypt(who.tenant_id, row.id, encrypted)?;
                let hash = store::hash(&plaintext);
                if row.response_hash.as_deref() != Some(hash.as_slice()) {
                    return Err(Error::ResultUnavailable);
                }
                let payload: Value =
                    serde_json::from_slice(&plaintext).map_err(|_| Error::ResultUnavailable)?;
                let result: CallToolResult = serde_json::from_value(payload.clone())
                    .map_err(|_| Error::ResultUnavailable)?;
                let decision = governance::check(
                    &self.pool,
                    &self.cipher,
                    who,
                    Check {
                        turn_id: row.turn_id,
                        operation_key: format!("{OUTPUT_OPERATION_PREFIX}{}", row.id),
                        stage: "tool_output".into(),
                        model_id: None,
                        tool_name: Some(row.downstream_name.clone()),
                        payload: payload.clone(),
                    },
                )
                .await?;
                governance::require_ready(decision.clone())?;
                let output_operation_id = parse_id(&decision["id"])?;
                let mut tx = self.pool.begin().await?;
                governance::tenant_lock(&mut tx, who.tenant_id).await?;
                let turn = governance::turn_locked(&mut tx, who, row.turn_id, true).await?;
                if !current_call_authorized(&mut tx, who, &row).await? {
                    return Err(Error::Unauthorized);
                }
                let operation: governance::Operation = sqlx::query_as(
                    "SELECT * FROM governance_operations WHERE tenant_id=$1 AND id=$2 AND key_id=$3 AND turn_id=$4 AND stage='tool_output' FOR UPDATE",
                )
                .bind(who.tenant_id).bind(output_operation_id).bind(who.key_id).bind(row.turn_id)
                .fetch_optional(&mut *tx).await?.ok_or(Error::ResultUnavailable)?;
                let (gate, grant_deadline) =
                    governance::gate_with_grant_deadline(&mut tx, who, &turn, &operation, &payload)
                        .await?;
                match gate {
                    governance::Gate::Ready => {}
                    governance::Gate::Approval(approval) => {
                        tx.commit().await?;
                        return Err(Error::ApprovalRequired(approval));
                    }
                    governance::Gate::Checking(_) => {
                        tx.commit().await?;
                        return Err(Error::InProgress);
                    }
                }
                if row.state == "completed" {
                    governance::require_live_grants(&mut tx, grant_deadline).await?;
                    tx.commit().await?;
                    return Ok(result.into());
                }
                let updated = sqlx::query("UPDATE mcp_tool_calls SET state='completed',output_operation_id=$3,completed_at=clock_timestamp() WHERE tenant_id=$1 AND id=$2 AND state='output_pending'")
                    .bind(who.tenant_id).bind(row.id).bind(output_operation_id)
                    .execute(&mut *tx).await?;
                if updated.rows_affected() != 1 {
                    return Err(Error::Conflict);
                }
                governance::require_live_grants(&mut tx, grant_deadline).await?;
                tx.commit().await?;
                Ok(result.into())
            }
            _ => Err(Error::Configuration),
        }
    }
}

async fn current_call_authorized(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    who: &Identity,
    row: &CallRow,
) -> Result<bool> {
    sqlx::query_scalar(
        "SELECT EXISTS(
           SELECT 1 FROM mcp_discovery_snapshots original
           JOIN mcp_servers s ON s.tenant_id=original.tenant_id AND s.id=original.server_id AND s.active
           JOIN LATERAL (
             SELECT d.id,d.server_config_revision,d.authorization_context_hash,d.expires_at
             FROM mcp_discovery_snapshots d WHERE d.tenant_id=s.tenant_id AND d.server_id=s.id
               AND d.server_config_revision=s.config_revision
             ORDER BY d.observed_at DESC,d.id DESC LIMIT 1
           ) current ON current.expires_at>clock_timestamp()
           JOIN mcp_discovered_tools t ON t.tenant_id=s.tenant_id AND t.discovery_snapshot_id=current.id
             AND t.server_id=s.id AND t.expires_at>clock_timestamp()
           JOIN mcp_key_tool_grants g ON g.tenant_id=t.tenant_id AND g.key_id=$2 AND g.server_id=t.server_id
             AND g.server_config_revision=current.server_config_revision AND g.upstream_name=t.upstream_name
             AND g.descriptor_hash=t.descriptor_hash AND g.active
           WHERE original.tenant_id=$1 AND original.id=$3 AND s.id=$4 AND s.config_revision=$5
             AND original.authorization_context_hash IS NOT DISTINCT FROM current.authorization_context_hash
             AND t.upstream_name=$6 AND t.downstream_name=$7 AND t.descriptor_hash=$8
             AND (t.cache_scope='public' OR current.authorization_context_hash IS NOT NULL)
         )",
    )
    .bind(who.tenant_id).bind(who.key_id).bind(row.discovery_snapshot_id).bind(row.server_id)
    .bind(row.server_config_revision).bind(&row.upstream_name).bind(&row.downstream_name)
    .bind(&row.descriptor_hash).fetch_one(&mut **tx).await.map_err(Into::into)
}

fn parse_id(value: &Value) -> Result<Uuid> {
    value
        .as_str()
        .and_then(|value| Uuid::parse_str(value).ok())
        .ok_or(Error::Configuration)
}

#[derive(sqlx::FromRow)]
struct CallRow {
    tenant_id: Uuid,
    id: Uuid,
    key_id: Uuid,
    turn_id: Uuid,
    input_operation_id: Uuid,
    server_id: Uuid,
    server_config_revision: i64,
    discovery_snapshot_id: Uuid,
    upstream_name: String,
    downstream_name: String,
    descriptor_hash: Vec<u8>,
    arguments_hash: Vec<u8>,
    state: String,
    response_hash: Option<Vec<u8>>,
    response_ciphertext: Option<Vec<u8>>,
}

async fn call_row(pool: &PgPool, who: &Identity, call_id: Uuid) -> Result<Option<CallRow>> {
    sqlx::query_as(
        "SELECT tenant_id,id,key_id,turn_id,input_operation_id,server_id,server_config_revision,discovery_snapshot_id,upstream_name,downstream_name,descriptor_hash,arguments_hash,state,response_hash,response_ciphertext
         FROM mcp_tool_calls WHERE tenant_id=$1 AND key_id=$2 AND id=$3",
    )
    .bind(who.tenant_id)
    .bind(who.key_id)
    .bind(call_id)
    .fetch_optional(pool)
    .await
    .map_err(Into::into)
}

async fn verify_call_identity(
    pool: &PgPool,
    row: &CallRow,
    who: &Identity,
    turn_id: Uuid,
    tool: &AuthorizedTool,
    arguments_hash: &[u8],
) -> Result<()> {
    if row.tenant_id != who.tenant_id
        || row.key_id != who.key_id
        || row.turn_id != turn_id
        || row.server_id != tool.server_id
        || row.server_config_revision != tool.server_config_revision
        || row.upstream_name != tool.upstream_name
        || row.downstream_name != tool.downstream_name
        || row.descriptor_hash != tool.descriptor_hash
        || row.arguments_hash != arguments_hash
    {
        return Err(Error::Conflict);
    }
    if !same_authorization_context(
        pool,
        who.tenant_id,
        row.discovery_snapshot_id,
        tool.discovery_snapshot_id,
    )
    .await?
    {
        return Err(Error::Conflict);
    }
    Ok(())
}

async fn same_authorization_context(
    pool: &PgPool,
    tenant: Uuid,
    original: Uuid,
    current: Uuid,
) -> Result<bool> {
    let same: Option<bool> = sqlx::query_scalar(
        "SELECT old.authorization_context_hash IS NOT DISTINCT FROM current.authorization_context_hash
         FROM mcp_discovery_snapshots old JOIN mcp_discovery_snapshots current
           ON current.tenant_id=old.tenant_id AND current.server_id=old.server_id
         WHERE old.tenant_id=$1 AND old.id=$2 AND current.id=$3",
    )
    .bind(tenant)
    .bind(original)
    .bind(current)
    .fetch_optional(pool)
    .await?;
    Ok(same.unwrap_or(false))
}

#[derive(Clone)]
pub struct Worker {
    pool: PgPool,
    result_cipher: Arc<ResultCipher>,
    config_cipher: Arc<ResultCipher>,
    resolver: InjectedCredentialResolver,
    stdio_registry_path: Option<PathBuf>,
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
            stdio_registry_path: None,
        }
    }

    pub fn with_stdio_registry_path(mut self, path: PathBuf) -> Self {
        self.stdio_registry_path = Some(path);
        self
    }

    pub async fn tick(&self) -> Result<bool> {
        self.maintain().await?;
        let Some((row, token)) = self.reserve().await? else {
            return Ok(false);
        };
        self.execute(row, token).await?;
        Ok(true)
    }

    async fn reserve(&self) -> Result<Option<(CallRow, String)>> {
        let mut tx = self.pool.begin().await?;
        let candidate: Option<(Uuid, Uuid)> = sqlx::query_as(
            "SELECT c.tenant_id,c.id FROM mcp_tool_calls c
             JOIN governance_operations o ON o.tenant_id=c.tenant_id AND o.id=c.input_operation_id
             WHERE c.state='queued' AND o.state='ready' AND c.expires_at>clock_timestamp()
             ORDER BY c.created_at,c.id LIMIT 1 FOR UPDATE OF c SKIP LOCKED",
        )
        .fetch_optional(&mut *tx)
        .await?;
        let Some((tenant, id)) = candidate else {
            tx.commit().await?;
            return Ok(None);
        };
        let mut random = [0_u8; 32];
        rand::rng().fill_bytes(&mut random);
        let token = governance::hex(&random);
        let hash = store::hash(token.as_bytes());
        sqlx::query("UPDATE mcp_tool_calls SET state='authorizing',claim_hash=$3,execution_deadline=LEAST(expires_at,clock_timestamp()+interval '180 seconds') WHERE tenant_id=$1 AND id=$2")
            .bind(tenant).bind(id).bind(hash).execute(&mut *tx).await?;
        tx.commit().await?;
        let who = Identity {
            tenant_id: tenant,
            key_id: sqlx::query_scalar(
                "SELECT key_id FROM mcp_tool_calls WHERE tenant_id=$1 AND id=$2",
            )
            .bind(tenant)
            .bind(id)
            .fetch_one(&self.pool)
            .await?,
        };
        let row = call_row(&self.pool, &who, id)
            .await?
            .ok_or(Error::NotFound)?;
        Ok(Some((row, token)))
    }

    async fn execute(&self, row: CallRow, token: String) -> Result<()> {
        let who = Identity {
            tenant_id: row.tenant_id,
            key_id: row.key_id,
        };
        let current = mcp_catalog::authorized_tool(&self.pool, &who, &row.downstream_name).await?;
        let valid = current.as_ref().is_some_and(|tool| {
            tool.server_id == row.server_id
                && tool.server_config_revision == row.server_config_revision
                && tool.upstream_name == row.upstream_name
                && tool.descriptor_hash == row.descriptor_hash
        });
        if !valid {
            return self.fail_before_submit(&row, "authorization_changed").await;
        }
        let current = current.ok_or(Error::Configuration)?;
        if !same_authorization_context(
            &self.pool,
            row.tenant_id,
            row.discovery_snapshot_id,
            current.discovery_snapshot_id,
        )
        .await?
        {
            return self.fail_before_submit(&row, "authorization_changed").await;
        }
        let server = match mcp_catalog::connector_server(
            &self.pool,
            &self.config_cipher,
            row.tenant_id,
            row.server_id,
        )
        .await
        {
            Ok(server) if server.config_revision == row.server_config_revision => server,
            _ => {
                return self
                    .fail_before_submit(&row, "server_configuration_changed")
                    .await;
            }
        };
        let input_payload: Vec<u8> = sqlx::query_scalar(
            "SELECT payload_ciphertext FROM governance_operations WHERE tenant_id=$1 AND id=$2",
        )
        .bind(row.tenant_id)
        .bind(row.input_operation_id)
        .fetch_one(&self.pool)
        .await?;
        let payload: Value = serde_json::from_slice(&self.result_cipher.decrypt(
            row.tenant_id,
            row.input_operation_id,
            &input_payload,
        )?)
        .map_err(|_| Error::ResultUnavailable)?;
        let arguments = payload["arguments"]
            .as_object()
            .cloned()
            .ok_or(Error::ResultUnavailable)?;
        if governance::digest(&Value::Object(arguments.clone()))? != row.arguments_hash {
            return self.fail_before_submit(&row, "arguments_changed").await;
        }
        let op_hash: Vec<u8> = sqlx::query_scalar(
            "SELECT payload_hash FROM governance_operations WHERE tenant_id=$1 AND id=$2",
        )
        .bind(row.tenant_id)
        .bind(row.input_operation_id)
        .fetch_one(&self.pool)
        .await?;
        let claim = governance::claim(
            &self.pool,
            &self.result_cipher,
            &who,
            row.input_operation_id,
            governance::Claim {
                token: token.clone(),
                payload_hash: governance::hex(&op_hash),
            },
        )
        .await;
        match claim {
            Ok(value) if value["dispatch_authorized"] == true => {}
            Err(Error::ApprovalRequired(_)) | Err(Error::InProgress) => {
                sqlx::query("UPDATE mcp_tool_calls SET state='queued',claim_hash=NULL,execution_deadline=NULL WHERE tenant_id=$1 AND id=$2 AND state='authorizing'")
                    .bind(row.tenant_id).bind(row.id).execute(&self.pool).await?;
                return Ok(());
            }
            Err(
                Error::TurnClosed | Error::ActionRejected | Error::NotFound | Error::Unauthorized,
            ) => {
                return self
                    .fail_before_submit(&row, "governance_claim_denied")
                    .await;
            }
            _ => return Err(Error::UpstreamUnknown),
        }
        let mut dispatch_tx = self.pool.begin().await?;
        let started = sqlx::query("UPDATE mcp_tool_calls SET state='executing' WHERE tenant_id=$1 AND id=$2 AND state='authorizing' AND claim_hash=$3 AND execution_deadline>clock_timestamp()")
            .bind(row.tenant_id).bind(row.id).bind(store::hash(token.as_bytes()))
            .execute(&mut *dispatch_tx).await?;
        if started.rows_affected() != 1 {
            return Err(Error::UpstreamUnknown);
        }
        sqlx::query("INSERT INTO audit_events(tenant_id,action,object_id,details) VALUES($1,'mcp.tool_call_dispatched',$2,$3)")
            .bind(row.tenant_id).bind(row.id.to_string())
            .bind(json!({"server_id":row.server_id,"turn_id":row.turn_id,"tool":row.downstream_name}))
            .execute(&mut *dispatch_tx).await?;
        dispatch_tx.commit().await?;
        let response = tokio::time::timeout(
            Duration::from_secs(150),
            mcp_client::execute_tool(
                &server,
                &self.resolver,
                &row.upstream_name,
                &row.descriptor_hash,
                arguments,
                self.stdio_registry_path.as_deref(),
            ),
        )
        .await
        .unwrap_or(Err(ToolCallError::ExecutionUnknown));
        match response {
            Ok(response) => self.record_response(&row, &token, response).await,
            Err(ToolCallError::NotSubmitted) => self.finish_failure(&row, &token, false).await,
            Err(ToolCallError::ExecutionUnknown) => self.finish_failure(&row, &token, true).await,
        }
    }

    async fn fail_before_submit(&self, row: &CallRow, code: &str) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let changed = sqlx::query("UPDATE mcp_tool_calls SET state='failed',failure_code=$3,execution_deadline=NULL,completed_at=clock_timestamp() WHERE tenant_id=$1 AND id=$2 AND state='authorizing'")
            .bind(row.tenant_id).bind(row.id).bind(code).execute(&mut *tx).await?;
        if changed.rows_affected() != 1 {
            return Err(Error::UpstreamUnknown);
        }
        sqlx::query("INSERT INTO audit_events(tenant_id,action,object_id,details) VALUES($1,'mcp.tool_call_not_submitted',$2,$3)")
            .bind(row.tenant_id).bind(row.id.to_string()).bind(json!({"reason":code}))
            .execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn finish_failure(&self, row: &CallRow, token: &str, unknown: bool) -> Result<()> {
        let outcome = if unknown {
            "execution_unknown"
        } else {
            "completed"
        };
        let state = if unknown {
            "execution_unknown"
        } else {
            "failed"
        };
        let code = if unknown {
            "upstream_result_unknown"
        } else {
            "not_submitted"
        };
        let who = Identity {
            tenant_id: row.tenant_id,
            key_id: row.key_id,
        };
        let mut tx = self.pool.begin().await?;
        governance::tenant_lock(&mut tx, row.tenant_id).await?;
        governance::turn_locked(&mut tx, &who, row.turn_id, false).await?;
        governance::receipt_in_tx(
            &mut tx,
            &self.result_cipher,
            &who,
            row.input_operation_id,
            &governance::Receipt {
                token: token.to_owned(),
                outcome: outcome.into(),
                evidence_ref: format!("mcp-tool:{}:{code}", row.id),
            },
        )
        .await?;
        let changed=sqlx::query("UPDATE mcp_tool_calls SET state=$3,failure_code=$4,execution_deadline=NULL,completed_at=clock_timestamp() WHERE tenant_id=$1 AND id=$2 AND state IN ('executing','execution_unknown') AND claim_hash=$5")
            .bind(row.tenant_id).bind(row.id).bind(state).bind(if unknown {None} else {Some(code)})
            .bind(store::hash(token.as_bytes())).execute(&mut *tx).await?;
        if changed.rows_affected() != 1 {
            return Err(Error::Conflict);
        }
        sqlx::query(
            "INSERT INTO audit_events(tenant_id,action,object_id,details) VALUES($1,$2,$3,$4)",
        )
        .bind(row.tenant_id)
        .bind(if unknown {
            "mcp.tool_call_execution_unknown"
        } else {
            "mcp.tool_call_not_submitted"
        })
        .bind(row.id.to_string())
        .bind(json!({"reason":code}))
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn record_response(
        &self,
        row: &CallRow,
        token: &str,
        response: ToolCallWireResult,
    ) -> Result<()> {
        let (kind, value) = match response {
            ToolCallWireResult::Complete(result) => (
                "complete",
                serde_json::to_value(result).map_err(|_| Error::ResultUnavailable)?,
            ),
            ToolCallWireResult::InputRequired(result) => (
                "input_required",
                serde_json::to_value(result).map_err(|_| Error::ResultUnavailable)?,
            ),
            ToolCallWireResult::Task(result) => (
                "task",
                serde_json::to_value(result).map_err(|_| Error::ResultUnavailable)?,
            ),
        };
        let encoded = serde_json::to_vec(&value).map_err(|_| Error::ResultUnavailable)?;
        if encoded.len() > MAX_RESULT_BYTES {
            return self.finish_failure(row, token, true).await;
        }
        let encrypted = self
            .result_cipher
            .encrypt(row.tenant_id, row.id, &encoded)?;
        let hash = store::hash(&encoded);
        let state = if kind == "complete" {
            "output_pending"
        } else {
            kind
        };
        let who = Identity {
            tenant_id: row.tenant_id,
            key_id: row.key_id,
        };
        let mut tx = self.pool.begin().await?;
        governance::tenant_lock(&mut tx, row.tenant_id).await?;
        governance::turn_locked(&mut tx, &who, row.turn_id, false).await?;
        governance::receipt_in_tx(
            &mut tx,
            &self.result_cipher,
            &who,
            row.input_operation_id,
            &governance::Receipt {
                token: token.to_owned(),
                outcome: "completed".into(),
                evidence_ref: format!("mcp-tool:{}:{}", row.id, governance::hex(&hash)),
            },
        )
        .await?;
        let changed=sqlx::query("UPDATE mcp_tool_calls SET state=$3,response_kind=$4,response_hash=$5,response_ciphertext=$6,execution_deadline=NULL,completed_at=CASE WHEN $3='output_pending' THEN NULL ELSE clock_timestamp() END WHERE tenant_id=$1 AND id=$2 AND state IN ('executing','execution_unknown') AND claim_hash=$7")
            .bind(row.tenant_id).bind(row.id).bind(state).bind(kind).bind(&hash).bind(encrypted).bind(store::hash(token.as_bytes()))
            .execute(&mut *tx).await?;
        if changed.rows_affected() != 1 {
            return Err(Error::Conflict);
        }
        sqlx::query("INSERT INTO audit_events(tenant_id,action,object_id,details) VALUES($1,'mcp.tool_call_result_recorded',$2,$3)")
            .bind(row.tenant_id).bind(row.id.to_string())
            .bind(json!({"kind":kind,"response_sha256":governance::hex(&hash)}))
            .execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn maintain(&self) -> Result<()> {
        sqlx::query(r#"WITH due AS (SELECT tenant_id,id FROM mcp_tool_calls WHERE state IN ('authorizing','executing') AND execution_deadline<=clock_timestamp() ORDER BY execution_deadline,id LIMIT 100 FOR UPDATE SKIP LOCKED), changed AS (UPDATE mcp_tool_calls c SET state='execution_unknown',execution_deadline=NULL,completed_at=clock_timestamp() FROM due WHERE c.tenant_id=due.tenant_id AND c.id=due.id RETURNING c.tenant_id,c.id) INSERT INTO audit_events(tenant_id,action,object_id,details) SELECT tenant_id,'mcp.tool_call_execution_unknown',id::text,'{"reason":"lease_expired"}'::jsonb FROM changed"#)
            .execute(&self.pool).await?;
        sqlx::query(r#"WITH due AS (SELECT tenant_id,id FROM mcp_tool_calls WHERE state='queued' AND expires_at<=clock_timestamp() ORDER BY expires_at,id LIMIT 100 FOR UPDATE SKIP LOCKED), changed AS (UPDATE mcp_tool_calls c SET state='failed',failure_code='turn_expired',completed_at=clock_timestamp() FROM due WHERE c.tenant_id=due.tenant_id AND c.id=due.id RETURNING c.tenant_id,c.id) INSERT INTO audit_events(tenant_id,action,object_id,details) SELECT tenant_id,'mcp.tool_call_not_submitted',id::text,'{"reason":"turn_expired"}'::jsonb FROM changed"#)
            .execute(&self.pool).await?;
        sqlx::query("WITH due AS (SELECT tenant_id,id FROM mcp_tool_calls WHERE state IN ('input_required','task','output_pending','completed') AND expires_at<=clock_timestamp() ORDER BY expires_at,id LIMIT 100 FOR UPDATE SKIP LOCKED), changed AS (UPDATE mcp_tool_calls c SET state='expired',response_ciphertext=NULL,response_hash=NULL,response_kind=NULL FROM due WHERE c.tenant_id=due.tenant_id AND c.id=due.id RETURNING c.tenant_id,c.id) INSERT INTO audit_events(tenant_id,action,object_id,details) SELECT tenant_id,'mcp.tool_call_result_expired',id::text,'{}'::jsonb FROM changed")
            .execute(&self.pool).await?;
        Ok(())
    }
}
