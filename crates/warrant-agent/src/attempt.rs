//! Best-of-N, adjudicated rather than presented.
//!
//! Parallel sampling beats every clever harness trick at matched compute, and
//! the open question it leaves is **selection** (arXiv 2607.12227). A
//! pre-registered proof plus a coverage figure is a general selector, which is
//! what makes this worth building here rather than anywhere else.
//!
//! The difference from showing a reviewer five diffs is the whole point: five
//! agents and five diffs is five times the review. Five agents and one proven
//! answer is less review than one agent, because the branches that could not
//! discharge the claim never reach a person.
//!
//! # How a winner is chosen
//!
//! Every branch is bound to the **same** claim, declared by the harness before
//! any of them start. Without that they would be judged by proofs of their own
//! choosing and the comparison would mean nothing. Then, in order:
//!
//! 1. the claim was discharged — branches that failed are not candidates;
//! 2. higher proof coverage;
//! 3. fewer changed lines;
//! 4. lowest index, so the result is deterministic.
//!
//! Coverage is used to *select*, never fed back to a model. Invariant 3 holds
//! here exactly as it does everywhere else.

use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::json;
use warrant_core::{ProofTier, Ratio, now_ms};
use warrant_diff::Snapshot;
use warrant_ledger::EntryKind;

use crate::error::Result;
use crate::policy::Approver;
use crate::provider::Provider;
use crate::session::{Session, SessionConfig, SessionOutcome, StopCondition};
use crate::workspace::{Services, Workspace};

/// How a best-of-N run is set up.
#[derive(Clone, Debug)]
pub struct AttemptConfig {
    /// How many branches to run.
    pub attempts: u32,
    /// What every branch is claiming.
    pub assertion: String,
    /// The proof every branch is judged by.
    pub proof: String,
    /// Tier the claim is judged at.
    pub tier: ProofTier,
    /// Consecutive off-track turns before a branch is abandoned.
    pub prune_patience: Option<u32>,
}

impl AttemptConfig {
    /// N attempts at one claim.
    pub fn new(attempts: u32, assertion: impl Into<String>, proof: impl Into<String>) -> Self {
        AttemptConfig {
            attempts: attempts.max(1),
            assertion: assertion.into(),
            proof: proof.into(),
            tier: ProofTier::Unit,
            prune_patience: Some(3),
        }
    }
}

/// What one branch did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attempt {
    /// Which branch.
    pub index: u32,
    /// Whether it discharged the claim.
    pub warranted: bool,
    /// Proven share of its changed lines.
    pub coverage: Ratio,
    /// How much it changed.
    pub changed_lines: u64,
    /// Why its session ended.
    pub stop: StopCondition,
    /// Turns it took.
    pub turns: u32,
    /// Tokens it spent.
    pub tokens: u64,
}

impl Attempt {
    /// Whether this branch is eligible to win.
    pub fn is_candidate(&self) -> bool {
        self.warranted
    }

    /// A line for the terminal.
    pub fn summary(&self) -> String {
        if self.warranted {
            format!(
                "attempt {}: warranted, coverage {}, {} changed lines, {} turns",
                self.index, self.coverage, self.changed_lines, self.turns
            )
        } else {
            format!("attempt {}: unproven — {}", self.index, self.stop.describe())
        }
    }
}

/// The result of running N branches.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Adjudication {
    /// Index of the branch that won, if any did.
    pub winner: Option<u32>,
    /// Every branch, in order.
    pub attempts: Vec<Attempt>,
}

impl Adjudication {
    /// The winning branch.
    pub fn winning_attempt(&self) -> Option<&Attempt> {
        self.winner.and_then(|index| self.attempts.iter().find(|a| a.index == index))
    }

    /// How many branches discharged the claim.
    pub fn warranted_count(&self) -> usize {
        self.attempts.iter().filter(|a| a.warranted).count()
    }

