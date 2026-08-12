//! The overlay diff — the evidence.
//!
//! This is computed by comparing two supervisor-taken snapshots. The agent is
//! never asked what it changed, and nothing it says is an input here. That is
//! the whole reason the diff is admissible.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use warrant_core::{Hash, HunkId};

use crate::error::{DiffError, Result};
use crate::hunk::{Hunk, HunkKind, decompose_modified, file_added, file_deleted};
use crate::lines::{join_lines, split_lines};
use crate::snapshot::{FileMeta, Snapshot};
use crate::store::ContentStore;

/// What happened to one file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    /// Did not exist before.
    Added,
    /// No longer exists.
    Deleted,
    /// Content changed.
    Modified,
    /// Only the executable bit changed. Not decomposable into hunks, and
    /// invisible on Windows, which has no such bit.
    ModeChanged,
}

/// One file's place in the diff.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDelta {
    /// Repo-relative path.
    pub path: String,
    /// What happened.
    pub change: ChangeKind,
    /// The hunks belonging to this file, in pre-image order.
    pub hunk_ids: Vec<HunkId>,
}

/// The complete difference between two observed states.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverlayDiff {
    /// Tree address before the agent ran.
    pub pre_root: Hash,
    /// Tree address after.
    pub post_root: Hash,
    /// Per-file summary, sorted by path.
    pub files: Vec<FileDelta>,
    /// Every hunk, sorted by path then pre-image position.
    pub hunks: Vec<Hunk>,
}

impl OverlayDiff {
    /// Compute the diff between two snapshots.
    pub fn between(
        pre: &Snapshot,
        post: &Snapshot,
        store: &dyn ContentStore,
    ) -> Result<OverlayDiff> {
        let mut files = Vec::new();
        let mut hunks = Vec::new();

        let paths: BTreeSet<&String> = pre.files.keys().chain(post.files.keys()).collect();

        for path in paths {
            match (pre.files.get(path), post.files.get(path)) {
                (None, Some(_)) => {
                    let content = read(post, path, store)?;
                    let hunk = file_added(path, &content);
                    files.push(FileDelta {
                        path: path.clone(),
                        change: ChangeKind::Added,
                        hunk_ids: vec![hunk.id],
                    });
                    hunks.push(hunk);
                }
                (Some(_), None) => {
                    let content = read(pre, path, store)?;
                    let hunk = file_deleted(path, &content);
                    files.push(FileDelta {
                        path: path.clone(),
                        change: ChangeKind::Deleted,
                        hunk_ids: vec![hunk.id],
                    });
                    hunks.push(hunk);
                }
                (Some(before), Some(after)) => {
                    if before.content == after.content {
                        if before.executable != after.executable {
                            files.push(FileDelta {
                                path: path.clone(),
                                change: ChangeKind::ModeChanged,
                                hunk_ids: Vec::new(),
                            });
                        }
                        continue;
                    }
                    let pre_bytes = read(pre, path, store)?;
                    let post_bytes = read(post, path, store)?;
                    let file_hunks = decompose_modified(path, &pre_bytes, &post_bytes);
                    files.push(FileDelta {
                        path: path.clone(),
                        change: ChangeKind::Modified,
                        hunk_ids: file_hunks.iter().map(|h| h.id).collect(),
                    });
                    hunks.extend(file_hunks);
                }
                (None, None) => unreachable!("path came from one of the two maps"),
            }
        }

        Ok(OverlayDiff { pre_root: pre.root_hash(), post_root: post.root_hash(), files, hunks })
    }

    /// Whether anything changed.
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Number of hunks.
    pub fn hunk_count(&self) -> usize {
        self.hunks.len()
    }

    /// Total changed lines across every hunk — the denominator of coverage.
    pub fn changed_lines(&self) -> u64 {
        self.hunks.iter().map(Hunk::changed_lines).sum()
    }

    /// Every hunk id, in order.
    pub fn hunk_ids(&self) -> Vec<HunkId> {
        self.hunks.iter().map(|h| h.id).collect()
    }

    /// Look one up.
    pub fn hunk(&self, id: HunkId) -> Option<&Hunk> {
        self.hunks.iter().find(|h| h.id == id)
    }

