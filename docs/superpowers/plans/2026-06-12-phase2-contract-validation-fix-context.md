# Phase 2: Contract Validation + Fix Context Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Eliminate contract-phase variance (the 42-vs-179 lottery) by compiler-validating contracts before fan-out, and shrink the E0277 error class by feeding real type/trait context into fix prompts.

**Architecture:** Two new engine modules. `contract_check.rs` assembles a throwaway skeleton crate from all package contracts (data surfaces + `todo!()`-bodied signatures), cargo-checks it, and drives regeneration of failing packages before anything fans out. `fix_context.rs` builds a per-file context block (syn item index + `rustc --explain` + trait impl listings) threaded into `prompt_fix_targeted`. Plus a one-line `[workspace]` fix in the scaffolder.

**Tech Stack:** Rust; syn 2 (already a dep, span byte_range pattern proven in dedup_items.rs); `rustyfi_core::compiler::run_cargo_check` for all checks.

**Spec:** `docs/superpowers/specs/2026-06-11-compile-clean-10x-design.md` §2 + baseline findings (commit de6b6b8: unvalidated contract → 19×E0038 cascade).

**Conventions:** work from repo root on branch `feat/phase2`; every task ends with `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all` clean; commits `<type>: <description>`, no attribution footer.

**Key existing signatures (verified):**
- `phase_contract` at `pipeline.rs:562` — iterates packages, per pkg: `llm.complete(SYSTEM_CONTRACT, prompt_extract_contract(...))` → `extract_rust_code` → `slicer::split_contract(&s) -> (data_surface, signatures)` → `repair_module_refs` → `dedup_top_level_items` → `scaffolder.write_package_contract(&root, &data)` (writes immediately, no validation, errors skip the pkg)
- `PackageContract { root_segment, package, is_entrypoint, data_surface, signatures }` (`checkpoint.rs:177`)
- `phase_verify` LLM loop at `pipeline.rs:~1300-1360`: per errored file: read source → `prompt_fix_targeted(rust_code, errors, families)` (`llm.rs:669`) → complete → write. Diagnostics: `rustyfi_core::compiler::parse_cargo_diagnostics` → `Vec<CompilerDiagnostic>` (spans with file_name/is_primary, message, code).
- `run_cargo_check(workspace)` (`rustyfi-core/src/compiler.rs:24`), `rustfix::error_count` pattern (`rustfix.rs:107`).
- Temp-dir pattern: `std::env::temp_dir().join(format!("rustyfi_<purpose>_{}", std::process::id()))`, or `tempfile::TempDir` (dep available).

---

### Task A: `[workspace]` table in generated Cargo.toml (quick win first)

**Files:** Modify `crates/rustyfi-engine/src/scaffold.rs` (the `scaffold()` method where Cargo.toml content is built); test in same file.

- [ ] **A1:** Write failing test: scaffold a crate into a tempdir, assert the generated `Cargo.toml` contains `\n[workspace]\n` (empty table). Follow the existing scaffold test style in scaffold.rs.
- [ ] **A2:** Run it → FAIL. Add `[workspace]\n` (with a comment `# empty table: keep this crate out of any enclosing workspace`) to the Cargo.toml template string in `scaffold()`. Run → PASS.
- [ ] **A3:** Full check (test/clippy/fmt). Commit: `fix: empty [workspace] table in generated crates (prevents parent-workspace capture)`

### Task B: `contract_check.rs` — skeleton validation + regeneration loop

**Files:** Create `crates/rustyfi-engine/src/contract_check.rs`; modify `crates/rustyfi-engine/src/lib.rs` (add `pub mod contract_check;`), `crates/rustyfi-engine/src/pipeline.rs` (`phase_contract` restructure), `crates/rustyfi-engine/src/llm.rs` (retry prompt).

**Module API (implement exactly):**

```rust
//! Compiler-validate package contracts BEFORE translation fan-out.
//! The contract is the highest-leverage LLM output in the pipeline: every file
//! inherits it, so a structurally broken contract (e.g. a dyn-incompatible
//! trait used behind Box<dyn …>) multiplies into dozens of errors. cargo is
//! the oracle — same principle as the rustfix pass, moved upstream.

pub struct ContractIssue {
    pub root_segment: String,
    /// Rendered compiler errors attributable to this package's mod.rs (capped).
    pub errors: String,
}

/// Build a throwaway skeleton crate from ALL contracts and cargo-check it.
/// Returns per-package issues (empty = all structurally sound).
pub fn check_contracts(
    contracts: &[PackageContract],
    crate_name: &str,
) -> Result<Vec<ContractIssue>, EngineError>
```

