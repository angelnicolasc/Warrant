//! The Anthropic transport against a real HTTP server.
//!
//! Unit tests can check that a body serialises correctly. Only a socket can
//! check that the headers arrive, that a 429 is retried and a 400 is not, that
//! a response body is read rather than discarded on an error status, and that
//! a whole session drives through the transport unchanged.
//!
//! The server here is a real one on a real port. What remains unverified is
//! the *live* endpoint, which this build had no credentials for — that is
//! stated in the crate documentation rather than left to be discovered.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::FakeApi;

use serde_json::{Value, json};
use warrant_agent::anthropic::AnthropicProvider;
use warrant_agent::{ContentBlock, Message, ModelRequest, Provider, StopReason, ToolSpec};

fn provider(api: &FakeApi) -> AnthropicProvider {
    AnthropicProvider::new("test-key")
        .with_base_url(api.base_url())
        .with_retry_delay(Duration::from_millis(1))
}

fn request() -> ModelRequest {
    ModelRequest {
        model: "claude-opus-5".into(),
        system: "be careful".into(),
        messages: vec![Message::user(vec![ContentBlock::text("fix the failing test")])],
        tools: vec![ToolSpec {
            name: "exec".into(),
            description: "run a command".into(),
            input_schema: json!({ "type": "object", "properties": { "command": { "type": "string" } } }),
        }],
        max_tokens: 4096,
    }
}

fn ok_body(text: &str) -> Value {
    json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "content": [{ "type": "text", "text": text }],
        "stop_reason": "end_turn",
        "usage": { "input_tokens": 10, "output_tokens": 4 }
    })
}

#[test]
fn a_request_arrives_with_the_headers_the_api_requires() {
    let api = FakeApi::start(vec![(200, ok_body("hello"))]);
    provider(&api).complete(&request()).unwrap();

    let headers = api.headers(0);
    let get = |name: &str| {
        headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone()).unwrap_or_default()
    };
    assert_eq!(get("x-api-key"), "test-key");
    assert_eq!(get("anthropic-version"), "2023-06-01");
    assert!(get("content-type").starts_with("application/json"));
}

#[test]
fn the_body_that_goes_over_the_wire_is_the_documented_shape() {
    let api = FakeApi::start(vec![(200, ok_body("hello"))]);
    provider(&api).complete(&request()).unwrap();

    let sent = &api.requests()[0];
    assert_eq!(sent["model"], "claude-opus-5");
    assert_eq!(sent["max_tokens"], 4096);
    assert_eq!(sent["system"], "be careful");
    assert_eq!(sent["messages"][0]["role"], "user");
    assert_eq!(sent["messages"][0]["content"][0]["type"], "text");
    assert_eq!(sent["tools"][0]["name"], "exec");
    assert_eq!(sent["tools"][0]["input_schema"]["type"], "object");
}

#[test]
fn a_tool_call_comes_back_ready_to_dispatch() {
    let api = FakeApi::start(vec![(
        200,
        json!({
            "content": [
                { "type": "text", "text": "checking" },
                { "type": "tool_use", "id": "toolu_1", "name": "exec", "input": { "command": "pytest -q" } }
            ],
            "stop_reason": "tool_use",
            "usage": { "input_tokens": 900, "output_tokens": 40 }
        }),
    )]);

    let response = provider(&api).complete(&request()).unwrap();
    assert_eq!(response.stop_reason, StopReason::ToolUse);
    assert!(response.wants_tools());

    let uses = response.tool_uses();
    assert_eq!(uses[0].1, "exec");
    assert_eq!(uses[0].2["command"], "pytest -q");
    assert_eq!(response.usage.total(), 940);
}

#[test]
fn tool_results_travel_back_in_the_next_request() {
    let api = FakeApi::start(vec![(200, ok_body("understood"))]);

    let mut next = request();
    next.messages.push(Message::assistant(vec![ContentBlock::ToolUse {
        id: "toolu_1".into(),
        name: "exec".into(),
        input: json!({ "command": "pytest" }),
    }]));
    next.messages.push(Message::user(vec![ContentBlock::ToolResult {
        tool_use_id: "toolu_1".into(),
        content: "exit 1".into(),
        is_error: true,
    }]));
    provider(&api).complete(&next).unwrap();

    let sent = &api.requests()[0];
    assert_eq!(sent["messages"][1]["content"][0]["type"], "tool_use");
    assert_eq!(sent["messages"][2]["content"][0]["type"], "tool_result");
    assert_eq!(sent["messages"][2]["content"][0]["tool_use_id"], "toolu_1");
    assert_eq!(sent["messages"][2]["content"][0]["is_error"], true);
    assert_eq!(sent["messages"][2]["content"][0]["content"], "exit 1");
}

