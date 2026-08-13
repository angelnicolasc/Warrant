//! The OpenAI chat-completions transport.
//!
//! One wire format, many providers: OpenAI, DeepSeek, Groq, Together,
//! Fireworks, OpenRouter, and anything self-hosted behind Ollama, vLLM or
//! LM Studio. All of them accept the same request shape, so this is a second
//! transport rather than a second integration per vendor.
//!
//! # It is not the same shape as Anthropic's
//!
//! Four differences, each of which is a place a naive adapter goes wrong:
//!
//! - the system prompt is a **message**, not a top-level field;
//! - tool arguments arrive as a **JSON string** that has to be parsed, and a
//!   model can and does emit one that will not parse;
//! - a tool result is a message with `role: "tool"`, not a block inside a
//!   user message — and one turn can produce several;
//! - token counts are `prompt_tokens` / `completion_tokens`.
//!
//! A model that produces unparseable tool arguments is not a transport
//! failure. The call is passed through with a null argument object and the
//! tool reports what it needed, so the model gets a chance to correct itself.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::error::{AgentError, Result};
use crate::provider::{
    ContentBlock, Message, ModelRequest, ModelResponse, Provider, Role, StopReason, Usage,
};

/// Where OpenAI lives unless told otherwise.
pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

/// DeepSeek's endpoint, which speaks the same format.
pub const DEEPSEEK_BASE_URL: &str = "https://api.deepseek.com/v1";

/// Environment variable holding the key.
pub const API_KEY_VAR: &str = "OPENAI_API_KEY";

/// Environment variable overriding the base URL.
pub const BASE_URL_VAR: &str = "OPENAI_BASE_URL";

/// Which field carries the output ceiling.
///
/// Not cosmetic. Newer OpenAI reasoning models reject `max_tokens` and
/// require `max_completion_tokens`; DeepSeek and most compatible servers
/// accept only `max_tokens`. Guessing wrong fails the very first request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenLimitField {
    /// `max_tokens`. The broadly compatible choice.
    MaxTokens,
    /// `max_completion_tokens`. Required by newer OpenAI models.
    MaxCompletionTokens,
}

impl TokenLimitField {
    fn name(&self) -> &'static str {
        match self {
            TokenLimitField::MaxTokens => "max_tokens",
            TokenLimitField::MaxCompletionTokens => "max_completion_tokens",
        }
    }

    /// Parse a name from configuration.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "max_tokens" => Some(TokenLimitField::MaxTokens),
            "max_completion_tokens" => Some(TokenLimitField::MaxCompletionTokens),
            _ => None,
        }
    }
}

/// A provider that talks to any OpenAI-compatible endpoint.
pub struct OpenAiProvider {
    agent: ureq::Agent,
    base_url: String,
    api_key: String,
    max_retries: u32,
    retry_delay: Duration,
    token_field: TokenLimitField,
    label: String,
}

impl OpenAiProvider {
    /// Build a provider from an explicit key.
    pub fn new(api_key: impl Into<String>) -> Self {
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(600)))
            .build()
            .into();
        OpenAiProvider {
            agent,
            base_url: DEFAULT_BASE_URL.to_owned(),
            api_key: api_key.into(),
            max_retries: 3,
            retry_delay: Duration::from_millis(500),
            token_field: TokenLimitField::MaxTokens,
            label: "openai".to_owned(),
        }
    }

    /// Build a provider from the environment.
    pub fn from_env() -> Result<Self> {
        let key = std::env::var(API_KEY_VAR).map_err(|_| AgentError::Provider {
            provider: "openai".into(),
            message: format!(
                "{API_KEY_VAR} is not set. Any OpenAI-compatible endpoint works — set \
                 {BASE_URL_VAR} to point at DeepSeek, a gateway, or a local server."
            ),
        })?;
        let mut provider = OpenAiProvider::new(key);
        if let Ok(base) = std::env::var(BASE_URL_VAR)
            && !base.trim().is_empty()
        {
            provider = provider.with_base_url(base);
        }
        Ok(provider)
    }

    /// Point the provider at a different host.
    ///
    /// The `/v1` suffix is added when it is missing, because half the
    /// documentation in this ecosystem includes it and half does not.
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        let trimmed = base_url.into().trim_end_matches('/').to_owned();
        self.base_url = if trimmed.ends_with("/v1") || trimmed.contains("/v1/") {
            trimmed
        } else {
            format!("{trimmed}/v1")
        };
        self
    }

    /// Name this provider in error messages.
    pub fn labelled(mut self, label: impl Into<String>) -> Self {
        self.label = label.into();
        self
    }

    /// Choose which field carries the output ceiling.
    pub fn with_token_field(mut self, field: TokenLimitField) -> Self {
        self.token_field = field;
        self
    }

    /// How many times a retryable failure is retried.
    pub fn with_max_retries(mut self, retries: u32) -> Self {
        self.max_retries = retries;
        self
    }

    /// How long to wait between retries.
    pub fn with_retry_delay(mut self, delay: Duration) -> Self {
        self.retry_delay = delay;
        self
    }

    fn endpoint(&self) -> String {
        format!("{}/chat/completions", self.base_url)
    }
}

