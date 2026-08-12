//! The six built-in tools.
//!
//! `fs`, `exec`, `fetch`, `declare`, `attest`, `delegate` — and deliberately
//! nothing else. Each built-in is another surface that has to produce
//! admissible evidence, so the catalogue is closed and anything further
//! belongs behind a protocol boundary rather than inside the trusted set.
//!
//! The set is not arbitrary. Two of the six exist so the agent can commit to
//! a claim and be judged on it; the other four are the smallest surface that
//! lets it do work worth judging.

use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use warrant_core::{ArtifactKind, ContextRenderable, Hash, ProofTier};
use warrant_diff::{Snapshot, join_relative};

use crate::error::{AgentError, Result};
use crate::provider::ToolSpec;
use crate::workspace::Workspace;

/// What a tool call produced.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolOutcome {
    /// What the model is told.
    pub content: String,
    /// Whether the call failed.
    pub is_error: bool,
}

impl ToolOutcome {
    /// A successful result.
    pub fn ok(content: impl Into<String>) -> Self {
        ToolOutcome { content: content.into(), is_error: false }
    }

    /// A failure the model should read and react to.
    ///
    /// Tool failures are reported back rather than raised: an agent that is
    /// told *why* it was refused can choose differently, and an agent that is
    /// killed cannot.
    pub fn failed(content: impl Into<String>) -> Self {
        ToolOutcome { content: content.into(), is_error: true }
    }
}

/// The closed set of built-in tools.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuiltinTool {
    /// Read, write, list and delete inside the cell.
    Fs,
    /// Run a command inside the cell.
    Exec,
    /// Reach an allow-listed host.
    Fetch,
    /// Seal a claim and the proof it will be judged by.
    Declare,
    /// Judge the open claim. Returns one bit.
    Attest,
    /// Hand a claim to a subagent with its own cell and budget.
    Delegate,
}

impl BuiltinTool {
    /// Every tool, in the order they are described to the model.
    pub const ALL: [BuiltinTool; 6] = [
        BuiltinTool::Declare,
        BuiltinTool::Fs,
        BuiltinTool::Exec,
        BuiltinTool::Fetch,
        BuiltinTool::Delegate,
        BuiltinTool::Attest,
    ];

