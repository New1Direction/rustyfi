# Changelog

All notable changes to Rustyfi are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
aims to follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

The release where behavioral verification reached **libraries** — most real
targets are libraries, and the second oracle could previously only verify CLIs.

### Added
- **Automated library behavioral oracle** (`behavior/lib_oracle.rs`). For a
  library (no CLI entrypoint), the engine synthesizes a thin driver that
  exercises the public API in *both* the source language and Rust, runs both, and
  diffs stdout (canonicalized for language reprs like Python `True` vs Rust
  `true`). Both drivers are model-generated, with bounded compile/run repair
  loops; fail-open. Proven end-to-end on `itsdangerous` — the translated `Signer`
  verified byte-identical to Python (sign, unsign, tamper-rejection). Shipped as a
  verified engine capability + `examples/lib_oracle`; auto-wiring into the
  pipeline is gated while it hardens. See
  `docs/superpowers/specs/2026-06-15-library-behavioral-oracle.md`.
- **End-to-end product bench (Round 1)**, real crates through the release CLI with
  `--deep`: **3/4 clean** (calculator, itsdangerous, emoji-java; paint partial).
  First proof the HTTP `--deep` doctor *converges* a real crate end-to-end
  (`emoji-java`, "partial" expectation → 0 errors via `deepseek-reasoner`), and
  the behavioral oracle running on real crates (calculator verified identical;
  itsdangerous now honestly skipped — see below).

### Fixed
- **Behavioral oracle no longer fabricates false mismatches on libraries.** The
  recipe hard-coded `python3 main.py` / `node index.js` without checking the
  entrypoint exists; for a library (no `main.py`) the interpreter's "can't open
  file" error was captured as golden output, flagging every translation as a
  false behavioral mismatch. It now skips honestly when there is no runnable
  entrypoint.

### Notes
- **Translation flywheel: no measured lift.** A fair A/B (contract in both arms,
  3 seeds) showed the file-level corpus-injection flywheel does not improve
  first-shot error counts (delta +1.59); it ships **experimental / off by
  default**, the "moat" framing retracted. Details:
  `docs/superpowers/specs/2026-06-15-flywheel-ab-results.md`.

## [0.4.0] — 2026-06-14

The release where the recipe first carried a **real foreign project all the way
to a clean, building compile — fully automatically**: `ky` (a 52-file
TypeScript HTTP client) translated to Rust that passes `cargo check` *and*
`cargo build` with zero errors and no `todo!()` stubs. Getting there took a
keyless translation backend, a stack of deterministic correctness passes that
make the generated crate *structurally* sound before any model repair, and an
agentic doctor that finally drives reliably over an HTTP fix model.

This is the **floor** — a clean, building compile — not behavioral equivalence;
for a library, sameness is still unverified.

### Added
- **Keyless Claude Code CLI provider** (`RUSTYFI_PROVIDER=claude_cli`, or
  `RUSTYFI_FIX_PROVIDER=claude_cli`): drive translation through the local
  `claude` CLI on your subscription, no API key. It strips `ANTHROPIC_API_KEY`
  from the child so the CLI uses its own login. Best for **single-shot
  translation** — see Notes on why it is *not* the right backend for the `--deep`
  doctor loop.
- **Deterministic cross-module import resolver**: re-points `use crate::…::Sym`
  to the module that actually defines `Sym` (unambiguous-only, never deletes),
  clearing namespace-flattening `E0432`/`E0433` storms for zero tokens. It is
  **compiler-gated** — applied only if the error count strictly drops, otherwise
  reverted — so it can never regress a crate.
- **Compiler-guided auto-derive pass**: reads `E0277` diagnostics and adds the
  missing `#[derive(...)]` for derivable std traits on simple local types, also
  snapshot-revert gated.
- `$0` measurement tooling: `dep_scan`, `doctor_crate`, and `resolve` engine
  examples (measure the doctor / resolver on an existing crate, no translation
  key needed).

### Fixed
- **Auto-added dependencies were silently ignored.** The generated `Cargo.toml`
  ends with a `[workspace]` table, and both dependency writers appended to
  end-of-file — so every added dependency landed *after* `[workspace]` and Cargo
  parsed it as a workspace key. Dependencies now splice into `[dependencies]`.
  This alone clears the dominant import/dep error class on dependency-heavy
  crates.
- **Dependency-strip parser** now reads Cargo's current multi-line
  resolution-error format (`searched package name: \`X\``), so hallucinated
  registry deps are actually removed instead of wedging resolution.
- **No fragment ever reaches disk.** Every translated file is `syn`-parse-gated
  before write; an unparseable result becomes a valid TODO stub instead of an
  orphaned fragment that both blocks compilation and *suppresses* the rest of the
  crate's error count.
- **`extract_rust_code` no longer eats the file head**: a fenced code block
  inside a doc comment is no longer mistaken for the outer code-block wrapper.
