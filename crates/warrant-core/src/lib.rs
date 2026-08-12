//! Shared primitives for Warrant.
//!
//! This crate holds the vocabulary every other layer speaks, and the three
//! invariants from the architecture record that are worth enforcing in the
//! type system rather than in review:
//!
//! 1. **No code path constructs a `Delta` from model output.** Enforced in
//!    `warrant-cell`: `Delta`'s constructor is crate-private and the cell
//!    supervisor is the only caller. A `compile_fail` test proves an external
//!    crate cannot build one.
//! 2. **A [`Verdict`] carries no numeric field.** Enforced here, by a test
//!    that walks the serialised form and rejects any number.
//! 3. **The necessity map never enters an agent's context.** Enforced by
//!    [`ContextRenderable`]: material bound for a model must implement it,
//!    and the coverage-bearing types deliberately do not.
//!
//! Everything is content-addressed. There are no counters and no random ids
//! in the evidence path.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod claim;
pub mod clock;
pub mod context;
pub mod hash;
pub mod ids;
pub mod ratio;
pub mod verdict;

pub use claim::{Budget, Claim, ProofTier};
pub use clock::{format_rfc3339, now_ms};
pub use context::{ContextRenderable, ModelContext};
pub use hash::{Hash, HashParseError, Hasher};
pub use ids::{CellId, ClaimId, HunkId, PredicateHash, ReceiptRef, RunId};
pub use ratio::Ratio;
pub use verdict::Verdict;
