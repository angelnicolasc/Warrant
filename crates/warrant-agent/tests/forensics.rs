//! Reading a run back out of the record.
//!
//! Every test here starts by throwing away everything except the ledger. If
//! a run cannot be rebuilt from the record alone, the record is decoration.

use std::sync::Arc;

use serde_json::json;
use warrant_agent::{
    ApproveAll, Fixture, ModelResponse, Policy, RunRecord, ScriptedProvider, Services, Session,
    SessionConfig, Workspace, bisect, refutations,
};
use warrant_attest::{Attestor, Predicate};
use warrant_cell::WorkspaceCell;
use warrant_diff::{ContentStore, MemoryStore, ScanOptions};
use warrant_ledger::Ledger;

struct Fixed {
    _root: tempfile::TempDir,
    root: std::path::PathBuf,
    services: Services,
}

fn services(files: &[(&str, &str)]) -> (Fixed, Workspace) {
    let root = tempfile::tempdir().unwrap();
    let cell_root = root.path().join("cell");
    std::fs::create_dir_all(&cell_root).unwrap();
    for (path, content) in files {
        let target = warrant_diff::join_relative(&cell_root, path).unwrap();
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(target, content).unwrap();
    }

    let store: Arc<dyn ContentStore> = Arc::new(MemoryStore::new());
    let scan = ScanOptions { respect_gitignore: false, ..ScanOptions::default() };
    let cell = WorkspaceCell::adopt(&cell_root, Arc::clone(&store), scan).unwrap();
    let services = Services::new(
        store,
        Arc::new(Ledger::open(root.path().join(".warrant")).unwrap()),
        Arc::new(Attestor::new().unwrap()),
        Policy::default(),
    );
    let workspace =
        Workspace::new(Arc::new(std::sync::Mutex::new(cell)), services.clone()).unwrap();

    let path = root.path().to_path_buf();
    (Fixed { _root: root, root: path, services }, workspace)
}

fn write(path: &str, content: &str) -> ModelResponse {
    ModelResponse::calling("w", "fs", json!({ "op": "write", "path": path, "content": content }))
}

fn config(fixed: &Fixed) -> SessionConfig {
    SessionConfig::new("test-model", fixed.root.join("work"))
}

#[test]
fn a_run_can_be_read_back_out_of_the_record() {
    let (fixed, workspace) = services(&[("src/a.txt", "start\n")]);
    let provider = ScriptedProvider::new([
        write("one.txt", "first\n"),
        write("two.txt", "second\n"),
        ModelResponse::saying("done"),
    ]);
    let mut session = Session::new(&provider, &ApproveAll, workspace, config(&fixed));
    session.run("write two files").unwrap();

    let record = RunRecord::read(&fixed.services.ledger).unwrap();
    assert_eq!(record.task, "write two files");
    assert_eq!(record.model, "test-model");
    assert_eq!(record.len(), 3);
    assert!(record.origin.files.contains_key("src/a.txt"));
    assert!(!record.origin.files.contains_key("one.txt"), "the origin is the starting tree");
}

/// §5.4. A run ends wrong; find the turn it went wrong on, without re-running
/// it from the start once per turn.
#[test]
fn bisecting_finds_the_turn_a_proof_stopped_holding() {
    let (fixed, workspace) = services(&[("src/config.txt", "timeout = 30\n")]);

    // Eight harmless turns, then one that breaks the invariant, then more.
    let mut script: Vec<ModelResponse> =
        (0..8).map(|i| write(&format!("note{i}.txt"), "harmless\n")).collect();
    script.push(write("src/config.txt", "timeout = 99\n"));
    script.extend((0..4).map(|i| write(&format!("after{i}.txt"), "more\n")));
    script.push(ModelResponse::saying("done"));

    let provider = ScriptedProvider::new(script);
    let mut session = Session::new(&provider, &ApproveAll, workspace, config(&fixed));
    session.run("do some work").unwrap();

    let record = RunRecord::read(&fixed.services.ledger).unwrap();
    assert_eq!(record.len(), 14);

    // The invariant: the configuration still says thirty.
    let proof = Predicate::compile(
        "exit(git diff --no-index --quiet src/config.txt src/config.txt) == 0 AND NOT diff_touches(\"src/config.txt\")",
    )
    .unwrap();

    let bisection = bisect(&record, &proof, &fixed.services, &fixed.root.join("bisect")).unwrap();

    assert!(bisection.held_at_start);
    assert!(!bisection.held_at_end);
    assert_eq!(bisection.first_bad_turn, Some(9), "{}", bisection.describe());
    assert!(
        bisection.replays <= 8,
        "binary search over 14 turns should not take {} replays",
        bisection.replays
    );
}

