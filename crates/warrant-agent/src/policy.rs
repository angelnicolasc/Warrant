//! What an agent is allowed to do, and what gets approved after the fact.
//!
//! # Blast radius
//!
//! A per-tool-call prompt gates *intent*: you approve "run a command" and
//! then find out what the command did. Cells make the better question
//! available, because the delta is observable — so approval here gates the
//! consequence:
//!
//! ```text
//! 4 files, egress to 2 hosts, 1 process — apply?
//! ```
//!
//! A rejection is not advice. The session restores the cell to the last
//! approved state and tells the model its change was rolled back, which means
//! a refusal actually refuses rather than merely disapproving.

use serde::{Deserialize, Serialize};

/// Limits applied to every tool call.
///
/// Serialisable because it is part of what a run *was*. A replay under a
/// different policy is a different run: a tool refused in one and permitted in
/// the other produces a different conversation from that point on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    /// Whether the agent may modify the cell at all.
    pub allow_writes: bool,
    /// Hosts `fetch` may reach. Empty denies everything.
    ///
    /// Default-deny, because an allow-list that starts open is an allow-list
    /// nobody ever closes.
    pub allowed_hosts: Vec<String>,
    /// Largest file `fs.read` will return.
    pub max_read_bytes: u64,
    /// Tool output longer than this becomes a handle instead of inline text.
    pub inline_limit: usize,
    /// Per-command timeout.
    pub command_timeout_ms: Option<u64>,
    /// Command prefixes the agent may not run.
    ///
    /// Not a security boundary — the cell is that. This stops an agent
    /// rewriting the history its own evidence is anchored to.
    pub denied_commands: Vec<String>,
}

impl Default for Policy {
    fn default() -> Self {
        Policy {
            allow_writes: true,
            allowed_hosts: Vec::new(),
            max_read_bytes: 2 * 1024 * 1024,
            inline_limit: 4096,
            command_timeout_ms: Some(10 * 60 * 1000),
            denied_commands: vec![
                "git push".into(),
                "git reset --hard".into(),
                "git rebase".into(),
                "git filter-branch".into(),
                "git filter-repo".into(),
            ],
        }
    }
}

impl Policy {
    /// A policy that refuses every modification.
    pub fn read_only() -> Self {
        Policy { allow_writes: false, ..Policy::default() }
    }

    /// Permit egress to a host.
    pub fn allowing_host(mut self, host: impl Into<String>) -> Self {
        self.allowed_hosts.push(host.into());
        self
    }

    /// Whether `fetch` may reach a host.
    ///
    /// Matches the host exactly or as a subdomain, so `example.com` permits
    /// `api.example.com` but never `notexample.com`.
    pub fn permits_host(&self, host: &str) -> bool {
        let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
        self.allowed_hosts.iter().any(|allowed| {
            let allowed = allowed.trim().to_ascii_lowercase();
            host == allowed || host.ends_with(&format!(".{allowed}"))
        })
    }

    /// Whether a command line is permitted.
    pub fn permits_command(&self, command: &str) -> bool {
        let normalised = command.split_whitespace().collect::<Vec<_>>().join(" ");
        !self.denied_commands.iter().any(|denied| {
            let denied = denied.split_whitespace().collect::<Vec<_>>().join(" ");
            normalised == denied || normalised.starts_with(&format!("{denied} "))
        })
    }
}

/// What a turn actually did, as observed rather than as described.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlastRadius {
    /// Files added or modified.
    pub files_changed: usize,
    /// Files removed.
    pub files_deleted: usize,
    /// Lines added and removed.
    pub changed_lines: u64,
    /// Hosts contacted.
    pub egress_hosts: Vec<String>,
    /// Commands run.
    pub processes: usize,
    /// Paths touched that hold tests or recorded expectations.
    pub verification_paths: Vec<String>,
}

impl BlastRadius {
    /// Whether anything happened at all.
    pub fn is_empty(&self) -> bool {
        self.files_changed == 0
            && self.files_deleted == 0
            && self.egress_hosts.is_empty()
            && self.processes == 0
    }

    /// One line, the way it is put to a person.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if self.files_changed > 0 {
            parts.push(format!("{} file{}", self.files_changed, plural(self.files_changed)));
        }
        if self.files_deleted > 0 {
            parts.push(format!("{} deleted", self.files_deleted));
        }
        if !self.egress_hosts.is_empty() {
            parts.push(format!(
                "egress to {} host{}",
                self.egress_hosts.len(),
                plural(self.egress_hosts.len())
            ));
        }
        if self.processes > 0 {
            parts.push(format!(
                "{} process{}",
                self.processes,
                if self.processes == 1 { "" } else { "es" }
            ));
        }
        if !self.verification_paths.is_empty() {
            parts.push(format!(
                "{} test file{}",
                self.verification_paths.len(),
                plural(self.verification_paths.len())
            ));
        }
        if parts.is_empty() { "nothing".to_string() } else { parts.join(", ") }
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// What to do with a turn's delta.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Keep it.
    Apply,
    /// Roll it back.
    Reject,
}

