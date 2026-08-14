#!/usr/bin/env python3
"""Build the Rewrite Rate task set.

A task is a real defect, in a public repository, presented the way an agent
would receive it: the repository at the commit *before* the fix, with the
fix's own regression test applied and failing. Making that test pass without
breaking anything else is the task. The fix its author actually wrote is
never shown to anyone; it exists only as a record of what the answer looked
like.

The filter that matters is the last one. A commit whose test changes already
pass on the unfixed code establishes nothing -- the suite is green before and
after, so nothing in the diff is load-bearing and the map would be vacuous.
Those commits are common, and dropping them is why this script exists rather
than a list of commit hashes chosen by hand.

Everything here runs without a model and without a network beyond `git`.

    python study/harvest.py --out study/tasks.json

See STUDY.md for what the set is for and what is done with it.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import shutil
import subprocess
import sys
import time
from collections import defaultdict
from dataclasses import dataclass, asdict
from pathlib import Path

# Repositories to draw from. Go only, deliberately: `go test ./...` needs no
# per-repository environment, which is the difference between a task set that
# reproduces on someone else's machine and one that reproduces on mine. Other
# ecosystems need a container story first; the hooks below are where they go.
REPOS = [
    "https://github.com/spf13/cast",
    "https://github.com/spf13/afero",
    "https://github.com/mitchellh/mapstructure",
    "https://github.com/hashicorp/go-multierror",
    "https://github.com/tidwall/gjson",
    "https://github.com/tidwall/sjson",
    "https://github.com/json-iterator/go",
    "https://github.com/google/uuid",
    "https://github.com/asaskevich/govalidator",
    "https://github.com/gorilla/mux",
]

IS_TEST = re.compile(r"_test\.go$")
IS_SOURCE = re.compile(r"^(?!.*_test\.go$).*\.go$")


@dataclass
class Task:
    """One unit of work, fully determined by what is written here."""

    id: str
    repo: str
    #: The commit whose fix is the answer. Never given to an agent.
    fix: str
    #: The commit the agent starts from: `fix~1` plus the test files from `fix`.
    base: str
    parent: str
    test_paths: list[str]
    source_paths: list[str]
    #: What the repository itself says its tests are, as Warrant detects it.
    command: str
    #: Lines the author's fix changed outside test files. Context, not a target.
    fix_source_lines: int
    #: Seconds for one run of the suite on the green parent.
    suite_seconds: float


def run(args, cwd, timeout=900, env=None):
    return subprocess.run(
        args, cwd=cwd, timeout=timeout, env=env,
        capture_output=True, text=True, errors="replace",
    )


def git(cwd, *args, check=True):
    result = run(["git", *args], cwd)
    if check and result.returncode != 0:
        raise RuntimeError(f"git {' '.join(args)}: {result.stderr.strip()[:300]}")
    return result.stdout


def suite_passes(cwd, command, timeout=900):
    """Run the repository's own test command. True when it exits zero."""
    return run(command, cwd, timeout=timeout).returncode == 0


def detect_command(warrant, cwd):
    """Ask Warrant what it would use, so the harvest and the study agree.

    Reading it back from the tool rather than restating it here means a task
    cannot be built against one command and then measured against another.
    """
    out = run([warrant, "proof"], cwd).stdout
    for line in out.splitlines():
        if line.strip().startswith("runs "):
            return line.split(maxsplit=1)[1].strip().split()
    return None


def candidates(cwd, depth):
    """Commits touching both a test file and a source file, newest first.

    Merges are excluded. `fix~1` of a merge is the mainline before it rather
    than the state the work started from, and a merge's diff against named
    paths is empty — which is how the first pass produced four "tasks" whose
    recorded fix was zero lines long.
    """
    log = git(cwd, "log", f"-{depth}", "--no-merges", "--format=%H")
    for sha in log.split():
        files = git(cwd, "show", "--name-only", "--format=", sha).split()
        tests = [f for f in files if IS_TEST.search(f)]
        source = [f for f in files if IS_SOURCE.match(f) and not IS_TEST.search(f)]
        if tests and source:
            yield sha, tests, source


def harvest_repo(url, workdir, warrant, depth, per_repo, stability, log, rejected):
    name = url.rstrip("/").split("/")[-1]
    clone = workdir / name
    if not clone.exists():
        log(f"  cloning {url}")
        # A full clone on purpose. A blobless one fetches old file contents on
        # demand, and `checkout <old-sha> -- <path>` is exactly where that
        # fails; a task set that depends on a promisor fetch succeeding is not
        # a task set anyone else can rebuild.
        r = run(["git", "clone", "--quiet", url, str(clone)], workdir, timeout=1800)
        if r.returncode != 0:
            log(f"  ! clone failed: {r.stderr.strip()[:160]}")
            return []
    git(clone, "config", "user.email", "harvest@example.invalid")
    git(clone, "config", "user.name", "harvest")
    git(clone, "config", "core.autocrlf", "false")

    # Scan from the default branch every time. Left to itself a reused clone
    # is wherever the last run abandoned it, and the range of commits examined
    # would silently depend on that -- the set would stop being a function of
    # this script and its inputs.
    head = git(clone, "symbolic-ref", "refs/remotes/origin/HEAD", check=False).strip()
    default = head.rsplit("/", 1)[-1] if head else "main"
    git(clone, "checkout", "--quiet", "--force", f"origin/{default}")
    git(clone, "clean", "-qfdx", check=False)

    found = []
    for sha, tests, source in candidates(clone, depth):
        if len(found) >= per_repo:
            break
        short = sha[:8]
        try:
            outcome = consider(clone, name, url, sha, tests, source, warrant, stability)
        except Exception as error:                     # noqa: BLE001
            # One unusable commit is not a reason to lose the rest of a
            # repository, and which commits are unusable is itself a finding.
            outcome = f"{type(error).__name__} while preparing"
            git(clone, "checkout", "--quiet", "--force", f"{sha}~1", check=False)

        if isinstance(outcome, Task):
            found.append(outcome)
            log(f"  {short} KEPT  ({outcome.fix_source_lines} source lines, "
                f"suite {outcome.suite_seconds:.1f}s)")
        else:
            rejected[outcome] += 1
            log(f"  {short} skipped: {outcome}")
    return found


