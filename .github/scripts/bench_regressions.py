#!/usr/bin/env python3
"""Flag criterion benches that got slower than a saved baseline.

The CI bench job on a pull request runs every bench at the base commit with
`--save-baseline base`, then at the head with `--baseline-lenient base`. Criterion then
writes, next to each bench's saved runs, a `change/estimates.json` with the relative
change of the head against the base. This script reads those files. See the `bench`
job in .github/workflows/ci.yml, and `just bench-compare` for the same thing locally.

    bench_regressions.py <criterion_home> [--threshold 0.30] [--write-ids FILE]
                         [--only FILE] [--self-test]

Layout: criterion 0.5 writes `<home>/<group>/<bench>/{base,new,change}/`, and the bench
id is the path between the home and `change` (`parse/overwrite_non_ascii`,
`firehose/generate/Mixed`). When `new/benchmark.json` is there, its `full_id` is used
instead; it is the same string unless the id has characters that are not safe in a file
name, and it is what `cargo bench -- --exact <id>` matches.

What counts: the change in the *median* time, `median.point_estimate` (0.25 means 25%
slower) with its confidence interval. A bench regressed when the interval's lower bound is
above the threshold: it is confidently at least that much slower. A bench whose point
estimate is past the threshold but whose interval reaches below it is shown as `noisy`
and does not count. Hosted runners are noisy, so the default threshold is a generous
30% (`BENCH_REGRESSION_THRESHOLD` or `--threshold` change it), and the workflow measures
a regressed bench a second time before it fails.

`--write-ids FILE` writes the regressed ids, one per line, for that second round, and
`--only FILE` looks at just the ids listed in a file of that form.

The table goes to stdout, and to $GITHUB_STEP_SUMMARY (as Markdown) when that is set.

Exit status: 0 nothing regressed (or no change files to look at), 1 something regressed,
2 the input is not what this expects: an unreadable estimates file, an id from `--only`
with no change file, a missing criterion home, bad arguments.

Python 3, standard library only. `--self-test` checks the logic on a made-up tree.
"""

from __future__ import annotations

import argparse
import contextlib
import io
import json
import math
import os
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path
from typing import Mapping, Sequence

DEFAULT_THRESHOLD = 0.30
THRESHOLD_ENV = "BENCH_REGRESSION_THRESHOLD"


class BadInput(Exception):
    """The input is not what this script expects; the exit status is 2."""


@dataclass(frozen=True)
class Change:
    """The change in one bench's median time, head against base (0.25 = 25% slower)."""

    id: str
    median: float
    lower: float
    upper: float

    def verdict(self, threshold: float) -> str:
        if self.lower > threshold:
            return "REGRESSED"
        if self.median > threshold:
            return "noisy"  # slower on paper, but the interval reaches below the threshold
        if self.upper < 0:
            return "faster"
        return "ok"


def bench_id(bench_dir: Path, home: Path) -> str:
    path_id = bench_dir.relative_to(home).as_posix()
    try:
        full_id = json.loads((bench_dir / "new" / "benchmark.json").read_text("utf-8"))["full_id"]
    except (OSError, ValueError, KeyError, TypeError):
        return path_id
    return full_id if isinstance(full_id, str) and full_id else path_id


def load_change(bench: str, path: Path) -> Change:
    try:
        median = json.loads(path.read_text("utf-8"))["median"]
        point = float(median["point_estimate"])
        interval = median["confidence_interval"]
        lower = float(interval["lower_bound"])
        upper = float(interval["upper_bound"])
    except (OSError, ValueError, KeyError, TypeError) as err:
        raise BadInput(f"{path}: not a criterion change estimate ({err!r})") from err
    if not all(math.isfinite(v) for v in (point, lower, upper)):
        raise BadInput(f"{path}: the median change is not a finite number")
    return Change(bench, point, lower, upper)


def collect(home: Path) -> list[Change]:
    if not home.is_dir():
        raise BadInput(f"{home}: no such criterion directory (did the benches run?)")
    return [
        load_change(bench_id(path.parent.parent, home), path)
        for path in sorted(home.rglob("change/estimates.json"))
    ]


def read_ids(path: Path) -> list[str]:
    try:
        lines = path.read_text("utf-8").splitlines()
    except OSError as err:
        raise BadInput(f"{path}: {err}") from err
    return [line.strip() for line in lines if line.strip()]


