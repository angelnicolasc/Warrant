//! The loop.
//!
//! ```text
//! think → declare → act → attest → map
//! ```
//!
//! Everything here is bookkeeping around a model call. That is the design:
//! the loop is deliberately dumb, and the judgement lives in the proof sealed
//! before the work and the map computed after it.
//!
//! Three things the loop does do, and each is a finding from the literature
//! rather than a preference:
//!
//! - **Every request and response is recorded verbatim**, with the request's
//!   address stored beside the answer, so a replay can prove it is answering
//!   the same question rather than a drifted one.
//! - **Stuck is arithmetic, not vibes.** Pre-registered claims make it
//!   measurable: turns elapsed, claims discharged, filesystem unchanged.
//! - **Approval gates the delta, not the intent.** A rejected turn is rolled
//!   back and the model is told, so a refusal actually refuses.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use warrant_core::{Budget, now_ms};
use warrant_diff::Snapshot;
use warrant_ledger::EntryKind;

use crate::error::{AgentError, Result};
use crate::policy::{Approver, Decision};
use crate::provider::{
    ContentBlock, Message, ModelRequest, Provider, RecordedTurn, Role, ToolSpec, Usage,
};
use crate::tools::{BuiltinTool, ToolOutcome, all_specs, invoke};
use crate::workspace::{DischargedClaim, Workspace};

/// The standing instructions every session starts with.
///
/// It explains the protocol rather than exhorting good behaviour. There is
/// nothing here of the form "do not cheat": the map already reports what the
/// proof depends on, and an instruction not to do the thing the architecture
/// already measures would be decoration.
pub const DEFAULT_SYSTEM_PROMPT: &str = "\
You are working inside an isolated cell on a software task.

Before doing work, call `declare` with what you intend to achieve and the proof \
you agree to be judged by. The proof is sealed at that moment: it is hashed into \
an append-only record and cannot be changed afterwards, so choose one that would \
actually fail if the work were wrong.

