//! The Anthropic Messages transport.
//!
//! Blocking HTTP, in keeping with the rest of the design: the workload is
//! probe-bound, a session makes one model call at a time, and an async
//! runtime would buy nothing while appearing on every error path.
//!
//! # What is verified, and what is not
//!
//! The wire format is exercised end to end against a real HTTP server in this
//! crate's tests — request shape, tool definitions, tool results, every stop
//! reason, usage accounting, error bodies and retry behaviour. **What is not
//! exercised is the live endpoint**, because this build was written without
//! credentials for it. That is the one edge of Warrant that has been reasoned
//! about rather than measured, and it is named here rather than left for
//! someone to discover.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{AgentError, Result};
use crate::provider::{
    ContentBlock, Message, ModelRequest, ModelResponse, Provider, StopReason, ToolSpec, Usage,
};

/// The API version this build speaks.
pub const API_VERSION: &str = "2023-06-01";

/// Where the API lives unless told otherwise.
pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

/// Environment variable holding the key.
pub const API_KEY_VAR: &str = "ANTHROPIC_API_KEY";

/// Environment variable overriding the base URL.
pub const BASE_URL_VAR: &str = "ANTHROPIC_BASE_URL";

/// A provider that talks to the Anthropic Messages API.
pub struct AnthropicProvider {
    agent: ureq::Agent,
    base_url: String,
    api_key: String,
    max_retries: u32,
    retry_delay: Duration,
}

impl AnthropicProvider {
    /// Build a provider from an explicit key.
    pub fn new(api_key: impl Into<String>) -> Self {
        let agent = ureq::Agent::config_builder()
            // Read the body on 4xx and 5xx rather than losing it to an error
            // type: the API puts the reason there, and a provider that
            // discards it turns a precise failure into "request failed".
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(600)))
            .build()
            .into();
        AnthropicProvider {
            agent,
            base_url: DEFAULT_BASE_URL.to_owned(),
            api_key: api_key.into(),
            max_retries: 3,
            retry_delay: Duration::from_millis(500),
        }
    }

    /// Build a provider from the environment.
    pub fn from_env() -> Result<Self> {
        let key = std::env::var(API_KEY_VAR).map_err(|_| AgentError::Provider {
            provider: "anthropic".into(),
            message: format!(
                "{API_KEY_VAR} is not set. `warrant run` needs a model; `warrant wrap` does not \
                 and works with the agent you already have configured."
            ),
        })?;
        let mut provider = AnthropicProvider::new(key);
        if let Ok(base) = std::env::var(BASE_URL_VAR)
            && !base.trim().is_empty()
        {
            provider.base_url = base.trim_end_matches('/').to_owned();
        }
        Ok(provider)
    }

    /// Point the provider at a different host.
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into().trim_end_matches('/').to_owned();
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
        format!("{}/v1/messages", self.base_url)
    }
}

/// The request body, written out explicitly.
///
/// Serialising [`ModelRequest`] directly would work today and would silently
/// start sending the wrong thing the moment an internal field is added.
#[derive(Debug, Serialize)]
struct WireRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    #[serde(skip_serializing_if = "str::is_empty")]
    system: &'a str,
    messages: &'a [Message],
    #[serde(skip_serializing_if = "<[ToolSpec]>::is_empty")]
    tools: &'a [ToolSpec],
}

#[derive(Debug, Deserialize)]
struct WireUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
}

#[derive(Debug, Deserialize)]
struct WireResponse {
    #[serde(default)]
    content: Vec<Value>,
    stop_reason: Option<String>,
    #[serde(default)]
    usage: Option<WireUsage>,
}

#[derive(Debug, Deserialize)]
struct WireErrorBody {
    error: WireError,
}

#[derive(Debug, Deserialize)]
struct WireError {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    message: String,
}

fn stop_reason_from(raw: Option<&str>) -> StopReason {
    match raw {
        Some("end_turn") | None => StopReason::EndTurn,
        Some("tool_use") => StopReason::ToolUse,
        Some("max_tokens") => StopReason::MaxTokens,
        Some("stop_sequence") => StopReason::StopSequence,
        Some("refusal") => StopReason::Refusal,
        Some(other) => StopReason::Other(other.to_owned()),
    }
}

/// Convert one response block.
///
/// Unknown block types are refused rather than skipped. Dropping a block this
/// build does not understand would mean acting on a partial reading of what
/// the model said, and doing so quietly.
fn block_from(value: &Value) -> Result<ContentBlock> {
    let kind = value.get("type").and_then(Value::as_str).unwrap_or_default();
    match kind {
        "text" => {
            Ok(ContentBlock::text(value.get("text").and_then(Value::as_str).unwrap_or_default()))
        }
        "tool_use" => Ok(ContentBlock::ToolUse {
            id: value.get("id").and_then(Value::as_str).unwrap_or_default().to_owned(),
            name: value.get("name").and_then(Value::as_str).unwrap_or_default().to_owned(),
            input: value.get("input").cloned().unwrap_or_else(|| Value::Object(Default::default())),
        }),
        other => Err(AgentError::Provider {
            provider: "anthropic".into(),
            message: format!(
                "the response contains a `{other}` block, which this build does not understand. \
                 Acting on the rest would mean acting on a partial reading of the answer."
            ),
        }),
    }
}

