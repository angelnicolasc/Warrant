//! What the tools act on.
//!
//! A workspace owns the cell, the record, the policy and the claim currently
//! in flight. Tools take `&mut Workspace` and nothing else, so there is no
//! path from a tool to a model's context and no path from a model's output to
//! a `Delta`.
//!
//! # Claims partition a session
//!
//! Each claim is judged against the state left by the previous one, or by the
//! start of the run. That is deliberate: if a claim's pre-image were taken at
//! the moment it was *declared*, an agent could do the work first, declare
//! afterwards, and the map would see an empty diff. Anchoring to the previous
//! boundary means work done before declaring is still inside the diff being
//! measured — where it shows up as unproven if the proof does not depend on it.

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use warrant_attest::{Attestor, CellEnvironment, Predicate, ProbeEnvironment};
use warrant_cell::{Cell, CommandSpec, ExitRecord};
use warrant_core::{
    ArtifactKind, Budget, Claim, ClaimId, Handle, PredicateHash, ProofTier, Verdict, now_ms,
};
use warrant_diff::{ContentStore, OverlayDiff, Snapshot};
use warrant_ledger::{EntryKind, Ledger};
use warrant_necessity::{NecessityConfig, NecessityMap, PathClassifier, Search};

use crate::error::{AgentError, Result};
use crate::policy::{BlastRadius, Policy};

/// A claim that has been sealed and not yet judged.
pub struct ActiveClaim {
    /// The sealed claim.
    pub claim: Claim,
    /// Its compiled proof.
    pub predicate: Predicate,
    /// The proof as written, for the receipt.
    pub source: String,
}

/// A claim that has been judged.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DischargedClaim {
    /// Which claim.
    pub id: ClaimId,
    /// What it said.
    pub assertion: String,
    /// Its sealed proof.
    pub predicate: PredicateHash,
    /// The proof as written.
    pub source: String,
    /// Whether it held.
    pub warranted: bool,
    /// The map produced while judging it.
    ///
    /// Human-facing. This never travels back to the model — see the invariant
    /// test in `warrant-necessity`.
    pub map: NecessityMap,
}

/// The things every workspace in a run shares.
///
/// One record, one content store, one proof runner, one policy — passed
/// around together because a subagent or a best-of-N branch that quietly got
/// a *different* ledger would produce evidence nobody could join up.
#[derive(Clone)]
pub struct Services {
    /// Where artefacts live.
    pub store: Arc<dyn ContentStore>,
    /// The record.
    pub ledger: Arc<Ledger>,
    /// The sealed proof runner.
    pub attestor: Arc<Attestor>,
    /// What the agent may do.
    pub policy: Policy,
}

impl Services {
    /// Assemble the shared services for a run.
    pub fn new(
        store: Arc<dyn ContentStore>,
        ledger: Arc<Ledger>,
        attestor: Arc<Attestor>,
        policy: Policy,
    ) -> Self {
        Services { store, ledger, attestor, policy }
    }
}

/// Everything the tools operate on.
pub struct Workspace {
    cell: Arc<Mutex<dyn Cell>>,
    store: Arc<dyn ContentStore>,
    ledger: Arc<Ledger>,
    attestor: Arc<Attestor>,
    classifier: PathClassifier,
    policy: Policy,

    /// The state this run started from.
    origin: Snapshot,
    /// The state the current claim is judged against.
    claim_baseline: Snapshot,
    /// The last state an approver allowed to stand.
    approved: Snapshot,

    active: Option<ActiveClaim>,
    discharged: Vec<DischargedClaim>,
    egress: Vec<String>,
    processes: usize,
    exits: Vec<ExitRecord>,
}

impl Workspace {
    /// Build a workspace over a cell that is already in its starting state.
    pub fn new(cell: Arc<Mutex<dyn Cell>>, services: Services) -> Result<Self> {
        let origin = {
            let mut guard = cell.lock().expect("cell poisoned");
            guard.snapshot()?.as_snapshot().clone()
        };
        let classifier = NecessityConfig::default().path_classifier()?;
        let Services { store, ledger, attestor, policy } = services;
        Ok(Workspace {
            cell,
            store,
            ledger,
            attestor,
            classifier,
            policy,
            claim_baseline: origin.clone(),
            approved: origin.clone(),
            origin,
            active: None,
            discharged: Vec::new(),
            egress: Vec::new(),
            processes: 0,
            exits: Vec::new(),
        })
    }

    /// The state the run started from.
    pub fn origin(&self) -> &Snapshot {
        &self.origin
    }

