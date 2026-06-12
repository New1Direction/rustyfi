# Phase 3: Agentic Deep-Fix ("rustyfi doctor") Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A budget-capped, compiler-converging agentic fix phase that grinds residual errors to zero regardless of which translation the dice produced — the variance-beating closer.

**Architecture:** `agent_fix.rs` runs a ReAct loop on the existing `LlmClient`: the model gets read/search/check/write tools over the workspace, edits confined to `src/`, snapshot-reverted at phase level if the error count doesn't strictly improve. Runs after the cheap loop + rustfix, opt-in (`--deep` / `RUSTYFI_DEEP_FIX=1`). Plus Task 0: item-preserving contract regeneration (the cobra 1→118 finding from the Phase 2 gate).

**Strategic context (from the Phase 2 gate, 2026-06-12):** single-shot translation through a noisy provider swings ±100 errors per repo run-to-run. Contract validation killed the catastrophic mode (prompt-cache 179→15) but ordinary variance remains (paint 0→10 with no Phase 2 machinery involved). The doctor is the convergence mechanism: it doesn't matter what came out of translation — `cargo check` is ground truth and the loop only terminates on improvement-or-revert. Pair it with a strong model behind `RUSTYFI_FIX_MODEL` (every measurement so far says fix-model quality is the binding constraint).

**Spec:** `docs/superpowers/specs/2026-06-11-compile-clean-10x-design.md` §3.

**Conventions:** branch `feat/phase3` off the merged main; per task: full TDD, `cargo test --workspace` green, clippy `-D warnings` clean, fmt clean; one commit per task, `<type>: <description>`, no attribution footer.

---

### Task 0: Item-preserving contract regeneration (Phase 2 gate finding)

**Problem:** when contract validation regenerates a failing package, the model "fixes" the structural error but silently drops items — cobra's nine regenerated contracts lost methods, turning 1 residual error into 118 E0599s.

**Files:** Modify `crates/rustyfi-engine/src/llm.rs` (`prompt_extract_contract_retry`), `crates/rustyfi-engine/src/contract_check.rs` (new pure helper), `crates/rustyfi-engine/src/pipeline.rs` (retry acceptance check).

- [ ] **0.1:** Failing unit test for new pure helper in contract_check.rs: `pub fn item_names(contract_rust: &str) -> BTreeSet<String>` — syn-parse (fall back to empty set on parse failure) and collect names of top-level fns/structs/enums/traits/types/consts AND trait-method names (as `Trait::method`). Test on a small contract string.
- [ ] **0.2:** Implement → green.
- [ ] **0.3:** `prompt_extract_contract_retry` gains the original item inventory: append a section `"The corrected API MUST still define ALL of these items (do not drop any):"` followed by the sorted item-name list from the previous contract. Unit test asserts presence.
- [ ] **0.4:** Acceptance check in the pipeline retry path: after re-split/repair/dedup of a regenerated contract, compute `item_names(old)` − `item_names(new)`; if the regenerated contract drops more than 10% of the old items (and the old set was non-empty), REJECT the regeneration for that package — keep the old contract for that package and log a Note ("regenerated contract for '<pkg>' dropped N items — keeping the original"). The structural error stays, but a structural error in one package beats an API amputation consumed by every other package. Unit-test the set-difference threshold logic as a pure helper (`fn regeneration_acceptable(old: &BTreeSet<String>, new: &BTreeSet<String>) -> bool`).
- [ ] **0.5:** Full gates + commit: `fix: contract regeneration must preserve the API surface (cobra finding)`

### Task 1: `agent_fix.rs` — the tool loop core (no LLM yet)

**Files:** Create `crates/rustyfi-engine/src/agent_fix.rs`; modify `lib.rs`.

The module is built transport-last so everything below is unit-testable without a model:

```rust
pub struct DoctorBudget { pub max_tool_calls: usize, pub max_wall_secs: u64 }   // defaults 40 / 1200
pub enum ToolCall { ListFiles, ReadFile{path:String}, Search{symbol:String}, CargoCheck,
                    Explain{code:String}, WriteFile{path:String, content:String}, Done{summary:String} }
pub struct ToolOutcome { pub payload: String, pub is_terminal: bool }

pub struct DoctorSession { /* workspace, budget, calls_used, started, item_index … */ }
impl DoctorSession {
    pub fn new(workspace: &Path, budget: DoctorBudget) -> Self;
    /// Execute one tool call against the workspace. Pure dispatch + guards.
    pub fn execute(&mut self, call: ToolCall) -> ToolOutcome;
    pub fn budget_exhausted(&self) -> bool;
}
```

