use anyhow::{Context, bail};
use clap::Parser;
use reqwest::StatusCode;
use serde_json::{Value, json};
use std::time::Duration;
use url::Url;
use uuid::Uuid;
use zeroize::Zeroizing;

#[derive(Parser)]
#[command(about = "Certify one Brokerrouter logical model for an exact two-turn tool roundtrip")]
struct Cli {
    #[arg(long, default_value = "http://127.0.0.1:8080")]
    base_url: String,
    #[arg(long)]
    model: String,
    #[arg(long, default_value = "BROKERROUTER_CERT_VIRTUAL_KEY")]
    virtual_key_env: String,
    #[arg(long)]
    operation_prefix: Option<Uuid>,
}

struct Call {
    body: Value,
    request_id: Uuid,
}

const MAX_GATEWAY_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let mut base = Url::parse(&cli.base_url).context("invalid Brokerrouter base URL")?;
    if base.host_str().is_none()
        || !base.username().is_empty()
        || base.password().is_some()
        || base.query().is_some()
        || base.fragment().is_some()
        || !matches!(base.scheme(), "http" | "https")
    {
        bail!("base URL must be an HTTP(S) origin without credentials, query or fragment");
    }
    if cli.model.is_empty() || cli.model.len() > 200 {
        bail!("invalid logical model ID");
    }
    base.set_path(&format!("{}/", base.path().trim_end_matches('/')));
    let token = Zeroizing::new(std::env::var(&cli.virtual_key_env).with_context(|| {
        format!(
            "missing virtual key environment variable {}",
            cli.virtual_key_env
        )
    })?);
    if !token.starts_with("sk_br_") || token.len() != 70 {
        bail!("virtual key environment variable does not contain a Brokerrouter key");
    }
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(120))
        .build()?;
    let prefix = cli.operation_prefix.unwrap_or_else(Uuid::new_v4);
    let tools = json!([{"type":"function","function":{"name":"add","description":"Add two integers locally","parameters":{"type":"object","properties":{"a":{"type":"integer"},"b":{"type":"integer"}},"required":["a","b"]}}}]);
    let first_body = json!({
        "model":cli.model,
        "messages":[{"role":"user","content":"Use the add tool exactly once to add 17 and 25. Do not calculate it yourself."}],
        "tools":tools,
        "tool_choice":{"type":"function","function":{"name":"add"}},
        "parallel_tool_calls":false,
        "max_tokens":64
    });
    let first_key = format!("tool-cert:{prefix}:call");
    let first = call(&client, &base, token.as_str(), &first_key, &first_body).await?;
    let message = &first.body["choices"][0]["message"];
    let calls = message["tool_calls"]
        .as_array()
        .filter(|calls| calls.len() == 1)
        .context("first response did not contain exactly one tool call")?;
    if calls[0].get("index").is_some()
        || calls[0]["type"] != "function"
        || calls[0]["function"]["name"] != "add"
    {
        bail!("tool call was not normalized to the certified id/type/function contract");
    }
    let arguments: Value = serde_json::from_str(
        calls[0]["function"]["arguments"]
            .as_str()
            .context("tool arguments are not a JSON string")?,
    )?;
    if arguments["a"].as_i64() != Some(17) || arguments["b"].as_i64() != Some(25) {
        bail!("model did not request the fixed add(17,25) fixture");
    }
    let call_id = calls[0]["id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .context("tool call ID missing")?;
    let second_body = json!({
        "model":cli.model,
        "messages":[
            {"role":"user","content":"Use the add tool exactly once to add 17 and 25. Do not calculate it yourself."},
            message,
            {"role":"tool","tool_call_id":call_id,"content":"42"}
        ],
        "tools":tools,
        "tool_choice":"auto",
        "parallel_tool_calls":false,
        "max_tokens":64
    });
    let second_key = format!("tool-cert:{prefix}:continue");
    let second = call(&client, &base, token.as_str(), &second_key, &second_body).await?;
    let content = second.body["choices"][0]["message"]["content"]
        .as_str()
        .context("continuation did not return assistant text")?;
    if !content.contains("42") {
        bail!("continuation did not consume the local tool result");
    }
    let first_replay = call(&client, &base, token.as_str(), &first_key, &first_body).await?;
    let second_replay = call(&client, &base, token.as_str(), &second_key, &second_body).await?;
    if first_replay.request_id != first.request_id
        || second_replay.request_id != second.request_id
        || first_replay.body != first.body
        || second_replay.body != second.body
    {
        bail!("idempotent replay changed request identity or result");
    }
    let first_detail = detail(&client, &base, token.as_str(), first.request_id).await?;
    let second_detail = detail(&client, &base, token.as_str(), second.request_id).await?;
    for detail in [&first_detail, &second_detail] {
        if detail["status"] != "succeeded"
            || detail["reserved_cny"] != "0.000000"
            || detail["cost_cny"].as_str().is_none()
            || detail["attempts"].as_array().map(Vec::len) != Some(1)
            || detail["attempts"][0]["status"] != "succeeded"
        {
            bail!("request detail does not prove one settled attempt with zero reservation");
        }
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "schema":"brokerrouter-tool-roundtrip-certification-v1",
            "logical_model":cli.model,
            "operation_prefix":prefix,
            "first":{"request_id":first.request_id,"usage":first.body["usage"],"cost_cny":first_detail["cost_cny"]},
            "continuation":{"request_id":second.request_id,"usage":second.body["usage"],"cost_cny":second_detail["cost_cny"]},
            "checks":{"normalized_tool_call":true,"exact_message_reuse":true,"assistant_continuation":true,"one_attempt_each":true,"idempotent_replay":true,"reserved_cny_zero":true},
            "notice":"Configured-price CNY evidence; reconcile separately with the provider invoice."
        }))?
    );
    Ok(())
}

async fn call(
    client: &reqwest::Client,
    base: &Url,
    token: &str,
    idempotency: &str,
    body: &Value,
) -> anyhow::Result<Call> {
    let response = client
        .post(base.join("v1/chat/completions")?)
        .bearer_auth(token)
        .header("Idempotency-Key", idempotency)
        .json(body)
        .send()
        .await?;
    let status = response.status();
    let request_id = response
        .headers()
        .get("x-brokerrouter-request-id")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<Uuid>().ok());
    let bytes = bounded_body(response, MAX_GATEWAY_RESPONSE_BYTES).await?;
    let value: Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("gateway returned non-JSON HTTP {status}"))?;
    if status != StatusCode::OK {
        bail!("gateway HTTP {status}: {}", value["error"]["code"]);
    }
    Ok(Call {
        body: value,
        request_id: request_id.context("gateway response omitted logical request ID")?,
    })
}

async fn detail(
    client: &reqwest::Client,
    base: &Url,
    token: &str,
    id: Uuid,
) -> anyhow::Result<Value> {
    let response = client
        .get(base.join(&format!("v1/requests/{id}"))?)
        .bearer_auth(token)
        .send()
        .await?;
    let status = response.status();
    if status != StatusCode::OK {
        bail!("request detail returned HTTP {status}");
    }
    Ok(serde_json::from_slice(
        &bounded_body(response, MAX_GATEWAY_RESPONSE_BYTES).await?,
    )?)
}

async fn bounded_body(mut response: reqwest::Response, limit: usize) -> anyhow::Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        bail!("gateway response exceeded {limit} bytes");
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if chunk.len() > limit.saturating_sub(body.len()) {
            bail!("gateway response exceeded {limit} bytes");
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}