    /// The name the model calls it by.
    pub fn name(&self) -> &'static str {
        match self {
            BuiltinTool::Fs => "fs",
            BuiltinTool::Exec => "exec",
            BuiltinTool::Fetch => "fetch",
            BuiltinTool::Declare => "declare",
            BuiltinTool::Attest => "attest",
            BuiltinTool::Delegate => "delegate",
        }
    }

    /// Resolve a name the model used.
    pub fn parse(name: &str) -> Option<Self> {
        BuiltinTool::ALL.into_iter().find(|tool| tool.name() == name)
    }

    /// How the tool is described to the model.
    pub fn spec(&self) -> ToolSpec {
        let (description, input_schema) = match self {
            BuiltinTool::Fs => (
                "Read, write, list or delete files inside the working cell. Reads return the \
                 content, or a handle when the file is large; pass `address` to read a handle you \
                 were given earlier.",
                json!({
                    "type": "object",
                    "properties": {
                        "op": { "type": "string", "enum": ["read", "write", "list", "delete"] },
                        "path": { "type": "string", "description": "Repository-relative path." },
                        "address": { "type": "string", "description": "A handle address, for reading stored output." },
                        "content": { "type": "string", "description": "New contents, for write." },
                        "offset": { "type": "integer", "description": "Byte offset, for read." },
                        "limit": { "type": "integer", "description": "Maximum bytes to return." }
                    },
                    "required": ["op"]
                }),
            ),
            BuiltinTool::Exec => (
                "Run a command in the cell. Not a shell: arguments are passed directly, so pipes \
                 and redirection are unavailable. Large output is returned as a handle.",
                json!({
                    "type": "object",
                    "properties": { "command": { "type": "string" } },
                    "required": ["command"]
                }),
            ),
            BuiltinTool::Fetch => (
                "Fetch a URL. Only hosts on the run's allow-list are reachable; everything else \
                 is refused and the refusal is recorded.",
                json!({
                    "type": "object",
                    "properties": { "url": { "type": "string" } },
                    "required": ["url"]
                }),
            ),
            BuiltinTool::Declare => (
                "State what you are about to achieve and the proof you agree to be judged by, \
                 before doing the work. The proof is sealed at this moment and cannot be changed \
                 afterwards. Example proof: exit(pytest -q) == 0 AND NOT diff_touches(\"tests/**\").",
                json!({
                    "type": "object",
                    "properties": {
                        "assertion": { "type": "string", "description": "What you will achieve." },
                        "proof": { "type": "string", "description": "The proof expression." },
                        "tier": { "type": "string", "enum": ["syntactic", "unit", "integration", "differential"] }
                    },
                    "required": ["assertion", "proof"]
                }),
            ),
            BuiltinTool::Attest => (
                "Judge the claim you declared. Returns `warranted` or `unproven` and nothing \
                 else — there is no score to optimise against.",
                json!({ "type": "object", "properties": {} }),
            ),
            BuiltinTool::Delegate => (
                "Hand one claim to a subagent with its own cell and budget. You receive a verdict \
                 and a one-line reason, never a transcript.",
                json!({
                    "type": "object",
                    "properties": {
                        "task": { "type": "string" },
                        "proof": { "type": "string" },
                        "max_turns": { "type": "integer" }
                    },
                    "required": ["task", "proof"]
                }),
            ),
        };

        ToolSpec { name: self.name().to_owned(), description: description.to_owned(), input_schema }
    }
}

/// Every tool's description, for a request.
pub fn all_specs() -> Vec<ToolSpec> {
    BuiltinTool::ALL.iter().map(BuiltinTool::spec).collect()
}

fn field<'a>(input: &'a Value, name: &str, tool: &'static str) -> Result<&'a str> {
    input.get(name).and_then(Value::as_str).ok_or_else(|| AgentError::BadToolInput {
        tool,
        reason: format!("`{name}` is required and must be a string"),
    })
}

/// Run one of the five tools that need only a workspace.
///
/// `delegate` is not here: it needs a provider, so the session runs it.
pub fn invoke(
    tool: BuiltinTool,
    input: &Value,
    workspace: &mut Workspace,
    probe_root: &Path,
) -> Result<ToolOutcome> {
    match tool {
        BuiltinTool::Fs => fs(input, workspace),
        BuiltinTool::Exec => exec(input, workspace),
        BuiltinTool::Fetch => fetch(input, workspace),
        BuiltinTool::Declare => declare(input, workspace),
        BuiltinTool::Attest => attest(workspace, probe_root),
        BuiltinTool::Delegate => Err(AgentError::BadToolInput {
            tool: "delegate",
            reason: "delegation is run by the session, not by the tool table".into(),
        }),
    }
}

fn fs(input: &Value, workspace: &mut Workspace) -> Result<ToolOutcome> {
    let op = field(input, "op", "fs")?;
    match op {
        "read" => fs_read(input, workspace),
        "list" => fs_list(input, workspace),
        "write" => fs_write(input, workspace),
        "delete" => fs_delete(input, workspace),
        other => Ok(ToolOutcome::failed(format!(
            "`{other}` is not an operation. Use read, write, list or delete."
        ))),
    }
}

