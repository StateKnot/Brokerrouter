use crate::{
    accounting::{self, Admission, Reservation},
    attempts,
    cipher::ResultCipher,
    egress,
    error::{Error, Result},
    governance,
    money::{self, PriceSnapshot},
    protocol::{self, ValidatedRequest},
    store::{self, Endpoint, Identity},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::sync::Semaphore;
use uuid::Uuid;

type AttemptRow = (
    Uuid,
    Uuid,
    i32,
    String,
    Option<i32>,
    Option<String>,
    Option<i64>,
);

#[derive(sqlx::FromRow)]
struct RequestLog {
    id: Uuid,
    model_id: String,
    status: String,
    created_at: chrono::DateTime<chrono::Utc>,
    final_micros: Option<i64>,
    reserve_micros: i64,
    guardrail_evaluation_id: Option<Uuid>,
}

#[derive(Clone, Copy)]
struct CallContext {
    turn_id: Option<Uuid>,
    judge: Option<(Uuid, i64)>,
}

fn reserved_cny(status: &str, maximum: i64) -> String {
    money::format_micros(
        if matches!(status, "reserved" | "submitting" | "submission_unknown") {
            maximum
        } else {
            0
        },
    )
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Route {
    pub endpoint: Endpoint,
    pub price: PriceSnapshot,
}

#[derive(Clone)]
pub struct Gateway {
    pub pool: PgPool,
    pub(crate) cipher: Arc<ResultCipher>,
    credentials: Arc<HashMap<String, String>>,
    capacity: Arc<Semaphore>,
    judge_capacity: Arc<Semaphore>,
    timeout_seconds: i32,
    result_ttl_seconds: i32,
    pub(crate) tasks: tokio_util::task::TaskTracker,
}
impl Gateway {
    pub fn new(
        pool: PgPool,
        cipher: ResultCipher,
        credentials: HashMap<String, String>,
        concurrency: usize,
        timeout_seconds: i32,
        result_ttl_seconds: i32,
    ) -> Result<Self> {
        if !(1..=1024).contains(&concurrency)
            || !(1..=600).contains(&timeout_seconds)
            || !(60..=604800).contains(&result_ttl_seconds)
        {
            return Err(Error::Configuration);
        }
        Ok(Self {
            pool,
            cipher: Arc::new(cipher),
            credentials: Arc::new(credentials),
            capacity: Arc::new(Semaphore::new(concurrency)),
            judge_capacity: Arc::new(Semaphore::new((concurrency / 4).max(1))),
            timeout_seconds,
            result_ttl_seconds,
            tasks: tokio_util::task::TaskTracker::new(),
        })
    }
    pub async fn drain(&self) {
        self.tasks.close();
        self.tasks.wait().await;
    }
    pub async fn from_env(pool: PgPool) -> Result<Self> {
        let refs: Vec<String> =
            sqlx::query_scalar("SELECT DISTINCT credential_env FROM endpoints WHERE active")
                .fetch_all(&pool)
                .await?;
        let mut credentials = HashMap::new();
        for name in refs {
            if let Ok(value) = std::env::var(&name) {
                credentials.insert(name, value);
            }
        }
        let number = |name: &str, default: i32| -> Result<i32> {
            std::env::var(name)
                .map(|v| v.parse().map_err(|_| Error::Configuration))
                .unwrap_or(Ok(default))
        };
        let concurrency = number("BROKERROUTER_MAX_CONCURRENCY", 64)?;
        Self::new(
            pool,
            ResultCipher::from_env()?,
            credentials,
            usize::try_from(concurrency).map_err(|_| Error::Configuration)?,
            number("BROKERROUTER_REQUEST_TIMEOUT_SECONDS", 120)?,
            number("BROKERROUTER_RESULT_TTL_SECONDS", 86400)?,
        )
    }
    pub async fn execute(
        &self,
        who: &Identity,
        idempotency: &str,
        body: Value,
        operation: &'static str,
    ) -> Result<(Uuid, Result<Value>)> {
        self.execute_in_turn(who, idempotency, body, operation, None)
            .await
    }
    pub async fn execute_in_turn(
        &self,
        who: &Identity,
        idempotency: &str,
        body: Value,
        operation: &'static str,
        turn_id: Option<Uuid>,
    ) -> Result<(Uuid, Result<Value>)> {
        if idempotency.starts_with("_br_guardrail:") || idempotency.starts_with("_br_media:") {
            return Err(Error::Invalid("reserved idempotency namespace"));
        }
        self.execute_inner(
            who,
            idempotency,
            body,
            operation,
            CallContext {
                turn_id,
                judge: None,
            },
        )
        .await
    }

    pub(crate) fn judge_timeout_seconds(&self) -> i32 {
        self.timeout_seconds
    }

    pub(crate) async fn execute_judge(
        &self,
        who: &Identity,
        evaluation: Uuid,
        turn: Uuid,
        body: Value,
        maximum: i64,
    ) -> Result<Value> {
        self.execute_inner(
            who,
            &format!("_br_guardrail:{evaluation}"),
            body,
            "chat",
            CallContext {
                turn_id: Some(turn),
                judge: Some((evaluation, maximum)),
            },
        )
        .await?
        .1
    }

    /// Runs the selected server-side checks, then re-reads current policy before
    /// permitting the original operation. The low-level check never calls HTTP.
    pub async fn check(&self, who: &Identity, check: governance::Check) -> Result<Value> {
        let first = governance::check(&self.pool, &self.cipher, who, check.clone()).await?;
        if first["state"] != "checking" {
            return Ok(first);
        }
        let ids: Vec<Uuid> = serde_json::from_value(first["evaluations"].clone())
            .map_err(|_| Error::Configuration)?;
        for id in ids {
            crate::judge::evaluate(self, who, id).await?;
        }
        // Do not loop indefinitely on policy edits. A newly selected version is
        // returned as checking and will be driven by the next identical check.
        governance::check(&self.pool, &self.cipher, who, check).await
    }

    async fn execute_inner(
        &self,
        who: &Identity,
        idempotency: &str,
        body: Value,
        operation: &'static str,
        context: CallContext,
    ) -> Result<(Uuid, Result<Value>)> {
        let turn_id = context.turn_id;
        if idempotency.is_empty()
            || idempotency.len() > 200
            || !idempotency.bytes().all(|b| b.is_ascii_graphic())
        {
            return Err(Error::Invalid(
                "Idempotency-Key must contain 1 to 200 visible ASCII characters",
            ));
        }
        let request = protocol::validate(body, operation)?;
        store::authorize_model(&self.pool, who, &request.model).await?;
        governance::context(&self.pool, who, turn_id, &request.model).await?;
        let mut envelope = json!({"operation":operation,"request":request.body});
        if let Some(id) = turn_id {
            envelope["turn_id"] = json!(id);
        }
        if let Some((id, _)) = context.judge {
            envelope["guardrail_evaluation_id"] = json!(id);
        }
        let bytes = serde_json::to_vec(&envelope).map_err(|_| Error::Invalid("invalid JSON"))?;
        let digest = store::hash(&bytes);
        let existing = sqlx::query_as::<_, Reservation>(&format!(
            "SELECT {} FROM requests WHERE tenant_id=$1 AND key_id=$2 AND idempotency_key=$3",
            accounting::COLUMNS
        ))
        .bind(who.tenant_id)
        .bind(who.key_id)
        .bind(idempotency)
        .fetch_optional(&self.pool)
        .await?;
        if context.judge.is_none()
            && existing.as_ref().is_none_or(|r| r.status == "reserved")
            && let Some(turn_id) = turn_id
        {
            governance::require_ready(
                Box::pin(self.check(
                    who,
                    governance::Check {
                        turn_id,
                        operation_key: idempotency.into(),
                        stage: "pre".into(),
                        model_id: Some(request.model.clone()),
                        tool_name: None,
                        payload: request.body.clone(),
                    },
                ))
                .await?,
            )?;
        }
        let capacity = if context.judge.is_some() {
            &self.judge_capacity
        } else {
            &self.capacity
        };
        let _permit = capacity.try_acquire().map_err(|_| Error::Busy)?;
        let row = if let Some(row) = existing {
            if row.request_hash != digest
                || row.guardrail_evaluation_id != context.judge.map(|(id, _)| id)
            {
                return Err(Error::Conflict);
            }
            row
        } else {
            let endpoints =
                store::endpoints(&self.pool, who, &request.model, request.max_output).await?;
            let mut routes = Vec::new();
            let mut maximum = 0;
            let mut bound = None;
            let mut missing_fx = false;
            for endpoint in endpoints.into_iter().filter(|e| request.matches(e)) {
                if !self.credentials.contains_key(&endpoint.credential_env) {
                    continue;
                }
                let price = match endpoint.price(&self.pool).await {
                    Ok(p) => p,
                    Err(Error::MissingFx) => {
                        missing_fx = true;
                        continue;
                    }
                    Err(e) => return Err(e),
                };
                let cost =
                    price.cost(request.maximum_input(&endpoint), request.max_output as u64)?;
                if context.judge.is_some_and(|(_, cap)| cost > cap) {
                    continue;
                }
                if bound.is_none() || cost > maximum {
                    maximum = cost;
                    bound = Some(price.clone());
                }
                routes.push(Route { endpoint, price });
                if routes.len() == 3 {
                    break;
                }
            }
            if routes.is_empty() {
                return Err(if missing_fx {
                    Error::MissingFx
                } else {
                    Error::NoEndpoint
                });
            }
            let admission = Admission {
                identity: who,
                model: &request.model,
                idempotency_key: idempotency,
                request_hash: &digest,
                maximum_micros: maximum,
                snapshot: bound.ok_or(Error::NoEndpoint)?,
                routing: Some(serde_json::to_value(routes).map_err(|_| Error::Configuration)?),
                timeout_seconds: self.timeout_seconds,
                governance_turn_id: turn_id,
            };
            if let Some((id, _)) = context.judge {
                accounting::reserve_evaluation(&self.pool, admission, id)
                    .await?
                    .0
            } else {
                accounting::reserve(&self.pool, admission).await?.0
            }
        };
        let result = self.run(who, &request, row.clone()).await;
        Ok((row.id, result))
    }

    async fn run(
        &self,
        who: &Identity,
        request: &ValidatedRequest,
        row: Reservation,
    ) -> Result<Value> {
        match row.status.as_str() {
            "succeeded" => {
                return self
                    .result_inner(who, row.id, row.guardrail_evaluation_id, true)
                    .await;
            }
            "failed" => return Err(Error::NotSubmitted),
            "reconciled" => return Err(Error::ResultUnavailable),
            "submission_unknown" => return Err(Error::UpstreamUnknown),
            "submitting" => return Err(Error::InProgress),
            "reserved" => {}
            _ => return Err(Error::Configuration),
        }
        for _ in 0..3 {
            let (attempt, route) = match attempts::begin(&self.pool, who, row.id).await {
                Ok(a) => a,
                Err(Error::NotSubmitted) => {
                    accounting::release_unsubmitted(&self.pool, who, row.id).await?;
                    return Err(Error::NotSubmitted);
                }
                Err(Error::Unauthorized) => {
                    let _ = accounting::release_unsubmitted(&self.pool, who, row.id).await;
                    return Err(Error::Unauthorized);
                }
                Err(e) => return Err(e),
            };
            let remaining = row
                .dispatch_deadline
                .ok_or(Error::Configuration)?
                .signed_duration_since(chrono::Utc::now())
                .to_std()
                .unwrap_or(Duration::from_millis(1));
            let prepared = egress::client(&route.endpoint, remaining).await;
            let (client, url) = match prepared {
                Ok(v) => v,
                Err(_) => {
                    attempts::failed(
                        &self.pool,
                        who,
                        row.id,
                        attempt,
                        true,
                        "preflight_failed",
                        None,
                    )
                    .await?;
                    continue;
                }
            };
            let Some(secret) = self.credentials.get(&route.endpoint.credential_env) else {
                attempts::failed(
                    &self.pool,
                    who,
                    row.id,
                    attempt,
                    true,
                    "credential_unavailable",
                    None,
                )
                .await?;
                continue;
            };
            let mut body = request.body.clone();
            body["model"] = route.endpoint.upstream_model.clone().into();
            let outgoing = match client
                .post(url)
                .bearer_auth(secret)
                .header("x-request-id", attempt.to_string())
                .json(&body)
                .build()
            {
                Ok(r) => r,
                Err(_) => {
                    attempts::failed(
                        &self.pool,
                        who,
                        row.id,
                        attempt,
                        true,
                        "request_build_failed",
                        None,
                    )
                    .await?;
                    continue;
                }
            };
            // DNS may have taken seconds; check live hard permissions again at the send boundary.
            let authority=async {
                store::authorize_model(&self.pool,who,&row.model_id).await?;
                governance::context(&self.pool,who,row.governance_turn_id,&row.model_id).await?;
                if let Some(id)=row.guardrail_evaluation_id {crate::judge::authority(&self.pool,who,id).await?;}
                let active:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM endpoints WHERE tenant_id=$1 AND id=$2 AND active)").bind(who.tenant_id).bind(route.endpoint.id).fetch_one(&self.pool).await?;
                if active {Ok(())}else{Err(Error::Unauthorized)}
            }.await;
            if let Err(error) = authority {
                attempts::failed(
                    &self.pool,
                    who,
                    row.id,
                    attempt,
                    true,
                    "authority_unavailable_before_send",
                    None,
                )
                .await?;
                accounting::release_unsubmitted(&self.pool, who, row.id).await?;
                return Err(error);
            }
            let response = match client.execute(outgoing).await {
                Ok(response) => response,
                Err(error) => {
                    let not_sent = error.is_connect();
                    attempts::failed(
                        &self.pool,
                        who,
                        row.id,
                        attempt,
                        not_sent,
                        if not_sent {
                            "connect_failed"
                        } else {
                            "transport_unknown"
                        },
                        None,
                    )
                    .await?;
                    if not_sent {
                        continue;
                    }
                    return Err(Error::UpstreamUnknown);
                }
            };
            let status = response.status().as_u16();
            if status != 200 {
                // Even an HTTP error does not prove absence of supplier charges.
                attempts::failed(
                    &self.pool,
                    who,
                    row.id,
                    attempt,
                    false,
                    "upstream_http_status",
                    Some(status),
                )
                .await?;
                return Err(Error::UpstreamUnknown);
            }
            let received = read_response(response).await.and_then(|body| {
                let usage = protocol::validate_response(&body, request, &route.endpoint)?;
                Ok((body, usage))
            });
            let (mut body, (input, output)) = match received {
                Ok(v) => v,
                Err(_) => {
                    attempts::failed(
                        &self.pool,
                        who,
                        row.id,
                        attempt,
                        false,
                        "invalid_or_incomplete_response",
                        Some(status),
                    )
                    .await?;
                    return Err(Error::UpstreamUnknown);
                }
            };
            body["model"] = request.model.clone().into();
            let encoded = serde_json::to_vec(&body).map_err(|_| Error::UpstreamUnknown)?;
            let ciphertext = self.cipher.encrypt(who.tenant_id, row.id, &encoded)?;
            if let Err(error) = attempts::complete(
                &self.pool,
                who,
                row.id,
                attempts::Completion {
                    attempt,
                    usage: body["usage"].clone(),
                    input,
                    output,
                    ciphertext: &ciphertext,
                    ttl_seconds: self.result_ttl_seconds,
                },
            )
            .await
            {
                // A lost commit acknowledgement is resolved by a read/replay, never a second POST.
                let _ = attempts::failed(
                    &self.pool,
                    who,
                    row.id,
                    attempt,
                    false,
                    "settlement_unknown",
                    Some(status),
                )
                .await;
                return Err(error);
            }
            return self
                .result_inner(who, row.id, row.guardrail_evaluation_id, true)
                .await;
        }
        accounting::release_unsubmitted(&self.pool, who, row.id).await?;
        Err(Error::NotSubmitted)
    }

    pub async fn result(&self, who: &Identity, id: Uuid) -> Result<Value> {
        self.result_inner(who, id, None, false).await
    }
    pub async fn check_result(&self, who: &Identity, id: Uuid) -> Result<Value> {
        self.result_inner(who, id, None, true).await
    }
    async fn result_inner(
        &self,
        who: &Identity,
        id: Uuid,
        evaluation: Option<Uuid>,
        run_checks: bool,
    ) -> Result<Value> {
        let row = accounting::get(&self.pool, who, id).await?;
        if row.guardrail_evaluation_id != evaluation {
            return Err(Error::NotFound);
        }
        store::authorize_model(&self.pool, who, &row.model_id).await?;
        governance::context(&self.pool, who, row.governance_turn_id, &row.model_id).await?;
        if row.status == "reconciled" {
            return Err(Error::ResultUnavailable);
        }
        if row.status == "submission_unknown" {
            return Err(Error::UpstreamUnknown);
        }
        if row.status != "succeeded" {
            return Err(Error::InProgress);
        }
        if row
            .result_expires_at
            .is_none_or(|e| e <= chrono::Utc::now())
        {
            return Err(Error::ResultUnavailable);
        }
        let bytes = self.cipher.decrypt(
            who.tenant_id,
            id,
            row.result_ciphertext
                .as_deref()
                .ok_or(Error::ResultUnavailable)?,
        )?;
        let value: Value = serde_json::from_slice(&bytes).map_err(|_| Error::ResultUnavailable)?;
        if evaluation.is_none()
            && let Some(turn_id) = row.governance_turn_id
        {
            let check = governance::Check {
                turn_id,
                operation_key: id.to_string(),
                stage: "post".into(),
                model_id: Some(row.model_id),
                tool_name: None,
                payload: value.clone(),
            };
            let checked = if run_checks {
                Box::pin(self.check(who, check)).await?
            } else {
                // GET may discover a newer policy, but never dispatch paid work.
                governance::check(&self.pool, &self.cipher, who, check).await?
            };
            governance::require_ready(checked)?;
        }
        Ok(value)
    }
    pub async fn request(&self, who: &Identity, id: Uuid) -> Result<Value> {
        let row = accounting::get(&self.pool, who, id).await?;
        store::authorize_model(&self.pool, who, &row.model_id).await?;
        let attempts:Vec<AttemptRow>=sqlx::query_as("SELECT id,endpoint_id,ordinal,status,http_status,error_code,cost_micros FROM attempts WHERE tenant_id=$1 AND request_id=$2 ORDER BY ordinal").bind(who.tenant_id).bind(id).fetch_all(&self.pool).await?;
        let state = if row.status == "submitting"
            && row
                .dispatch_deadline
                .is_some_and(|d| d <= chrono::Utc::now())
        {
            "submission_unknown"
        } else {
            &row.status
        };
        Ok(
            json!({"id":id,"model":row.model_id,"purpose":if row.guardrail_evaluation_id.is_some(){"guardrail"}else{"model"},"guardrail_evaluation_id":row.guardrail_evaluation_id,"status":state,"currency":"CNY","maximum_cny":money::format_micros(row.reserve_micros),"reserved_cny":reserved_cny(state,row.reserve_micros),"cost_cny":row.final_micros.map(money::format_micros),"error_code":row.error_code,"result_expires_at":row.result_expires_at,"attempts":attempts.into_iter().map(|(id,endpoint,ordinal,status,http,error,cost)|json!({"id":id,"endpoint_id":endpoint,"ordinal":ordinal,"status":status,"http_status":http,"error_code":error,"cost_cny":cost.map(money::format_micros)})).collect::<Vec<_>>()}),
        )
    }

    pub async fn logs(&self, who: &Identity, before: Option<Uuid>, limit: i64) -> Result<Value> {
        if !(1..=100).contains(&limit) {
            return Err(Error::Invalid("log limit must be between 1 and 100"));
        }
        if let Some(id) = before {
            let exists: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM requests WHERE tenant_id=$1 AND key_id=$2 AND id=$3)",
            )
            .bind(who.tenant_id)
            .bind(who.key_id)
            .bind(id)
            .fetch_one(&self.pool)
            .await?;
            if !exists {
                return Err(Error::NotFound);
            }
        }
        let rows:Vec<RequestLog>=sqlx::query_as("SELECT r.id,r.model_id,r.status,r.created_at,r.final_micros,r.reserve_micros,r.guardrail_evaluation_id FROM requests r WHERE r.tenant_id=$1 AND r.key_id=$2 AND ($3::uuid IS NULL OR (r.created_at,r.id)<(SELECT created_at,id FROM requests WHERE tenant_id=$1 AND key_id=$2 AND id=$3)) AND EXISTS(SELECT 1 FROM key_models g JOIN models m ON m.tenant_id=g.tenant_id AND m.id=g.model_id WHERE g.tenant_id=r.tenant_id AND g.key_id=r.key_id AND g.model_id=r.model_id AND m.active) ORDER BY r.created_at DESC,r.id DESC LIMIT $4")
            .bind(who.tenant_id).bind(who.key_id).bind(before).bind(limit).fetch_all(&self.pool).await?;
        let next = if rows.len() == limit as usize {
            rows.last().map(|r| r.id)
        } else {
            None
        };
        Ok(
            json!({"data":rows.into_iter().map(|r|json!({"id":r.id,"model":r.model_id,"purpose":if r.guardrail_evaluation_id.is_some(){"guardrail"}else{"model"},"guardrail_evaluation_id":r.guardrail_evaluation_id,"status":r.status,"created_at":r.created_at,"currency":"CNY","cost_cny":r.final_micros.map(money::format_micros),"maximum_cny":money::format_micros(r.reserve_micros),"reserved_cny":reserved_cny(&r.status,r.reserve_micros)})).collect::<Vec<_>>(),"next_cursor":next}),
        )
    }
}

async fn read_response(mut response: reqwest::Response) -> Result<Value> {
    const LIMIT: usize = 4 * 1024 * 1024;
    if response.content_length().is_some_and(|n| n > LIMIT as u64) {
        return Err(Error::UpstreamUnknown);
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| Error::UpstreamUnknown)? {
        if bytes.len() + chunk.len() > LIMIT {
            return Err(Error::UpstreamUnknown);
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| Error::UpstreamUnknown)
}
