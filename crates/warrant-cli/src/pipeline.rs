//! The shared pipeline: two observed states in, a proof map and a receipt out.
//!
//! `wrap` and `map` differ only in where the *before* state comes from — one
//! takes it immediately before launching an agent, the other checks it out of
//! git. Everything after that point is identical, which is why it lives here
//! rather than twice.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use warrant_attest::{Attestor, Predicate};
use warrant_cell::{Cell, IsolationReport, WorkspaceCell};
use warrant_core::{ProofTier, now_ms};
use warrant_diff::{ContentStore, OverlayDiff, ScanOptions, Snapshot};
use warrant_ledger::{EntryKind, Ledger};
use warrant_necessity::{NecessityConfig, NecessityMap, Search};
use warrant_receipt::{ProofSummary, ReceiptFile, RunSummary, SigningIdentity, Statement};

use crate::repo::{DetectedSuite, default_proof, detect_test_command};

/// What to map, and how.
pub struct MapRequest {
    /// Repository root.
    pub root: PathBuf,
    /// State before the work.
    pub before: Snapshot,
    /// State after.
    pub after: Snapshot,
    /// Proof written by the operator. `None` uses the repository's own suite.
    pub proof: Option<String>,
    /// Ceiling on probes.
    pub max_probes: Option<u32>,
    /// Per-command timeout inside a probe.
    pub timeout_ms: Option<u64>,
    /// The task as stated.
    pub task: Option<String>,
    /// The agent that did the work.
    pub harness: Option<String>,
    /// Isolation the *work* was done under, which is not necessarily the
    /// isolation the probes run under.
    pub isolation: IsolationReport,
}

/// Everything a caller needs to report and to record.
pub struct MapResult {
    /// The map.
    pub map: NecessityMap,
    /// The signed receipt.
    pub receipt: ReceiptFile,
    /// The proof, as declared.
    pub proof_source: String,
    /// Whether it came from the repository rather than the operator.
    pub proof_defaulted: bool,
}

/// Resolve the proof: the operator's if given, the repository's otherwise.
pub fn resolve_proof(
    root: &Path,
    requested: Option<&str>,
) -> Result<(String, bool, Option<DetectedSuite>)> {
    if let Some(source) = requested {
        return Ok((source.to_owned(), false, None));
    }
    match detect_test_command(root) {
        Some(suite) => Ok((default_proof(&suite), true, Some(suite))),
        None => bail!(
            "no test command could be detected in {}.\n\
             Warrant's default proof is whatever your repository already runs, and nothing \
             recognisable is here.\n\
             Pass one explicitly, for example:\n    \
             --proof 'exit(pytest -q) == 0'",
            root.display()
        ),
    }
}

/// Run the search and issue a receipt.
pub fn execute(
    request: MapRequest,
    ledger: &Ledger,
    blobs: Arc<dyn ContentStore>,
) -> Result<MapResult> {
    let (proof_source, proof_defaulted, _) =
        resolve_proof(&request.root, request.proof.as_deref())?;
    let predicate = Predicate::compile(&proof_source)
        .with_context(|| format!("compiling the proof `{proof_source}`"))?;

    // The sealed module goes into the record before anything runs, so the
    // proof that produced the number is recoverable from the ledger alone.
    ledger.append(EntryKind::PredicateSealed, predicate.wasm(), now_ms())?;

    let diff = OverlayDiff::between(&request.before, &request.after, blobs.as_ref())?;
    ledger.append_json(EntryKind::OverlayDiff, &diff, now_ms())?;

    // Probes run in their own cell. The operator's working tree is never the
    // thing being reverted and re-run — a search that mutated it would be a
    // search nobody would leave enabled.
    //
    // The cell honours the repository's own ignore rules, which arrive inside
    // the pre-image as a tracked `.gitignore`. That is what keeps build output
    // out of the snapshot: without it, a probe would delete `target/` on every
    // restore and each one would pay for a full rebuild. Ancestor ignore files
    // are *not* consulted, because the repository's `.gitignore` excludes
    // `.warrant/` and this cell lives underneath it.
    let probe_root = request.root.join(".warrant").join("probes").join(diff.post_root.short());
    std::fs::create_dir_all(&probe_root)
        .with_context(|| format!("creating the probe cell at {}", probe_root.display()))?;
    let probe_scan = ScanOptions { use_parent_ignores: false, ..ScanOptions::default() };
    let mut probe_cell = WorkspaceCell::adopt(&probe_root, Arc::clone(&blobs), probe_scan)?;
    probe_cell.restore(&request.before)?;
    ledger.append_json(
        EntryKind::CellCreated,
        &serde_json::json!({ "purpose": "necessity-probe", "id": probe_cell.id().to_string() }),
        now_ms(),
    )?;

    let defaults = NecessityConfig::default();
    let config = NecessityConfig {
        max_probes: request.max_probes,
        command_timeout_ms: request.timeout_ms.or(defaults.command_timeout_ms),
        ..defaults
    };

    let attestor = Attestor::new()?;
    let shared: Arc<Mutex<dyn Cell>> = Arc::new(Mutex::new(probe_cell));
    let mut search = Search::new(
        shared,
        &request.before,
        &request.after,
        &diff,
        blobs.as_ref(),
        &attestor,
        &predicate,
        &config,
    );
    let map = search.run()?;

    for record in search.commands() {
        ledger.append_json(EntryKind::Probe, record, now_ms())?;
    }
    ledger.append_json(EntryKind::NecessityMapped, &map, now_ms())?;
    if !map.satisfied {
        ledger.append_json(EntryKind::Refutation, &map, now_ms())?;
    }

    let receipt = issue_receipt(
        &map,
        &proof_source,
        proof_defaulted,
        request.isolation,
        request.task.clone(),
        request.harness.clone(),
        ledger,
    )?;

    Ok(MapResult { map, receipt, proof_source, proof_defaulted })
}