/// Translate a request into chat-completions shape.
///
/// Split out from the transport so the translation can be tested against
/// expected JSON without a socket.
pub fn to_wire(request: &ModelRequest, token_field: TokenLimitField) -> Value {
    let mut messages: Vec<Value> = Vec::new();
    if !request.system.is_empty() {
        messages.push(json!({ "role": "system", "content": request.system }));
    }
    for message in &request.messages {
        push_message(&mut messages, message);
    }

    let mut body = json!({
        "model": request.model,
        "messages": messages,
        token_field.name(): request.max_tokens,
    });
    if !request.tools.is_empty() {
        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "type": "function",
                    "function": {
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.input_schema,
                    }
                })
            })
            .collect();
        body["tools"] = Value::Array(tools);
    }
    body
}

fn push_message(out: &mut Vec<Value>, message: &Message) {
    let mut text = String::new();
    let mut tool_calls: Vec<Value> = Vec::new();

    for block in &message.content {
        match block {
            ContentBlock::Text { text: part } => {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(part);
            }
            ContentBlock::ToolUse { id, name, input } => tool_calls.push(json!({
                "id": id,
                "type": "function",
                "function": {
                    "name": name,
                    // Arguments go over the wire as a string in this format.
                    "arguments": serde_json::to_string(input).unwrap_or_else(|_| "{}".into()),
                },
            })),
            // A tool result is its own message here, and must come before any
            // prose the harness added in the same turn.
            ContentBlock::ToolResult { tool_use_id, content, .. } => out.push(json!({
                "role": "tool",
                "tool_call_id": tool_use_id,
                "content": content,
            })),
        }
    }

    let role = match message.role {
        Role::User => "user",
        Role::Assistant => "assistant",
    };

    if !tool_calls.is_empty() {
        out.push(json!({
            "role": role,
            // The field must be present even when there is no prose.
            "content": if text.is_empty() { Value::Null } else { Value::String(text) },
            "tool_calls": tool_calls,
        }));
    } else if !text.is_empty() {
        out.push(json!({ "role": role, "content": text }));
    }
}

fn stop_reason_from(raw: Option<&str>) -> StopReason {
    match raw {
        Some("stop") | None => StopReason::EndTurn,
        Some("tool_calls") | Some("function_call") => StopReason::ToolUse,
        Some("length") => StopReason::MaxTokens,
        Some("content_filter") => StopReason::Refusal,
        Some(other) => StopReason::Other(other.to_owned()),
    }
}

