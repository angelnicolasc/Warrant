//! Finding the repository, and finding out how it tests itself.
//!
//! The default proof is whatever the repository already runs. That is the
//! entire adoption story: the operator writes nothing, declares nothing, and
//! learns no language, and still gets a proof map on the next agent run.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

/// Drop Windows' extended-length `\\?\` prefix from a canonicalised path.
///
/// `canonicalize` returns it on Windows. It is correct, and it also leaks into
/// every error message and gets handed to child processes — and `git worktree`
/// among others does not accept it. Only plain drive paths are simplified;
/// `\\?\UNC\…` is left alone, because shortening it changes what it means.
#[cfg(windows)]
fn simplify(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => PathBuf::from(rest),
        _ => path,
    }
}

#[cfg(not(windows))]
fn simplify(path: PathBuf) -> PathBuf {
    path
}

/// A test command Warrant found on its own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DetectedSuite {
    /// The command line to run.
    pub command: String,
    /// The file that gave it away, for the receipt and for the terminal.
    pub evidence: &'static str,
}

/// Walk up from `start` looking for a repository root.
///
/// A `.git` directory wins; a `.warrant` directory is accepted for
/// repositories not under git, because `warrant wrap` should work on a plain
/// directory too.
pub fn find_root(start: &Path) -> Result<PathBuf> {
    let start =
        simplify(start.canonicalize().with_context(|| format!("resolving {}", start.display()))?);
    let mut current = start.as_path();
    loop {
        if current.join(".git").exists() || current.join(".warrant").exists() {
            return Ok(current.to_path_buf());
        }
        match current.parent() {
            Some(parent) => current = parent,
            // No marker anywhere above: treat the starting directory as the
            // root rather than refusing to run.
            None => return Ok(start),
        }
    }
}

/// Work out how this repository runs its tests.
///
/// Ordered so that the most specific signal wins. Returns `None` when nothing
/// recognisable is present, which is a prompt to pass `--proof`, not a reason
/// to guess.
pub fn detect_test_command(root: &Path) -> Option<DetectedSuite> {
    let exists = |name: &str| root.join(name).exists();
    let contains = |name: &str, needle: &str| {
        std::fs::read_to_string(root.join(name)).map(|text| text.contains(needle)).unwrap_or(false)
    };

    if exists("Cargo.toml") {
        return Some(DetectedSuite { command: "cargo test".into(), evidence: "Cargo.toml" });
    }
    if exists("go.mod") {
        return Some(DetectedSuite { command: "go test ./...".into(), evidence: "go.mod" });
    }
    if exists("package.json") && package_json_has_test_script(root) {
        return Some(DetectedSuite {
            command: "npm test --silent".into(),
            evidence: "package.json",
        });
    }
    if exists("pytest.ini") {
        return Some(DetectedSuite { command: "pytest -q".into(), evidence: "pytest.ini" });
    }
    if exists("conftest.py") {
        return Some(DetectedSuite { command: "pytest -q".into(), evidence: "conftest.py" });
    }
    if exists("pyproject.toml") && contains("pyproject.toml", "pytest") {
        return Some(DetectedSuite { command: "pytest -q".into(), evidence: "pyproject.toml" });
    }
    if exists("setup.cfg") && contains("setup.cfg", "pytest") {
        return Some(DetectedSuite { command: "pytest -q".into(), evidence: "setup.cfg" });
    }
    if exists("pom.xml") {
        return Some(DetectedSuite { command: "mvn -q test".into(), evidence: "pom.xml" });
    }
    if exists("build.gradle") || exists("build.gradle.kts") {
        return Some(DetectedSuite {
            command: "gradle test --quiet".into(),
            evidence: "build.gradle",
        });
    }
    if exists("mix.exs") {
        return Some(DetectedSuite { command: "mix test".into(), evidence: "mix.exs" });
    }
    if exists("Gemfile") && root.join("spec").is_dir() {
        return Some(DetectedSuite { command: "bundle exec rspec".into(), evidence: "Gemfile" });
    }
    if makefile_has_test_target(root) {
        return Some(DetectedSuite { command: "make test".into(), evidence: "Makefile" });
    }
    None
}

fn package_json_has_test_script(root: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(root.join("package.json")) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return false;
    };
    value
        .get("scripts")
        .and_then(|s| s.get("test"))
        .and_then(|t| t.as_str())
        // `npm init` writes a placeholder that exits 1 with a message. Taking
        // it as the proof would report every run as failing.
        .is_some_and(|script| !script.contains("no test specified"))
}

fn makefile_has_test_target(root: &Path) -> bool {
    for name in ["Makefile", "makefile", "GNUmakefile"] {
        if let Ok(text) = std::fs::read_to_string(root.join(name))
            && text.lines().any(|line| line.starts_with("test:") || line.starts_with("test :"))
        {
            return true;
        }
    }
    false
}

/// Build the default proof from a detected suite.
///
/// Just the exit status. Deliberately not `AND NOT diff_touches("tests/**")` —
/// that would be Warrant deciding what the operator's claim is. The default
/// proof measures what the repository already measures, and the map is what
/// reports where the test edits landed.
pub fn default_proof(suite: &DetectedSuite) -> String {
    format!("exit({}) == 0", suite.command)
}

