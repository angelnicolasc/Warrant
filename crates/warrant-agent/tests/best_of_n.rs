//! Best-of-N, running for real.
//!
//! Three branches fork from the same starting state, are bound to the same
//! pre-registered claim, and are adjudicated on what they actually produced.
//! The point being demonstrated is the one that separates this from showing a
//! reviewer five diffs: four of the five never reach a person.

use std::sync::Arc;

use serde_json::json;
use warrant_agent::{
    ApproveAll, AttemptConfig, BestOfN, ModelResponse, Policy, ScriptedProvider, SessionConfig,
    StopCondition,
};
use warrant_attest::Attestor;
use warrant_cell::{Cell, WorkspaceCell};
use warrant_diff::{ContentStore, MemoryStore, ScanOptions, Snapshot};
use warrant_ledger::Ledger;

/// The claim every branch is judged by. It runs no commands, so the whole of
/// it is checkable from the diff — which is also what makes it prunable.
const PROOF: &str = r#"file_exists(b.txt) AND NOT diff_touches("tests/**")"#;

struct Fixture {
    _root: tempfile::TempDir,
    origin: Snapshot,
    store: Arc<dyn ContentStore>,
    ledger: Arc<Ledger>,
    attestor: Arc<Attestor>,
    work_root: std::path::PathBuf,
}

fn fixture() -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let origin_dir = root.path().join("origin");
    std::fs::create_dir_all(origin_dir.join("src")).unwrap();
    std::fs::create_dir_all(origin_dir.join("tests")).unwrap();
    std::fs::write(origin_dir.join("src").join("a.txt"), "source\n").unwrap();
    std::fs::write(origin_dir.join("tests").join("t.txt"), "expected\n").unwrap();

    let store: Arc<dyn ContentStore> = Arc::new(MemoryStore::new());
    let scan = ScanOptions { respect_gitignore: false, ..ScanOptions::default() };
    let mut cell = WorkspaceCell::adopt(&origin_dir, Arc::clone(&store), scan).unwrap();
    let origin = cell.snapshot().unwrap().as_snapshot().clone();

    Fixture {
        origin,
        store,
        ledger: Arc::new(Ledger::open(root.path().join(".warrant")).unwrap()),
        attestor: Arc::new(Attestor::new().unwrap()),
        work_root: root.path().join("work"),
        _root: root,
    }
}

fn write(path: &str, content: &str) -> ModelResponse {
    ModelResponse::calling("w", "fs", json!({ "op": "write", "path": path, "content": content }))
}

fn attest() -> ModelResponse {
    ModelResponse::calling("a", "attest", json!({}))
}

fn done() -> ModelResponse {
    ModelResponse::saying("done")
}

#[test]
fn the_branch_that_proves_the_most_with_the_least_wins() {
    let fixture = fixture();

    // Branches run one after another, so the script reads top to bottom.
    let provider = ScriptedProvider::new([
        // Branch 0 edits a test on its way, which the proof forbids.
        write("tests/hack.txt", "moved the goalposts\n"),
        write("b.txt", "done\n"),
        attest(),
        done(),
        // Branch 1 succeeds, but drags a lot of unrelated work with it.
        write("b.txt", "done\n"),
        write("extra1.txt", "unrelated\nchange\nhere\n"),
        write("extra2.txt", "more\nunrelated\nwork\n"),
        attest(),
        done(),
        // Branch 2 does exactly what was asked.
        write("b.txt", "done\n"),
        attest(),
        done(),
    ]);

    let best = BestOfN::new(
        &provider,
        &ApproveAll,
        warrant_agent::Services::new(
            Arc::clone(&fixture.store),
            Arc::clone(&fixture.ledger),
            Arc::clone(&fixture.attestor),
            Policy::default(),
        ),
        SessionConfig::new("test-model", &fixture.work_root),
        fixture.work_root.join("attempts"),
    );

    let mut config = AttemptConfig::new(3, "b.txt exists without touching the tests", PROOF);
    config.prune_patience = None;
    let (adjudication, states) = best.run(&fixture.origin, "create b.txt", &config).unwrap();

    // The branch that edited a test could not discharge the claim.
    assert!(!adjudication.attempts[0].warranted, "{}", adjudication.attempts[0].summary());
    assert!(adjudication.attempts[1].warranted);
    assert!(adjudication.attempts[2].warranted);
    assert_eq!(adjudication.warranted_count(), 2);

    // Both survivors did the job; only one did only the job.
    assert_eq!(adjudication.attempts[2].coverage.percent(), Some(100));
    assert!(
        adjudication.attempts[1].coverage.percent().unwrap() < 100,
        "the branch with scope creep should not be fully proven"
    );
    assert_eq!(adjudication.winner, Some(2), "{:?}", adjudication.attempts);

    // The winning state is the one a reviewer would be handed.
    let winner = &states[2];
    assert!(winner.files.contains_key("b.txt"));
    assert!(!winner.files.contains_key("extra1.txt"));
    assert!(!winner.files.contains_key("tests/hack.txt"));

    // And the cost of the whole thing travels with the answer.
    assert_eq!(adjudication.total_tokens(), 0, "the scripted provider reports no usage");
}

