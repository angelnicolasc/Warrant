//! Diff failures.

use std::path::{Path, PathBuf};

use warrant_core::Hash;

/// Result alias for diff operations.
pub type Result<T> = std::result::Result<T, DiffError>;

/// Everything that can go wrong snapshotting, diffing or applying.
#[derive(Debug, thiserror::Error)]
pub enum DiffError {
    /// Filesystem failure.
    #[error("{context} at {}: {source}", path.display())]
    Io {
        /// What was being attempted.
        context: &'static str,
        /// The path involved.
        path: PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },

    /// A path could not be expressed relative to the tree root.
    #[error("{} is not inside the tree root", path.display())]
    OutsideRoot {
        /// The offending path.
        path: PathBuf,
    },

    /// Content referenced by a snapshot is not retrievable.
    #[error("content {hash} for {path} is unavailable: {reason}")]
    ContentUnavailable {
        /// The address that could not be resolved.
        hash: Hash,
        /// The file it belongs to.
        path: String,
        /// Why.
        reason: String,
    },

    /// Directory traversal failure.
    #[error("walking the tree: {0}")]
    Walk(#[from] ignore::Error),

    /// Two selected hunks claim overlapping regions of the same pre-image.
    #[error(
        "hunks in {path} overlap at pre-image lines {a:?} and {b:?}; the diff was not decomposed correctly"
    )]
    OverlappingHunks {
        /// File involved.
        path: String,
        /// First range.
        a: (usize, usize),
        /// Second range.
        b: (usize, usize),
    },

    /// A hunk refers to lines that do not exist in the pre-image it was cut
    /// from — always a sign the hunk was paired with the wrong base.
    #[error(
        "hunk in {path} covers pre-image lines {start}..{end} but the pre-image has {available} lines"
    )]
    HunkOutOfRange {
        /// File involved.
        path: String,
        /// First line the hunk claims.
        start: usize,
        /// One past the last line it claims.
        end: usize,
        /// How many lines the pre-image actually has.
        available: usize,
    },
}

impl DiffError {
    pub(crate) fn io(
        context: &'static str,
        path: impl AsRef<Path>,
        source: std::io::Error,
    ) -> Self {
        DiffError::Io { context, path: path.as_ref().to_path_buf(), source }
    }
}
