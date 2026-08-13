//! `warrant` — run any agent inside a cell and get a proof map back.

#![forbid(unsafe_code)]

mod agentic;
mod pipeline;
mod render;
mod repo;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use warrant_cell::{Cell, CommandSpec, WorkspaceCell};
use warrant_core::now_ms;
use warrant_diff::{ScanOptions, Snapshot};
use warrant_ledger::{EntryKind, Ledger};
use warrant_necessity::MapOutcome;
use warrant_receipt::ReceiptFile;

use pipeline::{MapRequest, execute, open_blobs, record_git_state};
use render::{Glyphs, render_map};

#[derive(Parser)]
#[command(
    name = "warrant",
    version,
    about = "Your agent says the tests pass. Warrant tells you whether the tests are why.",
    long_about = "Warrant reverts an agent's changes one group at a time and re-runs the proof \
                  it declared before it started working. Whatever you can revert without \
                  breaking the proof was never proven by it."
)]
struct Cli {
    /// Repository to work in. Defaults to the current directory.
    #[arg(long, global = true, value_name = "PATH")]
    repo: Option<PathBuf>,

    /// Use only ASCII characters in output.
    #[arg(long, global = true)]
    ascii: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run an agent inside a cell and map what it changed.
    ///
    /// Nobody has to switch harnesses to get a proof map. Point this at the
    /// agent you already run.
    Wrap {
        /// The agent to run, for example `claude-code` or `codex`.
        harness: String,

        /// Arguments passed to the agent, after `--`.
        #[arg(last = true)]
        args: Vec<String>,

        #[command(flatten)]
        mapping: MappingOptions,
    },

    /// Map changes already in the working tree against a git reference.
    Map {
        /// What to compare against.
        #[arg(long, default_value = "HEAD", value_name = "REF")]
        against: String,

        #[command(flatten)]
        mapping: MappingOptions,
    },

    /// Cut the change down to the part the proof depends on, verified.
    ///
    /// Maps first, then rebuilds the tree from the load-bearing hunks alone
    /// and re-runs the proof on it. What comes off is unproven, which is not
    /// the same as unwanted — a comment, a log line or a feature with no test
    /// all live there — so this shows the plan and writes nothing unless you
    /// pass --write.
    Trim {
        /// What to compare against.
        #[arg(long, default_value = "HEAD", value_name = "REF")]
        against: String,

        /// Put the trimmed tree into the working directory.
        #[arg(long)]
        write: bool,

        #[command(flatten)]
        mapping: MappingOptions,
    },

    /// Drive a model against this repository, under a claim it declares itself.
    ///
    /// Works with any provider: a local server, or any hosted endpoint on the
    /// OpenAI chat-completions format or Anthropic Messages. See --provider.
    ///
    /// `warrant wrap` needs no model at all and works with the agent you
    /// already have configured.
    Run {
        /// What to do.
        task: String,

        #[command(flatten)]
        agent: agentic::AgentOptions,

        /// Where to write the receipt.
        #[arg(long, value_name = "PATH")]
        receipt: Option<PathBuf>,

        /// Exit non-zero on a finding.
        #[arg(long)]
        strict: bool,
    },

    /// Run several attempts at one claim and keep only the proven one.
    #[command(name = "do")]
    BestOfN {
        /// What to do.
        task: String,

        /// The proof every attempt is judged by. Required: without a shared
        /// proof the attempts cannot be compared.
        #[arg(long, value_name = "EXPR")]
        proof: String,

        /// How many attempts to run.
        #[arg(long, default_value_t = 5, value_name = "N")]
        attempts: u32,

        /// Put the winning attempt into the working tree.
        #[arg(long)]
        apply: bool,

        #[command(flatten)]
        agent: agentic::AgentOptions,

        /// Exit non-zero if nothing was proven.
        #[arg(long)]
        strict: bool,
    },

    /// Find the turn a recorded run stopped satisfying a proof.
    Bisect {
        /// The proof to search against.
        #[arg(long, value_name = "EXPR")]
        proof: String,
    },

    /// Turn the recorded run into a replayable fixture.
    Freeze {
        /// Where to write it.
        #[arg(long, short, default_value = "warrant-fixture.json", value_name = "PATH")]
        out: PathBuf,
    },

