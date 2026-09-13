//! Durable media submission. Provider completion is not artifact delivery.
use crate::{
    accounting::{self, Admission, Reservation},
    error::{Error, Result},
    gateway::Gateway,
    governance,
    media_protocol::{self as protocol, CreateJob, Endpoint, Poll},
    money::{self, UnitUsage},
    store::{self, Identity},
};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, Transaction};
use std::{collections::HashMap, sync::Arc, time::Duration};
use uuid::Uuid;
type Tx<'a> = Transaction<'a, Postgres>;
#[derive(Clone, sqlx::FromRow)]
struct Job {
    tenant_id: Uuid,
    id: Uuid,
    endpoint_id: Uuid,
    state: String,
    input_ciphertext: Option<Vec<u8>>,
    provider_task_id: Option<String>,
    cancel_requested: bool,
    lease_token: Option<Uuid>,
    lease_valid: bool,
    due: bool,
    admission_deadline: DateTime<Utc>,
    poll_deadline: DateTime<Utc>,
    error_code: Option<String>,
    event_clock: i64,
}
#[derive(Clone)]
pub struct Media {
    pub gateway: Gateway,
    credentials: Arc<HashMap<String, String>>,
    timeout: Duration,
}
impl Media {
    pub fn new(
        gateway: Gateway,
        credentials: HashMap<String, String>,
        timeout_seconds: u64,
    ) -> Result<Self> {
        if !(1..=120).contains(&timeout_seconds) {
            return Err(Error::Configuration);
        }
        Ok(Self {
            gateway,
            credentials: Arc::new(credentials),
            timeout: Duration::from_secs(timeout_seconds),
        })
    }
    pub async fn from_env(pool: PgPool) -> Result<Self> {
        let refs: Vec<String> =
            sqlx::query_scalar("SELECT DISTINCT config->>'credential_env' FROM media_endpoints")
                .fetch_all(&pool)
                .await?;
        let credentials = refs
            .into_iter()
            .filter_map(|name| std::env::var(&name).ok().map(|v| (name, v)))
            .collect();
        Self::new(Gateway::from_env(pool).await?, credentials, 30)
    }
    pub async fn capabilities(&self, who: &Identity, model: &str) -> Result<Value> {
        store::authorize_model(&self.gateway.pool, who, model).await?;
        let configs: Vec<Value> = sqlx::query_scalar("SELECT config FROM media_endpoints WHERE tenant_id=$1 AND model_id=$2 AND active ORDER BY priority,id LIMIT 100")
            .bind(who.tenant_id).bind(model).fetch_all(&self.gateway.pool).await?;
        let mut endpoints = Vec::with_capacity(configs.len());
        for config in configs {
            let e: Endpoint = serde_json::from_value(config).map_err(|_| Error::Configuration)?;
            e.validate()?;
            if e.maximum_output_bytes == 0 && e.output_origins.is_empty() {
                continue;
            }
            e.validate_output_contract()?;
            endpoints.push(json!({"id":e.id,"name":e.name,"protocol":e.protocol,"currency":e.currency,"priority":e.priority,"profiles":e.profiles,"maximum_output_bytes":e.maximum_output_bytes}));
        }
        Ok(
            json!({"model":model,"task":"video.generate","protocol_version":"media-jobs-v1","endpoints":endpoints,"delivery":"not_released"}),
        )
    }
    pub async fn create(
        &self,
        who: &Identity,
        idem: &str,
        body: CreateJob,
        turn: Option<Uuid>,
    ) -> Result<Value> {
        body.validate()?;
        if idem.is_empty() || idem.len() > 180 || !idem.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(Error::Invalid(
                "media idempotency key must be 1..180 visible ASCII characters",
            ));
        }
        store::authorize_model(&self.gateway.pool, who, &body.model).await?;
        governance::context(&self.gateway.pool, who, turn, &body.model).await?;
        let hash = governance::digest(&json!({"body":body,"turn":turn,"operation":"media"}))?;
        let idem = format!("_br_media:{idem}");
        let mut tx = self.gateway.pool.begin().await?;
        accounting::lock_accounts(&mut tx, who).await?;
        let existing:Option<(Uuid,Vec<u8>)>=sqlx::query_as("SELECT id,request_hash FROM requests WHERE tenant_id=$1 AND key_id=$2 AND idempotency_key=$3").bind(who.tenant_id).bind(who.key_id).bind(&idem).fetch_optional(&mut *tx).await?;
        if let Some((id, old)) = existing {
            if old != hash {
                return Err(Error::Conflict);
            }
            tx.commit().await?;
            return self.get(who, id).await;
        }
        let counts:(i64,i64)=sqlx::query_as("SELECT count(*),count(*) FILTER(WHERE key_id=$2) FROM media_jobs WHERE tenant_id=$1 AND state IN ('queued','waiting_approval','submitting','submission_unknown','running','needs_reconciliation')").bind(who.tenant_id).bind(who.key_id).fetch_one(&mut *tx).await?;
        if counts.0 >= 256 || counts.1 >= 32 {
            return Err(Error::Busy);
        }
        let configs:Vec<Value>=sqlx::query_scalar("SELECT config FROM media_endpoints WHERE tenant_id=$1 AND model_id=$2 AND active ORDER BY priority,id LIMIT 100").bind(who.tenant_id).bind(&body.model).fetch_all(&mut *tx).await?;
        let mut selected = None;
        for v in configs {
            let e: Endpoint = serde_json::from_value(v).map_err(|_| Error::Configuration)?;
            e.validate()?;
            if e.maximum_output_bytes == 0 && e.output_origins.is_empty() {
                continue;
            }
            e.validate_output_contract()?;
            if e.profile(&body).is_some() {
                selected = Some(e);
                break;
            }
        }
        let endpoint = selected.ok_or(Error::NoEndpoint)?;
        endpoint.validate_output_contract()?;
        let fx:Option<(i64,rust_decimal::Decimal)>=sqlx::query_as("SELECT id,usd_cny FROM fx_rates WHERE tenant_id=$1 AND effective_at<=now() ORDER BY effective_at DESC LIMIT 1").bind(who.tenant_id).fetch_optional(&mut *tx).await?;
        let snapshot = endpoint.snapshot(&body, fx)?;
        let maximum = snapshot.maximum_cost()?;
        let (request, new) = accounting::reserve_units_in(
            &mut tx,
            Admission {
                identity: who,
                model: &body.model,
                idempotency_key: &idem,
                request_hash: &hash,
                maximum_micros: maximum,
                snapshot,
                routing: Some(json!({"kind":"media","endpoint":endpoint})),
                timeout_seconds: 600,
                governance_turn_id: turn,
            },
        )
        .await?;
        if !new {
            return Err(Error::Conflict);
        }
        let encrypted = self.gateway.cipher.encrypt(
            who.tenant_id,
            request.id,
            &serde_json::to_vec(&body).map_err(|_| Error::Configuration)?,
        )?;
        sqlx::query("INSERT INTO media_jobs(tenant_id,id,key_id,endpoint_id,input_ciphertext) VALUES($1,$2,$3,$4,$5)").bind(who.tenant_id).bind(request.id).bind(who.key_id).bind(endpoint.id).bind(encrypted).execute(&mut *tx).await?;
        crate::outputs::reserve(&mut tx, who, request.id, endpoint.maximum_output_bytes).await?;
        event(&mut tx, who.tenant_id, request.id, "queued", None).await?;
        tx.commit().await?;
        self.get(who, request.id).await
    }
    pub async fn get(&self, who: &Identity, id: Uuid) -> Result<Value> {
        let mut tx = self.gateway.pool.begin().await?;
        let (r, j) = locked(&mut tx, who, id).await?;
        live(&mut tx, who).await?;
        let mut v = view(&r, &j);
        let output:Option<(String,i64,Option<i64>,Option<String>)>=sqlx::query_as("SELECT i.state,i.maximum_bytes,a.byte_length,i.error_code FROM media_output_imports i LEFT JOIN assets a ON a.tenant_id=i.tenant_id AND a.id=i.asset_id WHERE i.tenant_id=$1 AND i.job_id=$2").bind(who.tenant_id).bind(id).fetch_optional(&mut *tx).await?;
        if let Some((state, maximum, bytes, error)) = output {
            v["output_storage"] = json!({"state":state,"maximum_bytes":maximum,"byte_length":bytes,"error_code":error,"content_safety":"not_evaluated","delivery":"not_released"});
        }
        let scan:Option<(i32,Option<String>,Option<Value>)>=sqlx::query_as("SELECT p.version,CASE WHEN x.expires_at<=clock_timestamp() THEN 'expired' ELSE x.state END,x.report FROM output_scan_profiles p JOIN requests r ON r.tenant_id=p.tenant_id AND r.model_id=p.model_id LEFT JOIN output_scans x ON x.tenant_id=r.tenant_id AND x.job_id=r.id AND x.profile_id=p.id WHERE r.tenant_id=$1 AND r.id=$2 AND p.active").bind(who.tenant_id).bind(id).fetch_optional(&mut *tx).await?;
        if let Some((version, state, report)) = scan {
            v["output_scan"] = json!({"profile_version":version,"state":state.unwrap_or_else(||"not_started".into()),"technical":report.as_ref().and_then(|r|r.get("technical")),"content":report.as_ref().and_then(|r|r.get("content")),"delivery":"not_released","approval":"not_granted"});
        }
        let approval:Option<governance::Approval>=sqlx::query_as("SELECT a.* FROM approvals a JOIN governance_operations o ON o.tenant_id=a.tenant_id AND o.id=a.operation_id WHERE o.tenant_id=$1 AND o.key_id=$2 AND o.operation_key=$3 AND a.state='pending' ORDER BY a.expires_at DESC LIMIT 1")
            .bind(who.tenant_id).bind(who.key_id).bind(format!("media:{id}")).fetch_optional(&mut *tx).await?;
        if let Some(approval) = approval {
            v["approval"] = governance::approval_view(&approval);
        }
        tx.commit().await?;
        Ok(v)
    }
    pub async fn events(&self, who: &Identity, id: Uuid, after: i64) -> Result<Value> {
        if after < 0 {
            return Err(Error::Invalid("invalid event cursor"));
        }
        self.get(who, id).await?;
        let data:Vec<(i64,String,Option<String>)>=sqlx::query_as("SELECT sequence,state,code FROM media_events WHERE tenant_id=$1 AND job_id=$2 AND sequence>$3 ORDER BY sequence LIMIT 100").bind(who.tenant_id).bind(id).bind(after).fetch_all(&self.gateway.pool).await?;
        Ok(
            json!({"data":data.into_iter().map(|(sequence,state,code)|json!({"sequence":sequence,"state":state,"code":code})).collect::<Vec<_>>()}),
        )
    }
    pub async fn cancel(&self, who: &Identity, id: Uuid) -> Result<Value> {
        let mut tx = self.gateway.pool.begin().await?;
        let (r, j) = locked(&mut tx, who, id).await?;
        live(&mut tx, who).await?;
        if matches!(j.state.as_str(), "queued" | "waiting_approval") {
            accounting::release_locked(&mut tx, who, &r).await?;
            crate::outputs::release_unmaterialized(&mut tx, who, id).await?;
            transition(
                &mut tx,
                &j,
                "cancelled",
                Some("cancelled_before_submission"),
            )
            .await?;
            sqlx::query("UPDATE media_jobs SET input_ciphertext=NULL,cancel_requested=true WHERE tenant_id=$1 AND id=$2").bind(who.tenant_id).bind(id).execute(&mut *tx).await?;
        } else if !j.cancel_requested {
            sqlx::query("UPDATE media_jobs SET cancel_requested=true WHERE tenant_id=$1 AND id=$2")
                .bind(who.tenant_id)
                .bind(id)
                .execute(&mut *tx)
                .await?;
            event(
                &mut tx,
                who.tenant_id,
                id,
                &j.state,
                Some("cancellation_not_confirmed"),
            )
            .await?;
        }
        tx.commit().await?;
        self.get(who, id).await
    }
    /// One durable work item. Multiple processes compete via lease fencing, never replay POST.
    pub async fn tick(&self) -> Result<bool> {
        self.purge().await?;
        let candidates:Vec<(Uuid,Uuid,Uuid)>=sqlx::query_as("SELECT tenant_id,key_id,id FROM media_jobs WHERE state IN ('queued','waiting_approval','submitting','running') AND next_poll_at<=clock_timestamp() AND (lease_until IS NULL OR lease_until<=clock_timestamp()) ORDER BY next_poll_at,id LIMIT 16").fetch_all(&self.gateway.pool).await?;
        for (tenant_id, key_id, id) in candidates {
            let who = Identity { tenant_id, key_id };
            let mut tx = self.gateway.pool.begin().await?;
            let (r, j) = locked(&mut tx, &who, id).await?;
            if j.lease_valid
                || !j.due
                || !matches!(
                    j.state.as_str(),
                    "queued" | "waiting_approval" | "submitting" | "running"
                )
            {
                continue;
            }
            if j.state == "submitting" {
                mark_unknown(&mut tx, &j, "submission_owner_lost").await?;
                tx.commit().await?;
                return Ok(true);
            }
            let token = Uuid::new_v4();
            sqlx::query("UPDATE media_jobs SET lease_token=$3,lease_until=clock_timestamp()+make_interval(secs=>$4) WHERE tenant_id=$1 AND id=$2").bind(tenant_id).bind(id).bind(token).bind(if j.state=="running"{180.0}else{900.0}).execute(&mut *tx).await?;
            tx.commit().await?;
            if j.state == "running" {
                self.query(&who, r, j, token).await?;
            } else {
                self.submit(&who, r, j, token).await?;
            }
            return Ok(true);
        }
        Ok(false)
    }
    fn input(&self, j: &Job) -> Result<CreateJob> {
        serde_json::from_slice(
            &self.gateway.cipher.decrypt(
                j.tenant_id,
                j.id,
                j.input_ciphertext
                    .as_deref()
                    .ok_or(Error::ResultUnavailable)?,
            )?,
        )
        .map_err(|_| Error::ResultUnavailable)
    }
    fn endpoint(&self, r: &Reservation) -> Result<Endpoint> {
        let e: Endpoint = serde_json::from_value(
            r.routing_snapshot
                .as_ref()
                .and_then(|v| v.get("endpoint"))
                .cloned()
                .ok_or(Error::Configuration)?,
        )
        .map_err(|_| Error::Configuration)?;
        e.validate()?;
        Ok(e)
    }
    async fn transport(
        &self,
        e: &Endpoint,
        path: &str,
    ) -> Result<(reqwest::Client, url::Url, String)> {
        let key = self
            .credentials
            .get(&e.credential_env)
            .filter(|s| !s.is_empty())
            .ok_or(Error::Configuration)?
            .clone();
        let url = url::Url::parse(&format!("{}{}", e.origin.trim_end_matches('/'), path))
            .map_err(|_| Error::Configuration)?;
        let client = crate::egress::pinned_client(&url, e.allow_private, self.timeout).await?;
        Ok((client, url, key))
    }
    async fn submit(&self, who: &Identity, r: Reservation, j: Job, token: Uuid) -> Result<()> {
        let body = match self.input(&j) {
            Ok(b) => b,
            Err(_) => {
                return self
                    .stop_unsubmitted(who, j.id, token, "input_unavailable")
                    .await;
            }
        };
        if j.admission_deadline <= Utc::now() {
            return self
                .stop_unsubmitted(who, j.id, token, "admission_expired")
                .await;
        }
        let context =
            governance::context(&self.gateway.pool, who, r.governance_turn_id, &r.model_id).await;
        match context {
            Ok(_) => {}
            Err(Error::Database(e)) => return Err(Error::Database(e)),
            Err(_) => {
                return self
                    .stop_unsubmitted(who, j.id, token, "authority_revoked")
                    .await;
            }
        }
        if let Some(turn_id) = r.governance_turn_id {
            let checked = self
                .gateway
                .check(
                    who,
                    governance::Check {
                        turn_id,
                        operation_key: format!("media:{}", j.id),
                        stage: "pre".into(),
                        model_id: Some(r.model_id.clone()),
                        tool_name: None,
                        payload: json!(body),
                    },
                )
                .await;
            match checked.and_then(governance::require_ready) {
                Ok(()) => {}
                Err(Error::ApprovalRequired(_) | Error::InProgress | Error::PolicyChanged) => {
                    return self
                        .defer(
                            who,
                            j.id,
                            token,
                            "waiting_approval",
                            "input_approval_required",
                        )
                        .await;
                }
                Err(Error::Database(e)) => return Err(Error::Database(e)),
                Err(_) => {
                    return self
                        .stop_unsubmitted(who, j.id, token, "input_action_rejected")
                        .await;
                }
            }
        }
        let e = self.endpoint(&r)?;
        let (client, url, key) = match self
            .transport(&e, "/api/v1/services/aigc/video-generation/video-synthesis")
            .await
        {
            Ok(v) => v,
            Err(_) => {
                return self
                    .stop_unsubmitted(who, j.id, token, "transport_not_prepared")
                    .await;
            }
        };
        let request = client
            .post(url)
            .bearer_auth(key)
            .header("X-DashScope-Async", "enable")
            .json(&e.payload(&body))
            .build()
            .map_err(|_| Error::Configuration)?;
        let mut tx = self.gateway.pool.begin().await?;
        let (_, current) = locked(&mut tx, who, j.id).await?;
        owned(&current, token)?;
        if !matches!(current.state.as_str(), "queued" | "waiting_approval") {
            return Err(Error::InProgress);
        }
        if current.admission_deadline <= Utc::now() {
            tx.rollback().await?;
            return self
                .stop_unsubmitted(who, j.id, token, "admission_expired")
                .await;
        }
        match live(&mut tx, who).await {
            Ok(()) => {}
            Err(Error::Unauthorized) => {
                tx.rollback().await?;
                return self
                    .stop_unsubmitted(who, j.id, token, "authority_revoked")
                    .await;
            }
            Err(e) => return Err(e),
        }
        let active:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM media_endpoints e JOIN key_models k ON k.tenant_id=e.tenant_id AND k.model_id=e.model_id JOIN models m ON m.tenant_id=e.tenant_id AND m.id=e.model_id WHERE e.tenant_id=$1 AND e.id=$2 AND k.key_id=$3 AND e.active AND m.active)").bind(who.tenant_id).bind(j.endpoint_id).bind(who.key_id).fetch_one(&mut *tx).await?;
        if !active {
            tx.rollback().await?;
            return self
                .stop_unsubmitted(who, j.id, token, "endpoint_or_model_revoked")
                .await;
        }
        // Recheck grants/policy under the tenant lock at the irreversible send boundary.
        if let Some(turn) = r.governance_turn_id {
            let turn = governance::turn_locked(&mut tx, who, turn, true).await?;
            let op:governance::Operation=sqlx::query_as("SELECT * FROM governance_operations WHERE tenant_id=$1 AND key_id=$2 AND turn_id=$3 AND operation_key=$4 AND stage='pre' FOR UPDATE")
                .bind(who.tenant_id).bind(who.key_id).bind(turn.id).bind(format!("media:{}",j.id)).fetch_one(&mut *tx).await?;
            if op.payload_hash
                != governance::digest(
                    &json!({"turn":turn.id,"model":Some(&r.model_id),"tool":Option::<String>::None,"payload":body}),
                )?
            {
                return Err(Error::Conflict);
            }
            if !matches!(
                governance::gate(&mut tx, who, &turn, &op, &json!(body)).await?,
                governance::Gate::Ready
            ) {
                transition(
                    &mut tx,
                    &current,
                    "waiting_approval",
                    Some("policy_recheck_required"),
                )
                .await?;
                tx.commit().await?;
                return Ok(());
            }
        } else {
            let required:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM runtime_bindings WHERE tenant_id=$1 AND key_id=$2) OR EXISTS(SELECT 1 FROM guardrail_policies p JOIN guardrails g ON g.tenant_id=p.tenant_id AND g.id=p.guardrail_id WHERE p.tenant_id=$1 AND p.active AND g.active AND (p.key_id IS NULL OR p.key_id=$2) AND (p.model_id IS NULL OR p.model_id=$3) AND p.tool_name IS NULL)").bind(who.tenant_id).bind(who.key_id).bind(&r.model_id).fetch_one(&mut *tx).await?;
            if required {
                return Err(Error::GovernanceContext);
            }
        }
        let dispatched=sqlx::query("UPDATE requests SET status='submitting',updated_at=now() WHERE tenant_id=$1 AND id=$2 AND status='reserved'").bind(who.tenant_id).bind(j.id).execute(&mut *tx).await?;
        if dispatched.rows_affected() != 1 {
            return Err(Error::InProgress);
        }
        sqlx::query("UPDATE media_jobs SET state='submitting' WHERE tenant_id=$1 AND id=$2")
            .bind(who.tenant_id)
            .bind(j.id)
            .execute(&mut *tx)
            .await?;
        event(&mut tx, who.tenant_id, j.id, "submitting", None).await?;
        tx.commit().await?;
        let response = bounded_json(client.execute(request).await).await;
        let accepted = response.and_then(|v| protocol::accepted(&v));
        let mut tx = self.gateway.pool.begin().await?;
        let (_, current) = locked(&mut tx, who, j.id).await?;
        owned(&current, token)?;
        if current.state != "submitting" {
            return Err(Error::InProgress);
        }
        match accepted {
            Ok(id) => {
                sqlx::query(
                    "UPDATE media_jobs SET provider_task_id=$3 WHERE tenant_id=$1 AND id=$2",
                )
                .bind(who.tenant_id)
                .bind(j.id)
                .bind(id)
                .execute(&mut *tx)
                .await?;
                transition(&mut tx, &current, "running", None).await?;
            }
            Err(_) => mark_unknown(&mut tx, &current, "submission_response_uncertain").await?,
        }
        tx.commit().await?;
        Ok(())
    }
    async fn query(&self, who: &Identity, r: Reservation, j: Job, token: Uuid) -> Result<()> {
        if j.poll_deadline <= Utc::now() {
            return self
                .defer(
                    who,
                    j.id,
                    token,
                    "needs_reconciliation",
                    "provider_query_deadline",
                )
                .await;
        }
        let body = self.input(&j)?;
        let e = self.endpoint(&r)?;
        let task = j.provider_task_id.as_deref().ok_or(Error::Configuration)?;
        let result = async {
            let (client, url, key) = self.transport(&e, &format!("/api/v1/tasks/{task}")).await?;
            bounded_json(client.get(url).bearer_auth(key).send().await).await
        }
        .await;
        let result = result.and_then(|v| protocol::poll(&v, task, &body, e.allow_private));
        match result {
            Ok(Poll::Running) => {
                self.defer(who, j.id, token, "running", "provider_running")
                    .await
            }
            Err(_) => {
                self.defer(who, j.id, token, "running", "provider_query_retry")
                    .await
            }
            Ok(Poll::Review(code)) => {
                self.defer(who, j.id, token, "needs_reconciliation", code)
                    .await
            }
            Ok(Poll::Complete { usage, output }) => {
                let ciphertext = self.gateway.cipher.encrypt(
                    who.tenant_id,
                    j.id,
                    &serde_json::to_vec(&output).map_err(|_| Error::Configuration)?,
                )?;
                let mut tx = self.gateway.pool.begin().await?;
                let (r, current) = locked(&mut tx, who, j.id).await?;
                owned(&current, token)?;
                if current.state != "running" {
                    return Err(Error::InProgress);
                }
                let (actual, mut details) = accounting::unit_bill(&r.price_snapshot, &usage)?;
                details["provider_task_id"] = json!(task);
                details["provider_protocol"] = json!(e.protocol);
                details["provider_usage"] = output["usage_evidence"].clone();
                accounting::finish_locked(&mut tx, who, &r, actual, None, details).await?;
                transition(&mut tx, &current, "output_pending", None).await?;
                sqlx::query("UPDATE media_jobs SET output_ciphertext=$3,output_expires_at=clock_timestamp()+interval '1 hour',input_ciphertext=NULL,provider_status='SUCCEEDED' WHERE tenant_id=$1 AND id=$2").bind(who.tenant_id).bind(j.id).bind(ciphertext).execute(&mut *tx).await?;
                tx.commit().await?;
                Ok(())
            }
        }
    }
    async fn stop_unsubmitted(
        &self,
        who: &Identity,
        id: Uuid,
        token: Uuid,
        code: &str,
    ) -> Result<()> {
        let mut tx = self.gateway.pool.begin().await?;
        let (r, j) = locked(&mut tx, who, id).await?;
        owned(&j, token)?;
        accounting::release_locked(&mut tx, who, &r).await?;
        crate::outputs::release_unmaterialized(&mut tx, who, id).await?;
        transition(&mut tx, &j, "cancelled", Some(code)).await?;
        sqlx::query("UPDATE media_jobs SET input_ciphertext=NULL WHERE tenant_id=$1 AND id=$2")
            .bind(who.tenant_id)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }
    async fn defer(
        &self,
        who: &Identity,
        id: Uuid,
        token: Uuid,
        state: &str,
        code: &str,
    ) -> Result<()> {
        let mut tx = self.gateway.pool.begin().await?;
        let (_, j) = locked(&mut tx, who, id).await?;
        owned(&j, token)?;
        transition(&mut tx, &j, state, Some(code)).await?;
        tx.commit().await?;
        Ok(())
    }
    pub async fn purge(&self) -> Result<()> {
        sqlx::query("WITH batch AS(SELECT tenant_id,id FROM media_jobs WHERE output_expires_at<=clock_timestamp() AND output_ciphertext IS NOT NULL LIMIT 100 FOR UPDATE SKIP LOCKED) UPDATE media_jobs j SET output_ciphertext=NULL FROM batch b WHERE j.tenant_id=b.tenant_id AND j.id=b.id").execute(&self.gateway.pool).await?;
        sqlx::query("WITH batch AS(SELECT tenant_id,id FROM media_jobs WHERE poll_deadline+interval '1 hour'<=clock_timestamp() AND input_ciphertext IS NOT NULL AND state NOT IN ('queued','waiting_approval','running','submitting') LIMIT 100 FOR UPDATE SKIP LOCKED) UPDATE media_jobs j SET input_ciphertext=NULL FROM batch b WHERE j.tenant_id=b.tenant_id AND j.id=b.id").execute(&self.gateway.pool).await?;
        Ok(())
    }
}

