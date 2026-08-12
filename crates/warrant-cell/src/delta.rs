//! The delta — environment-derived evidence.
//!
//! Invariant 1 of the architecture record: *no code path constructs a `Delta`
//! from model output.* That is enforced here by the compiler rather than by
//! review.
//!
//! [`Delta`]'s fields are private and its constructor is crate-private.
//! The only public way to obtain one is [`Supervisor::observe`], which does
//! not accept a diff — it accepts two [`CellSnapshot`]s and computes the diff
//! itself. A `CellSnapshot` in turn has a crate-private constructor and is
//! produced only by [`Cell::snapshot`], and [`Cell`] is a sealed trait. There
//! is no thread from "something the model said" to "a value of type `Delta`".
//!
//! The three tests below are `compile_fail` doctests: they are compiled by
//! `cargo test` and the suite fails if any of them ever starts compiling.
//!
//! A `Delta` cannot be built field by field:
//!
//! ```compile_fail
//! # use warrant_cell::Delta;
//! let forged = Delta { overlay: todo!(), exits: vec![], egress: vec![] };
//! ```
//!
//! A snapshot cannot be conjured from a value the model supplied:
//!
//! ```compile_fail
//! # use warrant_cell::CellSnapshot;
//! # use warrant_diff::Snapshot;
//! let claimed_state = Snapshot::default();
//! let forged = CellSnapshot(claimed_state);
//! ```
//!
//! And a new backend cannot be declared outside this crate in order to
//! produce snapshots that were never observed:
//!
//! ```compile_fail
//! # use warrant_cell::Cell;
//! struct AgentReportedCell;
//! impl Cell for AgentReportedCell {}
//! ```

use serde::{Deserialize, Serialize};
use warrant_diff::{ContentStore, OverlayDiff, Snapshot};

use crate::cell::{Cell, CellSnapshot, IsolationReport};
use crate::error::Result;
use crate::exec::ExitRecord;

/// One outbound network connection a cell observed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetRecord {
    /// Destination host, as requested.
    pub host: String,
    /// Destination port.
    pub port: u16,
    /// Whether the egress policy permitted it.
    pub allowed: bool,
}

/// What a cell was able to see of the syscalls made inside it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyscallSummary {
    /// Whether syscall observation was available at all.
    ///
    /// `false` on backends without a supervision hook, and reported as
    /// unavailable rather than as zero — an absent measurement and a
    /// measurement of nothing are different claims.
    pub observed: bool,
    /// Calls the policy refused.
    pub denied: Vec<String>,
    /// Total calls seen, when observation was available.
    pub total: u64,
}

/// Everything the supervisor observed about a piece of work.
///
/// Fields are private on purpose. Read them through the accessors; there is
/// no way to write them from outside this crate.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Delta {
    overlay: OverlayDiff,
    exits: Vec<ExitRecord>,
    syscalls: SyscallSummary,
    egress: Vec<NetRecord>,
    isolation: IsolationReport,
}

impl Delta {
    pub(crate) fn new(
        overlay: OverlayDiff,
        exits: Vec<ExitRecord>,
        syscalls: SyscallSummary,
        egress: Vec<NetRecord>,
        isolation: IsolationReport,
    ) -> Self {
        Delta { overlay, exits, syscalls, egress, isolation }
    }

    /// What changed on disk, computed by comparing observations.
    pub fn overlay(&self) -> &OverlayDiff {
        &self.overlay
    }

    /// Every command that ran, with its exit status and output addresses.
    pub fn exits(&self) -> &[ExitRecord] {
        &self.exits
    }

    /// What the syscall layer saw, or that it saw nothing.
    pub fn syscalls(&self) -> &SyscallSummary {
        &self.syscalls
    }

    /// Outbound connections observed.
    pub fn egress(&self) -> &[NetRecord] {
        &self.egress
    }

    /// The isolation this evidence was collected under.
    ///
    /// Carried with the delta so a receipt can state what was actually
    /// enforced rather than what the architecture is capable of.
    pub fn isolation(&self) -> &IsolationReport {
        &self.isolation
    }

    /// Whether anything changed on disk.
    pub fn is_empty(&self) -> bool {
        self.overlay.is_empty()
    }
}

/// The sole source of [`Delta`] values.
#[derive(Debug, Default)]
pub struct Supervisor {
    syscalls: SyscallSummary,
    egress: Vec<NetRecord>,
}

impl Supervisor {
    /// A supervisor with no syscall or egress observation.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record an observed outbound connection.
    pub fn record_egress(&mut self, record: NetRecord) {
        self.egress.push(record);
    }

    /// Record what the syscall layer saw.
    pub fn record_syscalls(&mut self, summary: SyscallSummary) {
        self.syscalls = summary;
    }

    /// Produce the delta between two observations of a cell.
    ///
    /// Note the signature: this takes snapshots the cell handed out, not a
    /// diff someone computed. The overlay is derived here and nowhere else.
    pub fn observe(
        &self,
        cell: &dyn Cell,
        before: &CellSnapshot,
        after: &CellSnapshot,
        exits: Vec<ExitRecord>,
        store: &dyn ContentStore,
    ) -> Result<Delta> {
        let overlay = OverlayDiff::between(before.as_snapshot(), after.as_snapshot(), store)?;
        Ok(Delta::new(overlay, exits, self.syscalls.clone(), self.egress.clone(), cell.isolation()))
    }

    /// The pre-image a delta was computed against, for callers that need to
    /// re-materialise it — the necessity search, above all.
    pub fn pre_image<'a>(&self, before: &'a CellSnapshot) -> &'a Snapshot {
        before.as_snapshot()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unobserved_syscall_layer_reports_unavailability_not_zero() {
        let summary = SyscallSummary::default();
        assert!(!summary.observed);
        assert_eq!(summary.total, 0);
        // The pairing is the point: a caller must check `observed` before
        // reading `total`, and the type makes that visible.
        let json = serde_json::to_value(&summary).unwrap();
        assert_eq!(json["observed"], serde_json::json!(false));
    }
}
