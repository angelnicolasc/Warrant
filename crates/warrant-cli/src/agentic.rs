//! The commands that drive a model: `run`, `do`, `bisect`, `freeze`,
//! `replay` and `refutations`.
//!
//! `wrap` and `map` need no model and are the ones most people will use.
//! These are the other half — Warrant as a harness rather than as an
//! instrument pointed at somebody else's harness.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use warrant_agent::anthropic::AnthropicProvider;
use warrant_agent::{
    ApproveAll, ApproveWithin, Approver, AttemptConfig, BestOfN, Fixture, Policy, RunRecord,
    Services, Session, SessionConfig, Workspace, bisect, refutations,
};
use warrant_attest::{Attestor, Predicate};
use warrant_cell::{Cell, WorkspaceCell};
use warrant_diff::ContentStore;
use warrant_ledger::Ledger;

use crate::render::{Glyphs, render_map};

/// Options shared by the commands that drive a model.
#[derive(clap::Args, Clone, Debug)]
pub struct AgentOptions {
    /// Which model to ask.
    #[arg(long, default_value = "claude-opus-5", value_name = "ID")]
    pub model: String,

    /// Hard ceiling on turns.
    #[arg(long, default_value_t = 40, value_name = "N")]
    pub max_turns: u32,

    /// Ceiling on total tokens.
    #[arg(long, value_name = "N")]
    pub max_tokens: Option<u64>,

    /// Turns without progress before the run is called stuck.
    #[arg(long, default_value_t = 5, value_name = "N")]
    pub stuck_after: u32,

    /// Hosts the agent may reach. Repeatable. Egress is denied otherwise.
    #[arg(long = "allow-host", value_name = "HOST")]
    pub allowed_hosts: Vec<String>,

    /// Most files one turn may touch before it is rolled back.
    #[arg(long, default_value_t = 25, value_name = "N")]
    pub max_files_per_turn: usize,

    /// Let the agent read but not write.
    #[arg(long)]
    pub read_only: bool,

    /// Keep every turn regardless of how much it changed.
    #[arg(long)]
    pub no_blast_radius: bool,
}

impl AgentOptions {
    fn policy(&self) -> Policy {
        let mut policy = if self.read_only { Policy::read_only() } else { Policy::default() };
        policy.allowed_hosts = self.allowed_hosts.clone();
        policy
    }

    fn session(&self, work_root: PathBuf) -> SessionConfig {
        let mut config = SessionConfig::new(&self.model, work_root);
        config.max_turns = self.max_turns;
        config.stuck_patience = self.stuck_after;
        config.budget.tokens = self.max_tokens;
        config
    }

    fn approver(&self) -> Box<dyn Approver> {
        if self.no_blast_radius {
            return Box::new(ApproveAll);
        }
        Box::new(ApproveWithin {
            max_files: self.max_files_per_turn,
            allow_deletions: true,
            // A turn may reach the network only if the operator named a host
            // it is allowed to reach.
            allow_egress: !self.allowed_hosts.is_empty(),
            allow_verification_edits: true,
        })
    }
}

/// Everything the agentic commands need from a repository.
struct Bench {
    ledger: Arc<Ledger>,
    services: Services,
    /// Under `.warrant`, so probe cells and subagent cells are never visible
    /// to a snapshot of the repository they are probing.
    work_root: PathBuf,
}

fn bench(root: &Path, policy: Policy) -> Result<Bench> {
    let ledger = Arc::new(Ledger::open_for_repo(root)?);
    let blobs: Arc<dyn ContentStore> =
        Arc::new(warrant_ledger::BlobStore::open(ledger.root().join("blobs"))?);
    let services = Services::new(blobs, Arc::clone(&ledger), Arc::new(Attestor::new()?), policy);
    Ok(Bench { work_root: ledger.root().join("work"), ledger, services })
}

fn provider() -> Result<AnthropicProvider> {
    AnthropicProvider::from_env().context("starting the model provider")
}

