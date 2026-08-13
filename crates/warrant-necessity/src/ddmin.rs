//! Delta debugging.
//!
//! Given a set of changes and a predicate that holds for the whole set, find
//! a subset that still satisfies it and where every remaining element is
//! individually necessary. Zeller and Hildebrandt's `ddmin`, applied to
//! hunks instead of to failing inputs.
//!
//! The algorithm is written here with no knowledge of files, cells or
//! proofs — it takes a slice and a closure — so it can be tested against
//! oracles whose answers are known in advance. That matters: a bug here
//! would not crash anything, it would quietly produce a plausible wrong
//! number.
//!
//! **Monotonicity.** `ddmin` assumes that if a subset satisfies the
//! predicate, so does every superset. Real test suites violate this — flaky
//! tests, order dependence, stateful integration suites. The search does not
//! pretend otherwise: [`confirm_minimal`] re-checks every surviving element
//! individually and reports anything it had to drop, which is a monotonicity
//! violation caught in the act rather than assumed away.

use std::collections::BTreeSet;

/// How many probes a search may spend.
#[derive(Clone, Copy, Debug)]
pub struct ProbeBudget {
    limit: Option<u32>,
    used: u32,
}

impl ProbeBudget {
    /// A budget with a ceiling.
    pub fn limited(limit: u32) -> Self {
        ProbeBudget { limit: Some(limit), used: 0 }
    }

    /// A budget with no ceiling.
    pub fn unlimited() -> Self {
        ProbeBudget { limit: None, used: 0 }
    }

    /// Whether the ceiling has been reached.
    pub fn exhausted(&self) -> bool {
        self.limit.is_some_and(|limit| self.used >= limit)
    }

    /// Probes spent so far.
    pub fn used(&self) -> u32 {
        self.used
    }

    /// Account for one probe.
    pub fn spend(&mut self) {
        self.used = self.used.saturating_add(1);
    }
}

/// Split `items` into `count` chunks as evenly as possible.
///
/// The remainder is spread one element at a time across the leading chunks
/// rather than dumped on the last, which keeps the recursion balanced and is
/// the difference between `O(log n)` and a long tail on awkward sizes.
fn partition<T: Copy>(items: &[T], count: usize) -> Vec<Vec<T>> {
    let count = count.min(items.len()).max(1);
    let base = items.len() / count;
    let remainder = items.len() % count;

    let mut chunks = Vec::with_capacity(count);
    let mut start = 0;
    for i in 0..count {
        let len = base + usize::from(i < remainder);
        chunks.push(items[start..start + len].to_vec());
        start += len;
    }
    chunks
}

/// Find a subset of `universe` that still satisfies `passes`.
///
/// `passes(universe)` is assumed to already be true; the caller establishes
/// that as the satisfaction check, and re-testing it here would waste a probe
/// on every claim.
pub fn ddmin<T, E, F>(universe: &[T], passes: &mut F, budget: &mut ProbeBudget) -> Result<Vec<T>, E>
where
    T: Copy + Ord,
    F: FnMut(&[T]) -> Result<bool, E>,
{
    let mut one_at_a_time =
        |batch: &[Vec<T>]| -> Result<Vec<bool>, E> { batch.iter().map(|s| passes(s)).collect() };
    ddmin_wide(universe, &mut one_at_a_time, budget, 1)
}

