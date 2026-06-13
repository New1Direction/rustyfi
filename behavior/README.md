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

## Status & where this is going

This is a **standalone proof** — pure mechanism, no model, $0 to run. It
deliberately does **not** prejudge the production architecture. The full
behavioral-repair phase (a `phase_behavior_verify` pipeline stage, a
`RunBehaviorChecks` tool inside the existing `agent_fix.rs` doctor loop, the
contract format, a multi-case corpus, and determinism handling) is the subject
of a dedicated design spec before any of it is built.
