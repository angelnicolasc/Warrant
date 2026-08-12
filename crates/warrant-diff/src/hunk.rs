//! Hunks: the unit the necessity search reverts.
//!
//! A hunk is a contiguous replacement of lines in a *pre-image*, and every
//! hunk in a file addresses that same pre-image. Because subsets are always
//! applied to the pristine pre-state rather than to each other's output,
//! application is exact arithmetic on line indices — there is no context
//! matching, no fuzz factor, and no way for a subset to apply "mostly".
//!
//! That is the difference between this and running `patch`, and it is what
//! makes a probe result mean something.

use serde::{Deserialize, Serialize};
use similar::{Algorithm, DiffOp, capture_diff_slices, group_diff_ops};
use warrant_core::{Hash, HunkId};

use crate::lines::{is_binary, join_lines, split_lines};

/// What kind of change a hunk represents.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HunkKind {
    /// A contiguous line range replaced within an existing text file.
    Replace,
    /// A file that did not exist before. Always one hunk: reverting half of a
    /// new file yields something nobody wrote.
    FileAdded,
    /// A file that no longer exists.
    FileDeleted,
    /// A binary file replaced wholesale. Not decomposed into lines.
    BinaryReplace,
}

/// One revertible change.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hunk {
    /// Content-derived identity, stable across runs.
    pub id: HunkId,
    /// Repo-relative, `/`-separated path.
    pub path: String,
    /// What kind of change this is.
    pub kind: HunkKind,
    /// First pre-image line this hunk replaces, zero-based.
    pub pre_start: usize,
    /// How many pre-image lines it replaces.
    pub pre_len: usize,
    /// The lines being removed, each with its terminator.
    pub pre_lines: Vec<Vec<u8>>,
    /// The lines being inserted, each with its terminator.
    pub post_lines: Vec<Vec<u8>>,
}

impl Hunk {
    fn new(
        path: &str,
        kind: HunkKind,
        pre_start: usize,
        pre_lines: Vec<Vec<u8>>,
        post_lines: Vec<Vec<u8>>,
    ) -> Self {
        let pre_len = pre_lines.len();
        let pre_digest = Hash::of(&join_lines(&pre_lines));
        let post_digest = Hash::of(&join_lines(&post_lines));
        let id = HunkId::derive(&[
            path.as_bytes(),
            kind_tag(kind).as_bytes(),
            &(pre_start as u64).to_le_bytes(),
            &(pre_len as u64).to_le_bytes(),
            pre_digest.as_bytes(),
            post_digest.as_bytes(),
        ]);
        Hunk { id, path: path.to_owned(), kind, pre_start, pre_len, pre_lines, post_lines }
    }

    /// One past the last pre-image line this hunk covers.
    pub fn pre_end(&self) -> usize {
        self.pre_start + self.pre_len
    }

    /// How many lines this hunk contributes to the diff — removals plus
    /// insertions, which is what a reviewer counts when they look at it.
    ///
    /// A binary replacement counts as one, matching how a diff renders it.
    pub fn changed_lines(&self) -> u64 {
        match self.kind {
            HunkKind::BinaryReplace => 1,
            _ => (self.pre_lines.len() + self.post_lines.len()) as u64,
        }
    }

    /// Lines added.
    pub fn added_lines(&self) -> usize {
        self.post_lines.len()
    }

    /// Lines removed.
    pub fn removed_lines(&self) -> usize {
        self.pre_lines.len()
    }

    /// The bytes this hunk inserts, concatenated.
    pub fn post_bytes(&self) -> Vec<u8> {
        join_lines(&self.post_lines)
    }
}

fn kind_tag(kind: HunkKind) -> &'static str {
    match kind {
        HunkKind::Replace => "replace",
        HunkKind::FileAdded => "added",
        HunkKind::FileDeleted => "deleted",
        HunkKind::BinaryReplace => "binary",
    }
}

/// Decompose a modification into hunks.
///
/// Text files are split at every contiguous run of changed lines, with zero
/// lines of context — context exists to help humans and `patch` locate a
/// change, and neither is involved here. Binary files become a single hunk.
pub fn decompose_modified(path: &str, pre: &[u8], post: &[u8]) -> Vec<Hunk> {
    if is_binary(pre) || is_binary(post) {
        return vec![Hunk::new(
            path,
            HunkKind::BinaryReplace,
            0,
            vec![pre.to_vec()],
            vec![post.to_vec()],
        )];
    }

    let pre_lines = split_lines(pre);
    let post_lines = split_lines(post);
    let ops = capture_diff_slices(Algorithm::Myers, &pre_lines, &post_lines);

    let mut hunks = Vec::new();
    for group in group_diff_ops(ops, 0) {
        let changes: Vec<&DiffOp> =
            group.iter().filter(|op| !matches!(op, DiffOp::Equal { .. })).collect();
        let Some(first) = changes.first() else { continue };
        let last = changes.last().expect("non-empty after first()");

        let old_start = first.old_range().start;
        let old_end = last.old_range().end;
        let new_start = first.new_range().start;
        let new_end = last.new_range().end;

        hunks.push(Hunk::new(
            path,
            HunkKind::Replace,
            old_start,
            pre_lines[old_start..old_end].to_vec(),
            post_lines[new_start..new_end].to_vec(),
        ));
    }
    hunks
}

