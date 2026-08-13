//! How the search behaves, and what counts as a test file.

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

use crate::error::{NecessityError, Result};

/// Paths treated as tests when deciding whether a load-bearing hunk is a
/// laundered green.
///
/// Deliberately broad. A false positive here says *look at this*, which costs
/// a glance; a false negative says nothing, which is the failure this whole
/// project exists to remove.
pub const DEFAULT_TEST_PATTERNS: &[&str] = &[
    "tests/**",
    "test/**",
    "spec/**",
    "**/tests/**",
    "**/test/**",
    "**/__tests__/**",
    "**/testdata/**",
    "**/fixtures/**",
    "**/test_*.py",
    "**/*_test.py",
    "**/conftest.py",
    "**/*_test.go",
    "**/*_test.rs",
    "**/*_test.rb",
    "**/*_spec.rb",
    "**/*.test.js",
    "**/*.test.jsx",
    "**/*.test.ts",
    "**/*.test.tsx",
    "**/*.spec.js",
    "**/*.spec.ts",
    "**/*Test.java",
    "**/*Tests.java",
    "**/*Test.cs",
    "**/*Tests.cs",
    "**/*Spec.scala",
];

/// Snapshot files, which are the fourth shape of the rewrite failure mode.
///
/// A regenerated snapshot is a test edit even when it lives beside the source.
pub const DEFAULT_SNAPSHOT_PATTERNS: &[&str] =
    &["**/__snapshots__/**", "**/snapshots/**", "**/*.snap", "**/*.ambr", "**/*.approved.txt"];

/// Search settings.
#[derive(Clone, Debug)]
pub struct NecessityConfig {
    /// Patterns whose load-bearing hunks are flagged as test tampering.
    pub test_patterns: Vec<String>,
    /// Patterns treated as recorded expectations.
    pub snapshot_patterns: Vec<String>,
    /// Ceiling on probes. `None` runs the search to completion.
    pub max_probes: Option<u32>,
    /// Re-check every surviving hunk individually after the search.
    ///
    /// On by default. Without it the map's per-hunk claim rests on the
    /// algorithm's invariant rather than on a probe, and that invariant is
    /// exactly the one flaky suites break.
    pub confirm_minimality: bool,
    /// How many candidates may be evaluated at once.
    ///
    /// Probes at the same level of the search are independent, so this trades
    /// probes for wall clock: a wide round runs candidates a sequential pass
    /// would have skipped after an early hit. On a suite that takes a minute
    /// that is the right trade, and the answer is identical either way.
    ///
    /// Capped at the number of cells the caller supplies.
    pub parallelism: usize,
    /// How many times to evaluate the proof on the agent's result before
    /// trusting the answer.
    ///
    /// One by default. A second is a whole extra suite run — a sixth of the
    /// cost of a small map — spent to find out whether the suite is flaky. It
    /// is worth paying deliberately rather than on every run, and the
    /// confirmation pass reports contradictions anyway as monotonicity
    /// violations, which is the same finding arriving later and for free.
    pub stability_probes: u32,
    /// Per-command timeout inside a probe.
    pub command_timeout_ms: Option<u64>,
}

impl Default for NecessityConfig {
    fn default() -> Self {
        NecessityConfig {
            test_patterns: DEFAULT_TEST_PATTERNS.iter().map(|s| (*s).to_owned()).collect(),
            snapshot_patterns: DEFAULT_SNAPSHOT_PATTERNS.iter().map(|s| (*s).to_owned()).collect(),
            max_probes: None,
            confirm_minimality: true,
            parallelism: default_parallelism(),
            stability_probes: 1,
            command_timeout_ms: Some(10 * 60 * 1000),
        }
    }
}

impl NecessityConfig {
    /// Cap the number of probes.
    pub fn with_max_probes(mut self, probes: u32) -> Self {
        self.max_probes = Some(probes);
        self
    }

    /// Replace the test-path patterns.
    pub fn with_test_patterns<I: IntoIterator<Item = S>, S: Into<String>>(
        mut self,
        patterns: I,
    ) -> Self {
        self.test_patterns = patterns.into_iter().map(Into::into).collect();
        self
    }