#[test]
fn a_proof_that_holds_throughout_has_no_first_bad_turn() {
    let (fixed, workspace) = services(&[("src/a.txt", "start\n")]);
    let provider = ScriptedProvider::new([
        write("one.txt", "first\n"),
        write("two.txt", "second\n"),
        ModelResponse::saying("done"),
    ]);
    let mut session = Session::new(&provider, &ApproveAll, workspace, config(&fixed));
    session.run("write two files").unwrap();

    let record = RunRecord::read(&fixed.services.ledger).unwrap();
    let proof = Predicate::compile("file_exists(src/a.txt)").unwrap();
    let bisection = bisect(&record, &proof, &fixed.services, &fixed.root.join("bisect")).unwrap();

    assert!(bisection.held_at_start);
    assert!(bisection.held_at_end);
    assert_eq!(bisection.first_bad_turn, None);
    assert!(bisection.describe().contains("held at every point"));
}

#[test]
fn a_proof_that_never_held_says_so_rather_than_naming_a_turn() {
    let (fixed, workspace) = services(&[("src/a.txt", "start\n")]);
    let provider =
        ScriptedProvider::new([write("one.txt", "first\n"), ModelResponse::saying("done")]);
    let mut session = Session::new(&provider, &ApproveAll, workspace, config(&fixed));
    session.run("write a file").unwrap();

    let record = RunRecord::read(&fixed.services.ledger).unwrap();
    let proof = Predicate::compile("file_exists(never-existed.txt)").unwrap();
    let bisection = bisect(&record, &proof, &fixed.services, &fixed.root.join("bisect")).unwrap();

    assert!(!bisection.held_at_start);
    assert_eq!(bisection.first_bad_turn, None);
    assert!(bisection.describe().contains("did not hold at the start"));
}

/// §5.8. A run becomes a regression test, self-contained enough to commit.
#[test]
fn a_run_freezes_into_a_fixture_that_replays_on_its_own() {
    let (fixed, workspace) = services(&[("src/a.txt", "start\n")]);
    let provider = ScriptedProvider::new([
        ModelResponse::calling(
            "d",
            "declare",
            json!({ "assertion": "b.txt exists", "proof": "file_exists(b.txt)" }),
        ),
        write("b.txt", "created\n"),
        ModelResponse::calling("a", "attest", json!({})),
        ModelResponse::saying("done"),
    ]);
    let mut session = Session::new(&provider, &ApproveAll, workspace, config(&fixed));
    let outcome = session.run("create b.txt").unwrap();
    let final_tree = session.workspace().observe().unwrap().root_hash();

    let record = RunRecord::read(&fixed.services.ledger).unwrap();
    let fixture = Fixture::freeze(
        record,
        final_tree,
        outcome.turns,
        outcome.discharged.iter().filter(|c| c.warranted).count(),
        fixed.services.store.as_ref(),
    )
    .unwrap();

    // A fixture is one file. Round-trip it the way a repository would.
    let encoded = serde_json::to_string(&fixture).unwrap();
    let restored: Fixture = serde_json::from_str(&encoded).unwrap();
    assert_eq!(restored, fixture);

    // Replay it into a world that shares nothing with the original — a fresh
    // store, a fresh ledger, a fresh directory.
    let elsewhere = tempfile::tempdir().unwrap();
    let fresh = Services::new(
        Arc::new(MemoryStore::new()),
        Arc::new(Ledger::open(elsewhere.path().join(".warrant")).unwrap()),
        Arc::new(Attestor::new().unwrap()),
        Policy::default(),
    );

    let reproduction = restored.replay(&fresh, &elsewhere.path().join("replay")).unwrap();
    assert!(reproduction.reproduced, "{}", reproduction.describe());
    assert_eq!(reproduction.actual_turns, outcome.turns);
    assert_eq!(reproduction.actual_warranted, 1);
}

