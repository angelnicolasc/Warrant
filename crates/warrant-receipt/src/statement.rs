//! The statement a receipt carries.
//!
//! Shaped as an in-toto attestation so that supply-chain tooling reads it
//! without modification, and written so that a person who was not there can
//! read it without knowing any of this project's vocabulary.
//!
//! Two things it deliberately does *not* say: that the work is correct, and
//! that the isolation was stronger than it was. The first is stated as an
//! explicit disclaimer in the statement itself; the second travels as the
//! cell's own [`IsolationReport`], recorded rather than summarised.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use warrant_cell::IsolationReport;
use warrant_core::{ClaimId, Hash, PredicateHash, ProofTier, format_rfc3339};
use warrant_ledger::Checkpoint;
use warrant_necessity::{MapOutcome, NecessityMap};

/// in-toto statement type.
pub const STATEMENT_TYPE: &str = "https://in-toto.io/Statement/v1";

/// Warrant's predicate type.
pub const PREDICATE_TYPE: &str = "https://warrant.dev/attestation/proof-map/v1";

/// What this build of Warrant is.
pub const TOOL_NAME: &str = "warrant";

/// The version of the tool that issued a receipt.
pub const TOOL_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Stated on every receipt, because the distinction is the one readers most
/// want to collapse.
pub const DISCLAIMER: &str = "Necessity is not sufficiency. This receipt states how much of the \
recorded change the declared proof depends on. It does not state that the change is correct.";

/// An artefact the statement is about.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subject {
    /// What it is.
    pub name: String,
    /// Its content address, keyed by algorithm.
    pub digest: BTreeMap<String, String>,
}

impl Subject {
    /// A subject naming a tree by its BLAKE3 root hash.
    pub fn tree(name: &str, root: Hash) -> Self {
        Subject {
            name: name.to_owned(),
            digest: BTreeMap::from([("blake3".to_owned(), root.to_hex())]),
        }
    }
}

/// Which build produced the receipt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tool {
    /// Tool name.
    pub name: String,
    /// Tool version.
    pub version: String,
}

impl Default for Tool {
    fn default() -> Self {
        Tool { name: TOOL_NAME.into(), version: TOOL_VERSION.into() }
    }
}

/// What was being done.
///
/// Has no `Default`. The two tree addresses are the evidence this whole
/// receipt hangs off, and a derived default would let one be silently zero.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunSummary {
    /// The task as the operator stated it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    /// Which agent did the work, when Warrant wrapped one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    /// The claim this maps, when one was declared.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claim: Option<ClaimId>,
    /// Tree address before the work.
    pub tree_before: Hash,
    /// Tree address after.
    pub tree_after: Hash,
}

impl RunSummary {
    /// A summary naming the two states the map was computed between.
    pub fn between(tree_before: Hash, tree_after: Hash) -> Self {
        RunSummary { task: None, harness: None, claim: None, tree_before, tree_after }
    }

    /// Record the task as the operator stated it.
    pub fn with_task(mut self, task: impl Into<String>) -> Self {
        self.task = Some(task.into());
        self
    }

    /// Record which agent did the work.
    pub fn with_harness(mut self, harness: impl Into<String>) -> Self {
        self.harness = Some(harness.into());
        self
    }

    /// Record the claim this maps.
    pub fn with_claim(mut self, claim: ClaimId) -> Self {
        self.claim = Some(claim);
        self
    }
}

/// The proof the work was judged against.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofSummary {
    /// Address of the sealed module.
    pub predicate: PredicateHash,
    /// The proof exactly as it was declared.
    pub source: String,
    /// The tier it was judged at.
    pub tier: ProofTier,
    /// Commands the proof may run.
    pub commands: Vec<String>,
    /// Whether the proof was written by the operator or defaulted from the
    /// repository's own test command.
    pub defaulted: bool,
}

