# Benchmark Suite + CLI `--json` Implementation Plan (Compile-Clean 10x, Phase 1)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A reproducible ~10-repo benchmark suite with a machine-readable CLI, producing a baseline `bench/RESULTS.md` scoreboard before any pipeline improvement lands.

**Architecture:** The CLI gains a `--json` flag that prints one summary object to stdout. A pinned-commit manifest (`bench/repos.toml`) drives `bench/run.sh`, which runs every repo through the release CLI and saves per-repo JSON; a python3 aggregator turns those into `bench/RESULTS.md` with the three headline metrics (% clean among achievable, median errors, prompt-cache count).

**Tech Stack:** Rust (clap, serde_json), POSIX sh, python3 ≥3.11 (stdlib `tomllib`/`json`/`statistics` only).

**Spec:** `docs/superpowers/specs/2026-06-11-compile-clean-10x-design.md` (section "1. Benchmark suite").

**Conventions for every task:** run commands from the repo root `rustyfi/`. After each implementation step, `cargo fmt --all` must leave no diff and `cargo clippy --workspace --all-targets -- -D warnings` must pass before committing. Commit messages use `<type>: <description>` with no attribution footer.

---

### Task 1: CLI `--json` summary

**Files:**
- Modify: `crates/rustyfi-cli/Cargo.toml` (add serde_json)
- Modify: `crates/rustyfi-cli/src/main.rs` (flag + summary builder + wiring)

- [ ] **Step 1.1: Add serde_json to the CLI crate**

In `crates/rustyfi-cli/Cargo.toml`, under `[dependencies]`, add:

```toml
serde_json = { workspace = true }
```

(The workspace already pins `serde_json = "1"` in the root `Cargo.toml`.)

- [ ] **Step 1.2: Write the failing test**

Append inside the existing `#[cfg(test)] mod tests` at the bottom of `crates/rustyfi-cli/src/main.rs`:

```rust
    #[test]
    fn json_summary_has_all_contract_fields() {
        let r = RunResult {
            zip: vec![],
            crate_name: "demo".into(),
            language: "go".into(),
            files_failed: 1,
            cargo_clean: false,
            error_count: 42,
            todo_count: 12,
            files_translated: 23,
        };
        let v = build_json_summary(&r, Path::new("/out/demo-rust"), 13, 271.5, "deepseek-chat", "deepseek-reasoner");
        assert_eq!(v["crate_name"], "demo");
        assert_eq!(v["crate_path"], "/out/demo-rust");
        assert_eq!(v["language"], "go");
        assert_eq!(v["files_total"], 24); // translated + failed
        assert_eq!(v["files_translated"], 23);
        assert_eq!(v["files_failed"], 1);
        assert_eq!(v["files_written"], 13);
        assert_eq!(v["errors"], 42);
        assert_eq!(v["todos"], 12);
        assert_eq!(v["cargo_clean"], false);
        assert_eq!(v["duration_secs"], 271.5);
        assert_eq!(v["translate_model"], "deepseek-chat");
        assert_eq!(v["fix_model"], "deepseek-reasoner");
        assert_eq!(v["exit_code"], 1);
    }
```

- [ ] **Step 1.3: Run the test to verify it fails**

Run: `cargo test -p rustyfi-cli json_summary -- --nocapture`
Expected: COMPILE ERROR — `build_json_summary` not found.

- [ ] **Step 1.4: Implement `build_json_summary`**

Add to `crates/rustyfi-cli/src/main.rs` (below `print_summary`, above the `env / preflight` section):

```rust
/// One-line machine-readable run summary (the `--json` contract).
/// Schema documented in bench/README later; exit_code mirrors the process exit.
fn build_json_summary(
    r: &RunResult,
    output: &Path,
    files_written: usize,
    duration_secs: f64,
    translate_model: &str,
    fix_model: &str,
) -> serde_json::Value {
    serde_json::json!({
        "crate_name": r.crate_name,
        "crate_path": output.display().to_string(),
        "language": r.language,
        "files_total": r.files_translated + r.files_failed,
        "files_translated": r.files_translated,
        "files_failed": r.files_failed,
        "files_written": files_written,
        "errors": r.error_count,
        "todos": r.todo_count,
        "cargo_clean": r.cargo_clean,
        "duration_secs": duration_secs,
        "translate_model": translate_model,
        "fix_model": fix_model,
        "exit_code": if r.cargo_clean { 0 } else { 1 },
    })
}
```

