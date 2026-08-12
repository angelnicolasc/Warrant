//! Running commands and recording what they did.
//!
//! Output never enters a context window. It is streamed to the content store
//! and referenced by address, which is L3 applied at the point where large
//! artefacts are actually produced — a test suite's stdout is the single
//! biggest thing an agent run generates, and it is exactly the thing that
//! compaction would later mangle.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use warrant_core::Hash;
use warrant_diff::ContentStore;

use crate::error::{CellError, Result};

/// How often a running child is checked for completion.
const POLL_INTERVAL: Duration = Duration::from_millis(5);

/// What to run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandSpec {
    /// Program and arguments. Never passed through a shell.
    pub argv: Vec<String>,
    /// Working directory, relative to the cell root. `None` means the root.
    pub cwd: Option<String>,
    /// Environment overlay.
    pub env: BTreeMap<String, String>,
    /// Start from an empty environment rather than inheriting.
    pub clear_env: bool,
    /// Kill the process after this long.
    pub timeout_ms: Option<u64>,
}

impl CommandSpec {
    /// A command from a program and its arguments.
    pub fn new<I, S>(argv: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        CommandSpec {
            argv: argv.into_iter().map(Into::into).collect(),
            cwd: None,
            env: BTreeMap::new(),
            clear_env: false,
            timeout_ms: None,
        }
    }

    /// Parse a command line without invoking a shell.
    ///
    /// Handles single and double quotes so that `pytest -k "not slow"`
    /// behaves as written. Everything else — pipes, redirections, globs — is
    /// deliberately not supported: a proof that depends on shell semantics is
    /// a proof that depends on which shell.
    pub fn parse(line: &str) -> Result<Self> {
        let mut argv = Vec::new();
        let mut current = String::new();
        let mut started = false;
        let mut quote: Option<char> = None;

        for ch in line.chars() {
            match (quote, ch) {
                (Some(q), c) if c == q => quote = None,
                (Some(_), c) => current.push(c),
                (None, '"') | (None, '\'') => {
                    quote = Some(ch);
                    started = true;
                }
                (None, c) if c.is_whitespace() => {
                    if started {
                        argv.push(std::mem::take(&mut current));
                        started = false;
                    }
                }
                (None, c) => {
                    current.push(c);
                    started = true;
                }
            }
        }
        if started {
            argv.push(current);
        }
        if argv.is_empty() {
            return Err(CellError::EmptyCommand);
        }
        Ok(CommandSpec::new(argv))
    }

    /// Set a timeout.
    pub fn with_timeout_ms(mut self, ms: u64) -> Self {
        self.timeout_ms = Some(ms);
        self
    }

    /// Set an environment variable.
    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    /// Run in a subdirectory of the cell root.
    pub fn in_dir(mut self, cwd: impl Into<String>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// The program name.
    pub fn program(&self) -> &str {
        self.argv.first().map(String::as_str).unwrap_or_default()
    }

    /// A stable address for this command, so probe results can be cached.
    pub fn digest(&self) -> Hash {
        let mut parts: Vec<Vec<u8>> = Vec::new();
        for arg in &self.argv {
            parts.push(arg.as_bytes().to_vec());
        }
        parts.push(self.cwd.clone().unwrap_or_default().into_bytes());
        for (k, v) in &self.env {
            parts.push(k.as_bytes().to_vec());
            parts.push(v.as_bytes().to_vec());
        }
        parts.push(vec![u8::from(self.clear_env)]);
        let refs: Vec<&[u8]> = parts.iter().map(Vec::as_slice).collect();
        Hash::of_tagged("warrant.command.v1", &refs)
    }
}

/// What a command did.
///
/// Stdout and stderr are addresses, not strings. Nothing here grows with the
/// size of a test suite's output.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExitRecord {
    /// The command that ran.
    pub command: CommandSpec,
    /// Exit status. `None` when the process was killed by a signal or a timeout.
    pub code: Option<i32>,
    /// Whether the timeout fired.
    pub timed_out: bool,
    /// Wall-clock duration.
    pub duration_ms: u64,
    /// Address of captured stdout.
    pub stdout: Hash,
    /// Address of captured stderr.
    pub stderr: Hash,
    /// Bytes of stdout, for reporting without a store round trip.
    pub stdout_len: u64,
    /// Bytes of stderr.
    pub stderr_len: u64,
}

impl ExitRecord {
    /// Whether the command reported success.
    ///
    /// A timeout is never success, even if the process managed to exit 0 in
    /// the same instant it was killed.
    pub fn succeeded(&self) -> bool {
        !self.timed_out && self.code == Some(0)
    }
}

