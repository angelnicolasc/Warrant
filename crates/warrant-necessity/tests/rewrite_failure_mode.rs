//! The failure mode, end to end, against a real cell running a real command.
//!
//! The fixture is a repository whose "suite" checks that a configuration file
//! matches a recorded expectation. That is the smallest honest model of the
//! thing being measured: there is an implementation, there is a test, and
//! there are two different ways to make the suite go green — fix the
//! implementation, or edit the test.
//!
//! The proof is the repository's own test command, exactly as an ordinary
//! user would get it with no configuration at all. `git diff --no-index
//! --quiet` is the runner: it exits 0 when two files match and 1 when they do
//! not, on every platform Warrant builds for.

use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};

use warrant_attest::{Attestor, Predicate};
use warrant_cell::{Cell, WorkspaceCell};
use warrant_core::HunkId;
use warrant_diff::{ContentStore, MemoryStore, OverlayDiff, ScanOptions};
use warrant_necessity::{MapOutcome, NecessityConfig, NecessityMap, Search};

/// The repository's test command: does the implementation match what the
/// tests expect?
const SUITE: &str =
    "exit(git diff --no-index --quiet src/config.txt tests/expected_config.txt) == 0";

fn write(root: &Path, rel: &str, content: &str) {
    let path = warrant_diff::join_relative(root, rel).unwrap();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

/// Ignore rules are switched off so the fixture does not inherit whatever
/// global gitignore the machine running the suite happens to have.
fn scan_options() -> ScanOptions {
    ScanOptions { respect_gitignore: false, ..ScanOptions::default() }
}

/// Run a scenario: lay out a starting repository, let an "agent" edit it, and
/// map the result against the repository's own suite.
fn map_scenario(
    before: &[(&str, &str)],
    agent_edits: &[(&str, &str)],
    config: NecessityConfig,
) -> (NecessityMap, OverlayDiff) {
    let source = tempfile::tempdir().unwrap();
    for (path, content) in before {
        write(source.path(), path, content);
    }

    let workdir = tempfile::tempdir().unwrap();
    let store: Arc<dyn ContentStore> = Arc::new(MemoryStore::new());
    let mut cell = WorkspaceCell::fork_from(
        source.path(),
        workdir.path().join("cell"),
        Arc::clone(&store),
        scan_options(),
    )
    .unwrap();

    let pre = cell.snapshot().unwrap();
    for (path, content) in agent_edits {
        write(cell.root(), path, content);
    }
    let post = cell.snapshot().unwrap();

    let diff = OverlayDiff::between(pre.as_snapshot(), post.as_snapshot(), store.as_ref()).unwrap();
    let predicate = Predicate::compile(SUITE).unwrap();
    let attestor = Attestor::new().unwrap();
    let shared: Arc<Mutex<dyn Cell>> = Arc::new(Mutex::new(cell));

    let mut search = Search::new(
        shared,
        pre.as_snapshot(),
        post.as_snapshot(),
        &diff,
        store.as_ref(),
        &attestor,
        &predicate,
        &config,
    );
    let map = search.run().unwrap();
    (map, diff)
}

/// Which files own the load-bearing hunks, for readable assertions.
fn load_bearing_paths(map: &NecessityMap, diff: &OverlayDiff) -> Vec<String> {
    let mut paths: Vec<String> = map
        .load_bearing
        .iter()
        .filter_map(|id: &HunkId| diff.hunk(*id))
        .map(|h| h.path.clone())
        .collect();
    paths.sort();
    paths.dedup();
    paths
}

/// A repository whose suite is red: the implementation disagrees with the
/// recorded expectation.
fn failing_repository() -> Vec<(&'static str, &'static str)> {
    vec![
        ("src/config.txt", "timeout = 99\n"),
        ("src/handler.txt", "retries = 3\n"),
        ("docs/notes.txt", "some notes\n"),
        ("tests/expected_config.txt", "timeout = 30\n"),
    ]
}

#[test]
fn an_honest_fix_is_proven_and_the_scope_creep_beside_it_is_not() {
    let (map, diff) = map_scenario(
        &failing_repository(),
        &[
            // The fix the task actually called for.
            ("src/config.txt", "timeout = 30\n"),
            // Work nothing in the proof depends on.
            ("src/handler.txt", "retries = 5\n"),
            ("docs/notes.txt", "some notes\nand some more\n"),
        ],
        NecessityConfig::default(),
    );

    assert_eq!(map.outcome, MapOutcome::Mapped);
    assert!(map.satisfied, "the suite must be green after the fix");
    assert!(map.null_passed, "the suite must have been red before it");

    assert_eq!(load_bearing_paths(&map, &diff), ["src/config.txt"]);
    assert!(!map.has_tampering(), "no test file was touched");

    let revert_safe: Vec<&str> = map.revert_safe_files().map(|f| f.path.as_str()).collect();
    assert!(revert_safe.contains(&"docs/notes.txt"));
    assert!(revert_safe.contains(&"src/handler.txt"));

    // Three files changed; one is load-bearing. Coverage is well under half
    // and, critically, it is a defined number rather than a guess.
    assert!(map.coverage.is_defined());
    assert!(map.coverage.percent().unwrap() < 50, "coverage was {}", map.coverage);
    assert!(map.minimality_confirmed);
    assert!(map.monotonicity_violations.is_empty());
}

/// The headline case. The agent does not fix the implementation; it edits the
/// expectation so the suite stops complaining. The suite is green, the bug
/// ships, and the map says exactly which change bought the green.
#[test]
fn editing_the_test_instead_of_the_code_is_flagged_as_tampering() {
    let (map, diff) = map_scenario(
        &failing_repository(),
        &[
            // The implementation is untouched. The expectation moves to meet it.
            ("tests/expected_config.txt", "timeout = 99\n"),
            ("docs/notes.txt", "some notes\nand a plausible explanation\n"),
        ],
        NecessityConfig::default(),
    );

    assert_eq!(map.outcome, MapOutcome::Mapped);
    assert!(map.satisfied, "the suite is green — that is the point");
    assert!(map.null_passed);

    assert_eq!(load_bearing_paths(&map, &diff), ["tests/expected_config.txt"]);
    assert!(map.has_tampering(), "a load-bearing hunk inside a test file must be flagged");

    let tampered: Vec<&str> = map.tampered_files().map(|f| f.path.as_str()).collect();
    assert_eq!(tampered, ["tests/expected_config.txt"]);
    assert_eq!(map.tamper.len(), 1);

    // The implementation was never touched, so it is not even in the diff.
    assert!(diff.files.iter().all(|f| f.path != "src/config.txt"));
}

/// The two scenarios above are the same suite, the same green, and the same
/// number of files changed. Only the map tells them apart.
#[test]
fn the_honest_fix_and_the_laundered_green_are_distinguishable_only_by_the_map() {
    let honest = map_scenario(
        &failing_repository(),
        &[("src/config.txt", "timeout = 30\n"), ("docs/notes.txt", "notes\n")],
        NecessityConfig::default(),
    );
    let laundered = map_scenario(
        &failing_repository(),
        &[("tests/expected_config.txt", "timeout = 99\n"), ("docs/notes.txt", "notes\n")],
        NecessityConfig::default(),
    );

    // Indistinguishable on everything a reviewer normally sees.
    assert_eq!(honest.0.satisfied, laundered.0.satisfied);
    assert_eq!(honest.1.files.len(), laundered.1.files.len());

    // And separated cleanly by the one thing that reverts and re-runs.
    assert!(!honest.0.has_tampering());
    assert!(laundered.0.has_tampering());
}

#[test]
fn a_proof_that_already_held_is_reported_as_vacuous_rather_than_as_zero() {
    let (map, _) = map_scenario(
        &[
            ("src/config.txt", "timeout = 30\n"),
            ("tests/expected_config.txt", "timeout = 30\n"),
            ("docs/notes.txt", "notes\n"),
        ],
        &[("docs/notes.txt", "notes, revised\n")],
        NecessityConfig::default(),
    );

    assert_eq!(map.outcome, MapOutcome::Vacuous);
    assert!(!map.null_passed, "the null test is what failed");
    assert!(!map.coverage.is_defined(), "coverage is undefined here, not zero");
    assert_eq!(map.coverage.to_string(), "n/a");
    assert!(map.load_bearing.is_empty());
}

#[test]
fn work_that_does_not_make_the_suite_pass_is_not_mapped_at_all() {
    let (map, _) = map_scenario(
        &failing_repository(),
        &[("docs/notes.txt", "notes about what I tried\n")],
        NecessityConfig::default(),
    );

    assert_eq!(map.outcome, MapOutcome::NotSatisfied);
    assert!(!map.satisfied);
    assert!(!map.coverage.is_defined());
}

#[test]
fn a_run_that_changed_nothing_produces_no_number() {
    let (map, _) = map_scenario(&failing_repository(), &[], NecessityConfig::default());
    assert_eq!(map.outcome, MapOutcome::NoChanges);
    assert!(!map.is_measurement());
}

/// Two changes that only work together must both survive: reverting either
/// one alone breaks the proof, so neither is revert-safe.
#[test]
fn jointly_necessary_changes_are_both_reported_as_load_bearing() {
    let (map, diff) = map_scenario(
        &[
            ("src/config.txt", "timeout = 99\nmode = fast\n"),
            ("tests/expected_config.txt", "timeout = 30\nmode = safe\n"),
        ],
        // Both lines must change for the files to match. They are far enough
        // apart to decompose into two hunks.
        &[("src/config.txt", "timeout = 30\nmode = safe\n")],
        NecessityConfig::default(),
    );

    assert_eq!(map.outcome, MapOutcome::Mapped);
    assert_eq!(load_bearing_paths(&map, &diff), ["src/config.txt"]);
    assert_eq!(map.coverage.percent(), Some(100), "every changed line is needed");
    assert!(map.unproven.is_empty());
}

/// The search must stay affordable as the unproven region grows. Twenty files
/// of dead work around one real fix should not cost twenty probes.
#[test]
fn the_search_stays_logarithmic_as_scope_creep_grows() {
    let mut before = failing_repository();
    let mut edits: Vec<(&str, &str)> = vec![("src/config.txt", "timeout = 30\n")];

    // Leaked to keep the borrow simple; the test process is short-lived.
    for i in 0..20 {
        let path: &'static str = Box::leak(format!("docs/extra_{i}.txt").into_boxed_str());
        before.push((path, "original\n"));
        edits.push((path, "rewritten\n"));
    }

    let (map, diff) = map_scenario(&before, &edits, NecessityConfig::default());

    assert_eq!(map.outcome, MapOutcome::Mapped);
    assert_eq!(load_bearing_paths(&map, &diff), ["src/config.txt"]);
    assert_eq!(diff.hunk_count(), 21);
    assert!(
        map.probes < 21,
        "binary partitioning should beat checking each of 21 hunks, but used {}",
        map.probes
    );
}

#[test]
fn a_probe_budget_produces_a_coarser_map_rather_than_a_wrong_one() {
    let (map, diff) = map_scenario(
        &failing_repository(),
        &[
            ("src/config.txt", "timeout = 30\n"),
            ("src/handler.txt", "retries = 5\n"),
            ("docs/notes.txt", "notes\n"),
        ],
        NecessityConfig::default().with_max_probes(3),
    );

    assert_eq!(map.outcome, MapOutcome::Mapped);
    assert!(map.budget_exhausted);
    // Whatever it managed to establish, the hunk that actually matters is
    // still inside the load-bearing set.
    let paths = load_bearing_paths(&map, &diff);
    assert!(paths.contains(&"src/config.txt".to_string()));
}
