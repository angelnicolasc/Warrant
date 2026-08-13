//! Reading a run back: refutations, bisection and frozen fixtures.
//!
//! All three exist because the record is append-only and content-addressed
//! rather than because each was designed for. A ledger that can reconstruct
//! the world a run happened in gives you the failed approaches for free
//! (§5.6), the ability to fork at turn *k* (§5.4), and a regression fixture
//! (§5.8) — and the last of those is how a held-out set grows from real
//! incidents instead of invented ones.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use warrant_attest::{CellEnvironment, Predicate, ProbeEnvironment};
use warrant_core::{Hash, Ratio};
use warrant_diff::{ContentStore, Snapshot};
use warrant_ledger::{EntryKind, Ledger};

use crate::error::{AgentError, Result};
use crate::policy::ApproveAll;
use crate::provider::{RecordedTurn, ReplayProvider, recorded_turns};
use crate::session::{Session, SessionConfig, SessionOutcome};
use crate::workspace::{Services, Workspace};

/// A claim that failed, kept with enough to know why.
///
/// Every memory system stores successes. None stores refutations, which is
/// why agents re-attempt dead approaches across sessions.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refutation {
    /// Ledger position, so the surrounding evidence can be found.
    pub entry: u64,
    /// When it was recorded.
    pub at_ms: u64,
    /// The proof that did not hold.
    pub proof: String,
    /// How the search ended.
    pub outcome: String,
    /// Coverage, when there was any to speak of.
    pub coverage: Ratio,
    /// Files whose load-bearing changes sat on a verification surface.
    pub laundered: Vec<String>,
}

/// Every failed claim in a record, oldest first.
pub fn refutations(ledger: &Ledger) -> Result<Vec<Refutation>> {
    let mut out = Vec::new();
    for entry in ledger.entries()? {
        if entry.kind != EntryKind::Refutation {
            continue;
        }
        // Refutations are written from more than one shape of evidence — a
        // necessity map, or a best-of-N attempt. Anything that does not parse
        // as a map is still recorded; it simply carries less.
        let Ok(map) = ledger.payload_json::<warrant_necessity::NecessityMap>(&entry) else {
            continue;
        };
        out.push(Refutation {
            entry: entry.seq,
            at_ms: entry.at_ms,
            proof: map.predicate.to_string(),
            outcome: map.outcome.describe().to_owned(),
            coverage: map.coverage,
            laundered: map.tampered_files().map(|f| f.path.clone()).collect(),
        });
    }
    Ok(out)
}

/// A run, reconstructed from the record alone.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRecord {
    /// What was asked.
    pub task: String,
    /// Which model answered.
    pub model: String,
    /// What the agent was allowed to do.
    ///
    /// Part of the run, not of the reader. Replaying under a different policy
    /// diverges the moment a tool is refused in one and permitted in the other.
    pub policy: crate::policy::Policy,
    /// The tree the run started from.
    pub origin: Snapshot,
    /// Every exchange, in order.
    pub turns: Vec<RecordedTurn>,
}

impl RunRecord {
    /// Read the outermost run out of a ledger.
    ///
    /// Subagent runs write their own `RunStarted`; this takes the first, which
    /// is the session an operator actually started.
    pub fn read(ledger: &Ledger) -> Result<Self> {
        let entries = ledger.entries()?;

        let started =
            entries.iter().find(|entry| entry.kind == EntryKind::RunStarted).ok_or_else(|| {
                AgentError::Refused { reason: "this record contains no run to read".into() }
            })?;
        let header: serde_json::Value = ledger.payload_json(started)?;

        let origin_entry = entries
            .iter()
            .find(|entry| entry.kind == EntryKind::CellSnapshot && entry.seq > started.seq)
            .ok_or_else(|| AgentError::Refused {
                reason: "this record has no starting tree, so the run cannot be rebuilt".into(),
            })?;

        Ok(RunRecord {
            task: header.get("task").and_then(|v| v.as_str()).unwrap_or_default().to_owned(),
            model: header.get("model").and_then(|v| v.as_str()).unwrap_or_default().to_owned(),
            policy: header
                .get("policy")
                .and_then(|value| serde_json::from_value(value.clone()).ok())
                .unwrap_or_default(),
            origin: ledger.payload_json(origin_entry)?,
            turns: recorded_turns(ledger)?,
        })
    }

    /// How many turns were recorded.
    pub fn len(&self) -> usize {
        self.turns.len()
    }

    /// Whether anything was recorded.
    pub fn is_empty(&self) -> bool {
        self.turns.is_empty()
    }
}