def pct(fraction: float) -> str:
    return f"{fraction * 100:+.1f}%"


def rows_for(changes: Sequence[Change], threshold: float) -> list[tuple[str, str, str, str]]:
    ordered = sorted(changes, key=lambda c: (-c.median, c.id))
    return [
        (c.id, pct(c.median), f"[{pct(c.lower)}, {pct(c.upper)}]", c.verdict(threshold))
        for c in ordered
    ]


def render_text(rows: Sequence[tuple[str, str, str, str]]) -> str:
    table = [("bench", "change", "interval", "verdict"), *rows]
    widths = [max(len(row[i]) for row in table) for i in range(4)]
    lines = []
    for id_, change, interval, verdict in table:
        lines.append(
            f"{id_:<{widths[0]}}  {change:>{widths[1]}}  {interval:<{widths[2]}}  {verdict}".rstrip()
        )
    return "\n".join(lines)


def render_markdown(rows: Sequence[tuple[str, str, str, str]], threshold: float) -> str:
    lines = [
        "### Bench comparison with the base commit",
        "",
        f"A bench regressed when it is confidently more than {pct(threshold)} slower: "
        "the lower bound of the confidence interval on the change in its median time is "
        f"above {pct(threshold)}.",
        "",
        "| bench | change | interval | verdict |",
        "| --- | ---: | --- | --- |",
    ]
    for id_, change, interval, verdict in rows:
        verdict = f"**{verdict}**" if verdict == "REGRESSED" else verdict
        lines.append(f"| `{id_}` | {change} | {interval} | {verdict} |")
    return "\n".join(lines) + "\n"


def append_summary(environ: Mapping[str, str], text: str) -> None:
    target = environ.get("GITHUB_STEP_SUMMARY")
    if not target:
        return
    try:
        with open(target, "a", encoding="utf-8") as summary:
            summary.write(text + "\n")
    except OSError as err:  # A summary is a courtesy; never let it decide the verdict.
        print(f"warning: could not write {target}: {err}", file=sys.stderr)


def notice(environ: Mapping[str, str], message: str) -> None:
    if environ.get("GITHUB_ACTIONS") == "true":
        print(f"::notice::{message}")
    else:
        print(f"notice: {message}")


def parse_threshold(text: str, source: str) -> float:
    try:
        value = float(text)
    except ValueError:
        raise argparse.ArgumentTypeError(f"{source}: {text!r} is not a number") from None
    if not math.isfinite(value) or value <= 0:
        raise argparse.ArgumentTypeError(f"{source}: the threshold must be above 0, not {text}")
    return value


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="bench_regressions.py",
        description="Flag criterion benches that regressed against a saved baseline.",
        epilog="Exit status: 0 nothing regressed, 1 a regression, 2 unexpected input.",
    )
    parser.add_argument("criterion_home", nargs="?", type=Path, help="criterion's output directory")
    parser.add_argument(
        "--threshold",
        metavar="FRACTION",
        help=f"regression threshold, 0.30 = 30%% slower (default ${THRESHOLD_ENV}, else {DEFAULT_THRESHOLD})",
    )
    parser.add_argument("--write-ids", type=Path, metavar="FILE", help="write the regressed ids here")
    parser.add_argument("--only", type=Path, metavar="FILE", help="look only at the ids in this file")
    parser.add_argument("--self-test", action="store_true", help="check this script and exit")
    return parser


