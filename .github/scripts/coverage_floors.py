#!/usr/bin/env python3
"""Hold line coverage to a floor, for each crate and for the workspace.

The CI coverage job runs the tests once under cargo-llvm-cov, then writes a summary:

    cargo llvm-cov report --json --summary-only --output-path coverage-summary.json \
        --ignore-filename-regex "$COVERAGE_IGNORE"
    python3 .github/scripts/coverage_floors.py coverage-summary.json

`just coverage` does the same locally. See the `coverage` job in .github/workflows/ci.yml.

Files are grouped by the crate directory they sit in: `crates/<name>/` and
`examples/plugins/<name>/`, keyed here by the directory name (`serialist` is the binary).
A crate's lines are every instrumented line of its source files that the report kept; what
`COVERAGE_IGNORE` leaves out never counts, for or against. The workspace row is the sum of
all of them.

Prints a table (crate, covered/total lines, percent, floor, verdict) to stdout, and to
$GITHUB_STEP_SUMMARY as Markdown when that is set.

Exit status: 0 every floor is met, 1 a crate or the workspace is under its floor (or a
crate in the report has no floor, or a floor has no crate: add or drop its line in FLOORS),
2 the input is not what this expects: not a cargo-llvm-cov JSON summary, a file outside
crates/ and examples/plugins/, bad arguments.

Python 3, standard library only.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
from dataclasses import dataclass
from pathlib import Path

WORKSPACE = "workspace"

# Minimum line coverage, in percent. Every crate directory in the report needs a line here.
#
# The rule: the lower of the two measurements below, minus 3 points, rounded down to a whole
# percent (never above the measurement minus 2). Both are lines covered of lines in the
# report `just coverage` and the CI coverage job write, which leaves out COVERAGE_IGNORE
# (crates/serialist/src/main.rs):
#
#   macOS    `just coverage` on 2026-10-01 at af07fe2, all tests, nothing skipped.
#   Linux    the CI coverage job's lcov artifact (run 36952756804, main at f3e06cb, in
#            lcov line counts), recounted without main.rs. Linux is where the floors are
#            enforced; it compiles different platform code than macOS, but the two agree to
#            within 0.2 points here. The binary's Linux figure is from before it grew.
#
#   crate                  macOS     Linux     floor
#   workspace              90.90%    90.74%    87
#   serialist-core         94.37%    94.21%    91
#   serialist-sim          93.52%    93.34%    90
#   serialist-vt           93.63%    93.63%    90
#   serialist-plugins      89.53%    89.47%    86
#   serialist-script       88.53%    88.39%    85
#   serialist-ui           89.10%    89.00%    86
#   serialist (binary)     97.11%    96.42%    93
#   serialist-plugin-sdk   73.78%    73.78%    70
#
# Raise a floor when its crate's coverage has gone up and should stay there. Lower one only
# with a reason in the commit: a floor that moves down with the code guards nothing.
FLOORS: dict[str, float] = {
    WORKSPACE: 87,
    "serialist-core": 91,
    "serialist-sim": 90,
    "serialist-vt": 90,
    "serialist-plugins": 86,
    "serialist-script": 85,
    "serialist-ui": 86,
    "serialist": 93,
    "serialist-plugin-sdk": 70,
}

# One directory under crates/ or examples/plugins/. A path from the report is absolute; the
# first such segment is the crate (the plugin test crates under
# crates/serialist-plugins/tests/plugins/ belong to serialist-plugins).
CRATE_DIR = re.compile(r"(?:^|/)(?:crates|examples/plugins)/([^/]+)/")


class BadInput(Exception):
    """The input is not what this script expects; the exit status is 2."""


@dataclass
class Lines:
    covered: int = 0
    count: int = 0

    def add(self, covered: int, count: int) -> None:
        self.covered += covered
        self.count += count

    @property
    def percent(self) -> float:
        return 100.0 if self.count == 0 else 100.0 * self.covered / self.count

    def meets(self, floor: float) -> bool:
        # Compared without dividing, so 87.0% exactly meets a floor of 87.
        return self.covered * 100 >= floor * self.count


def read_summary(path: Path) -> dict[str, Lines]:
    """Line counts by crate directory, plus the workspace total, from the JSON summary."""
    try:
        document = json.loads(path.read_text("utf-8"))
    except (OSError, ValueError) as err:
        raise BadInput(f"{path}: cannot read it as JSON ({err})") from err
    try:
        if document["type"] != "llvm.coverage.json.export":
            raise BadInput(f"{path}: not an llvm-cov export (type {document['type']!r})")
        (data,) = document["data"]
        files = data["files"]
        rows = [
            (str(file["filename"]).replace("\\", "/"), file["summary"]["lines"])
            for file in files
        ]
        counts = [(name, int(lines["covered"]), int(lines["count"])) for name, lines in rows]
    except BadInput:
        raise
    except (KeyError, TypeError, ValueError) as err:
        raise BadInput(
            f"{path}: not the JSON `cargo llvm-cov report --json --summary-only` writes ({err!r})"
        ) from err
    if not counts:
        raise BadInput(f"{path}: no files in the report (did the tests run?)")

    by_crate: dict[str, Lines] = {}
    total = Lines()
    for name, covered, count in counts:
        if not 0 <= covered <= count:
            raise BadInput(f"{name}: {covered} covered lines of {count}")
        match = CRATE_DIR.search(name)
        if match is None:
            raise BadInput(f"{name}: not under crates/ or examples/plugins/, so in no crate")
        by_crate.setdefault(match.group(1), Lines()).add(covered, count)
        total.add(covered, count)
    by_crate[WORKSPACE] = total
    return by_crate


@dataclass(frozen=True)
class Row:
    name: str
    lines: Lines | None
    floor: float | None

    @property
    def verdict(self) -> str:
        if self.floor is None:
            return "NO FLOOR"
        if self.lines is None:
            return "NO DATA"
        return "OK" if self.lines.meets(self.floor) else "FAIL"

    @property
    def ok(self) -> bool:
        return self.verdict == "OK"


def judge(measured: dict[str, Lines], floors: dict[str, float]) -> list[Row]:
    crates = sorted((set(measured) | set(floors)) - {WORKSPACE})
    return [Row(n, measured.get(n), floors.get(n)) for n in [WORKSPACE, *crates]]


def table(rows: list[Row]) -> list[tuple[str, str, str, str, str]]:
    out = []
    for row in rows:
        if row.lines is None:
            covered, percent = "-", "-"
        else:
            covered = f"{row.lines.covered}/{row.lines.count}"
            percent = f"{row.lines.percent:.2f}%"
        floor = "-" if row.floor is None else f"{row.floor:g}%"
        out.append((row.name, covered, percent, floor, row.verdict))
    return out


HEADER = ("crate", "lines covered/total", "percent", "floor", "verdict")


def render_text(cells: list[tuple[str, str, str, str, str]]) -> str:
    grid = [HEADER, *cells]
    widths = [max(len(line[i]) for line in grid) for i in range(5)]
    lines = []
    for line in grid:
        text = "  ".join(
            cell.ljust(widths[i]) if i in (0, 4) else cell.rjust(widths[i])
            for i, cell in enumerate(line)
        )
        lines.append(text.rstrip())
    return "\n".join(lines)


def render_markdown(cells: list[tuple[str, str, str, str, str]]) -> str:
    lines = [
        "### Line coverage by crate",
        "",
        "| crate | lines covered/total | percent | floor | verdict |",
        "| --- | ---: | ---: | ---: | --- |",
    ]
    for name, covered, percent, floor, verdict in cells:
        mark = verdict if verdict == "OK" else f"**{verdict}**"
        lines.append(f"| {name} | {covered} | {percent} | {floor} | {mark} |")
    return "\n".join(lines) + "\n"


def run(summary: Path, floors: dict[str, float]) -> int:
    rows = judge(read_summary(summary), floors)
    cells = table(rows)
    print(render_text(cells))
    step_summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if step_summary:
        with open(step_summary, "a", encoding="utf-8") as out:
            out.write(render_markdown(cells))

    failed = [row for row in rows if not row.ok]
    if not failed:
        print("\nEvery floor is met.")
        return 0
    print()
    for row in failed:
        if row.verdict == "FAIL":
            assert row.lines is not None and row.floor is not None
            message = f"{row.name}: {row.lines.percent:.2f}% of lines is under its floor of {row.floor:g}%"
        elif row.verdict == "NO FLOOR":
            message = f"{row.name}: in the report but has no floor; add it to FLOORS in {Path(__file__).name}"
        else:
            message = f"{row.name}: has a floor but is not in the report; drop it from FLOORS or run its tests"
        print(f"::error::{message}" if os.environ.get("GITHUB_ACTIONS") else f"FAILED: {message}")
    return 1


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(
        description="Fail when a crate's (or the workspace's) line coverage is under its floor.",
    )
    parser.add_argument(
        "summary",
        type=Path,
        help="the file `cargo llvm-cov report --json --summary-only --output-path` wrote",
    )
    args = parser.parse_args(argv)
    try:
        return run(args.summary, FLOORS)
    except BadInput as err:
        print(f"coverage_floors: {err}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
