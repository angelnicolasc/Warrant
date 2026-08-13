//! The loop.
//!
//! Deliberately dumb: the model decides, the harness executes. All the
//! judgement lives on either side of it — in the proof that was sealed before
//! the work started, and in the necessity map computed after it finished.
//!
//! What this crate refuses to do is as much of the design as what it does.
//! There is no summarisation, because [`warrant_core::Handle`] means there is
//! nothing to summarise. There is no per-step model routing, because
//! switching models mid-trajectory rewrites most of what follows. There is no
//! self-modification. And the tool catalogue is closed at six, because every
//! tool is another surface that has to produce admissible evidence.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod anthropic;
pub mod attempt;
pub mod error;
pub mod forensics;
pub mod policy;
pub mod provider;
pub mod session;
pub mod tools;
pub mod workspace;

pub use attempt::{Adjudication, Attempt, AttemptConfig, BestOfN, adjudicate};
pub use error::{AgentError, Result};
pub use forensics::{
    Bisection, Expectation, Fixture, Refutation, Reproduction, RunRecord, bisect, refutations,
    replay_prefix,
};
pub use policy::{ApproveAll, ApproveWithin, Approver, BlastRadius, Decision, Policy};
pub use provider::{
    ContentBlock, Message, ModelRequest, ModelResponse, Provider, RecordedTurn, ReplayProvider,
    Role, ScriptedProvider, StopReason, ToolSpec, Usage, recorded_turns,
};
pub use session::{DEFAULT_SYSTEM_PROMPT, Session, SessionConfig, SessionOutcome, StopCondition};
pub use tools::{BuiltinTool, ToolOutcome, all_specs};
pub use workspace::{ActiveClaim, DischargedClaim, Services, Workspace};

use std::path::Path;
use std::sync::{Arc, Mutex};

use warrant_cell::{Cell, WorkspaceCell};
use warrant_diff::{ContentStore, ScanOptions, Snapshot};

/// Scan options for a cell that lives inside the repository it works on.
///
/// Ancestor ignore files are skipped: the repository's own `.gitignore`
/// excludes `.warrant/`, and a cell underneath it must still be able to see
/// itself. Its own rules still apply, which is what keeps build output out of
/// snapshots and stops a restore deleting the build cache.
pub fn cell_scan_options() -> ScanOptions {
    ScanOptions { use_parent_ignores: false, ..ScanOptions::default() }
}

/// Build a cell holding `baseline`, for probing a claim.
pub fn probe_cell(
    root: &Path,
    baseline: &Snapshot,
    store: Arc<dyn ContentStore>,
) -> Result<Arc<Mutex<dyn Cell>>> {
    std::fs::create_dir_all(root).map_err(|source| AgentError::Io {
        context: format!("creating the probe cell at {}", root.display()),
        source,
    })?;
    let mut cell = WorkspaceCell::adopt(root, store, cell_scan_options())?;
    cell.restore(baseline)?;
    Ok(Arc::new(Mutex::new(cell)))
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use std::path::PathBuf;
    use warrant_attest::Attestor;
    use warrant_diff::MemoryStore;
    use warrant_ledger::Ledger;

    /// Keeps the temporary directories alive and gives tests a way to poke at
    /// the cell the way an agent would.
    pub struct Guards {
        _root: tempfile::TempDir,
        cell_root: PathBuf,
        probe_root: PathBuf,
    }

    impl Guards {
        pub fn cell_root(&self) -> &Path {
            &self.cell_root
        }

        pub fn probe_root(&self) -> PathBuf {
            self.probe_root.clone()
        }

        pub fn write(&self, relative: &str, content: &str) {
            let path = warrant_diff::join_relative(&self.cell_root, relative).unwrap();
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }

        pub fn read(&self, relative: &str) -> String {
            let path = warrant_diff::join_relative(&self.cell_root, relative).unwrap();
            std::fs::read_to_string(path).unwrap()
        }

        pub fn exists(&self, relative: &str) -> bool {
            warrant_diff::join_relative(&self.cell_root, relative).unwrap().exists()
        }
    }

    /// A workspace over a throwaway cell seeded with `files`.
    pub fn scratch_workspace(files: &[(&str, &str)]) -> (Guards, Workspace) {
        let root = tempfile::tempdir().unwrap();
        let cell_root = root.path().join("cell");
        let probe_root = root.path().join("probe");
        std::fs::create_dir_all(&cell_root).unwrap();

        let guards = Guards { _root: root, cell_root: cell_root.clone(), probe_root };
        for (path, content) in files {
            guards.write(path, content);
        }

        let store: Arc<dyn ContentStore> = Arc::new(MemoryStore::new());
        // Ignore rules are off so a machine's global gitignore cannot change
        // what these tests observe.
        let scan = ScanOptions { respect_gitignore: false, ..ScanOptions::default() };
        let cell = WorkspaceCell::adopt(&cell_root, Arc::clone(&store), scan).unwrap();
        let ledger =
            Arc::new(Ledger::open(guards.cell_root().parent().unwrap().join(".warrant")).unwrap());
        let attestor = Arc::new(Attestor::new().unwrap());

        let services = crate::workspace::Services::new(store, ledger, attestor, Policy::default());
        let workspace = Workspace::new(Arc::new(Mutex::new(cell)), services).unwrap();
        (guards, workspace)
    }
}