def main(argv: Sequence[str] | None = None, environ: Mapping[str, str] | None = None) -> int:
    environ = os.environ if environ is None else environ
    parser = build_parser()
    args = parser.parse_args(argv)
    if args.self_test:
        return self_test()
    if args.criterion_home is None:
        parser.error("the criterion home directory is required")
    try:
        if args.threshold is not None:
            threshold = parse_threshold(args.threshold, "--threshold")
        elif environ.get(THRESHOLD_ENV):
            threshold = parse_threshold(environ[THRESHOLD_ENV], f"${THRESHOLD_ENV}")
        else:
            threshold = DEFAULT_THRESHOLD
    except argparse.ArgumentTypeError as err:
        parser.error(str(err))

    try:
        changes = collect(args.criterion_home)
        if args.only is not None:
            wanted = read_ids(args.only)
            wanted_set = set(wanted)
            missing = [id_ for id_ in wanted if id_ not in {c.id for c in changes}]
            if missing:
                raise BadInput(
                    f"no change file for {', '.join(missing)}, listed in {args.only}: "
                    "did the re-run measure it?"
                )
            changes = [c for c in changes if c.id in wanted_set]
    except BadInput as err:
        print(f"error: {err}", file=sys.stderr)
        return 2

    if not changes:
        message = (
            f"no change files under {args.criterion_home}: nothing was measured against a "
            "baseline, so there is nothing to compare"
        )
        notice(environ, message)
        append_summary(environ, f"### Bench comparison with the base commit\n\n{message}.\n")
        if args.write_ids is not None:
            args.write_ids.write_text("", encoding="utf-8")
        return 0

    rows = rows_for(changes, threshold)
    print(render_text(rows))
    append_summary(environ, render_markdown(rows, threshold))
    regressed = [row[0] for row in rows if row[3] == "REGRESSED"]
    if args.write_ids is not None:
        args.write_ids.write_text("".join(f"{id_}\n" for id_ in regressed), encoding="utf-8")
    if regressed:
        print(f"\n{len(regressed)} of {len(rows)} benches regressed past {pct(threshold)}.")
        return 1
    print(f"\nNo bench regressed past {pct(threshold)} ({len(rows)} compared).")
    return 0


# --- self-test -------------------------------------------------------------------------


def _write_change(home: Path, id_: str, median: float, lower: float, upper: float) -> None:
    """A change/estimates.json shaped like criterion 0.5's. The mean is made to disagree
    with the median on purpose: only the median may decide."""

    def estimate(point: float, low: float, high: float) -> dict[str, object]:
        return {
            "confidence_interval": {
                "confidence_level": 0.95,
                "lower_bound": low,
                "upper_bound": high,
            },
            "point_estimate": point,
            "standard_error": 0.01,
        }

    directory = home / id_ / "change"
    directory.mkdir(parents=True)
    (directory / "estimates.json").write_text(
        json.dumps(
            {
                "mean": estimate(0.9, 0.8, 1.0),
                "median": estimate(median, lower, upper),
            }
        ),
        encoding="utf-8",
    )


