# Rustyfi

> **Semantic Compression to Rust** — Drop any application in, get a production-quality Rust version out.

Rustyfi is a deterministic Rust control-plane that orchestrates the conversion of non-Rust codebases into optimized, compiled Rust applications. It treats LLMs as *unreliable generators* and Rust's type system as the *deterministic enforcement layer*.

---

## Quick Start

### 1. Set your LLM API key

```bash
export RUSTYFI_LLM_API_KEY="your-api-key-here"

# Optional — defaults to Gemini Flash via OpenAI-compat endpoint
export RUSTYFI_LLM_BASE_URL="https://generativelanguage.googleapis.com/v1beta/openai"
export RUSTYFI_LLM_MODEL="gemini-2.0-flash"
```

For OpenAI:
```bash
export RUSTYFI_LLM_BASE_URL="https://api.openai.com/v1"
export RUSTYFI_LLM_MODEL="gpt-4o"
```

### 2. Build and run

```bash
cargo build --release -p rustyfi-server
./target/release/rustyfi-server
# → http://localhost:7410
```

Or in dev mode:
```bash
cargo run -p rustyfi-server
```

### 3. Use the UI

1. Open **http://localhost:7410** in your browser
2. ZIP your source project: `zip -r myapp.zip myapp/`
3. Drag the ZIP onto the drop zone
4. Click **Translate to Rust**
5. Watch the pipeline progress live (SSE streaming)
6. Click **Download Rust Project** to get the generated Cargo crate

---

## Architecture

```
Browser (drag & drop)
       │  ZIP upload (multipart)
       ▼
rustyfi-server  (Axum HTTP + SSE)
       │
       ├── ZIP extraction
       └── rustyfi-engine::pipeline::run()
                │
                ├── [Analysis]    SourceAnalyser → ContextManifest + DependencyEdges
                ├── [Scaffold]    Scaffolder → Cargo workspace skeleton
                ├── [Translate]   ModuleGraph (DAG topo-sort)
                │                  └─ per-file: SemanticChunker → chunks
                │                               OwnershipGraph  → dependency context
                │                               LlmClient       → Rust code
                │                               OwnershipGraph  → record signatures
                ├── [Verify]      cargo check → DiagnosticFamily classification
                │                  └─ fix loop: targeted LLM repair per error family
                └── [Package]     ZIP output

rustyfi-core::Orchestrator (typed state machine — all phase transitions)
       Idle → Parsing → Scaffolding → Translating → Verifying → Optimizing → Completed
```

### Context Window Management

Large repos trigger **context window pressure** — mitigated by three layers:

| Layer | Component | What it does |
|---|---|---|
| **Module DAG** | `graph.rs` | Topological sort — translate dependencies before importers |
| **Semantic Chunking** | `chunker.rs` | Split files at function/class boundaries, stay under token budget |
| **Ownership Context** | `slicer.rs` | Inject already-translated Rust signatures into downstream prompts |

---

## Workspace Layout

```
crates/
├── rustyfi-core/       # Typed state machine + compiler layer
│   ├── state.rs        # RustyfiState + per-state context + DiagnosticFamily
│   ├── events.rs       # StateEvent enum — only way to drive transitions
│   ├── transitions.rs  # Orchestrator with exhaustive match table
│   ├── context.rs      # ContextManifest ingestion contract
│   ├── compiler.rs     # cargo check harness + JSON diagnostic parsing
│   └── errors.rs       # TransitionError, CompilerError, ManifestError
│
├── rustyfi-engine/     # Pipeline orchestration + LLM + scaffolding
│   ├── analysis.rs     # Source directory walk + language detection + edge inference
│   ├── checkpoint.rs   # Resumable stage checkpoints (JSON, per-phase)
│   ├── chunker.rs      # SemanticChunker — token-budget file splitting (8 languages)
│   ├── graph.rs        # ModuleGraph — DAG + Kahn's topological scheduler
│   ├── llm.rs          # Blocking LLM client + context-aware prompt builders
│   ├── pipeline.rs     # End-to-end run() — graph-scheduled, checkpoint-driven
│   ├── scaffold.rs     # Cargo project generator + ZIP packager
│   └── slicer.rs       # OwnershipGraph — symbol → Rust signature accumulator
│
└── rustyfi-server/     # Axum HTTP server
    └── main.rs         # /health, /api/translate (SSE), /api/download/:name

web/
├── index.html          # Drop zone UI
├── style.css           # Dark glassmorphism design system
└── app.js              # SSE client, progress tracker, terminal log
```

---

## Environment Variables

| Variable | Default | Description |
|---|---|---|
| `RUSTYFI_LLM_API_KEY` | *(required)* | LLM provider API key |
| `RUSTYFI_LLM_BASE_URL` | Gemini OpenAI-compat | Any OpenAI-compatible base URL |
| `RUSTYFI_LLM_MODEL` | `gemini-2.0-flash` | Model name |
| `PORT` | `7410` | HTTP server port |
| `RUST_LOG` | `rustyfi_server=info` | Tracing filter |
| `RUSTYFI_CHUNK_TOKENS` | `5000` | Max tokens per semantic chunk |

---

## Supported Source Languages

Python · TypeScript · JavaScript · Go · C · C++ · Java · C# · Ruby

---

## Design Constraints

- **No silent state mutation** — all transitions go through `Orchestrator::transition()`
- **Exhaustive match, no `_ => {}`** — every state/event combination is explicitly handled
- **LLM is just a generator** — `cargo check` is the truth oracle, not the model
- **Retry loops are ceiling-bounded** — `translate_retries` and `verify_retries` in `RunConfig`
- **Blocking HTTP for LLM** — deterministic orchestration stays synchronous; only the server is async
- **Resumable by design** — every phase writes a checkpoint; interrupted runs resume from the last completed file
- **DiagnosticFamily-targeted repair** — fix loop classifies errors and sends context-aware prompts per family
- **Graph-scheduled translation** — dependencies translated before their importers; type-compatible interfaces guaranteed

---

## Test Coverage

```
cargo test --workspace   # 44 tests, 0 failures
cargo clippy --workspace -- -D warnings   # zero lints
```