    /// The hunks belonging to one file.
    pub fn hunks_for<'a>(&'a self, path: &'a str) -> impl Iterator<Item = &'a Hunk> {
        self.hunks.iter().filter(move |h| h.path == path)
    }

    /// Changed lines within a subset.
    pub fn changed_lines_in(&self, selected: &BTreeSet<HunkId>) -> u64 {
        self.hunks.iter().filter(|h| selected.contains(&h.id)).map(Hunk::changed_lines).sum()
    }
}

/// Reconstruct the tree that results from applying exactly `selected` to `pre`.
///
/// Two properties anchor everything downstream, and both are tested:
///
/// - applying the empty set reproduces `pre` exactly;
/// - applying every hunk reproduces `post` exactly.
///
/// Without the second, a "revert this hunk" probe would be measuring a tree
/// nobody ever had, which is the mistake static trajectory replay makes.
pub fn apply_subset(
    pre: &Snapshot,
    post: &Snapshot,
    diff: &OverlayDiff,
    selected: &BTreeSet<HunkId>,
    store: &dyn ContentStore,
) -> Result<Snapshot> {
    let mut files = pre.files.clone();

    // Group the selection by file, preserving pre-image order.
    let mut by_path: BTreeMap<&str, Vec<&Hunk>> = BTreeMap::new();
    for hunk in &diff.hunks {
        if selected.contains(&hunk.id) {
            by_path.entry(hunk.path.as_str()).or_default().push(hunk);
        }
    }

    for (path, chosen) in by_path {
        let total_for_file = diff.hunks.iter().filter(|h| h.path == path).count();
        let whole_file_selected = chosen.len() == total_for_file;

        if chosen.iter().any(|h| h.kind == HunkKind::FileDeleted) {
            files.remove(path);
            continue;
        }

        if let Some(replacement) =
            chosen.iter().find(|h| matches!(h.kind, HunkKind::FileAdded | HunkKind::BinaryReplace))
        {
            let bytes = replacement.post_bytes();
            let content = put(store, &bytes, path)?;
            let executable = post.files.get(path).is_some_and(|m| m.executable);
            files.insert(
                path.to_owned(),
                FileMeta { content, size: bytes.len() as u64, executable },
            );
            continue;
        }

        let base = pre.content_of(path, store)?.ok_or_else(|| DiffError::ContentUnavailable {
            hash: Hash::ZERO,
            path: path.to_owned(),
            reason: "a replace hunk was cut from a file that is not in the pre-image".into(),
        })?;
        let bytes = splice(path, &base, &chosen)?;
        let content = put(store, &bytes, path)?;

        // Mode follows the post-state only when the file is being taken there
        // whole. A partially applied file keeps the mode it started with, so
        // that applying nothing is exactly `pre` and applying everything is
        // exactly `post`.
        let executable = if whole_file_selected {
            post.files.get(path).is_some_and(|m| m.executable)
        } else {
            pre.files.get(path).is_some_and(|m| m.executable)
        };
        files.insert(path.to_owned(), FileMeta { content, size: bytes.len() as u64, executable });
    }

    // Mode-only changes carry no hunk, so they ride along whenever the whole
    // diff is applied and are otherwise left alone.
    if selected.len() == diff.hunks.len() {
        for delta in &diff.files {
            if delta.change == ChangeKind::ModeChanged
                && let (Some(meta), Some(after)) =
                    (files.get_mut(&delta.path), post.files.get(&delta.path))
            {
                meta.executable = after.executable;
            }
        }
    }

    Ok(Snapshot { files })
}

