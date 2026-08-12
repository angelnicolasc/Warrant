//! Talking to a model, and replaying what it said.
//!
//! The loop is deliberately dumb — the model decides, the harness executes —
//! so this layer is a transport and nothing more. It carries no retry policy
//! that could silently change a trajectory, no prompt rewriting, and no
//! summarisation.
//!
//! # Replay is checked, not assumed
//!
//! Forking a live trajectory and substituting a different model rewrites
//! **61–94% of subsequent actions** (*The Replay Gap*, arXiv 2608.08239). The
//! damaging version of that finding is not the number, it is what it implies
//! about naive replay: feeding recorded outputs back into a run that has
//! drifted scores a world that never existed.
//!
//! [`ReplayProvider`] therefore refuses to do it. Each recorded turn carries
//! the digest of the request it answered, and replay fails loudly the moment
//! the incoming request stops matching. A divergent replay is an error, not a
//! slightly-wrong result.
//!
//! Checked replay needs the *environment* to be reproducible too, since tool
//! results are part of the next request. Anything a tool reports that varies
//! between identical runs — a wall-clock duration, a temporary path, a
//! generated identifier — will surface here as a divergence rather than
//! passing silently. That is the intended direction of failure, and it is why
//! nothing derived from the clock is written into a tool result.

use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use warrant_core::Hash;

use crate::error::{AgentError, Result};

/// A tool as described to the model.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSpec {
    /// Name the model calls it by.
    pub name: String,
    /// What it does, and what it refuses to do.
    pub description: String,
    /// JSON Schema for the arguments.
    pub input_schema: serde_json::Value,
}

/// Who said something.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// The harness.
    User,
    /// The model.
    Assistant,
}

/// One piece of a message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// Prose.
    Text {
        /// The text.
        text: String,
    },
    /// The model asking for a tool to run.
    ToolUse {
        /// Correlates with the result.
        id: String,
        /// Which tool.
        name: String,
        /// Arguments.
        input: serde_json::Value,
    },
    /// The harness reporting what a tool did.
    ToolResult {
        /// The id of the call this answers.
        tool_use_id: String,
        /// What to tell the model. Large artefacts arrive as handles.
        content: String,
        /// Whether the tool failed.
        is_error: bool,
    },
}

impl ContentBlock {
    /// Prose helper.
    pub fn text(text: impl Into<String>) -> Self {
        ContentBlock::Text { text: text.into() }
    }
}

/// One turn of conversation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    /// Who is speaking.
    pub role: Role,
    /// What they said.
    pub content: Vec<ContentBlock>,
}

impl Message {
    /// A message from the harness.
    pub fn user(content: Vec<ContentBlock>) -> Self {
        Message { role: Role::User, content }
    }

    /// A message from the model.
    pub fn assistant(content: Vec<ContentBlock>) -> Self {
        Message { role: Role::Assistant, content }
    }
}

/// What is sent to a model.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRequest {
    /// Which model.
    pub model: String,
    /// Standing instructions.
    pub system: String,
    /// The conversation so far.
    pub messages: Vec<Message>,
    /// Tools the model may call.
    pub tools: Vec<ToolSpec>,
    /// Ceiling on the response.
    pub max_tokens: u32,
}

impl ModelRequest {
    /// A stable address for this request.
    ///
    /// This is what makes replay honest: a recorded answer is only valid for
    /// the exact question it answered.
    pub fn digest(&self) -> Hash {
        let encoded = serde_json::to_vec(self).expect("a request always serialises");
        Hash::of_tagged("warrant.model-request.v1", &[&encoded])
    }
}

/// Why the model stopped.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// It finished its turn.
    EndTurn,
    /// It wants a tool run.
    ToolUse,
    /// It hit the output ceiling.
    MaxTokens,
    /// It hit a stop sequence.
    StopSequence,
    /// It declined.
    Refusal,
    /// Something this build does not know about.
    Other(String),
}

/// Token accounting.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Tokens in.
    pub input_tokens: u64,
    /// Tokens out.
    pub output_tokens: u64,
}

impl Usage {
    /// Total tokens.
    pub fn total(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }

    /// Accumulate another turn's usage.
    pub fn add(&mut self, other: Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
    }
}

/// What a model said.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelResponse {
    /// The blocks it produced.
    pub content: Vec<ContentBlock>,
    /// Why it stopped.
    pub stop_reason: StopReason,
    /// What it cost.
    pub usage: Usage,
}

impl ModelResponse {
    /// A plain text response that asks for nothing.
    pub fn saying(text: impl Into<String>) -> Self {
        ModelResponse {
            content: vec![ContentBlock::text(text)],
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        }
    }

    /// A response requesting one tool call.
    pub fn calling(id: &str, name: &str, input: serde_json::Value) -> Self {
        ModelResponse {
            content: vec![ContentBlock::ToolUse {
                id: id.to_owned(),
                name: name.to_owned(),
                input,
            }],
            stop_reason: StopReason::ToolUse,
            usage: Usage::default(),
        }
    }