    /// Check that a frozen run still reproduces.
    Replay {
        /// The fixture.
        path: PathBuf,
    },

    /// List the claims that failed here, with their evidence.
    Refutations,

    /// Compile a proof and show exactly what it will run.
    Proof {
        /// The proof expression. Omit to show the one this repository defaults to.
        expression: Option<String>,
    },

    /// Inspect the record.
    Log {
        /// Check the hash chain and every stored payload.
        #[arg(long)]
        verify: bool,

        /// Report where repository history no longer matches the record.
        #[arg(long)]
        diverged: bool,

        /// How many entries to show.
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },

    /// Check a receipt someone else produced.
    Verify {
        /// Path to the receipt JSON.
        path: PathBuf,

        /// Public key the receipt must have been signed with, as hex.
        #[arg(long, value_name = "HEX")]
        key: Option<String>,
    },
}

#[derive(clap::Args)]
struct MappingOptions {
    /// The proof. Defaults to the repository's own test command.
    #[arg(long, value_name = "EXPR")]
    proof: Option<String>,

    /// Ceiling on necessity probes.
    #[arg(long, value_name = "N")]
    max_probes: Option<u32>,

    /// How many probes to run at once. Defaults to half the machine, capped
    /// at four. `--jobs 1` maps sequentially.
    #[arg(long, value_name = "N")]
    jobs: Option<usize>,

    /// Per-command timeout inside a probe, in seconds.
    #[arg(long, value_name = "SECONDS")]
    timeout: Option<u64>,

    /// Where to write the receipt.
    #[arg(long, value_name = "PATH")]
    receipt: Option<PathBuf>,

    /// Emit the map as JSON instead of a rendered report.
    #[arg(long)]
    json: bool,

    /// Emit the map as Markdown, for a pull-request comment.
    #[arg(long, conflicts_with = "json")]
    markdown: bool,

    /// Also write the map as JSON to a file.
    ///
    /// A search costs one run of the test command per probe, so asking for a
    /// second rendering must never mean asking for a second search. Every
    /// output form comes off the same map.
    #[arg(long, value_name = "PATH")]
    out_json: Option<PathBuf>,

    /// Also write the Markdown comment body to a file.
    #[arg(long, value_name = "PATH")]
    out_markdown: Option<PathBuf>,

    /// Append this run's step outputs to a file, normally `$GITHUB_OUTPUT`.
    ///
    /// The numbers a workflow branches on come off the map itself rather than
    /// being re-derived in shell. The difference is not cosmetic: summing
    /// `proven_lines` over `changed_lines` in `jq` publishes `0` for a vacuous
    /// proof, where the map publishes nothing at all. Those are different
    /// claims — one says the suite proved none of this change, the other says
    /// the question could not be asked — and `Ratio::UNDEFINED` exists so the
    /// first never renders as the second.
    #[arg(long, value_name = "PATH")]
    github_output: Option<PathBuf>,

    /// Exit non-zero on a finding, for continuous integration.
    #[arg(long)]
    strict: bool,

    /// Under `--strict`, the coverage below which the run fails.
    #[arg(long, value_name = "PERCENT")]
    min_coverage: Option<u32>,

