//! Filesystem snapshots.
//!
//! A snapshot is a manifest: sorted repo-relative paths to content
//! addresses. It is what the supervisor observes, never what the agent
//! reports, and it is cheap to compare because comparison is over 32-byte
//! addresses rather than file contents.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use warrant_core::Hash;

use crate::error::{DiffError, Result};
use crate::store::ContentStore;

/// Paths never included in a snapshot, whatever the ignore rules say.
///
/// `.git` is excluded because git is downstream of the ledger — churn in the
/// object database is not evidence about what the agent changed. `.warrant`
/// is excluded because the record must not observe itself.
pub const ALWAYS_EXCLUDED: [&str; 2] = [".git", ".warrant"];

/// How a tree is walked.
#[derive(Clone, Debug)]
pub struct ScanOptions {
    /// Honour `.gitignore` and friends. On by default: build output is not
    /// the agent's work, and including it makes every map useless.
    pub respect_gitignore: bool,
    /// Also consult ignore files in directories *above* the tree root.
    ///
    /// On by default, which is right for a repository. Off for a probe cell
    /// that lives inside the repository it is probing: the repository's own
    /// `.gitignore` excludes `.warrant/`, so an ancestor-aware walk would
    /// find every file in the cell ignored and every snapshot empty.
    pub use_parent_ignores: bool,
    /// Include dotfiles. On by default — agents edit `.github/workflows`.
    pub include_hidden: bool,
    /// Additional top-level names to skip.
    pub exclude: Vec<String>,
    /// Files larger than this are recorded by size and address only, never
    /// decomposed into hunks.
    pub max_hunked_bytes: u64,
}

impl Default for ScanOptions {
    fn default() -> Self {
        ScanOptions {
            respect_gitignore: true,
            use_parent_ignores: true,
            include_hidden: true,
            exclude: Vec::new(),
            max_hunked_bytes: 4 * 1024 * 1024,
        }
    }
}

/// One file in a snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileMeta {
    /// Address of the file's contents.
    pub content: Hash,
    /// Size in bytes.
    pub size: u64,
    /// Whether the file is executable.
    ///
    /// Always `false` on Windows, which has no equivalent bit. A mode-only
    /// change is therefore invisible there, and that is recorded as a
    /// platform limitation rather than papered over.
    pub executable: bool,
}

/// A content-addressed view of a directory tree.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    /// Repo-relative paths, `/`-separated, in sorted order.
    pub files: BTreeMap<String, FileMeta>,
}

impl Snapshot {
    /// Walk `root` and record every regular file.
    pub fn scan(root: &Path, store: &dyn ContentStore, options: &ScanOptions) -> Result<Snapshot> {
        let mut builder = ignore::WalkBuilder::new(root);
        builder
            .hidden(!options.include_hidden)
            .git_ignore(options.respect_gitignore)
            .git_global(options.respect_gitignore)
            .git_exclude(options.respect_gitignore)
            // Without this, ignore rules are silently skipped outside a git
            // repository — which is exactly where `warrant wrap` often runs.
            .require_git(false)
            .parents(options.respect_gitignore && options.use_parent_ignores)
            .follow_links(false);

        let extra: Vec<String> = options.exclude.clone();
        let root_owned = root.to_path_buf();
        builder.filter_entry(move |entry| {
            let Ok(rel) = entry.path().strip_prefix(&root_owned) else {
                return true;
            };
            let Some(first) = rel.components().next() else {
                return true;
            };
            let Component::Normal(name) = first else {
                return true;
            };
            let name = name.to_string_lossy();
            !ALWAYS_EXCLUDED.contains(&name.as_ref()) && !extra.iter().any(|e| e == name.as_ref())
        });

        let mut files = BTreeMap::new();
        for entry in builder.build() {
            let entry = entry?;
            // Symlinks are not followed and not recorded; a snapshot describes
            // regular file content, and resolving links would let a diff
            // escape the tree root.
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            let path = entry.path();
            let rel = relative_path(root, path)?;
            let bytes =
                fs::read(path).map_err(|e| DiffError::io("reading file for snapshot", path, e))?;
            let content = store.put(&bytes).map_err(|reason| DiffError::ContentUnavailable {
                hash: Hash::of(&bytes),
                path: rel.clone(),
                reason,
            })?;
            files.insert(
                rel,
                FileMeta { content, size: bytes.len() as u64, executable: is_executable(path) },
            );
        }

        Ok(Snapshot { files })
    }