/// Decides whether a turn's delta is kept.
pub trait Approver: Send + Sync {
    /// Judge an observed delta.
    fn judge(&self, radius: &BlastRadius) -> Decision;
}

/// Keeps everything. The default for non-interactive runs.
#[derive(Debug, Default)]
pub struct ApproveAll;

impl Approver for ApproveAll {
    fn judge(&self, _radius: &BlastRadius) -> Decision {
        Decision::Apply
    }
}

/// Rolls back a turn that exceeds a stated blast radius.
///
/// The point is not the thresholds, which are the operator's business. It is
/// that they are applied to what happened rather than to what was asked for.
#[derive(Clone, Debug)]
pub struct ApproveWithin {
    /// Most files one turn may touch.
    pub max_files: usize,
    /// Whether a turn may delete files.
    pub allow_deletions: bool,
    /// Whether a turn may reach the network.
    pub allow_egress: bool,
    /// Whether a turn may edit tests or recorded expectations.
    pub allow_verification_edits: bool,
}

impl Default for ApproveWithin {
    fn default() -> Self {
        ApproveWithin {
            max_files: 25,
            allow_deletions: true,
            allow_egress: false,
            allow_verification_edits: true,
        }
    }
}

impl Approver for ApproveWithin {
    fn judge(&self, radius: &BlastRadius) -> Decision {
        let within = radius.files_changed + radius.files_deleted <= self.max_files
            && (self.allow_deletions || radius.files_deleted == 0)
            && (self.allow_egress || radius.egress_hosts.is_empty())
            && (self.allow_verification_edits || radius.verification_paths.is_empty());
        if within { Decision::Apply } else { Decision::Reject }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn egress_is_denied_by_default() {
        let policy = Policy::default();
        assert!(!policy.permits_host("example.com"));
        assert!(!policy.permits_host("anything.at.all"));
    }

    #[test]
    fn an_allowed_host_covers_its_subdomains_and_nothing_else() {
        let policy = Policy::default().allowing_host("example.com");
        assert!(policy.permits_host("example.com"));
        assert!(policy.permits_host("api.example.com"));
        assert!(policy.permits_host("EXAMPLE.COM"), "host matching is case-insensitive");
        assert!(!policy.permits_host("notexample.com"), "a suffix is not a subdomain");
        assert!(!policy.permits_host("example.com.evil.test"));
    }

    #[test]
    fn history_rewriting_commands_are_refused_by_default() {
        let policy = Policy::default();
        assert!(!policy.permits_command("git push --force"));
        assert!(!policy.permits_command("git  reset   --hard HEAD~3"));
        assert!(!policy.permits_command("git rebase -i main"));
        assert!(policy.permits_command("git status"));
        assert!(policy.permits_command("git commit -m x"));
        assert!(policy.permits_command("cargo test"));
    }

    #[test]
    fn a_denied_prefix_does_not_swallow_an_unrelated_command() {
        let policy = Policy::default();
        assert!(
            policy.permits_command("git pushover"),
            "prefix matching must respect word boundaries"
        );
    }

    #[test]
    fn a_blast_radius_reads_the_way_it_is_asked() {
        let radius = BlastRadius {
            files_changed: 4,
            files_deleted: 0,
            changed_lines: 60,
            egress_hosts: vec!["a.test".into(), "b.test".into()],
            processes: 1,
            verification_paths: Vec::new(),
        };
        assert_eq!(radius.summary(), "4 files, egress to 2 hosts, 1 process");
        assert!(!radius.is_empty());
        assert_eq!(BlastRadius::default().summary(), "nothing");
    }

    #[test]
    fn a_turn_inside_the_stated_radius_is_kept() {
        let approver = ApproveWithin::default();
        let radius = BlastRadius { files_changed: 3, processes: 1, ..Default::default() };
        assert_eq!(approver.judge(&radius), Decision::Apply);
    }

    #[test]
    fn a_turn_that_exceeds_the_stated_radius_is_rolled_back() {
        let approver = ApproveWithin { max_files: 2, ..Default::default() };
        let radius = BlastRadius { files_changed: 9, ..Default::default() };
        assert_eq!(approver.judge(&radius), Decision::Reject);
    }

    #[test]
    fn egress_and_test_edits_can_each_be_refused_on_their_own() {
        let no_egress = ApproveWithin { allow_egress: false, ..Default::default() };
        let reached_out =
            BlastRadius { egress_hosts: vec!["pastebin.test".into()], ..Default::default() };
        assert_eq!(no_egress.judge(&reached_out), Decision::Reject);

        let no_test_edits = ApproveWithin { allow_verification_edits: false, ..Default::default() };
        let edited_tests = BlastRadius {
            files_changed: 1,
            verification_paths: vec!["tests/test_upload.py".into()],
            ..Default::default()
        };
        assert_eq!(no_test_edits.judge(&edited_tests), Decision::Reject);
        assert_eq!(ApproveWithin::default().judge(&edited_tests), Decision::Apply);
    }

    #[test]
    fn approve_all_is_the_non_interactive_default() {
        let radius = BlastRadius { files_changed: 10_000, ..Default::default() };
        assert_eq!(ApproveAll.judge(&radius), Decision::Apply);
    }
}
