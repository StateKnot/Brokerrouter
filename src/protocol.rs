use crate::{
    error::{Error, Result},
    store::Endpoint,
};
use serde_json::Value;

pub struct ValidatedRequest {
    pub body: Value,
    pub model: String,
    pub operation: &'static str,
    pub max_output: i32,
    pub batch_size: u32,
    pub needs_tools: bool,
}

fn fields(value: &Value, allowed: &[&str]) -> Result<()> {
    let object = value
        .as_object()
        .ok_or(Error::Invalid("JSON object required"))?;
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(Error::Invalid("unsupported request field"));
    }
    Ok(())
}
fn text(value: &Value) -> Result<&str> {
    value.as_str().ok_or(Error::Invalid("text string required"))
}
fn integer(value: Option<&Value>, default: i32, min: i32, max: i32) -> Result<i32> {
    let n = value
        .map(|v| v.as_i64().ok_or(Error::Invalid("integer required")))
        .transpose()?
        .unwrap_or(i64::from(default));
    if n < i64::from(min) || n > i64::from(max) {
        return Err(Error::Invalid("integer outside supported range"));
    }
    Ok(n as i32)
}
pub fn validate(mut body: Value, operation: &'static str) -> Result<ValidatedRequest> {
    let model = text(body.get("model").ok_or(Error::Invalid("model required"))?)?.to_owned();
    if model.is_empty() || model.len() > 200 {
        return Err(Error::Invalid("invalid model ID"));
    }
    if operation == "embedding" {
        fields(
            &body,
            &["model", "input", "encoding_format", "dimensions", "user"],
        )?;
        if body.get("encoding_format").is_some_and(|v| v != "float") {
            return Err(Error::Invalid("only float embeddings are supported"));
        }
        let input = body.get("input").ok_or(Error::Invalid("input required"))?;
        let inputs = if let Some(s) = input.as_str() {
            vec![s]
        } else {
            input
                .as_array()
                .ok_or(Error::Invalid("input must be a string or list of strings"))?
                .iter()
                .map(text)
                .collect::<Result<Vec<_>>>()?
        };
        if inputs.is_empty() || inputs.len() > 32 || inputs.iter().any(|s| s.is_empty()) {
            return Err(Error::Invalid(
                "embedding batch must contain 1 to 32 nonempty strings",
            ));
        }
        let batch_size = inputs.len() as u32;
        if let Some(v) = body.get("dimensions") {
            integer(Some(v), 1, 1, 65536)?;
        }
        if let Some(v) = body.get("user") {
            text(v)?;
        }
        body["encoding_format"] = Value::String("float".into());
        return Ok(ValidatedRequest {
            body,
            model,
            operation,
            max_output: 0,
            batch_size,
            needs_tools: false,
        });
    }
    fields(
        &body,
        &[
            "model",
            "messages",
            "max_tokens",
            "temperature",
            "top_p",
            "stop",
            "stream",
            "n",
            "tools",
            "tool_choice",
            "parallel_tool_calls",
            "user",
        ],
    )?;
    if body.get("stream").is_some_and(|v| v != false) {
        return Err(Error::Invalid(
            "streaming is not supported by this connector profile",
        ));
    }
    integer(body.get("n"), 1, 1, 1)?;
    let max_output = integer(body.get("max_tokens"), 1024, 1, 1_000_000)?;
    for (name, min, max) in [("temperature", 0.0, 2.0), ("top_p", 0.0, 1.0)] {
        if let Some(v) = body.get(name) {
            let n = v
                .as_f64()
                .ok_or(Error::Invalid("numeric sampling parameter required"))?;
            if !n.is_finite() || n < min || n > max {
                return Err(Error::Invalid("sampling parameter outside supported range"));
            }
        }
    }
    if let Some(v) = body.get("stop")
        && !v.is_string()
        && !(v
            .as_array()
            .is_some_and(|a| a.len() <= 4 && a.iter().all(Value::is_string)))
    {
        return Err(Error::Invalid("invalid stop parameter"));
    }
    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .ok_or(Error::Invalid("messages array required"))?;
    if messages.is_empty() || messages.len() > 1024 {
        return Err(Error::Invalid("messages must contain 1 to 1024 entries"));
    }
    let mut needs_tools = body.get("tools").is_some();
    for m in messages {
        fields(
            m,
            &["role", "content", "name", "tool_calls", "tool_call_id"],
        )?;
        let role = m
            .get("role")
            .and_then(Value::as_str)
            .ok_or(Error::Invalid("message role required"))?;
        if !["system", "user", "assistant", "tool"].contains(&role) {
            return Err(Error::Invalid("unsupported message role"));
        }
        if let Some(name) = m.get("name") {
            text(name)?;
        }
        let content = m.get("content");
        if !content.is_some_and(Value::is_string)
            && !(role == "assistant"
                && m.get("tool_calls").is_some()
                && content.is_none_or(Value::is_null))
        {
            return Err(Error::Invalid("only text message content is supported"));
        }
        if role == "tool" {
            needs_tools = true;
            text(
                m.get("tool_call_id")
                    .ok_or(Error::Invalid("tool_call_id required"))?,
            )?;
        }
        if let Some(calls) = m.get("tool_calls") {
            needs_tools = true;
            if role != "assistant" {
                return Err(Error::Invalid("tool_calls requires assistant role"));
            }
            for call in calls
                .as_array()
                .filter(|a| !a.is_empty() && a.len() <= 128)
                .ok_or(Error::Invalid("invalid tool_calls"))?
            {
                fields(call, &["id", "type", "function"])?;
                text(
                    call.get("id")
                        .ok_or(Error::Invalid("tool call id required"))?,
                )?;
                if call["type"] != "function" {
                    return Err(Error::Invalid("only function tools supported"));
                }
                fields(&call["function"], &["name", "arguments"])?;
                text(&call["function"]["name"])?;
                text(&call["function"]["arguments"])?;
            }
        }
    }
    if let Some(tools) = body.get("tools") {
        for tool in tools
            .as_array()
            .filter(|a| !a.is_empty() && a.len() <= 128)
            .ok_or(Error::Invalid("invalid tools"))?
        {
            fields(tool, &["type", "function"])?;
            if tool["type"] != "function" {
                return Err(Error::Invalid("only function tools supported"));
            }
            fields(&tool["function"], &["name", "description", "parameters"])?;
            text(&tool["function"]["name"])?;
            if let Some(d) = tool["function"].get("description") {
                text(d)?;
            }
            if !tool["function"]["parameters"].is_object() {
                return Err(Error::Invalid("tool parameters schema required"));
            }
        }
    }
    if let Some(choice) = body.get("tool_choice") {
        if !needs_tools {
            return Err(Error::Invalid("tool_choice requires tools"));
        }
        if !matches!(choice.as_str(), Some("auto" | "none" | "required")) {
            fields(choice, &["type", "function"])?;
            fields(&choice["function"], &["name"])?;
            if choice["type"] != "function" {
                return Err(Error::Invalid("invalid tool_choice"));
            }
            text(&choice["function"]["name"])?;
        }
    }
    if let Some(v) = body.get("parallel_tool_calls")
        && (!needs_tools || !v.is_boolean())
    {
        return Err(Error::Invalid(
            "parallel_tool_calls requires tools and a boolean",
        ));
    }
    if let Some(v) = body.get("user") {
        text(v)?;
    }
    body["max_tokens"] = max_output.into();
    body["stream"] = false.into();
    body["n"] = 1.into();
    Ok(ValidatedRequest {
        body,
        model,
        operation,
        max_output,
        batch_size: 1,
        needs_tools,
    })
}
impl ValidatedRequest {
    pub fn matches(&self, e: &Endpoint) -> bool {
        e.operation == self.operation
            && e.max_output_tokens >= self.max_output
            && (!self.needs_tools || e.supports_tools)
            && (self.operation != "embedding"
                || self
                    .body
                    .get("dimensions")
                    .is_none_or(|v| v.as_i64() == e.embedding_dimension.map(i64::from)))
    }
    pub fn maximum_input(&self, e: &Endpoint) -> u64 {
        e.context_tokens as u64 * u64::from(self.batch_size)
    }
}