    /// Print the agent's own output.
    #[arg(long)]
    verbose: bool,
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("warrant: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ExitCode> {
    let cli = Cli::parse();
    let glyphs = if cli.ascii { Glyphs::ASCII } else { Glyphs::UNICODE };
    let start = match &cli.repo {
        Some(path) => path.clone(),
        None => std::env::current_dir().context("reading the current directory")?,
    };
    let root = repo::find_root(&start)?;

    match cli.command {
        Command::Wrap { harness, args, mapping } => {
            cmd_wrap(&root, &harness, &args, &mapping, &glyphs)
        }
        Command::Map { against, mapping } => cmd_map(&root, &against, &mapping, &glyphs),
        Command::Trim { against, write, mapping } => {
            cmd_trim(&root, &against, write, &mapping, &glyphs)
        }
        Command::Run { task, agent, receipt, strict } => {
            agentic::run(&root, &task, &agent, receipt.as_deref(), strict, &glyphs)
        }
        Command::BestOfN { task, proof, attempts, apply, agent, strict } => {
            agentic::best_of_n(&root, &task, &proof, attempts, &agent, apply, strict, &glyphs)
        }
        Command::Bisect { proof } => agentic::bisect_run(&root, &proof, &glyphs),
        Command::Freeze { out } => agentic::freeze(&root, &out, &glyphs),
        Command::Replay { path } => agentic::replay(&root, &path, &glyphs),
        Command::Refutations => agentic::list_refutations(&root, &glyphs),
        Command::Proof { expression } => cmd_proof(&root, expression.as_deref()),
        Command::Log { verify, diverged, limit } => {
            cmd_log(&root, verify, diverged, limit, &glyphs)
        }
        Command::Verify { path, key } => cmd_verify(&path, key.as_deref(), &glyphs),
    }
}

fn cmd_wrap(
    root: &std::path::Path,
    harness: &str,
    args: &[String],
    options: &MappingOptions,
    glyphs: &Glyphs,
) -> Result<ExitCode> {
    let ledger = Ledger::open_for_repo(root)?;
    let blobs = open_blobs(&ledger)?;

    ledger.append_json(
        EntryKind::RunStarted,
        &serde_json::json!({ "harness": harness, "args": args, "root": root.to_string_lossy() }),
        now_ms(),
    )?;
    record_git_state(&ledger, root, "start")?;

    // The agent works in the operator's actual repository. That is what they
    // asked for, and the pre-image is captured before it starts.
    let mut agent_cell = WorkspaceCell::adopt(root, Arc::clone(&blobs), ScanOptions::default())?;
    let isolation = agent_cell.isolation();

    let before = agent_cell.snapshot()?;
    ledger.append_json(EntryKind::CellSnapshot, before.as_snapshot(), now_ms())?;

    let mut spec =
        CommandSpec::new(std::iter::once(harness.to_owned()).chain(args.iter().cloned()));
    if let Some(seconds) = options.timeout {
        spec = spec.with_timeout_ms(seconds * 1000);
    }
    ledger.append_json(EntryKind::ToolCall, &spec, now_ms())?;

    println!();
    let record = agent_cell.exec(&spec).with_context(|| format!("running `{harness}`"))?;
    ledger.append_json(EntryKind::ToolResult, &record, now_ms())?;

    if options.verbose {
        print_captured(&blobs, &record);
    }
    let agent_status = if record.succeeded() {
        format!("done {}", glyphs.tick)
    } else if record.timed_out {
        "timed out".to_string()
    } else {
        format!("exit {}", record.code.unwrap_or(-1))
    };
    println!("  agent says            {agent_status}");

    let after = agent_cell.snapshot()?;
    ledger.append_json(EntryKind::CellSnapshot, after.as_snapshot(), now_ms())?;

    let request = MapRequest {
        root: root.to_path_buf(),
        before: before.as_snapshot().clone(),
        after: after.as_snapshot().clone(),
        proof: options.proof.clone(),
        max_probes: options.max_probes,
        parallelism: options.jobs,
        timeout_ms: options.timeout.map(|s| s * 1000),
        task: (!args.is_empty()).then(|| args.join(" ")),
        harness: Some(harness.to_owned()),
        isolation,
    };

    // The search restores the probe cell, never this one — but the operator's
    // tree is the one thing that must be exactly as the agent left it, so it
    // is put back explicitly rather than trusted to have been untouched.
    let outcome = execute(request, &ledger, Arc::clone(&blobs));
    agent_cell.restore(after.as_snapshot())?;
    let outcome = outcome?;

    record_git_state(&ledger, root, "finish")?;
    ledger.append_json(
        EntryKind::RunFinished,
        &serde_json::json!({ "outcome": outcome.map.outcome }),
        now_ms(),
    )?;

    report(&outcome, options, glyphs)
}

/// The two states `map` and `trim` both work from.
struct Endpoints {
    before: Snapshot,
    after: Snapshot,
    isolation: warrant_cell::IsolationReport,
}

/// Check out a reference and observe both ends of the change.
fn endpoints(
    root: &std::path::Path,
    against: &str,
    command: &str,
    blobs: &Arc<dyn warrant_diff::ContentStore>,
) -> Result<Endpoints> {
    if !repo::is_git_repo(root) {
        bail!(
            "`warrant {command}` compares the working tree against a git reference, and {} is not \
             a git repository.\nUse `warrant wrap` to map an agent run instead.",
            root.display()
        );
    }

    // A detached worktree is the cheapest exact checkout of a reference that
    // does not disturb the tree the operator is standing in.
    let worktree = root.join(".warrant").join("worktrees").join(sanitize(against));
    if worktree.exists() {
        repo::remove_worktree(root, &worktree);
        let _ = std::fs::remove_dir_all(&worktree);
    }
    std::fs::create_dir_all(worktree.parent().expect("worktrees has a parent"))?;
    repo::add_worktree(root, &worktree, against)
        .with_context(|| format!("checking out `{against}` to compare against"))?;

    let scan = ScanOptions::default();
    let before = Snapshot::scan(&worktree, blobs.as_ref(), &scan);
    repo::remove_worktree(root, &worktree);
    let before = before?;

    let after = Snapshot::scan(root, blobs.as_ref(), &scan)?;

    // The working tree was not produced under any Warrant cell, so the
    // receipt reports the isolation the probes ran under and says nothing
    // about how the change itself was made.
    let isolation = WorkspaceCell::adopt(root, Arc::clone(blobs), scan)?.isolation();
    Ok(Endpoints { before, after, isolation })
}

fn cmd_map(
    root: &std::path::Path,
    against: &str,
    options: &MappingOptions,
    glyphs: &Glyphs,
) -> Result<ExitCode> {
    let ledger = Ledger::open_for_repo(root)?;
    let blobs = open_blobs(&ledger)?;
    let Endpoints { before, after, isolation } = endpoints(root, against, "map", &blobs)?;

    ledger.append_json(
        EntryKind::RunStarted,
        &serde_json::json!({ "mode": "map", "against": against }),
        now_ms(),
    )?;
    record_git_state(&ledger, root, "start")?;

    let request = MapRequest {
        root: root.to_path_buf(),
        before,
        after,
        proof: options.proof.clone(),
        max_probes: options.max_probes,
        parallelism: options.jobs,
        timeout_ms: options.timeout.map(|s| s * 1000),
        task: Some(format!("working tree against {against}")),
        harness: None,
        isolation,
    };

    // Nothing but the map may reach stdout when the map is being piped
    // somewhere — a pull-request comment, or a `jq`.
    if !options.json && !options.markdown {
        println!();
    }
    let outcome = execute(request, &ledger, blobs)?;
    ledger.append_json(
        EntryKind::RunFinished,
        &serde_json::json!({ "outcome": outcome.map.outcome }),
        now_ms(),
    )?;
    report(&outcome, options, glyphs)
}

fn cmd_trim(
    root: &std::path::Path,
    against: &str,
    write: bool,
    options: &MappingOptions,
    glyphs: &Glyphs,
) -> Result<ExitCode> {
    let ledger = Ledger::open_for_repo(root)?;
    let blobs = open_blobs(&ledger)?;
    let Endpoints { before, after, isolation } = endpoints(root, against, "trim", &blobs)?;

    ledger.append_json(
        EntryKind::RunStarted,
        &serde_json::json!({ "mode": "trim", "against": against, "write": write }),
        now_ms(),
    )?;
    record_git_state(&ledger, root, "start")?;

    let request = MapRequest {
        root: root.to_path_buf(),
        before: before.clone(),
        after: after.clone(),
        proof: options.proof.clone(),
        max_probes: options.max_probes,
        parallelism: options.jobs,
        timeout_ms: options.timeout.map(|s| s * 1000),
        task: Some(format!("trim against {against}")),
        harness: None,
        isolation,
    };

    println!();
    let outcome = execute(request, &ledger, blobs.clone())?;
    print!("{}", render_map(&outcome.map, &outcome.proof_source, outcome.proof_defaulted, glyphs));

    // A trim only means anything on a map that means anything. Everything else
    // — an unsatisfied proof, a vacuous one, a flaky suite — is a reason to
    // look at the proof rather than at the diff.
    if outcome.map.outcome != MapOutcome::Mapped {
        println!();
        println!("  nothing to trim: {}", outcome.map.outcome.describe());
        return Ok(ExitCode::from(2));
    }

    let plan = pipeline::plan_trim(
        root,
        &before,
        &after,
        &outcome.diff,
        &outcome.map,
        &outcome.proof_source,
        Arc::clone(&blobs),
        &ledger,
        options.timeout.map(|s| s * 1000),
    )?;

    println!();
    print!("{}", render::render_trim(&plan, write, glyphs));

    if plan.is_empty() {
        return Ok(ExitCode::SUCCESS);
    }
    if !plan.verified {
        return Ok(ExitCode::from(2));
    }

    if write {
        let scan = ScanOptions::default();
        let mut cell = WorkspaceCell::adopt(root, Arc::clone(&blobs), scan)?;
        cell.restore(&plan.snapshot)?;
        ledger.append_json(EntryKind::CellSnapshot, &plan.snapshot, now_ms())?;
        println!("  working tree is now the trimmed tree");
    }

    ledger.append_json(
        EntryKind::RunFinished,
        &serde_json::json!({ "outcome": "trimmed", "written": write }),
        now_ms(),
    )?;
    Ok(ExitCode::SUCCESS)
}

fn cmd_proof(root: &std::path::Path, expression: Option<&str>) -> Result<ExitCode> {
    let (source, defaulted, suite) = pipeline::resolve_proof(root, expression)?;
    let predicate = warrant_attest::Predicate::compile(&source)
        .with_context(|| format!("compiling the proof `{source}`"))?;

    println!();
    println!("  proof     {source}");
    if let Some(suite) = suite {
        println!(
            "  source    {} (detected from {})",
            if defaulted { "repository default" } else { "operator" },
            suite.evidence
        );
    }
    println!("  sealed    {}", predicate.hash());
    println!("  module    {} bytes", predicate.wasm().len());
    let commands = predicate.commands();
    if commands.is_empty() {
        println!("  runs      nothing — this proof only inspects the diff");
    } else {
        println!("  runs      {}", commands.join("\n            "));
    }
    println!();
    Ok(ExitCode::SUCCESS)
}

fn cmd_log(
    root: &std::path::Path,
    verify: bool,
    diverged: bool,
    limit: usize,
    glyphs: &Glyphs,
) -> Result<ExitCode> {
    let ledger = Ledger::open_for_repo(root)?;

    if verify {
        let count = ledger.verify_deep()?;
        println!(
            "  {} {count} entries verified: hash chain intact, every payload matches its address",
            glyphs.tick
        );
        return Ok(ExitCode::SUCCESS);
    }

    if diverged {
        return report_divergence(root, &ledger, glyphs);
    }

    let entries = ledger.entries()?;
    let shown = entries.iter().rev().take(limit);
    println!();
    for entry in shown {
        println!(
            "  {:>5}  {}  {:<18}  {}",
            entry.seq,
            warrant_core::format_rfc3339(entry.at_ms),
            entry.kind.name(),
            entry.payload.short()
        );
    }
    println!("\n  {} entries in total", entries.len());
    Ok(ExitCode::SUCCESS)
}

/// Compare what git says now against what the ledger recorded at the time.
///
/// A force-push rewrites a repository. It cannot unwrite the ledger, and the
/// divergence between the two is itself an entry.
fn report_divergence(root: &std::path::Path, ledger: &Ledger, glyphs: &Glyphs) -> Result<ExitCode> {
    if !repo::is_git_repo(root) {
        println!("  {} is not a git repository; there is no history to compare", root.display());
        return Ok(ExitCode::SUCCESS);
    }

    let mut findings = Vec::new();
    for entry in ledger.entries()? {
        if entry.kind != EntryKind::RepoState {
            continue;
        }
        let Ok(state) = ledger.payload_json::<serde_json::Value>(&entry) else { continue };
        let Some(head) = state.get("head").and_then(|h| h.as_str()) else { continue };

        if !repo::commit_exists(root, head) {
            findings.push((
                entry,
                head.to_owned(),
                "the commit no longer exists in this repository",
            ));
        } else if !repo::is_ancestor_of_head(root, head) {
            findings.push((entry, head.to_owned(), "the commit is no longer reachable from HEAD"));
        }
    }

    println!();
    if findings.is_empty() {
        println!(
            "  {} repository history still contains everything the ledger recorded",
            glyphs.tick
        );
        return Ok(ExitCode::SUCCESS);
    }

    for (entry, commit, reason) in &findings {
        println!("  {} repo history does not match ledger entry {}", glyphs.warn, entry.seq);
        println!(
            "    ledger:  HEAD was {} at {}",
            &commit[..commit.len().min(12)],
            warrant_core::format_rfc3339(entry.at_ms)
        );
        println!("    repo:    {reason}");
    }
    println!(
        "\n  {} history rewrite detected across {} recorded point(s)",
        glyphs.warn,
        findings.len()
    );
    // A rewrite is a finding, and a finding is worth an exit code.
    Ok(ExitCode::from(2))
}

fn cmd_verify(path: &std::path::Path, key: Option<&str>, glyphs: &Glyphs) -> Result<ExitCode> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let receipt = ReceiptFile::from_json(&bytes)?;