    /// Every tool call the model asked for, in order.
    pub fn tool_uses(&self) -> Vec<(&str, &str, &serde_json::Value)> {
        self.content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolUse { id, name, input } => {
                    Some((id.as_str(), name.as_str(), input))
                }
                _ => None,
            })
            .collect()
    }

    /// The prose the model produced, joined.
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Whether the model wants to keep working.
    pub fn wants_tools(&self) -> bool {
        !self.tool_uses().is_empty()
    }
}

/// One recorded exchange, as stored in the ledger.
///
/// The request digest travels with the answer, which is the whole basis of
/// checked replay.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordedTurn {
    /// Address of the request that was asked.
    pub request: Hash,
    /// What came back.
    pub response: ModelResponse,
}

/// Every exchange a ledger recorded, in order.
///
/// This is what `warrant bisect` and `warrant freeze` are built on: a run is
/// replayable from the record alone, with no reference to the process that
/// produced it.
pub fn recorded_turns(ledger: &warrant_ledger::Ledger) -> Result<Vec<RecordedTurn>> {
    let mut turns = Vec::new();
    for entry in ledger.entries()? {
        if entry.kind == warrant_ledger::EntryKind::ModelResponse {
            turns.push(ledger.payload_json::<RecordedTurn>(&entry)?);
        }
    }
    Ok(turns)
}

/// Somewhere model responses come from.
pub trait Provider: Send + Sync {
    /// Identifies the provider in the record.
    fn name(&self) -> &str;

    /// Ask for the next response.
    fn complete(&self, request: &ModelRequest) -> Result<ModelResponse>;
}

/// A provider whose answers are decided in advance.
///
/// Used for tests, and for exercising the loop's control flow without a
/// network or a bill.
#[derive(Debug)]
pub struct ScriptedProvider {
    responses: Mutex<std::collections::VecDeque<ModelResponse>>,
    seen: Mutex<Vec<ModelRequest>>,
}

impl ScriptedProvider {
    /// Queue a sequence of responses.
    pub fn new(responses: impl IntoIterator<Item = ModelResponse>) -> Self {
        ScriptedProvider {
            responses: Mutex::new(responses.into_iter().collect()),
            seen: Mutex::new(Vec::new()),
        }
    }

    /// Every request the loop made, in order.
    pub fn requests(&self) -> Vec<ModelRequest> {
        self.seen.lock().expect("scripted provider poisoned").clone()
    }

    /// How many responses are left unused.
    pub fn remaining(&self) -> usize {
        self.responses.lock().expect("scripted provider poisoned").len()
    }
}

impl Provider for ScriptedProvider {
    fn name(&self) -> &str {
        "scripted"
    }

    fn complete(&self, request: &ModelRequest) -> Result<ModelResponse> {
        self.seen.lock().expect("scripted provider poisoned").push(request.clone());
        self.responses
            .lock()
            .expect("scripted provider poisoned")
            .pop_front()
            .ok_or(AgentError::ScriptExhausted)
    }
}

/// A provider that replays a recorded run, and refuses to replay a different one.
pub struct ReplayProvider {
    turns: Vec<RecordedTurn>,
    cursor: Mutex<usize>,
    strict: bool,
}

impl ReplayProvider {
    /// Replay these turns, failing if the run diverges from them.
    pub fn new(turns: Vec<RecordedTurn>) -> Self {
        ReplayProvider { turns, cursor: Mutex::new(0), strict: true }
    }

    /// Replay without checking that requests match.
    ///
    /// Only honest for inspecting what a model said, never for re-scoring a
    /// run: the recorded answers stop being answers to the questions being
    /// asked. `warrant bisect` uses the checked form.
    pub fn unchecked(turns: Vec<RecordedTurn>) -> Self {
        ReplayProvider { turns, cursor: Mutex::new(0), strict: false }
    }

    /// How many turns were recorded.
    pub fn len(&self) -> usize {
        self.turns.len()
    }

    /// Whether anything was recorded.
    pub fn is_empty(&self) -> bool {
        self.turns.is_empty()
    }

    /// How many have been replayed so far.
    pub fn position(&self) -> usize {
        *self.cursor.lock().expect("replay provider poisoned")
    }
}

impl Provider for ReplayProvider {
    fn name(&self) -> &str {
        "replay"
    }

