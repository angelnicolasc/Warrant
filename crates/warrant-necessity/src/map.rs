//! The map.
//!
//! Note what this type does not do: implement `warrant_core::ContextRenderable`.
//! Invariant 3 of the architecture record says the necessity map is never an
//! input to the agent's context, because a coverage figure fed back to the
//! party being measured becomes the next thing to saturate. The enforcement
//! is the absent impl, and the test below is what keeps it absent.
//!
//! ```compile_fail
//! # use warrant_core::ModelContext;
//! # use warrant_necessity::NecessityMap;
//! fn leak(map: &NecessityMap, ctx: &mut ModelContext) {
//!     ctx.push(map);
//! }
//! ```

use serde::{Deserialize, Serialize};
use warrant_core::{ClaimId, Hash, HunkId, PredicateHash, Ratio};
use warrant_diff::ChangeKind;

/// How the search ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MapOutcome {
    /// A map was produced.
    Mapped,
    /// The agent changed nothing, so there was nothing to map.
    NoChanges,
    /// The proof did not hold on the agent's result. Nothing is proven, and
    /// the claim is not discharged.
    NotSatisfied,
    /// The proof already held before the agent touched anything.
    ///
    /// Antecedent failure, in the vocabulary of vacuity checking: the
    /// specification is satisfied for trivial reasons rather than because the
    /// intended behaviour was exercised. Coverage is meaningless here, and is
    /// reported as undefined rather than as zero.
    Vacuous,
    /// The proof gave different answers on identical state.
    ///
    /// Delta debugging assumes a stable predicate. Rather than produce a map
    /// built on probes that contradict each other, the search stops and says
    /// so — the instability is the finding.
    UnstableProof,
}

impl MapOutcome {
    /// Whether a coverage figure means anything for this outcome.
    pub fn has_coverage(&self) -> bool {
        matches!(self, MapOutcome::Mapped)
    }

    /// A one-line explanation for the terminal.
    pub fn describe(&self) -> &'static str {
        match self {
            MapOutcome::Mapped => "mapped",
            MapOutcome::NoChanges => "nothing changed on disk",
            MapOutcome::NotSatisfied => "the proof does not hold on this result",
            MapOutcome::Vacuous => "the proof already held before any work was done",
            MapOutcome::UnstableProof => "the proof answered differently on identical state",
        }
    }
}

/// One file's standing in the map.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileVerdict {
    /// Repo-relative path.
    pub path: String,
    /// What happened to the file.
    pub change: ChangeKind,
    /// Hunks in this file.
    pub total_hunks: usize,
    /// Hunks that turned out to be load-bearing.
    pub load_bearing_hunks: usize,
    /// Changed lines in this file.
    pub changed_lines: u64,
    /// Changed lines that are load-bearing.
    pub proven_lines: u64,
    /// Whether the path is a test or a recorded expectation.
    pub verification_surface: bool,
    /// Whether a load-bearing hunk sits on a verification surface.
    ///
    /// This is the signature the whole project is named for: part of why the
    /// proof passes is that the agent changed the thing doing the proving.
    pub tampered: bool,
}

impl FileVerdict {
    /// Proven share of this file's changed lines.
    pub fn coverage(&self) -> Ratio {
        Ratio::new(self.proven_lines, self.changed_lines)
    }

    /// Whether every hunk here reverts without breaking the proof.
    pub fn is_entirely_unproven(&self) -> bool {
        self.load_bearing_hunks == 0
    }
}