- **The fix loop never overwrites a good file with unparseable model output.**
- **The agentic doctor now works over native HTTP tool-calling**: it records only
  the single tool call it answers per turn, fixing the `400 insufficient tool
  messages` that previously broke multi-step repair over OpenAI-wire providers —
  what lets a reliable HTTP fix model converge a crate end-to-end.
- Roomier default timeout for the Claude CLI provider.

### Notes
- **Use an HTTP fix model for `--deep`.** The `claude_cli` provider is reliable
  for single-shot translation but hangs unpredictably inside the doctor's
  multi-step tool loop; the agentic doctor should be driven by an HTTP model via
  `RUSTYFI_FIX_MODEL` / `RUSTYFI_FIX_PROVIDER`.
- Honest metric: a clean compile is **binary** (`cargo check` exit 0). A syntax
  error makes Cargo abort early and *under*-reports the true error count, so error
  counts alone flatter a broken crate — exit code is ground truth.

## [0.3.0] — 2026-06-13

The **second oracle**: `cargo check` proves the output is valid Rust; behavioral
verification proves it's the *same app*. Rustyfi now runs the original program
and its Rust translation against the same inputs and diffs stdout / stderr /
exit code — the original is ground truth. This is differential testing against a
fixture corpus (sameness on the cases exercised), not a proof of equivalence.

### Added
- **Behavioral verification phase** for CLI tools. When the source toolchain is
  present, a translation run auto-mines a corpus from the README and `--help`,
  captures golden output from the source (running it twice to auto-quarantine
  nondeterministic cases), and — once the crate compiles — diffs the Rust target,
  writing `behavior.yaml` + `behavior_report.json` into the output crate.
- **`rustyfi verify-behavior <crate-dir>`** — re-runs an existing `behavior.yaml`
  against the built crate (the review loop). Exit `0` = behaviors match, `1` =
  divergence.
- **`--no-behavior`** flag to skip the phase.
- **`--deep` now also repairs behavioral mismatches**: an agentic doctor
  (`RunBehaviorChecks` tool) grinds divergences down, keeping its edits only if
  the crate still compiles *and* mismatches strictly decrease — otherwise it
  reverts (snapshot-revert). This composes with the existing compile-error doctor.
- **`behavior.yaml` format**: per-side build/run commands, per-stream compare
  modes (`exact` / `ignore` / `normalized`), normalize rules
  (`strip_trailing_ws`, regex `mask`), and cases with golden `expect` values.
- **`behavior` block in `--json`** output and a behavior section in
  `NEXT_STEPS.md`, both reporting matches, quarantines, and deep-fix outcome
  honestly (a reverted repair never claims improvement).
- **Bench suite** reports a per-repo behavior-match column.
- New engine module `crates/rustyfi-engine/src/behavior/` (types, process
  harness, diff, golden capture, miner, recipes, pipeline orchestration).

### Security
- Behavioral verification executes the source project, so it is **CLI/local +
  bench only**. The hosted server never builds or runs uploaded code
  (`verify_behavior` is off there) — uploads are never executed.

### Notes
- Scope is CLI tools for now; libraries and HTTP services are future work.
- The deterministic machinery (mining, capture, diff, verify, and the repair
  loop's convergence) is proven by tests, including a scripted end-to-end repair.
  Driving the behavioral doctor against real divergences benefits from a strong
  fix model via `RUSTYFI_FIX_MODEL`.
- Catches the workspace crate version up to the release tag (it had drifted at
  `0.1.0` through the `v0.2.0` tag).

## [0.2.0] — 2026-06-12

### Added
- **Agentic deep-fix doctor** (`--deep` / `RUSTYFI_DEEP_FIX`): a budget-capped
  ReAct loop that grinds residual compile errors toward zero, with snapshot-revert
  if it doesn't improve the error count. Separate fix model via `RUSTYFI_FIX_MODEL`.
- **Contract validation**: per-package canonical Rust API is compiler-checked
  before file fan-out, with item-preserving regeneration.
- Context-enriched compile repair (`syn` item index + `rustc --explain`).
- `--json` machine-readable CLI summary; pinned benchmark suite (`bench/`).

### Fixed
- Cross-platform release builds (rustls instead of `openssl-sys`); release
  workflow creates the GitHub release before uploading binaries.

## [0.1.0] — 2026-06-11

- Initial public release: drag-and-drop / CLI app-to-Rust translation with
  `cargo check` as the oracle, dependency auto-repair, rustfix pass,
  module-naming/contract scaffolding, and honest `NEXT_STEPS.md` output.

[0.4.0]: https://github.com/New1Direction/rustyfi/releases/tag/v0.4.0
[0.3.0]: https://github.com/New1Direction/rustyfi/releases/tag/v0.3.0
[0.2.0]: https://github.com/New1Direction/rustyfi/releases/tag/v0.2.0
[0.1.0]: https://github.com/New1Direction/rustyfi/releases/tag/v0.1.0