    fn complete(&self, request: &ModelRequest) -> Result<ModelResponse> {
        let mut cursor = self.cursor.lock().expect("replay provider poisoned");
        let turn = *cursor;
        let recorded = self.turns.get(turn).ok_or(AgentError::ReplayExhausted { turn })?;

        if self.strict {
            let asked = request.digest();
            if asked != recorded.request {
                return Err(AgentError::ReplayDiverged { turn, recorded: recorded.request, asked });
            }
        }

        *cursor += 1;
        Ok(recorded.response.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(system: &str) -> ModelRequest {
        ModelRequest {
            model: "claude-opus-5".into(),
            system: system.into(),
            messages: vec![Message::user(vec![ContentBlock::text("do the thing")])],
            tools: Vec::new(),
            max_tokens: 4096,
        }
    }

    #[test]
    fn a_request_digest_covers_everything_that_changes_the_answer() {
        let base = request("be careful");
        assert_eq!(base.digest(), request("be careful").digest());
        assert_ne!(base.digest(), request("be quick").digest());

        let mut other = base.clone();
        other.model = "claude-sonnet-5".into();
        assert_ne!(base.digest(), other.digest());

        let mut more = base.clone();
        more.messages.push(Message::assistant(vec![ContentBlock::text("ok")]));
        assert_ne!(base.digest(), more.digest());
    }

    #[test]
    fn tool_uses_are_extracted_in_order() {
        let response = ModelResponse {
            content: vec![
                ContentBlock::text("I will look first"),
                ContentBlock::ToolUse {
                    id: "a".into(),
                    name: "fs".into(),
                    input: serde_json::json!({"op": "read"}),
                },
                ContentBlock::ToolUse {
                    id: "b".into(),
                    name: "exec".into(),
                    input: serde_json::json!({"command": "pytest"}),
                },
            ],
            stop_reason: StopReason::ToolUse,
            usage: Usage::default(),
        };

        let uses = response.tool_uses();
        assert_eq!(uses.len(), 2);
        assert_eq!(uses[0].1, "fs");
        assert_eq!(uses[1].1, "exec");
        assert_eq!(response.text(), "I will look first");
        assert!(response.wants_tools());
    }

    #[test]
    fn a_scripted_provider_answers_in_order_and_records_what_it_was_asked() {
        let provider = ScriptedProvider::new([
            ModelResponse::calling("1", "exec", serde_json::json!({"command": "pytest"})),
            ModelResponse::saying("done"),
        ]);

        assert!(provider.complete(&request("a")).unwrap().wants_tools());
        assert_eq!(provider.complete(&request("b")).unwrap().text(), "done");
        assert!(matches!(provider.complete(&request("c")), Err(AgentError::ScriptExhausted)));

        let seen = provider.requests();
        assert_eq!(seen.len(), 3);
        assert_eq!(seen[0].system, "a");
    }

    #[test]
    fn replay_returns_exactly_what_was_recorded() {
        let asked = request("original");
        let turns = vec![RecordedTurn {
            request: asked.digest(),
            response: ModelResponse::saying("recorded answer"),
        }];

        let provider = ReplayProvider::new(turns);
        assert_eq!(provider.complete(&asked).unwrap().text(), "recorded answer");
        assert_eq!(provider.position(), 1);
    }

    /// The finding this design exists for: recorded answers stop being answers
    /// once the question changes, and a replay that carried on regardless
    /// would be scoring a world that never existed.
    #[test]
    fn replay_refuses_to_answer_a_question_it_was_not_asked() {
        let turns = vec![RecordedTurn {
            request: request("original").digest(),
            response: ModelResponse::saying("recorded answer"),
        }];

        let provider = ReplayProvider::new(turns);
        match provider.complete(&request("drifted")) {
            Err(AgentError::ReplayDiverged { turn, .. }) => assert_eq!(turn, 0),
            other => panic!("expected divergence to be caught, got {other:?}"),
        }
    }

    #[test]
    fn unchecked_replay_is_available_and_clearly_different() {
        let turns = vec![RecordedTurn {
            request: request("original").digest(),
            response: ModelResponse::saying("recorded answer"),
        }];
        let provider = ReplayProvider::unchecked(turns);
        assert_eq!(provider.complete(&request("drifted")).unwrap().text(), "recorded answer");
    }

    #[test]
    fn replaying_past_the_end_of_a_recording_is_an_error() {
        let provider = ReplayProvider::new(Vec::new());
        assert!(matches!(
            provider.complete(&request("a")),
            Err(AgentError::ReplayExhausted { turn: 0 })
        ));
    }

    #[test]
    fn usage_accumulates_across_turns() {
        let mut total = Usage::default();
        total.add(Usage { input_tokens: 100, output_tokens: 20 });
        total.add(Usage { input_tokens: 250, output_tokens: 35 });
        assert_eq!(total, Usage { input_tokens: 350, output_tokens: 55 });
        assert_eq!(total.total(), 405);
    }

    #[test]
    fn responses_round_trip_through_the_ledger_encoding() {
        let response = ModelResponse {
            content: vec![
                ContentBlock::text("thinking"),
                ContentBlock::ToolUse {
                    id: "x".into(),
                    name: "declare".into(),
                    input: serde_json::json!({"assertion": "a", "proof": "exit(t) == 0"}),
                },
            ],
            stop_reason: StopReason::ToolUse,
            usage: Usage { input_tokens: 9, output_tokens: 3 },
        };
        let encoded = serde_json::to_vec(&response).unwrap();
        assert_eq!(serde_json::from_slice::<ModelResponse>(&encoded).unwrap(), response);
    }
}