    let statement = match key {
        Some(expected) => receipt.verify_pinned(expected)?,
        None => receipt.verify()?,
    };

    println!();
    println!("  {} signature valid for key {}", glyphs.tick, receipt.key.keyid);
    if key.is_none() {
        println!(
            "    this proves the evidence has not changed since it was signed.\n    \
             it does not prove who signed it — pass --key to check that."
        );
    }
    println!();
    for line in statement.summary().lines() {
        println!("  {line}");
    }
    println!();
    Ok(ExitCode::SUCCESS)
}

fn report(
    outcome: &pipeline::MapResult,
    options: &MappingOptions,
    glyphs: &Glyphs,
) -> Result<ExitCode> {
    let machine_readable = options.json || options.markdown;
    if options.json {
        println!("{}", serde_json::to_string_pretty(&outcome.map)?);
    } else if options.markdown {
        print!(
            "{}",
            render::render_markdown(&outcome.map, &outcome.proof_source, outcome.proof_defaulted)
        );
    } else {
        print!(
            "{}",
            render_map(&outcome.map, &outcome.proof_source, outcome.proof_defaulted, glyphs)
        );
        println!();
    }

    if let Some(path) = &options.out_json {
        std::fs::write(path, serde_json::to_string_pretty(&outcome.map)?)
            .with_context(|| format!("writing the map to {}", path.display()))?;
    }
    if let Some(path) = &options.out_markdown {
        let body =
            render::render_markdown(&outcome.map, &outcome.proof_source, outcome.proof_defaulted);
        std::fs::write(path, body)
            .with_context(|| format!("writing the comment body to {}", path.display()))?;
    }

    if let Some(path) = &options.receipt {
        std::fs::write(path, outcome.receipt.to_json()?)
            .with_context(|| format!("writing the receipt to {}", path.display()))?;
        if !machine_readable {
            println!("  receipt written to {}", path.display());
            println!();
        }
    }

    let gate = gate_code(&outcome.map, options);

    if let Some(path) = &options.github_output {
        let outputs = github_outputs(&outcome.map, options, gate);
        append_github_outputs(path, &outputs)
            .with_context(|| format!("writing the step outputs to {}", path.display()))?;
    }

    Ok(ExitCode::from(gate))
}