/// `warrant run` — drive a model against this repository.
pub fn run(
    root: &Path,
    task: &str,
    options: &AgentOptions,
    receipt_path: Option<&Path>,
    strict: bool,
    glyphs: &Glyphs,
) -> Result<std::process::ExitCode> {
    let bench = bench(root, options.policy())?;
    let provider = provider()?;
    let approver = options.approver();

    // The agent works in the operator's actual checkout. Probe cells and
    // subagents live under `.warrant`, which no snapshot ever sees.
    let cell = WorkspaceCell::adopt(
        root,
        Arc::clone(&bench.services.store),
        warrant_agent::cell_scan_options(),
    )?;
    let isolation = cell.isolation();
    let workspace = Workspace::new(Arc::new(Mutex::new(cell)), bench.services.clone())?;

    crate::pipeline::record_git_state(&bench.ledger, root, "start")?;

    println!();
    let mut session = Session::new(
        &provider,
        approver.as_ref(),
        workspace,
        options.session(bench.work_root.clone()),
    );
    let outcome = session.run(task)?;

    crate::pipeline::record_git_state(&bench.ledger, root, "finish")?;

    println!(
        "  {} {} in {} turns, {} tokens",
        if outcome.stop.is_clean() { glyphs.tick } else { glyphs.warn },
        outcome.stop.describe(),
        outcome.turns,
        outcome.usage.total()
    );
    if outcome.rejected_turns > 0 {
        println!(
            "  {} {} turn(s) rolled back by the blast-radius limit",
            glyphs.warn, outcome.rejected_turns
        );
    }
    if !outcome.final_message.trim().is_empty() {
        println!("\n  agent says            {}", first_line(&outcome.final_message));
    }

    if outcome.discharged.is_empty() {
        println!("\n  nothing was claimed, so nothing was measured.");
        println!("  An agent that never calls `declare` produces no proof map — that is itself");
        println!("  the finding, and it is why the run is reported rather than summarised.\n");
        return Ok(exit_for(strict, false, true));
    }

    let mut any_tampering = false;
    for claim in &outcome.discharged {
        println!("\n  claim: {}", claim.assertion);
        print!("{}", render_map(&claim.map, &claim.source, false, glyphs));
        any_tampering |= claim.map.has_tampering();
    }
    println!();

    if let (Some(path), Some(last)) = (receipt_path, outcome.discharged.last()) {
        let receipt = crate::pipeline::issue_receipt(
            &last.map,
            &last.source,
            false,
            isolation,
            Some(task.to_owned()),
            Some(format!("warrant run ({})", options.model)),
            &bench.ledger,
        )?;
        std::fs::write(path, receipt.to_json()?)
            .with_context(|| format!("writing the receipt to {}", path.display()))?;
        println!("  receipt written to {}\n", path.display());
    }

    let all_warranted = outcome.all_warranted();
    Ok(exit_for(strict, any_tampering, !all_warranted))
}

/// `warrant do` — N attempts at one claim, adjudicated.
#[allow(clippy::too_many_arguments)]
pub fn best_of_n(
    root: &Path,
    task: &str,
    proof: &str,
    attempts: u32,
    options: &AgentOptions,
    apply: bool,
    strict: bool,
    glyphs: &Glyphs,
) -> Result<std::process::ExitCode> {
    let bench = bench(root, options.policy())?;
    let provider = provider()?;
    let approver = options.approver();

    let mut origin_cell = WorkspaceCell::adopt(
        root,
        Arc::clone(&bench.services.store),
        warrant_agent::cell_scan_options(),
    )?;
    let origin = origin_cell.snapshot()?.as_snapshot().clone();
    let isolation = origin_cell.isolation();

    let mut config = AttemptConfig::new(attempts, task, proof);
    config.prune_patience = Some(3);

    println!();
    println!("  {} attempts from an identical starting state", attempts);
    println!("  proof: {proof}\n");

    let best = BestOfN::new(
        &provider,
        approver.as_ref(),
        bench.services.clone(),
        options.session(bench.work_root.clone()),
        bench.work_root.join("attempts"),
    );
    let (adjudication, states) = best.run(&origin, task, &config)?;

    for attempt in &adjudication.attempts {
        let marker = if Some(attempt.index) == adjudication.winner { glyphs.tick } else { " " };
        println!("  {marker} {}", attempt.summary());
    }
    println!(
        "\n  {} of {} discharged the claim, {} tokens in total",
        adjudication.warranted_count(),
        adjudication.attempts.len(),
        adjudication.total_tokens()
    );

    let Some(winner) = adjudication.winner else {
        println!(
            "\n  Nothing was proven, so nothing is offered. Every attempt stays in the record"
        );
        println!("  with its evidence.\n");
        return Ok(exit_for(strict, false, true));
    };

    println!("\n  attempt {winner} wins.");
    if apply {
        let state = &states[winner as usize];
        state.materialize_into(root, &origin, bench.services.store.as_ref())?;
        println!("  Its result is now in your working tree.\n");
    } else {
        println!("  Re-run with --apply to put its result into your working tree.\n");
    }

    // The winner's evidence is what a reviewer reads.
    let _ = isolation;
    Ok(exit_for(strict, false, false))
}

