# Translation Flywheel — Self-Improving Translation from Oracle-Verified Pairs

- **Date:** 2026-06-14
- **Status:** Design — awaiting review
- **Branch:** `feat/translation-flywheel`

## Problem

Rustyfi's translation layer is a slot machine. The same source file, translated
twice, yields different Rust with different error counts (measured repeatedly:
itsdangerous 0 vs 15 errors, paint 10 vs 72). Quality comes entirely from the
model's per-call guess, and nothing the tool learns from one successful
translation carries into the next. There is no compounding asset and no moat —
the tool is only as good as the underlying model on any given call.

Meanwhile, every translation that passes the oracle (`cargo check` exit 0) is a
piece of **ground truth**: a `(source → Rust)` pair we *know* is valid Rust. We
throw these away.

## Goal

Make rustyfi **get better the more it runs** by feeding its own oracle-verified
`(source → Rust)` pairs back into translation as retrieved few-shot examples.
More runs → more verified pairs → better first-shot translations → more verified
pairs. The corpus is the compounding asset and the moat.

## Non-Goals (YAGNI)

- **Item-level pairs** (per function/struct). v1 is per-source-file. Item-level
  is the planned evolution once v1 shows a measured lift.
- **Embedding / semantic retrieval.** Adds a model/key dependency that breaks the
  $0/no-key ethos. v1 retrieval is deterministic and structural.
- **Doctor/fix-loop injection.** v1 injects at translation time only (where the
  variance originates). Fix-loop injection is future work.
- **Library `(source → Rust)` pairs with behavioral provenance.** Tied to the
  separate library-oracle effort; v1 harvests whatever clean crates exist.
- **A measured clean-rate lift.** That A/B requires a model key and is gated as a
  separate validation step (see Testing). v1 ships the mechanism + a real seed
  corpus + unit proofs; the lift is measured when a key is available.

## Locked Decisions

| Decision | Choice |
|---|---|
| Verification bar | **Gate** = crate passes `cargo check` (exit 0). **Tier** = pairs from runs that also passed the behavioral oracle rank higher. |
| Corpus home | **Both**: a curated, regenerable seed in the repo (`corpus/seed.jsonl`) **and** a local cache (`~/.cache/rustyfi/corpus.jsonl`) appended from the user's oracle-passed runs. |
| Merge precedence | Local outranks seed; within a tier, behavior-tier outranks compile-tier. |
| Granularity (v1) | **Per-source-file pairs**, recovered for free from the `// <<<rustyfi:src=…>>>` sentinels translated Rust already carries. |
| Retrieval signal | **Structural**: source-side API surface (imported modules + called external symbols) + source language. No embeddings. |
| Injection point | Translation prompt, under a dedicated context budget. |

## Correctness insight from the divergence probe (2026-06-14)

The probe proved that a crate can pass `cargo check` and still diverge
behaviorally — calculator compiles clean yet formats floats wrong
(`{:.10}` vs Go `%g`). Therefore:

> **A compile-tier pair is "valid Rust, behavior unverified" — it can encode a
> behavioral bug.** Copying it blindly would propagate that bug.

Consequences baked into this design:

1. Behavior-tier pairs are **strongly** preferred in ranking, not marginally.
2. When a compile-tier pair is injected, the prompt labels it honestly:
   *"compiles; behavior not verified — match the structure, not necessarily every
   literal."* The model is told the example proves *compilability*, not sameness.
3. The tier is a first-class field on every corpus entry, not metadata.

## Architecture

New module `crates/rustyfi-engine/src/corpus/`, four small units with narrow,
independently-testable interfaces:

```
corpus/
├── mod.rs        # public surface: CorpusEntry, Retriever, load/merge, retrieve
├── harvest.rs    # clean crate (out + src) → Vec<CorpusEntry>
├── signal.rs     # source code → ApiSurface (imports + called symbols)
├── retrieve.rs   # ApiSurface query → ranked top-K CorpusEntry
└── store.rs      # JSONL read/write/merge (seed + local cache)
```

### Data model

```rust
struct CorpusEntry {
    source_lang: String,        // "go" | "python" | "typescript" | ...
    source_api: Vec<String>,    // sorted, deduped API-surface symbols
    source_code: String,        // the source file
    rust_code: String,          // the verified Rust translation
    provenance: Provenance,     // { crate, file }
    tier: Tier,                 // Compile | Behavior
}
enum Tier { Compile, Behavior }
```

On-disk format: **JSONL**, one `CorpusEntry` per line (append-friendly,
diff-friendly, mergeable, human-auditable).

### `signal.rs` — the API surface (the emergent ontology)