    /// The policy in force.
    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    /// Replace the policy.
    ///
    /// Set by the operator before a run, never by the agent: there is no tool
    /// that reaches this, which is what stops a session widening its own
    /// permissions partway through.
    pub fn set_policy(&mut self, policy: Policy) {
        self.policy = policy;
    }

    /// The record.
    pub fn ledger(&self) -> &Arc<Ledger> {
        &self.ledger
    }

    /// The cell.
    pub fn cell(&self) -> &Arc<Mutex<dyn Cell>> {
        &self.cell
    }

    /// The content store.
    pub fn store(&self) -> &Arc<dyn ContentStore> {
        &self.store
    }

    /// The sealed proof runner.
    pub fn attestor(&self) -> &Arc<Attestor> {
        &self.attestor
    }

    /// The shared services, for building a subagent or a sibling branch.
    pub fn services(&self) -> Services {
        Services {
            store: Arc::clone(&self.store),
            ledger: Arc::clone(&self.ledger),
            attestor: Arc::clone(&self.attestor),
            policy: self.policy.clone(),
        }
    }

    /// Claims judged so far.
    pub fn discharged(&self) -> &[DischargedClaim] {
        &self.discharged
    }

    /// Whether a claim is waiting to be judged.
    pub fn has_active_claim(&self) -> bool {
        self.active.is_some()
    }

    /// Every command run so far.
    pub fn exits(&self) -> &[ExitRecord] {
        &self.exits
    }

    /// Observe the cell now.
    pub fn observe(&self) -> Result<Snapshot> {
        let mut guard = self.cell.lock().expect("cell poisoned");
        Ok(guard.snapshot()?.as_snapshot().clone())
    }

    /// The diff between the run's start and now.
    pub fn diff_since_origin(&self) -> Result<OverlayDiff> {
        let now = self.observe()?;
        Ok(OverlayDiff::between(&self.origin, &now, self.store.as_ref())?)
    }

    /// What has happened since the last approved state.
    pub fn blast_radius(&self) -> Result<BlastRadius> {
        let now = self.observe()?;
        self.blast_radius_at(&now)
    }

    /// The same, against a state the caller has already observed.
    ///
    /// A tree scan is the most expensive thing a turn does, so the session
    /// takes one observation and uses it for both approval and progress
    /// detection rather than scanning twice for the same answer.
    pub fn blast_radius_at(&self, now: &Snapshot) -> Result<BlastRadius> {
        let diff = OverlayDiff::between(&self.approved, now, self.store.as_ref())?;

        let mut radius = BlastRadius {
            changed_lines: diff.changed_lines(),
            egress_hosts: self.egress.clone(),
            processes: self.processes,
            ..BlastRadius::default()
        };
        for file in &diff.files {
            match file.change {
                warrant_diff::ChangeKind::Deleted => radius.files_deleted += 1,
                _ => radius.files_changed += 1,
            }
            if self.classifier.is_verification_surface(&file.path) {
                radius.verification_paths.push(file.path.clone());
            }
        }
        Ok(radius)
    }

    /// Accept everything done since the last approval.
    pub fn accept(&mut self) -> Result<()> {
        let now = self.observe()?;
        self.accept_at(now);
        Ok(())
    }

    /// The same, against a state the caller has already observed.
    pub fn accept_at(&mut self, now: Snapshot) {
        self.approved = now;
        self.egress.clear();
        self.processes = 0;
    }

    /// Put the cell into a given state and treat it as approved.
    ///
    /// Used when a subagent's work returns: only a warranted result is ever
    /// merged, so the parent adopts it wholesale rather than reviewing it
    /// again.
    pub fn adopt_state(&mut self, state: &Snapshot) -> Result<()> {
        {
            let mut guard = self.cell.lock().expect("cell poisoned");
            guard.restore(state)?;
        }
        self.approved = state.clone();
        Ok(())
    }

    /// Undo everything done since the last approval.
    pub fn roll_back(&mut self) -> Result<()> {
        {
            let mut guard = self.cell.lock().expect("cell poisoned");
            guard.restore(&self.approved)?;
        }
        self.egress.clear();
        self.processes = 0;
        Ok(())
    }

    /// Turn tool output into something context-sized.
    ///
    /// Below the policy's inline limit the text travels as itself. Above it,
    /// the bytes go to the store and a handle travels instead, so a session's
    /// context does not grow with the size of a test log.
    pub fn present(&self, kind: ArtifactKind, content: &[u8]) -> Result<String> {
        if content.len() <= self.policy.inline_limit {
            return Ok(String::from_utf8_lossy(content).into_owned());
        }
        let address = self.store.put(content).map_err(|reason| AgentError::Refused { reason })?;
        let handle = Handle::at(
            address,
            kind,
            content.len() as u64,
            String::from_utf8_lossy(&content[..content.len().min(400)]),
        );
        Ok(format!(
            "{}\nToo large to inline. Read a slice with fs.read using this address.",
            warrant_core::ContextRenderable::render_for_model(&handle)
        ))
    }