pub async fn put_endpoint(pool: &PgPool, e: Endpoint) -> Result<()> {
    e.validate()?;
    e.validate_output_contract()?;
    let mut tx = pool.begin().await?;
    governance::tenant_lock(&mut tx, e.tenant_id).await?;
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM media_endpoints WHERE tenant_id=$1 AND model_id=$2 AND active",
    )
    .bind(e.tenant_id)
    .bind(&e.model_id)
    .fetch_one(&mut *tx)
    .await?;
    if count >= 100 {
        return Err(Error::Invalid(
            "at most 100 active media endpoints per logical model",
        ));
    }
    sqlx::query(
        "INSERT INTO media_endpoints(tenant_id,id,model_id,config,priority) VALUES($1,$2,$3,$4,$5)",
    )
    .bind(e.tenant_id)
    .bind(e.id)
    .bind(&e.model_id)
    .bind(json!(e))
    .bind(e.priority)
    .execute(&mut *tx)
    .await?;
    sqlx::query("INSERT INTO audit_events(tenant_id,action,object_id,details) VALUES($1,'media_endpoint.created',$2,$3)").bind(e.tenant_id).bind(e.id.to_string()).bind(json!({"model":e.model_id,"protocol":e.protocol})).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}
pub async fn disable_endpoint(pool: &PgPool, tenant: Uuid, id: Uuid) -> Result<()> {
    let mut tx = pool.begin().await?;
    governance::tenant_lock(&mut tx, tenant).await?;
    let changed =
        sqlx::query("UPDATE media_endpoints SET active=false WHERE tenant_id=$1 AND id=$2")
            .bind(tenant)
            .bind(id)
            .execute(&mut *tx)
            .await?;
    if changed.rows_affected() != 1 {
        return Err(Error::NotFound);
    }
    sqlx::query("INSERT INTO audit_events(tenant_id,action,object_id,details) VALUES($1,'media_endpoint.disabled',$2,'{}')").bind(tenant).bind(id.to_string()).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}
