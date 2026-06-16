# Rustyfi 🎺🦀

> **Drop a codebase in. Get a Rust head-start out.**
> Rustyfi turns Python / TypeScript / JavaScript / Go / C / C++ / Java / C# / Ruby
> projects into Rust crates. It treats the LLM as an *unreliable generator* and
> `cargo check` as the *truth oracle* — so the result is verified by the compiler,
> not vibes.

It is the difference between staring at a blank `src/` and starting from a
compiling Rust project that already mirrors your structure, types, and logic.

---

## What it actually does (the honest version)

Translating real software between languages is not a solved problem, and Rustyfi
does not pretend otherwise. What you get depends on what you feed it:

| Your project | What comes out |
|---|---|
| **Pure logic** — CLIs, libraries, parsers, algorithms, data tools (standard library, no native deps) | Usually a **clean `cargo check`** — drop it, download it, it builds. See the [calculator example](examples/calculator). |
| **Real-world apps** — web services, framework-heavy, native-library-bound | A **compiling skeleton** with your modules wired up and the type-chaos eliminated, plus an honest `NEXT_STEPS.md` punch-list of what's left (framework API mappings, native deps). Run `--deep` to let the agentic doctor grind the remaining errors against `cargo check`. |

The UI never lies about which one you got. A partial run never masquerades as a
perfect one — the result banner and `NEXT_STEPS.md` tell you exactly where you
stand.

**What no tool can do** (including this one): take an arbitrary, framework-heavy
application and emit clean, idiomatic Rust with zero human follow-up. Closing the
last mile on a complex app means mapping one ecosystem's semantics onto another's
(say, a Go web framework's request context onto Axum's extractors) — that needs a
human or a much smarter model in the loop. Rustyfi gets you most of the way and is
honest about the rest.

---

## See it work: Go → Rust, clean

The [`examples/calculator`](examples/calculator) project is a real lexer +
recursive-descent parser + CLI in Go. Run it through Rustyfi and the output crate
compiles clean and behaves identically:

```bash
# Go in:
2 + 3 * (4 - 1)        2 ^ 10        (1 + 2) * (3 + 4)        2 +

# Rust out (cargo check → 0 errors), same answers:
11                     1024          21                       error: unexpected token: end of input
```

Two Go packages become `src/main.rs` and `src/calc/mod.rs`, Go's `iota` enum
becomes a real Rust `enum`, and `Token`/`TokenKind` keep one canonical shape
across files. Verified, not claimed.

---

## Quick Start

### 1. Pick a provider

Any OpenAI-compatible endpoint works. A fast, cheap default like DeepSeek is a
great fit — bulk translation is voluminous and cheap; only the compile-fix loop
benefits from a stronger model.

```bash
export RUSTYFI_LLM_API_KEY="your-api-key"
export RUSTYFI_LLM_BASE_URL="https://api.deepseek.com"   # or OpenAI, Gemini, Cerebras, OpenRouter…
export RUSTYFI_LLM_MODEL="deepseek-chat"
export RUSTYFI_NO_TIER=1                                  # non-OpenRouter providers: skip tier routing

# Optional but recommended — point the compile-fix loop at a reasoning model:
export RUSTYFI_FIX_MODEL="deepseek-reasoner"
```

Multiple keys? Comma-separate them in `RUSTYFI_LLM_API_KEY` and Rustyfi
round-robins across them. Prefer xAI Grok via OAuth? `export RUSTYFI_PROVIDER=grok`.