/// The exit code this map earns, under the caller's thresholds.
///
/// Zero unless `--strict` was asked for: a finding is a fact about a change,
/// and turning it into a failure is a decision the caller makes.
fn gate_code(map: &warrant_necessity::NecessityMap, options: &MappingOptions) -> u8 {
    if !options.strict {
        return 0;
    }

    let below_floor = options
        .min_coverage
        .zip(map.coverage.percent())
        .is_some_and(|(floor, actual)| actual < floor);

    let failing = map.has_tampering() || map.outcome != MapOutcome::Mapped || below_floor;
    if failing { 2 } else { 0 }
}

/// The step outputs for one map, in the order a reader would want them.
fn github_outputs(
    map: &warrant_necessity::NecessityMap,
    options: &MappingOptions,
    gate: u8,
) -> Vec<(&'static str, String)> {
    let path =
        |p: &Option<PathBuf>| p.as_ref().map(|p| p.display().to_string()).unwrap_or_default();

    // Counts come off the files, which are populated whatever the outcome, so
    // they stay true even where the ratio built from them does not mean
    // anything. The percentage is the part that is allowed to be absent.
    let changed: u64 = map.files.iter().map(|f| f.changed_lines).sum();
    let proven: u64 = map.files.iter().map(|f| f.proven_lines).sum();

    vec![
        ("outcome", map.outcome.name().to_owned()),
        // Empty rather than zero when the map is not a measurement. A vacuous
        // proof did not prove none of the change; it could not be asked.
        ("coverage", map.coverage.percent().map(|p| p.to_string()).unwrap_or_default()),
        ("changed-lines", changed.to_string()),
        ("proven-lines", proven.to_string()),
        ("tampered", map.has_tampering().to_string()),
        ("probes", map.probes.to_string()),
        ("rounds", map.rounds.to_string()),
        ("markdown", path(&options.out_markdown)),
        ("json", path(&options.out_json)),
        ("receipt", path(&options.receipt)),
        ("gate", gate.to_string()),
    ]
}

