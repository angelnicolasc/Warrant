//! The OpenAI-compatible transport against a real HTTP server.
//!
//! Same discipline as the Anthropic tests, against the other wire format —
//! the one DeepSeek, OpenAI, Groq, OpenRouter and every local server speak.
//! What is unverified is the same thing: a live vendor endpoint, for which
//! this build had no credentials.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::FakeApi;
use serde_json::{Value, json};
use warrant_agent::openai::{OpenAiProvider, TokenLimitField};
use warrant_agent::{ContentBlock, Message, ModelRequest, Provider, StopReason, ToolSpec};

fn provider(api: &FakeApi) -> OpenAiProvider {
    OpenAiProvider::new("sk-test")
        .with_base_url(api.base_url())
        .with_retry_delay(Duration::from_millis(1))
}

fn request() -> ModelRequest {
    ModelRequest {
        model: "deepseek-chat".into(),
        system: "be careful".into(),
        messages: vec![Message::user(vec![ContentBlock::text("fix the failing test")])],
        tools: vec![ToolSpec {
            name: "exec".into(),
            description: "run a command".into(),
            input_schema: json!({ "type": "object", "properties": {} }),
        }],
        max_tokens: 4096,
    }
}

fn reply(text: &str) -> Value {
    json!({
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": text },
            "finish_reason": "stop"
        }],
        "usage": { "prompt_tokens": 10, "completion_tokens": 4 }
    })
}

#[test]
fn a_request_goes_to_the_chat_completions_path_with_bearer_auth() {
    let api = FakeApi::start(vec![(200, reply("hello"))]);
    provider(&api).complete(&request()).unwrap();

    assert_eq!(api.header(0, "authorization"), "Bearer sk-test");
    assert!(api.header(0, "content-type").starts_with("application/json"));
}

#[test]
fn the_body_that_goes_over_the_wire_is_chat_completions_shaped() {
    let api = FakeApi::start(vec![(200, reply("hello"))]);
    provider(&api).complete(&request()).unwrap();

    let sent = &api.requests()[0];
    assert_eq!(sent["model"], "deepseek-chat");
    assert_eq!(sent["max_tokens"], 4096);
    // The system prompt is a message here, not a field.
    assert_eq!(sent["messages"][0]["role"], "system");
    assert_eq!(sent["messages"][0]["content"], "be careful");
    assert_eq!(sent["messages"][1]["role"], "user");
    assert_eq!(sent["tools"][0]["type"], "function");
    assert_eq!(sent["tools"][0]["function"]["name"], "exec");
}

#[test]
fn a_tool_call_comes_back_with_its_arguments_parsed() {
    let api = FakeApi::start(vec![(
        200,
        json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "checking",
                    "tool_calls": [{
                        "id": "call_9",
                        "type": "function",
                        "function": { "name": "exec", "arguments": "{\"command\":\"pytest -q\"}" }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": { "prompt_tokens": 900, "completion_tokens": 40 }
        }),
    )]);

    let response = provider(&api).complete(&request()).unwrap();
    assert_eq!(response.stop_reason, StopReason::ToolUse);
    assert_eq!(response.text(), "checking");
    assert_eq!(response.usage.total(), 940);

    let uses = response.tool_uses();
    assert_eq!(uses[0].0, "call_9");
    assert_eq!(uses[0].2["command"], "pytest -q");
}

#[test]
fn tool_results_travel_back_as_their_own_messages() {
    let api = FakeApi::start(vec![(200, reply("understood"))]);

    let mut next = request();
    next.messages.push(Message::assistant(vec![ContentBlock::ToolUse {
        id: "call_1".into(),
        name: "exec".into(),
        input: json!({ "command": "pytest" }),
    }]));
    next.messages.push(Message::user(vec![ContentBlock::ToolResult {
        tool_use_id: "call_1".into(),
        content: "exit 1".into(),
        is_error: true,
    }]));
    provider(&api).complete(&next).unwrap();

    let messages = &api.requests()[0]["messages"];
    assert_eq!(messages[2]["role"], "assistant");
    assert!(messages[2]["content"].is_null());
    assert_eq!(messages[2]["tool_calls"][0]["function"]["name"], "exec");
    // Arguments cross the wire as a string in this format.
    assert!(messages[2]["tool_calls"][0]["function"]["arguments"].is_string());

    assert_eq!(messages[3]["role"], "tool");
    assert_eq!(messages[3]["tool_call_id"], "call_1");
    assert_eq!(messages[3]["content"], "exit 1");
}