A proof is an expression over these terms:
  exit(<command>)          the command's exit status
  diff_touches(\"<glob>\")   whether your changes touch matching paths
  file_exists(\"<path>\")    whether a path exists now
  changed_files()          how many files you changed
combined with AND, OR, NOT and parentheses, and compared with ==, !=, <, <=, >, >=.

For example:
  exit(pytest -q) == 0 AND diff_touches(\"src/**\") AND NOT diff_touches(\"tests/**\")

When the work is done, call `attest`. You will receive one word — `warranted` or \
`unproven` — and nothing else. There is no score, and asking for one is not \
possible. If a claim comes back unproven, you may declare a new one and try again.

Your changes are compared against the state you started from by reverting them and \
re-running your proof, so work the proof does not depend on will be reported as \
unproven. That is information for a reviewer, not a penalty.";

/// How a session is run.
#[derive(Clone, Debug)]
pub struct SessionConfig {
    /// Which model to ask.
    pub model: String,
    /// Standing instructions.
    pub system: String,
    /// Hard ceiling on turns.
    pub max_turns: u32,
    /// Ceiling on a single response.
    pub max_tokens: u32,
    /// Wall-clock and token limits.
    pub budget: Budget,
    /// Turns without progress before the session is called stuck.
    pub stuck_patience: u32,
    /// Consecutive turns with the claim's cheap clauses false before the run
    /// is abandoned. `None` never abandons on that basis.
    ///
    /// Used by best-of-N to stop paying for a branch that has walked away
    /// from what it promised. See [`StopCondition::OffTrack`] for exactly what
    /// this does and does not establish.
    pub prune_patience: Option<u32>,
    /// Where probe cells are built.
    pub probe_root: PathBuf,
    /// Where subagent cells are built.
    pub delegate_root: PathBuf,
    /// How deep delegation may nest.
    pub max_delegation_depth: u32,
}

impl SessionConfig {
    /// A configuration rooted at a working directory.
    pub fn new(model: impl Into<String>, work_root: impl Into<PathBuf>) -> Self {
        let root: PathBuf = work_root.into();
        SessionConfig {
            model: model.into(),
            system: DEFAULT_SYSTEM_PROMPT.to_owned(),
            max_turns: 60,
            max_tokens: 8192,
            budget: Budget::UNLIMITED,
            stuck_patience: 5,
            prune_patience: None,
            // Under `.warrant/`, which every snapshot excludes unconditionally.
            // Probe and subagent cells are working artefacts, and a caller who
            // rooted them somewhere a snapshot could see would find them
            // counted as the agent's own changes — silently, and only in the
            // tree hash.
            probe_root: root.join(".warrant").join("probes"),
            delegate_root: root.join(".warrant").join("delegates"),
            max_delegation_depth: 2,
        }
    }
}

/// Why a session ended.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopCondition {
    /// The model stopped asking for tools.
    Finished,
    /// The turn ceiling was reached.
    MaxTurns,
    /// The token budget was spent.
    TokenBudget,
    /// The wall-clock budget was spent.
    WallClock,
    /// Nothing changed and nothing was claimed for too long.
    ///
    /// §5.7: the blocker was always *detecting* stuck, which without claims is
    /// guesswork. With pre-registered claims it is arithmetic.
    Stuck {
        /// How many consecutive turns made no progress.
        turns: u32,
    },
    /// The model declined to continue.
    Refused,
    /// The claim's command-free clauses were false for too long.
    ///
    /// §5.3's pruning signal. What it establishes is narrow and worth stating
    /// exactly: a command-free conjunct of the declared proof is false *right
    /// now*, so the proof is false right now. It is **not** a proof of
    /// unsatisfiability — a branch could put back a file it deleted. It is a
    /// decision about where to keep spending, and nothing is ever reported
    /// from it: the map is always recomputed from the real proof.
    OffTrack {
        /// How many consecutive turns the proof was false.
        turns: u32,
    },
}

impl StopCondition {
    /// Whether the session ended on its own terms.
    pub fn is_clean(&self) -> bool {
        matches!(self, StopCondition::Finished)
    }

    /// A line for the terminal.
    pub fn describe(&self) -> String {
        match self {
            StopCondition::Finished => "finished".into(),
            StopCondition::MaxTurns => "hit the turn limit".into(),
            StopCondition::TokenBudget => "spent its token budget".into(),
            StopCondition::WallClock => "ran out of time".into(),
            StopCondition::Stuck { turns } => {
                format!("stuck: {turns} turns with nothing claimed and nothing changed")
            }
            StopCondition::Refused => "declined to continue".into(),
            StopCondition::OffTrack { turns } => {
                format!("abandoned: its proof was false for {turns} turns running")
            }
        }
    }
}

/// What a session did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionOutcome {
    /// Turns taken.
    pub turns: u32,
    /// Tokens spent.
    pub usage: Usage,
    /// Why it ended.
    pub stop: StopCondition,
    /// Claims judged, in order.
    pub discharged: Vec<DischargedClaim>,
    /// Turns an approver rolled back.
    pub rejected_turns: u32,
    /// The model's closing words, if any.
    pub final_message: String,
}

impl SessionOutcome {
    /// Whether every claim made was warranted, and at least one was made.
    pub fn all_warranted(&self) -> bool {
        !self.discharged.is_empty() && self.discharged.iter().all(|claim| claim.warranted)
    }

    /// The last claim judged.
    pub fn last_claim(&self) -> Option<&DischargedClaim> {
        self.discharged.last()
    }
}

/// One run of the loop.
pub struct Session<'a> {
    provider: &'a dyn Provider,
    approver: &'a dyn Approver,
    workspace: Workspace,
    config: SessionConfig,
    tools: Vec<ToolSpec>,
    messages: Vec<Message>,
    usage: Usage,
    turns: u32,
    idle_turns: u32,
    off_track_turns: u32,
    rejected: u32,
    depth: u32,
    started: Instant,
}

impl<'a> Session<'a> {
    /// Prepare a session.
    pub fn new(
        provider: &'a dyn Provider,
        approver: &'a dyn Approver,
        workspace: Workspace,
        config: SessionConfig,
    ) -> Self {
        Session {
            provider,
            approver,
            workspace,
            config,
            tools: all_specs(),
            messages: Vec::new(),
            usage: Usage::default(),
            turns: 0,
            idle_turns: 0,
            off_track_turns: 0,
            rejected: 0,
            depth: 0,
            started: Instant::now(),
        }
    }

    /// The workspace, after the run.
    pub fn workspace(&self) -> &Workspace {
        &self.workspace
    }

    /// The workspace, mutably.
    pub fn workspace_mut(&mut self) -> &mut Workspace {
        &mut self.workspace
    }

    /// The conversation, for the operator. Never for another model.
    pub fn transcript(&self) -> &[Message] {
        &self.messages
    }

    fn at_depth(mut self, depth: u32) -> Self {
        self.depth = depth;
        self
    }