/// `warrant bisect` — find the turn a recorded run stopped satisfying a proof.
pub fn bisect_run(
    root: &Path,
    proof_source: &str,
    glyphs: &Glyphs,
) -> Result<std::process::ExitCode> {
    let bench = bench(root, Policy::read_only())?;
    let record = RunRecord::read(&bench.ledger)?;
    if record.is_empty() {
        bail!("this record contains no model turns to bisect");
    }
    let proof = Predicate::compile(proof_source)
        .with_context(|| format!("compiling the proof `{proof_source}`"))?;

    println!();
    println!("  replaying {} turns of `{}`", record.len(), record.task);

    let bisection = bisect(&record, &proof, &bench.services, &bench.work_root.join("bisect"))?;

    let marker = if bisection.first_bad_turn.is_some() { glyphs.warn } else { glyphs.tick };
    println!("  {marker} {}\n", bisection.describe());
    Ok(if bisection.first_bad_turn.is_some() {
        std::process::ExitCode::from(2)
    } else {
        std::process::ExitCode::SUCCESS
    })
}

/// `warrant freeze` — turn the recorded run into a replayable fixture.
pub fn freeze(root: &Path, out: &Path, glyphs: &Glyphs) -> Result<std::process::ExitCode> {
    let bench = bench(root, Policy::default())?;
    let record = RunRecord::read(&bench.ledger)?;

    // Replay the run once to establish what the fixture must reproduce.
    let (state, outcome) = warrant_agent::replay_prefix(
        &record,
        record.len(),
        &bench.services,
        &bench.work_root.join("freeze"),
    )?;
    let fixture = Fixture::freeze(
        record,
        state.root_hash(),
        outcome.turns,
        outcome.discharged.iter().filter(|claim| claim.warranted).count(),
        bench.services.store.as_ref(),
    )?;

    std::fs::write(out, serde_json::to_string_pretty(&fixture)?)
        .with_context(|| format!("writing the fixture to {}", out.display()))?;

    println!();
    println!(
        "  {} froze {} turns into {} ({} files of starting state)",
        glyphs.tick,
        fixture.record.len(),
        out.display(),
        fixture.record.origin.len()
    );
    println!("  Replay it anywhere with `warrant replay {}`\n", out.display());
    Ok(std::process::ExitCode::SUCCESS)
}

/// `warrant replay` — check that a frozen run still reproduces.
pub fn replay(root: &Path, path: &Path, glyphs: &Glyphs) -> Result<std::process::ExitCode> {
    let bench = bench(root, Policy::default())?;
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let fixture: Fixture = serde_json::from_slice(&bytes)
        .with_context(|| format!("{} is not a Warrant fixture", path.display()))?;

    let reproduction = fixture.replay(&bench.services, &bench.work_root.join("replay"))?;

    println!();
    let marker = if reproduction.reproduced { glyphs.tick } else { glyphs.warn };
    println!("  {marker} {}\n", reproduction.describe());
    Ok(if reproduction.reproduced {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::from(2)
    })
}

/// `warrant refutations` — the approaches already known not to work here.
pub fn list_refutations(root: &Path, glyphs: &Glyphs) -> Result<std::process::ExitCode> {
    let bench = bench(root, Policy::read_only())?;
    let found = refutations(&bench.ledger)?;

    println!();
    if found.is_empty() {
        println!("  {} no failed claims in this record\n", glyphs.tick);
        return Ok(std::process::ExitCode::SUCCESS);
    }

    for refutation in &found {
        println!(
            "  {:>5}  {}  {}",
            refutation.entry,
            warrant_core::format_rfc3339(refutation.at_ms),
            refutation.outcome
        );
        println!("         proof {}", refutation.proof);
        if !refutation.laundered.is_empty() {
            println!("         {} laundered: {}", glyphs.warn, refutation.laundered.join(", "));
        }
    }
    println!(
        "\n  {} failed claim(s). Every memory system stores successes; this stores what did not work,",
        found.len()
    );
    println!("  which is why a later session need not rediscover it.\n");
    Ok(std::process::ExitCode::SUCCESS)
}

fn first_line(text: &str) -> String {
    text.lines().find(|line| !line.trim().is_empty()).unwrap_or_default().trim().to_owned()
}

fn exit_for(strict: bool, tampering: bool, unproven: bool) -> std::process::ExitCode {
    if strict && (tampering || unproven) {
        std::process::ExitCode::from(2)
    } else {
        std::process::ExitCode::SUCCESS
    }
}