- [ ] **Step 1.5: Run the test to verify it passes**

Run: `cargo test -p rustyfi-cli json_summary`
Expected: PASS (1 passed).

- [ ] **Step 1.6: Wire the flag into `Cli` and `real_main`**

In the `Cli` struct, after the `fresh` field, add:

```rust
    /// Print a machine-readable JSON summary to stdout (implies --quiet).
    #[arg(long)]
    json: bool,
}
```

In `real_main`, make three changes:

(a) the progress display also goes quiet under `--json` — replace the `rich` line:

```rust
    let rich = !cli.quiet && !cli.json && std::io::stderr().is_terminal();
```

(b) time the run — immediately before `let outcome = run(config, |p| ui.handle(&p));` add:

```rust
    let started = std::time::Instant::now();
```

(c) replace the tail of `real_main` (from `print_summary(&result, &output, files);` through the final `Ok(...)`) with:

```rust
    if cli.json {
        let translate_model = rustyfi_engine::llm::LlmClient::build()
            .map(|c| c.model().to_string())
            .unwrap_or_else(|_| "unknown".into());
        let fix_model = rustyfi_engine::llm::LlmClient::for_fixing()
            .map(|c| c.model().to_string())
            .unwrap_or_else(|_| translate_model.clone());
        let summary = build_json_summary(
            &result,
            &output,
            files,
            started.elapsed().as_secs_f64(),
            &translate_model,
            &fix_model,
        );
        println!("{summary}");
    } else {
        print_summary(&result, &output, files);
    }
    Ok(if result.cargo_clean {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
```

