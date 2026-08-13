//! The review plane, on a terminal.
//!
//! The map answers a question nobody asked in those words. What a person
//! standing in front of an agent's diff wants to know is *how much of this do
//! I have to read*, and the map answers exactly that: the load-bearing hunks
//! have the suite standing behind them, and the rest does not.
//!
//! So the first line is the reading list, not the verdict. The last one is the
//! alarm — whether any of what the proof depends on is the proof itself.
//! Everything between them is context.
//!
//! Note the direction of the honest claim. Unproven does not mean wrong, and
//! proven does not mean correct: necessity is not sufficiency, and a
//! load-bearing hunk is proven relative to that proof and nothing else. What
//! can be said without qualification is which part of the diff has a test
//! behind it, and that is what this prints.

use crate::pipeline::TrimPlan;
use warrant_necessity::{FileVerdict, MapOutcome, NecessityMap};

/// Characters used for output. Unicode by default; ASCII for terminals that
/// would otherwise show boxes.
#[derive(Clone, Copy, Debug)]
pub struct Glyphs {
    /// Discharged.
    pub tick: &'static str,
    /// Coverage marker.
    pub gauge: &'static str,
    /// Warning.
    pub warn: &'static str,
    /// Filled bar segment.
    pub filled: &'static str,
    /// Empty bar segment.
    pub empty: &'static str,
    /// Continuation branch.
    pub branch: &'static str,
    /// En dash.
    pub dash: &'static str,
}

impl Glyphs {
    /// The default set.
    pub const UNICODE: Glyphs = Glyphs {
        tick: "✓",
        gauge: "⬒",
        warn: "⚠",
        filled: "█",
        empty: "░",
        branch: "└",
        dash: "—",
    };

    /// For terminals without the fonts for it.
    pub const ASCII: Glyphs = Glyphs {
        tick: "ok",
        gauge: "*",
        warn: "!!",
        filled: "#",
        empty: ".",
        branch: "\\",
        dash: "-",
    };
}

/// Width of the coverage bar, in segments.
const BAR_WIDTH: usize = 10;

/// How wide the path column is before it starts pushing the bar right.
const PATH_WIDTH: usize = 26;

fn bar(verdict: &FileVerdict, glyphs: &Glyphs) -> String {
    let share = verdict.coverage().as_f64().unwrap_or(0.0);
    let mut filled = (share * BAR_WIDTH as f64).round() as usize;
    // Any proven line at all must show at least one segment: rounding a real
    // finding down to an empty bar is exactly the wrong direction to err in.
    if verdict.proven_lines > 0 && filled == 0 {
        filled = 1;
    }
    let filled = filled.min(BAR_WIDTH);
    format!("{}{}", glyphs.filled.repeat(filled), glyphs.empty.repeat(BAR_WIDTH - filled))
}

fn status(verdict: &FileVerdict, glyphs: &Glyphs) -> String {
    let unread = verdict.changed_lines.saturating_sub(verdict.proven_lines);
    if verdict.tampered {
        format!("{} load-bearing test edit", glyphs.warn)
    } else if verdict.is_entirely_unproven() {
        format!("read {unread} lines {} unproven, revert-safe", glyphs.dash)
    } else if unread > 0 {
        format!("read {unread} lines")
    } else {
        "proven".to_string()
    }
}

/// Whether the whole file has the suite standing behind it.
fn fully_proven(verdict: &FileVerdict) -> bool {
    verdict.total_hunks > 0 && verdict.load_bearing_hunks == verdict.total_hunks
}