    /// Run a command inside the cell.
    pub fn exec(&mut self, command: &str) -> Result<ExitRecord> {
        if !self.policy.permits_command(command) {
            return Err(AgentError::Refused {
                reason: format!(
                    "`{command}` is not permitted. Warrant treats git as an output of the record, \
                     so history-rewriting commands are refused."
                ),
            });
        }
        let mut spec = CommandSpec::parse(command)?;
        if let Some(ms) = self.policy.command_timeout_ms {
            spec = spec.with_timeout_ms(ms);
        }
        self.ledger.append_json(EntryKind::ToolCall, &spec, now_ms())?;

        let record = {
            let mut guard = self.cell.lock().expect("cell poisoned");
            guard.exec(&spec)?
        };
        self.ledger.append_json(EntryKind::ToolResult, &record, now_ms())?;
        self.processes += 1;
        self.exits.push(record.clone());
        Ok(record)
    }

    /// Record that the network was reached.
    pub fn record_egress(&mut self, host: &str) {
        if !self.egress.iter().any(|h| h == host) {
            self.egress.push(host.to_owned());
        }
    }

    /// Seal a claim.
    ///
    /// Fails if one is already in flight: a session with two open claims has
    /// no answer to the question of which diff belongs to which.
    pub fn declare(&mut self, assertion: &str, proof: &str, tier: ProofTier) -> Result<ClaimId> {
        if self.active.is_some() {
            return Err(AgentError::Refused {
                reason: "a claim is already open; attest it before declaring another".into(),
            });
        }
        let predicate = Predicate::compile(proof)?;
        let claim = Claim::declare(assertion, predicate.hash(), tier, Budget::UNLIMITED, now_ms());

        // The proof is sealed into the record before any further work, which
        // is the entire point of declaring: it cannot be revised once the
        // outcome is known.
        self.ledger.append(EntryKind::PredicateSealed, predicate.wasm(), now_ms())?;
        self.ledger.append_json(EntryKind::ClaimDeclared, &claim, now_ms())?;

        let id = claim.id;
        self.active = Some(ActiveClaim { claim, predicate, source: proof.to_owned() });
        Ok(id)
    }

    /// Judge the open claim, and return one bit.
    ///
    /// The necessity map is produced here and written to the record. It is
    /// deliberately not part of the return value: coverage fed back to the
    /// party being measured becomes the next thing to saturate.
    pub fn attest(&mut self, probe_root: &std::path::Path) -> Result<Verdict> {
        let active = self.active.take().ok_or(AgentError::NoActiveClaim)?;
        let after = self.observe()?;
        let diff = OverlayDiff::between(&self.claim_baseline, &after, self.store.as_ref())?;

        let config = NecessityConfig {
            command_timeout_ms: self.policy.command_timeout_ms,
            ..NecessityConfig::default()
        };
        let cells = crate::probe_cells(
            probe_root,
            &self.claim_baseline,
            Arc::clone(&self.store),
            config.parallelism,
        )?;

        let mut search = Search::new(
            cells,
            &self.claim_baseline,
            &after,
            &diff,
            self.store.as_ref(),
            &self.attestor,
            &active.predicate,
            &config,
        )
        .for_claim(active.claim.id);
        let map = search.run()?;

        for record in &search.commands() {
            self.ledger.append_json(EntryKind::Probe, record, now_ms())?;
        }
        self.ledger.append_json(EntryKind::NecessityMapped, &map, now_ms())?;

        let warranted = map.satisfied && map.null_passed;
        let verdict = if warranted {
            Verdict::Warranted {
                receipt: warrant_core::ReceiptRef::derive(&[map.post_root.as_bytes()]),
            }
        } else {
            Verdict::Unproven
        };
        self.ledger.append_json(EntryKind::Attested, &verdict, now_ms())?;
        if !warranted {
            // Failed claims stay in the record with their evidence, so the
            // same dead approach is not re-attempted next session.
            self.ledger.append_json(EntryKind::Refutation, &map, now_ms())?;
        }

        self.discharged.push(DischargedClaim {
            id: active.claim.id,
            assertion: active.claim.assertion.clone(),
            predicate: active.predicate.hash(),
            source: active.source,
            warranted,
            map,
        });

        // The next claim is judged against what this one left behind.
        self.claim_baseline = after;
        Ok(verdict)
    }