#[test]
fn a_rate_limit_is_retried_with_the_identical_request() {
    let api = FakeApi::start(vec![
        (429, json!({ "error": { "type": "rate_limit_error", "message": "slow down" } })),
        (429, json!({ "error": { "type": "rate_limit_error", "message": "slow down" } })),
        (200, ok_body("finally")),
    ]);

    let response = provider(&api).complete(&request()).unwrap();
    assert_eq!(response.text(), "finally");
    assert_eq!(api.call_count(), 3);

    // Retrying is only safe because the request is unchanged.
    let sent = api.requests();
    assert_eq!(sent[0], sent[1]);
    assert_eq!(sent[1], sent[2]);
}

#[test]
fn a_request_the_server_has_already_judged_is_not_retried() {
    let api = FakeApi::start(vec![(
        400,
        json!({ "error": { "type": "invalid_request_error", "message": "max_tokens is too large" } }),
    )]);

    let error = provider(&api).complete(&request()).unwrap_err();
    assert_eq!(api.call_count(), 1, "a 400 must not be repeated");
    assert!(error.to_string().contains("invalid_request_error"), "{error}");
    assert!(error.to_string().contains("max_tokens is too large"));
}

#[test]
fn retries_are_bounded_and_the_last_failure_is_reported() {
    let failing =
        (503, json!({ "error": { "type": "overloaded_error", "message": "overloaded" } }));
    let api = FakeApi::start(vec![failing.clone(), failing.clone(), failing.clone(), failing]);

    let error = provider(&api).with_max_retries(2).complete(&request()).unwrap_err();
    assert_eq!(api.call_count(), 3, "one attempt plus two retries");
    assert!(error.to_string().contains("overloaded"), "{error}");
}

#[test]
fn an_error_body_is_read_rather_than_lost_to_the_status_code() {
    let api = FakeApi::start(vec![(
        401,
        json!({ "error": { "type": "authentication_error", "message": "invalid x-api-key" } }),
    )]);
    let error = provider(&api).complete(&request()).unwrap_err();
    assert!(error.to_string().contains("invalid x-api-key"), "{error}");
}

/// The whole loop, driven through the real transport rather than around it.
#[test]
fn a_session_runs_end_to_end_over_http() {
    let api = FakeApi::start(vec![
        (
            200,
            json!({
                "content": [{
                    "type": "tool_use", "id": "t1", "name": "declare",
                    "input": { "assertion": "b.txt will exist", "proof": "file_exists(b.txt)" }
                }],
                "stop_reason": "tool_use",
                "usage": { "input_tokens": 800, "output_tokens": 30 }
            }),
        ),
        (
            200,
            json!({
                "content": [{
                    "type": "tool_use", "id": "t2", "name": "fs",
                    "input": { "op": "write", "path": "b.txt", "content": "created" }
                }],
                "stop_reason": "tool_use",
                "usage": { "input_tokens": 850, "output_tokens": 25 }
            }),
        ),
        (
            200,
            json!({
                "content": [{ "type": "tool_use", "id": "t3", "name": "attest", "input": {} }],
                "stop_reason": "tool_use",
                "usage": { "input_tokens": 900, "output_tokens": 12 }
            }),
        ),
        (200, ok_body("Done.")),
    ]);

    let root = tempfile::tempdir().unwrap();
    let cell_root = root.path().join("cell");
    std::fs::create_dir_all(cell_root.join("src")).unwrap();
    std::fs::write(cell_root.join("src").join("a.txt"), "before").unwrap();

    let store: Arc<dyn warrant_diff::ContentStore> = Arc::new(warrant_diff::MemoryStore::new());
    let scan = warrant_diff::ScanOptions {
        respect_gitignore: false,
        ..warrant_diff::ScanOptions::default()
    };
    let cell = warrant_cell::WorkspaceCell::adopt(&cell_root, Arc::clone(&store), scan).unwrap();
    let ledger = Arc::new(warrant_ledger::Ledger::open(root.path().join(".warrant")).unwrap());
    let attestor = Arc::new(warrant_attest::Attestor::new().unwrap());
    let workspace = warrant_agent::Workspace::new(
        Arc::new(Mutex::new(cell)),
        warrant_agent::Services::new(store, ledger, attestor, warrant_agent::Policy::default()),
    )
    .unwrap();

    let provider = provider(&api);
    let approver = warrant_agent::ApproveAll;
    let mut session = warrant_agent::Session::new(
        &provider,
        &approver,
        workspace,
        warrant_agent::SessionConfig::new("claude-opus-5", root.path().join("work")),
    );

    let outcome = session.run("create b.txt").unwrap();
    assert_eq!(outcome.stop, warrant_agent::StopCondition::Finished);
    assert!(outcome.all_warranted(), "{:?}", outcome.discharged);
    assert_eq!(outcome.usage.total(), 800 + 30 + 850 + 25 + 900 + 12 + 10 + 4);
    assert_eq!(api.call_count(), 4);
    assert_eq!(std::fs::read_to_string(cell_root.join("b.txt")).unwrap(), "created");
}
