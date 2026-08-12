//! The portable backend.
//!
//! A `WorkspaceCell` gives the agent a private copy of the repository and
//! observes it by snapshot. It runs everywhere Warrant compiles, including
//! Windows, and it is what makes the proof map available on any machine.
//!
//! It is honest about being the weaker boundary. The filesystem is separated;
//! the network and the process tree are not. [`Cell::isolation`] reports that
//! per dimension, and the receipt carries it, so nobody reads a proof map
//! produced here as evidence about egress.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use warrant_core::CellId;
use warrant_diff::{ContentStore, ScanOptions, Snapshot};

use crate::cell::{Cell, CellSnapshot, IsolationLevel, IsolationReport, sealed};
use crate::error::{CellError, Result};
use crate::exec::{CommandSpec, ExitRecord};

/// A cell backed by a directory on the host.
pub struct WorkspaceCell {
    id: CellId,
    root: PathBuf,
    store: Arc<dyn ContentStore>,
    scan: ScanOptions,
    remove_on_drop: bool,
}

impl WorkspaceCell {
    /// Copy `source` into `root` and work there.
    ///
    /// The copy goes through the content store, so it is exact, deduplicated
    /// against anything already stored, and the starting state is addressable
    /// from the moment the cell exists.
    pub fn fork_from(
        source: &Path,
        root: impl Into<PathBuf>,
        store: Arc<dyn ContentStore>,
        scan: ScanOptions,
    ) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root).map_err(|e| CellError::io("creating cell root", &root, e))?;

        let origin = Snapshot::scan(source, store.as_ref(), &scan)?;
        origin.materialize_into(&root, &Snapshot::default(), store.as_ref())?;

        let id =
            CellId::derive(&[root.to_string_lossy().as_bytes(), origin.root_hash().as_bytes()]);
        Ok(WorkspaceCell { id, root, store, scan, remove_on_drop: false })
    }

    /// Work in an existing directory rather than a copy of one.
    ///
    /// Used by `warrant wrap` when the operator wants the agent's changes to
    /// land in their actual checkout.
    pub fn adopt(
        root: impl Into<PathBuf>,
        store: Arc<dyn ContentStore>,
        scan: ScanOptions,
    ) -> Result<Self> {
        let root = root.into();
        let origin = Snapshot::scan(&root, store.as_ref(), &scan)?;
        let id =
            CellId::derive(&[root.to_string_lossy().as_bytes(), origin.root_hash().as_bytes()]);
        Ok(WorkspaceCell { id, root, store, scan, remove_on_drop: false })
    }

    /// Delete the cell's directory when it is dropped.
    pub fn remove_on_drop(mut self, remove: bool) -> Self {
        self.remove_on_drop = remove;
        self
    }

    /// The scan options this cell observes with.
    pub fn scan_options(&self) -> &ScanOptions {
        &self.scan
    }

    /// The content store backing this cell.
    pub fn store(&self) -> &Arc<dyn ContentStore> {
        &self.store
    }
}

impl sealed::Sealed for WorkspaceCell {}

impl Cell for WorkspaceCell {
    fn id(&self) -> CellId {
        self.id
    }

    fn root(&self) -> &Path {
        &self.root
    }

    fn isolation(&self) -> IsolationReport {
        IsolationReport {
            backend: "workspace".into(),
            filesystem: IsolationLevel::Directory,
            network: IsolationLevel::None,
            process: IsolationLevel::None,
            caveats: vec![
                "Commands run as the invoking user on the host; only the working directory is separated."
                    .into(),
                "Network egress is neither restricted nor recorded.".into(),
                "Syscalls are not observed.".into(),
            ],
        }
    }

    fn exec(&mut self, spec: &CommandSpec) -> Result<ExitRecord> {
        crate::exec::run(&self.root, spec, self.store.as_ref())
    }

    fn snapshot(&mut self) -> Result<CellSnapshot> {
        Ok(CellSnapshot(Snapshot::scan(&self.root, self.store.as_ref(), &self.scan)?))
    }

    /// Restore by writing only the difference.
    ///
    /// The current state is rescanned rather than assumed. A test run can
    /// create tracked files, and a probe that inherited them from the
    /// previous probe would be measuring a tree the search never chose.
    fn restore(&mut self, snapshot: &Snapshot) -> Result<()> {
        let current = Snapshot::scan(&self.root, self.store.as_ref(), &self.scan)?;
        snapshot.materialize_into(&self.root, &current, self.store.as_ref())?;
        Ok(())
    }
}

