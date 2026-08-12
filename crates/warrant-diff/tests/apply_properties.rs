//! The two properties every probe result depends on.
//!
//! If `apply(∅)` is not exactly the pre-state, a "revert everything" probe
//! measures a tree that never existed. If `apply(all)` is not exactly the
//! post-state, the search is exploring a neighbourhood of the wrong point.
//! Fixed examples cover the cases that were thought of; these cover the ones
//! that were not.

use std::collections::BTreeSet;

use proptest::prelude::*;
use warrant_core::HunkId;
use warrant_diff::{MemoryStore, OverlayDiff, Snapshot, apply_subset};

/// Text built from a deliberately small alphabet, so the generator produces
/// plenty of *near*-matches — the input class where diff algorithms and
/// off-by-one errors actually meet.
fn text() -> impl Strategy<Value = String> {
    (prop::collection::vec("[ab]{1,3}", 0..12), any::<bool>()).prop_map(|(lines, trailing)| {
        let mut s = lines.join("\n");
        if trailing && !s.is_empty() {
            s.push('\n');
        }
        s
    })
}

/// A file tree, as path/content pairs.
type Tree = Vec<(String, String)>;

/// A pair of file trees over the same three candidate paths, where a file may
/// be absent from either side — so additions and deletions are generated too.
fn tree_pair() -> impl Strategy<Value = (Tree, Tree)> {
    let slot = || prop::option::of(text());
    (slot(), slot(), slot(), slot(), slot(), slot()).prop_map(|(a1, b1, c1, a2, b2, c2)| {
        let build = |a: Option<String>, b: Option<String>, c: Option<String>| {
            let mut out = Vec::new();
            if let Some(v) = a {
                out.push(("src/a.txt".to_string(), v));
            }
            if let Some(v) = b {
                out.push(("src/nested/b.txt".to_string(), v));
            }
            if let Some(v) = c {
                out.push(("tests/c.txt".to_string(), v));
            }
            out
        };
        (build(a1, b1, c1), build(a2, b2, c2))
    })
}

fn snapshot(store: &MemoryStore, files: &Tree) -> Snapshot {
    Snapshot::from_contents(files.iter().map(|(p, c)| (p.as_str(), c.as_bytes())), store).unwrap()
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 400, ..ProptestConfig::default() })]

    #[test]
    fn reverting_everything_lands_exactly_on_the_pre_state((before, after) in tree_pair()) {
        let store = MemoryStore::new();
        let pre = snapshot(&store, &before);
        let post = snapshot(&store, &after);
        let diff = OverlayDiff::between(&pre, &post, &store).unwrap();

        let result = apply_subset(&pre, &post, &diff, &BTreeSet::new(), &store).unwrap();
        prop_assert_eq!(result.root_hash(), pre.root_hash());
    }

    #[test]
    fn applying_everything_lands_exactly_on_the_post_state((before, after) in tree_pair()) {
        let store = MemoryStore::new();
        let pre = snapshot(&store, &before);
        let post = snapshot(&store, &after);
        let diff = OverlayDiff::between(&pre, &post, &store).unwrap();

        let all: BTreeSet<HunkId> = diff.hunk_ids().into_iter().collect();
        let result = apply_subset(&pre, &post, &diff, &all, &store).unwrap();
        prop_assert_eq!(result.root_hash(), post.root_hash());
    }

    /// Every subset must reconstruct *something* — a probe that errors out is
    /// a probe that cannot be scored, and a search full of holes is not a
    /// measurement.
    #[test]
    fn every_subset_reconstructs_a_tree(
        (before, after) in tree_pair(),
        mask in prop::collection::vec(any::<bool>(), 0..40),
    ) {
        let store = MemoryStore::new();
        let pre = snapshot(&store, &before);
        let post = snapshot(&store, &after);
        let diff = OverlayDiff::between(&pre, &post, &store).unwrap();

        let selected: BTreeSet<HunkId> = diff
            .hunks
            .iter()
            .enumerate()
            .filter(|(i, _)| mask.get(*i).copied().unwrap_or(false))
            .map(|(_, h)| h.id)
            .collect();

        let first = apply_subset(&pre, &post, &diff, &selected, &store).unwrap();
        let second = apply_subset(&pre, &post, &diff, &selected, &store).unwrap();
        prop_assert_eq!(first.root_hash(), second.root_hash(), "application must be deterministic");

        // Coverage arithmetic must stay inside its bounds for any subset.
        let part = diff.changed_lines_in(&selected);
        prop_assert!(part <= diff.changed_lines());
    }

    /// Hunk ids are content-derived, so the same pair of trees must always
    /// decompose into the same hunks. Probe caches key on these.
    #[test]
    fn decomposition_is_reproducible((before, after) in tree_pair()) {
        let store = MemoryStore::new();
        let pre = snapshot(&store, &before);
        let post = snapshot(&store, &after);

        let a = OverlayDiff::between(&pre, &post, &store).unwrap();
        let b = OverlayDiff::between(&pre, &post, &store).unwrap();
        prop_assert_eq!(a.hunk_ids(), b.hunk_ids());
    }

    /// Hunks within a file address disjoint regions of the same pre-image.
    /// If they ever overlapped, subset application would be order-dependent
    /// and the whole search would be meaningless.
    #[test]
    fn hunks_within_a_file_never_overlap((before, after) in tree_pair()) {
        let store = MemoryStore::new();
        let pre = snapshot(&store, &before);
        let post = snapshot(&store, &after);
        let diff = OverlayDiff::between(&pre, &post, &store).unwrap();

        for delta in &diff.files {
            let hunks: Vec<_> = diff.hunks_for(&delta.path).collect();
            for pair in hunks.windows(2) {
                prop_assert!(
                    pair[0].pre_end() <= pair[1].pre_start,
                    "overlapping hunks in {}", delta.path
                );
            }
        }
    }
}
