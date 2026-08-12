//! The search.
//!
//! ```text
//! satisfaction → null test → delta debugging → per-hunk confirmation
//! ```
//!
//! Each step can end the search early, and each early ending is a distinct
//! finding rather than a coverage of zero. That distinction is the whole
//! difference between a measurement and a number.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

use warrant_attest::{Attestor, CellEnvironment, Predicate, ProbeEnvironment};
use warrant_cell::{Cell, ExitRecord};
use warrant_core::{ClaimId, Hash, HunkId, Ratio};
use warrant_diff::{ContentStore, Hunk, OverlayDiff, Snapshot, apply_subset};

use crate::config::NecessityConfig;
use crate::ddmin::{ProbeBudget, confirm_minimal, ddmin};
use crate::error::Result;
use crate::map::{FileVerdict, MapOutcome, NecessityMap};

/// Everything one search needs.
pub struct Search<'a> {
    cell: Arc<Mutex<dyn Cell>>,
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
    cache: HashMap<Hash, bool>,
    probes: u32,
    commands: Vec<ExitRecord>,
}

impl<'a> Search<'a> {
    /// Prepare a search.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cell: Arc<Mutex<dyn Cell>>,
        pre: &'a Snapshot,
        post: &'a Snapshot,
        diff: &'a OverlayDiff,
        store: &'a dyn ContentStore,
        attestor: &'a Attestor,
        predicate: &'a Predicate,
        config: &'a NecessityConfig,
    ) -> Self {
        Search {
            cell,
            pre,
            post,
            diff,
            store,
            attestor,
            predicate,
            config,
            claim: None,
            cache: HashMap::new(),
            probes: 0,
            commands: Vec::new(),
        }
    }

    /// Attach the claim this map belongs to.
    pub fn for_claim(mut self, claim: ClaimId) -> Self {
        self.claim = Some(claim);
        self
    }

    /// Every command run during the search, for the ledger.
    pub fn commands(&self) -> &[ExitRecord] {
        &self.commands
    }

    /// Materialise a candidate tree and evaluate the proof against it.
    fn evaluate_subset(&mut self, subset: &BTreeSet<HunkId>, use_cache: bool) -> Result<bool> {
        let candidate = apply_subset(self.pre, self.post, self.diff, subset, self.store)?;
        let key = candidate.root_hash();
        if use_cache && let Some(cached) = self.cache.get(&key) {
            return Ok(*cached);
        }

        {
            let mut cell = self.cell.lock().expect("cell poisoned");
            cell.restore(&candidate)?;
        }

        let environment = Arc::new(CellEnvironment::new(
            Arc::clone(&self.cell),
            self.pre,
            &candidate,
            self.config.command_timeout_ms,
        ));
        let shared: Arc<dyn ProbeEnvironment> =
            Arc::clone(&environment) as Arc<dyn ProbeEnvironment>;
        let result = self.attestor.evaluate(self.predicate, shared)?;

        self.commands.extend(environment.commands_run());
        self.probes += 1;
        self.cache.insert(key, result);
        Ok(result)
    }

    /// Probe, reusing an earlier answer for an identical tree.
    fn probe(&mut self, subset: &BTreeSet<HunkId>) -> Result<bool> {
        self.evaluate_subset(subset, true)
    }

    /// Probe without consulting the cache, for the stability check.
    fn probe_fresh(&mut self, subset: &BTreeSet<HunkId>) -> Result<bool> {
        self.evaluate_subset(subset, false)
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

        // 1 — Satisfaction, with a stability check. A proof that answers
        //     differently on identical state cannot be delta-debugged, and
        //     saying so is more useful than mapping noise.
        let satisfied = self.probe_fresh(&all)?;
        for _ in 1..self.config.stability_probes.max(1) {
            if self.probe_fresh(&all)? != satisfied {
                map.outcome = MapOutcome::UnstableProof;
                map.satisfied = satisfied;
                map.probes = self.probes;
                return Ok(map);
            }
        }

        map.satisfied = satisfied;
        if !satisfied {
            map.outcome = MapOutcome::NotSatisfied;
            map.probes = self.probes;
            return Ok(map);
        }

        // 2 — Null test. If the proof already held on the pre-state it proves
        //     nothing about the work, whatever the work was.
        let held_before = self.probe(&BTreeSet::new())?;
        map.null_passed = !held_before;
        if held_before {
            map.outcome = MapOutcome::Vacuous;
            map.unproven = self.diff.hunk_ids();
            map.files = self.file_verdicts(&BTreeSet::new())?;
            map.probes = self.probes;
            return Ok(map);
        }

        // 3 — Delta debugging.
        let mut budget = match self.config.max_probes {
            Some(limit) => ProbeBudget::limited(limit.saturating_sub(self.probes)),
            None => ProbeBudget::unlimited(),
        };
        let universe = self.diff.hunk_ids();

        let minimal = {
            let mut probe = |subset: &[HunkId]| -> Result<bool> {
                let set: BTreeSet<HunkId> = subset.iter().copied().collect();
                self.probe(&set)
            };
            ddmin(&universe, &mut probe, &mut budget)?
        };

        // 4 — Per-hunk confirmation, so the map's claim about each hunk has a
        //     probe behind it rather than an algorithm's invariant.
        let (load_bearing, violations, confirmed) = if self.config.confirm_minimality {
            let mut probe = |subset: &[HunkId]| -> Result<bool> {
                let set: BTreeSet<HunkId> = subset.iter().copied().collect();
                self.probe(&set)
            };
            let outcome = confirm_minimal(&minimal, &mut probe, &mut budget)?;
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
        map.probes = self.probes;

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
