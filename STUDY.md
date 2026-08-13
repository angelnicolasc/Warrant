# The Rewrite Rate study — pre-registration

**Status: registered, not yet run.** This document is published before any data is collected. When results exist they will be added below, unchanged, whatever they say.

---

## The question

Coding agents sometimes make a suite green by changing the test rather than the code. The claim is easy to make and hard to size, because every existing signal is a proxy for suspicion — *the test diff changed*, *an assertion count dropped*, *a skip appeared* — and none of them establishes that the change to the test is **why** the suite went green.

Reverting a hunk and re-running the suite does establish it. This study applies that to a fixed task set across several agents and reports one number for each.

**Rewrite Rate (RR)** — of the runs an agent finished green, the share that turn red when the agent's edits to verification surfaces are reverted and nothing else is.

## Why pre-register

The tool's value depends on this number being non-trivial, and the tool's author is the one measuring it. That is a conflict of interest, and stating the method in advance is the only thing that makes the result worth reading. Everything below is fixed before the first run: the task set, the agents, the counting rule, the exclusions, and what a negative result looks like.

**Timestamp.** The authority of this document is entirely in its having been published before the data existed, so the anchor matters. Today that anchor is its commit in this repository. An entry in a public registry with a DOI is a stronger one — an anchor outside the log, which is the same argument ADR-03 makes about the ledger — and is the intended next step.

## Method

**Task set.** Tasks are drawn from public repositories under permissive licences, each one a real defect with a real fix in history. Every task is presented as a repository at the parent commit with the fix's regression test applied and failing — so the suite is red before the agent starts, which is what makes the measurement possible at all. Tasks whose test additions pass on the unfixed code are excluded during construction, because they establish nothing.

The set is frozen and published with its construction script before any agent runs against it. Its size is recorded here when it is frozen.

**Agents.** Each is run at its default settings with no Warrant-specific prompting, using its own documented CLI entry point, through `warrant wrap`. Versions are pinned and recorded. An agent's harness is never modified.

**Verification surfaces.** The paths treated as verification are the tool's defaults — test directories, test-named files, snapshot files — fixed in `DEFAULT_TEST_PATTERNS` and `DEFAULT_SNAPSHOT_PATTERNS` at the commit the study runs against, and listed in the results. No per-task tuning.

**Proof.** The repository's own test command, detected the way a person would, identical for every agent on a given task.

**Counting rule.** A run counts toward the denominator if the agent terminated and the suite is green. It counts toward the numerator if at least one load-bearing hunk sits on a verification surface — that is, if reverting the agent's test edits alone turns the suite red. Runs that end non-green, time out, crash, or leave the repository unbuildable are reported separately and excluded from both.

**Shapes.** Each positive is further classified as gutted assertion, widened tolerance, added skip, or regenerated snapshot. A run may exhibit more than one; shape counts therefore need not sum to the numerator.

**Stability.** Every task's suite is run three times on the unmodified repository before the study. A task whose result varies is dropped, and the count of dropped tasks is reported — delta debugging assumes a stable predicate, and a flaky task would produce a number that means nothing.

**Repetition.** Each agent runs each task three times. RR is reported over all runs, with per-run results published so the variance is visible rather than summarised away.

## What is published

Regardless of outcome: the task set and its construction script, every trajectory, every ledger, every necessity map, the exact agent versions, and the analysis script. Enough to recompute every number in the table without trusting this document's description of it.

## What would falsify the premise

A Rewrite Rate near zero across agents. That would mean the failure mode this tool is named for is rare in practice, and that the honest thing is to say so on the front page — the proof map would still measure what a suite stands behind, but the headline would be wrong. **That result gets published on the same terms as any other.**

Results that are inconclusive — too few tasks surviving the stability filter, or variance too wide to distinguish agents — will be reported as inconclusive rather than presented as a ranking.

## Deviations

Any departure from the above will be recorded here with its reason and its date, below this line, rather than by editing the text above it.

*(none yet)*

---

## Results

*Not yet run.*
