//! Snapshots, overlay diffs and exact hunk-subset application.
//!
//! The necessity map asks one question repeatedly: *does the proof still pass
//! if this group of changes is reverted?* Answering it honestly requires
//! being able to reconstruct, byte for byte, the tree that results from
//! applying any subset of the agent's changes to the state it started from.
//!
//! That is what this crate does, and the reason it does not shell out to
//! `patch`: patch application is fuzzy by design, and a probe run against a
//! tree that is *nearly* the intended one measures nothing.
//!
//! ```
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! use std::collections::BTreeSet;
//! use warrant_diff::{MemoryStore, OverlayDiff, Snapshot, apply_subset};
//!
//! let store = MemoryStore::new();
//! let pre = Snapshot::from_contents([("a.txt", &b"one\ntwo\n"[..])], &store)?;
//! let post = Snapshot::from_contents([("a.txt", &b"ONE\ntwo\n"[..])], &store)?;
//!
//! let diff = OverlayDiff::between(&pre, &post, &store)?;
//! assert_eq!(diff.hunk_count(), 1);
//!
//! // Reverting everything lands exactly on the pre-state.
//! let reverted = apply_subset(&pre, &post, &diff, &BTreeSet::new(), &store)?;
//! assert_eq!(reverted.root_hash(), pre.root_hash());
//! # Ok(())
//! # }
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod error;
mod hunk;
pub mod lines;
mod overlay;
mod snapshot;
mod store;

pub use error::{DiffError, Result};
pub use hunk::{Hunk, HunkKind, decompose_modified, file_added, file_deleted};
pub use overlay::{ChangeKind, FileDelta, OverlayDiff, apply_subset};
pub use snapshot::{
    ALWAYS_EXCLUDED, FileMeta, MaterializeStats, ScanOptions, Snapshot, join_relative,
    relative_path,
};
pub use store::{ContentStore, MemoryStore};
