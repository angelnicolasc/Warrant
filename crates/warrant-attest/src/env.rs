//! What a proof is allowed to ask about.
//!
//! Four questions, and no fifth. A proof cannot read the ledger, cannot see
//! the necessity map, cannot learn its own coverage, and cannot find out
//! whether it is running against the agent's real result or against a
//! candidate the search constructed. That last one matters more than it
//! looks: a proof that could tell the difference could pass on the result and
//! fail on every probe, which would report the entire diff as load-bearing.
//!
//! Environments are shared rather than borrowed — the WebAssembly runtime
//! requires host state to outlive the call — so the methods take `&self` and
//! keep their mutable parts behind a lock.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use globset::{GlobBuilder, GlobMatcher};
use warrant_cell::{Cell, CommandSpec, ExitRecord};
use warrant_diff::Snapshot;

use crate::error::{AttestError, Result};

/// Exit status reported for a command the timeout killed.
///
/// Matches the convention of `timeout(1)`. A killed command does not abort
/// the search — it is a command that did not succeed, which is a perfectly
/// good thing for a proof to observe.
pub const TIMEOUT_EXIT_CODE: i32 = 124;

/// The host surface a sealed proof runs against.
pub trait ProbeEnvironment: Send + Sync {
    /// Run a command and report its exit status.
    fn exit_code(&self, command: &str) -> Result<i32>;

    /// Whether the candidate diff touches any path matching `pattern`.
    fn diff_touches(&self, pattern: &str) -> Result<bool>;

    /// Whether `path` exists in the candidate tree.
    fn file_exists(&self, path: &str) -> Result<bool>;

    /// How many files the candidate diff changes.
    fn changed_files(&self) -> Result<i32>;
}

/// Compile a path pattern.
///
/// `literal_separator` is on, so `*` stops at a directory boundary and only
/// `**` crosses one — the semantics anyone who has written a `.gitignore`
/// already has in their head. Left at the library default, `src/*` would
/// quietly match `src/a/b/c.py`, and a proof reading
/// `NOT diff_touches("tests/*.py")` would forbid more than its author wrote.
fn matcher(pattern: &str) -> Result<GlobMatcher> {
    GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .map(|g| g.compile_matcher())
        .map_err(|e| AttestError::BadPattern { pattern: pattern.to_owned(), reason: e.to_string() })
}

/// Which paths differ between two observed trees.
pub fn changed_paths(baseline: &Snapshot, candidate: &Snapshot) -> BTreeSet<String> {
    let mut changed = BTreeSet::new();
    for (path, meta) in &candidate.files {
        if baseline.files.get(path) != Some(meta) {
            changed.insert(path.clone());
        }
    }
    for path in baseline.files.keys() {
        if !candidate.files.contains_key(path) {
            changed.insert(path.clone());
        }
    }
    changed
}

/// A proof running against a real cell.
pub struct CellEnvironment {
    cell: Arc<Mutex<dyn Cell>>,
    candidate: Snapshot,
    changed: BTreeSet<String>,
    timeout_ms: Option<u64>,
    matchers: Mutex<BTreeMap<String, GlobMatcher>>,
    commands_run: Mutex<Vec<ExitRecord>>,
}

impl CellEnvironment {
    /// Build an environment over a materialised candidate tree.
    ///
    /// `baseline` is the state the agent started from; `candidate` is what is
    /// on disk in the cell right now. The difference between them is what
    /// `diff_touches` and `changed_files` answer about — so those answers
    /// describe the *subset under test*, not the agent's full diff.
    pub fn new(
        cell: Arc<Mutex<dyn Cell>>,
        baseline: &Snapshot,
        candidate: &Snapshot,
        timeout_ms: Option<u64>,
    ) -> Self {
        CellEnvironment {
            cell,
            changed: changed_paths(baseline, candidate),
            candidate: candidate.clone(),
            timeout_ms,
            matchers: Mutex::new(BTreeMap::new()),
            commands_run: Mutex::new(Vec::new()),
        }
    }

    /// Every command this evaluation ran, with its exit status and the
    /// addresses of its output.
    pub fn commands_run(&self) -> Vec<ExitRecord> {
        self.commands_run.lock().expect("probe environment poisoned").clone()
    }
}

impl ProbeEnvironment for CellEnvironment {
    fn exit_code(&self, command: &str) -> Result<i32> {
        let mut spec = CommandSpec::parse(command)?;
        if let Some(ms) = self.timeout_ms {
            spec = spec.with_timeout_ms(ms);
        }
        let record = {
            let mut cell = self.cell.lock().expect("cell poisoned");
            cell.exec(&spec)?
        };
        let code = if record.timed_out { TIMEOUT_EXIT_CODE } else { record.code.unwrap_or(-1) };
        self.commands_run.lock().expect("probe environment poisoned").push(record);
        Ok(code)
    }

    fn diff_touches(&self, pattern: &str) -> Result<bool> {
        let mut matchers = self.matchers.lock().expect("probe environment poisoned");
        if !matchers.contains_key(pattern) {
            matchers.insert(pattern.to_owned(), matcher(pattern)?);
        }
        let glob = &matchers[pattern];
        Ok(self.changed.iter().any(|path| glob.is_match(path)))
    }