/// The single hunk representing a newly created file.
pub fn file_added(path: &str, post: &[u8]) -> Hunk {
    let post_lines = if is_binary(post) { vec![post.to_vec()] } else { split_lines(post) };
    Hunk::new(path, HunkKind::FileAdded, 0, Vec::new(), post_lines)
}

/// The single hunk representing a deleted file.
pub fn file_deleted(path: &str, pre: &[u8]) -> Hunk {
    let pre_lines = if is_binary(pre) { vec![pre.to_vec()] } else { split_lines(pre) };
    Hunk::new(path, HunkKind::FileDeleted, 0, pre_lines, Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines_of(hunk: &Hunk) -> (Vec<String>, Vec<String>) {
        let to_strings = |v: &Vec<Vec<u8>>| {
            v.iter().map(|l| String::from_utf8_lossy(l).into_owned()).collect::<Vec<_>>()
        };
        (to_strings(&hunk.pre_lines), to_strings(&hunk.post_lines))
    }

    #[test]
    fn an_unchanged_file_produces_no_hunks() {
        assert!(decompose_modified("a.txt", b"same\n", b"same\n").is_empty());
    }

    #[test]
    fn one_changed_line_produces_one_hunk() {
        let hunks = decompose_modified("a.txt", b"one\ntwo\nthree\n", b"one\nTWO\nthree\n");
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].pre_start, 1);
        assert_eq!(hunks[0].pre_len, 1);
        let (pre, post) = lines_of(&hunks[0]);
        assert_eq!(pre, ["two\n"]);
        assert_eq!(post, ["TWO\n"]);
    }

    #[test]
    fn separated_changes_produce_separate_hunks() {
        let pre = b"a\nb\nc\nd\ne\nf\ng\n";
        let post = b"A\nb\nc\nd\ne\nf\nG\n";
        let hunks = decompose_modified("a.txt", pre, post);
        assert_eq!(hunks.len(), 2, "changes far apart must be independently revertible");
        assert_eq!(hunks[0].pre_start, 0);
        assert_eq!(hunks[1].pre_start, 6);
    }

    #[test]
    fn adjacent_changes_coalesce_into_one_hunk() {
        let hunks = decompose_modified("a.txt", b"a\nb\nc\n", b"A\nB\nc\n");
        assert_eq!(hunks.len(), 1);
        assert_eq!((hunks[0].pre_start, hunks[0].pre_len), (0, 2));
    }

    #[test]
    fn pure_insertion_has_zero_pre_length() {
        let hunks = decompose_modified("a.txt", b"a\nc\n", b"a\nb\nc\n");
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].pre_len, 0);
        assert_eq!(hunks[0].pre_start, 1);
        let (_, post) = lines_of(&hunks[0]);
        assert_eq!(post, ["b\n"]);
    }

    #[test]
    fn pure_deletion_has_zero_post_length() {
        let hunks = decompose_modified("a.txt", b"a\nb\nc\n", b"a\nc\n");
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].post_lines.len(), 0);
        assert_eq!((hunks[0].pre_start, hunks[0].pre_len), (1, 1));
    }

    #[test]
    fn hunks_never_overlap() {
        let pre = b"1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n";
        let post = b"1\nX\n3\n4\nY\n6\n7\nZ\n9\n10\n";
        let hunks = decompose_modified("a.txt", pre, post);
        for pair in hunks.windows(2) {
            assert!(pair[0].pre_end() <= pair[1].pre_start, "hunks must be disjoint and ordered");
        }
    }

    #[test]
    fn a_missing_trailing_newline_is_a_real_change() {
        let hunks = decompose_modified("a.txt", b"a\nb\n", b"a\nb");
        assert_eq!(hunks.len(), 1, "removing the final newline must be visible");
    }

    #[test]
    fn binary_content_becomes_one_hunk() {
        let hunks = decompose_modified("logo.png", &[0u8, 1, 2], &[0u8, 9, 9]);
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].kind, HunkKind::BinaryReplace);
        assert_eq!(hunks[0].changed_lines(), 1);
    }

    #[test]
    fn added_and_deleted_files_are_atomic() {
        let added = file_added("new.rs", b"line one\nline two\nline three\n");
        assert_eq!(added.kind, HunkKind::FileAdded);
        assert_eq!(added.pre_len, 0);
        assert_eq!(added.added_lines(), 3);

        let deleted = file_deleted("old.rs", b"gone\n");
        assert_eq!(deleted.kind, HunkKind::FileDeleted);
        assert_eq!(deleted.post_lines.len(), 0);
        assert_eq!(deleted.removed_lines(), 1);
    }

    #[test]
    fn identity_is_content_derived_and_stable() {
        let a = decompose_modified("a.txt", b"x\n", b"y\n");
        let b = decompose_modified("a.txt", b"x\n", b"y\n");
        assert_eq!(a[0].id, b[0].id);

        let other_path = decompose_modified("b.txt", b"x\n", b"y\n");
        assert_ne!(a[0].id, other_path[0].id, "the same edit in another file is another hunk");
    }

    #[test]
    fn changed_line_counts_match_what_a_diff_shows() {
        // One line out, one line in.
        let replace = decompose_modified("a.txt", b"one\n", b"ONE\n");
        assert_eq!(replace[0].changed_lines(), 2);
        // Three lines in, none out.
        assert_eq!(file_added("n.rs", b"a\nb\nc\n").changed_lines(), 3);
    }
}
