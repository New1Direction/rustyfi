# Changelog

All notable changes to Rustyfi are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
aims to follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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

[0.3.0]: https://github.com/New1Direction/rustyfi/releases/tag/v0.3.0
[0.2.0]: https://github.com/New1Direction/rustyfi/releases/tag/v0.2.0
[0.1.0]: https://github.com/New1Direction/rustyfi/releases/tag/v0.1.0
