//! `warrant` — run any agent inside a cell and get a proof map back.

#![forbid(unsafe_code)]

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

    /// Per-command timeout inside a probe, in seconds.
    #[arg(long, value_name = "SECONDS")]
    timeout: Option<u64>,

    /// Where to write the receipt.
    #[arg(long, value_name = "PATH")]
    receipt: Option<PathBuf>,

    /// Emit the map as JSON instead of a rendered report.
    #[arg(long)]
    json: bool,

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
    record_git_state(&ledger, root, EntryKind::RunStarted)?;

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

    record_git_state(&ledger, root, EntryKind::RunFinished)?;
    ledger.append_json(
        EntryKind::RunFinished,
        &serde_json::json!({ "outcome": outcome.map.outcome }),
        now_ms(),
    )?;

    report(&outcome, options, glyphs)
}

fn cmd_map(
    root: &std::path::Path,
    against: &str,
    options: &MappingOptions,
    glyphs: &Glyphs,
) -> Result<ExitCode> {
    if !repo::is_git_repo(root) {
        bail!(
            "`warrant map` compares the working tree against a git reference, and {} is not a \
             git repository.\nUse `warrant wrap` to map an agent run instead.",
            root.display()
        );
    }

    let ledger = Ledger::open_for_repo(root)?;
    let blobs = open_blobs(&ledger)?;
    ledger.append_json(
        EntryKind::RunStarted,
        &serde_json::json!({ "mode": "map", "against": against }),
        now_ms(),
    )?;
    record_git_state(&ledger, root, EntryKind::RunStarted)?;

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
    let isolation = WorkspaceCell::adopt(root, Arc::clone(&blobs), scan)?.isolation();

    let request = MapRequest {
        root: root.to_path_buf(),
        before,
        after,
        proof: options.proof.clone(),
        max_probes: options.max_probes,
        timeout_ms: options.timeout.map(|s| s * 1000),
        task: Some(format!("working tree against {against}")),
        harness: None,
        isolation,
    };

    println!();
    let outcome = execute(request, &ledger, blobs)?;
    ledger.append_json(
        EntryKind::RunFinished,
        &serde_json::json!({ "outcome": outcome.map.outcome }),
        now_ms(),
    )?;
    report(&outcome, options, glyphs)
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
        if !matches!(entry.kind, EntryKind::RunStarted | EntryKind::RunFinished) {
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
    if options.json {
        println!("{}", serde_json::to_string_pretty(&outcome.map)?);
    } else {
        print!(
            "{}",
            render_map(&outcome.map, &outcome.proof_source, outcome.proof_defaulted, glyphs)
        );
        println!();
    }

    if let Some(path) = &options.receipt {
        std::fs::write(path, outcome.receipt.to_json()?)
            .with_context(|| format!("writing the receipt to {}", path.display()))?;
        if !options.json {
            println!("  receipt written to {}", path.display());
            println!();
        }
    }

    if !options.strict {
        return Ok(ExitCode::SUCCESS);
    }

    let below_floor = options
        .min_coverage
        .zip(outcome.map.coverage.percent())
        .is_some_and(|(floor, actual)| actual < floor);

    let failing =
        outcome.map.has_tampering() || outcome.map.outcome != MapOutcome::Mapped || below_floor;

    Ok(if failing { ExitCode::from(2) } else { ExitCode::SUCCESS })
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
