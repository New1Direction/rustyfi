# Behavioral-Equivalence Phase — Design Spec

**Date:** 2026-06-12
**Status:** Approved (brainstorm complete) — ready for implementation planning
**Branch:** `feat/behavioral-oracle` (the $0 detection PoC is committed here as `e324e0e`)
**Predecessor:** the compile-clean 10x effort (phases 1–3); this is the next axis.

---

## 1. Problem & thesis

`cargo check` proves Rustyfi's output is **valid Rust**. It does not prove the
output is the **same app**. A crate can compile clean and still print to stdout
instead of stderr, format floats differently, return the wrong exit code, or
drift on error wording.

This was proven on our own flagship example. The committed `examples/calculator`
README claims the Go→Rust output "compiles clean and behaves identically." Run
against Rustyfi's actual generated crate (`cargo check` → **0 errors**), the PoC
oracle found **6/6 cases diverge**: Go `%g` vs generated `{:.10}` float
formatting (every success case), `end of input` vs `EOF` token labels, and a
REPL prompt drift. Applying three one-line fixes flipped it to **6/6 matched**.

**Thesis:** add a second oracle. Oracle 1 = `rustc` (have it). Oracle 2 =
**source-behavior diff** — run the source project and its Rust translation
against the same inputs and compare observable behavior. The source binary is
ground truth.

This is **differential testing against a fixture corpus** — it verifies sameness
on the cases exercised, not equivalence in general (undecidable). The framing is
deliberately honest: "translate, verified by the compiler — and, for CLI tools,
by behavioral diff against the original." Not "provably 1:1."

## 2. Goals & success criteria

- Auto-mine a starter behavioral corpus from a CLI project and capture golden
  outputs from the source.
- A `phase_behavior` pipeline stage that, once the crate compiles, diffs the
  target against golden and reports honestly.
- A `RunBehaviorChecks` tool inside the existing `agent_fix.rs` doctor so the
  deep-fix loop converges on behavioral mismatches as well as compile errors.
- The calculator: from `cargo check`-clean-but-6/6-divergent to behavior-matched
  (manually proven; this spec automates the loop).
- Bench reports a behavior-match rate beside compile-clean for CLI repos.

## 3. Decisions (locked during brainstorm)

| # | Decision | Choice |
|---|----------|--------|
| 1 | Target program class (this spec) | **CLI tools only**. Libraries (function-level equivalence) and HTTP services are later specs. |
| 2 | Where test inputs come from | **Hybrid: auto-mine + review** — mine a starter corpus, emit `behavior.yaml`, user refines. |
| 3 | Repair scope | **Detection + mining + repair** in one spec. Repair ships code-complete but gate-unverified until the strong-model key is funded (same blocker as the phase-3 headline run). |
| 4 | Doctor repair integration | **Approach A** — a new `RunBehaviorChecks` tool in the existing unified doctor loop (not a separate phase, not synthetic-errors-only). Reuses budget caps, snapshot-revert, convergence. |

## 4. Scope boundaries

**In scope:** CLI tools (anything with a `main` producing stdout/stderr/exit);
mining from README + `--help` + test fixtures (fixtures best-effort); golden
capture with nondeterminism self-detection; `phase_behavior`; the
`verify-behavior` CLI subcommand; the doctor `RunBehaviorChecks` tool; bench
behavioral metrics; the production Rust port of the harness.

**Out of scope (explicit):**
- **Libraries** (no `main` → needs generated function-level equivalence tests) — later spec.
- **HTTP services** (spin-up/replay/teardown) — later spec.
- **Server-side execution of uploaded code.** Golden capture and verification run
  the source/target, which requires the source toolchain *and trust*. The hosted
  server will **not** execute untrusted uploads without a sandbox. For v1 the
  server does only **static** mining (README + fixture parsing) and emits a
  `behavior.yaml` *skeleton* (cases, no golden); execution lives in CLI/local +
  bench. Sandboxed server-side execution is a later spec.

## 5. Architecture

```
mine → capture golden → [compile-clean gate] → verify (diff) → repair (doctor, --deep)
```

**Components:**

1. **`behavior.yaml` (spec/contract)** — self-contained after mining: source &
   target build/run commands, a `cases` list, captured **golden** `expect`
   values per case, per-stream compare modes, and `normalize` transforms.
2. **Miner** — harvests candidate invocations from README command blocks,
   `--help`/subcommand discovery, and source test fixtures.
3. **Golden capture + nondeterminism self-detection** — builds & runs the source
   once per case to capture ground truth; runs it **twice** and quarantines any
   case where the source disagrees with itself.
4. **`phase_behavior`** — pipeline stage; mines+captures, gates on compile-clean,
   diffs target vs golden, emits `behavior_report.json`.