    /// Build a snapshot directly from path/content pairs. Used by tests and
    /// by backends that receive an overlay rather than a directory.
    pub fn from_contents<'a>(
        entries: impl IntoIterator<Item = (&'a str, &'a [u8])>,
        store: &dyn ContentStore,
    ) -> Result<Snapshot> {
        let mut files = BTreeMap::new();
        for (path, bytes) in entries {
            let content = store.put(bytes).map_err(|reason| DiffError::ContentUnavailable {
                hash: Hash::of(bytes),
                path: path.to_owned(),
                reason,
            })?;
            files.insert(
                path.to_owned(),
                FileMeta { content, size: bytes.len() as u64, executable: false },
            );
        }
        Ok(Snapshot { files })
    }

    /// A single address covering the whole tree.
    ///
    /// Order is fixed by the sorted manifest and every field is
    /// length-prefixed, so two trees hash equal exactly when they hold the
    /// same paths with the same contents and modes.
    pub fn root_hash(&self) -> Hash {
        let mut parts: Vec<Vec<u8>> = Vec::with_capacity(self.files.len() * 3);
        for (path, meta) in &self.files {
            parts.push(path.as_bytes().to_vec());
            parts.push(meta.content.as_bytes().to_vec());
            parts.push(vec![u8::from(meta.executable)]);
        }
        let refs: Vec<&[u8]> = parts.iter().map(Vec::as_slice).collect();
        Hash::of_tagged("warrant.snapshot.v1", &refs)
    }

    /// Number of files.
    pub fn len(&self) -> usize {
        self.files.len()
    }

    /// Whether the tree is empty.
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Content of one file.
    pub fn content_of(&self, path: &str, store: &dyn ContentStore) -> Result<Option<Vec<u8>>> {
        let Some(meta) = self.files.get(path) else {
            return Ok(None);
        };
        let bytes = store.get(&meta.content).map_err(|reason| DiffError::ContentUnavailable {
            hash: meta.content,
            path: path.to_owned(),
            reason,
        })?;
        Ok(Some(bytes))
    }

    /// Write this tree into `root`, changing only what differs from `current`.
    ///
    /// The delta form is what makes the necessity search affordable: a probe
    /// touches the handful of files the candidate subset actually alters
    /// rather than rewriting the whole repository.
    ///
    /// Pass `&Snapshot::default()` as `current` to materialise into an empty
    /// directory.
    pub fn materialize_into(
        &self,
        root: &Path,
        current: &Snapshot,
        store: &dyn ContentStore,
    ) -> Result<MaterializeStats> {
        let mut stats = MaterializeStats::default();

        for (path, meta) in &self.files {
            if current.files.get(path) == Some(meta) {
                stats.unchanged += 1;
                continue;
            }
            let target = join_relative(root, path)?;
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(|e| {
                    DiffError::io("creating directory for materialised file", parent, e)
                })?;
            }
            let bytes = store.get(&meta.content).map_err(|reason| {
                DiffError::ContentUnavailable { hash: meta.content, path: path.clone(), reason }
            })?;
            fs::write(&target, &bytes)
                .map_err(|e| DiffError::io("materialising file", &target, e))?;
            set_executable(&target, meta.executable);
            stats.written += 1;
        }

        let mut emptied: Vec<PathBuf> = Vec::new();
        for path in current.files.keys() {
            if self.files.contains_key(path) {
                continue;
            }
            let target = join_relative(root, path)?;
            match fs::remove_file(&target) {
                Ok(()) => stats.removed += 1,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(DiffError::io("removing file", &target, e)),
            }
            if let Some(parent) = target.parent() {
                emptied.push(parent.to_path_buf());
            }
        }

        // A directory left behind after its last file is deleted can change
        // how a test runner collects tests, so it does not survive the probe.
        emptied.sort_unstable();
        emptied.dedup();
        for dir in emptied.into_iter().rev() {
            prune_empty_dirs(&dir, root);
        }

        Ok(stats)
    }
}