def self_test() -> int:
    failures: list[str] = []

    def expect(label: str, got: object, want: object) -> None:
        if got != want:
            failures.append(f"{label}: got {got!r}, want {want!r}")

    def run(args: Sequence[object], env: Mapping[str, str] | None = None) -> tuple[int, str]:
        out = io.StringIO()
        try:
            with contextlib.redirect_stdout(out), contextlib.redirect_stderr(io.StringIO()):
                code = main([str(a) for a in args], env or {})
        except SystemExit as exit_:  # argparse's own errors
            code = exit_.code if isinstance(exit_.code, int) else 2
        return code, out.getvalue()

    def verdicts(output: str) -> dict[str, str]:
        rows = [line.split() for line in output.splitlines() if "[" in line and "]" in line]
        return {row[0]: row[-1] for row in rows}

    def order(output: str) -> list[str]:
        return list(verdicts(output))

    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        home = root / "criterion"
        _write_change(home, "firehose/generate/Mixed", 0.45, 0.38, 0.52)  # confidently slower
        _write_change(home, "parse/improved", -0.20, -0.25, -0.15)
        _write_change(home, "parse/noisy", 0.40, -0.10, 0.90)  # slower on paper, interval too wide
        _write_change(home, "parse/unchanged", 0.005, -0.01, 0.02)
        regressed = "firehose/generate/Mixed"

        code, out = run([home])
        expect("mixed tree: exit", code, 1)
        expect(
            "mixed tree: verdicts",
            verdicts(out),
            {
                regressed: "REGRESSED",
                "parse/noisy": "noisy",
                "parse/unchanged": "ok",
                "parse/improved": "faster",
            },
        )
        expect(
            "mixed tree: sorted by change, worst first",
            order(out),
            [regressed, "parse/noisy", "parse/unchanged", "parse/improved"],
        )

        ids = root / "regressed.txt"
        run([home, "--write-ids", ids])
        expect("write-ids: contents", ids.read_text("utf-8"), f"{regressed}\n")

        listed = root / "only.txt"
        listed.write_text("parse/noisy\n\nparse/unchanged\n", encoding="utf-8")
        code, out = run([home, "--only", listed])
        expect("only, nothing regressed: exit", code, 0)
        expect("only, nothing regressed: rows", sorted(verdicts(out)), ["parse/noisy", "parse/unchanged"])
        listed.write_text(f"{regressed}\n", encoding="utf-8")
        expect("only, the regressed one: exit", run([home, "--only", listed])[0], 1)
        listed.write_text("parse/not_measured\n", encoding="utf-8")
        expect("only, id with no change file: exit", run([home, "--only", listed])[0], 2)
        expect("only, no such file: exit", run([home, "--only", root / "absent.txt"])[0], 2)

        # The threshold: 0.38 is the regressed bench's lower bound.
        expect("threshold 0.40: exit", run([home, "--threshold", "0.40"])[0], 0)
        expect("threshold 0.37: exit", run([home, "--threshold", "0.37"])[0], 1)
        env = {THRESHOLD_ENV: "0.40"}
        expect("threshold from the environment: exit", run([home], env)[0], 0)
        expect("the flag beats the environment: exit", run([home, "--threshold", "0.30"], env)[0], 1)
        for bad in ("0", "-0.1", "abc", "nan"):
            expect(f"threshold {bad}: exit", run([home, "--threshold", bad])[0], 2)
        expect("threshold from a bad environment: exit", run([home], {THRESHOLD_ENV: "x"})[0], 2)

        summary = root / "summary.md"
        run([home], {"GITHUB_STEP_SUMMARY": str(summary)})
        text = summary.read_text("utf-8")
        expect("step summary: table", "| `firehose/generate/Mixed` | +45.0% |" in text, True)
        expect("step summary: verdict", "**REGRESSED**" in text, True)

        # Exactly at the threshold is not past it.
        edge = root / "edge"
        _write_change(edge, "append/plain", 0.31, 0.30, 0.32)
        expect("lower bound equal to the threshold: exit", run([edge])[0], 0)

        # Nothing regressed.
        quiet = root / "quiet"
        _write_change(quiet, "parse/improved", -0.20, -0.25, -0.15)
        _write_change(quiet, "parse/noisy", 0.40, -0.10, 0.90)
        ids.write_text("stale\n", encoding="utf-8")
        code, out = run([quiet, "--write-ids", ids])
        expect("no regression: exit", code, 0)
        expect("no regression: ids file emptied", ids.read_text("utf-8"), "")

        # Nothing to compare: not a failure, but not silent.
        empty = root / "empty"
        (empty / "append" / "plain" / "new").mkdir(parents=True)  # a bench new in the PR
        code, out = run([empty])
        expect("no change files: exit", code, 0)
        expect("no change files: notice", out.startswith("notice: "), True)
        code, out = run([empty], {"GITHUB_ACTIONS": "true"})
        expect("no change files, on GitHub: annotation", out.startswith("::notice::"), True)
        expect("missing criterion home: exit", run([root / "absent"])[0], 2)

        # Input that is not criterion's.
        broken = root / "broken"
        (broken / "append" / "plain" / "change").mkdir(parents=True)
        estimates = broken / "append" / "plain" / "change" / "estimates.json"
        for label, content in (
            ("not JSON", "{"),
            ("no median", "{}"),
            ("null estimate", '{"median": {"point_estimate": null, "confidence_interval": {}}}'),
            ("NaN estimate", '{"median": {"point_estimate": NaN, "confidence_interval": '
             '{"lower_bound": 0, "upper_bound": 0}}}'),
        ):
            estimates.write_text(content, encoding="utf-8")
            expect(f"unreadable estimates ({label}): exit", run([broken])[0], 2)
        expect("no arguments: exit", run([])[0], 2)

        # The id comes from new/benchmark.json when it is there (ids with characters that
        # are not safe in a file name get another directory name).
        odd = root / "odd"
        _write_change(odd, "weird_name", 0.01, 0.0, 0.02)
        (odd / "weird_name" / "new").mkdir()
        (odd / "weird_name" / "new" / "benchmark.json").write_text(
            json.dumps({"full_id": "weird:name"}), encoding="utf-8"
        )
        expect("full_id from benchmark.json", order(run([odd])[1]), ["weird:name"])

    if failures:
        print("self-test FAILED:", file=sys.stderr)
        for failure in failures:
            print(f"  {failure}", file=sys.stderr)
        return 1
    print("self-test passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
