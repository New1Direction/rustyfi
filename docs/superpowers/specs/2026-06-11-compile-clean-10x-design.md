# Compile-Clean 10x — Design

**Date:** 2026-06-11
**Status:** Approved for planning
**Owner axis:** translation quality — get complex real-world apps to compile clean, push-button.

## Goal & success criteria

Raise rustyfi's compile-clean rate on real projects by an order of magnitude, measured three ways (all must hold):

1. **Suite clean rate:** ≥ 80% of the benchmark suite's *achievable* repos (those with
   `expectation ≠ impossible`; ~9 of the ~10 pinned real repos) compile clean push-button.
2. **Median residue:** median remaining `cargo check` errors across the suite < 5.
3. **Hard case:** `prompt-cache` (23-file Go service; 42 errors as of v0.1.0) reaches 0 errors.

Baseline evidence (2026-06-11 benchmark, deepseek-chat translate + deepseek-reasoner fix,
`RUSTYFI_NO_TIER=1`, same input zip as all prior baselines):

| Configuration | prompt-cache errors |
|---|---|
| v0.1.0-era chat fix loop | 117 |
| R1 fix loop (fence-bug handicapped) | 88 |
| v0.1.0 shipped (contract + rustfix + R1) | **42** |

Residual error profile of those 42: **24× E0277** (trait bound), 7× E0599 (missing method),
5× E0433/E0432 (unresolved path/import), 3× E0515/E0596 (borrow design), 2× E0308 (type mismatch).
The dominant classes are context-starvation failures: the one-shot fixer sees a single file and a
bare error string — it cannot see the trait that E0277 names or restructure across files.

## Non-goals

- New input languages, bigger-repo support, incremental translation (separate "breadth" axis).
- Web-UI/IDE day-2 workflow changes (separate axis; CLI `--deep` flag is the only surface change).
- Making native-C-dependency projects (e.g. Hydra → gtk/openssl) compile clean — impossible by
  construction; the suite includes one such repo and reports it honestly instead.

## Architecture

Three components, built in this order so every change is measured:

```
bench/ suite (measure) ──► fix_context.rs (enrich one-shot loop) ──► agent_fix.rs (deep-fix closer)
```

Pipeline placement (phase_verify): cheap LLM fix loop → rustfix pass → **[deep-fix phase, opt-in]** → final check.

### 1. Benchmark suite — `bench/`

- `bench/repos.toml`: ~10 entries — `name`, `git_url`, `pinned_commit`, `language`, `size_tier`,
  `expectation` (`clean` | `partial` | `impossible`). Includes: prompt-cache (Go, hard),
  examples/calculator (Go, known clean), one native-heavy known-impossible repo, and a spread of
  small/medium real Python/TS/Java/Ruby/Go projects. Selection criteria (resolved at
  implementation time): 5–60 source files, single dominant language, permissive license,
  pure-logic or std-lib-portable dependencies, real (not toy) usage.
- CLI grows `--json`: machine-readable run summary to stdout — `{crate_name, files_total,
  files_translated, files_failed, errors, todos, cargo_clean, duration_secs, translate_model,
  fix_model, served_model?}`.
- `bench/run.sh`: clones/checks out pinned commits, runs each repo through the CLI, aggregates into
  `bench/RESULTS.md` — per-repo verdict (clean/partial/rough) + headline metrics (% clean, median
  errors, prompt-cache count). Idempotent; re-runnable per phase.
- **First action of implementation:** run the suite against the current pipeline to fix the
  baseline before any improvement lands.

### 2. Context-enriched fix loop — `crates/rustyfi-engine/src/fix_context.rs`

Existing one-shot fixer keeps its shape (single-file write, snapshot-revert, `<ws>/src`
confinement). Each fix prompt gains a context block under `FIX_CTX_BUDGET` (~8 KB, mirroring
`CONTRACT_CTX_BUDGET`):