impl Drop for WorkspaceCell {
    fn drop(&mut self) {
        if self.remove_on_drop {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delta::Supervisor;
    use warrant_diff::MemoryStore;

    fn store() -> Arc<dyn ContentStore> {
        Arc::new(MemoryStore::new())
    }

    fn write(root: &Path, rel: &str, content: &[u8]) {
        let path = warrant_diff::join_relative(root, rel).unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    #[test]
    fn forking_reproduces_the_source_tree_without_touching_it() {
        let source = tempfile::tempdir().unwrap();
        write(source.path(), "src/main.rs", b"fn main() {}\n");
        write(source.path(), "README.md", b"# project\n");

        let work = tempfile::tempdir().unwrap();
        let store = store();
        let mut cell = WorkspaceCell::fork_from(
            source.path(),
            work.path().join("cell"),
            store.clone(),
            ScanOptions::default(),
        )
        .unwrap();

        let snap = cell.snapshot().unwrap();
        assert_eq!(snap.len(), 2);

        // Changing the cell must not reach the source.
        write(cell.root(), "src/main.rs", b"fn main() { changed }\n");
        assert_eq!(fs::read(source.path().join("src/main.rs")).unwrap(), b"fn main() {}\n");
    }

    #[test]
    fn restore_returns_the_tree_to_an_earlier_observation() {
        let source = tempfile::tempdir().unwrap();
        write(source.path(), "a.txt", b"original\n");

        let work = tempfile::tempdir().unwrap();
        let store = store();
        let mut cell = WorkspaceCell::fork_from(
            source.path(),
            work.path().join("cell"),
            store,
            ScanOptions::default(),
        )
        .unwrap();

        let before = cell.snapshot().unwrap();
        write(cell.root(), "a.txt", b"modified\n");
        write(cell.root(), "new.txt", b"added\n");

        cell.restore(before.as_snapshot()).unwrap();
        assert_eq!(fs::read(cell.root().join("a.txt")).unwrap(), b"original\n");
        assert!(!cell.root().join("new.txt").exists(), "restore must remove what was added");
        assert_eq!(cell.snapshot().unwrap().root_hash(), before.root_hash());
    }

    #[test]
    fn a_file_created_by_a_previous_probe_does_not_leak_into_the_next() {
        let source = tempfile::tempdir().unwrap();
        write(source.path(), "a.txt", b"base\n");

        let work = tempfile::tempdir().unwrap();
        let store = store();
        let mut cell = WorkspaceCell::fork_from(
            source.path(),
            work.path().join("cell"),
            store,
            ScanOptions::default(),
        )
        .unwrap();
        let base = cell.snapshot().unwrap();

        // Simulate a test run leaving a tracked artefact behind.
        write(cell.root(), "generated.txt", b"residue\n");
        cell.restore(base.as_snapshot()).unwrap();
        assert!(!cell.root().join("generated.txt").exists());
    }

    #[test]
    fn the_supervisor_computes_the_delta_from_its_own_observations() {
        let source = tempfile::tempdir().unwrap();
        write(source.path(), "a.txt", b"one\ntwo\n");

        let work = tempfile::tempdir().unwrap();
        let store = store();
        let mut cell = WorkspaceCell::fork_from(
            source.path(),
            work.path().join("cell"),
            store.clone(),
            ScanOptions::default(),
        )
        .unwrap();

        let before = cell.snapshot().unwrap();
        write(cell.root(), "a.txt", b"ONE\ntwo\n");
        write(cell.root(), "b.txt", b"new file\n");
        let after = cell.snapshot().unwrap();

        let supervisor = Supervisor::new();
        let delta = supervisor.observe(&cell, &before, &after, Vec::new(), store.as_ref()).unwrap();

        assert_eq!(delta.overlay().files.len(), 2);
        assert!(!delta.is_empty());
        // The report states what was actually enforced, not what the design
        // is capable of.
        assert_eq!(delta.isolation().network, IsolationLevel::None);
        assert!(!delta.syscalls().observed);
    }

    #[test]
    fn cell_identity_follows_its_starting_state() {
        let source = tempfile::tempdir().unwrap();
        write(source.path(), "a.txt", b"content\n");
        let work = tempfile::tempdir().unwrap();
        let store = store();

        let a = WorkspaceCell::fork_from(
            source.path(),
            work.path().join("same"),
            store.clone(),
            ScanOptions::default(),
        )
        .unwrap();
        let b = WorkspaceCell::fork_from(
            source.path(),
            work.path().join("same"),
            store.clone(),
            ScanOptions::default(),
        )
        .unwrap();
        assert_eq!(a.id(), b.id());

        write(source.path(), "a.txt", b"different\n");
        let c = WorkspaceCell::fork_from(
            source.path(),
            work.path().join("same"),
            store,
            ScanOptions::default(),
        )
        .unwrap();
        assert_ne!(a.id(), c.id());
    }
}