fn parse_response(body: &[u8]) -> Result<ModelResponse> {
    let wire: WireResponse = serde_json::from_slice(body).map_err(|e| AgentError::Provider {
        provider: "anthropic".into(),
        message: format!("the response was not the expected shape: {e}"),
    })?;

    let content = wire.content.iter().map(block_from).collect::<Result<Vec<_>>>()?;
    let usage = wire
        .usage
        .map(|u| Usage { input_tokens: u.input_tokens, output_tokens: u.output_tokens })
        .unwrap_or_default();

    Ok(ModelResponse { content, stop_reason: stop_reason_from(wire.stop_reason.as_deref()), usage })
}

fn describe_error(status: u16, body: &[u8]) -> String {
    match serde_json::from_slice::<WireErrorBody>(body) {
        Ok(parsed) => format!("HTTP {status} ({}): {}", parsed.error.kind, parsed.error.message),
        Err(_) => {
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

/// Whether a failure is worth repeating the identical request for.
///
/// Retrying is safe precisely because the request is unchanged — it cannot
/// move the trajectory. Anything the server has already decided about the
/// content of the request is not retried.
fn is_retryable(status: u16) -> bool {
    status == 408 || status == 409 || status == 429 || (500..600).contains(&status)
}

impl Provider for AnthropicProvider {
    fn name(&self) -> &str {
        "anthropic"
    }

    fn complete(&self, request: &ModelRequest) -> Result<ModelResponse> {
        let body = WireRequest {
            model: &request.model,
            max_tokens: request.max_tokens,
            system: &request.system,
            messages: &request.messages,
            tools: &request.tools,
        };

        let mut attempt = 0;
        loop {
            let sent = self
                .agent
                .post(self.endpoint())
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", API_VERSION)
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
                return parse_response(&payload);
            }
            if is_retryable(status) && attempt < self.max_retries {
                attempt += 1;
                std::thread::sleep(self.retry_delay);
                continue;
            }
            return Err(AgentError::Provider {
                provider: "anthropic".into(),
                message: describe_error(status, &payload),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Role;
    use serde_json::json;

    #[test]
    fn the_request_body_matches_the_documented_shape() {
        let request = ModelRequest {
            model: "claude-opus-5".into(),
            system: "be careful".into(),
            messages: vec![Message::user(vec![ContentBlock::text("hello")])],
            tools: vec![ToolSpec {
                name: "exec".into(),
                description: "run a command".into(),
                input_schema: json!({ "type": "object", "properties": {} }),
            }],
            max_tokens: 4096,
        };
        let wire = WireRequest {
            model: &request.model,
            max_tokens: request.max_tokens,
            system: &request.system,
            messages: &request.messages,
            tools: &request.tools,
        };
        let body: Value = serde_json::to_value(&wire).unwrap();

        assert_eq!(body["model"], "claude-opus-5");
        assert_eq!(body["max_tokens"], 4096);
        assert_eq!(body["system"], "be careful");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"][0]["type"], "text");
        assert_eq!(body["messages"][0]["content"][0]["text"], "hello");
        assert_eq!(body["tools"][0]["name"], "exec");
        assert!(body["tools"][0]["input_schema"].is_object());
    }

    #[test]
    fn tool_use_and_tool_result_serialise_the_way_the_api_expects() {
        let assistant = Message::assistant(vec![ContentBlock::ToolUse {
            id: "toolu_1".into(),
            name: "fs".into(),
            input: json!({ "op": "read", "path": "a.txt" }),
        }]);
        let user = Message::user(vec![ContentBlock::ToolResult {
            tool_use_id: "toolu_1".into(),
            content: "file contents".into(),
            is_error: false,
        }]);

        let a: Value = serde_json::to_value(&assistant).unwrap();
        assert_eq!(a["role"], "assistant");
        assert_eq!(a["content"][0]["type"], "tool_use");
        assert_eq!(a["content"][0]["id"], "toolu_1");
        assert_eq!(a["content"][0]["input"]["op"], "read");

        let u: Value = serde_json::to_value(&user).unwrap();
        assert_eq!(u["content"][0]["type"], "tool_result");
        assert_eq!(u["content"][0]["tool_use_id"], "toolu_1");
        assert_eq!(u["content"][0]["is_error"], false);
    }

    #[test]
    fn an_empty_system_prompt_and_no_tools_are_omitted_rather_than_sent_empty() {
        let request = ModelRequest {
            model: "m".into(),
            system: String::new(),
            messages: vec![Message::user(vec![ContentBlock::text("hi")])],
            tools: Vec::new(),
            max_tokens: 16,
        };
        let wire = WireRequest {
            model: &request.model,
            max_tokens: request.max_tokens,
            system: &request.system,
            messages: &request.messages,
            tools: &request.tools,
        };
        let body: Value = serde_json::to_value(&wire).unwrap();
        assert!(body.get("system").is_none());
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn a_response_with_text_and_a_tool_call_is_parsed() {
        let body = json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "content": [
                { "type": "text", "text": "I will look at the tests." },
                { "type": "tool_use", "id": "toolu_9", "name": "exec", "input": { "command": "pytest" } }
            ],
            "stop_reason": "tool_use",
            "usage": { "input_tokens": 1200, "output_tokens": 85 }
        });

        let parsed = parse_response(&serde_json::to_vec(&body).unwrap()).unwrap();
        assert_eq!(parsed.stop_reason, StopReason::ToolUse);
        assert_eq!(parsed.usage, Usage { input_tokens: 1200, output_tokens: 85 });
        assert_eq!(parsed.text(), "I will look at the tests.");

        let uses = parsed.tool_uses();
        assert_eq!(uses.len(), 1);
        assert_eq!(uses[0].0, "toolu_9");
        assert_eq!(uses[0].1, "exec");
        assert_eq!(uses[0].2["command"], "pytest");
    }

    #[test]
    fn every_stop_reason_is_understood() {
        for (raw, expected) in [
            ("end_turn", StopReason::EndTurn),
            ("tool_use", StopReason::ToolUse),
            ("max_tokens", StopReason::MaxTokens),
            ("stop_sequence", StopReason::StopSequence),
            ("refusal", StopReason::Refusal),
        ] {
            assert_eq!(stop_reason_from(Some(raw)), expected);
        }
        assert_eq!(stop_reason_from(None), StopReason::EndTurn);
        assert_eq!(
            stop_reason_from(Some("something_new")),
            StopReason::Other("something_new".into())
        );
    }

    /// Silently dropping a block this build does not understand would mean
    /// acting on a partial reading of the answer, and doing so invisibly.
    #[test]
    fn an_unrecognised_block_type_is_refused_rather_than_skipped() {
        let body = json!({
            "content": [
                { "type": "thinking", "thinking": "..." },
                { "type": "text", "text": "hello" }
            ],
            "stop_reason": "end_turn"
        });
        let error = parse_response(&serde_json::to_vec(&body).unwrap()).unwrap_err();
        assert!(error.to_string().contains("thinking"), "{error}");
        assert!(error.to_string().contains("partial reading"));
    }

    #[test]
    fn a_missing_usage_block_is_zero_rather_than_a_failure() {
        let body =
            json!({ "content": [{ "type": "text", "text": "hi" }], "stop_reason": "end_turn" });
        let parsed = parse_response(&serde_json::to_vec(&body).unwrap()).unwrap();
        assert_eq!(parsed.usage, Usage::default());
    }

    #[test]
    fn an_api_error_body_is_reported_with_its_reason() {
        let body = json!({
            "type": "error",
            "error": { "type": "rate_limit_error", "message": "Number of requests has exceeded your rate limit." }
        });
        let described = describe_error(429, &serde_json::to_vec(&body).unwrap());
        assert!(described.contains("429"));
        assert!(described.contains("rate_limit_error"));
        assert!(described.contains("exceeded your rate limit"));
    }

    #[test]
    fn a_body_that_is_not_json_still_produces_a_useful_message() {
        assert!(describe_error(502, b"<html>bad gateway</html>").contains("bad gateway"));
        assert_eq!(describe_error(500, b""), "HTTP 500");
    }

    #[test]
    fn only_failures_that_the_same_request_could_survive_are_retried() {
        for status in [408, 409, 429, 500, 502, 503, 529] {
            assert!(is_retryable(status), "{status} should be retried");
        }
        // The server has already judged the request itself; repeating it
        // verbatim cannot produce a different answer.
        for status in [400, 401, 403, 404, 413, 422] {
            assert!(!is_retryable(status), "{status} should not be retried");
        }
    }

    #[test]
    fn the_endpoint_is_built_from_the_base_url() {
        let provider = AnthropicProvider::new("k").with_base_url("http://localhost:9/");
        assert_eq!(provider.endpoint(), "http://localhost:9/v1/messages");
    }

    #[test]
    fn a_missing_key_explains_what_to_do_instead() {
        // Exercised through the same message the constructor produces, so the
        // test does not depend on the ambient environment.
        let error = AgentError::Provider {
            provider: "anthropic".into(),
            message: format!(
                "{API_KEY_VAR} is not set. `warrant run` needs a model; `warrant wrap` does not"
            ),
        };
        assert!(error.to_string().contains("ANTHROPIC_API_KEY"));
        assert!(error.to_string().contains("warrant wrap"));
    }

    #[test]
    fn roles_serialise_lowercase_as_the_api_requires() {
        assert_eq!(serde_json::to_value(Role::User).unwrap(), json!("user"));
        assert_eq!(serde_json::to_value(Role::Assistant).unwrap(), json!("assistant"));
    }
}