/// Replace the selected line ranges in `base`, working from the end so that
/// earlier indices stay valid.
fn splice(path: &str, base: &[u8], chosen: &[&Hunk]) -> Result<Vec<u8>> {
    let mut lines = split_lines(base);

    let mut ordered: Vec<&&Hunk> = chosen.iter().collect();
    ordered.sort_by_key(|h| h.pre_start);

    for pair in ordered.windows(2) {
        if pair[0].pre_end() > pair[1].pre_start {
            return Err(DiffError::OverlappingHunks {
                path: path.to_owned(),
                a: (pair[0].pre_start, pair[0].pre_end()),
                b: (pair[1].pre_start, pair[1].pre_end()),
            });
        }
    }

    for hunk in ordered.into_iter().rev() {
        if hunk.pre_end() > lines.len() {
            return Err(DiffError::HunkOutOfRange {
                path: path.to_owned(),
                start: hunk.pre_start,
                end: hunk.pre_end(),
                available: lines.len(),
            });
        }
        lines.splice(hunk.pre_start..hunk.pre_end(), hunk.post_lines.iter().cloned());
    }

    Ok(join_lines(&lines))
}

fn read(snapshot: &Snapshot, path: &str, store: &dyn ContentStore) -> Result<Vec<u8>> {
    snapshot.content_of(path, store)?.ok_or_else(|| DiffError::ContentUnavailable {
        hash: Hash::ZERO,
        path: path.to_owned(),
        reason: "not present in the snapshot it was read from".into(),
    })
}