/// Replay the first `turns` exchanges into a fresh cell and report the state.
///
/// This is a real re-execution: the tools run again, against a tree rebuilt
/// from the record. Substituting recorded tool output instead would score a
/// world that never existed, which is the mistake this whole layer is written
/// to avoid.
pub fn replay_prefix(
    record: &RunRecord,
    turns: usize,
    services: &Services,
    root: &Path,
) -> Result<(Snapshot, SessionOutcome)> {
    // The cell and the session's working directories are siblings. Nesting
    // them would put probe cells inside the tree being observed.
    let cell = crate::probe_cell(&root.join("cell"), &record.origin, Arc::clone(&services.store))?;

    // A replay writes to a scratch record, never to the operator's. Its own
    // model turns would otherwise be appended alongside the run's, and the
    // next read would find a recording that interleaves a session with its own
    // re-execution — which is not a recording of anything that happened.
    let scratch = Services {
        ledger: Arc::new(warrant_ledger::Ledger::open(root.join("scratch-ledger"))?),
        // The recorded policy, not the caller's. A reader who happened to open
        // the record read-only would otherwise replay a run in which every
        // write was refused, and get a confidently wrong answer.
        policy: record.policy.clone(),
        ..services.clone()
    };
    let workspace = Workspace::new(cell, scratch)?;

    let mut config = SessionConfig::new(&record.model, root.join("work"));
    config.max_turns = turns as u32;
    // A replay must not be cut short by a heuristic that was not part of the
    // original run.
    config.stuck_patience = u32::MAX;
    config.prune_patience = None;

    let provider = ReplayProvider::new(record.turns.clone());
    let mut session = Session::new(&provider, &ApproveAll, workspace, config);
    let outcome = session.run(&record.task)?;
    let state = session.workspace().observe()?;
    Ok((state, outcome))
}

/// Where a run stopped satisfying its proof.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bisection {
    /// How many turns the run had.
    pub turns: usize,
    /// The first turn after which the proof no longer held.
    ///
    /// `None` when the proof held at every point, or held nowhere.
    pub first_bad_turn: Option<usize>,
    /// Whether the proof held on the starting state.
    pub held_at_start: bool,
    /// Whether the proof held at the end.
    pub held_at_end: bool,
    /// How many replays it took.
    pub replays: u32,
}

impl Bisection {
    /// A line for the terminal.
    pub fn describe(&self) -> String {
        match self.first_bad_turn {
            Some(turn) => format!(
                "the proof held through turn {} and stopped holding at turn {turn} ({} replays over {} turns)",
                turn - 1,
                self.replays,
                self.turns
            ),
            None if self.held_at_end => "the proof held at every point in the run".into(),
            None if !self.held_at_start => {
                "the proof did not hold at the start either, so there is no first bad turn".into()
            }
            None => "the proof never held".into(),
        }
    }
}

/// Binary-search a run for the turn its proof stopped holding.
///
/// The naive version re-runs from the beginning once per turn. This is
/// `O(log n)` replays, which is what makes it usable on a two-hundred-turn
/// run — the same reason the necessity search partitions rather than
/// enumerating.
pub fn bisect(
    record: &RunRecord,
    proof: &Predicate,
    services: &Services,
    root: &Path,
) -> Result<Bisection> {
    let mut replays = 0;
    let holds_at = |turns: usize, replays: &mut u32| -> Result<bool> {
        *replays += 1;
        let at = root.join(format!("turn-{turns}"));
        let (state, _) = replay_prefix(record, turns, services, &at)?;
        evaluate(proof, &record.origin, &state, services, &at)
    };

    let total = record.turns.len();
    let held_at_start = holds_at(0, &mut replays)?;
    let held_at_end = holds_at(total, &mut replays)?;

    // A first bad turn only means something if the proof held to begin with
    // and stopped holding by the end.
    if !held_at_start || held_at_end {
        return Ok(Bisection {
            turns: total,
            first_bad_turn: None,
            held_at_start,
            held_at_end,
            replays,
        });
    }

    // Invariant: it holds at `good` and fails at `bad`.
    let (mut good, mut bad) = (0usize, total);
    while bad - good > 1 {
        let middle = good + (bad - good) / 2;
        if holds_at(middle, &mut replays)? {
            good = middle;
        } else {
            bad = middle;
        }
    }

    Ok(Bisection { turns: total, first_bad_turn: Some(bad), held_at_start, held_at_end, replays })
}