Guards (each unit-tested):
- `WriteFile` path must resolve under `<ws>/src` (reject `..`, absolute paths outside, symlink escape via canonicalize-parent); rejection returns an error payload to the model, not a panic.
- `ReadFile` confined to the workspace (src + Cargo.toml + NEXT_STEPS.md); payload capped 24_000 bytes (tail-truncated with a marker).
- `CargoCheck` returns rendered errors capped 8_000 bytes + the error count; reuses `run_cargo_check`/`parse_cargo_diagnostics`.
- `Search` uses `fix_context::ItemIndex` (rebuild after every `WriteFile`).
- `Explain` reuses fix_context's cached `rustc --explain`.
- Every call increments `calls_used`; `execute` returns terminal outcome when `Done` or budget exhausted.

- [ ] **1.1:** Failing tests for all guards + dispatch (tempdir with a tiny crate).
- [ ] **1.2:** Implement → green. Commit: `feat: doctor session core — guarded tool loop for the deep-fix phase`

### Task 2: Transport — native tool-calling with JSON-action fallback

**Files:** Modify `crates/rustyfi-engine/src/llm.rs` (tool-call request/response support on `complete`-family), `agent_fix.rs` (the driver).

- [ ] **2.1:** llm.rs: add `complete_with_tools(system, messages, tools_json) -> Result<AssistantTurn, EngineError>` where `AssistantTurn` is either `ToolInvocation{name, arguments_json}` or `Text(String)`. OpenAI-compatible `tools`/`tool_calls` wire format (DeepSeek + OpenRouter + OpenAI all speak it). Multi-turn: messages vec carries the running conversation incl. `role:"tool"` results. Unit tests on request-body serialization + response parsing from canned JSON fixtures (no network).
- [ ] **2.2:** Fallback protocol for providers without tools: system prompt instructs ONE action per reply as a fenced JSON object `{"tool": "...", "args": {...}}`; strict parse; one malformed-reply reprompt, second failure ends the session. Pure parser unit-tested (valid/malformed/extra-prose cases).
- [ ] **2.3:** The driver: `pub fn run_doctor(ws, llm, budget, progress_cb) -> DoctorReport` — SYSTEM_DOCTOR prompt (role: senior Rust engineer fixing a translated crate; cargo check is ground truth; fix root causes; never delete functionality to silence errors; call done when clean or stuck), seeded with the current error rendering + NEXT_STEPS.md. Loop: model turn → execute → feed result back → repeat until Done/budget/terminal. `DoctorReport { start_errors, end_errors, tool_calls_used, wall_secs, summary }`. Progress notes per tool call ("doctor: reading src/cache/mod.rs…"). Integration test with a SCRIPTED fake transport (inject a closure transport for tests — add a `#[cfg(test)]` constructor or a small trait) walking a full session: check → read → write a fix → check clean → done.
- [ ] **2.4:** Full gates + commit: `feat: doctor transport and driver (native tool-calling + JSON fallback)`

### Task 3: Pipeline + CLI wiring, snapshot safety

**Files:** Modify `pipeline.rs` (phase_verify tail), `crates/rustyfi-cli/src/main.rs` (`--deep` flag), `llm.rs` (env plumbing if needed).

- [ ] **3.1:** phase_verify: after the existing fix loop + rustfix sweep, if `!clean && deep_fix_enabled()` (`RUSTYFI_DEEP_FIX=1`): snapshot `<ws>/src` (recursive copy to a TempDir), run `run_doctor` with the FIX client (`LlmClient::for_fixing()`), budgets from `RUSTYFI_DEEP_FIX_BUDGET` (tool calls) / `RUSTYFI_DEEP_FIX_TIMEOUT` (wall secs); after: final cargo check — if end error count is NOT strictly lower than start, restore the snapshot wholesale and Note the revert; if lower, keep and Note the improvement. NEXT_STEPS.md records doctor spend (calls, wall time) and outcome. Unit-test snapshot/restore as a pure helper on tempdirs.
- [ ] **3.2:** CLI: `--deep` flag sets the env for the run (document in help text: "engage the deep-fix agent on residual errors (slower, costs more tokens)"); `--json` gains `deep_fix: {ran, start_errors, end_errors, tool_calls} | null`. Update bench/README.md schema table + aggregate.py to pass through unknown fields untouched (verify it already does — it reads specific keys only).
- [ ] **3.3:** Full gates + commit: `feat: --deep agentic fix phase with snapshot-revert safety`

### Task 4: Phase 3 gate

- [ ] **4.1:** Final holistic review (controller dispatches), all local gates.
- [ ] **4.2:** Gate run A (regression): suite WITHOUT --deep — confirm no regressions vs the Phase 2 run (variance caveat: judge suite-wide, not per-repo).
- [ ] **4.3:** Gate run B (the headline): suite WITH --deep and the strongest fix model available (RUSTYFI_FIX_MODEL per user's key — discuss model/cost with the user before launching; this is the "whatever it takes, capped" run). Success per spec: ≥80% achievable clean, median <5, prompt-cache 0.
- [ ] **4.4:** Commit RESULTS + comparison; merge (push needs user OK); consider tagging v0.2.0.

## Out of scope
Web-UI deep-fix toggle (CLI/env only this phase); parallel doctor sessions; cost-based (token) budget enforcement beyond call/wall caps.