    /// Total tokens across every branch, which is the number to report a
    /// best-of-N result against.
    ///
    /// Comparing five attempts to somebody else's one is the benchmark
    /// theatre this project objects to, so the cost travels with the result.
    pub fn total_tokens(&self) -> u64 {
        self.attempts.iter().map(|a| a.tokens).sum()
    }
}

/// Pick a winner from finished branches.
///
/// Split out from the running so it can be tested against inputs that never
/// touched a model or a filesystem.
pub fn adjudicate(attempts: &[Attempt]) -> Option<u32> {
    attempts
        .iter()
        .filter(|attempt| attempt.is_candidate())
        .min_by(|a, b| {
            // Higher coverage first, then a smaller diff, then the lowest
            // index so that two equally good branches resolve the same way
            // every time.
            let coverage =
                b.coverage.as_f64().unwrap_or(0.0).total_cmp(&a.coverage.as_f64().unwrap_or(0.0));
            coverage.then(a.changed_lines.cmp(&b.changed_lines)).then(a.index.cmp(&b.index))
        })
        .map(|attempt| attempt.index)
}

/// Runs N branches from one starting state and adjudicates between them.
pub struct BestOfN<'a> {
    provider: &'a dyn Provider,
    approver: &'a dyn Approver,
    services: Services,
    session: SessionConfig,
    root: PathBuf,
}

impl<'a> BestOfN<'a> {
    /// Prepare a run.
    pub fn new(
        provider: &'a dyn Provider,
        approver: &'a dyn Approver,
        services: Services,
        session: SessionConfig,
        root: impl Into<PathBuf>,
    ) -> Self {
        BestOfN { provider, approver, services, session, root: root.into() }
    }

    /// Run every branch and pick a winner.
    ///
    /// Branches run one after another. They are fully independent — separate
    /// cells, separate claims of the same shape — so running them
    /// concurrently is a scheduling change rather than a design change; doing
    /// it sequentially keeps the record and the adjudication deterministic,
    /// which is what makes a result reproducible.
    pub fn run(
        &self,
        origin: &Snapshot,
        task: &str,
        config: &AttemptConfig,
    ) -> Result<(Adjudication, Vec<Snapshot>)> {
        self.services.ledger.append_json(
            EntryKind::RunStarted,
            &json!({
                "mode": "best-of-n",
                "attempts": config.attempts,
                "assertion": config.assertion,
                "proof": config.proof,
            }),
            now_ms(),
        )?;

        let mut attempts = Vec::new();
        let mut states = Vec::new();

        for index in 0..config.attempts {
            let (attempt, state) = self.run_one(index, origin, task, config)?;
            attempts.push(attempt);
            states.push(state);
        }

        let winner = adjudicate(&attempts);
        let adjudication = Adjudication { winner, attempts };
        self.services.ledger.append_json(EntryKind::Attested, &adjudication, now_ms())?;

        // Every branch that did not win stays in the record with its evidence,
        // so a later session does not re-attempt an approach already known to
        // fail here.
        for attempt in &adjudication.attempts {
            if Some(attempt.index) != adjudication.winner {
                self.services.ledger.append_json(EntryKind::Refutation, attempt, now_ms())?;
            }
        }
        Ok((adjudication, states))
    }

