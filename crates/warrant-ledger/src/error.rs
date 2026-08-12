//! Ledger failures.

use std::path::{Path, PathBuf};

use warrant_core::Hash;

/// Result alias for ledger operations.
pub type Result<T> = std::result::Result<T, LedgerError>;

/// Everything that can go wrong reading or extending the record.
#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    /// Filesystem failure, with the operation that caused it.
    #[error("{context} at {}: {source}", path.display())]
    Io {
        /// What was being attempted.
        context: &'static str,
        /// The path involved.
        path: PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },

    /// A payload referenced by an entry is not in the blob store.
    #[error("blob {hash} is referenced by the log but absent from the store")]
    BlobMissing {
        /// The address that could not be resolved.
        hash: Hash,
    },

    /// A payload's bytes no longer hash to the address that names them.
    #[error("blob {expected} was altered on disk; its content now hashes to {actual}")]
    BlobCorrupt {
        /// The address recorded in the log.
        expected: Hash,
        /// What the bytes on disk actually hash to.
        actual: Hash,
    },

    /// An entry's recorded hash does not match its fields.
    #[error(
        "entry {seq} does not hash to its recorded address (recorded {recorded}, computed {computed})"
    )]
    EntryForged {
        /// Sequence number of the offending entry.
        seq: u64,
        /// The address stored alongside the entry.
        recorded: Hash,
        /// What the entry's fields actually hash to.
        computed: Hash,
    },

    /// An entry does not point at its predecessor.
    #[error("entry {seq} points at parent {claimed} but entry {} hashes to {actual}", seq - 1)]
    ChainBroken {
        /// Sequence number where the chain breaks.
        seq: u64,
        /// The parent this entry claims.
        claimed: Hash,
        /// The parent it should have claimed.
        actual: Hash,
    },

    /// The log is missing a sequence number.
    #[error("log jumps from entry {previous} to entry {found}; entries cannot be removed")]
    SequenceGap {
        /// The last intact sequence number.
        previous: u64,
        /// The next one present.
        found: u64,
    },

    /// The log no longer contains history a previously observed checkpoint
    /// committed to.
    #[error(
        "log was truncated: entry {expected_seq} should hash to {expected_head}, but the log now holds {present} entries"
    )]
    Truncated {
        /// Sequence number the checkpoint anchored.
        expected_seq: u64,
        /// Head the checkpoint recorded.
        expected_head: Hash,
        /// How many entries survive.
        present: u64,
    },

    /// Something tried to write over an existing entry.
    #[error("entry {seq} already exists; the log is append-only")]
    Overwrite {
        /// The sequence number that was about to be clobbered.
        seq: u64,
    },

    /// Payload encoding failure.
    #[error("encoding ledger payload: {0}")]
    Json(#[from] serde_json::Error),

    /// Database open failure.
    #[error("opening ledger database: {0}")]
    Database(#[from] redb::DatabaseError),

    /// Transaction failure.
    #[error("ledger transaction: {0}")]
    Transaction(#[from] redb::TransactionError),

    /// Table access failure.
    #[error("ledger table: {0}")]
    Table(#[from] redb::TableError),

    /// Storage-layer failure.
    #[error("ledger storage: {0}")]
    Storage(#[from] redb::StorageError),

    /// Commit failure.
    #[error("committing to the ledger: {0}")]
    Commit(#[from] redb::CommitError),
}

impl LedgerError {
    pub(crate) fn io(
        context: &'static str,
        path: impl AsRef<Path>,
        source: std::io::Error,
    ) -> Self {
        LedgerError::Io { context, path: path.as_ref().to_path_buf(), source }
    }
}