    /// Run until the model stops, the budget runs out, or nothing is moving.
    pub fn run(&mut self, task: &str) -> Result<SessionOutcome> {
        self.messages.push(Message::user(vec![ContentBlock::text(task)]));
        self.workspace.ledger().append_json(
            EntryKind::RunStarted,
            // The policy goes into the header because it is part of what the
            // run was. Replaying under a different one produces a different
            // conversation the moment a tool is refused in one and not the
            // other.
            &json!({
                "task": task,
                "model": self.config.model,
                "depth": self.depth,
                "policy": self.workspace.policy(),
            }),
            now_ms(),
        )?;
        // The starting tree goes in too, so the run is self-describing: a
        // reader with only the record can rebuild the world it happened in.
        // `bisect` and `freeze` are both built on that.
        self.workspace.ledger().append_json(
            EntryKind::CellSnapshot,
            self.workspace.origin(),
            now_ms(),
        )?;

        let stop = self.drive()?;
        let outcome = SessionOutcome {
            turns: self.turns,
            usage: self.usage,
            stop: stop.clone(),
            discharged: self.workspace.discharged().to_vec(),
            rejected_turns: self.rejected,
            final_message: self.last_text(),
        };
        self.workspace.ledger().append_json(
            EntryKind::RunFinished,
            &json!({ "stop": stop, "turns": self.turns, "usage": self.usage }),
            now_ms(),
        )?;
        Ok(outcome)
    }

    fn drive(&mut self) -> Result<StopCondition> {
        loop {
            if self.turns >= self.config.max_turns {
                return Ok(StopCondition::MaxTurns);
            }
            if let Some(limit) = self.config.budget.tokens
                && self.usage.total() >= limit
            {
                return Ok(StopCondition::TokenBudget);
            }
            if let Some(limit) = self.config.budget.wall_ms
                && self.started.elapsed().as_millis() as u64 >= limit
            {
                return Ok(StopCondition::WallClock);
            }

            let request = self.build_request();
            self.workspace.ledger().append_json(EntryKind::ModelRequest, &request, now_ms())?;

            let response = self.provider.complete(&request)?;
            self.workspace.ledger().append_json(
                EntryKind::ModelResponse,
                &RecordedTurn { request: request.digest(), response: response.clone() },
                now_ms(),
            )?;

            self.usage.add(response.usage);
            self.turns += 1;
            self.messages.push(Message::assistant(response.content.clone()));

            if matches!(response.stop_reason, crate::provider::StopReason::Refusal) {
                return Ok(StopCondition::Refused);
            }
            if !response.wants_tools() {
                return Ok(StopCondition::Finished);
            }

            let before = self.workspace.observe()?;
            let claims_before = self.workspace.discharged().len();
            let claim_open_before = self.workspace.has_active_claim();

            let mut results = Vec::new();
            for (id, name, input) in response.tool_uses() {
                let outcome = self.dispatch(name, input)?;
                results.push(ContentBlock::ToolResult {
                    tool_use_id: id.to_owned(),
                    content: outcome.content,
                    is_error: outcome.is_error,
                });
            }

            let after = self.workspace.observe()?;
            if let Some(note) = self.review(&after)? {
                results.push(ContentBlock::text(note));
            }
            self.messages.push(Message::user(results));

            // Progress is a *change*: the tree moved, a claim was judged, or a
            // claim opened or closed. Counting "a claim is open" as progress
            // would mean a session could never be found stuck once it had
            // declared anything, which is precisely the state worth catching.
            let progressed = after.root_hash() != before.root_hash()
                || self.workspace.discharged().len() != claims_before
                || self.workspace.has_active_claim() != claim_open_before;
            self.idle_turns = if progressed { 0 } else { self.idle_turns + 1 };
            if self.idle_turns >= self.config.stuck_patience {
                return Ok(StopCondition::Stuck { turns: self.idle_turns });
            }

            if let Some(patience) = self.config.prune_patience {
                // Cheap: this evaluates only the conjuncts that run no
                // commands, so it costs a tree comparison rather than a test
                // run.
                match self.workspace.structural_check()? {
                    Some(false) => {
                        self.off_track_turns += 1;
                        if self.off_track_turns >= patience {
                            return Ok(StopCondition::OffTrack { turns: self.off_track_turns });
                        }
                    }
                    _ => self.off_track_turns = 0,
                }
            }
        }
    }

    /// Judge what the turn actually did, and roll it back if refused.
    fn review(&mut self, after: &Snapshot) -> Result<Option<String>> {
        let radius = self.workspace.blast_radius_at(after)?;
        if radius.is_empty() {
            return Ok(None);
        }
        match self.approver.judge(&radius) {
            Decision::Apply => {
                self.workspace.accept_at(after.clone());
                Ok(None)
            }
            Decision::Reject => {
                self.workspace.roll_back()?;
                self.rejected += 1;
                self.workspace.ledger().append_json(
                    EntryKind::Note,
                    &json!({ "rejected": radius }),
                    now_ms(),
                )?;
                Ok(Some(format!(
                    "That turn was rolled back: it changed {}, which is outside the blast radius \
                     this run permits. The cell is back to how it was before the turn.",
                    radius.summary()
                )))
            }
        }
    }

