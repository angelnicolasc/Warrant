//! L2 — cells.
//!
//! An isolation boundary the agent works inside and the supervisor observes
//! from outside. The upper-layer difference between two observations *is* the
//! evidence; the agent is never asked what it changed, and nothing it says is
//! an input to a [`Delta`].
//!
//! That last sentence is enforced by the compiler. See [`delta`] for the
//! three `compile_fail` tests that hold it in place.
//!
//! # Backends
//!
//! [`WorkspaceCell`] is the portable backend: a private directory, observed
//! by content-addressed snapshot, running anywhere Warrant compiles. It
//! separates the filesystem and nothing else, and says so — every cell
//! reports an [`IsolationReport`] per dimension, which travels with the delta
//! into the receipt so that no reader mistakes a directory for a hypervisor.
//!
//! A hardware-isolated backend fits behind the same sealed [`Cell`] trait.
//! Until one is present and tested on the platform in question, the
//! `filesystem` / `network` / `process` levels reported here are the honest
//! ones.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod cell;
pub mod delta;
mod error;
mod exec;
mod workspace;

pub use cell::{Cell, CellSnapshot, IsolationLevel, IsolationReport};
pub use delta::{Delta, NetRecord, Supervisor, SyscallSummary};
pub use error::{CellError, Result};
pub use exec::{CommandSpec, ExitRecord, run};
pub use workspace::WorkspaceCell;
