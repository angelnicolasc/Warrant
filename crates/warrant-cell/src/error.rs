//! Cell failures.

use std::path::{Path, PathBuf};

/// Result alias for cell operations.
pub type Result<T> = std::result::Result<T, CellError>;

/// Everything that can go wrong creating, running inside, or observing a cell.
#[derive(Debug, thiserror::Error)]
pub enum CellError {
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

    /// A command was requested with no program to run.
    #[error("a command needs at least a program name")]
    EmptyCommand,

    /// The program could not be found.
    #[error("`{program}` is not on PATH")]
    ProgramNotFound {
        /// What was asked for.
        program: String,
    },

    /// The program could not be started.
    #[error("starting `{program}`: {source}")]
    SpawnFailed {
        /// What was asked for.
        program: String,
        /// The underlying error.
        source: std::io::Error,
    },

    /// Snapshot or diff failure.
    #[error(transparent)]
    Diff(#[from] warrant_diff::DiffError),

    /// Content store failure.
    #[error("content store: {0}")]
    Store(String),
}

impl CellError {
    pub(crate) fn io(
        context: &'static str,
        path: impl AsRef<Path>,
        source: std::io::Error,
    ) -> Self {
        CellError::Io { context, path: path.as_ref().to_path_buf(), source }
    }
}