    fn file_exists(&self, path: &str) -> Result<bool> {
        Ok(self.candidate.files.contains_key(path))
    }

    fn changed_files(&self) -> Result<i32> {
        Ok(self.changed.len() as i32)
    }
}

/// A scripted environment, for testing proofs without running anything.
///
/// It records the order of host calls, which is how short-circuiting is
/// verified — a proof that ran the test suite after an earlier clause had
/// already failed would be a real cost regression and an invisible one.
#[derive(Debug, Default)]
pub struct ScriptedEnvironment {
    exit_codes: BTreeMap<String, i32>,
    default_exit_code: i32,
    changed_paths: BTreeSet<String>,
    existing_files: BTreeSet<String>,
    calls: Mutex<Vec<String>>,
}

impl ScriptedEnvironment {
    /// An environment where every command succeeds and nothing changed.
    pub fn new() -> Self {
        Self::default()
    }

    /// Make a specific command return `code`.
    pub fn with_exit(mut self, command: &str, code: i32) -> Self {
        self.exit_codes.insert(command.to_owned(), code);
        self
    }

    /// Exit code for commands not named explicitly.
    pub fn with_default_exit(mut self, code: i32) -> Self {
        self.default_exit_code = code;
        self
    }

    /// Declare which paths the candidate diff touches.
    pub fn with_changed<I: IntoIterator<Item = S>, S: Into<String>>(mut self, paths: I) -> Self {
        self.changed_paths = paths.into_iter().map(Into::into).collect();
        self
    }

    /// Declare which paths exist in the candidate tree.
    pub fn with_files<I: IntoIterator<Item = S>, S: Into<String>>(mut self, paths: I) -> Self {
        self.existing_files = paths.into_iter().map(Into::into).collect();
        self
    }

    /// The host calls made, in order.
    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("scripted environment poisoned").clone()
    }

    fn record(&self, call: String) {
        self.calls.lock().expect("scripted environment poisoned").push(call);
    }
}

impl ProbeEnvironment for ScriptedEnvironment {
    fn exit_code(&self, command: &str) -> Result<i32> {
        self.record(format!("exit({command})"));
        Ok(self.exit_codes.get(command).copied().unwrap_or(self.default_exit_code))
    }

    fn diff_touches(&self, pattern: &str) -> Result<bool> {
        self.record(format!("diff_touches({pattern})"));
        let glob = matcher(pattern)?;
        Ok(self.changed_paths.iter().any(|p| glob.is_match(p)))
    }

    fn file_exists(&self, path: &str) -> Result<bool> {
        self.record(format!("file_exists({path})"));
        Ok(self.existing_files.contains(path))
    }

    fn changed_files(&self) -> Result<i32> {
        self.record("changed_files()".to_owned());
        Ok(self.changed_paths.len() as i32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_double_star_matches_at_any_depth_below_its_prefix() {
        let env = ScriptedEnvironment::new()
            .with_changed(["src/auth/login.py", "src/auth/tokens/jwt.py"]);
        assert!(env.diff_touches("src/auth/**").unwrap());
        assert!(!env.diff_touches("src/api/**").unwrap());
    }

    #[test]
    fn a_single_star_does_not_cross_a_directory_boundary() {
        let env = ScriptedEnvironment::new().with_changed(["tests/unit/test_upload.py"]);
        assert!(!env.diff_touches("tests/*.py").unwrap(), "`*` must not span `/`");
        assert!(env.diff_touches("tests/**/*.py").unwrap());
        assert!(env.diff_touches("tests/**").unwrap());
    }

    #[test]
    fn the_test_directory_pattern_from_the_readme_behaves() {
        let env = ScriptedEnvironment::new().with_changed(["tests/test_upload.py"]);
        assert!(env.diff_touches("tests/**").unwrap());

        let only_source = ScriptedEnvironment::new().with_changed(["src/api/upload.py"]);
        assert!(!only_source.diff_touches("tests/**").unwrap());
    }

    #[test]
    fn a_malformed_pattern_is_reported_rather_than_matching_nothing() {
        let env = ScriptedEnvironment::new().with_changed(["a.py"]);
        assert!(matches!(env.diff_touches("["), Err(AttestError::BadPattern { .. })));
    }

    #[test]
    fn scripted_exit_codes_are_returned_and_recorded() {
        let env = ScriptedEnvironment::new().with_exit("pytest", 1).with_default_exit(0);
        assert_eq!(env.exit_code("pytest").unwrap(), 1);
        assert_eq!(env.exit_code("cargo test").unwrap(), 0);
        assert_eq!(env.calls(), ["exit(pytest)", "exit(cargo test)"]);
    }

    #[test]
    fn changed_paths_covers_additions_deletions_and_edits() {
        let store = warrant_diff::MemoryStore::new();
        let before = Snapshot::from_contents(
            [("keep", &b"k"[..]), ("gone", &b"g"[..]), ("edit", &b"a"[..])],
            &store,
        )
        .unwrap();
        let after = Snapshot::from_contents(
            [("keep", &b"k"[..]), ("edit", &b"b"[..]), ("new", &b"n"[..])],
            &store,
        )
        .unwrap();

        let changed = changed_paths(&before, &after);
        assert_eq!(changed.into_iter().collect::<Vec<_>>(), ["edit", "gone", "new"]);
    }
}