/// §5.3's pruning signal. A branch that walks away from what it promised stops
/// costing money, and the search says so rather than reporting it as a normal
/// failure.
#[test]
fn a_branch_that_breaks_its_own_proof_is_abandoned_early() {
    let fixture = fixture();

    let provider = ScriptedProvider::new([
        // Off-track on the very first turn, and it keeps going.
        write("tests/hack.txt", "moved the goalposts\n"),
        write("more.txt", "still going\n"),
        write("and-more.txt", "and going\n"),
        write("and-again.txt", "and going\n"),
        write("yet-more.txt", "and going\n"),
        write("still-more.txt", "and going\n"),
    ]);

    let best = BestOfN::new(
        &provider,
        &ApproveAll,
        warrant_agent::Services::new(
            Arc::clone(&fixture.store),
            Arc::clone(&fixture.ledger),
            Arc::clone(&fixture.attestor),
            Policy::default(),
        ),
        SessionConfig::new("test-model", &fixture.work_root),
        fixture.work_root.join("attempts"),
    );

    let mut config = AttemptConfig::new(1, "b.txt exists without touching the tests", PROOF);
    config.prune_patience = Some(2);
    let (adjudication, _) = best.run(&fixture.origin, "create b.txt", &config).unwrap();

    let attempt = &adjudication.attempts[0];
    assert_eq!(attempt.stop, StopCondition::OffTrack { turns: 2 }, "{}", attempt.summary());
    assert_eq!(attempt.turns, 2, "it should stop rather than run the whole script");
    assert!(!attempt.warranted);
    assert_eq!(adjudication.winner, None);
    assert!(provider.remaining() > 0, "the abandoned branch stopped spending");
}

/// A branch that never judges itself is judged anyway. "Ran out of turns"
/// must not be able to masquerade as "not applicable".
#[test]
fn a_branch_that_never_attests_is_still_measured() {
    let fixture = fixture();
    let provider = ScriptedProvider::new([write("b.txt", "done\n"), done()]);

    let best = BestOfN::new(
        &provider,
        &ApproveAll,
        warrant_agent::Services::new(
            Arc::clone(&fixture.store),
            Arc::clone(&fixture.ledger),
            Arc::clone(&fixture.attestor),
            Policy::default(),
        ),
        SessionConfig::new("test-model", &fixture.work_root),
        fixture.work_root.join("attempts"),
    );

    let mut config = AttemptConfig::new(1, "b.txt exists", PROOF);
    config.prune_patience = None;
    let (adjudication, _) = best.run(&fixture.origin, "create b.txt", &config).unwrap();

    assert_eq!(adjudication.attempts[0].stop, StopCondition::Finished);
    assert!(adjudication.attempts[0].warranted, "the harness judged it even though it did not ask");
    assert_eq!(adjudication.winner, Some(0));
}

#[test]
fn every_losing_branch_stays_in_the_record_with_its_evidence() {
    let fixture = fixture();
    let provider = ScriptedProvider::new([
        write("tests/hack.txt", "cheat\n"),
        attest(),
        done(),
        write("b.txt", "done\n"),
        attest(),
        done(),
    ]);

    let best = BestOfN::new(
        &provider,
        &ApproveAll,
        warrant_agent::Services::new(
            Arc::clone(&fixture.store),
            Arc::clone(&fixture.ledger),
            Arc::clone(&fixture.attestor),
            Policy::default(),
        ),
        SessionConfig::new("test-model", &fixture.work_root),
        fixture.work_root.join("attempts"),
    );

    let mut config = AttemptConfig::new(2, "b.txt exists", PROOF);
    config.prune_patience = None;
    let (adjudication, _) = best.run(&fixture.origin, "create b.txt", &config).unwrap();
    assert_eq!(adjudication.winner, Some(1));

    // §5.6: refutations are first class. The approach that failed is
    // queryable, so a later session does not rediscover it.
    let refutations = fixture
        .ledger
        .entries()
        .unwrap()
        .into_iter()
        .filter(|entry| entry.kind == warrant_ledger::EntryKind::Refutation)
        .count();
    assert!(refutations > 0, "the losing branch should be recorded as a refutation");
    assert_eq!(fixture.ledger.verify_deep().unwrap(), fixture.ledger.len().unwrap());
}

/// Guard against the one mistake that would make the whole comparison
/// meaningless: branches judged by proofs of their own choosing.
#[test]
fn every_branch_is_bound_to_the_same_proof() {
    let fixture = fixture();
    let provider = ScriptedProvider::new([
        // Both branches try to declare a weaker proof of their own.
        ModelResponse::calling(
            "d",
            "declare",
            json!({ "assertion": "anything", "proof": "changed_files() >= 0" }),
        ),
        write("b.txt", "done\n"),
        attest(),
        done(),
        ModelResponse::calling(
            "d",
            "declare",
            json!({ "assertion": "anything", "proof": "changed_files() >= 0" }),
        ),
        write("b.txt", "done\n"),
        attest(),
        done(),
    ]);

    let best = BestOfN::new(
        &provider,
        &ApproveAll,
        warrant_agent::Services::new(
            Arc::clone(&fixture.store),
            Arc::clone(&fixture.ledger),
            Arc::clone(&fixture.attestor),
            Policy::default(),
        ),
        SessionConfig::new("test-model", &fixture.work_root),
        fixture.work_root.join("attempts"),
    );

    let mut config = AttemptConfig::new(2, "b.txt exists", PROOF);
    config.prune_patience = None;
    let (adjudication, _) = best.run(&fixture.origin, "create b.txt", &config).unwrap();

    // The attempt to declare a second claim is refused, and both branches are
    // judged by the harness's proof.
    assert!(adjudication.attempts.iter().all(|a| a.warranted));
    assert_eq!(adjudication.warranted_count(), 2);
}
