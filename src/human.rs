//! Signed, request-bound attestations from a trusted user-interaction BFF.
//! This is deliberately NOT an Agent credential or a generic OIDC ID token.
use crate::{
    cipher::ResultCipher,
    error::{Error, Result},
    governance::{self, Approval, Guardrail, Operation, Turn, Tx},
    store::{self, Identity},
};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::collections::HashMap;
use uuid::Uuid;

pub struct Verifier {
    keys: HashMap<String, DecodingKey>,
    validation: Validation,
    token_type: &'static str,
}
#[derive(Clone, Deserialize, Serialize)]
pub struct Claims {
    pub tenant_id: Uuid,
    pub iss: String,
    pub sub: String,
    pub aud: String,
    pub exp: i64,
    pub nbf: i64,
    pub iat: i64,
    pub auth_time: i64,
    pub jti: String,
    pub method: String,
    pub path: String,
    pub body_sha256: String,
    pub interactive: bool,
}
pub struct Human {
    pub(crate) tenant_id: Uuid,
    pub(crate) id: Uuid,
    claims: Claims,
    digest: Vec<u8>,
}
impl Human {
    pub(crate) fn expires_at(&self) -> i64 {
        self.claims.exp
    }
}
impl Verifier {
    pub fn new_approvals(
        issuer: &str,
        audience: &str,
        pem_keys: HashMap<String, String>,
    ) -> Result<Self> {
        let mut v = Self::new(issuer, audience, pem_keys)?;
        v.token_type = "br-approvals+jwt";
        Ok(v)
    }
    pub fn approvals_from_env() -> Result<Self> {
        let get = |name| std::env::var(name).map_err(|_| Error::Configuration);
        Self::new_approvals(
            &get("BROKERROUTER_APPROVALS_ISSUER")?,
            &get("BROKERROUTER_APPROVALS_AUDIENCE")?,
            serde_json::from_str(&get("BROKERROUTER_APPROVALS_PUBLIC_KEYS")?)
                .map_err(|_| Error::Configuration)?,
        )
    }
    pub fn new_settings(
        issuer: &str,
        audience: &str,
        pem_keys: HashMap<String, String>,
    ) -> Result<Self> {
        let mut verifier = Self::new(issuer, audience, pem_keys)?;
        verifier.token_type = "br-settings+jwt";
        Ok(verifier)
    }
    pub fn settings_from_env() -> Result<Self> {
        let get = |name| std::env::var(name).map_err(|_| Error::Configuration);
        Self::new_settings(
            &get("BROKERROUTER_SETTINGS_ISSUER")?,
            &get("BROKERROUTER_SETTINGS_AUDIENCE")?,
            serde_json::from_str(&get("BROKERROUTER_SETTINGS_PUBLIC_KEYS")?)
                .map_err(|_| Error::Configuration)?,
        )
    }
    pub fn new_catalog(
        issuer: &str,
        audience: &str,
        pem_keys: HashMap<String, String>,
    ) -> Result<Self> {
        let mut verifier = Self::new(issuer, audience, pem_keys)?;
        verifier.token_type = "br-catalog+jwt";
        Ok(verifier)
    }
    pub fn catalog_from_env() -> Result<Self> {
        let get = |name| std::env::var(name).map_err(|_| Error::Configuration);
        Self::new_catalog(
            &get("BROKERROUTER_CATALOG_ISSUER")?,
            &get("BROKERROUTER_CATALOG_AUDIENCE")?,
            serde_json::from_str(&get("BROKERROUTER_CATALOG_PUBLIC_KEYS")?)
                .map_err(|_| Error::Configuration)?,
        )
    }
    pub fn new_console(
        issuer: &str,
        audience: &str,
        pem_keys: HashMap<String, String>,
    ) -> Result<Self> {
        let mut verifier = Self::new(issuer, audience, pem_keys)?;
        verifier.token_type = "br-console+jwt";
        Ok(verifier)
    }
    pub fn console_from_env() -> Result<Self> {
        let get = |name| std::env::var(name).map_err(|_| Error::Configuration);
        Self::new_console(
            &get("BROKERROUTER_CONSOLE_ISSUER")?,
            &get("BROKERROUTER_CONSOLE_AUDIENCE")?,
            serde_json::from_str(&get("BROKERROUTER_CONSOLE_PUBLIC_KEYS")?)
                .map_err(|_| Error::Configuration)?,
        )
    }
    pub fn new(issuer: &str, audience: &str, pem_keys: HashMap<String, String>) -> Result<Self> {
        if !issuer.starts_with("https://")
            || !governance::valid_text(audience, 200)
            || pem_keys.is_empty()
            || pem_keys.len() > 16
        {
            return Err(Error::Configuration);
        }
        let mut keys = HashMap::new();
        for (kid, pem) in pem_keys {
            if !governance::valid_text(&kid, 100) || pem.len() > 16384 {
                return Err(Error::Configuration);
            }
            keys.insert(
                kid,
                DecodingKey::from_rsa_pem(pem.as_bytes()).map_err(|_| Error::Configuration)?,
            );
        }
        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_audience(&[audience]);
        validation.set_issuer(&[issuer]);
        validation.set_required_spec_claims(&["exp", "nbf", "iss", "aud", "sub"]);
        validation.leeway = 0;
        validation.validate_nbf = true;
        Ok(Self {
            keys,
            validation,
            token_type: "br-human+jwt",
        })
    }
    pub fn from_env() -> Result<Self> {
        let get = |name| std::env::var(name).map_err(|_| Error::Configuration);
        Self::new(
            &get("BROKERROUTER_HUMAN_ISSUER")?,
            &get("BROKERROUTER_HUMAN_AUDIENCE")?,
            serde_json::from_str(&get("BROKERROUTER_HUMAN_PUBLIC_KEYS")?)
                .map_err(|_| Error::Configuration)?,
        )
    }
    pub async fn verify(
        &self,
        pool: &PgPool,
        token: &str,
        method: &str,
        path: &str,
        body: &[u8],
    ) -> Result<Human> {
        if token.len() > 16384 {
            return Err(Error::HumanUnauthorized);
        }
        let header = decode_header(token).map_err(|_| Error::HumanUnauthorized)?;
        if header.alg != Algorithm::RS256
            || header.typ.as_deref() != Some(self.token_type)
            || header.crit.is_some()
            || header.jku.is_some()
            || header.jwk.is_some()
            || header.x5u.is_some()
            || header.enc.is_some()
            || header.zip.is_some()
        {
            return Err(Error::HumanUnauthorized);
        }
        let key = self
            .keys
            .get(header.kid.as_deref().ok_or(Error::HumanUnauthorized)?)
            .ok_or(Error::HumanUnauthorized)?;
        let claims = decode::<Claims>(token, key, &self.validation)
            .map_err(|_| Error::HumanUnauthorized)?
            .claims;
        let now = chrono::Utc::now().timestamp();
        if !claims.interactive
            || claims.exp <= claims.iat
            || claims.exp.saturating_sub(claims.iat) > 300
            || claims.iat > now + 5
            || claims.iat < now - 300
            || claims.auth_time > now + 5
            || claims.auth_time < now - 3600
            || claims.nbf > claims.exp
            || !governance::valid_text(&claims.jti, 200)
            || claims.method != method
            || claims.path != path
            || claims.body_sha256 != governance::hex(&store::hash(body))
        {
            return Err(Error::HumanUnauthorized);
        }
        let (tenant_id,id): (Uuid,Uuid)=sqlx::query_as("SELECT u.tenant_id,u.id FROM governance_users u JOIN tenants t ON t.id=u.tenant_id WHERE u.issuer=$1 AND u.subject=$2 AND u.tenant_id=$3 AND u.active AND t.active").bind(&claims.iss).bind(&claims.sub).bind(claims.tenant_id).fetch_optional(pool).await?.ok_or(Error::HumanUnauthorized)?;
        let digest = governance::digest(
            &json!({"tenant_id":tenant_id,"sub":claims.sub,"method":method,"path":path,"body":claims.body_sha256}),
        )?;
        Ok(Human {
            tenant_id,
            id,
            claims,
            digest,
        })
    }
}
pub(crate) async fn authorize(tx: &mut Tx<'_>, human: &Human) -> Result<()> {
    governance::tenant_lock(tx, human.tenant_id).await?;
    let live: bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM governance_users WHERE tenant_id=$1 AND id=$2 AND active) AND clock_timestamp()<to_timestamp($3::double precision)").bind(human.tenant_id).bind(human.id).bind(human.claims.exp as f64).fetch_one(&mut **tx).await?;
    if !live {
        return Err(Error::HumanUnauthorized);
    }
    sqlx::query("INSERT INTO human_assertions(issuer,jti,request_hash,expires_at) VALUES($1,$2,$3,to_timestamp($4::double precision)) ON CONFLICT DO NOTHING").bind(&human.claims.iss).bind(&human.claims.jti).bind(&human.digest).bind(human.claims.exp as f64).execute(&mut **tx).await?;
    let hash: Vec<u8> =
        sqlx::query_scalar("SELECT request_hash FROM human_assertions WHERE issuer=$1 AND jti=$2")
            .bind(&human.claims.iss)
            .bind(&human.claims.jti)
            .fetch_one(&mut **tx)
            .await?;
    if hash != human.digest {
        return Err(Error::HumanUnauthorized);
    }
    Ok(())
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NewTurn {
    pub key_id: Uuid,
    pub session_id: String,
    pub message_id: String,
    pub run_id: String,
    pub generation: i32,
    pub ttl_seconds: i32,
}
pub async fn create_turn(pool: &PgPool, human: &Human, new: NewTurn) -> Result<Value> {
    if ![&new.session_id, &new.message_id, &new.run_id]
        .iter()
        .all(|s| governance::valid_text(s, 200))
        || new.generation < 0
        || !(60..=3600).contains(&new.ttl_seconds)
    {
        return Err(Error::Invalid("invalid authoritative turn"));
    }
    let mut tx = pool.begin().await?;
    authorize(&mut tx, human).await?;
    let allowed: bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM runtime_bindings b JOIN virtual_keys k ON k.tenant_id=b.tenant_id AND k.id=b.key_id WHERE b.tenant_id=$1 AND b.user_id=$2 AND b.key_id=$3 AND b.active AND k.revoked_at IS NULL AND (k.expires_at IS NULL OR k.expires_at>clock_timestamp()))").bind(human.tenant_id).bind(human.id).bind(new.key_id).fetch_one(&mut *tx).await?;
    if !allowed {
        return Err(Error::NotFound);
    }
    let existing:Option<Turn>=sqlx::query_as("SELECT * FROM governance_turns WHERE tenant_id=$1 AND key_id=$2 AND session_id=$3 AND message_id=$4 AND generation=$5 FOR UPDATE").bind(human.tenant_id).bind(new.key_id).bind(&new.session_id).bind(&new.message_id).bind(new.generation).fetch_optional(&mut *tx).await?;
    if let Some(existing) = existing {
        if existing.run_id != new.run_id || existing.user_id != human.id {
            return Err(Error::Conflict);
        }
        tx.commit().await?;
        return serde_json::to_value(existing).map_err(|_| Error::Configuration);
    }
    let superseded:Vec<Turn>=sqlx::query_as("SELECT * FROM governance_turns WHERE tenant_id=$1 AND key_id=$2 AND session_id=$3 AND state='active' FOR UPDATE").bind(human.tenant_id).bind(new.key_id).bind(&new.session_id).fetch_all(&mut *tx).await?;
    for old in superseded {
        sqlx::query("UPDATE governance_turns SET state='cancelled' WHERE tenant_id=$1 AND id=$2")
            .bind(human.tenant_id)
            .bind(old.id)
            .execute(&mut *tx)
            .await?;
        governance::event(
            &mut tx,
            &old,
            "turn.closed",
            json!({"state":"cancelled","reason":"superseded_by_new_user_turn","user_id":human.id}),
        )
        .await?;
    }
    let id = Uuid::new_v4();
    let changed=sqlx::query("INSERT INTO governance_turns(tenant_id,id,key_id,user_id,session_id,message_id,run_id,generation,expires_at) VALUES($1,$2,$3,$4,$5,$6,$7,$8,now()+make_interval(secs=>$9)) ON CONFLICT(tenant_id,key_id,session_id,message_id,generation) DO NOTHING").bind(human.tenant_id).bind(id).bind(new.key_id).bind(human.id).bind(&new.session_id).bind(&new.message_id).bind(&new.run_id).bind(new.generation).bind(f64::from(new.ttl_seconds)).execute(&mut *tx).await?;
    let turn: Turn=sqlx::query_as("SELECT * FROM governance_turns WHERE tenant_id=$1 AND key_id=$2 AND session_id=$3 AND message_id=$4 AND generation=$5 FOR UPDATE").bind(human.tenant_id).bind(new.key_id).bind(new.session_id).bind(new.message_id).bind(new.generation).fetch_one(&mut *tx).await?;
    if turn.run_id != new.run_id || turn.user_id != human.id {
        return Err(Error::Conflict);
    }
    if changed.rows_affected() == 1 {
        governance::event(
            &mut tx,
            &turn,
            "turn.created",
            json!({"user_id":human.id,"run_id":turn.run_id,"generation":turn.generation}),
        )
        .await?;
    }
    tx.commit().await?;
    serde_json::to_value(turn).map_err(|_| Error::Configuration)
}
pub async fn close_turn(pool: &PgPool, human: &Human, id: Uuid, state: &str) -> Result<Value> {
    if !matches!(state, "completed" | "cancelled" | "failed") {
        return Err(Error::Invalid("invalid terminal turn state"));
    }
    let mut tx = pool.begin().await?;
    authorize(&mut tx, human).await?;
    let turn: Turn = sqlx::query_as(
        "SELECT * FROM governance_turns WHERE tenant_id=$1 AND user_id=$2 AND id=$3 FOR UPDATE",
    )
    .bind(human.tenant_id)
    .bind(human.id)
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(Error::NotFound)?;
    if turn.state != "active" && turn.state != state {
        return Err(Error::Conflict);
    }
    if turn.state == "active" {
        sqlx::query("UPDATE governance_turns SET state=$3 WHERE tenant_id=$1 AND id=$2")
            .bind(human.tenant_id)
            .bind(id)
            .bind(state)
            .execute(&mut *tx)
            .await?;
        governance::event(
            &mut tx,
            &turn,
            "turn.closed",
            json!({"state":state,"user_id":human.id}),
        )
        .await?;
    }
    tx.commit().await?;
    Ok(json!({"id":id,"state":state}))
}
pub async fn list_approvals(
    pool: &PgPool,
    cipher: &ResultCipher,
    human: &Human,
    turn: Uuid,
) -> Result<Value> {
    let mut tx = pool.begin().await?;
    authorize(&mut tx, human).await?;
    let key: Uuid = sqlx::query_scalar(
        "SELECT key_id FROM governance_turns WHERE tenant_id=$1 AND user_id=$2 AND id=$3",
    )
    .bind(human.tenant_id)
    .bind(human.id)
    .bind(turn)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(Error::NotFound)?;
    tx.commit().await?;
    governance::approvals(
        pool,
        cipher,
        &Identity {
            tenant_id: human.tenant_id,
            key_id: key,
        },
        turn,
    )
    .await
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Decision {
    pub decision: String,
    #[serde(default)]
    pub approved_guardrail_ids: Vec<String>,
    pub feedback: Option<String>,
}
pub async fn resolve(
    pool: &PgPool,
    cipher: &ResultCipher,
    human: &Human,
    id: Uuid,
    decision: Decision,
) -> Result<Value> {
    resolve_inner(pool, cipher, human, id, decision, None).await
}
pub(crate) async fn resolve_reviewed(
    pool: &PgPool,
    cipher: &ResultCipher,
    human: &Human,
    id: Uuid,
    decision: Decision,
    review_id: Uuid,
) -> Result<Value> {
    resolve_inner(pool, cipher, human, id, decision, Some(review_id)).await
}
async fn resolve_inner(
    pool: &PgPool,
    cipher: &ResultCipher,
    human: &Human,
    id: Uuid,
    mut decision: Decision,
    review_id: Option<Uuid>,
) -> Result<Value> {
    if !matches!(
        decision.decision.as_str(),
        "continue_this_turn" | "reject" | "feedback"
    ) || decision.approved_guardrail_ids.len() > 256
        || (decision.decision == "feedback"
            && decision
                .feedback
                .as_deref()
                .is_none_or(|s| s.trim().is_empty() || s.len() > 4000))
        || (decision.decision != "feedback" && decision.feedback.is_some())
        || (decision.decision != "continue_this_turn"
            && !decision.approved_guardrail_ids.is_empty())
    {
        return Err(Error::Invalid("invalid explicit user decision"));
    }
    decision.approved_guardrail_ids.sort();
    if decision
        .approved_guardrail_ids
        .windows(2)
        .any(|w| w[0] == w[1])
        || (decision.decision == "continue_this_turn" && decision.approved_guardrail_ids.is_empty())
    {
        return Err(Error::Invalid(
            "explicit unique guardrail selection required",
        ));
    }
    let decision_hash = governance::digest(
        &serde_json::to_value(&decision).map_err(|_| Error::Invalid("invalid decision"))?,
    )?;
    let mut tx = pool.begin().await?;
    authorize(&mut tx, human).await?;
    let a: Approval=sqlx::query_as("SELECT a.* FROM approvals a JOIN governance_turns t ON t.tenant_id=a.tenant_id AND t.id=a.turn_id WHERE a.tenant_id=$1 AND a.id=$2 AND t.user_id=$3 FOR UPDATE OF a").bind(human.tenant_id).bind(id).bind(human.id).fetch_optional(&mut *tx).await?.ok_or(Error::NotFound)?;
    let op: Operation = sqlx::query_as(
        "SELECT * FROM governance_operations WHERE tenant_id=$1 AND id=$2 FOR UPDATE",
    )
    .bind(human.tenant_id)
    .bind(a.operation_id)
    .fetch_one(&mut *tx)
    .await?;
    let who = Identity {
        tenant_id: human.tenant_id,
        key_id: op.key_id,
    };
    let turn = governance::turn_locked(&mut tx, &who, a.turn_id, false).await?;
    if let Some(hash) = a.decision_hash.as_ref() {
        if hash != &decision_hash {
            return Err(Error::Conflict);
        }
        let view = governance::approval_view(&a);
        tx.commit().await?;
        return Ok(view);
    }
    if let Some(review_id) = review_id {
        crate::approval_workbench::validate_review(&mut tx, human, review_id, &a, &op, &decision)
            .await?;
    }
    if turn.state != "active"
        || turn.expires_at <= chrono::Utc::now()
        || a.expires_at <= chrono::Utc::now()
    {
        return Err(Error::TurnClosed);
    }
    if matches!(
        op.state.as_str(),
        "rejected" | "feedback" | "executing" | "execution_unknown" | "completed"
    ) {
        return Err(Error::ActionRejected);
    }
    if let Some(model) = &op.model_id {
        let allowed: bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM key_models g JOIN models m ON m.tenant_id=g.tenant_id AND m.id=g.model_id WHERE g.tenant_id=$1 AND g.key_id=$2 AND g.model_id=$3 AND m.active)").bind(who.tenant_id).bind(who.key_id).bind(model).fetch_one(&mut *tx).await?;
        if !allowed {
            return Err(Error::Unauthorized);
        }
    }
    if op.stage == "media_output" {
        crate::output_delivery::validate_operation(&mut tx, &who, &op).await?;
    }
    let current = governance::selected(
        &mut tx,
        &who,
        &op.stage,
        op.model_id.as_deref(),
        op.tool_name.as_deref(),
    )
    .await?;
    if serde_json::to_value(current).map_err(|_| Error::Configuration)? != a.policy_snapshot {
        return Err(Error::PolicyChanged);
    }
    let risks = a.guardrails.as_array().ok_or(Error::Configuration)?;
    for selected in &decision.approved_guardrail_ids {
        let r = risks
            .iter()
            .find(|r| r["id"].as_str() == Some(selected))
            .ok_or(Error::Invalid("selected guardrail was not requested"))?;
        let version = r["version"].as_i64().ok_or(Error::Configuration)? as i32;
        let grant_expiry = if op.stage == "media_output" {
            turn.expires_at
        } else {
            a.expires_at
        };
        sqlx::query("INSERT INTO guardrail_grants(tenant_id,turn_id,guardrail_id,version,approval_id,expires_at) VALUES($1,$2,$3,$4,$5,$6) ON CONFLICT DO NOTHING").bind(who.tenant_id).bind(turn.id).bind(selected).bind(version).bind(id).bind(grant_expiry).execute(&mut *tx).await?;
    }
    let state = match decision.decision.as_str() {
        "continue_this_turn" => "approved",
        "reject" => "rejected",
        _ => "feedback",
    };
    let feedback = decision
        .feedback
        .as_ref()
        .map(|v| {
            cipher.encrypt(
                who.tenant_id,
                id,
                &serde_json::to_vec(v).map_err(|_| Error::Invalid("invalid feedback"))?,
            )
        })
        .transpose()?;
    sqlx::query("UPDATE approvals SET state=$3,decision_hash=$4,decided_by=$5,feedback_ciphertext=$6,decided_at=clock_timestamp() WHERE tenant_id=$1 AND id=$2 AND state='pending'").bind(who.tenant_id).bind(id).bind(state).bind(decision_hash).bind(human.id).bind(feedback).execute(&mut *tx).await?;
    if state != "approved" {
        sqlx::query("UPDATE governance_operations SET state=$3,payload_ciphertext=NULL WHERE tenant_id=$1 AND id=$2").bind(who.tenant_id).bind(op.id).bind(state).execute(&mut *tx).await?;
        // A rejected post result remains a real cost, but its protected content is destroyed.
        sqlx::query("UPDATE requests r SET result_ciphertext=NULL FROM governance_operations o WHERE o.tenant_id=$1 AND o.id=$2 AND o.stage='post' AND r.tenant_id=o.tenant_id AND r.id::text=o.operation_key AND r.governance_turn_id=o.turn_id").bind(who.tenant_id).bind(op.id).execute(&mut *tx).await?;
        if state == "feedback" {
            sqlx::query(
                "UPDATE governance_turns SET state='cancelled' WHERE tenant_id=$1 AND id=$2",
            )
            .bind(who.tenant_id)
            .bind(turn.id)
            .execute(&mut *tx)
            .await?;
            governance::event(
                &mut tx,
                &turn,
                "turn.closed",
                json!({"state":"cancelled","reason":"feedback_requires_new_turn"}),
            )
            .await?;
        }
    }
    governance::event(&mut tx,&turn,"approval.decided",json!({"approval_id":id,"operation_id":op.id,"state":state,"user_id":human.id,"approved_guardrail_ids":decision.approved_guardrail_ids,"review_id":review_id})).await?;
    if let Some(review_id) = review_id {
        sqlx::query("INSERT INTO audit_events(tenant_id,action,object_id,details) VALUES($1,'approval.decided',$2,$3)")
            .bind(human.tenant_id).bind(id.to_string()).bind(json!({"user_id":human.id,"review_id":review_id,"operation_id":op.id,"state":state,"approved_guardrail_ids":decision.approved_guardrail_ids})).execute(&mut *tx).await?;
    }
    crate::approval_workbench::fresh(&mut tx, human).await?;
    tx.commit().await?;
    let mut result = governance::approval_view(&a);
    result["state"] = state.into();
    Ok(result)
}