/// Run a git command in `root`, returning trimmed stdout.
fn git(root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .with_context(|| format!("running `git {}`", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "`git {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Whether `root` is inside a git working tree.
pub fn is_git_repo(root: &Path) -> bool {
    git(root, &["rev-parse", "--is-inside-work-tree"]).is_ok_and(|out| out == "true")
}

/// The current commit, if there is one.
pub fn head_commit(root: &Path) -> Option<String> {
    git(root, &["rev-parse", "HEAD"]).ok()
}

/// The most recent commits, newest first.
pub fn recent_commits(root: &Path, count: usize) -> Vec<String> {
    git(root, &["rev-list", &format!("--max-count={count}"), "HEAD"])
        .map(|out| out.lines().map(str::to_owned).collect())
        .unwrap_or_default()
}

/// Whether `commit` is still reachable from the current HEAD.
///
/// This is the force-push detector. A commit the ledger recorded that is no
/// longer an ancestor of HEAD has been written out of the repository's
/// history — the repository changed, and the record did not.
pub fn is_ancestor_of_head(root: &Path, commit: &str) -> bool {
    Command::new("git")
        .args(["merge-base", "--is-ancestor", commit, "HEAD"])
        .current_dir(root)
        .output()
        .is_ok_and(|out| out.status.success())
}

/// Whether a commit object exists at all.
pub fn commit_exists(root: &Path, commit: &str) -> bool {
    git(root, &["cat-file", "-e", &format!("{commit}^{{commit}}")]).is_ok()
}

/// Check out `reference` into `path` as a detached worktree.
pub fn add_worktree(root: &Path, path: &Path, reference: &str) -> Result<()> {
    git(root, &["worktree", "add", "--detach", &path.to_string_lossy(), reference])?;
    Ok(())
}

/// Remove a worktree created by [`add_worktree`].
pub fn remove_worktree(root: &Path, path: &Path) {
    let _ = git(root, &["worktree", "remove", "--force", &path.to_string_lossy()]);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, content) in files {
            let path = dir.path().join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }
        dir
    }

    #[test]
    fn rust_projects_are_detected() {
        let dir = scratch(&[("Cargo.toml", "[package]\nname = \"x\"\n")]);
        let suite = detect_test_command(dir.path()).unwrap();
        assert_eq!(suite.command, "cargo test");
        assert_eq!(default_proof(&suite), "exit(cargo test) == 0");
    }

    #[test]
    fn python_projects_are_detected_from_several_signals() {
        for (file, content) in [
            ("pytest.ini", "[pytest]\n"),
            ("conftest.py", "import pytest\n"),
            ("pyproject.toml", "[tool.pytest.ini_options]\n"),
            ("setup.cfg", "[tool:pytest]\n"),
        ] {
            let dir = scratch(&[(file, content)]);
            let suite = detect_test_command(dir.path()).unwrap();
            assert_eq!(suite.command, "pytest -q", "failed for {file}");
        }
    }

    #[test]
    fn a_pyproject_without_pytest_is_not_assumed_to_use_it() {
        let dir = scratch(&[("pyproject.toml", "[project]\nname = \"x\"\n")]);
        assert_eq!(detect_test_command(dir.path()), None);
    }

    #[test]
    fn the_npm_placeholder_test_script_is_not_taken_as_a_proof() {
        let dir = scratch(&[(
            "package.json",
            r#"{"scripts":{"test":"echo \"Error: no test specified\" && exit 1"}}"#,
        )]);
        assert_eq!(
            detect_test_command(dir.path()),
            None,
            "the placeholder would report every run as failing"
        );

        let real = scratch(&[("package.json", r#"{"scripts":{"test":"vitest run"}}"#)]);
        assert_eq!(detect_test_command(real.path()).unwrap().command, "npm test --silent");
    }

    #[test]
    fn a_makefile_is_only_used_when_it_has_a_test_target() {
        let without = scratch(&[("Makefile", "build:\n\tcc main.c\n")]);
        assert_eq!(detect_test_command(without.path()), None);

        let with = scratch(&[("Makefile", "build:\n\tcc main.c\ntest:\n\t./run-tests\n")]);
        assert_eq!(detect_test_command(with.path()).unwrap().command, "make test");
    }

    #[test]
    fn go_java_gradle_elixir_and_ruby_are_recognised() {
        for (file, dirs, expected) in [
            ("go.mod", vec![], "go test ./..."),
            ("pom.xml", vec![], "mvn -q test"),
            ("build.gradle", vec![], "gradle test --quiet"),
            ("mix.exs", vec![], "mix test"),
            ("Gemfile", vec!["spec"], "bundle exec rspec"),
        ] {
            let dir = scratch(&[(file, "x")]);
            for d in dirs {
                std::fs::create_dir_all(dir.path().join(d)).unwrap();
            }
            assert_eq!(detect_test_command(dir.path()).unwrap().command, expected);
        }
    }

    #[test]
    fn an_unrecognisable_repository_yields_nothing_rather_than_a_guess() {
        let dir = scratch(&[("README.md", "# just docs\n")]);
        assert_eq!(detect_test_command(dir.path()), None);
    }

    #[test]
    fn the_root_is_found_by_walking_up_to_a_marker() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        let nested = dir.path().join("src").join("deep");
        std::fs::create_dir_all(&nested).unwrap();

        let found = find_root(&nested).unwrap();
        assert_eq!(found.canonicalize().unwrap(), dir.path().canonicalize().unwrap());
    }

    #[test]
    fn a_directory_with_no_marker_is_its_own_root() {
        let dir = tempfile::tempdir().unwrap();
        let found = find_root(dir.path()).unwrap();
        assert_eq!(found.canonicalize().unwrap(), dir.path().canonicalize().unwrap());
    }

    /// The extended-length prefix is correct and unusable: it reaches child
    /// processes and error messages, and `git worktree` rejects it.
    #[test]
    #[cfg(windows)]
    fn windows_extended_length_prefixes_do_not_escape_into_paths() {
        let dir = tempfile::tempdir().unwrap();
        let found = find_root(dir.path()).unwrap();
        assert!(
            !found.to_string_lossy().starts_with(r"\\?\"),
            "a \\\\?\\ path leaked out of find_root: {}",
            found.display()
        );
    }
}
