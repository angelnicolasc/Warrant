//! The isolation boundary.
//!
//! Warrant owns the sandbox, not the loop. From outside a *loop* you observe
//! only the framework's report of a tool call; from outside the *filesystem*
//! you observe what actually happened. Everything attestable comes from this
//! side of the boundary.

use std::path::Path;

use serde::{Deserialize, Serialize};
use warrant_core::CellId;
use warrant_diff::Snapshot;

use crate::error::Result;
use crate::exec::{CommandSpec, ExitRecord};

/// How strongly one dimension of a cell is isolated.
///
/// Reported per dimension rather than as a single grade, because the
/// available strength genuinely differs by platform and dimension, and a
/// receipt that averaged them would be a receipt that overclaimed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IsolationLevel {
    /// Hardware virtualisation.
    Hardware,
    /// Kernel-enforced sandboxing without a hypervisor.
    Kernel,
    /// A separate directory on the host, and nothing more.
    Directory,
    /// Not isolated, and not observed.
    None,
}

impl IsolationLevel {
    /// A short label for terminal output.
    pub fn label(&self) -> &'static str {
        match self {
            IsolationLevel::Hardware => "hardware",
            IsolationLevel::Kernel => "kernel",
            IsolationLevel::Directory => "directory",
            IsolationLevel::None => "none",
        }
    }
}

/// What a cell actually enforced, dimension by dimension.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IsolationReport {
    /// Which backend produced this.
    pub backend: String,
    /// Isolation of the filesystem.
    pub filesystem: IsolationLevel,
    /// Isolation and observation of the network.
    pub network: IsolationLevel,
    /// Isolation of the process tree.
    pub process: IsolationLevel,
    /// Anything a reader needs in order not to over-read the levels above.
    pub caveats: Vec<String>,
}

/// An isolated place to work, which can be observed and rewound.
///
/// Sealed. A `Delta` is only trustworthy if every snapshot feeding it came
/// from a boundary this crate implements, so implementations outside it would
/// defeat the point — see the invariant tests in [`crate::delta`].
pub trait Cell: sealed::Sealed + Send {
    /// Content-derived identity.
    fn id(&self) -> CellId;

    /// The root of the cell's filesystem, as the host sees it.
    fn root(&self) -> &Path;

    /// What this cell enforces.
    fn isolation(&self) -> IsolationReport;

    /// Run a command inside the cell.
    fn exec(&mut self, spec: &CommandSpec) -> Result<ExitRecord>;

    /// Observe the current filesystem state.
    fn snapshot(&mut self) -> Result<CellSnapshot>;

    /// Put the filesystem into a given state.
    ///
    /// Takes a plain [`Snapshot`] rather than a [`CellSnapshot`] on purpose:
    /// the necessity search materialises *candidate* trees that no cell ever
    /// observed, and that is legitimate. What must not be forgeable is a
    /// `Delta`, and a delta is computed only from `CellSnapshot`s — which
    /// [`Cell::snapshot`] alone hands out.
    ///
    /// This is the operation the search performs thousands of times, so
    /// backends are expected to make it proportional to the difference rather
    /// than to the size of the tree.
    fn restore(&mut self, snapshot: &Snapshot) -> Result<()>;
}

/// A filesystem state that a cell actually observed.
///
/// The single field is private: this type cannot be built from anything a
/// model produced, only handed out by [`Cell::snapshot`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellSnapshot(pub(crate) Snapshot);

impl CellSnapshot {
    /// Read the underlying tree.
    pub fn as_snapshot(&self) -> &Snapshot {
        &self.0
    }

    /// The tree's content address.
    pub fn root_hash(&self) -> warrant_core::Hash {
        self.0.root_hash()
    }

    /// How many files it holds.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether it is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

pub(crate) mod sealed {
    /// Implemented only for the backends in this crate.
    pub trait Sealed {}
}
