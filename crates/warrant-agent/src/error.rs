//! Agent failures.

use warrant_core::Hash;

/// Result alias for the agent layer.
pub type Result<T> = std::result::Result<T, AgentError>;

/// Everything that can stop a session.
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    /// A scripted provider ran out of answers.
    #[error("the scripted provider has no further responses")]
    ScriptExhausted,

    /// A replay ran past the end of its recording.
    #[error("the recording ends at turn {turn}; the run wanted to continue")]
    ReplayExhausted {
        /// Which turn was wanted.
        turn: usize,
    },

    /// A replay was asked a question the recording does not answer.
    #[error(
        "the run diverged from the recording at turn {turn}: it was recorded answering {recorded}, \
         and is now being asked {asked}. Replaying anyway would score a trajectory that never happened."
    )]
    ReplayDiverged {
        /// Where the divergence appeared.
        turn: usize,
        /// The request the recorded answer belongs to.
        recorded: Hash,
        /// The request actually made.
        asked: Hash,
    },

    /// The model provider returned an error.
    #[error("{provider}: {message}")]
    Provider {
        /// Which provider.
        provider: String,
        /// What it said.
        message: String,
    },

    /// The transport failed before a response arrived.
    #[error("reaching the model: {0}")]
    Transport(String),

    /// The model asked for a tool that does not exist.
    #[error("`{0}` is not one of this harness's tools")]
    UnknownTool(String),

    /// The model called a tool with arguments it cannot use.
    #[error("`{tool}` was called with arguments it cannot use: {reason}")]
    BadToolInput {
        /// Which tool.
        tool: &'static str,
        /// What was wrong.
        reason: String,
    },

    /// Policy refused an action.
    #[error("refused: {reason}")]
    Refused {
        /// Why.
        reason: String,
    },

    /// A claim was attested without having been declared.
    #[error("nothing has been declared, so there is nothing to attest")]
    NoActiveClaim,

    /// Cell failure.
    #[error(transparent)]
    Cell(#[from] warrant_cell::CellError),

    /// Proof failure.
    #[error(transparent)]
    Attest(#[from] warrant_attest::AttestError),

    /// Snapshot or diff failure.
    #[error(transparent)]
    Diff(#[from] warrant_diff::DiffError),

    /// Ledger failure.
    #[error(transparent)]
    Ledger(#[from] warrant_ledger::LedgerError),

    /// Necessity search failure.
    #[error(transparent)]
    Necessity(#[from] warrant_necessity::NecessityError),

    /// Filesystem failure.
    #[error("{context}: {source}")]
    Io {
        /// What was being attempted.
        context: String,
        /// The underlying error.
        source: std::io::Error,
    },

    /// Encoding failure.
    #[error("encoding: {0}")]
    Json(#[from] serde_json::Error),
}
