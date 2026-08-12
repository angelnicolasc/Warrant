//! Receipt failures.

use std::path::{Path, PathBuf};

/// Result alias for receipt operations.
pub type Result<T> = std::result::Result<T, ReceiptError>;

/// Everything that can go wrong issuing or checking a receipt.
#[derive(Debug, thiserror::Error)]
pub enum ReceiptError {
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

    /// The stored signing key is not 32 bytes of hex.
    #[error("the signing key at {} is not readable as a key; move it aside rather than deleting it", path.display())]
    MalformedKey {
        /// Where the key was expected.
        path: PathBuf,
    },

    /// The supplied public key is not a valid ed25519 key.
    #[error("that is not a valid ed25519 public key")]
    MalformedPublicKey,

    /// The envelope is structurally wrong.
    #[error("this is not a well-formed receipt: {reason}")]
    MalformedEnvelope {
        /// What was wrong with it.
        reason: String,
    },

    /// No signature on the envelope verified.
    #[error("no signature on this receipt verifies against key {expected_key_id}")]
    SignatureInvalid {
        /// The key id the verifier was checking against.
        expected_key_id: String,
    },

    /// The payload is not a Warrant statement.
    #[error("the receipt's payload is not a Warrant proof map: {reason}")]
    NotAProofMap {
        /// What was wrong with it.
        reason: String,
    },

    /// JSON encoding or decoding failure.
    #[error("reading receipt JSON: {0}")]
    Json(#[from] serde_json::Error),
}

impl ReceiptError {
    pub(crate) fn io(
        context: &'static str,
        path: impl AsRef<Path>,
        source: std::io::Error,
    ) -> Self {
        ReceiptError::Io { context, path: path.as_ref().to_path_buf(), source }
    }
}