    fn dispatch(&mut self, name: &str, input: &Value) -> Result<ToolOutcome> {
        let Some(tool) = BuiltinTool::parse(name) else {
            return Ok(ToolOutcome::failed(format!(
                "`{name}` is not a tool. Available: {}",
                BuiltinTool::ALL.iter().map(BuiltinTool::name).collect::<Vec<_>>().join(", ")
            )));
        };
        if tool == BuiltinTool::Delegate {
            return self.delegate(input);
        }
        // Resolved before the workspace is borrowed mutably.
        let probe_root = self.probe_root_for_turn();
        invoke(tool, input, &mut self.workspace, &probe_root)
    }

    fn probe_root_for_turn(&self) -> PathBuf {
        self.config.probe_root.join(format!("d{}-t{}", self.depth, self.turns))
    }

    /// Hand one claim to a subagent.
    ///
    /// The subagent gets its own cell, its own claim and its own budget. What
    /// comes back is a verdict and one line — never a transcript, which is
    /// what stops the telephone effect at the boundary rather than hoping a
    /// summary survives it.
    ///
    /// Its work is merged only if the claim was warranted. Unproven work is
    /// discarded, which makes the boundary meaningful in both directions.
    fn delegate(&mut self, input: &Value) -> Result<ToolOutcome> {
        if self.depth >= self.config.max_delegation_depth {
            return Ok(ToolOutcome::failed(format!(
                "delegation is limited to {} levels and this session is already at {}",
                self.config.max_delegation_depth, self.depth
            )));
        }
        let task = input.get("task").and_then(Value::as_str).ok_or(AgentError::BadToolInput {
            tool: "delegate",
            reason: "`task` is required".into(),
        })?;
        let proof = input.get("proof").and_then(Value::as_str).ok_or(AgentError::BadToolInput {
            tool: "delegate",
            reason: "`proof` is required".into(),
        })?;
        let max_turns = input
            .get("max_turns")
            .and_then(Value::as_u64)
            .unwrap_or(u64::from(self.config.max_turns / 2))
            .clamp(1, u64::from(self.config.max_turns)) as u32;

        let state = self.workspace.observe()?;
        let root = self.config.delegate_root.join(format!("d{}-t{}", self.depth, self.turns));
        let cell = crate::probe_cell(&root, &state, Arc::clone(self.workspace.store()))?;

        let mut sub_workspace = Workspace::new(cell, self.workspace.services())?;

        // The claim is sealed by the parent, so the subagent is bound to it
        // and cannot quietly widen what it agreed to be judged by.
        if let Err(AgentError::Attest(e)) =
            sub_workspace.declare(task, proof, warrant_core::ProofTier::Unit)
        {
            return Ok(ToolOutcome::failed(format!("that proof does not parse:\n{e}")));
        }

        let mut config = self.config.clone();
        config.max_turns = max_turns;
        config.system = format!(
            "{}\n\nA claim has already been declared for you:\n  {task}\nwith the proof:\n  {proof}\n\
             Do the work and call `attest` when it is done.",
            self.config.system
        );

        let outcome = {
            let mut sub = Session::new(self.provider, self.approver, sub_workspace, config)
                .at_depth(self.depth + 1);
            let result = sub.run(task)?;
            let final_state = sub.workspace().observe()?;
            (result, final_state)
        };
        let (result, final_state) = outcome;

        self.usage.add(result.usage);
        let warranted = result.all_warranted();

        self.workspace.ledger().append_json(
            EntryKind::Note,
            &json!({
                "delegation": { "task": task, "warranted": warranted, "turns": result.turns, "stop": result.stop }
            }),
            now_ms(),
        )?;

        if warranted {
            self.workspace.adopt_state(&final_state)?;
            Ok(ToolOutcome::ok(format!(
                "warranted — the subagent discharged its claim in {} turns, and its work is now in \
                 your cell.",
                result.turns
            )))
        } else {
            Ok(ToolOutcome::failed(format!(
                "unproven — the subagent {} after {} turns, and its work was discarded. Your cell \
                 is unchanged.",
                result.stop.describe(),
                result.turns
            )))
        }
    }

