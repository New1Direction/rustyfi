# Behavioral oracle (proof-of-concept)

`cargo check` proves Rustyfi's output is **valid Rust**. It does not prove the
output is the **same app**. This harness is the second oracle: it builds the
source project and its Rust translation, runs both against the same fixtures,
and diffs `stdout` / `stderr` / `exit code`.

The source binary is **ground truth** — the target must match it. This is
differential testing against a fixture corpus: it verifies sameness on the
cases exercised, not equivalence in general (that's undecidable). Nondeterminism
— clocks, randomness, map ordering, temp paths — is out of scope for this PoC
and is the first thing the full design has to handle.

## Run it

```bash
# from the repo root
python3 behavior/check.py examples/calculator/behavior.yaml
```

Exit `0` = behaviors match · `1` = divergence · `2` = build/setup failure.
A `behavior_report.json` is written next to the spec. Requires `PyYAML`, plus
whatever toolchain the spec's source/target need (here: `go`, `cargo`).

## What it proved on the calculator

The committed example claims the Go→Rust output "compiles clean and behaves
identically." Run against Rustyfi's actual generated crate, the oracle found
**6/6 cases diverge** — every one invisible to `cargo check`, which reported
zero errors:

| Case | Source (Go) | Target (Rust) |
|------|-------------|---------------|
| `2 + 3 * (4 - 1)` | `11` | `11.0000000000` |
| `2 +` (error) | `unexpected token: end of input` | `unexpected token: EOF` |
| REPL session | `= 4` | `> = 4.0000000000` |

Two root causes: float formatting (`%g` → `{:.10}`) and a token-label /
prompt drift. Apply three one-line fixes and the oracle flips to **6/6 matched,
exit 0** — confirming it's a real two-sided judge, not a tripwire that always
fires. That fix → re-run → converge cycle is exactly what the behavioral-repair
loop will automate.

## Spec format (`behavior.yaml`)

```yaml
name: calculator
source: { lang: go,   dir: examples/calculator,        build: [...], run: [...] }
target: { lang: rust, dir: bench/.work/out/calculator, build: [...], run: [...] }
compare: { stdout: exact, stderr: exact, exit_code: exact }  # per-stream: exact | ignore
cases:
  - { name: precedence, args: ["2 + 3 * (4 - 1)"] }
  - { name: repl,       stdin: "2 + 2\n" }              # cases may also set env
```

`{work}` in a build/run command expands to a git-ignored scratch dir
(`behavior/.work/`). Paths are relative to the repo root; commands run with
`cwd=dir`.

## Status

This `check.py` is the original **standalone proof** — pure mechanism, no model,
$0 to run. The production version now lives in Rust (`rustyfi-engine`'s
`behavior` module) and is wired into the tool end to end:

- **Detection + mining** (Plan 1 + 2): a translation run with the source
  toolchain present auto-mines a corpus from the README/`--help`, captures golden
  output from the source (quarantining nondeterministic cases), and — once the
  crate compiles — diffs the Rust target against it, writing `behavior.yaml` +
  `behavior_report.json` into the crate. Re-run anytime with
  `rustyfi verify-behavior <crate-dir>`.
- **Repair** (Plan 3): under `--deep`, a `RunBehaviorChecks` tool in the
  `agent_fix.rs` doctor loop grinds behavioral mismatches down, keeping the
  doctor's edits only if the crate still compiles **and** mismatches strictly
  decrease (snapshot-revert otherwise).

Design: `docs/superpowers/specs/2026-06-12-behavioral-equivalence-design.md`.
Plans: `docs/superpowers/plans/2026-06-13-behavior-*.md`.

**Security boundary:** behavioral verification builds and runs the source
project, so it is **CLI/local + bench only**. The hosted server never executes
uploaded code (`verify_behavior` stays off there).

**Only open item:** the *live* repair gate — driving the doctor's behavioral
loop against a real divergence needs a Claude-class fix model behind
`RUSTYFI_FIX_MODEL` (the same key as the phase-3 headline run). Every
deterministic seam is proven by a scripted e2e; the live convergence run is
pending that model.