Note: with `--json` the crate path moves *inside* the JSON (`crate_path`); the bare-path stdout line only prints in the human mode (it lives in `print_summary`'s `println!`). That is intentional — `--json` consumers parse one object.

- [ ] **Step 1.7: Full check**

Run: `cargo test -p rustyfi-cli && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all --check`
Expected: tests pass, clippy clean, no fmt diff.

- [ ] **Step 1.8: Smoke-test against the in-repo example (needs `RUSTYFI_LLM_API_KEY` set)**

Run: `cargo build --release -p rustyfi-cli && ./target/release/rustyfi examples/calculator -o /tmp/calc-json-test --fresh --json | python3 -m json.tool`
Expected: pretty-printed JSON with `"cargo_clean": true`, `"exit_code": 0`. (Skip this step if no key is configured; Task 5 exercises it fully.)

- [ ] **Step 1.9: Commit**

```bash
git add crates/rustyfi-cli/Cargo.toml crates/rustyfi-cli/src/main.rs Cargo.lock
git commit -m "feat: --json machine-readable summary on the CLI"
```

---

### Task 2: Benchmark manifest `bench/repos.toml`

**Files:**
- Create: `bench/repos.toml`
- Modify: `.gitignore` (ignore `bench/.work/`)

- [ ] **Step 2.1: Pin candidate commits**

For each remote candidate below, resolve HEAD at execution time:

```bash
for r in messkan/prompt-cache tidwall/pretty pallets/itsdangerous lukeed/clsx janlelis/paint vdurmont/emoji-java vanhauser-thc/thc-hydra; do
  echo "$r $(git ls-remote https://github.com/$r HEAD | cut -f1)"
done
```

Expected: one `owner/repo <40-hex-sha>` line each. Record the SHAs for Step 2.2.

Verify each candidate against the spec's selection criteria (5–60 source files, single dominant language, permissive license, pure-logic deps). Check file count with:

```bash
gh repo view <owner/repo> --json licenseInfo -q .licenseInfo.spdxId
```

and after Task 3's first clone, `find bench/.work/src/<name> -name "*.<ext>" | wc -l`. If a candidate fails criteria (too big, wrong license), substitute a similar-sized repo in the same language and note the swap in the commit message. Target: ≥8 achievable repos + 1 `impossible` + the local example.

- [ ] **Step 2.2: Write the manifest**

Create `bench/repos.toml` (SHAs below are placeholders by necessity — use the real ones from Step 2.1; `pinned_commit = "local"` means an in-repo path, no clone):

```toml
# rustyfi benchmark suite — pinned real-world repos.
# expectation: clean      → must compile clean for the suite to count it
#              partial    → real app, gaps acceptable, counted in median
#              impossible → native-C-dependency heavy; reported, excluded from clean-rate denominator

[[repo]]
name = "calculator"
source = "examples/calculator"
pinned_commit = "local"
language = "go"
size_tier = "small"
expectation = "clean"

[[repo]]
name = "prompt-cache"
source = "https://github.com/messkan/prompt-cache"
pinned_commit = "<sha-from-step-2.1>"
language = "go"
size_tier = "medium"
expectation = "clean"

[[repo]]
name = "pretty"
source = "https://github.com/tidwall/pretty"
pinned_commit = "<sha-from-step-2.1>"
language = "go"
size_tier = "small"
expectation = "clean"

[[repo]]
name = "itsdangerous"
source = "https://github.com/pallets/itsdangerous"
pinned_commit = "<sha-from-step-2.1>"
language = "python"
size_tier = "small"
expectation = "clean"

[[repo]]
name = "clsx"
source = "https://github.com/lukeed/clsx"
pinned_commit = "<sha-from-step-2.1>"
language = "javascript"
size_tier = "small"
expectation = "clean"

[[repo]]
name = "paint"
source = "https://github.com/janlelis/paint"
pinned_commit = "<sha-from-step-2.1>"
language = "ruby"
size_tier = "small"
expectation = "clean"

[[repo]]
name = "emoji-java"
source = "https://github.com/vdurmont/emoji-java"
pinned_commit = "<sha-from-step-2.1>"
language = "java"
size_tier = "medium"
expectation = "partial"

[[repo]]
name = "thc-hydra"
source = "https://github.com/vanhauser-thc/thc-hydra"
pinned_commit = "<sha-from-step-2.1>"
language = "c"
size_tier = "large"
expectation = "impossible"
```

Plus two more achievable repos chosen in Step 2.1 (one TS, one Go/Python — same entry shape) to reach ~10.

- [ ] **Step 2.3: Ignore the work directory**

Append to `.gitignore`:

```gitignore
# Benchmark suite scratch space (clones + generated crates + per-run JSON)
bench/.work/
```

- [ ] **Step 2.4: Commit**

```bash
git add bench/repos.toml .gitignore
git commit -m "feat: pinned benchmark-suite manifest (bench/repos.toml)"
```

---

### Task 3: Runner `bench/run.sh`

**Files:**
- Create: `bench/run.sh` (mode `755`)

- [ ] **Step 3.1: Write the runner**

Create `bench/run.sh`:

```sh
#!/bin/sh
# Run the rustyfi benchmark suite: every repo in bench/repos.toml through the
# release CLI, one JSON result per repo into bench/.work/results/, then aggregate.
#
#   bench/run.sh                  # full suite
#   bench/run.sh prompt-cache     # single repo by name
#   bench/run.sh --aggregate-only # rebuild RESULTS.md from existing JSON
set -eu

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BENCH="$ROOT/bench"
WORK="$BENCH/.work"
RESULTS="$WORK/results"
ONLY="${1:-}"

say() { printf '\033[1;33m[bench]\033[0m %s\n' "$1" >&2; }

[ "$ONLY" = "--aggregate-only" ] || {
  [ -n "${RUSTYFI_LLM_API_KEY:-}" ] || { say "RUSTYFI_LLM_API_KEY not set"; exit 2; }
  say "building release CLI"
  cargo build --release -p rustyfi-cli --manifest-path "$ROOT/Cargo.toml" >&2
  mkdir -p "$RESULTS" "$WORK/src"

  TAB="$(printf '\t')"
  python3 - "$BENCH/repos.toml" <<'EOF' | while IFS="$TAB" read -r name source commit; do
import sys, tomllib
with open(sys.argv[1], "rb") as f:
    for r in tomllib.load(f)["repo"]:
        print(f"{r['name']}\t{r['source']}\t{r['pinned_commit']}")
EOF
    [ -n "$ONLY" ] && [ "$ONLY" != "$name" ] && continue
    say "── $name ──"
    if [ "$commit" = "local" ]; then
      src="$ROOT/$source"
    else
      src="$WORK/src/$name"
      if [ ! -d "$src/.git" ]; then
        git clone --quiet "$source" "$src" >&2
      fi
      git -C "$src" fetch --quiet origin "$commit" 2>/dev/null || true
      git -C "$src" checkout --quiet "$commit"
      rm -rf "$src/.git"   # the analyzer must not see VCS internals
    fi
    out="$WORK/out/$name"
    rm -rf "$out"
    if "$ROOT/target/release/rustyfi" "$src" -o "$out" --fresh --json \
        > "$RESULTS/$name.json" 2> "$RESULTS/$name.log"; then
      say "$name: clean"
    else
      code=$?
      if [ "$code" = "1" ]; then say "$name: partial (see $RESULTS/$name.json)"
      else say "$name: FAILED exit=$code (see $RESULTS/$name.log)"; echo "{\"crate_name\":\"$name\",\"pipeline_failed\":true,\"exit_code\":$code}" > "$RESULTS/$name.json"; fi
    fi
  done
}

say "aggregating → bench/RESULTS.md"
python3 "$BENCH/aggregate.py" "$BENCH/repos.toml" "$RESULTS" > "$BENCH/RESULTS.md"
say "done: bench/RESULTS.md"
```

Note the `rm -rf "$src/.git"` — pinned checkout happens first, then VCS internals are removed so the translator never reads `.git`. Re-runs of a cloned repo therefore re-clone (the `-d "$src/.git"` guard fails and the directory is left from last time); to force a re-clone, `rm -rf bench/.work/src/<name>`. That trade-off is acceptable for a benchmark tool and documented here.

- [ ] **Step 3.2: Make it executable and sanity-check the parse loop**

```bash
chmod +x bench/run.sh
python3 -c "import tomllib; d=tomllib.load(open('bench/repos.toml','rb')); print(len(d['repo']), 'repos')"
```

Expected: `10 repos` (or however many Task 2 pinned).

- [ ] **Step 3.3: Commit**

```bash
git add bench/run.sh
git commit -m "feat: benchmark suite runner (bench/run.sh)"
```

---

### Task 4: Aggregator `bench/aggregate.py` (with self-test fixtures)

**Files:**
- Create: `bench/aggregate.py`
- Create: `bench/testdata/alpha.json`, `bench/testdata/beta.json`, `bench/testdata/gamma.json`
- Create: `bench/testdata/repos.toml`

- [ ] **Step 4.1: Write the failing self-test fixtures**

`bench/testdata/repos.toml`:

```toml
[[repo]]
name = "alpha"
source = "x"
pinned_commit = "local"
language = "go"
size_tier = "small"
expectation = "clean"

[[repo]]
name = "beta"
source = "x"
pinned_commit = "local"
language = "python"
size_tier = "small"
expectation = "clean"

[[repo]]
name = "gamma"
source = "x"
pinned_commit = "local"
language = "c"
size_tier = "large"
expectation = "impossible"
```

`bench/testdata/alpha.json`:

```json
{"crate_name":"alpha","language":"go","files_total":5,"files_translated":5,"files_failed":0,"errors":0,"todos":0,"cargo_clean":true,"duration_secs":60.0,"translate_model":"m","fix_model":"m","exit_code":0}
```

`bench/testdata/beta.json`:

```json
{"crate_name":"beta","language":"python","files_total":10,"files_translated":9,"files_failed":1,"errors":7,"todos":3,"cargo_clean":false,"duration_secs":120.0,"translate_model":"m","fix_model":"m","exit_code":1}
```

`bench/testdata/gamma.json`:

```json
{"crate_name":"gamma","language":"c","files_total":80,"files_translated":60,"files_failed":20,"errors":300,"todos":50,"cargo_clean":false,"duration_secs":900.0,"translate_model":"m","fix_model":"m","exit_code":1}
```

- [ ] **Step 4.2: Run the self-test to verify it fails**

Run: `python3 bench/aggregate.py --self-test`
Expected: FAIL — file does not exist yet.

- [ ] **Step 4.3: Write the aggregator**

Create `bench/aggregate.py`:

```python
#!/usr/bin/env python3
"""Aggregate per-repo benchmark JSON into bench/RESULTS.md.

Usage:  aggregate.py <repos.toml> <results-dir>      # markdown to stdout
        aggregate.py --self-test                     # assert against bench/testdata
Headline metrics (spec §success criteria):
  clean-rate  = clean / achievable          (achievable = expectation != "impossible")
  median      = median errors across ALL repos with a result
  hard case   = prompt-cache errors (when present)
"""
import json
import statistics
import sys
import tomllib
from pathlib import Path

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
    achievable = [r for r in rows if r["meta"]["expectation"] != "impossible" and r["result"]]
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
    assert h["clean_rate"] == (1, 2), h
    assert h["median_errors"] == 7, h
    assert h["prompt_cache_errors"] is None, h
    md = render(rows)
    assert "1/2 (50%)" in md, md
    assert "🟢 clean" in md and "🟠 partial" in md, md
    print("self-test: OK")


if __name__ == "__main__":
    if sys.argv[1:] == ["--self-test"]:
        self_test()
    elif len(sys.argv) == 3:
        print(render(load(Path(sys.argv[1]), Path(sys.argv[2]))))
    else:
        sys.exit(__doc__)
```

- [ ] **Step 4.4: Run the self-test to verify it passes**

Run: `python3 bench/aggregate.py --self-test`
Expected: `self-test: OK`

- [ ] **Step 4.5: Commit**

```bash
git add bench/aggregate.py bench/testdata/
git commit -m "feat: benchmark aggregator with self-test fixtures"
```

---

### Task 5: Baseline run — the Phase 1 gate

**Files:**
- Create: `bench/RESULTS.md` (generated, committed as the baseline record)

- [ ] **Step 5.1: Run the full suite against the current pipeline**

Requires `RUSTYFI_LLM_API_KEY` (+ optionally `RUSTYFI_LLM_BASE_URL`, `RUSTYFI_LLM_MODEL`, `RUSTYFI_FIX_MODEL`, `RUSTYFI_NO_TIER=1` to match prior baselines). Expect 30–90 minutes wall time; run in the background.

Run: `bench/run.sh`
Expected: one `[bench] ── <name> ──` block per repo; `bench/.work/results/<name>.json` for each; ends with `done: bench/RESULTS.md`.

- [ ] **Step 5.2: Sanity-check the scoreboard**

Run: `head -8 bench/RESULTS.md`
Expected: headline line with a real clean-rate fraction, a numeric median, and prompt-cache errors ≈ 42 (the v0.1.0 measurement; alias drift may move it slightly — record what it says).

- [ ] **Step 5.3: Gate check (spec: "suite runs end-to-end, baseline recorded")**

Confirm: every manifest repo has a JSON or an explicit `pipeline_failed` entry; no repo is silently missing from RESULTS.md. If a remote repo turned out to violate the selection criteria (e.g. far more than 60 files), swap it per Task 2 Step 2.1 and rerun just that repo: `bench/run.sh <name>`.

- [ ] **Step 5.4: Commit the baseline**

```bash
git add bench/RESULTS.md
git commit -m "feat: baseline benchmark results for the compile-clean 10x effort"
```

---

## Out of scope for this plan

Phases 2 (`fix_context.rs`) and 3 (`agent_fix.rs`) get their own plans once this baseline is committed — their designs are in the same spec, and their plans will reference the baseline numbers recorded here.