**Already have Claude Code? Use it — no API key, no extra spend.** If the
[`claude`](https://claude.com/claude-code) CLI is installed and signed in,
Rustyfi can drive it as a model backend through your existing subscription.
This is the cheapest way to point the precision work (the compile-fix loop and
the `--deep` doctor) at a Claude-class model while keeping bulk translation on a
cheap API model:

```bash
# Cheap API model for the voluminous translation pass…
export RUSTYFI_LLM_API_KEY="your-api-key"
export RUSTYFI_LLM_BASE_URL="https://api.deepseek.com"
export RUSTYFI_LLM_MODEL="deepseek-chat"
# …and your local Claude Code (Opus) for the compile-fix loop + doctor:
export RUSTYFI_FIX_PROVIDER=claude_cli
export RUSTYFI_FIX_MODEL=opus

rustyfi ./myapp -o ./myapp-rust --deep
```

Set `RUSTYFI_PROVIDER=claude_cli` to route *everything* (translation included)
through the CLI. The subprocess runs `claude -p` with `ANTHROPIC_API_KEY`
removed from its environment so it uses your Claude Code login rather than a
key; set `RUSTYFI_CLAUDE_KEEP_KEY=1` to keep the key, or `RUSTYFI_CLAUDE_BIN` to
point at a non-default `claude` binary.

### 2. Translate — pick your interface

**A — the CLI** (no server, no browser; scriptable and CI-friendly):

```bash
# install the released binary (macOS · Linux · Windows, x86_64 + arm64):
curl -fsSL https://raw.githubusercontent.com/New1Direction/rustyfi/main/install.sh | sh
#   …or from source:  cargo install --git https://github.com/New1Direction/rustyfi rustyfi-cli
#   …or in-tree:      cargo run -p rustyfi-cli -- ./myapp -o ./myapp-rust

# then just point it at any project (a directory or a .zip):
rustyfi ./myapp -o ./myapp-rust

# stuck on the last few errors of a complex app? engage the agentic doctor:
rustyfi ./myapp -o ./myapp-rust --deep   # needs a strong RUSTYFI_FIX_MODEL

# skip behavioral verification (e.g. source toolchain not present):
rustyfi ./myapp -o ./myapp-rust --no-behavior

# re-run behavioral checks against an already-built crate (edit/extend behavior.yaml, then):
rustyfi verify-behavior ./myapp-rust
```

```text
  rustyfi 🎺🦀
  source  ./myapp
  output  ./myapp-rust  (crate: myapp)

✓ Analyzing source
✓ Scaffolding + pinning type contract
✓ Translating to Rust   ████████████ 12/12
✓ Verifying with cargo check
  · Auto-fixed 37 compile error(s) using the compiler's own suggestions (no AI needed)

  ✓ compiles clean
  · 12 translated from go · 18 file(s) written · 0 todo!() stub(s)

  → cd ./myapp-rust && cargo run
```

The exit code tells the truth, so it drops straight into a script or CI:
**`0`** = compiles clean · **`1`** = compiles with errors (a head-start +
`NEXT_STEPS.md`) · **`2`** = failed. The crate path is printed to **stdout**;
everything else goes to stderr. Live progress is rich in a terminal and plain
when piped. Re-run the same command to **resume** where an interrupted run left
off (`--fresh` to start over).

**B — the web UI** (drag & drop, live stream):

```bash
cargo run -p rustyfi-server     # → http://localhost:7410
```

Open it, drag a ZIP onto the drop zone, hit **Translate to Rust**, and
**Download Rust Project** when it's done. Same engine, same honesty — the
terminal panel tells you whether your provider is configured before you upload.

---

## How it works

The core idea: **let the LLM generate, let `cargo check` judge, and do everything
the compiler can already tell you deterministically — for free.**

```
Browser (drag & drop ZIP)
       ▼
rustyfi-server  (Axum HTTP + SSE)
       │  zip-slip safe · content fingerprint (resume/reset) · bomb-capped
       ▼
rustyfi-engine::pipeline::run()
   │
   ├─ [Analysis]    walk · language detect · import edges
   ├─ [Scaffold]    Cargo skeleton · directory-as-package module map
   ├─ [Contract]    ⭐ one cheap call per package extracts the canonical Rust
   │                API (every struct's full fields, enums, traits, signatures),
   │                then COMPILER-VALIDATES it as a skeleton before fan-out —
   │                a structurally broken contract (e.g. a dyn-incompatible
   │                trait) is regenerated, never multiplied across every file
   ├─ [Translate]   DAG-scheduled, semantically chunked, parallel, rate-gated;
   │                each file translated against the canonical contract
   ├─ [Verify]      cargo check ──► the truth oracle
   │     │
   │     ├─ rustfix ⭐ harvest rustc's OWN machine-applicable suggestions and
   │     │            apply them deterministically (zero tokens). MaybeIncorrect
   │     │            guesses are applied only if they reduce the error count.
   │     ├─ dep repair  strip hallucinated/unresolvable crates so resolution
   │     │              succeeds and real errors become visible
   │     ├─ fix loop    targeted LLM repair per error family — with the trait
   │     │              definitions, `rustc --explain`, and impls the error
   │     │              names injected as context; deduped on every write
   │     └─ doctor ⭐ (--deep) an agentic loop that reads, searches, edits, and
   │                  re-checks the crate until it compiles or the budget caps —
   │                  src-confined and snapshot-reverted, so it can never make
   │                  the crate worse than it found it; also engages on behavioral
   │                  mismatches (keeps edits only if the crate still compiles AND
   │                  mismatches strictly decrease, else reverts)
   └─ [Package]     ZIP output + NEXT_STEPS.md (Done is sent only after the ZIP
                    is on disk — the download can never race it)
```

---

## The second oracle: behavioral verification

`cargo check` tells you the translation compiles. Behavioral verification tells
you whether it *runs the same way*.

For CLI tools, a translation run auto-mines a fixture corpus from the project's
README and `--help` output, runs the **source binary** on each case to capture
golden stdout/stderr/exit-code, then — once the crate compiles — diffs the Rust
target against it. This is **differential testing on a corpus of exercised
cases**, not a proof of equivalence. It tells you: for every input in the
fixture set, the source and the Rust produce identical observable output. The
source binary is ground truth.

Non-deterministic cases are quarantined automatically: if running the source
twice produces different output, that case is excluded from the corpus rather
than silently letting it flap.

Two artifacts land in the output crate:

- **`behavior.yaml`** — the fixture corpus (build + run commands, per-stream
  compare mode, normalize rules, and per-case golden `expect`). Human-readable
  and hand-editable.
- **`behavior_report.json`** — the diff of the last run: pass/fail per case,
  actual vs. expected output for every divergence.

**`behavior.yaml` compare modes** (per stream):

| Mode | Meaning |
|---|---|
| `exact` | byte-for-byte match |
| `ignore` | stream not checked |
| `normalized` | apply rules first (e.g. `strip_trailing_ws`, regex mask), then exact-match |

**The review loop:**

```bash
# After a translation, check the report:
cat ./myapp-rust/behavior_report.json

# Edit behavior.yaml (add cases, adjust compare modes, add normalize rules),
# then re-verify the already-built crate without re-translating:
rustyfi verify-behavior ./myapp-rust
# exit 0 = behaviors match · exit 1 = divergence
```

`--deep` now also drives the doctor on behavioral mismatches — it will read,
edit, and re-verify the crate, keeping changes only when the crate still
compiles *and* the mismatch count strictly decreases.

**Honest scope:** behavioral verification **auto-runs for CLI tools** today.
Because it runs the source project, it is a local-only phase — the hosted web
flow never executes uploaded code. Skip it with `--no-behavior`.

**Library behavioral verification is proven and rolling out.** Most real targets
are libraries (no CLI to diff), so the engine synthesizes a thin *driver* that
exercises the public API in *both* the source language and Rust, then diffs the
output — turning a library into a comparable program. On `itsdangerous` the
translated `Signer` verified **byte-identical to Python** (sign, unsign, and
tamper-rejection), with both drivers model-generated. It ships today as a
verified engine capability; auto-wiring it into every library translation is
gated behind a flag while it hardens (it costs a model call per run). HTTP
service support is still planned.

---

Two ideas do most of the work, and both came from asking *"what does the compiler
already know that we're throwing away?"*

- **The contract phase** fixes consistency *where it's created*. Translating each
  file in isolation makes the same type come out differently in different files
  (the classic "struct has 3 fields here, 2 fields there" bug). Pinning one
  canonical API per package up front eliminates that entire class of error — and
  in testing, the cheap model *with* the contract beat the expensive model
  *without* it.
- **rustfix** stops paying an LLM to re-derive fixes the compiler already
  computed. `cargo check` emits structured, machine-applicable suggestions for a
  large class of errors; Rustyfi applies them directly. On a real run it fixed
  **200+ errors deterministically, before the model touched anything.**

---

## Why it's trustworthy

- **Two oracles, not one** — `cargo check` verifies compilation; behavioral
  verification diffs the source and Rust binaries on a fixture corpus. Both run
  before the result is declared. Neither is the model's word.
- **Honest output** — if files fell back to stubs or the build isn't clean, the UI
  and `NEXT_STEPS.md` say so plainly.
- **Deterministic where possible** — module wiring, dedup, dependency repair, and
  rustfix are exact, repeatable passes, not model guesses.
- **Resumable by design** — every phase checkpoints; identical re-uploads resume,
  changed re-uploads reset (content fingerprint).
- **Fail fast on config** — a missing/invalid API key aborts immediately with an
  actionable message instead of stubbing out every file.
- **Safe by construction** — edits are confined to the generated crate's `src/`; a
  dependency's cached source is never touched.

---

## Environment Variables

| Variable | Default | Description |
|---|---|---|
| `RUSTYFI_LLM_API_KEY` | *(required for openai)* | API key(s) — comma-separate for round-robin |
| `RUSTYFI_LLM_BASE_URL` | `https://openrouter.ai/api/v1` | Any OpenAI-compatible base URL |
| `RUSTYFI_LLM_MODEL` | `google/gemini-2.5-flash` | Translation model ID |
| `RUSTYFI_FIX_MODEL` | *(falls back to `RUSTYFI_LLM_MODEL`)* | **Stronger model for the compile-fix loop** — translation is cheap, repair is precision work |
| `RUSTYFI_FIX_BASE_URL` / `RUSTYFI_FIX_API_KEY` / `RUSTYFI_FIX_PROVIDER` | *(fall back to translation config)* | Point the fix loop at a different endpoint entirely |
| `RUSTYFI_FIX_TIMEOUT` | `180` | Per-request timeout for fixes (reasoning models think longer) |
| `RUSTYFI_DEEP_FIX` | *(unset)* | Engage the agentic doctor on residual errors (the CLI's `--deep` sets this) |
| `RUSTYFI_DEEP_FIX_BUDGET` | `40` | Max doctor tool calls before it stops |
| `RUSTYFI_DEEP_FIX_TIMEOUT` | `1200` | Doctor wall-clock budget, seconds |
| `RUSTYFI_PROVIDER` | `openai` | `grok`/`xai` for Grok OAuth, or `claude_cli` to drive the local Claude Code CLI (no API key) |
| `RUSTYFI_CLAUDE_BIN` | `claude` | Path to the Claude Code binary when `*_PROVIDER=claude_cli` |
| `RUSTYFI_CLAUDE_KEEP_KEY` | *(unset)* | Keep `ANTHROPIC_API_KEY` for the `claude` subprocess instead of using its own login |
| `RUSTYFI_VERIFY_RETRIES` | `4` | Max compile-fix cycles |
| `RUSTYFI_RPM` | `25` | Global requests-per-minute gate across all workers |
| `RUSTYFI_PARALLEL` | `16` | Files translated concurrently |
| `RUSTYFI_CHUNK_TOKENS` | `5000` | Max tokens per semantic chunk |
| `RUSTYFI_NO_TIER` | *(unset)* | Disable tiered model routing (use for non-OpenRouter providers) |
| `RUSTYFI_NO_STUB` | *(unset)* | Translate test/fixture/generated files too |
| `PORT` | `7410` | HTTP server port |
| `RUST_LOG` | `rustyfi_server=info` | Tracing filter |

---

## Workspace Layout

```
crates/
├── rustyfi-core/       # Typed state machine + cargo-check harness
│   ├── state.rs · events.rs · transitions.rs   # Orchestrator (exhaustive match)
│   ├── context.rs · compiler.rs · errors.rs    # manifest + diagnostic parsing
│
├── rustyfi-engine/     # Pipeline orchestration + LLM + scaffolding
│   ├── analysis.rs     # walk, language detect, import resolution
│   ├── checkpoint.rs   # resumable per-phase checkpoints (JSON)
│   ├── chunker.rs      # SemanticChunker — token-budget file splitting
│   ├── graph.rs        # ModuleGraph — DAG + topological scheduler
│   ├── scaffold.rs     # Cargo generator, directory-as-package layout, ZIP
│   ├── contract*       # canonical per-package Rust API (in pipeline.rs/llm.rs)
│   ├── contract_check.rs # ⭐ compiler-validate contracts before fan-out
│   ├── slicer.rs       # signature extraction / contract splitting
│   ├── deps.rs         # curated dependency auto-detection (allowlist-only)
│   ├── rustfix.rs      # apply rustc's own machine-applicable suggestions
│   ├── fix_context.rs  # ⭐ trait defs + rustc --explain + impls for fix prompts
│   ├── agent_fix.rs    # ⭐ the doctor — guarded, budgeted, snapshot-reverted
│   ├── behavior/       # ⭐ behavioral verification — corpus mining, golden capture,
│   │                   #   diff runner, behavior.yaml + behavior_report.json writer
│   ├── dedup_items.rs  # syn-based duplicate-definition removal
│   ├── llm.rs          # blocking LLM client (OpenAI-compat + Grok OAuth)
│   └── pipeline.rs     # end-to-end run() — phased, checkpointed, parallel
│
├── rustyfi-server/     # Axum HTTP server — /health /api/translate (SSE) /api/download/:name
└── rustyfi-cli/        # `rustyfi` command — drives the engine directly, no server
    ├── main.rs         # args, run, exit codes, summary
    ├── progress.rs     # TTY-aware live progress (rich interactive / plain piped)
    └── unzip.rs        # zip-slip-safe archive in/out

web/                    # Drop-zone UI: live SSE progress, honest result banner
examples/calculator/    # Go → clean Rust, verified
bench/                   # Benchmark suite: pinned real repos, --json, scoreboard
```

---

## Tests

```bash
cargo test --workspace
cargo clippy --workspace -- -D warnings
```

---

## Easter eggs

There are at least three. The trombone knows what it did. 🎺