fn fs_read(input: &Value, workspace: &mut Workspace) -> Result<ToolOutcome> {
    let offset = input.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
    let limit =
        input.get("limit").and_then(Value::as_u64).unwrap_or(workspace.policy().max_read_bytes)
            as usize;

    let bytes = if let Some(address) = input.get("address").and_then(Value::as_str) {
        let hash = Hash::parse(address).map_err(|_| AgentError::BadToolInput {
            tool: "fs",
            reason: format!("`{address}` is not a handle address"),
        })?;
        match workspace.store().get(&hash) {
            Ok(bytes) => bytes,
            Err(reason) => return Ok(ToolOutcome::failed(reason)),
        }
    } else {
        let path = field(input, "path", "fs")?;
        let snapshot = workspace.observe()?;
        match snapshot.content_of(path, workspace.store().as_ref())? {
            Some(bytes) => bytes,
            None => return Ok(ToolOutcome::failed(format!("{path} does not exist"))),
        }
    };

    let start = offset.min(bytes.len());
    let end = start.saturating_add(limit).min(bytes.len());
    let slice = &bytes[start..end];

    let mut content = workspace.present(ArtifactKind::FileContent, slice)?;
    if end < bytes.len() {
        content.push_str(&format!(
            "\n[{} of {} bytes shown; continue with offset {}]",
            end - start,
            bytes.len(),
            end
        ));
    }
    Ok(ToolOutcome::ok(content))
}

fn fs_list(input: &Value, workspace: &mut Workspace) -> Result<ToolOutcome> {
    let prefix = input.get("path").and_then(Value::as_str).unwrap_or("");
    let snapshot: Snapshot = workspace.observe()?;
    let mut paths: Vec<&str> = snapshot
        .files
        .keys()
        .map(String::as_str)
        .filter(|path| prefix.is_empty() || path.starts_with(prefix))
        .collect();
    paths.sort_unstable();

    if paths.is_empty() {
        return Ok(ToolOutcome::ok(format!("nothing under {:?}", prefix)));
    }
    let listing = paths.join("\n");
    Ok(ToolOutcome::ok(workspace.present(ArtifactKind::Blob, listing.as_bytes())?))
}

fn fs_write(input: &Value, workspace: &mut Workspace) -> Result<ToolOutcome> {
    if !workspace.policy().allow_writes {
        return Ok(ToolOutcome::failed("this run is read-only"));
    }
    let path = field(input, "path", "fs")?;
    let content = input.get("content").and_then(Value::as_str).unwrap_or("");

    let target = join_relative(workspace.cell().lock().expect("cell poisoned").root(), path)?;
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|source| AgentError::Io {
            context: format!("creating {}", parent.display()),
            source,
        })?;
    }
    std::fs::write(&target, content).map_err(|source| AgentError::Io {
        context: format!("writing {}", target.display()),
        source,
    })?;
    Ok(ToolOutcome::ok(format!("wrote {} bytes to {path}", content.len())))
}

fn fs_delete(input: &Value, workspace: &mut Workspace) -> Result<ToolOutcome> {
    if !workspace.policy().allow_writes {
        return Ok(ToolOutcome::failed("this run is read-only"));
    }
    let path = field(input, "path", "fs")?;
    let target = join_relative(workspace.cell().lock().expect("cell poisoned").root(), path)?;
    match std::fs::remove_file(&target) {
        Ok(()) => Ok(ToolOutcome::ok(format!("deleted {path}"))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Ok(ToolOutcome::failed(format!("{path} does not exist")))
        }
        Err(source) => {
            Err(AgentError::Io { context: format!("deleting {}", target.display()), source })
        }
    }
}

fn exec(input: &Value, workspace: &mut Workspace) -> Result<ToolOutcome> {
    let command = field(input, "command", "exec")?;
    let record = match workspace.exec(command) {
        Ok(record) => record,
        Err(AgentError::Refused { reason }) => return Ok(ToolOutcome::failed(reason)),
        Err(other) => return Err(other),
    };

    let stdout = workspace.store().get(&record.stdout).unwrap_or_default();
    let stderr = workspace.store().get(&record.stderr).unwrap_or_default();

    // The duration is recorded in the ledger but not reported here. It varies
    // between identical runs, which would make a session unreplayable, and it
    // is a fact about the machine rather than about the work.
    let mut content = if record.timed_out {
        "timed out".to_string()
    } else {
        format!("exit {}", record.code.unwrap_or(-1))
    };
    if !stdout.is_empty() {
        content.push_str("\n--- stdout ---\n");
        content.push_str(&workspace.present(ArtifactKind::Stdout, &stdout)?);
    }
    if !stderr.is_empty() {
        content.push_str("\n--- stderr ---\n");
        content.push_str(&workspace.present(ArtifactKind::Stderr, &stderr)?);
    }
    Ok(ToolOutcome { content, is_error: !record.succeeded() })
}