/// The same search, evaluating up to `width` candidates per round.
///
/// The answer is identical: within a round the lowest-index candidate that
/// passes is the one taken, which is exactly what a sequential pass with an
/// early exit would have chosen. What changes is *rounds*, and a round is one
/// run of the repository's test suite in wall-clock terms.
///
/// The trade is deliberate. A wide round runs candidates the sequential
/// version would have skipped after an early hit, so it spends **more probes
/// to spend less time**. On a suite that takes a minute, that is the right
/// direction; `width = 1` reproduces the frugal behaviour exactly.
pub fn ddmin_wide<T, E, F>(
    universe: &[T],
    probe_batch: &mut F,
    budget: &mut ProbeBudget,
    width: usize,
) -> Result<Vec<T>, E>
where
    T: Copy + Ord,
    F: FnMut(&[Vec<T>]) -> Result<Vec<bool>, E>,
{
    let width = width.max(1);
    let mut current = universe.to_vec();
    let mut granularity = 2usize;

    while current.len() > 1 && !budget.exhausted() {
        let chunks = partition(&current, granularity);
        let mut progressed = false;

        // Can the whole thing be replaced by one of its parts?
        if let Some(hit) = first_passing(&chunks, probe_batch, budget, width)? {
            current = hit;
            granularity = 2;
            progressed = true;
        }
        if progressed {
            continue;
        }

        // Can any single part be dropped? At granularity two the complements
        // are the chunks, which were just tested, so this starts at three.
        if granularity > 2 {
            let complements: Vec<Vec<T>> = chunks
                .iter()
                .map(|chunk| {
                    let excluded: BTreeSet<T> = chunk.iter().copied().collect();
                    current.iter().copied().filter(|x| !excluded.contains(x)).collect::<Vec<T>>()
                })
                .filter(|complement| !complement.is_empty())
                .collect();

            if let Some(hit) = first_passing(&complements, probe_batch, budget, width)? {
                current = hit;
                granularity = granularity.saturating_sub(1).max(2);
                progressed = true;
            }
        }
        if progressed {
            continue;
        }

        if granularity >= current.len() {
            break;
        }
        granularity = (granularity * 2).min(current.len());
    }

    Ok(current)
}

/// Evaluate candidates `width` at a time and return the first that passes.
///
/// "First" is by position in `candidates`, not by which round finished first,
/// so the result does not depend on scheduling.
fn first_passing<T, E, F>(
    candidates: &[Vec<T>],
    probe_batch: &mut F,
    budget: &mut ProbeBudget,
    width: usize,
) -> Result<Option<Vec<T>>, E>
where
    T: Copy + Ord,
    F: FnMut(&[Vec<T>]) -> Result<Vec<bool>, E>,
{
    for round in candidates.chunks(width) {
        if budget.exhausted() {
            return Ok(None);
        }
        for _ in round {
            budget.spend();
        }
        let results = probe_batch(round)?;
        if let Some(index) = results.iter().position(|passed| *passed) {
            return Ok(Some(round[index].clone()));
        }
    }
    Ok(None)
}

/// What a minimality check found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Minimality<T> {
    /// The surviving subset, every element of which is individually necessary.
    pub subset: Vec<T>,
    /// Elements dropped during confirmation.
    ///
    /// Under a monotone predicate this is empty, because `ddmin` already
    /// returned a 1-minimal set. Anything here is direct evidence that the
    /// proof answered differently on states the search had already explored.
    pub dropped: Vec<T>,
    /// Whether confirmation completed rather than running out of budget.
    pub complete: bool,
}

/// Verify that every element of `subset` is individually necessary, dropping
/// any that are not.
///
/// This is the step that lets the map say, of each load-bearing hunk,
/// *reverting this one makes the proof fail* — and mean it literally, with a
/// probe behind it, rather than as a consequence of an algorithm's invariant.
pub fn confirm_minimal<T, E, F>(
    subset: &[T],
    passes: &mut F,
    budget: &mut ProbeBudget,
) -> Result<Minimality<T>, E>
where
    T: Copy + Ord,
    F: FnMut(&[T]) -> Result<bool, E>,
{
    let mut one_at_a_time =
        |batch: &[Vec<T>]| -> Result<Vec<bool>, E> { batch.iter().map(|s| passes(s)).collect() };
    confirm_minimal_wide(subset, &mut one_at_a_time, budget, 1)
}

