#![allow(dead_code)]
use brokerrouter::{
    cipher::ResultCipher,
    human::{self, Claims},
    store,
};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use rsa::pkcs8::{EncodePrivateKey, EncodePublicKey, LineEnding};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::{collections::HashMap, sync::OnceLock};
use uuid::Uuid;
pub const ISSUER: &str = "https://test-bff.example";
pub const AUDIENCE: &str = "brokerrouter-human-test";
fn keys() -> &'static (EncodingKey, String) {
    static KEYS: OnceLock<(EncodingKey, String)> = OnceLock::new();
    KEYS.get_or_init(|| {
        let private = rsa::RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 2048).unwrap();
        let public = private
            .to_public_key()
            .to_public_key_pem(LineEnding::LF)
            .unwrap();
        (
            EncodingKey::from_rsa_pem(private.to_pkcs8_pem(LineEnding::LF).unwrap().as_bytes())
                .unwrap(),
            public,
        )
    })
}
pub fn verifier() -> human::Verifier {
    human::Verifier::new(
        ISSUER,
        AUDIENCE,
        HashMap::from([("fixture".into(), keys().1.clone())]),
    )
    .unwrap()
}
pub fn console_verifier() -> human::Verifier {
    human::Verifier::new_console(
        ISSUER,
        "brokerrouter-console-test",
        HashMap::from([("fixture".into(), keys().1.clone())]),
    )
    .unwrap()
}
pub fn console_sign(mut claims: Claims) -> String {
    claims.aud = "brokerrouter-console-test".into();
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some("fixture".into());
    header.typ = Some("br-console+jwt".into());
    jsonwebtoken::encode(&header, &claims, &keys().0).unwrap()
}
pub fn catalog_verifier() -> human::Verifier {
    human::Verifier::new_catalog(
        ISSUER,
        "brokerrouter-catalog-test",
        HashMap::from([("fixture".into(), keys().1.clone())]),
    )
    .unwrap()
}
pub fn catalog_sign(mut claims: Claims) -> String {
    claims.aud = "brokerrouter-catalog-test".into();
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some("fixture".into());
    header.typ = Some("br-catalog+jwt".into());
    jsonwebtoken::encode(&header, &claims, &keys().0).unwrap()
}
pub fn settings_verifier() -> human::Verifier {
    human::Verifier::new_settings(
        ISSUER,
        "brokerrouter-settings-test",
        HashMap::from([("fixture".into(), keys().1.clone())]),
    )
    .unwrap()
}
pub fn settings_sign(mut claims: Claims) -> String {
    claims.aud = "brokerrouter-settings-test".into();
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some("fixture".into());
    header.typ = Some("br-settings+jwt".into());
    jsonwebtoken::encode(&header, &claims, &keys().0).unwrap()
}
pub fn cipher() -> ResultCipher {
    ResultCipher::new(
        "fixture".into(),
        HashMap::from([("fixture".into(), "88".repeat(32))]),
    )
    .unwrap()
}
pub fn claims(tenant_id: Uuid, subject: &str, path: &str, body: &Value) -> Claims {
    let now = chrono::Utc::now().timestamp();
    Claims {
        tenant_id,
        iss: ISSUER.into(),
        sub: subject.into(),
        aud: AUDIENCE.into(),
        exp: now + 120,
        nbf: now - 1,
        iat: now,
        auth_time: now,
        jti: Uuid::new_v4().to_string(),
        method: "POST".into(),
        path: path.into(),
        body_sha256: hex(&store::hash(&serde_json::to_vec(body).unwrap())),
        interactive: true,
    }
}
pub fn sign(claims: &Claims) -> String {
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some("fixture".into());
    header.typ = Some("br-human+jwt".into());
    jsonwebtoken::encode(&header, claims, &keys().0).unwrap()
}
pub fn hex(b: &[u8]) -> String {
    b.iter().map(|v| format!("{v:02x}")).collect()
}
pub async fn proof(pool: &PgPool, subject: &str, path: &str, body: &Value) -> human::Human {
    let tenant: Uuid =
        sqlx::query_scalar("SELECT tenant_id FROM governance_users WHERE subject=$1 LIMIT 1")
            .bind(subject)
            .fetch_one(pool)
            .await
            .unwrap();
    verifier()
        .verify(
            pool,
            &sign(&claims(tenant, subject, path, body)),
            "POST",
            path,
            &serde_json::to_vec(body).unwrap(),
        )
        .await
        .unwrap()
}
pub async fn provision(pool: &PgPool, who: &store::Identity, subject: &str) -> Uuid {
    let user = human::add_user(pool, who.tenant_id, ISSUER, subject)
        .await
        .unwrap();
    human::bind_runtime(
        pool,
        who.tenant_id,
        who.key_id,
        user,
        "hermes-fixture",
        "agent-fixture",
    )
    .await
    .unwrap();
    user
}
pub async fn turn(pool: &PgPool, who: &store::Identity, subject: &str, generation: i32) -> Uuid {
    let body = json!({"key_id":who.key_id,"session_id":"session","message_id":"message","run_id":"run","generation":generation,"ttl_seconds":900});
    let human = proof(pool, subject, "/v1/governance/turns", &body).await;
    let response = human::create_turn(pool, &human, serde_json::from_value(body).unwrap())
        .await
        .unwrap();
    serde_json::from_value(response["id"].clone()).unwrap()
}
pub async fn decide(
    pool: &PgPool,
    subject: &str,
    id: Uuid,
    body: Value,
) -> brokerrouter::error::Result<Value> {
    let path = format!("/v1/approvals/{id}/resolve");
    let p = proof(pool, subject, &path, &body).await;
    human::resolve(
        pool,
        &cipher(),
        &p,
        id,
        serde_json::from_value(body).unwrap(),
    )
    .await
}
pub async fn guardrail(
    pool: &PgPool,
    tenant: Uuid,
    id: &str,
    version: i32,
    stage: &str,
    rule: Value,
) {
    human::publish_guardrail(
        pool,
        tenant,
        brokerrouter::governance::Guardrail {
            id: id.into(),
            version,
            name: id.into(),
            stage: stage.into(),
            rule,
        },
    )
    .await
    .unwrap();
    human::put_policy(
        pool,
        tenant,
        human::Policy {
            id: Uuid::new_v4(),
            guardrail_id: id.into(),
            model_id: None,
            key_id: None,
            runtime_id: None,
            tool_name: None,
            active: true,
        },
    )
    .await
    .unwrap();
}
pub fn approve(ids: &[&str]) -> Value {
    json!({"decision":"continue_this_turn","approved_guardrail_ids":ids,"feedback":null})
}
pub fn approval_id(value: &Value) -> Uuid {
    serde_json::from_value(value["approval"]["id"].clone()).unwrap()
}