/// Append `key=value` pairs in the format GitHub Actions reads.
///
/// Appended, never written: the file belongs to the step, and other commands
/// in it have their own outputs to add.
fn append_github_outputs(path: &std::path::Path, outputs: &[(&str, String)]) -> Result<()> {
    use std::io::Write;

    let mut body = String::new();
    for (key, value) in outputs {
        body.push_str(&github_output_line(key, value));
    }

    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(body.as_bytes())?;
    Ok(())
}

/// One output, in the plain form where it is safe and the delimited form
/// where it is not.
///
/// Two of these values are paths the caller chose. A newline inside one would
/// otherwise let it forge further outputs, so anything that is not a single
/// clean line goes in a heredoc whose delimiter is derived from the value —
/// which is why the delimiter cannot occur inside it.
fn github_output_line(key: &str, value: &str) -> String {
    if !value.contains(['\n', '\r']) {
        return format!("{key}={value}\n");
    }
    let delimiter = format!("warrant-{}", warrant_core::Hash::of(value.as_bytes()).short());
    format!("{key}<<{delimiter}\n{value}\n{delimiter}\n")
}

fn print_captured(blobs: &Arc<dyn warrant_diff::ContentStore>, record: &warrant_cell::ExitRecord) {
    for (label, address) in [("stdout", record.stdout), ("stderr", record.stderr)] {
        if let Ok(bytes) = blobs.get(&address)
            && !bytes.is_empty()
        {
            println!("--- agent {label} ---");
            print!("{}", String::from_utf8_lossy(&bytes));
            println!("--- end {label} ---");
        }
    }
}