5. **`RunBehaviorChecks` doctor tool** — converges behavioral mismatches inside
   the existing ReAct loop.
6. **Rust port** — production harness in the engine (reuses cargo/process infra,
   `serde_yaml`, the doctor). The Python PoC (`behavior/check.py`) is retained as
   a language-agnostic reference/bench tool.

**Two hard constraints:**
- **(a) Compile-clean gate.** You can't run a binary that doesn't compile;
  verify/repair run only after compile-clean, else skipped with an honest note.
- **(b) Execution requires source toolchain + trust** — see §4 server boundary.

## 6. `behavior.yaml` format

```yaml
name: calculator
source: { lang: go,   dir: ., build: [...], run: ["{work}/calc-go"] }
target: { lang: rust, dir: ., build: ["cargo","build","-q"], run: ["target/debug/calculator"] }
compare:                       # defaults; per-stream: exact | ignore | normalized
  stdout: exact
  stderr: exact
  exit_code: exact
normalize:                     # ordered; applied to BOTH sides before exact compare
  - strip_trailing_ws
  - mask: { pattern: '\d{4}-\d{2}-\d{2}', token: '<DATE>' }
cases:
  - name: precedence
    source: readme             # provenance: readme | help | fixture | manual
    args: ["2 + 3 * (4 - 1)"]
    expect: { stdout: "11\n", stderr: "", exit_code: 0 }   # golden, captured from source
  - name: help
    source: help
    args: ["--help"]
    expect: { ... }
  - name: repl
    source: readme
    stdin: "2 + 2\n"
    expect: { ... }
  - name: now
    source: fixture
    args: ["now"]
    nondeterministic: true     # quarantined: source disagreed with itself
```

`{work}` expands to a git-ignored scratch dir. Paths are relative to the crate;
commands run with `cwd=dir`. A case may set `args`, `stdin`, and `env`.

## 7. Miner — three sources (priority order)

1. **README command blocks** — scan fenced code blocks for lines invoking the
   binary (`$ calc "…"`, `binary <args>`, documented entrypoint); extract args.
   (The calculator README is directly minable this way.)
2. **`--help` / subcommand discovery** — run `--help` (and `<subcmd> --help`);
   the help text itself is a behavioral case, and discovered subcommands become
   probes. (Requires execution — CLI/local only.)
3. **Test fixtures** — the source's own tests encode input→output (Go
   table-driven tests, pytest `parametrize`, golden files). Richest but most
   format-specific; **best-effort + extensible** in v1, never a correctness
   dependency.

Server-side (static only): README + fixture parsing. `--help` and golden capture
require execution and are CLI/local.

## 8. Determinism — three layers (cheapest first)

1. **Self-detection (automatic):** run the source twice; if it disagrees with
   itself on any compared stream, set `nondeterministic: true`, keep the case in
   the file for visibility, exclude it from the gate and from repair targets.
2. **`normalize` transforms** (`strip_trailing_ws`; regex `mask → token`) applied
   to both sides before an exact compare; auto-seeded, user-extensible.
3. **Per-stream `ignore`** for streams known to be noisy.

## 9. Pipeline integration

A single `phase_behavior` at the tail of the pipeline:

1. **Mine + capture golden** from the source → write `behavior.yaml`.
   Checkpointed (`BehaviorCheckpoint`, mirroring `ContractCheckpoint`) so resume
   never re-runs the source.
2. **Compile-clean gate** — proceed only if the target builds.
3. **Verify** — build target, diff each deterministic case vs golden →
   `behavior_report.json`.
4. **Repair (`--deep`, key-gated)** — doctor `RunBehaviorChecks` loop (§12).

Full sequence:
`compile fix-loop → rustfix → (--deep compile-doctor) → [compiles?] → mine+capture → verify → (--deep behavior-doctor)`.

Mining needs the source toolchain (available from the input project start);
verify/repair need the compiled target. If the source toolchain is absent,
behavior steps skip with a note ("install go to verify behavior"), never fatal.

## 10. CLI surface

- **Translation flow** (`rustyfi <proj> -o out`) gains behavioral output
  automatically when the source toolchain is present: a summary line +
  `behavior.yaml` + `behavior_report.json` written into the crate. `--no-behavior`
  skips; `--deep` also drives behavioral repair.
- **`rustyfi verify-behavior <crate-dir>`** — standalone re-run of an existing
  `behavior.yaml` against the built target. This *is* the review loop: edit the
  spec, re-run.
- Compile exit codes (0 clean / 1 errors / 2 failure) stay primary; behavioral
  status rides in the summary and `--json`.

## 11. Product (server/web) boundary