fn put(store: &dyn ContentStore, bytes: &[u8], path: &str) -> Result<Hash> {
    store.put(bytes).map_err(|reason| DiffError::ContentUnavailable {
        hash: Hash::of(bytes),
        path: path.to_owned(),
        reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MemoryStore;

    fn snap(store: &MemoryStore, entries: &[(&str, &[u8])]) -> Snapshot {
        Snapshot::from_contents(entries.iter().copied(), store).unwrap()
    }

    fn all(diff: &OverlayDiff) -> BTreeSet<HunkId> {
        diff.hunk_ids().into_iter().collect()
    }

    #[test]
    fn identical_trees_produce_an_empty_diff() {
        let store = MemoryStore::new();
        let a = snap(&store, &[("a.txt", b"same\n")]);
        let diff = OverlayDiff::between(&a, &a, &store).unwrap();
        assert!(diff.is_empty());
        assert_eq!(diff.changed_lines(), 0);
    }

    #[test]
    fn additions_deletions_and_modifications_are_classified() {
        let store = MemoryStore::new();
        let pre =
            snap(&store, &[("keep.txt", b"k\n"), ("gone.txt", b"g\n"), ("edit.txt", b"a\nb\n")]);
        let post =
            snap(&store, &[("keep.txt", b"k\n"), ("edit.txt", b"a\nB\n"), ("new.txt", b"n\n")]);

        let diff = OverlayDiff::between(&pre, &post, &store).unwrap();
        let kinds: Vec<(&str, ChangeKind)> =
            diff.files.iter().map(|f| (f.path.as_str(), f.change)).collect();
        assert_eq!(
            kinds,
            [
                ("edit.txt", ChangeKind::Modified),
                ("gone.txt", ChangeKind::Deleted),
                ("new.txt", ChangeKind::Added),
            ]
        );
    }

    #[test]
    fn applying_nothing_reproduces_the_pre_state_exactly() {
        let store = MemoryStore::new();
        let pre = snap(&store, &[("a.txt", b"one\ntwo\n"), ("b.txt", b"x\n")]);
        let post = snap(&store, &[("a.txt", b"ONE\ntwo\n"), ("c.txt", b"new\n")]);
        let diff = OverlayDiff::between(&pre, &post, &store).unwrap();

        let result = apply_subset(&pre, &post, &diff, &BTreeSet::new(), &store).unwrap();
        assert_eq!(result, pre);
        assert_eq!(result.root_hash(), pre.root_hash());
    }

    #[test]
    fn applying_everything_reproduces_the_post_state_exactly() {
        let store = MemoryStore::new();
        let pre = snap(
            &store,
            &[("a.txt", b"one\ntwo\nthree\n"), ("gone.txt", b"g\n"), ("bin.dat", &[0u8, 1, 2])],
        );
        let post = snap(
            &store,
            &[
                ("a.txt", b"ONE\ntwo\nTHREE\n"),
                ("added.txt", b"brand new\nlines\n"),
                ("bin.dat", &[0u8, 9, 9, 9]),
            ],
        );
        let diff = OverlayDiff::between(&pre, &post, &store).unwrap();

        let result = apply_subset(&pre, &post, &diff, &all(&diff), &store).unwrap();
        assert_eq!(
            result.root_hash(),
            post.root_hash(),
            "full application must land exactly on post"
        );
        assert_eq!(result, post);
    }

    #[test]
    fn a_single_hunk_can_be_reverted_while_its_neighbours_stay() {
        let store = MemoryStore::new();
        let pre = snap(&store, &[("a.txt", b"1\n2\n3\n4\n5\n6\n7\n")]);
        let post = snap(&store, &[("a.txt", b"ONE\n2\n3\n4\n5\n6\nSEVEN\n")]);
        let diff = OverlayDiff::between(&pre, &post, &store).unwrap();
        assert_eq!(diff.hunk_count(), 2);

        // Keep only the second hunk.
        let mut selected = BTreeSet::new();
        selected.insert(diff.hunks[1].id);
        let result = apply_subset(&pre, &post, &diff, &selected, &store).unwrap();

        assert_eq!(
            result.content_of("a.txt", &store).unwrap().unwrap(),
            b"1\n2\n3\n4\n5\n6\nSEVEN\n"
        );
    }

    #[test]
    fn reverting_a_deletion_restores_the_file() {
        let store = MemoryStore::new();
        let pre = snap(&store, &[("doomed.txt", b"content\n")]);
        let post = snap(&store, &[]);
        let diff = OverlayDiff::between(&pre, &post, &store).unwrap();

        let kept = apply_subset(&pre, &post, &diff, &BTreeSet::new(), &store).unwrap();
        assert!(kept.files.contains_key("doomed.txt"));

        let removed = apply_subset(&pre, &post, &diff, &all(&diff), &store).unwrap();
        assert!(!removed.files.contains_key("doomed.txt"));
    }

    #[test]
    fn byte_exactness_survives_crlf_and_a_missing_final_newline() {
        let store = MemoryStore::new();
        let pre = snap(&store, &[("a.txt", b"one\r\ntwo\r\nthree")]);
        let post = snap(&store, &[("a.txt", b"one\r\nTWO\r\nthree")]);
        let diff = OverlayDiff::between(&pre, &post, &store).unwrap();

        let full = apply_subset(&pre, &post, &diff, &all(&diff), &store).unwrap();
        assert_eq!(full.content_of("a.txt", &store).unwrap().unwrap(), b"one\r\nTWO\r\nthree");

        let none = apply_subset(&pre, &post, &diff, &BTreeSet::new(), &store).unwrap();
        assert_eq!(none.content_of("a.txt", &store).unwrap().unwrap(), b"one\r\ntwo\r\nthree");
    }

    #[test]
    fn coverage_arithmetic_adds_up_over_subsets() {
        let store = MemoryStore::new();
        let pre = snap(&store, &[("a.txt", b"1\n2\n3\n4\n5\n6\n7\n")]);
        let post = snap(&store, &[("a.txt", b"ONE\n2\n3\n4\n5\n6\nSEVEN\n")]);
        let diff = OverlayDiff::between(&pre, &post, &store).unwrap();

        let total: u64 = diff.changed_lines();
        let each: u64 = diff.hunks.iter().map(|h| h.changed_lines()).sum();
        assert_eq!(total, each);
        assert_eq!(diff.changed_lines_in(&all(&diff)), total);
        assert_eq!(diff.changed_lines_in(&BTreeSet::new()), 0);
    }

    #[test]
    fn a_mode_only_change_is_reported_without_a_hunk() {
        let store = MemoryStore::new();
        let mut pre = snap(&store, &[("run.sh", b"#!/bin/sh\n")]);
        let mut post = pre.clone();
        pre.files.get_mut("run.sh").unwrap().executable = false;
        post.files.get_mut("run.sh").unwrap().executable = true;

        let diff = OverlayDiff::between(&pre, &post, &store).unwrap();
        assert_eq!(diff.files.len(), 1);
        assert_eq!(diff.files[0].change, ChangeKind::ModeChanged);
        assert_eq!(diff.hunk_count(), 0);
    }
}
