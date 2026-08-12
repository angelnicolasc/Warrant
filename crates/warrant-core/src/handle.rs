//! L3 — handles.
//!
//! Nothing large enters a context window. Artefacts live in the ledger as
//! content-addressed blobs and the model sees a reference:
//!
//! ```text
//! Handle(blake3:ab12…, TestReport, 4.2 MB, "1247 passed, 3 failed")
//! ```
//!
//! The consequence is that **there is nothing to compact**. Compaction is a
//! lossy rewrite of the record applied at exactly the moment the record
//! becomes large, which is exactly the moment it matters — and it is where
//! evidence chains get severed. A summary can be wrong about what a test run
//! said; a handle cannot, because it is the address of the bytes themselves.
//!
//! A handle is deliberately cheap to render and expensive to expand: the
//! model must ask for content by address, and that ask is a recorded tool
//! call rather than an invisible context growth.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::context::ContextRenderable;
use crate::hash::Hash;

/// How many characters of preview a handle carries into context.
///
/// Enough to decide whether to open it, not enough to reason from. A preview
/// long enough to reason from is a summary, and a summary is the thing this
/// design exists to avoid.
pub const PREVIEW_LIMIT: usize = 160;

/// What kind of artefact a handle points at.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    /// Captured standard output.
    Stdout,
    /// Captured standard error.
    Stderr,
    /// The result of running a test suite.
    TestReport,
    /// The contents of a file in a cell.
    FileContent,
    /// A unified diff.
    Diff,
    /// A response body from the network.
    HttpBody,
    /// Anything else.
    Blob,
}

impl ArtifactKind {
    /// The name shown to the model.
    pub fn name(&self) -> &'static str {
        match self {
            ArtifactKind::Stdout => "Stdout",
            ArtifactKind::Stderr => "Stderr",
            ArtifactKind::TestReport => "TestReport",
            ArtifactKind::FileContent => "FileContent",
            ArtifactKind::Diff => "Diff",
            ArtifactKind::HttpBody => "HttpBody",
            ArtifactKind::Blob => "Blob",
        }
    }
}

/// A reference to an artefact, sized for a context window.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Handle {
    /// Where the bytes are.
    pub address: Hash,
    /// What they are.
    pub kind: ArtifactKind,
    /// How many there are.
    pub bytes: u64,
    /// A short excerpt, for deciding whether to open it.
    pub preview: String,
}

impl Handle {
    /// Build a handle over some bytes, deriving the preview from them.
    pub fn of(kind: ArtifactKind, content: &[u8]) -> Self {
        Handle {
            address: Hash::of(content),
            kind,
            bytes: content.len() as u64,
            preview: preview_of(content),
        }
    }

    /// Build a handle for content already stored, without re-reading it.
    pub fn at(address: Hash, kind: ArtifactKind, bytes: u64, preview: impl Into<String>) -> Self {
        Handle { address, kind, bytes, preview: truncate(&preview.into()) }
    }

    /// Whether the artefact is empty.
    pub fn is_empty(&self) -> bool {
        self.bytes == 0
    }

    /// Size rendered the way a person reads it.
    pub fn human_size(&self) -> String {
        const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
        let mut size = self.bytes as f64;
        let mut unit = 0;
        while size >= 1024.0 && unit < UNITS.len() - 1 {
            size /= 1024.0;
            unit += 1;
        }
        if unit == 0 {
            format!("{} {}", self.bytes, UNITS[0])
        } else {
            format!("{size:.1} {}", UNITS[unit])
        }
    }
}

/// Take the first line or so of content, with control characters removed.
fn preview_of(content: &[u8]) -> String {
    let text = String::from_utf8_lossy(&content[..content.len().min(PREVIEW_LIMIT * 4)]);
    let cleaned: String = text
        .chars()
        .map(|c| if c.is_control() && c != '\n' { ' ' } else { c })
        .collect::<String>()
        .lines()
        // A test runner's most informative line is rarely its first, but the
        // first non-empty one is a far better default than the whole file.
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default()
        .trim()
        .to_string();
    truncate(&cleaned)
}