    fn run_one(
        &self,
        index: u32,
        origin: &Snapshot,
        task: &str,
        config: &AttemptConfig,
    ) -> Result<(Attempt, Snapshot)> {
        let cell_root = self.root.join(format!("attempt-{index}"));
        let cell = crate::probe_cell(&cell_root, origin, Arc::clone(&self.services.store))?;

        let mut workspace = Workspace::new(cell, self.services.clone())?;

        // The harness declares the claim, not the branch. Every branch is
        // therefore judged by the same proof, which is the only way the
        // comparison between them means anything.
        workspace.declare(&config.assertion, &config.proof, config.tier)?;

        let mut session_config = self.session.clone();
        session_config.prune_patience = config.prune_patience;
        session_config.probe_root = cell_root.join("probes");
        session_config.delegate_root = cell_root.join("delegates");
        session_config.system = format!(
            "{}\n\nA claim has already been declared for you:\n  {}\nwith the proof:\n  {}\n\
             Do the work and call `attest` when it is done.",
            self.session.system, config.assertion, config.proof
        );

        let mut session = Session::new(self.provider, self.approver, workspace, session_config);
        let outcome: SessionOutcome = session.run(task)?;

        // A branch that stopped without judging itself is judged anyway.
        // Leaving it unmeasured would let "ran out of turns" masquerade as
        // "not applicable".
        let probe_root = cell_root.join("final-probe");
        if session.workspace().has_active_claim() {
            let _ = session.workspace_mut().attest(&probe_root)?;
        }

        let judged = session.workspace().discharged().last().cloned();
        let state = session.workspace().observe()?;

        let attempt = Attempt {
            index,
            warranted: judged.as_ref().is_some_and(|claim| claim.warranted),
            coverage: judged.as_ref().map_or(Ratio::UNDEFINED, |claim| claim.map.coverage),
            changed_lines: judged.as_ref().map_or(0, |claim| claim.map.coverage.denominator),
            stop: outcome.stop,
            turns: outcome.turns,
            tokens: outcome.usage.total(),
        };
        Ok((attempt, state))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attempt(index: u32, warranted: bool, proven: u64, total: u64) -> Attempt {
        Attempt {
            index,
            warranted,
            coverage: Ratio::new(proven, total),
            changed_lines: total,
            stop: StopCondition::Finished,
            turns: 3,
            tokens: 100,
        }
    }

    #[test]
    fn only_a_branch_that_discharged_the_claim_can_win() {
        let attempts = [attempt(0, false, 100, 100), attempt(1, true, 10, 100)];
        assert_eq!(adjudicate(&attempts), Some(1));
    }

    #[test]
    fn nothing_wins_when_nothing_was_proven() {
        let attempts = [attempt(0, false, 90, 100), attempt(1, false, 80, 100)];
        assert_eq!(adjudicate(&attempts), None);
    }

    #[test]
    fn higher_coverage_wins() {
        let attempts =
            [attempt(0, true, 20, 100), attempt(1, true, 80, 100), attempt(2, true, 50, 100)];
        assert_eq!(adjudicate(&attempts), Some(1));
    }

    #[test]
    fn a_smaller_diff_breaks_a_tie_on_coverage() {
        let attempts = [attempt(0, true, 40, 80), attempt(1, true, 20, 40)];
        // Both are 50% proven; the second changed half as much.
        assert_eq!(adjudicate(&attempts), Some(1));
    }

    #[test]
    fn adjudication_is_deterministic_when_branches_are_indistinguishable() {
        let attempts =
            [attempt(2, true, 50, 100), attempt(0, true, 50, 100), attempt(1, true, 50, 100)];
        assert_eq!(adjudicate(&attempts), Some(0));
        assert_eq!(adjudicate(&attempts), Some(0));
    }

    #[test]
    fn an_undefined_coverage_loses_to_a_measured_one() {
        let mut vacuous = attempt(0, true, 0, 0);
        vacuous.coverage = Ratio::UNDEFINED;
        let attempts = [vacuous, attempt(1, true, 1, 100)];
        assert_eq!(adjudicate(&attempts), Some(1));
    }

    #[test]
    fn the_cost_of_every_branch_travels_with_the_result() {
        let adjudication = Adjudication {
            winner: Some(1),
            attempts: vec![attempt(0, false, 0, 10), attempt(1, true, 5, 10)],
        };
        assert_eq!(adjudication.total_tokens(), 200);
        assert_eq!(adjudication.warranted_count(), 1);
        assert_eq!(adjudication.winning_attempt().unwrap().index, 1);
    }

    #[test]
    fn a_summary_reads_differently_for_a_failure() {
        assert!(attempt(0, true, 5, 10).summary().contains("warranted"));
        let mut failed = attempt(1, false, 0, 10);
        failed.stop = StopCondition::OffTrack { turns: 3 };
        assert!(failed.summary().contains("unproven"));
        assert!(failed.summary().contains("proof was false"));
    }
}
