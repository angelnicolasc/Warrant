//! The search.
//!
//! ```text
//! satisfaction → null test → delta debugging → per-hunk confirmation
//! ```
//!
//! Each step can end the search early, and each early ending is a distinct
//! finding rather than a coverage of zero. That distinction is the whole
//! difference between a measurement and a number.
//!
//! # What actually costs time
//!
//! A probe is one run of the repository's test command, so probe count is the
//! overhead. Measured on this build, a three-hunk diff costs six probes and a
//! sixty-six-hunk diff costs eleven: the search is genuinely logarithmic, and
//! **the fixed cost dominates** — satisfaction, the null test, and the
//! confirmation pass, before the partitioning does any work at all. Which is
//! why the second satisfaction probe is opt-in: on a small map it is a sixth
//! of the total spent to ask whether the suite is flaky, and the confirmation
//! pass reports that anyway, as a monotonicity violation.
//!
//! So the thing worth attacking is not the algorithm but the wall clock, and
//! probes at the same level of the search are independent. Given a pool of
//! cells they run concurrently, and the number a person waits on becomes
//! *rounds* rather than probes. Answers are unchanged: within a round the
//! lowest-index candidate that passes is the one taken, which is exactly what
//! a sequential pass with an early exit would have chosen.
//!
//! Measured on this build, four cells against one, on a fixture whose proof
//! carries a two-second floor — the honest model of a suite:
//!
//! ```text
//! hunks   probes 1→4   rounds 1→4   wall clock 1→4
//!     6      6 →  8      6 → 4      13.3s →  9.1s   -31%
//!    18      8 → 12      8 → 6      17.5s → 13.6s   -22%
//!    66     10 → 16     10 → 8      22.2s → 18.3s   -17%
//! ```
//!
//! Note what the trade is: a third more probes for a fifth to a third less
//! time. On a suite that runs in milliseconds the trade inverts and the pool
//! loses to its own setup — 1.7s to 2.2s at sixty-six hunks — which is what
//! `--jobs 1` is for. Two of the rounds saved come from the fixed cost rather
//! than the search: satisfaction and the null test are independent questions,
//! so with a spare cell the second rides along with the first.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use warrant_attest::{Attestor, CellEnvironment, Predicate, ProbeEnvironment};
use warrant_cell::{Cell, ExitRecord};
use warrant_core::{ClaimId, Hash, HunkId, Ratio};
use warrant_diff::{ContentStore, Hunk, OverlayDiff, Snapshot, apply_subset};

use crate::config::NecessityConfig;
use crate::ddmin::{ProbeBudget, confirm_minimal_wide, ddmin_wide};
use crate::error::{NecessityError, Result};
use crate::map::{FileVerdict, MapOutcome, NecessityMap};

/// Everything one search needs.
pub struct Search<'a> {
    /// One cell per concurrent probe. A probe rewrites the filesystem, so
    /// concurrency needs separate cells rather than separate threads.
    cells: Vec<Arc<Mutex<dyn Cell>>>,
    pre: &'a Snapshot,
    post: &'a Snapshot,
    diff: &'a OverlayDiff,
    store: &'a dyn ContentStore,
    attestor: &'a Attestor,
    predicate: &'a Predicate,
    config: &'a NecessityConfig,
    claim: Option<ClaimId>,

    /// Probe results, keyed by the address of the tree that was tested.
    ///
    /// Keying on the *tree* rather than on the hunk subset means two
    /// different subsets that reconstruct the same file contents share one
    /// answer. That happens more than it sounds like it should — reverting a
    /// hunk whose insertion and deletion cancel out is the common case.
    cache: Mutex<HashMap<Hash, bool>>,
    probes: AtomicU32,
    rounds: AtomicU32,
    commands: Mutex<Vec<ExitRecord>>,
}