Per §4, the server does **not** execute uploaded code. It performs **static**
mining (README + fixture parsing) and emits a `behavior.yaml` *skeleton* (cases,
no golden) into the crate; the web UI states "behavioral spec generated — run
`rustyfi verify-behavior` locally to verify." Full capture/verify/repair are
CLI/local + bench for v1.

## 12. Repair loop — `RunBehaviorChecks` doctor tool

**Session model (resolves the §9 sequence):** behavioral repair is **not** a
reimplemented loop. It is a second invocation of the *same* doctor engine, run
**after** compile-clean (behavior tools are useless on a non-compiling crate),
with `RunBehaviorChecks` added alongside `CargoCheck` in the tool set — so the
session can fix a behavioral bug *and* keep compilation green in the same loop.
That is what "approach A, not a separate phase" means: one engine, two tools,
two gated invocations (compile first, behavior once it compiles).

- New `ToolCall::RunBehaviorChecks` in `agent_fix.rs`: builds the target, runs
  the deterministic cases vs golden, returns per-case pass/fail and (for
  failures) the diff (`expected` vs `actual` stdout/stderr/exit), capped like
  `CargoCheck`'s rendering.
- `SYSTEM_DOCTOR` extended: cargo check is ground truth for **validity**, the
  corpus is ground truth for **sameness**; fix root causes (formatting, output
  routing, off-by-one); **never edit `behavior.yaml` or delete cases to pass** —
  golden values are immutable (behavioral analog of "never delete functionality
  to silence errors").
- **Guard already holds:** `WriteFile` is confined to `src/`; `behavior.yaml`
  lives at the crate root, so the doctor structurally cannot rewrite the truth it
  is judged against. Asserted by test.
- **Convergence & safety:** acceptance spans two dimensions — keep the session's
  edits iff *(target compiles) AND (behavioral mismatches strictly decreased) AND
  (compile errors not increased)*; otherwise wholesale `restore_src` revert
  (reuses existing `snapshot_src`/`restore_src`). The model has `CargoCheck` in
  the same loop, so it can't silently trade a behavior fix for a compile break;
  the phase-level gate backstops it.
- **Budget:** reuses `DoctorBudget` + `RUSTYFI_DEEP_FIX_BUDGET`/`_TIMEOUT`. Note:
  each `RunBehaviorChecks` rebuilds+runs the target, costlier than `CargoCheck` —
  budgeted accordingly.

## 13. Reporting (honest output)

- `BehaviorSummary { cases, matched, quarantined }` + an extended deep-fix block.
- `--json` gains a `behavior` object: `{ cases, matched, quarantined, deep_fix:
  {ran, start_mismatches, end_mismatches, tool_calls} | null }`.
- NEXT_STEPS records mined/quarantined counts, match results, and a pointer to
  review/extend `behavior.yaml`; the Done event carries behavioral status.

## 14. Edge cases

- Source won't build → skip behavior, honest note.
- Target won't compile → gate-skip verify/repair.
- All cases quarantined → "no deterministic behavior to verify."
- Zero cases mined → "no behavioral cases found; add them to `behavior.yaml`."
- User-authored cases merge with mined cases; user cases win on conflict.

## 15. Testing

- **Unit:** miner parsers (README blocks, `--help`, fixtures) on fixtures;
  `normalize` transforms; nondeterminism self-detection (source-twice); diff /
  compare logic; golden serialization round-trip.
- **Scripted doctor e2e:** `ScriptedTransport` walks a behavioral-repair session
  (`RunBehaviorChecks` shows a float-format mismatch → read `main.rs` → write fix
  → checks clean → done), mirroring the existing compile-doctor scripted test.
- **Real `#[ignore]` e2e:** the **calculator** is the built-in fixture — mine,
  capture golden from Go, verify the generated Rust (expect mismatches), repair
  if a model key is present (0/6→6/6 already proven by hand).
- **Bench:** behavior-match rate reported for `calculator` (+ any CLI repos).

## 16. Risks & blockers

- **Strong-model key** (~$10 OpenRouter top-up → `RUSTYFI_FIX_MODEL=
  anthropic/claude-sonnet-4.6`): the behavioral-repair half cannot be
  gate-proven without it (same blocker as the pending phase-3 headline run). One
  spend unlocks both. Detection + mining are fully testable without it.
- **Corpus coverage:** the current bench corpus is mostly libraries, so CLI-first
  behavioral scoring covers only `calculator` (+ any CLI repos). Reported
  honestly; broadened by the libraries spec.
- **Mining recall:** README/`--help` mining will miss cases; the hybrid review
  loop (user extends `behavior.yaml`) is the mitigation, not a guarantee.

## 17. Out of scope / future specs

Libraries (function-level equivalence); HTTP services (spin-up/replay);
sandboxed server-side execution; behavioral diffing of files written / network
calls; auto-generating `normalize` rules beyond timestamps.
