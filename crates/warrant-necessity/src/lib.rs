//! The necessity map.
//!
//! > Revert each hunk. Re-run the agent's own proof. Whatever doesn't break
//! > was never proven.
//!
//! Formal verification named the underlying problem twenty-five years ago. A
//! specification is satisfied *vacuously* when it holds for trivial reasons
//! rather than because the intended behaviour was exercised — the canonical
//! case being antecedent failure, where "every request is followed by a
//! grant" passes in a model that never sends requests. Vacuity checking has
//! been standard in commercial model checkers for two decades. This crate
//! applies it to agent claims.
//!
//! The output is not a judgement about whether the work is correct. It is a
//! statement about how much of the work the declared proof actually depends
//! on — and **necessity is not sufficiency**. A load-bearing hunk is proven
//! relative to that proof and nothing more.
//!
//! # What the search does
//!
//! 1. **Satisfaction.** The proof must hold on the agent's result, twice, so
//!    that an unstable proof is reported as unstable rather than mapped.
//! 2. **Null test.** The proof must *fail* on the state before the agent
//!    started. If it already held, it proves nothing, and coverage is
//!    reported as undefined rather than as zero.
//! 3. **Delta debugging.** Binary partitioning over hunks to find a subset
//!    the proof still passes with — `O(log n)` probes in the ordinary case.
//! 4. **Confirmation.** Every surviving hunk is reverted individually, so the
//!    map's per-hunk claim has a probe behind it.
//!
//! Three findings fall out of the same mechanism: coverage near zero means
//! the proof or the work was vacuous; a large unproven region means scope
//! creep; and a load-bearing hunk inside a test file means the change that
//! made the proof pass was the change to the test.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod config;
pub mod ddmin;
mod error;
mod map;
mod search;

pub use config::{
    DEFAULT_SNAPSHOT_PATTERNS, DEFAULT_TEST_PATTERNS, NecessityConfig, PathClassifier,
    default_parallelism,
};
pub use ddmin::{
    Minimality, ProbeBudget, confirm_minimal, confirm_minimal_wide, ddmin, ddmin_wide,
};
pub use error::{NecessityError, Result};
pub use map::{FileVerdict, MapOutcome, NecessityMap};
pub use search::Search;