pub fn validate_response(
    body: &Value,
    request: &ValidatedRequest,
    e: &Endpoint,
) -> Result<(u64, u64)> {
    let invalid = || Error::UpstreamUnknown;
    let usage = body.get("usage").ok_or_else(invalid)?;
    let input = usage["prompt_tokens"].as_u64().ok_or_else(invalid)?;
    let output = if request.operation == "chat" {
        usage["completion_tokens"].as_u64().ok_or_else(invalid)?
    } else {
        0
    };
    if input > request.maximum_input(e)
        || output > request.max_output as u64
        || usage["total_tokens"].as_u64() != input.checked_add(output)
    {
        return Err(invalid());
    }
    if request.operation == "chat" {
        let choices = body["choices"]
            .as_array()
            .filter(|a| a.len() == 1)
            .ok_or_else(invalid)?;
        let message = &choices[0]["message"];
        if body["object"] != "chat.completion"
            || !body["id"].is_string()
            || message["role"] != "assistant"
            || !choices[0]["finish_reason"].is_string()
        {
            return Err(invalid());
        }
        if !message["content"].is_string()
            && !message["tool_calls"].is_array()
            && !message["refusal"].is_string()
        {
            return Err(invalid());
        }
        if message.get("tool_calls").is_some() && !request.needs_tools {
            return Err(invalid());
        }
        if let Some(calls) = message.get("tool_calls") {
            let calls = calls
                .as_array()
                .filter(|a| !a.is_empty() && a.len() <= 128)
                .ok_or_else(invalid)?;
            let mut ids = std::collections::HashSet::new();
            for call in calls {
                let id = call["id"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .ok_or_else(invalid)?;
                if !ids.insert(id)
                    || call["type"] != "function"
                    || !call["function"]["name"].is_string()
                    || !call["function"]["arguments"].is_string()
                {
                    return Err(invalid());
                }
            }
        }
    } else {
        let data = body["data"]
            .as_array()
            .filter(|v| v.len() == request.batch_size as usize)
            .ok_or_else(invalid)?;
        let dimension = e.embedding_dimension.ok_or_else(invalid)? as usize;
        let mut indices = std::collections::HashSet::new();
        for item in data {
            let index = item["index"]
                .as_u64()
                .filter(|v| *v < u64::from(request.batch_size))
                .ok_or_else(invalid)?;
            if !indices.insert(index) {
                return Err(invalid());
            }
            if !item["embedding"].as_array().is_some_and(|a| {
                a.len() == dimension && a.iter().all(|v| v.as_f64().is_some_and(f64::is_finite))
            }) {
                return Err(invalid());
            }
        }
    }
    Ok((input, output))
}