    /// Evaluate only the parts of the open claim's proof that run no commands.
    ///
    /// Cheap, and sound in one direction: if a command-free conjunct of the
    /// proof is false right now, the proof is false right now. Used to prune
    /// best-of-N branches without paying for a test run.
    pub fn structural_check(&self) -> Result<Option<bool>> {
        let Some(active) = &self.active else { return Ok(None) };
        let Some(structural) = active.predicate.structural_only()? else { return Ok(None) };

        let after = self.observe()?;
        let environment: Arc<dyn ProbeEnvironment> = Arc::new(CellEnvironment::new(
            Arc::clone(&self.cell),
            &self.claim_baseline,
            &after,
            self.policy.command_timeout_ms,
        ));
        Ok(Some(self.attestor.evaluate(&structural, environment)?))
    }

    /// The proof text of the open claim, if there is one.
    pub fn active_proof(&self) -> Option<&str> {
        self.active.as_ref().map(|a| a.source.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::scratch_workspace;

    #[test]
    fn small_output_is_inlined_and_large_output_becomes_a_handle() {
        let (_guards, ws) = scratch_workspace(&[("a.txt", "x")]);

        let small = ws.present(ArtifactKind::Stdout, b"3 passed").unwrap();
        assert_eq!(small, "3 passed");

        let large = ws.present(ArtifactKind::TestReport, &vec![b'y'; 200_000]).unwrap();
        assert!(large.starts_with("Handle(blake3:"), "{large}");
        assert!(large.contains("195.3 KB"));
        assert!(large.len() < 400, "a handle must stay context-sized: {large}");
    }

    #[test]
    fn declaring_twice_without_attesting_is_refused() {
        let (_guards, mut ws) = scratch_workspace(&[("a.txt", "x")]);
        ws.declare("first", "exit(git --version) == 0", ProofTier::Unit).unwrap();
        let second = ws.declare("second", "exit(git --version) == 0", ProofTier::Unit);
        assert!(matches!(second, Err(AgentError::Refused { .. })));
    }

    #[test]
    fn attesting_without_declaring_is_an_error_not_a_verdict() {
        let (guards, mut ws) = scratch_workspace(&[("a.txt", "x")]);
        assert!(matches!(ws.attest(&guards.probe_root()), Err(AgentError::NoActiveClaim)));
    }

    #[test]
    fn a_sealed_proof_reaches_the_record_before_any_further_work() {
        let (_guards, mut ws) = scratch_workspace(&[("a.txt", "x")]);
        ws.declare("something", "exit(git --version) == 0", ProofTier::Unit).unwrap();

        let kinds: Vec<_> = ws.ledger().entries().unwrap().iter().map(|e| e.kind).collect();
        assert!(kinds.contains(&EntryKind::PredicateSealed));
        assert!(kinds.contains(&EntryKind::ClaimDeclared));
    }

    #[test]
    fn history_rewriting_commands_are_refused_by_the_workspace() {
        let (_guards, mut ws) = scratch_workspace(&[("a.txt", "x")]);
        let refused = ws.exec("git reset --hard HEAD~2");
        assert!(matches!(refused, Err(AgentError::Refused { .. })));
    }

    #[test]
    fn the_blast_radius_reports_what_actually_changed() {
        let (guards, mut ws) = scratch_workspace(&[("src/a.txt", "one"), ("tests/t.txt", "two")]);
        assert!(ws.blast_radius().unwrap().is_empty());

        guards.write("src/a.txt", "changed");
        guards.write("tests/t.txt", "changed too");
        let radius = ws.blast_radius().unwrap();
        assert_eq!(radius.files_changed, 2);
        assert_eq!(radius.verification_paths, ["tests/t.txt"]);

        ws.accept().unwrap();
        assert!(ws.blast_radius().unwrap().is_empty(), "accepting resets the radius");
    }

    #[test]
    fn rolling_back_returns_the_cell_to_the_last_approved_state() {
        let (guards, mut ws) = scratch_workspace(&[("a.txt", "original")]);
        guards.write("a.txt", "modified");
        guards.write("new.txt", "added");
        assert_eq!(ws.blast_radius().unwrap().files_changed, 2);

        ws.roll_back().unwrap();
        assert_eq!(guards.read("a.txt"), "original");
        assert!(!guards.exists("new.txt"), "a rejected turn must not leave files behind");
        assert!(ws.blast_radius().unwrap().is_empty());
    }
}