- `rustc --explain EXXXX` excerpt (first ~40 lines) per error code in the batch.
- syn-built **crate item index** (rebuilt once per fix cycle): map of item name → full source of
  its definition (structs/enums/traits/fns/impls). Every identifier mentioned in the diagnostic
  text or span gets its definition attached.
- E0277 special-casing: attach the named trait's definition **plus the list of existing `impl`
  blocks** for the named type, so the model sees what is and isn't implemented.
- Cross-file references (`pkg::Type` in the message): attach that package's contract section.

Priority order when the budget overflows: explain-excerpt < sibling impls < trait/type defs
(defs win; truncate excerpts first).

### 3. Agentic deep-fix phase — `crates/rustyfi-engine/src/agent_fix.rs` ("rustyfi doctor")

A ReAct-style loop on the existing `LlmClient`:

- **Tools:** `list_files()`, `read_file(path)`, `search(symbol)` (item-index lookup),
  `cargo_check()` (structured diagnostics), `explain(code)`, `write_file(path, content)`
  (confined to `<ws>/src`, same guard as the bug-#6 fix), `done(summary)`.
- **Transport:** native OpenAI-style function calling where the provider supports it; fallback to
  a JSON-action text protocol (one action per response, parsed strictly; malformed action → one
  reprompt, then abort the turn).
- **Activation:** only when errors survive the cheap loop + rustfix, and only when enabled —
  CLI `--deep` / env `RUSTYFI_DEEP_FIX=1`. Off by default (cost).
- **Budgets:** `RUSTYFI_DEEP_FIX_BUDGET` caps tool calls (default 40); plus token and wall-clock
  caps. Hitting any cap ends the phase gracefully.
- **Safety:** snapshot `src/` before the phase; after the phase, re-run `cargo check` — if the
  error count is not strictly lower than at phase start, revert the entire phase. Compiler is the
  oracle; a bad doctor session can never make the crate worse.
- **UX:** emits `Progress::Note` events ("doctor: examining E0277 in cache/mod.rs…"); summary note
  with fixes applied + budget spent. NEXT_STEPS.md records whether deep-fix ran and what it spent.

## Build order

| Phase | Deliverable | Gate |
|---|---|---|
| 1 | `bench/` suite + CLI `--json`; baseline RESULTS.md | suite runs end-to-end, baseline recorded |
| 2 | `fix_context.rs` wired into the existing loop | suite rerun; E0277 cluster shrinks; no regressions |
| 3 | `agent_fix.rs` + `--deep` | suite rerun; targets met: ≥80% clean, median <5, prompt-cache 0 |

## Testing

- **Unit:** syn item-index construction; context-budget truncation priority; E0277 trait+impls
  attachment; JSON-action protocol parsing (valid/malformed/reprompt); budget enforcement;
  snapshot/revert behavior.
- **Integration:** `#[ignore]`d end-to-end test — tiny broken crate + mocked LLM transport
  scripting a full doctor session through to `done()`, asserting the fix landed and budgets held.
- **E2E:** the benchmark suite itself, rerun at each phase gate.
- Existing 93-test suite stays green; `cargo fmt` + `clippy --workspace --all-targets -D warnings`
  remain CI gates.

## Risks & honesty notes

- **Provider drift:** DeepSeek serves newer models behind stable aliases (observed: deepseek-chat →
  deepseek-v4-flash). The suite pins repo commits and records the served model per run; model
  uplift is acknowledged in RESULTS.md rather than claimed as pipeline gains.
- **Cost:** a deep-fix session on a hard repo with a flagship model may cost a few dollars —
  capped, off by default, and spend is reported.
- **Impossible repos:** native-dependency projects are kept in the suite with
  `expectation = "impossible"` and excluded from the clean-rate denominator but shown in results.
- **Overfitting:** prompt-cache is one of ten repos, not the whole suite; phase gates use suite-wide
  metrics.