def consider(clone, name, url, sha, tests, source, warrant, stability):
    """A `Task`, or the reason this commit cannot be one.

    The reasons are tallied and published with the set. How often a candidate
    fails each filter is a fact about how real repositories are maintained,
    and it is the evidence that hand-picking commits would have been the wrong
    way to build this.
    """
    short = sha[:8]
    git(clone, "checkout", "--quiet", "--force", f"{sha}~1")
    git(clone, "clean", "-qfd", check=False)

    command = detect_command(warrant, clone)
    if not command:
        return "no test command detected"

    # The parent must be green, and reliably so. A task whose suite wobbles
    # would produce a map built on probes that contradict each other, which is
    # worse than no task at all.
    started = time.monotonic()
    if not suite_passes(clone, command):
        return "parent is not green"
    suite_seconds = time.monotonic() - started
    if any(not suite_passes(clone, command) for _ in range(stability - 1)):
        return "parent suite is unstable"

    # Apply the author's tests and nothing else. This is the task.
    git(clone, "checkout", "--quiet", sha, "--", *tests)
    if suite_passes(clone, command):
        git(clone, "checkout", "--quiet", "--force", f"{sha}~1")
        return "the tests pass on the unfixed code"

    # And the author's fix has to actually resolve it, or the task has no
    # known solution and belongs in nobody's benchmark.
    git(clone, "checkout", "--quiet", sha, "--", *source)
    if not suite_passes(clone, command):
        git(clone, "checkout", "--quiet", "--force", f"{sha}~1")
        return "the author's own fix does not make it green"

    fix_lines = sum(
        1
        for line in git(clone, "show", "--unified=0", "--format=", sha, "--", *source).splitlines()
        if (line.startswith("+") or line.startswith("-"))
        and not line.startswith(("+++", "---"))
    )

    # Belt and braces after the merge exclusion above: a fix that changes no
    # source lines is not a fix, and a task built on one has no answer.
    if fix_lines == 0:
        git(clone, "checkout", "--quiet", "--force", f"{sha}~1")
        return "the fix changes no source lines"

    git(clone, "checkout", "--quiet", "--force", f"{sha}~1")
    git(clone, "checkout", "--quiet", sha, "--", *tests)
    git(clone, "commit", "--quiet", "-am", f"task: regression test from {short}", check=False)
    base = git(clone, "rev-parse", "HEAD").strip()
    parent = git(clone, "rev-parse", f"{sha}~1").strip()
    git(clone, "checkout", "--quiet", "--force", f"{sha}~1")

    return Task(
        id=f"{name}-{short}",
        repo=url,
        fix=sha,
        base=base,
        parent=parent,
        test_paths=sorted(tests),
        source_paths=sorted(source),
        command=" ".join(command),
        fix_source_lines=fix_lines,
        suite_seconds=round(suite_seconds, 2),
    )


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--out", type=Path, default=Path("study/tasks.json"))
    ap.add_argument("--workdir", type=Path, default=Path(".harvest"))
    ap.add_argument("--warrant", default="warrant")
    ap.add_argument("--depth", type=int, default=120, help="commits to scan per repository")
    ap.add_argument("--per-repo", type=int, default=4, help="tasks to keep per repository")
    ap.add_argument("--stability", type=int, default=3, help="green runs required of the parent")
    ap.add_argument("--repos", nargs="*", default=REPOS)
    args = ap.parse_args()

    if not shutil.which(args.warrant):
        sys.exit(f"{args.warrant} is not on the path; build it or pass --warrant")

    args.workdir.mkdir(parents=True, exist_ok=True)
    def log(message):
        print(message, flush=True)

    tasks = []
    rejected: dict[str, int] = defaultdict(int)
    for url in args.repos:
        log(f"{url}")
        try:
            tasks.extend(harvest_repo(url, args.workdir, args.warrant, args.depth,
                                      args.per_repo, args.stability, log, rejected))
        except Exception as error:                     # noqa: BLE001
            log(f"  ! {type(error).__name__}: {error}")

    tasks.sort(key=lambda t: t.id)
    body = [asdict(t) for t in tasks]
    # A fingerprint over the identifying fields alone, so the set can be
    # quoted and checked without depending on timings that vary by machine.
    identity = json.dumps(
        [{k: t[k] for k in ("id", "repo", "fix", "parent", "test_paths", "command")} for t in body],
        sort_keys=True,
    )
    document = {
        "generated_by": "study/harvest.py",
        "task_count": len(body),
        # Why candidates were turned away, in the order that matters most.
        "rejected": dict(sorted(rejected.items(), key=lambda kv: -kv[1])),
        "fingerprint": "blake2b:" + hashlib.blake2b(identity.encode(), digest_size=16).hexdigest(),
        "tasks": body,
    }
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(json.dumps(document, indent=2) + "\n", encoding="utf-8")
    log(f"\n{len(body)} tasks kept, {sum(rejected.values())} candidates rejected -> {args.out}")
    for why, count in document["rejected"].items():
        log(f"  {count:4}  {why}")
    log(f"fingerprint {document['fingerprint']}")


if __name__ == "__main__":
    main()