/// Run a command inside `root`, capturing output into `store`.
pub fn run(root: &Path, spec: &CommandSpec, store: &dyn ContentStore) -> Result<ExitRecord> {
    let program = spec.argv.first().ok_or(CellError::EmptyCommand)?;

    // Resolve through PATH ourselves so the recorded evidence names a
    // concrete executable, and so Windows finds `npm.cmd` for `npm`.
    let resolved = which::which(program)
        .map_err(|_| CellError::ProgramNotFound { program: program.clone() })?;

    let workdir = match &spec.cwd {
        Some(rel) => warrant_diff::join_relative(root, rel)?,
        None => root.to_path_buf(),
    };

    let mut command = Command::new(&resolved);
    command
        .args(&spec.argv[1..])
        .current_dir(&workdir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if spec.clear_env {
        command.env_clear();
    }
    for (key, value) in &spec.env {
        command.env(key, value);
    }

    let started = Instant::now();
    let mut child = command
        .spawn()
        .map_err(|source| CellError::SpawnFailed { program: program.clone(), source })?;

    // Drain both pipes on their own threads. A test suite that fills the
    // stdout pipe while the parent waits on exit is a deadlock, and it is the
    // kind that only shows up on the one run that matters.
    let mut out_pipe = child.stdout.take().expect("stdout was piped");
    let mut err_pipe = child.stderr.take().expect("stderr was piped");
    let out_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = out_pipe.read_to_end(&mut buf);
        buf
    });
    let err_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = err_pipe.read_to_end(&mut buf);
        buf
    });

    let deadline = spec.timeout_ms.map(|ms| started + Duration::from_millis(ms));
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {}
            Err(source) => {
                return Err(CellError::SpawnFailed { program: program.clone(), source });
            }
        }
        if let Some(deadline) = deadline
            && Instant::now() >= deadline
        {
            timed_out = true;
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(POLL_INTERVAL);
    };

    let stdout_bytes = out_reader.join().unwrap_or_default();
    let stderr_bytes = err_reader.join().unwrap_or_default();
    let duration_ms = started.elapsed().as_millis() as u64;

    let stdout = store.put(&stdout_bytes).map_err(CellError::Store)?;
    let stderr = store.put(&stderr_bytes).map_err(CellError::Store)?;

    Ok(ExitRecord {
        command: spec.clone(),
        code: status.and_then(|s| s.code()),
        timed_out,
        duration_ms,
        stdout,
        stderr,
        stdout_len: stdout_bytes.len() as u64,
        stderr_len: stderr_bytes.len() as u64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use warrant_diff::MemoryStore;

    /// A program that exists on every platform this runs on, with a
    /// predictable exit code and output.
    fn echo(text: &str) -> CommandSpec {
        #[cfg(windows)]
        {
            CommandSpec::new(["cmd", "/C", &format!("echo {text}")])
        }
        #[cfg(not(windows))]
        {
            CommandSpec::new(["sh", "-c", &format!("echo {text}")])
        }
    }

    fn exit_with(code: i32) -> CommandSpec {
        #[cfg(windows)]
        {
            CommandSpec::new(["cmd", "/C", &format!("exit {code}")])
        }
        #[cfg(not(windows))]
        {
            CommandSpec::new(["sh", "-c", &format!("exit {code}")])
        }
    }

    #[test]
    fn parsing_splits_on_whitespace_and_respects_quotes() {
        assert_eq!(CommandSpec::parse("pytest -q").unwrap().argv, ["pytest", "-q"]);
        assert_eq!(
            CommandSpec::parse(r#"pytest -k "not slow""#).unwrap().argv,
            ["pytest", "-k", "not slow"]
        );
        assert_eq!(
            CommandSpec::parse("cargo test --package 'my crate'").unwrap().argv,
            ["cargo", "test", "--package", "my crate"]
        );
        assert_eq!(CommandSpec::parse("   spaced   out   ").unwrap().argv, ["spaced", "out"]);
        assert!(CommandSpec::parse("   ").is_err());
    }

    #[test]
    fn an_empty_quoted_argument_survives_parsing() {
        assert_eq!(CommandSpec::parse(r#"prog "" x"#).unwrap().argv, ["prog", "", "x"]);
    }

    #[test]
    fn the_command_digest_distinguishes_what_matters() {
        let base = CommandSpec::new(["pytest", "-q"]);
        assert_eq!(base.digest(), CommandSpec::new(["pytest", "-q"]).digest());
        assert_ne!(base.digest(), CommandSpec::new(["pytest", "-v"]).digest());
        assert_ne!(base.digest(), base.clone().with_env("SEED", "1").digest());
        assert_ne!(base.digest(), base.clone().in_dir("sub").digest());
    }

    #[test]
    fn a_successful_command_is_recorded_with_its_output() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::new();
        let record = run(dir.path(), &echo("hello"), &store).unwrap();

        assert!(record.succeeded());
        assert_eq!(record.code, Some(0));
        let out = store.get(&record.stdout).unwrap();
        assert!(String::from_utf8_lossy(&out).contains("hello"));
    }

    #[test]
    fn a_failing_exit_code_is_recorded_faithfully() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::new();
        let record = run(dir.path(), &exit_with(3), &store).unwrap();
        assert!(!record.succeeded());
        assert_eq!(record.code, Some(3));
    }

    #[test]
    fn a_missing_program_is_an_error_rather_than_a_silent_failure() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::new();
        let spec = CommandSpec::new(["definitely-not-a-real-program-xyzzy"]);
        assert!(matches!(run(dir.path(), &spec, &store), Err(CellError::ProgramNotFound { .. })));
    }

    #[test]
    fn output_is_addressed_not_inlined() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::new();
        let record = run(dir.path(), &echo("addressed"), &store).unwrap();

        // The record is small regardless of how much the command printed.
        let encoded = serde_json::to_vec(&record).unwrap();
        assert!(encoded.len() < 1024);
        assert!(store.contains(&record.stdout));
    }
}
