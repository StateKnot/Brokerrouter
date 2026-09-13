use crate::{
    error::{Error, Result},
    money::{self, UnitPriceSnapshot, UnitRate, UnitUsage},
};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Endpoint {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub model_id: String,
    pub name: String,
    pub protocol: String,
    pub upstream_model: String,
    pub origin: String,
    pub credential_env: String,
    pub allow_private: bool,
    pub currency: String,
    pub priority: i32,
    pub profiles: Vec<Profile>,
    // Defaults decode previously submitted M4.2 routing snapshots. New admission requires a contract.
    #[serde(default)]
    pub maximum_output_bytes: i64,
    #[serde(default)]
    pub output_origins: Vec<String>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub resolution: String,
    pub aspect_ratio: String,
    pub durations: Vec<u32>,
    #[serde(with = "rust_decimal::serde::str")]
    pub price_per_second: Decimal,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateJob {
    pub model: String,
    pub task: String,
    pub input: Input,
    pub output: Output,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Input {
    pub prompt: String,
    #[serde(default)]
    pub negative_prompt: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Output {
    pub resolution: String,
    pub aspect_ratio: String,
    pub duration_seconds: u32,
}
impl CreateJob {
    pub fn validate(&self) -> Result<()> {
        if !crate::governance::valid_text(&self.model, 200)
            || self.task != "video.generate"
            || self.input.prompt.trim().is_empty()
            || self.input.prompt.chars().count() > 5000
            || self.input.prompt.contains('\0')
            || self
                .input
                .negative_prompt
                .as_ref()
                .is_some_and(|s| s.chars().count() > 500 || s.contains('\0'))
            || !(2..=15).contains(&self.output.duration_seconds)
        {
            return Err(Error::Invalid("unsupported media request or prompt limits"));
        }
        Ok(())
    }
}
impl Endpoint {
    pub fn validate_output_contract(&self) -> Result<()> {
        if !(1..=crate::assets::MAX_ASSET_BYTES).contains(&self.maximum_output_bytes)
            || self.output_origins.is_empty()
            || self.output_origins.len() > 16
        {
            return Err(Error::Invalid(
                "bounded output storage and allowed output origins are required",
            ));
        }
        let mut seen = std::collections::HashSet::new();
        for origin in &self.output_origins {
            let u = url::Url::parse(origin).map_err(|_| Error::Configuration)?;
            if origin.len() > 512
                || !u.username().is_empty()
                || u.password().is_some()
                || u.host_str().is_none()
                || u.query().is_some()
                || u.fragment().is_some()
                || u.path() != "/"
                || !(u.scheme() == "https" || (self.allow_private && u.scheme() == "http"))
                || !seen.insert(u.origin().ascii_serialization())
            {
                return Err(Error::Configuration);
            }
        }
        Ok(())
    }
    pub fn validate(&self) -> Result<()> {
        let u = url::Url::parse(&self.origin).map_err(|_| Error::Configuration)?;
        if self.id.is_nil()
            || self.protocol != "dashscope-wan27-v1"
            || !crate::governance::valid_text(&self.model_id, 200)
            || !crate::governance::valid_text(&self.name, 200)
            || !self.upstream_model.starts_with("wan2.7-t2v")
            || !crate::governance::valid_text(&self.upstream_model, 200)
            || u.host_str().is_none()
            || !u.username().is_empty()
            || u.password().is_some()
            || u.query().is_some()
            || u.fragment().is_some()
            || u.path() != "/"
            || !(u.scheme() == "https" || (self.allow_private && u.scheme() == "http"))
            || self.credential_env.is_empty()
            || self.credential_env.len() > 128
            || !self.credential_env.as_bytes()[0].is_ascii_uppercase()
            || !self
                .credential_env
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
            || !matches!(self.currency.as_str(), "CNY" | "USD")
            || !(0..=1000000).contains(&self.priority)
            || !(1..=10).contains(&self.profiles.len())
        {
            return Err(Error::Configuration);
        }
        let mut seen = std::collections::HashSet::new();
        for p in &self.profiles {
            if !matches!(p.resolution.as_str(), "720P" | "1080P")
                || !matches!(
                    p.aspect_ratio.as_str(),
                    "16:9" | "9:16" | "1:1" | "4:3" | "3:4"
                )
                || !seen.insert((&p.resolution, &p.aspect_ratio))
                || p.durations.is_empty()
                || p.durations.len() > 14
                || p.durations.iter().any(|d| !(2..=15).contains(d))
            {
                return Err(Error::Configuration);
            }
            money::nonnegative(&p.price_per_second.to_string())?;
        }
        Ok(())
    }
    pub fn profile(&self, r: &CreateJob) -> Option<&Profile> {
        self.profiles.iter().find(|p| {
            p.resolution == r.output.resolution
                && p.aspect_ratio == r.output.aspect_ratio
                && p.durations.contains(&r.output.duration_seconds)
        })
    }
    pub fn snapshot(&self, r: &CreateJob, fx: Option<(i64, Decimal)>) -> Result<UnitPriceSnapshot> {
        self.validate()?;
        let p = self.profile(r).ok_or(Error::NoEndpoint)?;
        let (fx_version, cny_per_unit) = if self.currency == "USD" {
            let (v, n) = fx.ok_or(Error::MissingFx)?;
            (Some(v), n)
        } else {
            (None, Decimal::ONE)
        };
        Ok(UnitPriceSnapshot {
            schema: "units-v1".into(),
            price_version: self.id,
            currency: self.currency.clone(),
            fx_version,
            cny_per_unit,
            rates: vec![UnitRate {
                unit: format!("video_second_{}", p.resolution.to_ascii_lowercase()),
                price_per_unit: p.price_per_second,
                maximum_quantity: Decimal::from(r.output.duration_seconds),
            }],
        })
    }
    pub fn payload(&self, r: &CreateJob) -> Value {
        let mut input = json!({"prompt":r.input.prompt});
        if let Some(n) = &r.input.negative_prompt {
            input["negative_prompt"] = json!(n);
        }
        json!({"model":self.upstream_model,"input":input,"parameters":{"resolution":r.output.resolution,"ratio":r.output.aspect_ratio,"duration":r.output.duration_seconds,"watermark":true,"prompt_extend":false}})
    }
}
pub fn task_id(v: &str) -> bool {
    !v.is_empty()
        && v.len() <= 200
        && v.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}
pub fn accepted(v: &Value) -> Result<String> {
    let id = v
        .pointer("/output/task_id")
        .and_then(Value::as_str)
        .filter(|v| task_id(v))
        .ok_or(Error::UpstreamUnknown)?;
    Ok(id.into())
}
pub enum Poll {
    Running,
    Complete {
        usage: Vec<UnitUsage>,
        output: Value,
    },
    Review(&'static str),
}
pub fn poll(v: &Value, id: &str, r: &CreateJob, allow_private: bool) -> Result<Poll> {
    if accepted(v)? != id {
        return Err(Error::UpstreamUnknown);
    }
    match v.pointer("/output/task_status").and_then(Value::as_str) {
        Some("PENDING" | "RUNNING") => Ok(Poll::Running),
        Some("SUCCEEDED") => {
            let number = |path: &str| {
                v.pointer(path)
                    .and_then(Value::as_u64)
                    .ok_or(Error::UpstreamUnknown)
            };
            let duration = number("/usage/output_video_duration")?;
            if number("/usage/video_count")? != 1
                || number("/usage/input_video_duration")? != 0
                || duration != u64::from(r.output.duration_seconds)
                || number("/usage/SR")?.to_string() != r.output.resolution.trim_end_matches('P')
                || v.pointer("/usage/ratio").and_then(Value::as_str) != Some(&r.output.aspect_ratio)
            {
                return Err(Error::UpstreamUnknown);
            }
            let billed = v
                .pointer("/usage/duration")
                .ok_or(Error::UpstreamUnknown)?
                .to_string();
            if money::nonnegative(&billed).map_err(|_| Error::UpstreamUnknown)?
                != Decimal::from(duration)
            {
                return Err(Error::UpstreamUnknown);
            }
            let raw = v
                .pointer("/output/video_url")
                .and_then(Value::as_str)
                .filter(|s| s.len() <= 8192)
                .ok_or(Error::UpstreamUnknown)?;
            let url = url::Url::parse(raw).map_err(|_| Error::UpstreamUnknown)?;
            if !(url.scheme() == "https" || (allow_private && url.scheme() == "http"))
                || url.host_str().is_none()
                || !url.username().is_empty()
                || url.password().is_some()
                || url.fragment().is_some()
            {
                return Err(Error::UpstreamUnknown);
            }
            Ok(Poll::Complete {
                usage: vec![UnitUsage {
                    unit: format!("video_second_{}", r.output.resolution.to_ascii_lowercase()),
                    quantity: Decimal::from(duration),
                }],
                output: json!({"video_url":raw,"media_type":"video/mp4","usage_evidence":{
                    "duration":v["usage"]["duration"],
                    "input_video_duration":v["usage"]["input_video_duration"],
                    "output_video_duration":v["usage"]["output_video_duration"],
                    "SR":v["usage"]["SR"],"ratio":v["usage"]["ratio"],"video_count":v["usage"]["video_count"]
                }}),
            })
        }
        Some("FAILED") => Ok(Poll::Review("provider_failed_verify_charge")),
        Some("CANCELED") => Ok(Poll::Review("provider_cancelled_verify_charge")),
        _ => Ok(Poll::Review("provider_state_unknown")),
    }
}