pub async fn attach_task(
    pool: &PgPool,
    tenant: Uuid,
    id: Uuid,
    task: &str,
    evidence: &str,
) -> Result<()> {
    if !protocol::task_id(task) || !governance::valid_text(evidence, 500) {
        return Err(Error::Invalid(
            "valid task ID and evidence reference required",
        ));
    }
    let key = sqlx::query_scalar("SELECT key_id FROM media_jobs WHERE tenant_id=$1 AND id=$2")
        .bind(tenant)
        .bind(id)
        .fetch_optional(pool)
        .await?
        .ok_or(Error::NotFound)?;
    let who = Identity {
        tenant_id: tenant,
        key_id: key,
    };
    let mut tx = pool.begin().await?;
    let (_, j) = locked(&mut tx, &who, id).await?;
    if j.provider_task_id.as_deref() == Some(task) {
        tx.commit().await?;
        return Ok(());
    }
    if j.state != "submission_unknown" {
        return Err(Error::InProgress);
    }
    sqlx::query("UPDATE media_jobs SET provider_task_id=$3 WHERE tenant_id=$1 AND id=$2")
        .bind(tenant)
        .bind(id)
        .bind(task)
        .execute(&mut *tx)
        .await?;
    transition(&mut tx, &j, "running", Some("operator_attached_task")).await?;
    sqlx::query("INSERT INTO audit_events(tenant_id,action,object_id,details) VALUES($1,'media.task_attached',$2,$3)").bind(tenant).bind(id.to_string()).bind(json!({"provider_task_id":task,"evidence_reference":evidence})).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}