    fn build_request(&self) -> ModelRequest {
        ModelRequest {
            model: self.config.model.clone(),
            system: self.config.system.clone(),
            messages: self.messages.clone(),
            tools: self.tools.clone(),
            max_tokens: self.config.max_tokens,
        }
    }

    fn last_text(&self) -> String {
        self.messages
            .iter()
            .rev()
            .find(|message| message.role == Role::Assistant)
            .map(|message| {
                message
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{ApproveAll, ApproveWithin};
    use crate::provider::{ModelResponse, ScriptedProvider, recorded_turns};
    use crate::test_support::{Guards, scratch_workspace};

    fn config(guards: &Guards) -> SessionConfig {
        let root = guards.cell_root().parent().expect("cell has a parent").join("work");
        SessionConfig { max_turns: 12, stuck_patience: 3, ..SessionConfig::new("test-model", root) }
    }

    fn declare(id: &str, assertion: &str, proof: &str) -> ModelResponse {
        ModelResponse::calling(id, "declare", json!({ "assertion": assertion, "proof": proof }))
    }

    fn write(id: &str, path: &str, content: &str) -> ModelResponse {
        ModelResponse::calling(id, "fs", json!({ "op": "write", "path": path, "content": content }))
    }

    fn attest(id: &str) -> ModelResponse {
        ModelResponse::calling(id, "attest", json!({}))
    }

    #[test]
    fn a_session_declares_works_attests_and_stops() {
        let (guards, workspace) = scratch_workspace(&[("src/a.txt", "before")]);
        let provider = ScriptedProvider::new([
            declare("1", "b.txt will exist", "file_exists(b.txt)"),
            write("2", "b.txt", "created"),
            attest("3"),
            ModelResponse::saying("Done."),
        ]);

        let mut session = Session::new(&provider, &ApproveAll, workspace, config(&guards));
        let outcome = session.run("create b.txt").unwrap();

        assert_eq!(outcome.stop, StopCondition::Finished);
        assert_eq!(outcome.turns, 4);
        assert!(outcome.all_warranted(), "{:?}", outcome.discharged);
        assert_eq!(outcome.final_message, "Done.");
        assert_eq!(guards.read("b.txt"), "created");
    }

    #[test]
    fn a_claim_whose_proof_does_not_hold_comes_back_unproven() {
        let (guards, workspace) = scratch_workspace(&[("src/a.txt", "before")]);
        let provider = ScriptedProvider::new([
            declare("1", "b.txt will exist", "file_exists(b.txt)"),
            attest("2"),
            ModelResponse::saying("I could not do it."),
        ]);

        let mut session = Session::new(&provider, &ApproveAll, workspace, config(&guards));
        let outcome = session.run("create b.txt").unwrap();

        assert!(!outcome.all_warranted());
        assert_eq!(outcome.discharged.len(), 1);
        assert!(!outcome.discharged[0].warranted);
    }

    /// §5.7. Without pre-registered claims, "stuck" is guesswork. With them it
    /// is arithmetic: turns elapsed, nothing claimed, nothing changed.
    #[test]
    fn a_session_that_stops_making_progress_is_detected() {
        let (guards, workspace) = scratch_workspace(&[("a.txt", "x")]);
        let reading =
            || ModelResponse::calling("r", "fs", json!({ "op": "read", "path": "a.txt" }));
        let provider =
            ScriptedProvider::new([reading(), reading(), reading(), reading(), reading()]);

        let mut session = Session::new(&provider, &ApproveAll, workspace, config(&guards));
        let outcome = session.run("look around forever").unwrap();

        assert_eq!(outcome.stop, StopCondition::Stuck { turns: 3 });
        assert_eq!(outcome.turns, 3, "it should stop rather than burn the whole budget");
    }

    /// Declaring counts as progress on the turn it happens, and only then.
    /// Otherwise a session could never be found stuck once it had declared
    /// anything, which is exactly the state worth catching.
    #[test]
    fn an_open_claim_does_not_mask_a_stuck_session() {
        let (guards, workspace) = scratch_workspace(&[("a.txt", "x")]);
        let reading =
            || ModelResponse::calling("r", "fs", json!({ "op": "read", "path": "a.txt" }));
        let provider = ScriptedProvider::new([
            declare("1", "something", "file_exists(a.txt)"),
            reading(),
            reading(),
            reading(),
            reading(),
        ]);

        let mut session = Session::new(&provider, &ApproveAll, workspace, config(&guards));
        let outcome = session.run("declare then idle").unwrap();
        assert!(matches!(outcome.stop, StopCondition::Stuck { .. }), "{:?}", outcome.stop);
    }

    #[test]
    fn the_turn_ceiling_is_enforced() {
        let (guards, workspace) = scratch_workspace(&[("a.txt", "x")]);
        let responses: Vec<ModelResponse> =
            (0..20).map(|i| write(&i.to_string(), &format!("f{i}.txt"), "content")).collect();
        let provider = ScriptedProvider::new(responses);

        let mut settings = config(&guards);
        settings.max_turns = 4;
        let mut session = Session::new(&provider, &ApproveAll, workspace, settings);
        let outcome = session.run("write forever").unwrap();

        assert_eq!(outcome.stop, StopCondition::MaxTurns);
        assert_eq!(outcome.turns, 4);
    }

    #[test]
    fn a_token_budget_stops_the_session() {
        let (guards, workspace) = scratch_workspace(&[("a.txt", "x")]);
        let costly = |i: usize| {
            let mut response = write(&i.to_string(), &format!("f{i}.txt"), "c");
            response.usage = Usage { input_tokens: 400, output_tokens: 100 };
            response
        };
        let provider = ScriptedProvider::new((0..10).map(costly));

        let mut settings = config(&guards);
        settings.budget = Budget::UNLIMITED;
        settings.budget.tokens = Some(1200);
        let mut session = Session::new(&provider, &ApproveAll, workspace, settings);
        let outcome = session.run("spend").unwrap();

        assert_eq!(outcome.stop, StopCondition::TokenBudget);
        assert!(outcome.usage.total() >= 1200);
        assert_eq!(outcome.turns, 3, "it stops on the turn that crosses the line, not after");
    }

    /// §5.5. Approval gates the consequence rather than the intent, and a
    /// refusal actually refuses: the turn is rolled back and the model is told.
    #[test]
    fn a_turn_outside_the_blast_radius_is_rolled_back_and_reported() {
        let (guards, workspace) = scratch_workspace(&[("a.txt", "original")]);
        let provider = ScriptedProvider::new([
            ModelResponse {
                content: vec![
                    ContentBlock::ToolUse {
                        id: "1".into(),
                        name: "fs".into(),
                        input: json!({ "op": "write", "path": "one.txt", "content": "x" }),
                    },
                    ContentBlock::ToolUse {
                        id: "2".into(),
                        name: "fs".into(),
                        input: json!({ "op": "write", "path": "two.txt", "content": "x" }),
                    },
                ],
                stop_reason: crate::provider::StopReason::ToolUse,
                usage: Usage::default(),
            },
            ModelResponse::saying("understood"),
        ]);

        let approver = ApproveWithin { max_files: 1, ..ApproveWithin::default() };
        let mut session = Session::new(&provider, &approver, workspace, config(&guards));
        let outcome = session.run("write two files").unwrap();

        assert_eq!(outcome.rejected_turns, 1);
        assert!(!guards.exists("one.txt"), "a rejected turn must leave nothing behind");
        assert!(!guards.exists("two.txt"));

        let told = session.transcript().iter().flat_map(|m| &m.content).any(
            |block| matches!(block, ContentBlock::Text { text } if text.contains("rolled back")),
        );
        assert!(told, "the model must be told its turn was rejected");
    }

    #[test]
    fn a_turn_inside_the_blast_radius_is_kept() {
        let (guards, workspace) = scratch_workspace(&[("a.txt", "original")]);
        let provider =
            ScriptedProvider::new([write("1", "one.txt", "kept"), ModelResponse::saying("done")]);
        let approver = ApproveWithin { max_files: 5, ..ApproveWithin::default() };
        let mut session = Session::new(&provider, &approver, workspace, config(&guards));
        let outcome = session.run("write one file").unwrap();

        assert_eq!(outcome.rejected_turns, 0);
        assert_eq!(guards.read("one.txt"), "kept");
    }

    #[test]
    fn an_unknown_tool_is_reported_to_the_model_rather_than_ending_the_run() {
        let (guards, workspace) = scratch_workspace(&[("a.txt", "x")]);
        let provider = ScriptedProvider::new([
            ModelResponse::calling("1", "kubectl", json!({})),
            ModelResponse::saying("understood"),
        ]);

        let mut session = Session::new(&provider, &ApproveAll, workspace, config(&guards));
        let outcome = session.run("use a tool that does not exist").unwrap();
        assert_eq!(outcome.stop, StopCondition::Finished);

        let told = session.transcript().iter().flat_map(|m| &m.content).any(|block| {
            matches!(block, ContentBlock::ToolResult { content, is_error, .. }
                if *is_error && content.contains("is not a tool"))
        });
        assert!(told);
    }

    /// L5. A subagent returns a verdict and a line, never a transcript — and
    /// its work only crosses the boundary if the claim was warranted.
    #[test]
    fn warranted_delegated_work_comes_back_and_unproven_work_does_not() {
        let (guards, workspace) = scratch_workspace(&[("a.txt", "x")]);
        let provider = ScriptedProvider::new([
            // Parent delegates.
            ModelResponse::calling(
                "1",
                "delegate",
                json!({ "task": "create done.txt", "proof": "file_exists(done.txt)", "max_turns": 4 }),
            ),
            // Subagent does the work and attests.
            write("s1", "done.txt", "by the subagent"),
            attest("s2"),
            ModelResponse::saying("subagent finished"),
            // Parent finishes.
            ModelResponse::saying("all done"),
        ]);

        let mut session = Session::new(&provider, &ApproveAll, workspace, config(&guards));
        let outcome = session.run("delegate the work").unwrap();

        assert_eq!(outcome.stop, StopCondition::Finished);
        assert_eq!(guards.read("done.txt"), "by the subagent", "warranted work must be merged");

        let reported: Vec<&str> = session
            .transcript()
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|block| match block {
                ContentBlock::ToolResult { content, .. } => Some(content.as_str()),
                _ => None,
            })
            .collect();
        let delegation = reported.iter().find(|c| c.contains("subagent")).expect("a report");
        assert!(delegation.contains("warranted"));
        assert!(
            !delegation.contains("create done.txt\n"),
            "a transcript must not cross the boundary: {delegation}"
        );
    }

    #[test]
    fn unproven_delegated_work_is_discarded() {
        let (guards, workspace) = scratch_workspace(&[("a.txt", "x")]);
        let provider = ScriptedProvider::new([
            ModelResponse::calling(
                "1",
                "delegate",
                json!({ "task": "create done.txt", "proof": "file_exists(done.txt)", "max_turns": 3 }),
            ),
            // The subagent writes the wrong file, then attests anyway.
            write("s1", "wrong.txt", "not what was asked"),
            attest("s2"),
            ModelResponse::saying("subagent gave up"),
            ModelResponse::saying("understood"),
        ]);

        let mut session = Session::new(&provider, &ApproveAll, workspace, config(&guards));
        session.run("delegate the work").unwrap();

        assert!(!guards.exists("done.txt"));
        assert!(!guards.exists("wrong.txt"), "unproven work must not reach the parent cell");
    }

    #[test]
    fn delegation_can_be_switched_off_entirely() {
        let (guards, workspace) = scratch_workspace(&[("a.txt", "x")]);
        let provider = ScriptedProvider::new([
            ModelResponse::calling(
                "1",
                "delegate",
                json!({ "task": "do it for me", "proof": "file_exists(a.txt)" }),
            ),
            ModelResponse::saying("I will do it myself then"),
        ]);

        let mut settings = config(&guards);
        settings.max_delegation_depth = 0;
        let mut session = Session::new(&provider, &ApproveAll, workspace, settings);
        session.run("delegate").unwrap();

        let refused = session.transcript().iter().flat_map(|m| &m.content).any(|block| {
            matches!(block, ContentBlock::ToolResult { content, is_error, .. }
                if *is_error && content.contains("delegation is limited"))
        });
        assert!(refused, "the bound must be reported to the caller that hit it");
    }

    /// Recursion terminates. The refusal happens in the deepest session, so
    /// what the *parent* sees is its subagent coming back unproven — which is
    /// the boundary working as designed: a failure crosses it, a transcript
    /// does not.
    #[test]
    fn nested_delegation_terminates_and_surfaces_as_a_failed_claim() {
        let (guards, workspace) = scratch_workspace(&[("a.txt", "x")]);
        let delegating = || {
            ModelResponse::calling(
                "d",
                "delegate",
                json!({ "task": "go deeper", "proof": "file_exists(deep.txt)", "max_turns": 3 }),
            )
        };
        let provider = ScriptedProvider::new([
            delegating(),
            delegating(),
            delegating(),
            ModelResponse::saying("cannot go deeper"),
            ModelResponse::saying("subagent done"),
            ModelResponse::saying("parent done"),
        ]);

        let mut settings = config(&guards);
        settings.max_delegation_depth = 2;
        let mut session = Session::new(&provider, &ApproveAll, workspace, settings);
        let outcome = session.run("delegate recursively").unwrap();

        assert_eq!(outcome.stop, StopCondition::Finished, "recursion must terminate");
        let reported = session
            .transcript()
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|block| match block {
                ContentBlock::ToolResult { content, .. } => Some(content.as_str()),
                _ => None,
            })
            .find(|content| content.contains("subagent"))
            .expect("the parent should have heard back");
        assert!(reported.contains("unproven"), "{reported}");
        assert!(!guards.exists("deep.txt"));
    }

    /// The phase-one gate, at the level of a whole session: a run replays from
    /// the record alone, and the replay is checked rather than assumed.
    #[test]
    fn a_recorded_session_replays_from_the_ledger_alone() {
        let script = || {
            [
                declare("1", "b.txt will exist", "file_exists(b.txt)"),
                write("2", "b.txt", "created"),
                attest("3"),
                ModelResponse::saying("Done."),
            ]
        };

        let (first_guards, first_workspace) = scratch_workspace(&[("src/a.txt", "before")]);
        let ledger = Arc::clone(first_workspace.ledger());
        let live = ScriptedProvider::new(script());
        let mut original = Session::new(&live, &ApproveAll, first_workspace, config(&first_guards));
        let first = original.run("create b.txt").unwrap();

        let turns = recorded_turns(&ledger).unwrap();
        assert_eq!(turns.len(), 4, "every exchange must be in the record");

        // A fresh cell in the same starting state, driven only by the record.
        let (second_guards, second_workspace) = scratch_workspace(&[("src/a.txt", "before")]);
        let replay = crate::provider::ReplayProvider::new(turns);
        let mut replayed =
            Session::new(&replay, &ApproveAll, second_workspace, config(&second_guards));
        let second = replayed.run("create b.txt").unwrap();

        assert_eq!(second.turns, first.turns);
        assert_eq!(second.stop, first.stop);
        assert_eq!(second.final_message, first.final_message);
        assert_eq!(guards_state(&second_guards), guards_state(&first_guards));
        assert_eq!(replay.position(), 4, "every recorded turn was used");
    }

    #[test]
    fn a_replay_of_a_different_task_is_refused_rather_than_answered() {
        let (guards, workspace) = scratch_workspace(&[("a.txt", "x")]);
        let ledger = Arc::clone(workspace.ledger());
        let live = ScriptedProvider::new([ModelResponse::saying("done")]);
        let mut original = Session::new(&live, &ApproveAll, workspace, config(&guards));
        original.run("the original task").unwrap();

        let (other_guards, other_workspace) = scratch_workspace(&[("a.txt", "x")]);
        let replay = crate::provider::ReplayProvider::new(recorded_turns(&ledger).unwrap());
        let mut replayed =
            Session::new(&replay, &ApproveAll, other_workspace, config(&other_guards));

        let error = replayed.run("a completely different task").unwrap_err();
        assert!(
            matches!(error, AgentError::ReplayDiverged { .. }),
            "a drifted replay must fail loudly: {error}"
        );
    }

    /// Invariant 3, at the session boundary: no coverage figure ever appears
    /// in anything the model reads.
    #[test]
    fn no_coverage_reaches_the_model() {
        let (guards, workspace) = scratch_workspace(&[("a.txt", "x")]);
        let provider = ScriptedProvider::new([
            declare("1", "b.txt will exist", "file_exists(b.txt)"),
            write("2", "b.txt", "created"),
            attest("3"),
            ModelResponse::saying("done"),
        ]);

        let mut session = Session::new(&provider, &ApproveAll, workspace, config(&guards));
        let outcome = session.run("create b.txt").unwrap();
        assert!(outcome.all_warranted());

        // The map exists and carries a real number...
        assert!(outcome.discharged[0].map.coverage.is_defined());

        // ...and none of it is in anything the model was shown.
        let seen = serde_json::to_string(session.transcript()).unwrap();
        assert!(!seen.contains("coverage"), "coverage leaked into the transcript");
        assert!(!seen.contains("load_bearing"));
        assert!(!seen.contains("proof_coverage"));
        for request in provider.requests() {
            let encoded = serde_json::to_string(&request).unwrap();
            assert!(!encoded.contains("coverage"), "coverage leaked into a request");
        }
    }

    fn guards_state(guards: &Guards) -> Vec<(String, String)> {
        let mut entries: Vec<(String, String)> = walk(guards.cell_root(), guards.cell_root());
        entries.sort();
        entries
    }

    fn walk(root: &std::path::Path, dir: &std::path::Path) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let Ok(read) = std::fs::read_dir(dir) else { return out };
        for entry in read.flatten() {
            let path = entry.path();
            if path.is_dir() {
                out.extend(walk(root, &path));
            } else if let Ok(text) = std::fs::read_to_string(&path) {
                let rel = path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
                out.push((rel, text));
            }
        }
        out
    }
}