// Operator-only provisioning. No HTTP administrator bearer key is introduced.
pub async fn add_user(pool: &PgPool, tenant: Uuid, issuer: &str, subject: &str) -> Result<Uuid> {
    if !issuer.starts_with("https://")
        || !governance::valid_text(issuer, 500)
        || !governance::valid_text(subject, 500)
    {
        return Err(Error::Invalid("invalid trusted user mapping"));
    }
    let id = Uuid::new_v4();
    let mut tx = pool.begin().await?;
    governance::tenant_lock(&mut tx, tenant).await?;
    sqlx::query("INSERT INTO governance_users(tenant_id,id,issuer,subject) VALUES($1,$2,$3,$4)")
        .bind(tenant)
        .bind(id)
        .bind(issuer)
        .bind(subject)
        .execute(&mut *tx)
        .await?;
    operator_audit(
        &mut tx,
        tenant,
        "governance.user_registered",
        json!({"user_id":id}),
    )
    .await?;
    tx.commit().await?;
    Ok(id)
}
pub async fn bind_runtime(
    pool: &PgPool,
    tenant: Uuid,
    key: Uuid,
    user: Uuid,
    runtime: &str,
    agent: &str,
) -> Result<()> {
    if !governance::valid_text(runtime, 200) || !governance::valid_text(agent, 200) {
        return Err(Error::Invalid("invalid runtime identity"));
    }
    let mut tx = pool.begin().await?;
    governance::tenant_lock(&mut tx, tenant).await?;
    sqlx::query("INSERT INTO runtime_bindings(tenant_id,key_id,user_id,runtime_id,agent_id) VALUES($1,$2,$3,$4,$5)").bind(tenant).bind(key).bind(user).bind(runtime).bind(agent).execute(&mut *tx).await?;
    operator_audit(
        &mut tx,
        tenant,
        "governance.runtime_bound",
        json!({"key_id":key,"user_id":user,"runtime_id":runtime,"agent_id":agent}),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}
pub(crate) fn validate_guardrail(rule: &Guardrail) -> Result<()> {
    if rule.stage == "artifact" && rule.rule["kind"] == "model_judge" {
        return Err(Error::Invalid(
            "text_json judge does not inspect binary artifacts; an asset-aware connector is required",
        ));
    }
    if !governance::valid_text(&rule.id, 100)
        || !governance::valid_text(&rule.name, 200)
        || rule.version < 1
        || !matches!(
            rule.stage.as_str(),
            "pre" | "post" | "tool_input" | "tool_output" | "artifact" | "media_output"
        )
    {
        return Err(Error::Invalid("invalid guardrail version"));
    }
    governance::validate_rule(&rule.rule)?;
    if (rule.stage == "media_output"
        && !matches!(rule.rule["kind"].as_str(), Some("asset_scan" | "always")))
        || (rule.stage != "media_output" && rule.rule["kind"] == "asset_scan")
    {
        return Err(Error::Invalid(
            "asset_scan is server-only media_output; binary output does not support text rules",
        ));
    }
    Ok(())
}
pub async fn publish_guardrail(pool: &PgPool, tenant: Uuid, rule: Guardrail) -> Result<()> {
    validate_guardrail(&rule)?;
    let mut tx = pool.begin().await?;
    governance::tenant_lock(&mut tx, tenant).await?;
    let old: Option<i32> =
        sqlx::query_scalar("SELECT version FROM guardrails WHERE tenant_id=$1 AND id=$2")
            .bind(tenant)
            .bind(&rule.id)
            .fetch_optional(&mut *tx)
            .await?;
    if old.is_some_and(|v| rule.version <= v) {
        return Err(Error::Conflict);
    }
    sqlx::query("INSERT INTO guardrails(tenant_id,id,version) VALUES($1,$2,$3) ON CONFLICT(tenant_id,id) DO UPDATE SET version=EXCLUDED.version").bind(tenant).bind(&rule.id).bind(rule.version).execute(&mut *tx).await?;
    sqlx::query("INSERT INTO guardrail_versions(tenant_id,id,version,name,stage,rule) VALUES($1,$2,$3,$4,$5,$6)").bind(tenant).bind(&rule.id).bind(rule.version).bind(&rule.name).bind(&rule.stage).bind(&rule.rule).execute(&mut *tx).await?;
    operator_audit(
        &mut tx,
        tenant,
        "guardrail.published",
        json!({"id":rule.id,"version":rule.version,"stage":rule.stage}),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub id: Uuid,
    pub guardrail_id: String,
    pub model_id: Option<String>,
    pub key_id: Option<Uuid>,
    pub runtime_id: Option<String>,
    pub tool_name: Option<String>,
    pub active: bool,
}
pub async fn put_policy(pool: &PgPool, tenant: Uuid, policy: Policy) -> Result<()> {
    if !governance::valid_text(&policy.guardrail_id, 100)
        || [&policy.model_id, &policy.runtime_id, &policy.tool_name]
            .iter()
            .any(|s| {
                s.as_deref()
                    .is_some_and(|s| !governance::valid_text(s, 200))
            })
    {
        return Err(Error::Invalid("invalid policy selector"));
    }
    let mut tx = pool.begin().await?;
    governance::tenant_lock(&mut tx, tenant).await?;
    sqlx::query("INSERT INTO guardrail_policies(tenant_id,id,guardrail_id,model_id,key_id,runtime_id,tool_name,active) VALUES($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT(tenant_id,id) DO UPDATE SET guardrail_id=EXCLUDED.guardrail_id,model_id=EXCLUDED.model_id,key_id=EXCLUDED.key_id,runtime_id=EXCLUDED.runtime_id,tool_name=EXCLUDED.tool_name,active=EXCLUDED.active").bind(tenant).bind(policy.id).bind(&policy.guardrail_id).bind(&policy.model_id).bind(policy.key_id).bind(&policy.runtime_id).bind(&policy.tool_name).bind(policy.active).execute(&mut *tx).await?;
    operator_audit(
        &mut tx,
        tenant,
        "policy.updated",
        serde_json::to_value(policy).map_err(|_| Error::Configuration)?,
    )
    .await?;
    tx.commit().await?;
    Ok(())
}
async fn operator_audit(tx: &mut Tx<'_>, tenant: Uuid, action: &str, data: Value) -> Result<()> {
    sqlx::query("INSERT INTO audit_events(tenant_id,action,object_id,details) VALUES($1,$2,'governance',$3)").bind(tenant).bind(action).bind(data).execute(&mut **tx).await?;
    Ok(())
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Reconcile {
    pub outcome: String,
    pub evidence_ref: String,
}
pub async fn reconcile_operation(
    pool: &PgPool,
    cipher: &ResultCipher,
    human: &Human,
    id: Uuid,
    body: Reconcile,
) -> Result<Value> {
    if !matches!(
        body.outcome.as_str(),
        "confirmed_executed" | "confirmed_not_executed"
    ) || !governance::valid_text(&body.evidence_ref, 1000)
    {
        return Err(Error::Invalid(
            "explicit external evidence and outcome required",
        ));
    }
    let value =
        serde_json::to_value(&body).map_err(|_| Error::Invalid("invalid reconciliation"))?;
    let hash = governance::digest(&value)?;
    let mut tx = pool.begin().await?;
    authorize(&mut tx, human).await?;
    let op:Operation=sqlx::query_as("SELECT o.* FROM governance_operations o JOIN governance_turns t ON t.tenant_id=o.tenant_id AND t.id=o.turn_id WHERE o.tenant_id=$1 AND o.id=$2 AND t.user_id=$3 FOR UPDATE OF o").bind(human.tenant_id).bind(id).bind(human.id).fetch_optional(&mut *tx).await?.ok_or(Error::NotFound)?;
    if op.receipt_hash.as_ref() == Some(&hash) {
        tx.commit().await?;
        return Ok(value);
    }
    if op.state != "execution_unknown"
        && !(op.state == "executing"
            && op
                .execution_deadline
                .is_some_and(|d| d <= chrono::Utc::now()))
    {
        return Err(Error::InProgress);
    }
    let turn: Turn =
        sqlx::query_as("SELECT * FROM governance_turns WHERE tenant_id=$1 AND id=$2 FOR UPDATE")
            .bind(human.tenant_id)
            .bind(op.turn_id)
            .fetch_one(&mut *tx)
            .await?;
    let state = if body.outcome == "confirmed_executed" {
        "completed"
    } else {
        "rejected"
    };
    let encrypted = cipher.encrypt(
        human.tenant_id,
        id,
        &serde_json::to_vec(&value).map_err(|_| Error::Configuration)?,
    )?;
    sqlx::query("UPDATE governance_operations SET state=$3,receipt_hash=$4,receipt_ciphertext=$5,payload_ciphertext=NULL WHERE tenant_id=$1 AND id=$2").bind(human.tenant_id).bind(id).bind(state).bind(hash).bind(encrypted).execute(&mut *tx).await?;
    governance::event(&mut tx,&turn,"operation.reconciled",json!({"operation_id":id,"outcome":body.outcome,"user_id":human.id,"evidence_hash":governance::hex(&store::hash(body.evidence_ref.as_bytes()))})).await?;
    tx.commit().await?;
    Ok(value)
}
pub async fn maintain(pool: &PgPool) -> Result<()> {
    sqlx::query("WITH batch AS (SELECT issuer,jti FROM human_assertions WHERE expires_at<clock_timestamp() ORDER BY expires_at LIMIT 100 FOR UPDATE SKIP LOCKED) DELETE FROM human_assertions a USING batch b WHERE a.issuer=b.issuer AND a.jti=b.jti").execute(pool).await?;
    sqlx::query("WITH batch AS (SELECT tenant_id,id FROM approvals WHERE feedback_ciphertext IS NOT NULL AND expires_at+interval '24 hours'<=clock_timestamp() ORDER BY expires_at LIMIT 100 FOR UPDATE SKIP LOCKED) UPDATE approvals a SET feedback_ciphertext=NULL FROM batch b WHERE a.tenant_id=b.tenant_id AND a.id=b.id").execute(pool).await?;
    Ok(())
}