#[test]
fn the_token_ceiling_field_can_be_switched_for_newer_models() {
    let api = FakeApi::start(vec![(200, reply("hi"))]);
    provider(&api)
        .with_token_field(TokenLimitField::MaxCompletionTokens)
        .complete(&request())
        .unwrap();

    let sent = &api.requests()[0];
    assert_eq!(sent["max_completion_tokens"], 4096);
    assert!(sent.get("max_tokens").is_none());
}

#[test]
fn a_rate_limit_is_retried_with_the_identical_request() {
    let error = json!({ "error": { "message": "slow down", "type": "rate_limit_exceeded" } });
    let api = FakeApi::start(vec![(429, error.clone()), (429, error), (200, reply("finally"))]);

    assert_eq!(provider(&api).complete(&request()).unwrap().text(), "finally");
    assert_eq!(api.call_count(), 3);

    let sent = api.requests();
    assert_eq!(sent[0], sent[1], "retrying is only safe because the request is unchanged");
    assert_eq!(sent[1], sent[2]);
}

#[test]
fn a_request_the_server_has_already_judged_is_not_retried() {
    let api = FakeApi::start(vec![(
        402,
        json!({ "error": { "message": "Insufficient Balance", "type": "insufficient_quota" } }),
    )]);

    let error = provider(&api).complete(&request()).unwrap_err();
    assert_eq!(api.call_count(), 1);
    assert!(error.to_string().contains("Insufficient Balance"), "{error}");
}

/// The whole loop over the other wire format, including a model that emits
/// tool arguments which will not parse — which is routine on this transport,
/// and must not end the run.
#[test]
fn a_session_runs_end_to_end_and_survives_a_malformed_tool_call() {
    let call = |id: &str, name: &str, arguments: &str| {
        json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": id,
                        "type": "function",
                        "function": { "name": name, "arguments": arguments }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": { "prompt_tokens": 100, "completion_tokens": 10 }
        })
    };

    let api = FakeApi::start(vec![
        (
            200,
            call(
                "c1",
                "declare",
                "{\"assertion\":\"b.txt exists\",\"proof\":\"file_exists(b.txt)\"}",
            ),
        ),
        // Truncated JSON, exactly as a model emits it under pressure.
        (200, call("c2", "fs", "{\"op\": \"write\", \"pat")),
        (200, call("c3", "fs", "{\"op\":\"write\",\"path\":\"b.txt\",\"content\":\"created\"}")),
        (200, call("c4", "attest", "{}")),
        (200, reply("Done.")),
    ]);

    let root = tempfile::tempdir().unwrap();
    let cell_root = root.path().join("cell");
    std::fs::create_dir_all(&cell_root).unwrap();
    std::fs::write(cell_root.join("a.txt"), "before").unwrap();

    let store: Arc<dyn warrant_diff::ContentStore> = Arc::new(warrant_diff::MemoryStore::new());
    let scan = warrant_diff::ScanOptions {
        respect_gitignore: false,
        ..warrant_diff::ScanOptions::default()
    };
    let cell = warrant_cell::WorkspaceCell::adopt(&cell_root, Arc::clone(&store), scan).unwrap();
    let services = warrant_agent::Services::new(
        store,
        Arc::new(warrant_ledger::Ledger::open(root.path().join(".warrant")).unwrap()),
        Arc::new(warrant_attest::Attestor::new().unwrap()),
        warrant_agent::Policy::default(),
    );
    let workspace = warrant_agent::Workspace::new(Arc::new(Mutex::new(cell)), services).unwrap();

    let transport = provider(&api);
    let approver = warrant_agent::ApproveAll;
    let mut session = warrant_agent::Session::new(
        &transport,
        &approver,
        workspace,
        warrant_agent::SessionConfig::new("deepseek-chat", root.path().join("work")),
    );

    let outcome = session.run("create b.txt").unwrap();
    assert_eq!(outcome.stop, warrant_agent::StopCondition::Finished);
    assert!(outcome.all_warranted(), "{:?}", outcome.discharged);
    assert_eq!(api.call_count(), 5);
    assert_eq!(std::fs::read_to_string(cell_root.join("b.txt")).unwrap(), "created");

    // The model was told what the tool needed, and corrected itself.
    let told = session.transcript().iter().flat_map(|m| &m.content).any(|block| {
        matches!(block, ContentBlock::ToolResult { content, is_error, .. }
            if *is_error && content.contains("could not use those arguments"))
    });
    assert!(told, "a malformed call must come back to the model, not end the run");
}
