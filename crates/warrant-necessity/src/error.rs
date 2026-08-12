//! Search failures.

/// Result alias for the necessity search.
pub type Result<T> = std::result::Result<T, NecessityError>;

/// Everything that can stop a map from being produced.
#[derive(Debug, thiserror::Error)]
pub enum NecessityError {
    /// A path pattern is not a valid glob.
    #[error("`{pattern}` is not a valid path pattern: {reason}")]
    BadPattern {
        /// The pattern as configured.
        pattern: String,
        /// Why it was rejected.
        reason: String,
    },

    /// Reconstructing or comparing trees failed.
    #[error(transparent)]
    Diff(#[from] warrant_diff::DiffError),

    /// The cell failed.
    #[error(transparent)]
    Cell(#[from] warrant_cell::CellError),

    /// The proof could not be evaluated.
    #[error(transparent)]
    Attest(#[from] warrant_attest::AttestError),
}