/// Sign a map into a receipt and record that it was issued.
///
/// Separated from `execute` because `warrant run` reaches the same point by a
/// different route: several claims, each with its own map, rather than one
/// map over a whole delta.
pub fn issue_receipt(
    map: &NecessityMap,
    proof_source: &str,
    proof_defaulted: bool,
    isolation: IsolationReport,
    task: Option<String>,
    harness: Option<String>,
    ledger: &Ledger,
) -> Result<ReceiptFile> {
    let predicate = Predicate::compile(proof_source)
        .with_context(|| format!("compiling the proof `{proof_source}`"))?;

    let mut run = RunSummary::between(map.pre_root, map.post_root);
    if let Some(task) = task {
        run = run.with_task(task);
    }
    if let Some(harness) = harness {
        run = run.with_harness(harness);
    }
    if let Some(claim) = map.claim {
        run = run.with_claim(claim);
    }

    let statement = Statement::from_map(
        map,
        ProofSummary {
            predicate: predicate.hash(),
            source: proof_source.to_owned(),
            tier: ProofTier::Unit,
            commands: predicate.commands(),
            defaulted: proof_defaulted,
        },
        isolation,
        run,
        ledger.checkpoint()?,
        now_ms(),
    );

    let identity = SigningIdentity::load_or_create(warrant_receipt::key_path(ledger.root()))?;
    let receipt = ReceiptFile::issue(&statement, &identity);
    ledger.append(EntryKind::ReceiptIssued, receipt.to_json()?.as_bytes(), now_ms())?;
    Ok(receipt)
}

/// Record what git currently believes, so a later rewrite is detectable.
///
/// Warrant treats git as an output. This is the entry that makes a force-push
/// visible: the commits reachable at this moment, written somewhere a
/// `git push --force` cannot reach.
///
/// It has its own entry kind. Writing it as a `RunStarted` — which an earlier
/// version did — meant two unrelated payload shapes shared a label, and
/// anything reading the log for a run header found repository state instead.
pub fn record_git_state(ledger: &Ledger, root: &Path, phase: &str) -> Result<()> {
    if !crate::repo::is_git_repo(root) {
        return Ok(());
    }
    let state = serde_json::json!({
        "phase": phase,
        "head": crate::repo::head_commit(root),
        "recent": crate::repo::recent_commits(root, 32),
    });
    ledger.append_json(EntryKind::RepoState, &state, now_ms())?;
    Ok(())
}

/// Open the content store shared by cells, diffs and the ledger.
pub fn open_blobs(ledger: &Ledger) -> Result<Arc<dyn ContentStore>> {
    Ok(Arc::new(warrant_ledger::BlobStore::open(ledger.root().join("blobs"))?))
}
