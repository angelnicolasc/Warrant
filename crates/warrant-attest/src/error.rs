//! Attestation failures.

/// Result alias for attestation.
pub type Result<T> = std::result::Result<T, AttestError>;

/// Everything that can go wrong compiling or discharging a proof.
#[derive(Debug, thiserror::Error)]
pub enum AttestError {
    /// The proof could not be parsed.
    #[error("{message}\n  {source_text}\n  {caret:>offset$}", offset = position + 1, caret = "^")]
    Parse {
        /// What went wrong.
        message: String,
        /// Byte offset into the source.
        position: usize,
        /// The proof as written.
        source_text: String,
    },

    /// The proof parsed but does not typecheck.
    #[error("{0}")]
    Type(String),

    /// The proof could not be turned into a module.
    #[error("compiling proof to WebAssembly: {0}")]
    Compile(String),

    /// The module is not a valid sealed proof.
    #[error("this is not a sealed Warrant proof: {0}")]
    NotAProof(String),

    /// The runtime refused or failed.
    #[error("running proof: {0}")]
    Runtime(String),

    /// The proof exhausted its execution budget.
    ///
    /// Distinct from a proof that returned false: a proof that ran out of
    /// fuel has produced no verdict at all, and saying "unproven" would be
    /// reporting a measurement that never happened.
    #[error("proof exhausted its budget before returning")]
    BudgetExhausted,

    /// The environment the proof queried failed.
    #[error("evaluating {function}: {reason}")]
    Environment {
        /// Which host function.
        function: &'static str,
        /// Why it failed.
        reason: String,
    },

    /// A path pattern in the proof is not a valid glob.
    #[error("`{pattern}` is not a valid path pattern: {reason}")]
    BadPattern {
        /// The pattern as written.
        pattern: String,
        /// Why it was rejected.
        reason: String,
    },

    /// Command execution failure.
    #[error(transparent)]
    Cell(#[from] warrant_cell::CellError),
}

impl AttestError {
    pub(crate) fn parse(message: impl Into<String>, position: usize, source_text: &str) -> Self {
        AttestError::Parse {
            message: message.into(),
            position,
            source_text: source_text.replace(['\n', '\r'], " "),
        }
    }
}
