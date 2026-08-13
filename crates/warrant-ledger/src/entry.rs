//! Log entries.
//!
//! An entry is a hash-chained header over a content-addressed payload. The
//! header commits to its predecessor, so removing or reordering any entry
//! invalidates every entry after it — you cannot unwrite the middle of the
//! record and leave the ends intact.

use serde::{Deserialize, Serialize};
use warrant_core::Hash;

/// Domain tag for entry hashing.
const ENTRY_TAG: &str = "warrant.ledger.entry.v1";

/// What an entry records.
///
/// Deliberately closed. Adding a kind is a deliberate act with a matching
/// projection update; an open string field would let anything be recorded as
/// anything.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    /// A run began: repository, harness, arguments.
    RunStarted,
    /// A run ended, successfully or otherwise.
    RunFinished,
    /// A claim was declared. Written before any tool executes.
    ClaimDeclared,
    /// The compiled predicate module, stored so a third party can re-run it.
    PredicateSealed,
    /// A prompt sent to a model.
    ModelRequest,
    /// A model's response, verbatim.
    ModelResponse,
    /// A tool invocation and its arguments.
    ToolCall,
    /// A tool's result.
    ToolResult,
    /// A cell was created, with its isolation parameters.
    CellCreated,
    /// A content-addressed snapshot of a cell's filesystem.
    CellSnapshot,
    /// The overlay diff observed by the supervisor. Never model-reported.
    OverlayDiff,
    /// One necessity probe: the hunk subset applied and the bit that came back.
    Probe,
    /// A claim was discharged, or was not.
    Attested,
    /// A completed necessity map.
    NecessityMapped,
    /// A signed receipt was issued.
    ReceiptIssued,
    /// What git history looked like at a moment, so a later rewrite is
    /// detectable.
    ///
    /// Deliberately its own kind. Folding it into `RunStarted` would mean two
    /// unrelated payload shapes sharing a label, and anything reading the log
    /// for run headers would find repository state instead.
    RepoState,
    /// Repository history stopped matching what the ledger recorded.
    RepoDiverged,
    /// A claim that failed, kept with its evidence so it is not re-attempted.
    Refutation,
    /// A delta was cut down to the part its proof depends on, and the result
    /// was re-run against that proof.
    Trimmed,
    /// Free-form annotation from the operator.
    Note,
}

impl EntryKind {
    /// Stable name, used in the hash and in the ledger's textual form.
    pub fn name(&self) -> &'static str {
        match self {
            EntryKind::RunStarted => "run_started",
            EntryKind::RunFinished => "run_finished",
            EntryKind::ClaimDeclared => "claim_declared",
            EntryKind::PredicateSealed => "predicate_sealed",
            EntryKind::ModelRequest => "model_request",
            EntryKind::ModelResponse => "model_response",
            EntryKind::ToolCall => "tool_call",
            EntryKind::ToolResult => "tool_result",
            EntryKind::CellCreated => "cell_created",
            EntryKind::CellSnapshot => "cell_snapshot",
            EntryKind::OverlayDiff => "overlay_diff",
            EntryKind::Probe => "probe",
            EntryKind::Attested => "attested",
            EntryKind::NecessityMapped => "necessity_mapped",
            EntryKind::ReceiptIssued => "receipt_issued",
            EntryKind::RepoState => "repo_state",
            EntryKind::RepoDiverged => "repo_diverged",
            EntryKind::Refutation => "refutation",
            EntryKind::Trimmed => "trimmed",
            EntryKind::Note => "note",
        }
    }
}

/// One immutable record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// Position in the log. Starts at 0 and never skips.
    pub seq: u64,
    /// Address of the previous entry, or the zero address for the first.
    pub prev: Hash,
    /// Milliseconds since the Unix epoch. Recorded, so replay reproduces it
    /// rather than re-reading the clock.
    pub at_ms: u64,
    /// What this entry records.
    pub kind: EntryKind,
    /// Address of the payload in the blob store.
    pub payload: Hash,
    /// This entry's own address, over all the fields above.
    pub hash: Hash,
}