/// Translate a chat-completions response back.
pub fn from_wire(body: &[u8], label: &str) -> Result<ModelResponse> {
    let value: Value = serde_json::from_slice(body).map_err(|e| AgentError::Provider {
        provider: label.to_owned(),
        message: format!("the response was not JSON: {e}"),
    })?;

    let choice =
        value.get("choices").and_then(|c| c.get(0)).ok_or_else(|| AgentError::Provider {
            provider: label.to_owned(),
            message: "the response contained no choices".into(),
        })?;
    let message = choice.get("message").ok_or_else(|| AgentError::Provider {
        provider: label.to_owned(),
        message: "the response's choice contained no message".into(),
    })?;

    let mut content = Vec::new();
    if let Some(text) = message.get("content").and_then(Value::as_str)
        && !text.is_empty()
    {
        content.push(ContentBlock::text(text));
    }

    if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
        for call in calls {
            let function = call.get("function");
            let name = function
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let raw =
                function.and_then(|f| f.get("arguments")).and_then(Value::as_str).unwrap_or("{}");

            // A model that emits arguments which will not parse is a routine
            // event, not a transport failure. The call goes through with a
            // null argument object; the tool then reports what it needed and
            // the model gets a turn to fix it.
            let input = serde_json::from_str(raw).unwrap_or(Value::Null);

            content.push(ContentBlock::ToolUse {
                id: call.get("id").and_then(Value::as_str).unwrap_or_default().to_owned(),
                name,
                input,
            });
        }
    }

    let usage = value.get("usage");
    let read =
        |field: &str| usage.and_then(|u| u.get(field)).and_then(Value::as_u64).unwrap_or_default();

    Ok(ModelResponse {
        content,
        stop_reason: stop_reason_from(choice.get("finish_reason").and_then(Value::as_str)),
        usage: Usage {
            input_tokens: read("prompt_tokens"),
            output_tokens: read("completion_tokens"),
        },
    })
}

#[derive(Debug, Deserialize, Serialize)]
struct WireErrorBody {
    error: WireError,
}

#[derive(Debug, Deserialize, Serialize)]
struct WireError {
    #[serde(default)]
    message: String,
    #[serde(rename = "type", default)]
    kind: String,
}

fn describe_error(status: u16, body: &[u8]) -> String {
    match serde_json::from_slice::<WireErrorBody>(body) {
        Ok(parsed) if !parsed.error.message.is_empty() => {
            format!("HTTP {status} ({}): {}", parsed.error.kind, parsed.error.message)
        }
        _ => {
            let text = String::from_utf8_lossy(body);
            let text = text.trim();
            if text.is_empty() {
                format!("HTTP {status}")
            } else {
                format!("HTTP {status}: {}", &text[..text.len().min(400)])
            }
        }
    }
}

fn is_retryable(status: u16) -> bool {
    status == 408 || status == 409 || status == 429 || (500..600).contains(&status)
}

impl Provider for OpenAiProvider {
    fn name(&self) -> &str {
        &self.label
    }