/// Make a git reference safe to use as a directory name.
fn sanitize(reference: &str) -> String {
    reference
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_argument_parser_is_internally_consistent() {
        Cli::command().debug_assert();
    }

    /// The mapping flags off a `warrant map` invocation.
    fn mapping(flags: &[&str]) -> MappingOptions {
        let mut argv = vec!["warrant", "map"];
        argv.extend_from_slice(flags);
        match Cli::parse_from(argv).command {
            Command::Map { mapping, .. } => mapping,
            _ => unreachable!("that was a map"),
        }
    }

    /// A map that ended some way other than `Mapped`, over a change that does
    /// have lines in it.
    fn unmeasurable(outcome: MapOutcome) -> warrant_necessity::NecessityMap {
        use warrant_core::{Hash, PredicateHash};
        let mut map = warrant_necessity::NecessityMap::no_changes(
            PredicateHash::derive(&[b"p"]),
            Hash::of(b"t"),
        );
        map.outcome = outcome;
        map.files = vec![warrant_necessity::FileVerdict {
            path: "docs/notes.md".into(),
            change: warrant_diff::ChangeKind::Modified,
            total_hunks: 1,
            load_bearing_hunks: 0,
            changed_lines: 12,
            proven_lines: 0,
            verification_surface: false,
            tampered: false,
        }];
        map
    }

    #[test]
    fn a_finding_is_a_failure_only_where_strict_was_asked_for() {
        let map = unmeasurable(MapOutcome::Vacuous);
        assert_eq!(gate_code(&map, &mapping(&[])), 0, "a finding is a fact, not a verdict");
        assert_eq!(gate_code(&map, &mapping(&["--strict"])), 2);
    }

    #[test]
    fn coverage_is_published_as_nothing_when_it_could_not_be_measured() {
        // The shape this replaced: `jq` summing proven over changed publishes
        // `0` here, which reads as "the suite proved none of this change".
        // What actually happened is that the question could not be asked.
        let outputs = github_outputs(&unmeasurable(MapOutcome::Vacuous), &mapping(&[]), 0);
        let value =
            |key: &str| outputs.iter().find(|(k, _)| *k == key).map(|(_, v)| v.clone()).unwrap();

        assert_eq!(value("outcome"), "vacuous");
        assert_eq!(value("coverage"), "", "a ratio that is undefined publishes no number");
        assert_eq!(value("changed-lines"), "12", "the count is true whatever the outcome");
        assert_eq!(value("proven-lines"), "0");
    }

    #[test]
    fn an_unset_path_publishes_an_empty_output_rather_than_a_word() {
        let outputs = github_outputs(&unmeasurable(MapOutcome::Mapped), &mapping(&[]), 0);
        for key in ["markdown", "json", "receipt"] {
            let value = outputs.iter().find(|(k, _)| *k == key).map(|(_, v)| v.as_str());
            assert_eq!(value, Some(""), "{key} should be empty, not \"None\"");
        }
    }

    #[test]
    fn a_value_carrying_a_newline_cannot_forge_a_second_output() {
        let line = github_output_line("markdown", "body.md\ntampered=false");
        assert!(!line.starts_with("markdown=body.md\n"), "the plain form would have forged one");

        let mut lines = line.lines();
        let opening = lines.next().unwrap();
        let delimiter = opening.strip_prefix("markdown<<").expect("delimited form");
        assert!(!line.contains(&format!("\n{delimiter}=")), "the delimiter is not a key");
        assert_eq!(lines.next_back(), Some(delimiter), "and it closes the block");
    }

    #[test]
    fn an_ordinary_value_stays_readable() {
        assert_eq!(github_output_line("coverage", "40"), "coverage=40\n");
        assert_eq!(github_output_line("coverage", ""), "coverage=\n");
    }

    #[test]
    fn wrap_takes_the_agent_arguments_after_a_double_dash() {
        let cli =
            Cli::parse_from(["warrant", "wrap", "claude-code", "--", "fix the flaky upload test"]);
        match cli.command {
            Command::Wrap { harness, args, .. } => {
                assert_eq!(harness, "claude-code");
                assert_eq!(args, ["fix the flaky upload test"]);
            }
            _ => panic!("expected wrap"),
        }
    }

    #[test]
    fn a_custom_proof_can_be_supplied() {
        let cli = Cli::parse_from([
            "warrant",
            "wrap",
            "codex",
            "--proof",
            r#"exit(pytest) == 0 AND NOT diff_touches("tests/**")"#,
            "--",
            "migrate auth",
        ]);
        match cli.command {
            Command::Wrap { mapping, .. } => {
                assert!(mapping.proof.unwrap().contains("diff_touches"));
            }
            _ => panic!("expected wrap"),
        }
    }

    #[test]
    fn map_defaults_to_comparing_against_head() {
        let cli = Cli::parse_from(["warrant", "map"]);
        match cli.command {
            Command::Map { against, .. } => assert_eq!(against, "HEAD"),
            _ => panic!("expected map"),
        }
    }

    #[test]
    fn references_are_sanitised_before_becoming_directory_names() {
        assert_eq!(sanitize("HEAD"), "HEAD");
        assert_eq!(sanitize("refs/heads/main"), "refs-heads-main");
        assert_eq!(
            sanitize("../../etc"),
            "------etc",
            "no separators survive to build a path with"
        );
    }
}