fn fetch(input: &Value, workspace: &mut Workspace) -> Result<ToolOutcome> {
    let url = field(input, "url", "fetch")?;
    let Some(host) = host_of(url) else {
        return Ok(ToolOutcome::failed(format!("{url} is not a URL this tool can read")));
    };
    if !workspace.policy().permits_host(&host) {
        return Ok(ToolOutcome::failed(format!(
            "{host} is not on this run's allow-list. Egress is denied by default; the operator \
             adds hosts explicitly."
        )));
    }

    workspace.record_egress(&host);
    let mut response = match ureq::get(url).call() {
        Ok(response) => response,
        Err(e) => return Ok(ToolOutcome::failed(format!("fetching {url}: {e}"))),
    };
    let status = response.status().as_u16();
    let body = match response.body_mut().read_to_vec() {
        Ok(body) => body,
        Err(e) => return Ok(ToolOutcome::failed(format!("reading {url}: {e}"))),
    };

    let presented = workspace.present(ArtifactKind::HttpBody, &body)?;
    Ok(ToolOutcome { content: format!("HTTP {status}\n{presented}"), is_error: status >= 400 })
}

fn declare(input: &Value, workspace: &mut Workspace) -> Result<ToolOutcome> {
    let assertion = field(input, "assertion", "declare")?;
    let proof = field(input, "proof", "declare")?;
    let tier = match input.get("tier").and_then(Value::as_str) {
        Some("syntactic") => ProofTier::Syntactic,
        Some("integration") => ProofTier::Integration,
        Some("differential") => ProofTier::Differential,
        _ => ProofTier::Unit,
    };

    match workspace.declare(assertion, proof, tier) {
        // The claim's identity is deliberately *not* reported here. It is
        // derived partly from the instant of declaration, so echoing it would
        // put wall-clock time into the context — and a context that differs
        // between two runs of the same script cannot be replayed. The id is in
        // the ledger, where it is useful; the model has one open claim at a
        // time and nothing to do with it.
        Ok(_) => Ok(ToolOutcome::ok(format!(
            "claim sealed at tier {}. It is now fixed: the proof cannot be changed, and you will \
             be judged by it exactly as written.",
            tier.name()
        ))),
        // A proof that will not compile is the agent's to fix, so the parse
        // error goes back rather than ending the run.
        Err(AgentError::Attest(e)) => {
            Ok(ToolOutcome::failed(format!("that proof does not parse:\n{e}")))
        }
        Err(AgentError::Refused { reason }) => Ok(ToolOutcome::failed(reason)),
        Err(other) => Err(other),
    }
}

fn attest(workspace: &mut Workspace, probe_root: &Path) -> Result<ToolOutcome> {
    match workspace.attest(probe_root) {
        Ok(verdict) => Ok(ToolOutcome::ok(verdict.render_for_model())),
        Err(AgentError::NoActiveClaim) => {
            Ok(ToolOutcome::failed("nothing has been declared, so there is nothing to attest"))
        }
        Err(other) => Err(other),
    }
}