/// What materialising a snapshot actually did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MaterializeStats {
    /// Files written or overwritten.
    pub written: usize,
    /// Files deleted.
    pub removed: usize,
    /// Files already correct, and therefore untouched.
    pub unchanged: usize,
}

/// Normalise a path to a `/`-separated, repo-relative string.
pub fn relative_path(root: &Path, path: &Path) -> Result<String> {
    let rel =
        path.strip_prefix(root).map_err(|_| DiffError::OutsideRoot { path: path.to_path_buf() })?;
    let mut parts = Vec::new();
    for component in rel.components() {
        match component {
            Component::Normal(name) => parts.push(name.to_string_lossy().into_owned()),
            // Anything else would let a snapshot path escape the root.
            _ => return Err(DiffError::OutsideRoot { path: path.to_path_buf() }),
        }
    }
    Ok(parts.join("/"))
}

/// Resolve a repo-relative path against a root, refusing to escape it.
pub fn join_relative(root: &Path, relative: &str) -> Result<PathBuf> {
    let mut out = root.to_path_buf();
    for segment in relative.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(DiffError::OutsideRoot { path: PathBuf::from(relative) });
        }
        out.push(segment);
    }
    Ok(out)
}

fn prune_empty_dirs(dir: &Path, stop_at: &Path) {
    let mut current = dir.to_path_buf();
    while current.starts_with(stop_at) && current != stop_at {
        match fs::remove_dir(&current) {
            Ok(()) => {}
            // Non-empty, or already gone. Either way there is nothing above
            // it worth trying.
            Err(_) => return,
        }
        let Some(parent) = current.parent() else { return };
        current = parent.to_path_buf();
    }
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).map(|m| m.permissions().mode() & 0o111 != 0).unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(_path: &Path) -> bool {
    false
}

#[cfg(unix)]
fn set_executable(path: &Path, executable: bool) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(metadata) = fs::metadata(path) {
        let mut perms = metadata.permissions();
        let mode = perms.mode();
        perms.set_mode(if executable { mode | 0o111 } else { mode & !0o111 });
        let _ = fs::set_permissions(path, perms);
    }
}