fn evaluate(
    proof: &Predicate,
    origin: &Snapshot,
    state: &Snapshot,
    services: &Services,
    root: &Path,
) -> Result<bool> {
    let cell = crate::probe_cell(&root.join("evaluate"), state, Arc::clone(&services.store))?;
    let environment: Arc<dyn ProbeEnvironment> =
        Arc::new(CellEnvironment::new(cell, origin, state, services.policy.command_timeout_ms));
    Ok(services.attestor.evaluate(proof, environment)?)
}

/// A run turned into a regression test.
///
/// Self-contained on purpose: the starting tree's bytes travel inside it, so
/// a fixture is one file that can be committed, moved between machines and
/// replayed years later without the ledger it came from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fixture {
    /// Format version, so an old fixture fails clearly rather than oddly.
    pub version: u32,
    /// The run.
    pub record: RunRecord,
    /// Every blob the starting tree needs, hex-addressed.
    pub blobs: BTreeMap<String, String>,
    /// What the original run produced, for a replay to be checked against.
    pub expected: Expectation,
}

/// What a frozen run is expected to reproduce.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Expectation {
    /// The tree the run ended with.
    pub final_tree: Hash,
    /// How many turns it took.
    pub turns: u32,
    /// How many claims it discharged.
    pub warranted_claims: usize,
}

/// The fixture format this build writes and reads.
pub const FIXTURE_VERSION: u32 = 1;

impl Fixture {
    /// Freeze a finished run.
    pub fn freeze(
        record: RunRecord,
        final_tree: Hash,
        turns: u32,
        warranted_claims: usize,
        store: &dyn ContentStore,
    ) -> Result<Self> {
        let mut blobs = BTreeMap::new();
        for meta in record.origin.files.values() {
            let bytes =
                store.get(&meta.content).map_err(|reason| AgentError::Refused { reason })?;
            blobs.insert(meta.content.to_hex(), hex::encode(&bytes));
        }
        Ok(Fixture {
            version: FIXTURE_VERSION,
            record,
            blobs,
            expected: Expectation { final_tree, turns, warranted_claims },
        })
    }

    /// Put the fixture's blobs into a store so its tree can be materialised.
    pub fn hydrate(&self, store: &dyn ContentStore) -> Result<()> {
        if self.version != FIXTURE_VERSION {
            return Err(AgentError::Refused {
                reason: format!(
                    "this fixture is version {} and this build reads version {FIXTURE_VERSION}",
                    self.version
                ),
            });
        }
        for (address, encoded) in &self.blobs {
            let bytes = hex::decode(encoded).map_err(|_| AgentError::Refused {
                reason: format!("blob {address} in this fixture is not readable"),
            })?;
            store.put(&bytes).map_err(|reason| AgentError::Refused { reason })?;
        }
        Ok(())
    }

    /// Replay the fixture and report whether it reproduced.
    pub fn replay(&self, services: &Services, root: &Path) -> Result<Reproduction> {
        self.hydrate(services.store.as_ref())?;
        let (state, outcome) =
            replay_prefix(&self.record, self.record.turns.len(), services, root)?;

        let warranted = outcome.discharged.iter().filter(|claim| claim.warranted).count();
        Ok(Reproduction {
            reproduced: state.root_hash() == self.expected.final_tree
                && outcome.turns == self.expected.turns
                && warranted == self.expected.warranted_claims,
            actual_tree: state.root_hash(),
            actual_turns: outcome.turns,
            actual_warranted: warranted,
            expected: self.expected.clone(),
        })
    }
}

/// What replaying a fixture produced.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reproduction {
    /// Whether everything matched.
    pub reproduced: bool,
    /// The tree the replay ended with.
    pub actual_tree: Hash,
    /// Turns the replay took.
    pub actual_turns: u32,
    /// Claims the replay discharged.
    pub actual_warranted: usize,
    /// What was expected.
    pub expected: Expectation,
}

impl Reproduction {
    /// A line for the terminal.
    pub fn describe(&self) -> String {
        if self.reproduced {
            return format!(
                "reproduced: {} turns, {} warranted, tree {}",
                self.actual_turns,
                self.actual_warranted,
                self.actual_tree.short()
            );
        }
        let mut differences = Vec::new();
        if self.actual_tree != self.expected.final_tree {
            differences.push(format!(
                "tree {} instead of {}",
                self.actual_tree.short(),
                self.expected.final_tree.short()
            ));
        }
        if self.actual_turns != self.expected.turns {
            differences
                .push(format!("{} turns instead of {}", self.actual_turns, self.expected.turns));
        }
        if self.actual_warranted != self.expected.warranted_claims {
            differences.push(format!(
                "{} warranted instead of {}",
                self.actual_warranted, self.expected.warranted_claims
            ));
        }
        format!("did not reproduce: {}", differences.join(", "))
    }
}
