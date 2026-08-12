//! What a probe cell can see of itself.
//!
//! A probe cell is materialised at `<repo>/.warrant/probes/<id>`, which is
//! inside a directory the repository's own `.gitignore` excludes. Two things
//! have to hold at once, and neither is obvious enough to assume:
//!
//! - the cell's *own* ignore rules must still apply, so that a `cargo test`
//!   inside it does not put `target/` into every snapshot — and, worse, so
//!   that restoring a candidate does not delete `target/` and force a full
//!   rebuild on every probe;
//! - the repository's rules must not reach down and hide the cell's contents,
//!   which would make every probe measure an empty tree.
//!
//! These tests pin down the observed behaviour rather than the assumed one.

use std::fs;
use std::path::Path;

use warrant_diff::{MemoryStore, ScanOptions, Snapshot};

fn write(root: &Path, rel: &str, content: &[u8]) {
    let path = warrant_diff::join_relative(root, rel).unwrap();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

/// A repository containing a probe cell, laid out exactly as Warrant builds it.
fn repo_with_probe_cell(repo_ignore: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let repo = tempfile::tempdir().unwrap();
    fs::create_dir_all(repo.path().join(".git")).unwrap();
    write(repo.path(), ".gitignore", repo_ignore.as_bytes());
    write(repo.path(), "src/lib.rs", b"pub fn f() {}\n");

    let cell = repo.path().join(".warrant").join("probes").join("abc123");
    fs::create_dir_all(&cell).unwrap();
    // The cell holds a copy of the repository's tracked files, including its
    // ignore file, because that file is itself tracked.
    write(&cell, ".gitignore", repo_ignore.as_bytes());
    write(&cell, "src/lib.rs", b"pub fn f() {}\n");
    write(&cell, "tests/it.rs", b"#[test] fn t() {}\n");
    // Build output, created by a probe running the suite.
    write(&cell, "target/debug/deps/libdemo.rlib", b"artifact");
    write(&cell, "target/CACHEDIR.TAG", b"Signature: 8a477f597d28d172");

    (repo, cell)
}

fn scan(root: &Path, options: &ScanOptions) -> Vec<String> {
    let store = MemoryStore::new();
    Snapshot::scan(root, &store, options).unwrap().files.keys().cloned().collect()
}

#[test]
fn a_probe_cell_keeps_build_output_out_of_its_own_snapshots() {
    for ignore in ["target/\n.warrant/\n", "/target\n/.warrant/\n"] {
        let (_repo, cell) = repo_with_probe_cell(ignore);
        let paths = scan(&cell, &ScanOptions { use_parent_ignores: false, ..Default::default() });

        assert!(
            !paths.iter().any(|p| p.starts_with("target/")),
            "build output leaked into the snapshot with ignore rules {ignore:?}: {paths:?}"
        );
        assert!(
            paths.contains(&"src/lib.rs".to_string()),
            "the cell must still see its own sources: {paths:?}"
        );
        assert!(paths.contains(&"tests/it.rs".to_string()));
    }
}

/// The reason `target/` must not be snapshotted is not tidiness. A snapshot
/// that contains it is a snapshot that `restore` will *delete*, and every
/// probe then pays for a full rebuild of the repository under test.
#[test]
fn restoring_a_candidate_does_not_destroy_the_build_cache() {
    let (_repo, cell) = repo_with_probe_cell("target/\n.warrant/\n");
    let store = MemoryStore::new();
    let options = ScanOptions { use_parent_ignores: false, ..Default::default() };

    let baseline = Snapshot::scan(&cell, &store, &options).unwrap();

    // A candidate that changes one source file.
    write(&cell, "src/lib.rs", b"pub fn f() { }\n");
    let modified = Snapshot::scan(&cell, &store, &options).unwrap();
    baseline.materialize_into(&cell, &modified, &store).unwrap();

    assert!(
        cell.join("target/debug/deps/libdemo.rlib").exists(),
        "restore deleted the build cache; every probe would rebuild from scratch"
    );
    assert_eq!(fs::read(cell.join("src/lib.rs")).unwrap(), b"pub fn f() {}\n");
}

/// Turning ignore rules off entirely — the naive way to make a probe cell
/// visible — is what causes the damage above. Recorded so the reason the
/// option exists is not lost.
#[test]
fn ignoring_the_ignore_rules_is_what_pulls_build_output_in() {
    let (_repo, cell) = repo_with_probe_cell("target/\n.warrant/\n");
    let paths = scan(&cell, &ScanOptions { respect_gitignore: false, ..Default::default() });
    assert!(
        paths.iter().any(|p| p.starts_with("target/")),
        "expected build output to be pulled in when ignore rules are off: {paths:?}"
    );
}

/// And the repository's own rules, consulted from above, do not hide the cell.
/// Measured rather than assumed: the walk starts inside the excluded
/// directory, so the directory-level exclusion never gets the chance to fire.
#[test]
fn the_repositorys_rules_do_not_hide_the_cell_from_itself() {
    for ignore in ["target/\n.warrant/\n", "/target\n/.warrant/\n"] {
        let (_repo, cell) = repo_with_probe_cell(ignore);
        let paths = scan(&cell, &ScanOptions::default());
        assert!(
            paths.contains(&"src/lib.rs".to_string()),
            "the cell became invisible under ignore rules {ignore:?}: {paths:?}"
        );
    }
}
