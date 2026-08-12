//! L1 — the record.
//!
//! Append-only, content-addressed, BLAKE3. Every tool call, model response
//! and mutation lands here and stays. Sessions are *projections* over this
//! log rather than objects beside it, which is why replay, fork, resume and
//! audit are one mechanism instead of four.
//!
//! The API is the enforcement. [`Ledger`] exposes `append`, reads and
//! projections — there is no delete verb, no update verb, and no path by
//! which a tool call can reach one. Tampering underneath the API, by editing
//! the files directly, is not prevented but is *detected*: entry headers are
//! hash-chained and payloads are named by their own digest, so
//! [`Ledger::verify_deep`] reports exactly where the record stopped being
//! consistent.
//!
//! ```
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! use warrant_ledger::{EntryKind, Ledger};
//!
//! let dir = tempfile::tempdir()?;
//! let ledger = Ledger::open(dir.path().join(".warrant"))?;
//!
//! ledger.append(EntryKind::RunStarted, b"warrant wrap claude-code", 1_786_492_800_000)?;
//! ledger.append(EntryKind::ToolCall, br#"{"tool":"exec","argv":["pytest"]}"#, 1_786_492_801_000)?;
//!
//! assert_eq!(ledger.verify_deep()?, 2);
//! # Ok(())
//! # }
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod blob;
mod entry;
mod error;
mod ledger;
mod projection;

pub use blob::BlobStore;
pub use entry::{Entry, EntryKind};
pub use error::{LedgerError, Result};
pub use ledger::{Checkpoint, LEDGER_DIR, Ledger};
pub use projection::{Fold, Projection, Replay, ReplayedEvent};
