//! The whole thing, through the binary.
//!
//! These tests run the real `warrant` executable against real repositories on
//! disk, with a real command as the proof. They are what keeps the claim on
//! the front page of the README true: an honest fix and a laundered green
//! look identical on every conventional signal, and the proof map separates
//! them.
//!
//! **The stand-in agent is `git`.** Each fixture carries branches holding the
//! "after" states, and the agent restores files from one of them into the
//! working tree. That keeps these tests free of any interpreter that might
//! not be installed, while still exercising the real path: an external
//! program runs inside a cell and Warrant observes what it left behind.
//!
//! **The proof is `git diff --no-index --quiet`**, which exits 0 when two
//! files match and 1 when they do not, on every platform Warrant builds for.

use std::path::Path;
use std::process::{Command, Output};

/// The repository's suite: does the implementation match what the tests expect?
const PROOF: &str =
    "exit(git diff --no-index --quiet src/config.txt tests/expected_config.txt) == 0";

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git").args(args).current_dir(root).output().unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A repository whose suite is red, carrying two branches that each make it
/// green — one by fixing the code, one by editing the test.
fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    git(root, &["init", "--quiet", "-b", "main"]);
    git(root, &["config", "user.name", "fixture"]);
    git(root, &["config", "user.email", "fixture@example.invalid"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    git(root, &["config", "core.autocrlf", "false"]);

    write(root, ".gitignore", ".warrant/\n");
    write(root, "src/config.txt", "timeout = 99\n");
    write(root, "src/handler.txt", "retries = 3\n");
    write(root, "tests/expected_config.txt", "timeout = 30\n");
    write(root, "docs/notes.md", "# Notes\n");
    git(root, &["add", "-A"]);
    git(root, &["commit", "--quiet", "-m", "the suite is red"]);

    // The laundered green: the expectation moves to meet the implementation.
    git(root, &["checkout", "--quiet", "-b", "laundered"]);
    write(root, "tests/expected_config.txt", "timeout = 99\n");
    write(root, "docs/notes.md", "# Notes\n\nAligned the fixtures.\n");
    write(root, "src/handler.txt", "retries = 5\n");
    git(root, &["commit", "--quiet", "-am", "laundered"]);

    // The honest fix: the implementation moves to meet the expectation.
    git(root, &["checkout", "--quiet", "-b", "honest", "main"]);
    write(root, "src/config.txt", "timeout = 30\n");
    write(root, "docs/notes.md", "# Notes\n\nCorrected the timeout.\n");
    write(root, "src/handler.txt", "retries = 5\n");
    git(root, &["commit", "--quiet", "-am", "honest"]);

    git(root, &["checkout", "--quiet", "main"]);
    dir
}

fn run_warrant(repo: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_warrant"))
        .args(args)
        .current_dir(repo)
        .output()
        .expect("cargo builds the warrant binary before running integration tests")
}

/// Wrap `git checkout <branch> -- <paths…>` as the agent.
fn wrap_agent(repo: &Path, branch: &str, paths: &[&str], extra: &[&str]) -> Output {
    let mut args: Vec<&str> = vec!["wrap", "git"];
    args.extend(extra.iter().copied());
    args.extend(["--proof", PROOF, "--ascii", "--", "checkout", branch, "--"]);
    args.extend(paths.iter().copied());
    run_warrant(repo, &args)
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

const CHANGED_FILES: [&str; 3] = ["src/config.txt", "src/handler.txt", "docs/notes.md"];
const TEST_FILES: [&str; 3] = ["tests/expected_config.txt", "src/handler.txt", "docs/notes.md"];

/// The headline case: the agent edits the expectation rather than the code.
#[test]
fn a_laundered_green_is_flagged_and_fails_a_strict_run() {
    let repo = fixture();
    let output = wrap_agent(repo.path(), "laundered", &TEST_FILES, &["--strict"]);
    let text = stdout(&output);

    assert!(text.contains("claim discharged"), "the suite must be green:\n{text}");
    assert!(
        text.contains("tests/expected_config.txt"),
        "the test file must appear in the map:\n{text}"
    );
    assert!(
        text.contains("load-bearing test edit"),
        "the laundered green must be flagged:\n{text}"
    );
    assert!(
        text.contains("the change that made it pass was the change to the test"),
        "the explanation must be spelled out:\n{text}"
    );
    assert_eq!(output.status.code(), Some(2), "--strict must turn a finding into an exit code");
}

/// The same repository, the same green suite, the same number of changed
/// files — and no finding, because the fix landed in the implementation.
#[test]
fn an_honest_fix_passes_a_strict_run() {
    let repo = fixture();
    let output = wrap_agent(repo.path(), "honest", &CHANGED_FILES, &["--strict"]);
    let text = stdout(&output);

    assert!(text.contains("claim discharged"), "the suite must be green:\n{text}");
    assert!(!text.contains("load-bearing test edit"), "nothing should be flagged:\n{text}");
    assert!(text.contains("src/config.txt"), "the fix must be visible in the map:\n{text}");
    assert!(text.contains("unproven"), "the scope creep must be reported as revert-safe:\n{text}");
    assert_eq!(output.status.code(), Some(0), "an honest fix must not fail a strict run");
}

/// The two runs above are the same suite, the same green, and the same number
/// of changed files. Only the map tells them apart.
#[test]
fn the_two_outcomes_differ_only_in_the_map() {
    let laundered = fixture();
    let honest = fixture();

    let a = stdout(&wrap_agent(laundered.path(), "laundered", &TEST_FILES, &[]));
    let b = stdout(&wrap_agent(honest.path(), "honest", &CHANGED_FILES, &[]));

    // Indistinguishable on everything a reviewer normally sees.
    for text in [&a, &b] {
        assert!(text.contains("agent says            done"), "{text}");
        assert!(text.contains("claim discharged"), "{text}");
        assert!(text.contains("1 of 3 hunks load-bearing"), "{text}");
    }
    // Separated cleanly by the one thing that reverts and re-runs.
    assert!(a.contains("load-bearing test edit"));
    assert!(!b.contains("load-bearing test edit"));
}

#[test]
fn the_record_verifies_and_matches_repository_history() {
    let repo = fixture();
    wrap_agent(repo.path(), "honest", &CHANGED_FILES, &[]);

    let verified = run_warrant(repo.path(), &["log", "--verify", "--ascii"]);
    assert!(verified.status.success(), "{}", stdout(&verified));
    assert!(stdout(&verified).contains("hash chain intact"), "{}", stdout(&verified));

    let diverged = run_warrant(repo.path(), &["log", "--diverged", "--ascii"]);
    assert_eq!(diverged.status.code(), Some(0));
    assert!(stdout(&diverged).contains("still contains everything the ledger recorded"));
}

/// The second prohibition. A force-push rewrites a repository; it cannot
/// unwrite the ledger, and the divergence between the two is reported.
#[test]
fn a_rewritten_history_is_detected_against_the_record() {
    let repo = fixture();
    wrap_agent(repo.path(), "honest", &CHANGED_FILES, &[]);

    // Rewrite the commit the ledger recorded, exactly as a force-push would.
    git(repo.path(), &["add", "-A"]);
    git(repo.path(), &["commit", "--quiet", "--amend", "-m", "quality-of-life update"]);

    let output = run_warrant(repo.path(), &["log", "--diverged", "--ascii"]);
    let text = stdout(&output);
    assert!(text.contains("history rewrite detected"), "{text}");
    assert!(text.contains("no longer reachable from HEAD"), "{text}");
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn a_receipt_written_by_a_run_verifies_on_its_own() {
    let repo = fixture();
    let receipt = repo.path().join("receipt.json");
    wrap_agent(repo.path(), "laundered", &TEST_FILES, &["--receipt", receipt.to_str().unwrap()]);

    assert!(receipt.exists(), "the run should have written a receipt");
    let output = run_warrant(repo.path(), &["verify", receipt.to_str().unwrap(), "--ascii"]);
    let text = stdout(&output);

    assert!(output.status.success(), "{text}");
    assert!(text.contains("signature valid"), "{text}");
    assert!(text.contains("Necessity is not sufficiency"), "{text}");
    assert!(
        text.contains("Part of why this passes is the change to the test"),
        "the receipt must lead with the finding:\n{text}"
    );
}

/// A tampered receipt must not verify. Anyone can edit the JSON; nobody
/// without the key can make the edit survive.
#[test]
fn an_edited_receipt_stops_verifying() {
    let repo = fixture();
    let receipt = repo.path().join("receipt.json");
    wrap_agent(repo.path(), "laundered", &TEST_FILES, &["--receipt", receipt.to_str().unwrap()]);

    let original = std::fs::read_to_string(&receipt).unwrap();
    // Flip one character of the signed payload.
    let doctored = original.replacen("\"payload\": \"e", "\"payload\": \"f", 1);
    assert_ne!(doctored, original, "the payload field should have been found");
    std::fs::write(&receipt, doctored).unwrap();

    let output = run_warrant(repo.path(), &["verify", receipt.to_str().unwrap(), "--ascii"]);
    assert!(!output.status.success(), "an edited receipt must not verify");
}

#[test]
fn a_proof_that_already_held_is_reported_as_vacuous() {
    let repo = fixture();
    // Start from the honest branch, where the suite is already green, and let
    // the agent change only documentation.
    git(repo.path(), &["checkout", "--quiet", "honest"]);
    let output = wrap_agent(repo.path(), "laundered", &["docs/notes.md"], &["--strict"]);
    let text = stdout(&output);

    assert!(text.contains("proof is vacuous"), "{text}");
    assert!(text.contains("n/a"), "coverage must be undefined rather than zero:\n{text}");
    assert!(!text.contains('%'), "no percentage may be printed:\n{text}");
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn the_default_proof_is_the_repositorys_own_test_command() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "Cargo.toml", "[package]\nname = \"x\"\nversion = \"0.1.0\"\n");
    std::fs::create_dir_all(dir.path().join(".warrant")).unwrap();

    let output = run_warrant(dir.path(), &["proof", "--ascii"]);
    let text = stdout(&output);
    assert!(output.status.success(), "{text}");
    assert!(text.contains("exit(cargo test) == 0"), "{text}");
    assert!(text.contains("detected from Cargo.toml"), "{text}");
    assert!(text.contains("blake3:"), "the proof must be sealed and addressed:\n{text}");
}

#[test]
fn an_unrecognisable_repository_asks_for_a_proof_rather_than_guessing() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "README.md", "# just docs\n");
    std::fs::create_dir_all(dir.path().join(".warrant")).unwrap();

    let output = run_warrant(dir.path(), &["proof", "--ascii"]);
    assert!(!output.status.success());
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(text.contains("no test command could be detected"), "{text}");
    assert!(text.contains("--proof"), "the message must say what to do:\n{text}");
}