/// Render a completed map.
pub fn render_map(
    map: &NecessityMap,
    proof_source: &str,
    proof_defaulted: bool,
    glyphs: &Glyphs,
) -> String {
    let mut out = String::new();
    let origin = if proof_defaulted { " (default)" } else { "" };

    match map.outcome {
        MapOutcome::Mapped => {
            let changed: u64 = map.files.iter().map(|f| f.changed_lines).sum();
            let proven: u64 = map.files.iter().map(|f| f.proven_lines).sum();
            let to_read = changed.saturating_sub(proven);
            let files_to_read =
                map.files.iter().filter(|f| f.load_bearing_hunks < f.total_hunks).count();

            out.push_str(&format!(
                "  {} read {} of {} changed lines{}\n",
                glyphs.gauge,
                to_read,
                changed,
                match files_to_read {
                    0 => String::new(),
                    1 => "  in 1 file".to_string(),
                    n => format!("  in {n} files"),
                }
            ));
            out.push_str(&format!(
                "  {} the other {proven} are load-bearing: revert any one and the proof turns red\n",
                glyphs.tick
            ));
            out.push_str(&format!("    proof: {proof_source}{origin}\n"));
        }
        MapOutcome::NotSatisfied => {
            out.push_str(&format!(
                "  {} claim not discharged    proof: {proof_source}{origin}\n",
                glyphs.warn
            ));
            out.push_str("    the proof does not hold on this result; nothing was mapped\n");
        }
        MapOutcome::Vacuous => {
            out.push_str(&format!(
                "  {} proof is vacuous        proof: {proof_source}{origin}\n",
                glyphs.warn
            ));
            out.push_str(
                "    it already held before any work was done, so it proves nothing about this change\n",
            );
            out.push_str("    proof coverage  n/a\n");
        }
        MapOutcome::UnstableProof => {
            out.push_str(&format!(
                "  {} proof is unstable       proof: {proof_source}{origin}\n",
                glyphs.warn
            ));
            out.push_str(
                "    it answered differently on identical state; the suite is flaky, and no map is meaningful\n",
            );
        }
        MapOutcome::NoChanges => {
            out.push_str(&format!("  {} nothing changed on disk\n", glyphs.gauge));
            return out;
        }
    }

    if map.files.is_empty() {
        return out;
    }
    out.push('\n');

    // The reading list first, then what needs no reading, then the flagged
    // ones last so the reader's eye ends on the thing that matters most.
    let mut files: Vec<&FileVerdict> = map.files.iter().collect();
    files.sort_by_key(|f| (f.tampered, fully_proven(f), f.path.clone()));

    for verdict in &files {
        out.push_str(&format!(
            "  {:<width$}  {}  {}\n",
            verdict.path,
            bar(verdict, glyphs),
            status(verdict, glyphs),
            width = PATH_WIDTH
        ));
        if verdict.tampered {
            out.push_str(&format!(
                "  {:<width$}  {} reverting it makes the proof fail\n",
                "",
                glyphs.branch,
                width = PATH_WIDTH
            ));
            out.push_str(&format!(
                "  {:<width$}    the change that made it pass was the change to the test\n",
                "",
                width = PATH_WIDTH
            ));
        }
    }

    let mut notes = Vec::new();
    if map.budget_exhausted {
        notes.push(
            "the probe budget ran out, so the map is coarser than it could be — raise --max-probes"
                .to_string(),
        );
    }
    if !map.monotonicity_violations.is_empty() {
        notes.push(format!(
            "the proof contradicted itself on {} hunks; treat this map as approximate and look at suite flakiness",
            map.monotonicity_violations.len()
        ));
    }
    if map.outcome == MapOutcome::Mapped && !map.minimality_confirmed {
        notes.push(
            "per-hunk confirmation did not finish, so load-bearing is not proven individually"
                .into(),
        );
    }
    if !notes.is_empty() {
        out.push('\n');
        for note in notes {
            out.push_str(&format!("  note: {note}\n"));
        }
    }

    out
}