/// What the search found.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutcomeSummary {
    /// How the search ended.
    pub outcome: MapOutcome,
    /// Whether the proof held on the result.
    pub satisfied: bool,
    /// Whether the proof failed on the starting state, as a real proof must.
    pub null_test_passed: bool,
    /// Proven share of changed lines, when the outcome admits one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coverage_percent: Option<u32>,
    /// Changed lines the proof depends on.
    pub proven_lines: u64,
    /// Changed lines in total.
    pub changed_lines: u64,
    /// Load-bearing hunks.
    pub load_bearing_hunks: usize,
    /// Hunks in total.
    pub total_hunks: usize,
    /// Probes run.
    pub probes: u32,
    /// Whether every load-bearing hunk was individually re-checked.
    pub minimality_confirmed: bool,
    /// Whether the search stopped early on budget.
    pub budget_exhausted: bool,
}

/// What a reader should look at.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Findings {
    /// Files where a load-bearing change sits on a test or a snapshot.
    pub laundered_verification: Vec<String>,
    /// Files that revert without the proof noticing.
    pub revert_safe: Vec<String>,
    /// How many hunks the proof contradicted itself about.
    pub proof_instability: usize,
}

impl Findings {
    /// Whether anything here needs a human.
    pub fn needs_attention(&self) -> bool {
        !self.laundered_verification.is_empty() || self.proof_instability > 0
    }
}

/// The body of a Warrant attestation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofMapPredicate {
    /// Which build issued this.
    pub tool: Tool,
    /// When, in UTC.
    pub issued_at: String,
    /// What was being done.
    pub run: RunSummary,
    /// The proof used.
    pub proof: ProofSummary,
    /// What the cell actually enforced while the evidence was collected.
    pub isolation: IsolationReport,
    /// What the search found.
    pub outcome: OutcomeSummary,
    /// What to look at.
    pub findings: Findings,
    /// The ledger head this receipt was issued against.
    ///
    /// A hash chain cannot detect that its own tail was cut off. This anchor
    /// can: a ledger that no longer extends it has had entries removed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ledger: Option<Checkpoint>,
    /// The limit of what this receipt claims.
    pub disclaimer: String,
}

/// A complete in-toto statement.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Statement {
    /// Always [`STATEMENT_TYPE`].
    #[serde(rename = "_type")]
    pub statement_type: String,
    /// What the statement is about.
    pub subject: Vec<Subject>,
    /// Always [`PREDICATE_TYPE`].
    #[serde(rename = "predicateType")]
    pub predicate_type: String,
    /// The Warrant body.
    pub predicate: ProofMapPredicate,
}

impl Statement {
    /// Build a statement from a completed map.
    pub fn from_map(
        map: &NecessityMap,
        proof: ProofSummary,
        isolation: IsolationReport,
        run: RunSummary,
        ledger: Option<Checkpoint>,
        issued_at_ms: u64,
    ) -> Self {
        let outcome = OutcomeSummary {
            outcome: map.outcome,
            satisfied: map.satisfied,
            null_test_passed: map.null_passed,
            // Reported only where it means something. A vacuous proof has no
            // coverage; writing zero would invite the reader to average it
            // with numbers that were actually measured.
            coverage_percent: map.outcome.has_coverage().then(|| map.coverage.percent()).flatten(),
            proven_lines: map.coverage.numerator,
            changed_lines: map.coverage.denominator,
            load_bearing_hunks: map.load_bearing.len(),
            total_hunks: map.load_bearing.len() + map.unproven.len(),
            probes: map.probes,
            minimality_confirmed: map.minimality_confirmed,
            budget_exhausted: map.budget_exhausted,
        };

        let findings = Findings {
            laundered_verification: map.tampered_files().map(|f| f.path.clone()).collect(),
            revert_safe: map.revert_safe_files().map(|f| f.path.clone()).collect(),
            proof_instability: map.monotonicity_violations.len(),
        };

        Statement {
            statement_type: STATEMENT_TYPE.into(),
            subject: vec![Subject::tree("repository-tree", map.post_root)],
            predicate_type: PREDICATE_TYPE.into(),
            predicate: ProofMapPredicate {
                tool: Tool::default(),
                issued_at: format_rfc3339(issued_at_ms),
                run,
                proof,
                isolation,
                outcome,
                findings,
                ledger,
                disclaimer: DISCLAIMER.into(),
            },
        }
    }