    /// Re-check the proof on the agent's result before mapping.
    ///
    /// Costs one extra suite run and turns a flaky suite into a stated
    /// finding rather than a noisy map.
    pub fn with_stability_check(mut self) -> Self {
        self.stability_probes = 2;
        self
    }

    /// Evaluate one candidate at a time.
    pub fn sequential(mut self) -> Self {
        self.parallelism = 1;
        self
    }

    /// Build the matcher for test and snapshot paths.
    pub fn path_classifier(&self) -> Result<PathClassifier> {
        Ok(PathClassifier {
            tests: build_set(&self.test_patterns)?,
            snapshots: build_set(&self.snapshot_patterns)?,
        })
    }
}

/// How many probes to run at once when nothing says otherwise.
///
/// Capped well below the core count: each probe runs the repository's test
/// command, and most suites already use several cores themselves. Taking the
/// whole machine would make every probe slower and gain nothing.
pub fn default_parallelism() -> usize {
    std::thread::available_parallelism().map(|n| n.get().div_ceil(2)).unwrap_or(1).clamp(1, 4)
}

fn build_set(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = GlobBuilder::new(pattern).literal_separator(true).build().map_err(|e| {
            NecessityError::BadPattern { pattern: pattern.clone(), reason: e.to_string() }
        })?;
        builder.add(glob);
    }
    builder.build().map_err(|e| NecessityError::BadPattern {
        pattern: patterns.join(", "),
        reason: e.to_string(),
    })
}

/// Decides whether a path is a test or a recorded expectation.
#[derive(Clone, Debug)]
pub struct PathClassifier {
    tests: GlobSet,
    snapshots: GlobSet,
}

impl PathClassifier {
    /// Whether the path holds tests.
    pub fn is_test(&self, path: &str) -> bool {
        self.tests.is_match(path)
    }

    /// Whether the path holds a recorded expectation.
    pub fn is_snapshot(&self, path: &str) -> bool {
        self.snapshots.is_match(path)
    }

    /// Whether a load-bearing hunk here should be flagged.
    pub fn is_verification_surface(&self, path: &str) -> bool {
        self.is_test(path) || self.is_snapshot(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classifier() -> PathClassifier {
        NecessityConfig::default().path_classifier().unwrap()
    }

    #[test]
    fn the_usual_test_layouts_are_recognised() {
        let c = classifier();
        for path in [
            "tests/test_upload.py",
            "tests/unit/deep/test_upload.py",
            "test/helper.js",
            "src/auth/auth_test.go",
            "src/lib/parser_test.rs",
            "app/components/Button.test.tsx",
            "app/components/Button.spec.ts",
            "src/test/java/com/example/FooTest.java",
            "lib/__tests__/index.js",
            "spec/models/user_spec.rb",
            "tests/conftest.py",
        ] {
            assert!(c.is_test(path), "should be recognised as a test path: {path}");
        }
    }

    #[test]
    fn source_files_are_not_mistaken_for_tests() {
        let c = classifier();
        for path in [
            "src/api/upload.py",
            "src/middleware/rate.py",
            "src/utils/cache.py",
            "lib/protest.py",
            "src/contested.rs",
            "README.md",
            "latest/index.js",
        ] {
            assert!(!c.is_test(path), "should not be a test path: {path}");
        }
    }

    #[test]
    fn regenerated_snapshots_count_as_a_verification_surface() {
        let c = classifier();
        for path in [
            "src/__snapshots__/Button.snap",
            "tests/snapshots/api.ambr",
            "app/Button.test.tsx.snap",
        ] {
            assert!(c.is_verification_surface(path), "should be flagged: {path}");
        }
    }

    #[test]
    fn a_test_file_at_the_repository_root_is_recognised() {
        let c = classifier();
        assert!(c.is_test("test_main.py"), "`**/` must also match zero directories");
        assert!(c.is_test("main_test.go"));
    }

    #[test]
    fn a_malformed_pattern_is_reported_at_configuration_time() {
        let config = NecessityConfig::default().with_test_patterns(["["]);
        assert!(config.path_classifier().is_err());
    }
}
