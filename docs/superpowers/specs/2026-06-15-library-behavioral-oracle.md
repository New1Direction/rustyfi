# Library Behavioral Oracle — thin API-driver (PROVEN prototype)

- **Date:** 2026-06-15
- **Status:** Concept proven end-to-end; productization path defined
- **Why:** The behavioral oracle verifies *executables* (stdout diff) but the bulk
  of real targets are **libraries** (ky, axios, itsdangerous, emoji-java). They
  have no CLI entrypoint, so the oracle skipped them (and previously *fabricated*
  false mismatches — fixed in `64ee884`). This makes the oracle verify libraries.

## The approach

Wrap the library's public API in a **thin driver** — a few function calls with
literal inputs, printing labeled, deterministic output (hex for bytes) — in
**both** the source language and Rust. Then the existing stdout-diff oracle does
the rest. A library becomes a CLI via a synthesized entrypoint; the surface is
small, so generating it in two languages is tractable.

## Proof (this session, itsdangerous, $0 — python3 already present)

Hand-written driver pair against `Signer`, Python source vs translated Rust crate:

| check | result |
|---|---|
| `sign` — 6 cases (default, empty, unicode/emoji, 100-char key, custom salt, custom sep) | ✅ byte-identical |
| `unsign` round-trip | ✅ recovers value |
| tamper-rejection (flip a signature byte → must reject) | ✅ rejected both sides |

Identical HMAC-SHA1 + django-concat key derivation + base64 signature + separator.
First behavioral verification of a *library* in this project. The same harness
would have flagged a real divergence (e.g. a dropped `b"signer"` in key
derivation → different hex) or a security regression (tamper ACCEPTED).

## Productization path (the next increment)

1. **Generate the driver pair.** Given the library's public API (the Rust
   contract already pinned by the pipeline + the source files), have the model
   write a small **source** driver exercising N public functions with literal
   inputs and printing labeled canonical output. Then produce the **Rust**
   equivalent — best done by *translating the source driver with rustyfi itself*
   (small file → its sweet spot), so it references the already-translated Rust
   API by the contract's names.
2. **Wire into the harness.** In `behavior/recipe.rs`, when `source_side` finds no
   CLI entrypoint (the library case that now honestly skips), synthesize the
   driver entrypoint instead of returning `None`. Source side runs the source
   driver; target side runs the Rust driver; existing `diff_case` compares.
3. **Determinism + fairness.** Print bytes as hex (avoids language repr
   differences). Compare accept/reject for error paths, not error *messages*
   (which legitimately differ across languages). Mark cases that hit a `todo!()`
   stub as "incomplete," distinct from "diverged."

## Risks / mitigations

- **Driver equivalence drift** (two independently generated drivers disagree on
  cases/format). → Generate ONE source driver, translate it to Rust via rustyfi;
  one source of truth.
- **Side-effecting / non-deterministic APIs** (HTTP clients like ky/axios, time,
  RNG). → Restrict generated drivers to pure/deterministic public functions;
  quarantine the rest (the harness already quarantines nondeterministic cases).
- **Stubs panic.** → Catch and report as "incomplete translation," not a false
  behavioral diff.

## Automation — PROVEN end-to-end (2026-06-15)

The full loop runs with **zero hand-written code** (DeepSeek `deepseek-chat`,
`/tmp/auto_driver2.py`):
1. Generate the **source** (Python) driver from the API → run it for golden.
2. **Port** it to a Rust driver against the contract → compile (repair loop on
   failure) → run.
3. Diff. **Result: byte-identical** on itsdangerous Signer (sign/unsign/tamper).

The model independently picked a deterministic API path and matched output exactly;
the generated Rust driver compiled first try (correctly using the crate's `hex`
dep). The riskiest part of the feature — "can a model produce a Rust driver that
compiles and matches the source" — is **de-risked**.

## Remaining work (mechanical, not research)

Wire the proven flow into the engine:
- Factor the orchestration (`auto_driver2.py`) into Rust using the existing
  `LlmClient`, `build_contract` (syn extraction, already in `flywheel_ab`), and
  `extract_rust_code`.
- In `behavior/recipe.rs`, the library branch of `source_side` synthesizes the
  driver pair + entrypoint instead of returning `None`; run the source driver for
  golden, run the Rust driver, hand to the existing `diff_case`.
- Per-language source-driver runners (python: PYTHONPATH; node: index; etc.).
- Repair loop on Rust-driver compile failure (cap attempts); quarantine
  non-deterministic / `todo!()`-hitting cases as "incomplete," not "diverged".