#[cfg(not(unix))]
fn set_executable(_path: &Path, _executable: bool) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MemoryStore;

    fn write(root: &Path, rel: &str, content: &[u8]) {
        let path = join_relative(root, rel).unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    #[test]
    fn scanning_records_every_regular_file_with_forward_slash_paths() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "src/main.rs", b"fn main() {}");
        write(dir.path(), "src/util/helper.rs", b"pub fn help() {}");
        write(dir.path(), "README.md", b"# hi");

        let store = MemoryStore::new();
        let snap = Snapshot::scan(dir.path(), &store, &ScanOptions::default()).unwrap();

        let paths: Vec<&str> = snap.files.keys().map(String::as_str).collect();
        assert_eq!(paths, ["README.md", "src/main.rs", "src/util/helper.rs"]);
    }

    #[test]
    fn the_ledger_and_git_directories_are_never_snapshotted() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "src/main.rs", b"fn main() {}");
        write(dir.path(), ".git/objects/ab/cdef", b"binary");
        write(dir.path(), ".warrant/ledger.redb", b"record");

        let store = MemoryStore::new();
        let snap = Snapshot::scan(dir.path(), &store, &ScanOptions::default()).unwrap();
        assert_eq!(snap.files.keys().collect::<Vec<_>>(), ["src/main.rs"]);
    }

    #[test]
    fn gitignored_build_output_is_excluded() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), ".gitignore", b"target/\n*.log\n");
        write(dir.path(), "src/main.rs", b"fn main() {}");
        write(dir.path(), "target/debug/binary", b"artifact");
        write(dir.path(), "run.log", b"noise");

        let store = MemoryStore::new();
        let snap = Snapshot::scan(dir.path(), &store, &ScanOptions::default()).unwrap();
        let paths: Vec<&str> = snap.files.keys().map(String::as_str).collect();
        assert_eq!(paths, [".gitignore", "src/main.rs"]);
    }

    #[test]
    fn identical_trees_hash_identically_and_different_ones_do_not() {
        let store = MemoryStore::new();
        let a = Snapshot::from_contents([("a.txt", &b"one"[..]), ("b.txt", &b"two"[..])], &store)
            .unwrap();
        let b = Snapshot::from_contents([("b.txt", &b"two"[..]), ("a.txt", &b"one"[..])], &store)
            .unwrap();
        let c = Snapshot::from_contents([("a.txt", &b"one"[..]), ("b.txt", &b"THREE"[..])], &store)
            .unwrap();

        assert_eq!(a.root_hash(), b.root_hash(), "insertion order must not matter");
        assert_ne!(a.root_hash(), c.root_hash());
    }

    #[test]
    fn materialising_writes_only_what_differs() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::new();

        let first =
            Snapshot::from_contents([("a.txt", &b"one"[..]), ("b.txt", &b"two"[..])], &store)
                .unwrap();
        let stats = first.materialize_into(dir.path(), &Snapshot::default(), &store).unwrap();
        assert_eq!(stats, MaterializeStats { written: 2, removed: 0, unchanged: 0 });

        let second =
            Snapshot::from_contents([("a.txt", &b"one"[..]), ("b.txt", &b"CHANGED"[..])], &store)
                .unwrap();
        let stats = second.materialize_into(dir.path(), &first, &store).unwrap();
        assert_eq!(stats, MaterializeStats { written: 1, removed: 0, unchanged: 1 });
        assert_eq!(fs::read(dir.path().join("b.txt")).unwrap(), b"CHANGED");
    }

    #[test]
    fn materialising_removes_files_and_prunes_the_directories_they_emptied() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::new();

        let with = Snapshot::from_contents(
            [("keep.txt", &b"k"[..]), ("nested/deep/gone.txt", &b"g"[..])],
            &store,
        )
        .unwrap();
        with.materialize_into(dir.path(), &Snapshot::default(), &store).unwrap();
        assert!(dir.path().join("nested/deep/gone.txt").exists());

        let without = Snapshot::from_contents([("keep.txt", &b"k"[..])], &store).unwrap();
        let stats = without.materialize_into(dir.path(), &with, &store).unwrap();
        assert_eq!(stats.removed, 1);
        assert!(!dir.path().join("nested").exists(), "emptied directories must not survive");
    }

    #[test]
    fn a_round_trip_through_the_filesystem_preserves_the_tree_hash() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::new();
        let original = Snapshot::from_contents(
            [
                ("a.txt", &b"one\ntwo\n"[..]),
                ("nested/b.bin", &[0u8, 1, 2, 255][..]),
                ("no-newline.txt", &b"trailing"[..]),
            ],
            &store,
        )
        .unwrap();

        original.materialize_into(dir.path(), &Snapshot::default(), &store).unwrap();
        let rescanned = Snapshot::scan(dir.path(), &store, &ScanOptions::default()).unwrap();
        assert_eq!(rescanned.root_hash(), original.root_hash());
    }

    #[test]
    fn a_tree_can_be_scanned_without_consulting_ignore_files_above_it() {
        let outer = tempfile::tempdir().unwrap();
        write(outer.path(), ".gitignore", b"secret.txt\n");
        let inner = outer.path().join("nested");
        write(&inner, "secret.txt", b"hidden by the parent rule");
        write(&inner, "visible.txt", b"always present");

        let store = MemoryStore::new();

        let inheriting = Snapshot::scan(&inner, &store, &ScanOptions::default()).unwrap();
        assert_eq!(inheriting.files.keys().collect::<Vec<_>>(), ["visible.txt"]);

        let standalone = Snapshot::scan(
            &inner,
            &store,
            &ScanOptions { use_parent_ignores: false, ..ScanOptions::default() },
        )
        .unwrap();
        assert_eq!(
            standalone.files.keys().collect::<Vec<_>>(),
            ["secret.txt", "visible.txt"],
            "an ancestor's rules must be skippable for a tree that is not part of it"
        );
    }

    #[test]
    fn relative_paths_cannot_escape_the_root() {
        let root = Path::new("/repo");
        assert!(join_relative(root, "../etc/passwd").is_err());
        assert!(join_relative(root, "src/../../etc").is_err());
        assert!(join_relative(root, "").is_err());
        assert_eq!(join_relative(root, "src/main.rs").unwrap(), root.join("src").join("main.rs"));
    }
}