/// Extract the host from a URL, ignoring any userinfo before it.
///
/// `https://allowed.test@evil.test/` is a request to **evil.test**. Reading
/// the host as the text after `://` would allow-list exactly the wrong one,
/// and it is a mistake that looks correct when skimmed.
pub fn host_of(url: &str) -> Option<String> {
    let after_scheme = url.split_once("://")?.1;
    let authority =
        after_scheme.split(['/', '?', '#']).next().filter(|authority| !authority.is_empty())?;
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
    let host = host_port.rsplit_once(':').map_or(host_port, |(host, port)| {
        if port.chars().all(|c| c.is_ascii_digit()) { host } else { host_port }
    });
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::Policy;
    use crate::test_support::scratch_workspace;

    #[test]
    fn the_catalogue_is_closed_at_six_and_names_are_unique() {
        let mut names: Vec<&str> = BuiltinTool::ALL.iter().map(|t| t.name()).collect();
        assert_eq!(names.len(), 6);
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), 6);
        assert_eq!(BuiltinTool::parse("exec"), Some(BuiltinTool::Exec));
        assert_eq!(BuiltinTool::parse("kubectl"), None);
    }

    #[test]
    fn declare_is_described_first_so_it_is_read_first() {
        assert_eq!(BuiltinTool::ALL[0], BuiltinTool::Declare);
        assert_eq!(*BuiltinTool::ALL.last().unwrap(), BuiltinTool::Attest);
    }

    #[test]
    fn every_spec_is_valid_json_schema_shaped() {
        for spec in all_specs() {
            assert!(!spec.description.is_empty());
            assert_eq!(spec.input_schema["type"], "object");
            assert!(spec.input_schema.get("properties").is_some());
        }
    }

    #[test]
    fn reading_a_file_returns_its_contents() {
        let (_g, mut ws) = scratch_workspace(&[("src/a.txt", "hello world")]);
        let out = fs(&json!({"op": "read", "path": "src/a.txt"}), &mut ws).unwrap();
        assert_eq!(out.content, "hello world");
        assert!(!out.is_error);
    }

    #[test]
    fn reading_a_missing_file_tells_the_model_rather_than_ending_the_run() {
        let (_g, mut ws) = scratch_workspace(&[("a.txt", "x")]);
        let out = fs(&json!({"op": "read", "path": "nope.txt"}), &mut ws).unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("does not exist"));
    }

    #[test]
    fn a_slice_of_a_file_can_be_read_and_says_how_to_continue() {
        let (_g, mut ws) = scratch_workspace(&[("a.txt", "0123456789")]);
        let out =
            fs(&json!({"op": "read", "path": "a.txt", "offset": 2, "limit": 3}), &mut ws).unwrap();
        assert!(out.content.starts_with("234"));
        assert!(out.content.contains("continue with offset 5"));
    }

    #[test]
    fn writing_then_reading_round_trips_through_the_cell() {
        let (_g, mut ws) = scratch_workspace(&[]);
        fs(&json!({"op": "write", "path": "new/deep.txt", "content": "written"}), &mut ws).unwrap();
        let out = fs(&json!({"op": "read", "path": "new/deep.txt"}), &mut ws).unwrap();
        assert_eq!(out.content, "written");
    }

    #[test]
    fn a_read_only_run_refuses_writes_and_deletes() {
        let (_g, mut ws) = scratch_workspace(&[("a.txt", "x")]);
        ws.set_policy(Policy::read_only());
        assert!(
            fs(&json!({"op": "write", "path": "a.txt", "content": "y"}), &mut ws).unwrap().is_error
        );
        assert!(fs(&json!({"op": "delete", "path": "a.txt"}), &mut ws).unwrap().is_error);
        assert_eq!(fs(&json!({"op": "read", "path": "a.txt"}), &mut ws).unwrap().content, "x");
    }

    #[test]
    fn listing_is_sorted_and_filterable() {
        let (_g, mut ws) =
            scratch_workspace(&[("src/b.txt", "1"), ("src/a.txt", "2"), ("docs/c.md", "3")]);
        let all = fs(&json!({"op": "list"}), &mut ws).unwrap();
        assert_eq!(all.content, "docs/c.md\nsrc/a.txt\nsrc/b.txt");

        let scoped = fs(&json!({"op": "list", "path": "src/"}), &mut ws).unwrap();
        assert_eq!(scoped.content, "src/a.txt\nsrc/b.txt");
    }

    #[test]
    fn a_path_cannot_escape_the_cell() {
        let (_g, mut ws) = scratch_workspace(&[("a.txt", "x")]);
        let escape =
            fs(&json!({"op": "write", "path": "../../outside.txt", "content": "x"}), &mut ws);
        assert!(escape.is_err(), "traversal must not be allowed");
    }

    #[test]
    fn exec_reports_the_exit_code_and_output() {
        let (_g, mut ws) = scratch_workspace(&[]);
        let out = exec(&json!({"command": "git --version"}), &mut ws).unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.starts_with("exit 0"));
        assert!(out.content.contains("git version"));
    }

    #[test]
    fn exec_refuses_history_rewriting_and_says_why() {
        let (_g, mut ws) = scratch_workspace(&[]);
        let out = exec(&json!({"command": "git push --force"}), &mut ws).unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("not permitted"));
    }

    #[test]
    fn fetch_is_denied_by_default_and_names_the_host() {
        let (_g, mut ws) = scratch_workspace(&[]);
        let out = fetch(&json!({"url": "https://pastebin.test/raw/abc"}), &mut ws).unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("pastebin.test"));
        assert!(out.content.contains("allow-list"));
    }

    #[test]
    fn a_url_with_userinfo_resolves_to_the_real_host() {
        assert_eq!(host_of("https://example.com/path"), Some("example.com".into()));
        assert_eq!(host_of("https://user:pw@evil.test/path"), Some("evil.test".into()));
        assert_eq!(
            host_of("https://allowed.test@evil.test/"),
            Some("evil.test".into()),
            "userinfo must not be mistaken for the host"
        );
        assert_eq!(host_of("http://localhost:8080/x"), Some("localhost".into()));
        assert_eq!(host_of("https://EXAMPLE.com"), Some("example.com".into()));
        assert_eq!(host_of("not a url"), None);
        assert_eq!(host_of("https:///nohost"), None);
    }

    #[test]
    fn declaring_seals_a_claim_and_says_it_is_fixed() {
        let (_g, mut ws) = scratch_workspace(&[("a.txt", "x")]);
        let out = declare(
            &json!({"assertion": "the suite passes", "proof": "exit(git --version) == 0"}),
            &mut ws,
        )
        .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("sealed"));
        assert!(ws.has_active_claim());
    }

    #[test]
    fn an_unparseable_proof_comes_back_to_the_model_to_fix() {
        let (_g, mut ws) = scratch_workspace(&[]);
        let out = declare(&json!({"assertion": "x", "proof": "tests_pass()"}), &mut ws).unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("does not parse"));
        assert!(!ws.has_active_claim());
    }

    #[test]
    fn attesting_returns_one_word_and_no_score() {
        let (g, mut ws) = scratch_workspace(&[("a.txt", "x")]);
        declare(&json!({"assertion": "x", "proof": "file_exists(b.txt)"}), &mut ws).unwrap();
        g.write("b.txt", "created");

        let out = attest(&mut ws, &g.probe_root()).unwrap();
        assert_eq!(out.content, "warranted");
        assert!(!out.content.contains('%'), "no coverage may reach the model");
    }

    #[test]
    fn a_claim_whose_proof_does_not_hold_comes_back_unproven() {
        let (g, mut ws) = scratch_workspace(&[("a.txt", "x")]);
        declare(&json!({"assertion": "x", "proof": "file_exists(never.txt)"}), &mut ws).unwrap();
        let out = attest(&mut ws, &g.probe_root()).unwrap();
        assert_eq!(out.content, "unproven");
    }

    #[test]
    fn attesting_nothing_is_reported_rather_than_raised() {
        let (g, mut ws) = scratch_workspace(&[]);
        let out = attest(&mut ws, &g.probe_root()).unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("nothing to attest"));
    }
}