#[test]
fn a_fixture_from_a_future_version_is_refused_rather_than_misread() {
    let (fixed, workspace) = services(&[("a.txt", "x\n")]);
    let provider = ScriptedProvider::new([ModelResponse::saying("nothing to do")]);
    let mut session = Session::new(&provider, &ApproveAll, workspace, config(&fixed));
    let outcome = session.run("do nothing").unwrap();
    let tree = session.workspace().observe().unwrap().root_hash();

    let record = RunRecord::read(&fixed.services.ledger).unwrap();
    let mut fixture =
        Fixture::freeze(record, tree, outcome.turns, 0, fixed.services.store.as_ref()).unwrap();
    fixture.version = 99;

    let store = MemoryStore::new();
    let error = fixture.hydrate(&store).unwrap_err();
    assert!(error.to_string().contains("version 99"), "{error}");
}

/// The policy is part of what a run *was*, so a replay uses the recorded one
/// rather than whatever the reader happens to be holding.
///
/// This is a regression test for a real bug: a command that opened the record
/// read-only replayed a run in which every write was refused, and produced a
/// confidently wrong answer. The strict request check caught it, which is
/// exactly what it is for.
#[test]
fn a_replay_uses_the_policy_the_run_had_not_the_readers() {
    let (fixed, workspace) = services(&[("src/a.txt", "start\n")]);
    let provider = ScriptedProvider::new([
        write("written.txt", "the run could write\n"),
        ModelResponse::saying("done"),
    ]);
    let mut session = Session::new(&provider, &ApproveAll, workspace, config(&fixed));
    session.run("write a file").unwrap();

    let record = RunRecord::read(&fixed.services.ledger).unwrap();
    assert!(record.policy.allow_writes, "the run was allowed to write");

    // A reader that opened everything read-only. Under the reader's policy the
    // write would be refused and the very next request would differ.
    let reading = Services { policy: Policy::read_only(), ..fixed.services.clone() };
    let (state, outcome) =
        warrant_agent::replay_prefix(&record, record.len(), &reading, &fixed.root.join("replay"))
            .expect("the recorded policy should be used, so the replay should not diverge");

    assert_eq!(outcome.turns, 2);
    assert!(state.files.contains_key("written.txt"), "the replay reproduced the write");
}

/// §5.6. Failed claims stay in the record with their evidence, so the same
/// dead approach is not rediscovered next session.
#[test]
fn failed_claims_are_queryable_afterwards() {
    let (fixed, workspace) = services(&[("src/a.txt", "start\n")]);
    let provider = ScriptedProvider::new([
        ModelResponse::calling(
            "d1",
            "declare",
            json!({ "assertion": "the wrong idea", "proof": "file_exists(never.txt)" }),
        ),
        write("something-else.txt", "x\n"),
        ModelResponse::calling("a1", "attest", json!({})),
        ModelResponse::calling(
            "d2",
            "declare",
            json!({ "assertion": "the right idea", "proof": "file_exists(b.txt)" }),
        ),
        write("b.txt", "created\n"),
        ModelResponse::calling("a2", "attest", json!({})),
        ModelResponse::saying("done"),
    ]);
    let mut session = Session::new(&provider, &ApproveAll, workspace, config(&fixed));
    let outcome = session.run("try twice").unwrap();

    assert_eq!(outcome.discharged.len(), 2);
    assert!(!outcome.discharged[0].warranted);
    assert!(outcome.discharged[1].warranted);

    let refuted = refutations(&fixed.services.ledger).unwrap();
    assert_eq!(refuted.len(), 1, "only the failed claim is a refutation");
    assert!(refuted[0].outcome.contains("does not hold"), "{}", refuted[0].outcome);

    // And the record it came from is still intact.
    assert_eq!(fixed.services.ledger.verify_deep().unwrap(), fixed.services.ledger.len().unwrap());
}