Behavior:
1. Skeleton dir via `tempfile::TempDir` (auto-cleanup). Layout: `Cargo.toml` (name = `<crate_name>_skeleton`, edition 2021, `[workspace]` table), `src/lib.rs` = `#![allow(unused, dead_code)]` + `pub mod <root_segment>;` per non-entrypoint contract, `src/<root>/mod.rs` = `data_surface` + `\n// --- signatures ---\n` + `stub_bodies(&signatures)`.
2. `stub_bodies(sigs: &str) -> String`: each contract signature line ends with `;` (split_contract guarantees `sig;` form) → rewrite to `sig { todo!() }`. Non-fn lines pass through unchanged. Handle multi-line sigs by splitting on `;` only at brace-depth 0 (simple char scanner — skip strings/chars; the slicer emits flat sigs so this is defensive).
3. Detect external deps the contracts reference: reuse `crate::deps::scan_crate_heads` + the registry (same calls the scaffolder uses via `add_registry_deps` — read its implementation and mirror the minimal dep-table write into the skeleton Cargo.toml). This keeps `serde`/`tokio`-using contracts checkable.
4. `run_cargo_check(skeleton_path)` → `parse_cargo_diagnostics`. **Filter the verdict**: ignore diagnostics whose code is `E0432`, `E0433`, `E0405`, `E0412` (unresolved imports/paths/traits/types — external-world resolution the skeleton can't fully model) AND ignore anything in `src/lib.rs`. Everything else (E0038 object-safety, E0072, E0119, E0107, syntax errors…) attributes to a package by primary-span path `src/<root>/mod.rs`; render those errors (cap 4_000 bytes per package) into `ContractIssue`s.
5. Unit-testable helpers must be pure: `stub_bodies`, `skeleton_layout(contracts) -> Vec<(PathBuf, String)>` (path→content pairs), `attribute_issues(diags, roots) -> Vec<ContractIssue>` with the code filter.

**phase_contract restructure (pipeline.rs):**
- Collect all `PackageContract`s in memory first (today's per-pkg loop, minus the `write_package_contract` call).
- Then: `let issues = contract_check::check_contracts(&contracts, &config_crate_name)?;` — on issues, for each failing package, re-call the LLM with `prompt_extract_contract_retry(pkg, lang, labeled_source, &prev_contract_text, &issue.errors)` (new fn in llm.rs — same as `prompt_extract_contract` plus: previous contract, the compiler errors, and the instruction block: "Your previous API did not compile. Fix ONLY the structural problems shown. If a trait is used as `dyn Trait` it must be object-safe: no generic methods, no `Self`-returning methods without `where Self: Sized`. Re-emit the COMPLETE corrected API."). Re-split/repair/dedup the replacement. Max **2** validation rounds total (initial + 2 retries); after that, keep the best round (fewest issues) and emit a `Progress::Note` warning naming the packages that remain unvalidated — never abort the run for this.
- After validation: write all surfaces via the existing `write_package_contract` loop, checkpoint as today.
- Progress notes: `"Compiler-checking the type contract before translation…"`, `"Contract for '<pkg>' failed validation — regenerating (round N)…"`, `"Contract validated clean ✓"` / warning variant.
- Resume/back-compat: `ContractCheckpoint` unchanged (validation happens before checkpoint write; a resumed run with an existing checkpoint skips validation exactly as it skips generation).

- [ ] **B1:** Failing unit tests first (in `contract_check.rs` `#[cfg(test)]`): `stub_bodies` turns `pub fn get(&self, k: &str) -> Option<String>;` into `pub fn get(&self, k: &str) -> Option<String> { todo!() }` and passes structs through; `skeleton_layout` produces lib.rs with the mod decls + per-root mod.rs with surface+stubs; `attribute_issues` maps a fabricated E0038 diagnostic on `src/storage/mod.rs` to root `storage` and DROPS a fabricated E0433.
- [ ] **B2:** Implement the pure helpers → tests PASS.
- [ ] **B3:** Implement `check_contracts` (tempdir + deps + run_cargo_check + attribute). Add `#[ignore]`d e2e test: two hand-written contracts — pkg `bad` with `pub trait Provider { fn get<T>(&self) -> T; }` + `pub struct S; ` + signature `pub fn make() -> Box<dyn Provider>;` (→ must yield an E0038 issue for `bad`), pkg `good` with a plain struct + sigs (→ no issue). Run with `cargo test -p rustyfi-engine contract_check -- --ignored` → PASS.
- [ ] **B4:** llm.rs: add `prompt_extract_contract_retry` (unit test: contains the previous contract, the errors, and the object-safety instruction). Strengthen `SYSTEM_CONTRACT` with one added line: "If the API exposes a trait via `Box<dyn …>` or `&dyn …`, that trait MUST be object-safe (no generic methods)."
- [ ] **B5:** Restructure `phase_contract` per above. The existing 66 engine tests must stay green (especially contract checkpoint tests).
- [ ] **B6:** Full check + commit: `feat: compiler-validate package contracts before translation fan-out`

### Task C: `fix_context.rs` — context-enriched fix prompts

**Files:** Create `crates/rustyfi-engine/src/fix_context.rs`; modify `lib.rs` (`pub mod fix_context;`), `llm.rs` (`prompt_fix_targeted` gains a param), `pipeline.rs` (phase_verify LLM loop wiring).

**Module API (implement exactly):**

```rust
//! Per-file context for the compile-fix prompt: the fixer finally SEES the
//! trait the compiler says is unsatisfied, the type defined two modules away,
//! and rustc's own explanation — instead of one file + a bare error string.

pub struct ItemIndex { /* name -> Vec<ItemDef { kind, source_text }>, impls: Vec<ImplDef { trait_name, type_name, source_text }> */ }

impl ItemIndex {
    /// Parse every .rs under <ws>/src with syn; collect top-level items by name
    /// using span byte ranges over the original source (the dedup_items pattern;
    /// files that fail to parse are skipped). Build once per fix cycle.
    pub fn build(workspace: &Path) -> ItemIndex;

    /// Context block for fixing `file`, given its diagnostics. Budget-capped.
    pub fn context_for(
        &self,
        file: &Path,
        diags: &[CompilerDiagnostic],
        budget: usize,
    ) -> String;
}

pub const FIX_CTX_BUDGET: usize = 8_000;
```

`context_for` behavior:
1. Identifier harvest: every `` `backticked` `` token in the messages of diagnostics whose primary span is `file`; split paths (`a::b::C` → keep last segment `C`); dedupe; drop identifiers DEFINED in `file` itself (the fixer already sees that file).
2. For each harvested identifier with index entries: append `// definition of <name> (from <relative path>)\n<source_text>`.
3. For diagnostics with code E0277/E0038/E0599: for each harvested identifier that is a TRAIT in the index, also append every impl block whose trait_name or type_name matches: `// existing impls involving <name>\n…`.
4. `rustc --explain <code>` excerpt (first 40 lines) for each distinct error code, via a process-global cache (`OnceLock<Mutex<HashMap<String,String>>>`); tolerate rustc absence (skip silently — the pipeline already requires cargo, but never panic).
5. Assembly priority under budget: definitions first, then impls, then explains; truncate whole sections, never mid-item; if nothing harvested, return an empty string (prompt unchanged).

**Wiring:**
- `prompt_fix_targeted(rust_code, errors, families, fix_context: &str)` — insert after the family-hints block: when non-empty, a section headed `"Relevant project definitions (CANONICAL — match these exactly, do not redefine):"`.
- phase_verify LLM loop: `let index = fix_context::ItemIndex::build(ws);` once per cycle (after diagnostics parse), then per file `let ctx = index.context_for(&file, &diags, fix_context::FIX_CTX_BUDGET);` → pass to the prompt. Errors building the index = empty context, never fatal.

- [ ] **C1:** Failing unit tests (tempdir with two hand-written .rs files): index finds `struct CacheEntry` + `trait Provider` + an `impl Provider for X`; identifier harvest pulls `` `Provider` `` and `` `cache::CacheEntry` ``→`CacheEntry` from fabricated diagnostics; `context_for` includes the trait def + impl for an E0277 diag; budget of 50 truncates to sections that fit; identifiers defined in the target file are excluded.
- [ ] **C2:** Implement; tests PASS.
- [ ] **C3:** Wire into llm.rs + pipeline.rs; update the one existing `prompt_fix_targeted` unit test (if any) for the new param; add a test asserting the prompt contains the CANONICAL header when context is non-empty and omits it when empty.
- [ ] **C4:** Full check + commit: `feat: context-enriched fix prompts (item index + rustc --explain + trait impls)`

### Task D: Phase 2 gate — suite rerun

- [ ] **D1:** Merge-readiness: final review of the whole branch (controller does this via subagents), then full local verification.
- [ ] **D2:** Controller (not a subagent) reruns the benchmark suite with the same env as the baseline and compares `bench/RESULTS.md`: gate = clean rate ≥ baseline 4/9 AND prompt-cache + ky materially improved AND no repo regresses to pipeline-failure. Commit the new RESULTS.md with a comparison table in the commit message.
- [ ] **D3:** Merge `feat/phase2` to main (push needs user approval). Phase 3 may begin only after D2 passes.

---

## Out of scope

The agentic deep-fix (`agent_fix.rs`) is Phase 3 and gets its own plan after the D2 gate. No web-UI changes. No new benchmark repos.