/// The same confirmation, evaluating up to `width` candidates per round.
///
/// The usual case is that nothing needs dropping — `ddmin` already returned a
/// 1-minimal set — and the sequential version still pays one probe per
/// element to find that out. A wide round asks the whole question at once, so
/// the common case costs **one round instead of |S|**.
pub fn confirm_minimal_wide<T, E, F>(
    subset: &[T],
    probe_batch: &mut F,
    budget: &mut ProbeBudget,
    width: usize,
) -> Result<Minimality<T>, E>
where
    T: Copy + Ord,
    F: FnMut(&[Vec<T>]) -> Result<Vec<bool>, E>,
{
    let width = width.max(1);
    let mut current = subset.to_vec();
    let mut dropped = Vec::new();

    loop {
        if current.is_empty() {
            return Ok(Minimality { subset: current, dropped, complete: true });
        }
        if budget.exhausted() {
            return Ok(Minimality { subset: current, dropped, complete: false });
        }

        // Every element, left out in turn.
        let candidates: Vec<Vec<T>> = (0..current.len())
            .map(|index| {
                let mut without = current.clone();
                without.remove(index);
                without
            })
            .collect();

        let mut survivor = None;
        for (offset, round) in candidates.chunks(width).enumerate() {
            if budget.exhausted() {
                return Ok(Minimality { subset: current, dropped, complete: false });
            }
            for _ in round {
                budget.spend();
            }
            let results = probe_batch(round)?;
            if let Some(index) = results.iter().position(|passed| *passed) {
                survivor = Some(offset * width + index);
                break;
            }
        }

        match survivor {
            // Something was not load-bearing after all. Drop the
            // lowest-indexed one and ask again, because removing it can change
            // the answer for the rest.
            Some(index) => {
                dropped.push(current.remove(index));
            }
            // Nothing can be removed: every element is individually necessary.
            None => return Ok(Minimality { subset: current, dropped, complete: true }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::convert::Infallible;

    /// An oracle that passes exactly when the subset contains every element
    /// of `required`. The 1-minimal answer is `required` itself.
    fn requires(required: &[u32]) -> impl FnMut(&[u32]) -> Result<bool, Infallible> + '_ {
        move |subset: &[u32]| Ok(required.iter().all(|r| subset.contains(r)))
    }

    fn universe(n: u32) -> Vec<u32> {
        (0..n).collect()
    }

    #[test]
    fn partitioning_is_balanced_and_lossless() {
        for len in 0..40usize {
            for count in 1..8usize {
                let items: Vec<usize> = (0..len).collect();
                let chunks = partition(&items, count);
                let flattened: Vec<usize> = chunks.iter().flatten().copied().collect();
                assert_eq!(flattened, items, "partitioning lost or reordered elements");

                if len > 0 {
                    let sizes: Vec<usize> = chunks.iter().map(Vec::len).collect();
                    let smallest = sizes.iter().min().unwrap();
                    let largest = sizes.iter().max().unwrap();
                    assert!(largest - smallest <= 1, "chunk sizes {sizes:?} are not balanced");
                }
            }
        }
    }

    #[test]
    fn a_single_necessary_element_is_found_among_many() {
        let items = universe(64);
        let mut budget = ProbeBudget::unlimited();
        let found = ddmin(&items, &mut requires(&[41]), &mut budget).unwrap();
        assert_eq!(found, [41]);
    }

    #[test]
    fn the_search_is_logarithmic_not_linear() {
        let items = universe(1024);
        let mut budget = ProbeBudget::unlimited();
        ddmin(&items, &mut requires(&[700]), &mut budget).unwrap();
        assert!(
            budget.used() < 60,
            "binary partitioning should not cost {} probes over 1024 hunks",
            budget.used()
        );
    }

    #[test]
    fn several_necessary_elements_are_all_retained() {
        let items = universe(32);
        let required = [3u32, 17, 29];
        let mut budget = ProbeBudget::unlimited();
        let found = ddmin(&items, &mut requires(&required), &mut budget).unwrap();
        for r in required {
            assert!(found.contains(&r), "dropped a necessary element: {r}");
        }
    }

    #[test]
    fn when_nothing_is_necessary_the_result_is_empty_after_confirmation() {
        let items = universe(16);
        let mut budget = ProbeBudget::unlimited();
        let mut always = |_: &[u32]| Ok::<_, Infallible>(true);

        let found = ddmin(&items, &mut always, &mut budget).unwrap();
        let confirmed = confirm_minimal(&found, &mut always, &mut budget).unwrap();
        assert!(confirmed.subset.is_empty(), "a vacuous proof must prove nothing");
    }

    #[test]
    fn when_everything_is_necessary_nothing_is_dropped() {
        let items = universe(12);
        let required: Vec<u32> = items.clone();
        let mut budget = ProbeBudget::unlimited();

        let found = ddmin(&items, &mut requires(&required), &mut budget).unwrap();
        let confirmed = confirm_minimal(&found, &mut requires(&required), &mut budget).unwrap();
        assert_eq!(confirmed.subset, items);
        assert!(confirmed.dropped.is_empty());
        assert!(confirmed.complete);
    }

    #[test]
    fn confirmation_yields_a_one_minimal_set() {
        let items = universe(40);
        let required = [5u32, 6, 7, 30];
        let mut budget = ProbeBudget::unlimited();

        let found = ddmin(&items, &mut requires(&required), &mut budget).unwrap();
        let confirmed = confirm_minimal(&found, &mut requires(&required), &mut budget).unwrap();

        let mut subset = confirmed.subset.clone();
        subset.sort_unstable();
        assert_eq!(subset, required);

        // Every element is individually necessary, by definition of 1-minimal.
        for element in &confirmed.subset {
            let without: Vec<u32> =
                confirmed.subset.iter().copied().filter(|x| x != element).collect();
            assert!(!requires(&required)(&without).unwrap());
        }
    }

    /// Redundancy is where delta debugging has to make a choice: either of
    /// two elements suffices, so exactly one survives. The number stays
    /// honest — one hunk really is enough — and the confirmation pass proves
    /// the survivor is necessary *relative to what else remains*.
    #[test]
    fn with_redundant_alternatives_exactly_one_survives() {
        let items = universe(8);
        let mut oracle =
            |subset: &[u32]| Ok::<_, Infallible>(subset.contains(&2) || subset.contains(&5));
        let mut budget = ProbeBudget::unlimited();

        let found = ddmin(&items, &mut oracle, &mut budget).unwrap();
        let confirmed = confirm_minimal(&found, &mut oracle, &mut budget).unwrap();
        assert_eq!(confirmed.subset.len(), 1);
        assert!(confirmed.subset[0] == 2 || confirmed.subset[0] == 5);
    }

    #[test]
    fn a_budget_stops_the_search_rather_than_letting_it_run_away() {
        let items = universe(1024);
        let mut budget = ProbeBudget::limited(5);
        let found = ddmin(&items, &mut requires(&[999]), &mut budget).unwrap();

        assert!(budget.exhausted());
        assert!(budget.used() <= 5);
        // The answer is coarser, never wrong: it still contains what is needed.
        assert!(found.contains(&999));
    }

    #[test]
    fn confirmation_reports_incompleteness_instead_of_guessing() {
        let items = universe(10);
        let mut budget = ProbeBudget::limited(3);
        let confirmed =
            confirm_minimal(&items, &mut requires(&items.clone()), &mut budget).unwrap();
        assert!(!confirmed.complete);
    }

    #[test]
    fn a_probe_failure_propagates_rather_than_being_read_as_false() {
        let items = universe(8);
        let mut budget = ProbeBudget::unlimited();
        let mut failing = |_: &[u32]| Err::<bool, &str>("the suite could not be run");
        assert!(ddmin(&items, &mut failing, &mut budget).is_err());
    }

    #[test]
    fn an_empty_universe_is_handled() {
        let mut budget = ProbeBudget::unlimited();
        let mut always = |_: &[u32]| Ok::<_, Infallible>(true);
        assert!(ddmin(&[], &mut always, &mut budget).unwrap().is_empty());
        assert_eq!(budget.used(), 0);
    }

    /// Turn a single-candidate oracle into a batch one, so the wide and narrow
    /// paths can be compared against the same requirement.
    fn batched(required: Vec<u32>) -> impl FnMut(&[Vec<u32>]) -> Result<Vec<bool>, Infallible> {
        move |batch: &[Vec<u32>]| {
            Ok(batch.iter().map(|subset| required.iter().all(|r| subset.contains(r))).collect())
        }
    }

    /// The whole basis for running probes concurrently: a wider round must not
    /// change the answer, only how long it takes to reach it.
    #[test]
    fn width_changes_the_schedule_and_never_the_answer() {
        let items = universe(40);
        for required in [vec![7u32], vec![3, 9, 30], vec![0, 39], Vec::new()] {
            let mut narrow_budget = ProbeBudget::unlimited();
            let narrow =
                ddmin_wide(&items, &mut batched(required.clone()), &mut narrow_budget, 1).unwrap();
            let narrow = confirm_minimal_wide(
                &narrow,
                &mut batched(required.clone()),
                &mut narrow_budget,
                1,
            )
            .unwrap();

            for width in [2usize, 4, 8, 64] {
                let mut budget = ProbeBudget::unlimited();
                let found =
                    ddmin_wide(&items, &mut batched(required.clone()), &mut budget, width).unwrap();
                let confirmed = confirm_minimal_wide(
                    &found,
                    &mut batched(required.clone()),
                    &mut budget,
                    width,
                )
                .unwrap();

                let mut a = narrow.subset.clone();
                let mut b = confirmed.subset.clone();
                a.sort_unstable();
                b.sort_unstable();
                assert_eq!(a, b, "width {width} changed the answer for {required:?}");
            }
        }
    }

    /// The trade being made, stated as a test so it cannot drift: a wide round
    /// spends more probes and fewer rounds. On a suite that takes a minute,
    /// rounds are what the person is waiting for.
    #[test]
    fn a_wide_confirmation_costs_one_round_instead_of_one_per_element() {
        let required: Vec<u32> = (0..12).collect();

        let mut rounds = 0;
        let mut counting = |batch: &[Vec<u32>]| -> Result<Vec<bool>, Infallible> {
            rounds += 1;
            Ok(batch.iter().map(|s| required.iter().all(|r| s.contains(r))).collect())
        };
        let mut budget = ProbeBudget::unlimited();
        let wide = confirm_minimal_wide(&required, &mut counting, &mut budget, 16).unwrap();

        assert!(wide.complete);
        assert_eq!(wide.subset.len(), 12, "nothing should be dropped");
        assert_eq!(rounds, 1, "an already-minimal set should take a single round");
        assert_eq!(budget.used(), 12, "and still account for every probe it ran");
    }

    #[test]
    fn a_budget_still_bounds_a_wide_search() {
        let items = universe(256);
        let mut budget = ProbeBudget::limited(6);
        let found = ddmin_wide(&items, &mut batched(vec![200]), &mut budget, 8).unwrap();
        assert!(budget.exhausted());
        assert!(found.contains(&200), "a bounded search is coarser, never wrong");
    }

    proptest::proptest! {
        /// Whatever the required set, the search must find it and the
        /// confirmation must reduce to exactly it.
        #[test]
        fn the_minimal_set_is_recovered_for_any_requirement(
            required in proptest::collection::btree_set(0u32..24, 0..6)
        ) {
            let items = universe(24);
            let required: Vec<u32> = required.into_iter().collect();
            let mut budget = ProbeBudget::unlimited();

            let found = ddmin(&items, &mut requires(&required), &mut budget).unwrap();
            let confirmed = confirm_minimal(&found, &mut requires(&required), &mut budget).unwrap();

            let mut subset = confirmed.subset;
            subset.sort_unstable();
            proptest::prop_assert_eq!(subset, required);
        }

        /// The same property, at every width. Scheduling must not be able to
        /// change what a map reports.
        #[test]
        fn any_width_recovers_the_same_minimal_set(
            required in proptest::collection::btree_set(0u32..24, 0..6),
            width in 1usize..12,
        ) {
            let items = universe(24);
            let required: Vec<u32> = required.into_iter().collect();
            let mut budget = ProbeBudget::unlimited();

            let found = ddmin_wide(&items, &mut batched(required.clone()), &mut budget, width)
                .unwrap();
            let confirmed =
                confirm_minimal_wide(&found, &mut batched(required.clone()), &mut budget, width)
                    .unwrap();

            let mut subset = confirmed.subset;
            subset.sort_unstable();
            proptest::prop_assert_eq!(subset, required);
        }
    }
}