impl Entry {
    /// Compute the address an entry with these fields must have.
    pub fn compute_hash(
        seq: u64,
        prev: &Hash,
        at_ms: u64,
        kind: EntryKind,
        payload: &Hash,
    ) -> Hash {
        Hash::of_tagged(
            ENTRY_TAG,
            &[
                &seq.to_le_bytes(),
                prev.as_bytes(),
                &at_ms.to_le_bytes(),
                kind.name().as_bytes(),
                payload.as_bytes(),
            ],
        )
    }

    /// Build a sealed entry from its fields.
    pub fn seal(seq: u64, prev: Hash, at_ms: u64, kind: EntryKind, payload: Hash) -> Self {
        let hash = Entry::compute_hash(seq, &prev, at_ms, kind, &payload);
        Entry { seq, prev, at_ms, kind, payload, hash }
    }

    /// Whether the recorded address matches the fields.
    pub fn is_self_consistent(&self) -> bool {
        Entry::compute_hash(self.seq, &self.prev, self.at_ms, self.kind, &self.payload) == self.hash
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sealing_is_deterministic_and_self_consistent() {
        let e =
            Entry::seal(3, Hash::of(b"parent"), 1700, EntryKind::ToolCall, Hash::of(b"payload"));
        assert!(e.is_self_consistent());
        assert_eq!(
            e,
            Entry::seal(3, Hash::of(b"parent"), 1700, EntryKind::ToolCall, Hash::of(b"payload"))
        );
    }

    #[test]
    fn every_field_is_covered_by_the_address() {
        let base = Entry::seal(3, Hash::of(b"p"), 1700, EntryKind::ToolCall, Hash::of(b"x"));
        let variants = [
            Entry::seal(4, Hash::of(b"p"), 1700, EntryKind::ToolCall, Hash::of(b"x")),
            Entry::seal(3, Hash::of(b"q"), 1700, EntryKind::ToolCall, Hash::of(b"x")),
            Entry::seal(3, Hash::of(b"p"), 1701, EntryKind::ToolCall, Hash::of(b"x")),
            Entry::seal(3, Hash::of(b"p"), 1700, EntryKind::ToolResult, Hash::of(b"x")),
            Entry::seal(3, Hash::of(b"p"), 1700, EntryKind::ToolCall, Hash::of(b"y")),
        ];
        for v in variants {
            assert_ne!(base.hash, v.hash, "changing a field must change the address");
        }
    }

    #[test]
    fn relabelling_an_entry_breaks_self_consistency() {
        let mut e = Entry::seal(1, Hash::ZERO, 1, EntryKind::ToolCall, Hash::of(b"x"));
        assert!(e.is_self_consistent());
        e.kind = EntryKind::Note;
        assert!(!e.is_self_consistent(), "a relabelled entry must not verify");
    }

    #[test]
    fn kind_names_are_unique() {
        let kinds = [
            EntryKind::RunStarted,
            EntryKind::RunFinished,
            EntryKind::ClaimDeclared,
            EntryKind::PredicateSealed,
            EntryKind::ModelRequest,
            EntryKind::ModelResponse,
            EntryKind::ToolCall,
            EntryKind::ToolResult,
            EntryKind::CellCreated,
            EntryKind::CellSnapshot,
            EntryKind::OverlayDiff,
            EntryKind::Probe,
            EntryKind::Attested,
            EntryKind::NecessityMapped,
            EntryKind::ReceiptIssued,
            EntryKind::RepoState,
            EntryKind::RepoDiverged,
            EntryKind::Refutation,
            EntryKind::Note,
        ];
        let mut names: Vec<_> = kinds.iter().map(|k| k.name()).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "two kinds share a name; the hash would conflate them");
    }
}