    fn complete(&self, request: &ModelRequest) -> Result<ModelResponse> {
        let body = to_wire(request, self.token_field);

        let mut attempt = 0;
        loop {
            let sent = self
                .agent
                .post(self.endpoint())
                .header("authorization", format!("Bearer {}", self.api_key))
                .header("content-type", "application/json")
                .send_json(&body);

            let (status, payload) = match sent {
                Ok(mut response) => {
                    let status = response.status().as_u16();
                    let payload = response.body_mut().read_to_vec().map_err(|e| {
                        AgentError::Transport(format!("reading the response body: {e}"))
                    })?;
                    (status, payload)
                }
                Err(e) => {
                    if attempt < self.max_retries {
                        attempt += 1;
                        std::thread::sleep(self.retry_delay);
                        continue;
                    }
                    return Err(AgentError::Transport(e.to_string()));
                }
            };

            if (200..300).contains(&status) {
                return from_wire(&payload, &self.label);
            }
            if is_retryable(status) && attempt < self.max_retries {
                attempt += 1;
                std::thread::sleep(self.retry_delay);
                continue;
            }
            return Err(AgentError::Provider {
                provider: self.label.clone(),
                message: describe_error(status, &payload),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ToolSpec;

    fn request() -> ModelRequest {
        ModelRequest {
            model: "deepseek-chat".into(),
            system: "be careful".into(),
            messages: vec![Message::user(vec![ContentBlock::text("fix the test")])],
            tools: vec![ToolSpec {
                name: "exec".into(),
                description: "run a command".into(),
                input_schema: json!({ "type": "object", "properties": {} }),
            }],
            max_tokens: 4096,
        }
    }

    #[test]
    fn the_system_prompt_becomes_the_first_message() {
        let body = to_wire(&request(), TokenLimitField::MaxTokens);
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][0]["content"], "be careful");
        assert_eq!(body["messages"][1]["role"], "user");
        assert!(body.get("system").is_none(), "there is no top-level system field here");
    }

    #[test]
    fn tools_are_wrapped_in_a_function_envelope() {
        let body = to_wire(&request(), TokenLimitField::MaxTokens);
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["name"], "exec");
        assert_eq!(body["tools"][0]["function"]["parameters"]["type"], "object");
    }

    #[test]
    fn the_token_ceiling_field_is_selectable() {
        let a = to_wire(&request(), TokenLimitField::MaxTokens);
        assert_eq!(a["max_tokens"], 4096);
        assert!(a.get("max_completion_tokens").is_none());

        let b = to_wire(&request(), TokenLimitField::MaxCompletionTokens);
        assert_eq!(b["max_completion_tokens"], 4096);
        assert!(b.get("max_tokens").is_none());

        assert_eq!(TokenLimitField::parse("max_tokens"), Some(TokenLimitField::MaxTokens));
        assert_eq!(TokenLimitField::parse("nonsense"), None);
    }

    #[test]
    fn tool_arguments_go_over_the_wire_as_a_string() {
        let mut next = request();
        next.messages.push(Message::assistant(vec![ContentBlock::ToolUse {
            id: "call_1".into(),
            name: "fs".into(),
            input: json!({ "op": "read", "path": "a.txt" }),
        }]));
        let body = to_wire(&next, TokenLimitField::MaxTokens);

        let call = &body["messages"][2]["tool_calls"][0];
        assert_eq!(call["id"], "call_1");
        assert_eq!(call["type"], "function");
        assert_eq!(call["function"]["name"], "fs");

        let arguments = call["function"]["arguments"].as_str().expect("a string, not an object");
        let parsed: Value = serde_json::from_str(arguments).unwrap();
        assert_eq!(parsed["path"], "a.txt");
    }

    #[test]
    fn an_assistant_message_with_only_tool_calls_still_carries_a_content_field() {
        let mut next = request();
        next.messages.push(Message::assistant(vec![ContentBlock::ToolUse {
            id: "call_1".into(),
            name: "fs".into(),
            input: json!({}),
        }]));
        let body = to_wire(&next, TokenLimitField::MaxTokens);
        assert!(body["messages"][2].get("content").is_some());
        assert!(body["messages"][2]["content"].is_null());
    }

    #[test]
    fn each_tool_result_becomes_its_own_message() {
        let mut next = request();
        next.messages.push(Message::user(vec![
            ContentBlock::ToolResult {
                tool_use_id: "call_1".into(),
                content: "first".into(),
                is_error: false,
            },
            ContentBlock::ToolResult {
                tool_use_id: "call_2".into(),
                content: "second".into(),
                is_error: true,
            },
        ]));
        let body = to_wire(&next, TokenLimitField::MaxTokens);
        let messages = body["messages"].as_array().unwrap();

        assert_eq!(messages[2]["role"], "tool");
        assert_eq!(messages[2]["tool_call_id"], "call_1");
        assert_eq!(messages[3]["role"], "tool");
        assert_eq!(messages[3]["tool_call_id"], "call_2");
        assert_eq!(messages[3]["content"], "second");
    }

    /// The harness appends prose to the same turn when it rolls a turn back.
    /// The tool results must precede it, or the transcript is out of order.
    #[test]
    fn tool_results_precede_prose_added_in_the_same_turn() {
        let mut next = request();
        next.messages.push(Message::user(vec![
            ContentBlock::ToolResult {
                tool_use_id: "call_1".into(),
                content: "done".into(),
                is_error: false,
            },
            ContentBlock::text("That turn was rolled back."),
        ]));
        let messages = to_wire(&next, TokenLimitField::MaxTokens);
        let messages = messages["messages"].as_array().unwrap();

        assert_eq!(messages[2]["role"], "tool");
        assert_eq!(messages[3]["role"], "user");
        assert_eq!(messages[3]["content"], "That turn was rolled back.");
    }

    #[test]
    fn a_response_with_text_and_a_tool_call_is_parsed() {
        let body = json!({
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "Checking the suite.",
                    "tool_calls": [{
                        "id": "call_9",
                        "type": "function",
                        "function": { "name": "exec", "arguments": "{\"command\":\"pytest -q\"}" }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": { "prompt_tokens": 1200, "completion_tokens": 85 }
        });

        let parsed = from_wire(&serde_json::to_vec(&body).unwrap(), "deepseek").unwrap();
        assert_eq!(parsed.stop_reason, StopReason::ToolUse);
        assert_eq!(parsed.text(), "Checking the suite.");
        assert_eq!(parsed.usage, Usage { input_tokens: 1200, output_tokens: 85 });

        let uses = parsed.tool_uses();
        assert_eq!(uses[0].0, "call_9");
        assert_eq!(uses[0].1, "exec");
        assert_eq!(uses[0].2["command"], "pytest -q");
    }

    /// Models emit invalid JSON in `arguments` often enough that killing the
    /// run over it would be the wrong call. It goes through, and the tool
    /// tells the model what it needed.
    #[test]
    fn unparseable_tool_arguments_reach_the_tool_rather_than_ending_the_run() {
        let body = json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": { "name": "fs", "arguments": "{\"op\": \"read\", pat" }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        });

        let parsed = from_wire(&serde_json::to_vec(&body).unwrap(), "openai").unwrap();
        let uses = parsed.tool_uses();
        assert_eq!(uses.len(), 1);
        assert_eq!(uses[0].1, "fs");
        assert!(uses[0].2.is_null(), "the arguments could not be parsed, and that is reported");
    }

    #[test]
    fn every_finish_reason_is_understood() {
        for (raw, expected) in [
            ("stop", StopReason::EndTurn),
            ("tool_calls", StopReason::ToolUse),
            ("function_call", StopReason::ToolUse),
            ("length", StopReason::MaxTokens),
            ("content_filter", StopReason::Refusal),
        ] {
            assert_eq!(stop_reason_from(Some(raw)), expected);
        }
        assert_eq!(stop_reason_from(None), StopReason::EndTurn);
        assert_eq!(stop_reason_from(Some("novel")), StopReason::Other("novel".into()));
    }

    #[test]
    fn a_response_with_no_choices_is_an_error_rather_than_an_empty_turn() {
        let body = json!({ "choices": [] });
        let error = from_wire(&serde_json::to_vec(&body).unwrap(), "openai").unwrap_err();
        assert!(error.to_string().contains("no choices"), "{error}");
    }

    #[test]
    fn missing_usage_counts_as_zero() {
        let body = json!({
            "choices": [{ "message": { "role": "assistant", "content": "hi" }, "finish_reason": "stop" }]
        });
        let parsed = from_wire(&serde_json::to_vec(&body).unwrap(), "openai").unwrap();
        assert_eq!(parsed.usage, Usage::default());
        assert_eq!(parsed.text(), "hi");
    }

    #[test]
    fn base_urls_work_with_or_without_the_version_suffix() {
        let with = OpenAiProvider::new("k").with_base_url("https://api.deepseek.com/v1");
        assert_eq!(with.endpoint(), "https://api.deepseek.com/v1/chat/completions");

        let without = OpenAiProvider::new("k").with_base_url("https://api.deepseek.com");
        assert_eq!(without.endpoint(), "https://api.deepseek.com/v1/chat/completions");

        let local = OpenAiProvider::new("k").with_base_url("http://localhost:11434/");
        assert_eq!(local.endpoint(), "http://localhost:11434/v1/chat/completions");
    }

    #[test]
    fn an_error_body_is_reported_with_its_reason() {
        let body = json!({
            "error": { "message": "Insufficient Balance", "type": "insufficient_quota" }
        });
        let described = describe_error(402, &serde_json::to_vec(&body).unwrap());
        assert!(described.contains("402"));
        assert!(described.contains("Insufficient Balance"));
    }

    #[test]
    fn only_failures_a_repeated_request_could_survive_are_retried() {
        for status in [408, 429, 500, 503] {
            assert!(is_retryable(status));
        }
        for status in [400, 401, 402, 404, 422] {
            assert!(!is_retryable(status), "{status} should not be retried");
        }
    }
}