fn truncate(text: &str) -> String {
    if text.chars().count() <= PREVIEW_LIMIT {
        return text.to_string();
    }
    let kept: String = text.chars().take(PREVIEW_LIMIT).collect();
    format!("{kept}…")
}

impl fmt::Display for Handle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Handle({}…, {}, {}",
            &self.address.to_string()[.."blake3:".len() + 12],
            self.kind.name(),
            self.human_size()
        )?;
        if self.preview.is_empty() { write!(f, ")") } else { write!(f, ", {:?})", self.preview) }
    }
}

impl ContextRenderable for Handle {
    /// What the model sees: an address, a kind, a size and an excerpt.
    ///
    /// Constant in the size of the artefact. A four-hundred-megabyte test log
    /// and an empty one render to the same number of tokens.
    fn render_for_model(&self) -> String {
        self.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_handle_renders_at_constant_size_however_large_the_artefact() {
        let small = Handle::of(ArtifactKind::Stdout, b"ok\n");
        let large = Handle::of(ArtifactKind::Stdout, &vec![b'x'; 8 * 1024 * 1024]);

        let rendered = large.render_for_model();
        assert!(rendered.len() < 300, "a handle must not grow with its artefact: {rendered}");
        assert!(rendered.contains("8.0 MB"));
        assert!(small.render_for_model().contains("3 B"));
    }

    #[test]
    fn the_preview_is_the_first_line_worth_reading() {
        let report = Handle::of(
            ArtifactKind::TestReport,
            b"\n\n   \n1247 passed, 3 failed in 41.2s\nmore detail follows\n",
        );
        assert_eq!(report.preview, "1247 passed, 3 failed in 41.2s");
    }

    #[test]
    fn a_preview_is_capped_so_it_cannot_become_a_summary() {
        let long = "a".repeat(10_000);
        let handle = Handle::of(ArtifactKind::Blob, long.as_bytes());
        assert!(handle.preview.chars().count() <= PREVIEW_LIMIT + 1);
        assert!(handle.preview.ends_with('…'));
    }

    #[test]
    fn control_characters_do_not_reach_the_model() {
        let handle = Handle::of(ArtifactKind::Stdout, b"progress\x1b[2K\x07 done\n");
        assert!(!handle.preview.contains('\x1b'));
        assert!(!handle.preview.contains('\x07'));
    }

    #[test]
    fn binary_content_still_produces_a_usable_handle() {
        let handle = Handle::of(ArtifactKind::Blob, &[0u8, 159, 146, 150, 255]);
        assert_eq!(handle.bytes, 5);
        assert!(!handle.render_for_model().is_empty());
    }

    #[test]
    fn identical_artefacts_share_an_address() {
        let a = Handle::of(ArtifactKind::Stdout, b"same output");
        let b = Handle::of(ArtifactKind::Stderr, b"same output");
        assert_eq!(a.address, b.address, "the address names the bytes, not the role");
        assert_ne!(a.kind, b.kind);
    }

    #[test]
    fn sizes_read_the_way_a_person_reads_them() {
        let cases = [(0u64, "0 B"), (512, "512 B"), (1024, "1.0 KB"), (4_404_019, "4.2 MB")];
        for (bytes, expected) in cases {
            let handle = Handle::at(Hash::ZERO, ArtifactKind::Blob, bytes, "");
            assert_eq!(handle.human_size(), expected);
        }
    }

    #[test]
    fn the_rendering_matches_the_documented_shape() {
        let handle = Handle::at(Hash::of(b"x"), ArtifactKind::TestReport, 4_404_019, "3 failed");
        let rendered = handle.render_for_model();
        assert!(rendered.starts_with("Handle(blake3:"));
        assert!(rendered.contains("TestReport"));
        assert!(rendered.contains("4.2 MB"));
        assert!(rendered.contains("3 failed"));
    }
}
