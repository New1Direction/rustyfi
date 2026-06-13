#!/usr/bin/env python3
"""Behavioral-equivalence oracle for Rustyfi.

Builds a source project and its Rust translation, runs both against the same
fixture corpus, and diffs stdout / stderr / exit code. `cargo check` proves the
output is valid Rust; this proves it is the *same app*.

The source binary is ground truth — the target must match it. Differential
testing against a corpus: it verifies sameness on the cases exercised, not
equivalence in general.

Usage:
    python3 behavior/check.py [SPEC]          # default: examples/calculator/behavior.yaml
    python3 behavior/check.py SPEC --json      # machine-readable report to stdout
    python3 behavior/check.py SPEC --report PATH

Exit code: 0 if every case matches, 1 on any behavioral mismatch, 2 on a
build/setup failure.
"""
import argparse
import json
import os
import subprocess
import sys
from dataclasses import asdict, dataclass
from pathlib import Path

import yaml

DEFAULT_SPEC = "examples/calculator/behavior.yaml"
MAX_CAPTURE = 4000  # chars retained per stream in the report
RUN_TIMEOUT = 30  # seconds per binary invocation


@dataclass(frozen=True)
class Outcome:
    """What one binary did on one case."""

    stdout: str
    stderr: str
    exit_code: int


def expand(cmd: list[str], work: Path) -> list[str]:
    """Substitute the {work} scratch-dir placeholder in a command vector."""
    return [part.replace("{work}", str(work)) for part in cmd]


def build(side: dict, label: str, root: Path, work: Path) -> None:
    """Run a side's build command; raise RuntimeError with output on failure."""
    cwd = root / side["dir"]
    cmd = expand(side["build"], work)
    proc = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True)
    if proc.returncode != 0:
        raise RuntimeError(
            f"{label} build failed ({' '.join(cmd)}):\n"
            f"{proc.stdout}\n{proc.stderr}".strip()
        )


def run_case(side: dict, case: dict, root: Path, work: Path) -> Outcome:
    """Invoke one side's binary for one case and capture its behavior."""
    cwd = root / side["dir"]
    cmd = expand(side["run"], work) + list(case.get("args", []))
    env = {**os.environ, **case.get("env", {})}
    try:
        proc = subprocess.run(
            cmd,
            cwd=cwd,
            input=case.get("stdin", ""),
            capture_output=True,
            text=True,
            env=env,
            timeout=RUN_TIMEOUT,
        )
    except subprocess.TimeoutExpired:
        return Outcome(stdout="", stderr=f"<timeout after {RUN_TIMEOUT}s>", exit_code=124)
    return Outcome(stdout=proc.stdout, stderr=proc.stderr, exit_code=proc.returncode)


def diff_case(source: Outcome, target: Outcome, compare: dict) -> list[str]:
    """Return a list of human-readable mismatch lines (empty == behaviors match)."""
    diffs: list[str] = []
    for stream in ("stdout", "stderr"):
        if compare.get(stream, "exact") != "exact":
            continue
        want, got = getattr(source, stream), getattr(target, stream)
        if want != got:
            diffs.append(f"{stream}: source={want!r} target={got!r}")
    if compare.get("exit_code", "exact") == "exact":
        if source.exit_code != target.exit_code:
            diffs.append(f"exit_code: source={source.exit_code} target={target.exit_code}")
    return diffs


def _clip(outcome: Outcome) -> dict:
    return {
        "stdout": outcome.stdout[:MAX_CAPTURE],
        "stderr": outcome.stderr[:MAX_CAPTURE],
        "exit_code": outcome.exit_code,
    }


def evaluate(spec: dict, root: Path, work: Path) -> dict:
    """Build both sides, run every case, and assemble the report."""
    build(spec["source"], "source", root, work)
    build(spec["target"], "target", root, work)

    default_compare = spec.get("compare", {})
    results = []
    for case in spec["cases"]:
        compare = {**default_compare, **case.get("compare", {})}
        source = run_case(spec["source"], case, root, work)
        target = run_case(spec["target"], case, root, work)
        diffs = diff_case(source, target, compare)
        results.append(
            {
                "name": case["name"],
                "match": not diffs,
                "diffs": diffs,
                "source": _clip(source),
                "target": _clip(target),
            }
        )

    matched = sum(1 for r in results if r["match"])
    return {
        "name": spec.get("name", "unnamed"),
        "passed": matched == len(results),
        "matched": matched,
        "total": len(results),
        "cases": results,
    }


def print_summary(report: dict) -> None:
    """Human-readable scoreboard to stderr (stdout stays clean for --json)."""
    print(f"\nbehavior: {report['name']}  ({report['matched']}/{report['total']} matched)\n", file=sys.stderr)
    for case in report["cases"]:
        mark = "✓" if case["match"] else "✗"
        print(f"  {mark} {case['name']}", file=sys.stderr)
        for line in case["diffs"]:
            print(f"      {line}", file=sys.stderr)
    verdict = "PASS — behaviors match" if report["passed"] else "FAIL — behavioral divergence"
    print(f"\n{verdict}\n", file=sys.stderr)


def main() -> int:
    parser = argparse.ArgumentParser(description="Behavioral-equivalence oracle.")
    parser.add_argument("spec", nargs="?", default=DEFAULT_SPEC, help="path to a behavior.yaml")
    parser.add_argument("--json", action="store_true", help="emit the report as JSON to stdout")
    parser.add_argument("--report", help="also write the JSON report to this path")
    args = parser.parse_args()

    root = Path.cwd()
    spec_path = Path(args.spec)
    if not spec_path.is_file():
        print(f"error: spec not found: {spec_path}", file=sys.stderr)
        return 2
    spec = yaml.safe_load(spec_path.read_text())

    work = root / "behavior" / ".work"
    work.mkdir(parents=True, exist_ok=True)

    try:
        report = evaluate(spec, root, work)
    except RuntimeError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2

    report_path = Path(args.report) if args.report else spec_path.parent / "behavior_report.json"
    report_path.write_text(json.dumps(report, indent=2) + "\n")

    if args.json:
        print(json.dumps(report, indent=2))
    else:
        print_summary(report)
        print(f"report: {report_path}", file=sys.stderr)

    return 0 if report["passed"] else 1


if __name__ == "__main__":
    sys.exit(main())