    /// Whether this statement is shaped as Warrant expects.
    pub fn is_well_formed(&self) -> bool {
        self.statement_type == STATEMENT_TYPE && self.predicate_type == PREDICATE_TYPE
    }

    /// A plain-language rendering for someone who was not there.
    pub fn summary(&self) -> String {
        let p = &self.predicate;
        let mut lines = Vec::new();

        lines.push(format!("{} {} issued this on {}.", p.tool.name, p.tool.version, p.issued_at));
        if let Some(task) = &p.run.task {
            lines.push(format!("Task: {task}"));
        }
        if let Some(harness) = &p.run.harness {
            lines.push(format!("Agent: {harness}"));
        }
        lines.push(format!("Proof: {}", p.proof.source));

        match p.outcome.outcome {
            MapOutcome::Mapped => {
                let percent = p.outcome.coverage_percent.unwrap_or(0);
                lines.push(format!(
                    "The proof holds, and it depends on {percent}% of the changed lines \
                     ({} of {}), spread over {} of {} hunks.",
                    p.outcome.proven_lines,
                    p.outcome.changed_lines,
                    p.outcome.load_bearing_hunks,
                    p.outcome.total_hunks
                ));
            }
            MapOutcome::Vacuous => lines.push(
                "The proof already held before any work was done, so it proves nothing about \
                 this change."
                    .into(),
            ),
            MapOutcome::NotSatisfied => {
                lines.push("The proof does not hold on this result.".into())
            }
            MapOutcome::NoChanges => lines.push("Nothing changed on disk.".into()),
            MapOutcome::UnstableProof => lines.push(
                "The proof gave different answers on identical state, so no map was produced."
                    .into(),
            ),
        }

        if !p.findings.laundered_verification.is_empty() {
            lines.push(format!(
                "Look here first: the proof depends on changes to {}, which is where the \
                 verification lives. Part of why this passes is the change to the test.",
                p.findings.laundered_verification.join(", ")
            ));
        }
        if !p.findings.revert_safe.is_empty() {
            lines.push(format!(
                "These reverted without the proof noticing: {}.",
                p.findings.revert_safe.join(", ")
            ));
        }

        lines.push(format!(
            "Isolation: filesystem {}, network {}, process {} ({} backend).",
            p.isolation.filesystem.label(),
            p.isolation.network.label(),
            p.isolation.process.label(),
            p.isolation.backend
        ));
        lines.push(p.disclaimer.clone());
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use warrant_cell::IsolationLevel;
    use warrant_core::Ratio;
    use warrant_diff::ChangeKind;
    use warrant_necessity::{FileVerdict, NecessityMap};

    fn isolation() -> IsolationReport {
        IsolationReport {
            backend: "workspace".into(),
            filesystem: IsolationLevel::Directory,
            network: IsolationLevel::None,
            process: IsolationLevel::None,
            caveats: vec!["Network egress is neither restricted nor recorded.".into()],
        }
    }

    fn proof() -> ProofSummary {
        ProofSummary {
            predicate: PredicateHash::derive(&[b"p"]),
            source: "exit(pytest) == 0".into(),
            tier: ProofTier::Unit,
            commands: vec!["pytest".into()],
            defaulted: true,
        }
    }

    fn mapped_with(outcome: MapOutcome, coverage: Ratio, tampered: &[&str]) -> NecessityMap {
        let mut map = NecessityMap::no_changes(PredicateHash::derive(&[b"p"]), Hash::of(b"tree"));
        map.outcome = outcome;
        map.satisfied = true;
        map.null_passed = true;
        map.coverage = coverage;
        map.files = tampered
            .iter()
            .map(|path| FileVerdict {
                path: (*path).to_owned(),
                change: ChangeKind::Modified,
                total_hunks: 1,
                load_bearing_hunks: 1,
                changed_lines: 2,
                proven_lines: 2,
                verification_surface: true,
                tampered: true,
            })
            .collect();
        map
    }

    #[test]
    fn a_statement_is_shaped_as_in_toto_expects() {
        let map = mapped_with(MapOutcome::Mapped, Ratio::new(14, 37), &[]);
        let statement = Statement::from_map(
            &map,
            proof(),
            isolation(),
            RunSummary::between(Hash::of(b"before"), Hash::of(b"after")),
            None,
            1_786_492_800_000,
        );

        assert!(statement.is_well_formed());
        let json = serde_json::to_value(&statement).unwrap();
        assert_eq!(json["_type"], STATEMENT_TYPE);
        assert_eq!(json["predicateType"], PREDICATE_TYPE);
        assert!(json["subject"][0]["digest"]["blake3"].is_string());
    }

    #[test]
    fn a_vacuous_map_carries_no_coverage_figure_at_all() {
        let map = mapped_with(MapOutcome::Vacuous, Ratio::UNDEFINED, &[]);
        let statement = Statement::from_map(
            &map,
            proof(),
            isolation(),
            RunSummary::between(Hash::of(b"before"), Hash::of(b"after")),
            None,
            0,
        );

        assert_eq!(statement.predicate.outcome.coverage_percent, None);
        let json = serde_json::to_value(&statement).unwrap();
        assert!(
            json["predicate"]["outcome"].get("coverage_percent").is_none(),
            "an unmeasured number must be absent, not zero"
        );
    }

    #[test]
    fn the_summary_leads_with_the_laundered_green() {
        let map = mapped_with(MapOutcome::Mapped, Ratio::new(2, 20), &["tests/test_upload.py"]);
        let statement = Statement::from_map(
            &map,
            proof(),
            isolation(),
            RunSummary::between(Hash::of(b"before"), Hash::of(b"after")),
            None,
            0,
        );

        let summary = statement.summary();
        assert!(summary.contains("Look here first"));
        assert!(summary.contains("tests/test_upload.py"));
        assert!(summary.contains("the change to the test"));
    }

    #[test]
    fn the_summary_states_the_isolation_that_was_actually_enforced() {
        let map = mapped_with(MapOutcome::Mapped, Ratio::new(1, 2), &[]);
        let statement = Statement::from_map(
            &map,
            proof(),
            isolation(),
            RunSummary::between(Hash::of(b"before"), Hash::of(b"after")),
            None,
            0,
        );
        assert!(statement.summary().contains("network none"));
    }

    #[test]
    fn every_receipt_states_the_limit_of_what_it_claims() {
        let map = mapped_with(MapOutcome::Mapped, Ratio::new(1, 2), &[]);
        let statement = Statement::from_map(
            &map,
            proof(),
            isolation(),
            RunSummary::between(Hash::of(b"before"), Hash::of(b"after")),
            None,
            0,
        );
        assert!(statement.summary().contains("Necessity is not sufficiency"));
        assert!(
            statement.predicate.disclaimer.contains("does not state that the change is correct")
        );
    }

    #[test]
    fn the_proof_travels_verbatim_so_a_reader_can_re_run_it() {
        let map = mapped_with(MapOutcome::Mapped, Ratio::new(1, 2), &[]);
        let statement = Statement::from_map(
            &map,
            proof(),
            isolation(),
            RunSummary::between(Hash::of(b"before"), Hash::of(b"after")),
            None,
            0,
        );
        assert_eq!(statement.predicate.proof.source, "exit(pytest) == 0");
        assert_eq!(statement.predicate.proof.commands, ["pytest"]);
    }
}
