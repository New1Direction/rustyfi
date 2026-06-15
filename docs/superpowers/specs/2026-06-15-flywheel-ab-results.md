# Translation Flywheel — A/B Results (REFUTED at file granularity)

- **Date:** 2026-06-15
- **Status:** Measured — the file-level flywheel shows no reliable lift
- **Branch:** `feat/translation-flywheel` (PR #2)
- **Cost:** < $1 total (DeepSeek `deepseek-chat` via API)

## Question

The flywheel ships as "the corpus is the compounding asset and the moat." That
claim was never measured. Does injecting oracle-verified `(source → Rust)` pairs
as few-shot examples actually lower the first-shot error count?

## Method (cheapest evidence first)

1. **$0 retrieval dry-run** (`examples/retrieve_dryrun.rs`) — leave-one-out over
   each clean crate's sentinel regions; print retrieved neighbors + Jaccard.
2. **Smoke A/B** — single seed, found two confounds (below).
3. **Fair A/B** (`examples/flywheel_ab.rs`) — file-level leave-one-out on
   `ky-doctored` (clean, baseline 0 errors), 9 regions × 3 seeds, DeepSeek.
   Both arms receive an identical **contract** (the crate's pub type/sig surface,
   syn-extracted) so neither is type-blind; the corpus (the *other* verified
   pairs, via the real `build_corpus_context`) is the ONLY difference between
   arms. Each translation is spliced back into the otherwise-verified crate and
   `cargo check`-counted.

## Confounds found and fixed

- **Wrong crate variant.** `bench/.work/out/ky` is the *pre-doctor* 12-error
  crate; the clean one is `ky-doctored` (0 errors). Harvesting `ky` yields
  "verified" pairs that aren't verified. Fixed: measure on `ky-doctored`.
- **Type-blind baseline.** Without a contract, the OFF arm has no type info, so a
  big file (`Ky.ts`) cold-translates to 169 errors and the corpus "wins" merely
  by smuggling the crate's types in via sibling examples. Fixed: give both arms
  the contract; the corpus is then the only differing variable.

## Result

| file (held out) | OFF mean[min..max] | ON mean[min..max] | effect |
|---|---|---|---|
| `Ky.ts` | 57.0 [17..118] | 73.0 [19..160] | **+16 hurt** |
| `constants.ts` | 15.7 [14..17] | 7.7 [7..9] | −8 helped |
| `NetworkError.ts` | 5.3 [4..6] | 2.0 [2..2] | −3.3 helped |
| `HTTPError.ts` | 5.7 [4..7] | 4.0 [4..4] | −1.7 small |
| `SchemaValidationError.ts` | 3.7 | 3.3 | noise |
| `ForceRetryError.ts` | 2.3 | 2.0 | noise |
| `NonError.ts` | 3.0 | 3.0 | 0 |
| `KyError.ts` | 2.0 | 3.3 | +1.3 hurt |
| `TimeoutError.ts` | 2.3 [1..3] | 13.0 [13..13] | **+10.7 hurt** |

**Mean: OFF 10.78 → ON 12.37 — delta +1.59. The corpus did not help; it slightly hurt.**

## Why

1. **Variance dwarfs the effect.** `Ky.ts` ranges 17–160 errors across seeds
   regardless of arm. The smoke single-seed caught `Ky.ts` 95→17 — a pure-noise
   "win." A single-seed A/B would have *falsely* reported the flywheel a success.
2. **Retrieval is a boilerplate matcher.** Dry-run top-1 Jaccard was 0.80 between
   near-duplicate error classes but **0.059** for the one substantive file
   (`Ky.ts`). Whole-file API-surface overlap captures boilerplate, not logic.
3. **High-confidence retrieval is miscalibrated.** `TimeoutError` had the single
   highest retrieval confidence (J=0.80 to `NetworkError`) and the corpus
   degraded it the most — reproducibly (13/13/13). Best match → worst outcome.

## Honesty caveat (the result is, if anything, optimistic)

Leave-one-out *within* one crate means the retrieved examples are same-crate
siblings (shared conventions, helpers, types) — which the real *cross-crate*
flywheel would not have. The genuine production case (corpus from crate A,
translating unrelated crate B) would retrieve even less relevant pairs. So
+1.59 is an **upper-bound-flavoured** non-result.

## Recommendation

- **Do not sell the file-level flywheel as a moat.** It has no measured lift.
- **Land PR #2 as experimental, off-by-default-in-effect** (fail-open; a thin
  corpus is already a no-op; `RUSTYFI_NO_FLYWHEEL` disables it). The code and the
  measurement tooling (`retrieve_dryrun`, `flywheel_ab`, `passes_measure`) are
  worth keeping; the *claim* is not.
- **Item-level granularity** (the design's deferred crux) is NOT worth building
  on the strength of this: item-level produces *more* high-confidence matches,
  and high-confidence matches are exactly what hurt here. Gate any future work on
  a $0 item-level retrieval probe first.
- The deterministic verify passes are the better bet, but note they are proven
  *safe* (compiler-gated), not yet proven *beneficial* — measuring their true
  contribution needs ablation toggles + re-translation (see `passes_measure`).
