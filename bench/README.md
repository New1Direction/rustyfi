# rustyfi benchmark suite

Measures the compile-clean rate of the translation pipeline against real,
pinned open-source repos. This is the evidence base for the
[compile-clean 10x effort](../docs/superpowers/specs/2026-06-11-compile-clean-10x-design.md).

## Run it

```sh
export RUSTYFI_LLM_API_KEY=…        # any OpenAI-compatible endpoint
bench/run.sh                        # full suite (hours; sequential)
bench/run.sh prompt-cache           # one repo by name
bench/run.sh --aggregate-only       # rebuild RESULTS.md from existing JSON
```

Per-repo artifacts land in `bench/.work/` (gitignored); the scoreboard is
written to [`RESULTS.md`](RESULTS.md). Repos are pinned by commit in
[`repos.toml`](repos.toml) — `expectation = "impossible"` marks
native-dependency-heavy entries that are reported but excluded from the
clean-rate denominator.

## `--json` schema

`rustyfi <src> -o <out> --fresh --json` prints one JSON object to stdout:

| field | meaning |
|---|---|
| `crate_name`, `crate_path`, `language` | identity of the generated crate |
| `files_total`, `files_translated`, `files_failed`, `files_written` | translation counts |
| `errors` | remaining `cargo check` errors after the fix loop |
| `todos` | `todo!()` gaps left in the crate |
| `cargo_clean` | true only if the crate compiles **and** nothing was stripped/stubbed dishonestly |
| `duration_secs`, `translate_model`, `fix_model`, `exit_code` | run metadata |

A run that crashes (exit 2) gets a stub record instead:
`{"crate_name": …, "pipeline_failed": true, "exit_code": N}`.

## Metric semantics

- **clean rate** = clean / achievable, where achievable = `expectation != "impossible"`
  with a result. Pipeline failures count against the rate — they are not exclusions.
- **median errors** is computed over runs that produced a crate. Note from the
  baseline: median alone flatters the pipeline — several repos compile with
  0 errors but ship dozens of `todo!()` gaps; read `errors` and `todos` together.
- Single-run numbers are noisy (LLM nondeterminism — see the baseline's
  prompt-cache 42-vs-179 story in the RESULTS commit message). Trends across
  the whole suite carry more signal than any one repo's run.