impl<'a> Search<'a> {
    /// Prepare a search over a pool of cells.
    ///
    /// One cell is enough and is what a sequential search uses; more cells
    /// buy wall clock. Every cell must already hold the pre-state.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cells: Vec<Arc<Mutex<dyn Cell>>>,
        pre: &'a Snapshot,
        post: &'a Snapshot,
        diff: &'a OverlayDiff,
        store: &'a dyn ContentStore,
        attestor: &'a Attestor,
        predicate: &'a Predicate,
        config: &'a NecessityConfig,
    ) -> Self {
        Search {
            cells,
            pre,
            post,
            diff,
            store,
            attestor,
            predicate,
            config,
            claim: None,
            cache: Mutex::new(HashMap::new()),
            probes: AtomicU32::new(0),
            rounds: AtomicU32::new(0),
            commands: Mutex::new(Vec::new()),
        }
    }

    /// Attach the claim this map belongs to.
    pub fn for_claim(mut self, claim: ClaimId) -> Self {
        self.claim = Some(claim);
        self
    }

    /// Every command run during the search, for the ledger.
    pub fn commands(&self) -> Vec<ExitRecord> {
        self.commands.lock().expect("search poisoned").clone()
    }

    /// How many candidates can be evaluated at once.
    fn width(&self) -> usize {
        self.config.parallelism.clamp(1, self.cells.len().max(1))
    }

    /// Evaluate one candidate tree in one cell.
    fn evaluate_in(&self, cell: &Arc<Mutex<dyn Cell>>, candidate: &Snapshot) -> Result<bool> {
        {
            let mut guard = cell.lock().expect("cell poisoned");
            guard.restore(candidate)?;
        }
        let environment = Arc::new(CellEnvironment::new(
            Arc::clone(cell),
            self.pre,
            candidate,
            self.config.command_timeout_ms,
        ));
        let shared: Arc<dyn ProbeEnvironment> =
            Arc::clone(&environment) as Arc<dyn ProbeEnvironment>;
        let result = self.attestor.evaluate(self.predicate, shared)?;

        self.commands.lock().expect("search poisoned").extend(environment.commands_run());
        self.probes.fetch_add(1, Ordering::Relaxed);
        Ok(result)
    }

    /// Evaluate a batch of candidate subsets, using the cache and the pool.
    ///
    /// One call is one round, which is the unit a person waits on.
    fn probe_batch(&self, subsets: &[Vec<HunkId>], use_cache: bool) -> Result<Vec<bool>> {
        let mut prepared = Vec::with_capacity(subsets.len());
        for subset in subsets {
            let set: BTreeSet<HunkId> = subset.iter().copied().collect();
            let candidate = apply_subset(self.pre, self.post, self.diff, &set, self.store)?;
            prepared.push((candidate.root_hash(), candidate));
        }

        let mut results: Vec<Option<bool>> = vec![None; prepared.len()];
        if use_cache {
            let cache = self.cache.lock().expect("search poisoned");
            for (index, (key, _)) in prepared.iter().enumerate() {
                results[index] = cache.get(key).copied();
            }
        }

        // Two candidates in one batch can reconstruct the same tree. Running
        // it twice would be a wasted suite run, so only the first is queued.
        let mut queued: Vec<usize> = Vec::new();
        let mut seen: BTreeMap<Hash, usize> = BTreeMap::new();
        for index in 0..prepared.len() {
            if results[index].is_some() {
                continue;
            }
            match seen.get(&prepared[index].0) {
                Some(_) => {}
                None => {
                    seen.insert(prepared[index].0, index);
                    queued.push(index);
                }
            }
        }

        if !queued.is_empty() {
            self.rounds.fetch_add(1, Ordering::Relaxed);
            let outcomes: Mutex<Vec<(usize, bool)>> = Mutex::new(Vec::new());
            let failure: Mutex<Option<NecessityError>> = Mutex::new(None);
            let workers = self.width().min(queued.len());

            std::thread::scope(|scope| {
                for worker in 0..workers {
                    let cell = &self.cells[worker];
                    let mine: Vec<usize> =
                        queued.iter().copied().skip(worker).step_by(workers).collect();
                    let outcomes = &outcomes;
                    let failure = &failure;
                    let prepared = &prepared;

                    scope.spawn(move || {
                        for index in mine {
                            if failure.lock().expect("search poisoned").is_some() {
                                return;
                            }
                            match self.evaluate_in(cell, &prepared[index].1) {
                                Ok(result) => {
                                    outcomes.lock().expect("search poisoned").push((index, result))
                                }
                                Err(error) => {
                                    let mut slot = failure.lock().expect("search poisoned");
                                    if slot.is_none() {
                                        *slot = Some(error);
                                    }
                                    return;
                                }
                            }
                        }
                    });
                }
            });

            if let Some(error) = failure.into_inner().expect("search poisoned") {
                return Err(error);
            }
            let mut cache = self.cache.lock().expect("search poisoned");
            for (index, result) in outcomes.into_inner().expect("search poisoned") {
                results[index] = Some(result);
                cache.insert(prepared[index].0, result);
            }
            // Duplicates within the batch take the answer their twin got.
            for index in 0..prepared.len() {
                if results[index].is_none()
                    && let Some(answer) = cache.get(&prepared[index].0).copied()
                {
                    results[index] = Some(answer);
                }
            }
        }

        Ok(results.into_iter().map(|r| r.unwrap_or(false)).collect())
    }

    /// Probe one subset, reusing an earlier answer for an identical tree.
    fn probe(&self, subset: &BTreeSet<HunkId>) -> Result<bool> {
        let one = vec![subset.iter().copied().collect::<Vec<_>>()];
        Ok(self.probe_batch(&one, true)?[0])
    }

    /// Probe without consulting the cache, for the stability check.
    fn probe_fresh(&self, subset: &BTreeSet<HunkId>) -> Result<bool> {
        let one = vec![subset.iter().copied().collect::<Vec<_>>()];
        Ok(self.probe_batch(&one, false)?[0])
    }

    /// Run the search and produce the map.
    pub fn run(&mut self) -> Result<NecessityMap> {
        let all: BTreeSet<HunkId> = self.diff.hunk_ids().into_iter().collect();
        let mut map = self.blank(MapOutcome::Mapped);

        if self.diff.is_empty() {
            let mut empty = NecessityMap::no_changes(self.predicate.hash(), self.diff.pre_root);
            empty.claim = self.claim;
            return Ok(empty);
        }

        // 1 — Satisfaction, optionally re-checked. A proof that answers
        //     differently on identical state cannot be delta-debugged, and
        //     saying so is more useful than mapping noise.
        //
        //     The null test that follows does not depend on this answer, only
        //     on whether it is needed. With a spare cell it therefore rides
        //     along in the same round: a wasted probe when the proof does not
        //     hold, and one round of the fixed cost removed when it does. With
        //     one cell there is no spare, and speculating would cost real time.
        let speculate = self.width() > 1 && self.config.stability_probes <= 1;
        let mut null_answer = None;
        let satisfied = if speculate {
            let batch = vec![all.iter().copied().collect::<Vec<_>>(), Vec::new()];
            let answers = self.probe_batch(&batch, false)?;
            null_answer = Some(answers[1]);
            answers[0]
        } else {
            self.probe_fresh(&all)?
        };
        for _ in 1..self.config.stability_probes.max(1) {
            if self.probe_fresh(&all)? != satisfied {
                map.outcome = MapOutcome::UnstableProof;
                map.satisfied = satisfied;
                map.probes = self.probes.load(Ordering::Relaxed);
                map.rounds = self.rounds.load(Ordering::Relaxed);
                return Ok(map);
            }
        }

        map.satisfied = satisfied;
        if !satisfied {
            map.outcome = MapOutcome::NotSatisfied;
            map.probes = self.probes.load(Ordering::Relaxed);
            map.rounds = self.rounds.load(Ordering::Relaxed);
            return Ok(map);
        }

        // 2 — Null test. If the proof already held on the pre-state it proves
        //     nothing about the work, whatever the work was.
        let held_before = match null_answer {
            Some(answer) => answer,
            None => self.probe(&BTreeSet::new())?,
        };
        map.null_passed = !held_before;
        if held_before {
            map.outcome = MapOutcome::Vacuous;
            map.unproven = self.diff.hunk_ids();
            map.files = self.file_verdicts(&BTreeSet::new())?;
            map.probes = self.probes.load(Ordering::Relaxed);
            map.rounds = self.rounds.load(Ordering::Relaxed);
            return Ok(map);
        }

        // 3 — Delta debugging.
        let mut budget = match self.config.max_probes {
            Some(limit) => {
                ProbeBudget::limited(limit.saturating_sub(self.probes.load(Ordering::Relaxed)))
            }
            None => ProbeBudget::unlimited(),
        };
        let universe = self.diff.hunk_ids();
        let width = self.width();

        let mut batch = |candidates: &[Vec<HunkId>]| self.probe_batch(candidates, true);
        let minimal = ddmin_wide(&universe, &mut batch, &mut budget, width)?;

        // 4 — Per-hunk confirmation, so the map's claim about each hunk has a
        //     probe behind it rather than an algorithm's invariant.
        let (load_bearing, violations, confirmed) = if self.config.confirm_minimality {
            let outcome = confirm_minimal_wide(&minimal, &mut batch, &mut budget, width)?;
            (outcome.subset, outcome.dropped, outcome.complete)
        } else {
            (minimal, Vec::new(), false)
        };

        let proven: BTreeSet<HunkId> = load_bearing.iter().copied().collect();
        map.load_bearing = ordered(&self.diff.hunks, &proven);
        map.unproven = self.diff.hunk_ids().into_iter().filter(|h| !proven.contains(h)).collect();
        map.monotonicity_violations = violations;
        map.minimality_confirmed = confirmed;
        map.budget_exhausted = budget.exhausted();
        map.probes = self.probes.load(Ordering::Relaxed);
        map.rounds = self.rounds.load(Ordering::Relaxed);

        map.coverage = Ratio::new(self.diff.changed_lines_in(&proven), self.diff.changed_lines());
        map.hunk_coverage = Ratio::new(proven.len() as u64, self.diff.hunk_count() as u64);

        map.files = self.file_verdicts(&proven)?;

        // The signature: a hunk that is load-bearing *and* sits on a
        // verification surface. Part of why the proof passes is that the
        // agent changed the thing doing the proving.
        let surfaces: BTreeSet<&str> =
            map.files.iter().filter(|f| f.verification_surface).map(|f| f.path.as_str()).collect();
        map.tamper = map
            .load_bearing
            .iter()
            .copied()
            .filter(|id| self.diff.hunk(*id).is_some_and(|h| surfaces.contains(h.path.as_str())))
            .collect();

        Ok(map)
    }

    fn blank(&self, outcome: MapOutcome) -> NecessityMap {
        NecessityMap {
            claim: self.claim,
            predicate: self.predicate.hash(),
            outcome,
            satisfied: false,
            null_passed: false,
            load_bearing: Vec::new(),
            unproven: Vec::new(),
            tamper: Vec::new(),
            coverage: Ratio::UNDEFINED,
            hunk_coverage: Ratio::UNDEFINED,
            files: Vec::new(),
            probes: 0,
            rounds: 0,
            budget_exhausted: false,
            minimality_confirmed: false,
            monotonicity_violations: Vec::new(),
            pre_root: self.diff.pre_root,
            post_root: self.diff.post_root,
        }
    }

    fn file_verdicts(&self, proven: &BTreeSet<HunkId>) -> Result<Vec<FileVerdict>> {
        let classifier = self.config.path_classifier()?;
        let mut by_path: BTreeMap<&str, FileVerdict> = BTreeMap::new();

        for delta in &self.diff.files {
            let surface = classifier.is_verification_surface(&delta.path);
            by_path.insert(
                delta.path.as_str(),
                FileVerdict {
                    path: delta.path.clone(),
                    change: delta.change,
                    total_hunks: 0,
                    load_bearing_hunks: 0,
                    changed_lines: 0,
                    proven_lines: 0,
                    verification_surface: surface,
                    tampered: false,
                },
            );
        }

        for hunk in &self.diff.hunks {
            let Some(verdict) = by_path.get_mut(hunk.path.as_str()) else { continue };
            verdict.total_hunks += 1;
            verdict.changed_lines += hunk.changed_lines();
            if proven.contains(&hunk.id) {
                verdict.load_bearing_hunks += 1;
                verdict.proven_lines += hunk.changed_lines();
            }
        }

        Ok(by_path
            .into_values()
            .map(|mut v| {
                v.tampered = v.verification_surface && v.load_bearing_hunks > 0;
                v
            })
            .collect())
    }
}

/// Put a set of hunk ids back into diff order, so the map reads top to bottom.
fn ordered(hunks: &[Hunk], selected: &BTreeSet<HunkId>) -> Vec<HunkId> {
    hunks.iter().map(|h| h.id).filter(|id| selected.contains(id)).collect()
}
