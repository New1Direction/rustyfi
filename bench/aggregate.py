#!/usr/bin/env python3
"""Aggregate per-repo benchmark JSON into bench/RESULTS.md.

Usage:  aggregate.py <repos.toml> <results-dir>      # markdown to stdout
        aggregate.py --self-test                     # assert against bench/testdata
Headline metrics (spec: success criteria):
  clean-rate  = clean / achievable          (achievable = expectation != "impossible")
  median      = median errors across ALL repos with a result
  hard case   = prompt-cache errors (when present)
"""
import json
import statistics
import sys
from pathlib import Path

try:
    import tomllib
except ImportError:  # python < 3.11 — bench/run.sh preflights tomli's presence
    import tomli as tomllib

VERDICT = {True: "🟢 clean", False: "🟠 partial"}


def load(repos_toml: Path, results_dir: Path) -> list[dict]:
    with open(repos_toml, "rb") as f:
        manifest = tomllib.load(f)["repo"]
    rows = []
    for repo in manifest:
        path = results_dir / f"{repo['name']}.json"
        result = json.loads(path.read_text()) if path.exists() else None
        rows.append({"meta": repo, "result": result})
    return rows


def headline(rows: list[dict]) -> dict:
    # A pipeline failure on an achievable repo counts against the clean rate
    # (denominator includes it, numerator can't) — failures are not exclusions.
    achievable = [r for r in rows if r["meta"]["expectation"] != "impossible" and r["result"]]
    # median is computed over runs that produced a crate (failure stubs have no error count)
    ran = [r for r in rows if r["result"] and not r["result"].get("pipeline_failed")]
    clean = [r for r in achievable if r["result"].get("cargo_clean")]
    errors = [r["result"]["errors"] for r in ran]
    hard = next((r["result"]["errors"] for r in rows
                 if r["meta"]["name"] == "prompt-cache" and r["result"]
                 and not r["result"].get("pipeline_failed")), None)
    return {
        "clean_rate": (len(clean), len(achievable)),
        "median_errors": statistics.median(errors) if errors else None,
        "prompt_cache_errors": hard,
    }


def render(rows: list[dict]) -> str:
    h = headline(rows)
    done, total = h["clean_rate"]
    pct = f"{100 * done / total:.0f}%" if total else "n/a"
    out = ["# rustyfi benchmark results", ""]
    out.append(f"**Clean rate (achievable):** {done}/{total} ({pct}) · "
               f"**median errors:** {h['median_errors']} · "
               f"**prompt-cache:** {h['prompt_cache_errors']} errors")
    out += ["", "| repo | lang | expectation | verdict | errors | todos | files | secs |",
            "|---|---|---|---|---|---|---|---|"]
    for r in rows:
        m, res = r["meta"], r["result"]
        if res is None:
            verdict, errs, todos, files, secs = "⚪ not run", "—", "—", "—", "—"
        elif res.get("pipeline_failed"):
            verdict, errs, todos, files, secs = "🔴 pipeline failed", "—", "—", "—", "—"
        else:
            verdict = VERDICT[bool(res["cargo_clean"])]
            errs, todos = res["errors"], res["todos"]
            files = f"{res['files_translated']}/{res['files_total']}"
            secs = f"{res['duration_secs']:.0f}"
        out.append(f"| {m['name']} | {m['language']} | {m['expectation']} "
                   f"| {verdict} | {errs} | {todos} | {files} | {secs} |")
    out += ["", "_impossible repos are shown but excluded from the clean-rate denominator._", ""]
    return "\n".join(out)


def self_test() -> None:
    base = Path(__file__).parent / "testdata"
    rows = load(base / "repos.toml", base)
    h = headline(rows)
    assert h["clean_rate"] == (1, 3), h
    assert h["median_errors"] == 7, h
    assert h["prompt_cache_errors"] is None, h
    md = render(rows)
    assert "1/3 (33%)" in md, md
    assert "🟢 clean" in md and "🟠 partial" in md, md
    assert "🔴 pipeline failed" in md, md
    print("self-test: OK")


if __name__ == "__main__":
    if sys.argv[1:] == ["--self-test"]:
        self_test()
    elif len(sys.argv) == 3:
        print(render(load(Path(sys.argv[1]), Path(sys.argv[2]))))
    else:
        sys.exit(__doc__)