/// The result of a necessity search.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NecessityMap {
    /// Which claim this maps, when the run declared one.
    pub claim: Option<ClaimId>,
    /// Address of the sealed proof used.
    pub predicate: PredicateHash,
    /// How the search ended.
    pub outcome: MapOutcome,
    /// Whether the proof held on the agent's result.
    pub satisfied: bool,
    /// Whether the null test passed — that is, whether the proof *failed* on
    /// the pre-state, as a non-vacuous proof must.
    pub null_passed: bool,
    /// Hunks reverting any one of which breaks the proof.
    pub load_bearing: Vec<HunkId>,
    /// Hunks that revert without the proof noticing.
    pub unproven: Vec<HunkId>,
    /// Load-bearing hunks sitting on a verification surface.
    pub tamper: Vec<HunkId>,
    /// Proven share of changed lines. The standing number.
    pub coverage: Ratio,
    /// Proven share of hunks, which is what the per-file bars show.
    pub hunk_coverage: Ratio,
    /// Per-file breakdown, sorted by path.
    pub files: Vec<FileVerdict>,
    /// How many probes the search actually ran.
    pub probes: u32,
    /// Whether the probe budget ran out before the search finished.
    pub budget_exhausted: bool,
    /// Whether every load-bearing hunk was individually re-checked.
    pub minimality_confirmed: bool,
    /// Hunks the confirmation pass had to drop.
    ///
    /// Non-empty means the proof contradicted itself across probes — a
    /// monotonicity violation, usually a flaky suite.
    pub monotonicity_violations: Vec<HunkId>,
    /// Tree address before the agent ran.
    pub pre_root: Hash,
    /// Tree address after.
    pub post_root: Hash,
}

impl NecessityMap {
    /// A map for a run that changed nothing.
    pub fn no_changes(predicate: PredicateHash, root: Hash) -> Self {
        NecessityMap {
            claim: None,
            predicate,
            outcome: MapOutcome::NoChanges,
            satisfied: false,
            null_passed: false,
            load_bearing: Vec::new(),
            unproven: Vec::new(),
            tamper: Vec::new(),
            coverage: Ratio::UNDEFINED,
            hunk_coverage: Ratio::UNDEFINED,
            files: Vec::new(),
            probes: 0,
            budget_exhausted: false,
            minimality_confirmed: false,
            monotonicity_violations: Vec::new(),
            pre_root: root,
            post_root: root,
        }
    }

    /// Whether any load-bearing hunk sits on a verification surface.
    pub fn has_tampering(&self) -> bool {
        !self.tamper.is_empty()
    }

    /// Files nothing in the proof depends on.
    pub fn revert_safe_files(&self) -> impl Iterator<Item = &FileVerdict> {
        self.files.iter().filter(|f| f.is_entirely_unproven())
    }

    /// Files carrying a laundered green.
    pub fn tampered_files(&self) -> impl Iterator<Item = &FileVerdict> {
        self.files.iter().filter(|f| f.tampered)
    }

    /// Whether this map is worth reading as a measurement.
    pub fn is_measurement(&self) -> bool {
        self.outcome.has_coverage()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_vacuous_outcome_reports_undefined_coverage_not_zero() {
        assert!(!MapOutcome::Vacuous.has_coverage());
        assert!(!MapOutcome::NotSatisfied.has_coverage());
        assert!(!MapOutcome::UnstableProof.has_coverage());
        assert!(MapOutcome::Mapped.has_coverage());
    }

    #[test]
    fn a_map_over_no_changes_carries_no_number() {
        let map = NecessityMap::no_changes(PredicateHash::derive(&[b"p"]), Hash::of(b"tree"));
        assert_eq!(map.coverage, Ratio::UNDEFINED);
        assert_eq!(map.coverage.to_string(), "n/a");
        assert!(!map.is_measurement());
    }

    #[test]
    fn a_file_with_no_load_bearing_hunks_is_revert_safe() {
        let verdict = FileVerdict {
            path: "src/utils/cache.py".into(),
            change: ChangeKind::Modified,
            total_hunks: 4,
            load_bearing_hunks: 0,
            changed_lines: 40,
            proven_lines: 0,
            verification_surface: false,
            tampered: false,
        };
        assert!(verdict.is_entirely_unproven());
        assert_eq!(verdict.coverage().percent(), Some(0));
    }
}