/// Render what a trim would take back, and whether the result held.
pub fn render_trim(plan: &TrimPlan, write: bool, glyphs: &Glyphs) -> String {
    let mut out = String::new();

    if plan.is_empty() {
        out.push_str(&format!("  {} nothing to trim: every hunk is load-bearing\n", glyphs.tick));
        return out;
    }

    if !plan.verified {
        out.push_str(&format!(
            "  {} the trimmed tree does not pass the proof, so there is no trim to offer\n",
            glyphs.warn
        ));
        out.push_str(
            "    the load-bearing hunks are jointly necessary but not jointly sufficient here;\n",
        );
        out.push_str("    the change relies on something the search could not isolate\n");
        return out;
    }

    let total = plan.kept_lines + plan.dropped_lines;
    out.push_str(&format!(
        "  {} trimmed {} of {} changed lines, and the proof still holds\n",
        glyphs.tick, plan.dropped_lines, total
    ));
    out.push_str(&format!(
        "  {} verified on the trimmed tree itself, not inferred from the map\n",
        glyphs.gauge
    ));
    out.push('\n');

    for file in &plan.files {
        let note = if file.fully_reverted { "reverted" } else { "partly reverted" };
        let hunks = if file.dropped_hunks == 1 { "hunk" } else { "hunks" };
        out.push_str(&format!(
            "  {:<width$}  -{:<5} {note}, {} {hunks}\n",
            file.path,
            file.dropped_lines,
            file.dropped_hunks,
            width = PATH_WIDTH
        ));
    }

    out.push('\n');
    if write {
        out.push_str("  what came off was unproven, which is not the same as unwanted.\n");
        out.push_str("  the tree before the trim is still in the record: `warrant log`\n");
    } else {
        out.push_str(&format!("  the verified tree is at {}\n", plan.root.display()));
        out.push_str(
            "  nothing was written. `warrant trim --write` puts it in the working tree.\n",
        );
        out.push_str(
            "  read what comes off first: unproven means the suite says nothing about it,\n",
        );
        out.push_str("  not that you did not want it.\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::TrimmedFile;
    use warrant_core::{Hash, HunkId, PredicateHash, Ratio};
    use warrant_diff::ChangeKind;

    fn verdict(path: &str, load_bearing: usize, total: usize, tampered: bool) -> FileVerdict {
        FileVerdict {
            path: path.into(),
            change: ChangeKind::Modified,
            total_hunks: total,
            load_bearing_hunks: load_bearing,
            changed_lines: total as u64 * 10,
            proven_lines: load_bearing as u64 * 10,
            verification_surface: tampered,
            tampered,
        }
    }

    /// Build a map whose totals actually follow from its files, so the test
    /// cannot assert numbers the renderer would never produce.
    fn map_with(outcome: MapOutcome, files: Vec<FileVerdict>) -> NecessityMap {
        let mut map = NecessityMap::no_changes(PredicateHash::derive(&[b"p"]), Hash::of(b"t"));
        map.outcome = outcome;
        map.satisfied = outcome == MapOutcome::Mapped;
        map.coverage = Ratio::new(
            files.iter().map(|f| f.proven_lines).sum(),
            files.iter().map(|f| f.changed_lines).sum(),
        );
        map.load_bearing = (0..files.iter().map(|f| f.load_bearing_hunks).sum::<usize>())
            .map(|i| HunkId::derive(&[b"lb", &[i as u8]]))
            .collect();
        map.unproven =
            (0..files.iter().map(|f| f.total_hunks - f.load_bearing_hunks).sum::<usize>())
                .map(|i| HunkId::derive(&[b"up", &[i as u8]]))
                .collect();
        map.minimality_confirmed = true;
        map.files = files;
        map
    }

    fn empty_map(outcome: MapOutcome) -> NecessityMap {
        let mut map = map_with(outcome, Vec::new());
        map.coverage = Ratio::UNDEFINED;
        map
    }

    #[test]
    fn a_mapped_run_reads_like_the_documented_output() {
        let map = map_with(
            MapOutcome::Mapped,
            vec![
                verdict("src/middleware/rate.py", 4, 5, false),
                verdict("src/api/upload.py", 3, 5, false),
                verdict("src/utils/cache.py", 0, 4, false),
                verdict("tests/test_upload.py", 1, 5, true),
            ],
        );

        // 8 of 19 hunks load-bearing, 80 of 190 changed lines. The headline is
        // the reading list: 110 lines with nothing standing behind them.
        let text = render_map(&map, "pytest -q", true, &Glyphs::UNICODE);
        assert!(text.contains("read 110 of 190 changed lines"), "{text}");
        assert!(text.contains("in 4 files"), "{text}");
        assert!(text.contains("the other 80 are load-bearing"), "{text}");
        assert!(text.contains("proof: pytest -q (default)"), "{text}");
        assert!(text.contains("unproven, revert-safe"), "{text}");
        assert!(text.contains("⚠ load-bearing test edit"));
        assert!(text.contains("the change that made it pass was the change to the test"));
    }

    /// What a reader has to read comes before what they do not, so the list
    /// can be worked from the top and abandoned partway down.
    #[test]
    fn the_reading_list_comes_before_the_files_that_need_no_reading() {
        let map = map_with(
            MapOutcome::Mapped,
            vec![verdict("src/proven.py", 3, 3, false), verdict("src/unproven.py", 0, 3, false)],
        );
        let text = render_map(&map, "pytest", true, &Glyphs::UNICODE);
        let unproven_at = text.find("src/unproven.py").unwrap();
        let proven_at = text.find("src/proven.py").unwrap();
        assert!(unproven_at < proven_at, "the reading list must come first:\n{text}");
    }

    /// A diff with nothing unproven in it is the case worth stating plainly:
    /// there is no reading list at all.
    #[test]
    fn a_fully_proven_diff_asks_for_no_reading() {
        let map = map_with(MapOutcome::Mapped, vec![verdict("src/a.py", 4, 4, false)]);
        let text = render_map(&map, "pytest", false, &Glyphs::UNICODE);
        assert!(text.contains("read 0 of 40 changed lines"), "{text}");
        assert!(!text.contains("in 1 file"), "no file needs reading: {text}");
    }

    #[test]
    fn the_flagged_file_is_rendered_last() {
        let map = map_with(
            MapOutcome::Mapped,
            vec![verdict("tests/test_x.py", 1, 1, true), verdict("src/a.py", 1, 1, false)],
        );
        let text = render_map(&map, "pytest", true, &Glyphs::UNICODE);
        let tests_at = text.find("tests/test_x.py").unwrap();
        let src_at = text.find("src/a.py").unwrap();
        assert!(src_at < tests_at, "the reader's eye should end on the finding");
    }

    #[test]
    fn a_file_with_any_proven_line_never_renders_as_an_empty_bar() {
        let mut v = verdict("src/a.py", 1, 100, false);
        v.proven_lines = 1;
        v.changed_lines = 1000;
        let rendered = bar(&v, &Glyphs::UNICODE);
        assert!(rendered.starts_with('█'), "rounding must not hide a real finding: {rendered}");
    }

    #[test]
    fn a_fully_proven_file_fills_the_bar() {
        let v = verdict("src/a.py", 5, 5, false);
        assert_eq!(bar(&v, &Glyphs::UNICODE), "██████████");
    }

    #[test]
    fn a_vacuous_proof_says_so_and_prints_no_percentage() {
        let map = empty_map(MapOutcome::Vacuous);
        let text = render_map(&map, "pytest", true, &Glyphs::UNICODE);
        assert!(text.contains("proof is vacuous"));
        assert!(text.contains("proof coverage  n/a"));
        assert!(!text.contains('%'), "no percentage may appear: {text}");
    }

    #[test]
    fn an_unstable_proof_points_at_flakiness_rather_than_at_the_agent() {
        let map = empty_map(MapOutcome::UnstableProof);
        let text = render_map(&map, "pytest", true, &Glyphs::UNICODE);
        assert!(text.contains("flaky"));
    }

    #[test]
    fn an_exhausted_budget_is_stated_rather_than_hidden() {
        let mut map = map_with(MapOutcome::Mapped, vec![verdict("a.py", 1, 2, false)]);
        map.budget_exhausted = true;
        let text = render_map(&map, "pytest", true, &Glyphs::UNICODE);
        assert!(text.contains("probe budget ran out"));
    }

    fn trim_plan(verified: bool, files: Vec<TrimmedFile>) -> TrimPlan {
        let dropped_lines = files.iter().map(|f| f.dropped_lines).sum();
        TrimPlan {
            snapshot: warrant_diff::Snapshot { files: Default::default() },
            verified,
            root: std::path::PathBuf::from("/tmp/trim"),
            files,
            kept_lines: 80,
            dropped_lines,
        }
    }

    fn dropped(path: &str, hunks: usize, lines: u64, fully: bool) -> TrimmedFile {
        TrimmedFile {
            path: path.into(),
            dropped_hunks: hunks,
            dropped_lines: lines,
            fully_reverted: fully,
        }
    }

    #[test]
    fn a_dry_run_says_what_would_come_off_and_writes_nothing() {
        let plan = trim_plan(
            true,
            vec![dropped("docs/notes.md", 1, 4, true), dropped("src/a.py", 2, 6, false)],
        );
        let text = render_trim(&plan, false, &Glyphs::UNICODE);

        assert!(text.contains("trimmed 10 of 90 changed lines"), "{text}");
        assert!(text.contains("reverted, 1 hunk\n"), "singular reads badly otherwise:\n{text}");
        assert!(text.contains("partly reverted, 2 hunks"), "{text}");
        assert!(text.contains("nothing was written"), "{text}");
        assert!(text.contains("not that you did not want it"), "{text}");
    }

    /// Necessity is not sufficiency, and the one place that could be mistaken
    /// for a proof of it is a trim. So the trimmed tree is re-run, and when it
    /// does not hold there is no offer to make.
    #[test]
    fn a_trim_that_does_not_hold_is_refused_rather_than_offered() {
        let plan = trim_plan(false, vec![dropped("src/a.py", 1, 4, true)]);
        let text = render_trim(&plan, false, &Glyphs::UNICODE);

        assert!(text.contains("does not pass the proof"), "{text}");
        assert!(text.contains("jointly necessary but not jointly sufficient"), "{text}");
        assert!(!text.contains("--write"), "no write may be suggested:\n{text}");
    }

    #[test]
    fn a_fully_load_bearing_change_has_nothing_to_trim() {
        let text = render_trim(&trim_plan(true, Vec::new()), false, &Glyphs::UNICODE);
        assert!(text.contains("every hunk is load-bearing"), "{text}");
    }

    #[test]
    fn trim_output_is_ascii_in_ascii_mode() {
        let plan = trim_plan(true, vec![dropped("docs/notes.md", 1, 4, true)]);
        for write in [false, true] {
            let text = render_trim(&plan, write, &Glyphs::ASCII);
            assert!(text.is_ascii(), "ascii mode leaked a non-ascii character:\n{text}");
        }
    }

    #[test]
    fn ascii_mode_emits_nothing_outside_ascii() {
        let map = map_with(
            MapOutcome::Mapped,
            vec![verdict("tests/t.py", 1, 2, true), verdict("src/a.py", 0, 2, false)],
        );
        let text = render_map(&map, "pytest", true, &Glyphs::ASCII);
        assert!(text.is_ascii(), "ascii mode leaked a non-ascii character:\n{text}");
    }
}