`api_surface(source_code, lang) -> ApiSurface` extracts the symbols that drive
the hard translation choices: imported modules and called external functions
(e.g. Go `fmt.Printf`, `strconv.ParseFloat`; JS `axios.get`). This is the
*learned* ontology — "how did we translate code that uses this API before" —
emerging from data instead of a hand-authored table. Reuses the existing
language-aware head/import scanning in `deps.rs`/`analysis.rs`; this unit adapts
and extends it, it does not fork a parallel scanner.

### `harvest.rs` — mining verified pairs

`harvest_crate(out_dir, src_dir) -> Vec<CorpusEntry>`:

1. Gate: confirm the crate passes `cargo check` (exit 0). If not, yield nothing.
2. Split each generated Rust module by `// <<<rustyfi:src=PATH>>>` sentinels into
   per-source-file regions.
3. For each region, load the matching source file → build a `CorpusEntry`
   (compute `source_api` via `signal.rs`, set `source_lang` from the file).
4. Tier = `Behavior` if a passing `behavior_report.json` exists for the crate,
   else `Compile`.

Harvest is deterministic and runs on crates already on disk — no pipeline run and
no key required.

### `retrieve.rs` — ranking

`Retriever::top_k(query: ApiSurface, lang, k) -> Vec<&CorpusEntry>`:

- Candidate filter: same `source_lang`.
- Score: **Jaccard similarity** of the query and candidate `source_api` sets.
- Tie-breaks, in order: behavior-tier > compile-tier; local > seed; higher
  Jaccard; shorter `source_code` (cheaper to inject).
- Returns top-K (**K defaults to 3**).

### `store.rs` — persistence and merge

- `load() -> Retriever` reads `corpus/seed.jsonl` (repo) then
  `~/.cache/rustyfi/corpus.jsonl` (local), merging with local-outranks-seed.
- `append(entries)` writes new oracle-passed pairs to the local cache.
- Path of the local cache honours `XDG_CACHE_HOME`; falls back to
  `~/.cache/rustyfi/`. Failures are non-fatal (corpus is an enhancement, never a
  hard dependency — fail-open, same posture as every other optional pass).

## Data flow (translation time)

1. `pipeline::run` builds the `Retriever` once (load seed + local) and threads it
   into `phase_translate`. If loading fails, translation proceeds exactly as today
   (fail-open).
2. Per file, before the translate call: compute the file's `ApiSurface`, retrieve
   top-K pairs, and splice them into the **existing** context slot as few-shot
   ("here is verified Rust for source with a similar API surface"), under a new
   `CORPUS_CTX_BUDGET` (mirrors `CONTRACT_CTX_BUDGET`, ~8 KB). Compile-tier
   examples carry the honesty label from the correctness insight above.
3. After a run reaches the oracle bar, harvest the new pairs and `append` them to
   the local cache. **The flywheel closes.**

## Tooling / first concrete artifacts (this session, $0)

- `examples/harvest_corpus.rs` — regenerate `corpus/seed.jsonl` from the clean
  bench crates on disk. Produces the **first real seed corpus** as a committed
  artifact.
- `examples/retrieve_demo.rs` (optional) — given a source file, print the top-K
  pairs that would be injected. Makes the retrieval visible/reviewable without a
  translation run.

## Testing strategy

**$0, fully provable now (unit + example):**
- `harvest`: from a fixture clean crate (and the real ky/calculator/itsdangerous/
  emoji-java on disk) → correct pairs, correct tiers, correct sentinel splitting.
- `signal`: known source → expected API surface (per language).
- `retrieve`: a fixed corpus + query → expected ranking, including tier and
  local/seed precedence tie-breaks.
- `store`: JSONL round-trip; seed+local merge precedence; missing-cache fail-open.

**Key-gated (separate validation, when a model key is available):**
- The lift A/B: translate a held-out crate **with** vs **without** the corpus,
  measured by **binary compile (exit 0)** — the honest metric. This is the
  experiment that proves the discovery; it is explicitly not part of the v1
  ship and does not block it.

## Risks & mitigations

- **Cold start / thin corpus.** Only ~4 clean crates exist today, so retrieval
  often returns weak or no matches. Mitigation: fail-open (no match → translate
  as today); the seed grows as the bench and users run. v1's job is the
  *mechanism* + proof, not a big lift.
- **A pair teaches a behavioral bug** (the probe insight). Mitigation: tier
  preference + honesty labelling above; behavior-tier pairs are the trustworthy
  ones, and the library/behavior work will grow that tier.
- **Token cost of injection.** Mitigation: `CORPUS_CTX_BUDGET` cap + prefer
  shorter examples + small K.
- **Retrieval is too coarse at file granularity.** Accepted for v1; item-level is
  the planned next step, gated on a measured lift.

## Open / deferred (post-v1)
- Item-level pairs (needs source-side AST + source↔Rust alignment).
- Fix-loop / doctor injection.
- Coverage-driven corpus quality scoring.
- Library `(source → Rust)` pairs once the library oracle exists.