pub async fn reconcile(
    pool: &PgPool,
    tenant: Uuid,
    id: Uuid,
    usage: &[UnitUsage],
    evidence: &str,
) -> Result<()> {
    if !governance::valid_text(evidence, 500) {
        return Err(Error::Invalid("evidence reference required"));
    }
    let key = sqlx::query_scalar("SELECT key_id FROM media_jobs WHERE tenant_id=$1 AND id=$2")
        .bind(tenant)
        .bind(id)
        .fetch_optional(pool)
        .await?
        .ok_or(Error::NotFound)?;
    let who = Identity {
        tenant_id: tenant,
        key_id: key,
    };
    let mut tx = pool.begin().await?;
    let (r, j) = locked(&mut tx, &who, id).await?;
    let (actual, details) = accounting::unit_bill(&r.price_snapshot, usage)?;
    if j.state == "reconciled" {
        let old:Value=sqlx::query_scalar("SELECT details FROM ledger_entries WHERE tenant_id=$1 AND request_id=$2 AND kind='finalize'").bind(tenant).bind(id).fetch_one(&mut *tx).await?;
        return if old == details {
            Ok(())
        } else {
            Err(Error::Conflict)
        };
    }
    if !matches!(
        j.state.as_str(),
        "submission_unknown" | "needs_reconciliation"
    ) {
        return Err(Error::InProgress);
    }
    accounting::finish_locked(&mut tx, &who, &r, actual, None, details).await?;
    crate::outputs::release_unmaterialized(&mut tx, &who, id).await?;
    transition(&mut tx, &j, "reconciled", Some("operator_verified_charge")).await?;
    sqlx::query("UPDATE requests SET status='reconciled' WHERE tenant_id=$1 AND id=$2")
        .bind(tenant)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE media_jobs SET input_ciphertext=NULL WHERE tenant_id=$1 AND id=$2")
        .bind(tenant)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO audit_events(tenant_id,action,object_id,details) VALUES($1,'media.reconciled',$2,$3)").bind(tenant).bind(id.to_string()).bind(json!({"evidence_reference":evidence,"actual_cny_micros":actual})).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}
async fn locked(tx: &mut Tx<'_>, who: &Identity, id: Uuid) -> Result<(Reservation, Job)> {
    accounting::lock_accounts(tx, who).await?;
    let r = sqlx::query_as(&format!(
        "SELECT {} FROM requests WHERE tenant_id=$1 AND key_id=$2 AND id=$3 FOR UPDATE",
        accounting::COLUMNS
    ))
    .bind(who.tenant_id)
    .bind(who.key_id)
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(Error::NotFound)?;
    let j=sqlx::query_as("SELECT *,COALESCE(lease_until>clock_timestamp(),false) AS lease_valid,next_poll_at<=clock_timestamp() AS due FROM media_jobs WHERE tenant_id=$1 AND key_id=$2 AND id=$3 FOR UPDATE").bind(who.tenant_id).bind(who.key_id).bind(id).fetch_optional(&mut **tx).await?.ok_or(Error::NotFound)?;
    Ok((r, j))
}
async fn live(tx: &mut Tx<'_>, who: &Identity) -> Result<()> {
    let ok:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM tenants t JOIN virtual_keys k ON k.tenant_id=t.id WHERE t.id=$1 AND k.id=$2 AND t.active AND k.revoked_at IS NULL AND (k.expires_at IS NULL OR k.expires_at>clock_timestamp()))").bind(who.tenant_id).bind(who.key_id).fetch_one(&mut **tx).await?;
    if ok { Ok(()) } else { Err(Error::Unauthorized) }
}
fn owned(j: &Job, token: Uuid) -> Result<()> {
    if j.lease_token == Some(token) && j.lease_valid {
        Ok(())
    } else {
        Err(Error::InProgress)
    }
}
async fn event(
    tx: &mut Tx<'_>,
    tenant: Uuid,
    id: Uuid,
    state: &str,
    code: Option<&str>,
) -> Result<()> {
    let seq:i64=sqlx::query_scalar("UPDATE media_jobs SET event_clock=event_clock+1 WHERE tenant_id=$1 AND id=$2 RETURNING event_clock").bind(tenant).bind(id).fetch_one(&mut **tx).await?;
    sqlx::query(
        "INSERT INTO media_events(tenant_id,job_id,sequence,state,code) VALUES($1,$2,$3,$4,$5)",
    )
    .bind(tenant)
    .bind(id)
    .bind(seq)
    .bind(state)
    .bind(code)
    .execute(&mut **tx)
    .await?;
    Ok(())
}
async fn transition(tx: &mut Tx<'_>, j: &Job, state: &str, code: Option<&str>) -> Result<()> {
    sqlx::query("UPDATE media_jobs SET state=$3,error_code=$4,next_poll_at=clock_timestamp()+interval '15 seconds',lease_token=NULL,lease_until=NULL WHERE tenant_id=$1 AND id=$2").bind(j.tenant_id).bind(j.id).bind(state).bind(code).execute(&mut **tx).await?;
    if j.state != state || j.error_code.as_deref() != code {
        event(tx, j.tenant_id, j.id, state, code).await?;
    }
    Ok(())
}
async fn mark_unknown(tx: &mut Tx<'_>, j: &Job, code: &str) -> Result<()> {
    sqlx::query("UPDATE requests SET status='submission_unknown',error_code=$3,updated_at=now() WHERE tenant_id=$1 AND id=$2 AND status='submitting'").bind(j.tenant_id).bind(j.id).bind(code).execute(&mut **tx).await?;
    transition(tx, j, "submission_unknown", Some(code)).await
}
async fn bounded_json(
    response: std::result::Result<reqwest::Response, reqwest::Error>,
) -> Result<Value> {
    let mut response = response.map_err(|_| Error::UpstreamUnknown)?;
    if !response.status().is_success() {
        return Err(Error::UpstreamUnknown);
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| Error::UpstreamUnknown)? {
        if bytes.len() + chunk.len() > 65536 {
            return Err(Error::UpstreamUnknown);
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| Error::UpstreamUnknown)
}
fn view(r: &Reservation, j: &Job) -> Value {
    json!({"id":j.id,"object":"media.job","model":r.model_id,"task":"video.generate","state":j.state,"endpoint_id":j.endpoint_id,"provider_task_id":j.provider_task_id,"cancel_requested":j.cancel_requested,"error_code":j.error_code,"event_cursor":j.event_clock,"artifacts":[],"delivery":"not_released","cost":{"currency":"CNY","reserved":money::format_micros(if r.final_micros.is_some(){0}else{r.reserve_micros}),"final":r.final_micros.map(money::format_micros)},"turn_id":r.governance_turn_id})
}
