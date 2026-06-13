# Behavioral Repair Doctor — Implementation Plan (Plan 3 of 3)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Teach the existing agentic deep-fix "doctor" to converge BEHAVIORAL mismatches, not just compile errors — add a `RunBehaviorChecks` tool to the same ReAct loop (approach A), make `run_doctor` behavior-aware, and add a gated `behavior_repair` step that keeps the doctor's edits only if behavior improves and the crate still compiles (snapshot-revert otherwise).

**Architecture:** One engine, two tools. `DoctorSession` optionally carries a `BehaviorSpec` + work dir; a new `ToolCall::RunBehaviorChecks` runs `behavior::verify` and returns per-case diffs. `run_doctor` gains an optional behavior context — when present it advertises the tool, extends the system prompt, and seeds the conversation with the mismatch corpus. The pipeline's `phase_behavior` invokes `behavior_repair` when the target compiled, behavior was verified, cases still mismatch, the deep-fix gate is on, and a fix model is configured. Acceptance spans two dimensions: keep iff the crate still compiles AND behavioral mismatches strictly decreased; else `restore_src`. Live convergence needs a Claude-class fix model (same blocker as the phase-3 headline gate); the loop is fully provable now with a `ScriptedTransport`.

**Tech Stack:** Rust. Builds on Plan 1 (`behavior::{verify, BehaviorSpec, BehaviorReport}`) and the existing `agent_fix.rs` doctor + `pipeline.rs` `snapshot_src`/`restore_src`.

**Spec:** `docs/superpowers/specs/2026-06-12-behavioral-equivalence-design.md` (§12).
**Recon facts (verified 2026-06-13):**
- `agent_fix.rs`: `DoctorSession { workspace, budget, calls_used, started, item_index, last_error_count }` + `new(ws, budget)`. `ToolCall` (7 variants) + `execute()` match (~L121). `tools_schema()` → JSON array (~L667). `parse_action_reply` JSON-fallback match (~L488) AND `tool_call_from_native` (~L783) AND a `ToolCall→name` match in `run_doctor` (~L982). `SYSTEM_DOCTOR` const (~L759). `run_doctor(ws, transport, budget, progress_cb) -> DoctorReport` (~L836), seeds with `CargoCheck` + `NEXT_STEPS.md`. `ScriptedTransport(VecDeque<AssistantTurn>)` in `#[cfg(test)]` (~L1522); existing `#[ignore]` scripted test drives read→write→done. `snapshot_src`/`restore_src` are `pub` (~L1861); keep-iff-improved + `DeepFixSummary{ran,start_errors,end_errors,tool_calls}` in `phase_verify` tail (~L1704).
- `pipeline.rs`: `phase_behavior` (Plan 2) runs after verification, returns `BehaviorCheckpoint`; `BehaviorSummary` + `RunResult.behavior`. The deep-fix gate is `std::env::var("RUSTYFI_DEEP_FIX").is_ok()`. The fix client is `LlmClient::for_fixing()` (already built in `run()` as `fix_llm`).
**Conventions:** branch `feat/behavioral-oracle`. TDD; per task `cargo test`/`clippy -D warnings`/`fmt` clean; one commit, NO attribution footer. Full two-stage review on Tasks 1 and 3 (novel logic + acceptance); lighter touch + final holistic review on 2/4/5.

---

### Task 1: `RunBehaviorChecks` tool + behavior-aware `DoctorSession`

**Files:** Modify `crates/rustyfi-engine/src/agent_fix.rs`.

- [ ] **Step 1: Add the session fields + builder + the enum variant.** In `DoctorSession`, add before `last_error_count`:
```rust
    /// Optional behavioral corpus + scratch dir; enables the RunBehaviorChecks tool.
    behavior: Option<(crate::behavior::BehaviorSpec, std::path::PathBuf)>,
```
In `new()`, initialize `behavior: None,`. Add a builder:
```rust
    /// Attach a behavioral corpus so the session can run `RunBehaviorChecks`.
    pub fn with_behavior(mut self, spec: crate::behavior::BehaviorSpec, work: std::path::PathBuf) -> Self {
        self.behavior = Some((spec, work));
        self
    }
```
Add to `ToolCall`:
```rust
    /// Build + run the target against the behavioral corpus; report per-case diffs.
    RunBehaviorChecks,
```
Add the `execute()` arm (next to `CargoCheck`):
```rust
            ToolCall::RunBehaviorChecks => self.run_behavior_checks(),
```

- [ ] **Step 2: Write the failing test** (in the `#[cfg(test)] mod tests`), using sh recipes + a tiny target so it needs no real toolchain beyond `sh`:
```rust
    #[test]
    fn run_behavior_checks_reports_mismatch() {
        use crate::behavior::{BehaviorSpec, Side, CompareSpec, Case, Provenance, Expect};
        let tmp = tempfile::tempdir().unwrap();
        // target prints "WRONG"; golden expects "OK" → one mismatch.
        let spec = BehaviorSpec {
            name: "t".into(),
            source: Side { lang: "sh".into(), dir: ".".into(), build: vec![], run: vec![] },
            target: Side { lang: "sh".into(), dir: ".".into(), build: vec![],
                run: vec!["sh".into(), "-c".into(), "printf WRONG".into(), "sh".into()] },
            compare: CompareSpec::default(),
            normalize: vec![],
            cases: vec![Case {
                name: "c".into(), provenance: Provenance::Manual, args: vec![],
                stdin: None, env: Default::default(),
                expect: Some(Expect { stdout: "OK".into(), stderr: String::new(), exit_code: 0 }),
                nondeterministic: false, compare: None,
            }],
        };
        let work = tempfile::tempdir().unwrap();
        let mut session = DoctorSession::new(tmp.path(), DoctorBudget::default())
            .with_behavior(spec, work.path().to_path_buf());
        let out = session.execute(ToolCall::RunBehaviorChecks);
        assert!(!out.is_terminal);
        assert!(out.payload.contains("c"));
        assert!(out.payload.to_lowercase().contains("mismatch") || out.payload.contains("0/1"));
    }

    #[test]
    fn run_behavior_checks_without_corpus_is_a_noop_error() {
        let tmp = tempfile::tempdir().unwrap();
        let mut session = DoctorSession::new(tmp.path(), DoctorBudget::default());
        let out = session.execute(ToolCall::RunBehaviorChecks);
        assert!(!out.is_terminal);
        assert!(out.payload.contains("no behavioral corpus"));
    }
```

- [ ] **Step 3: Confirm RED**, then implement `run_behavior_checks`:
```rust
    /// Build + run the target against the corpus and render per-case diffs.
    fn run_behavior_checks(&mut self) -> ToolOutcome {
        let Some((spec, work)) = &self.behavior else {
            return ToolOutcome {
                payload: "no behavioral corpus loaded for this session".to_string(),
                is_terminal: false,
            };
        };
        match crate::behavior::verify(spec, &self.workspace, work) {
            Ok(report) => {
                let mut p = format!(
                    "behavior: {}/{} cases matched ({} quarantined)\n",
                    report.matched, report.total, report.quarantined
                );
                for c in report.cases.iter().filter(|c| !c.matched) {
                    p.push_str(&format!("MISMATCH {}:\n", c.name));
                    for d in &c.diffs {
                        p.push_str(&format!("  {d}\n"));
                    }
                }
                if p.len() > 8_000 {
                    p.truncate(8_000);
                    p.push_str("\n…(truncated)");
                }
                ToolOutcome { payload: p, is_terminal: false }
            }
            Err(e) => ToolOutcome {
                payload: format!("behavior check failed to run: {e}"),
                is_terminal: false,
            },
        }
    }
```

- [ ] **Step 4:** `cargo test -p rustyfi-engine agent_fix` → green (both new tests). fmt + clippy clean.

- [ ] **Step 5: Wire the tool name into the three name-mapping sites** (so native + JSON-fallback transports can invoke it). (a) `parse_action_reply` (~L488): add `"run_behavior_checks" => Ok(ToolCall::RunBehaviorChecks),` before the `other =>` arm. (b) `tool_call_from_native` (~L783): add the same case. (c) the `ToolCall → &str` match in `run_doctor` (~L982): add `ToolCall::RunBehaviorChecks => "run_behavior_checks",`. Add a parser unit test:
```rust
    #[test]
    fn parses_run_behavior_checks_action() {
        let t = parse_action_reply(r#"{"tool":"run_behavior_checks","args":{}}"#).unwrap();
        assert!(matches!(t, ToolCall::RunBehaviorChecks));
    }
```

- [ ] **Step 6:** `cargo test -p rustyfi-engine agent_fix` green; fmt + clippy clean. Commit:
```bash
git add crates/rustyfi-engine/src/agent_fix.rs
git commit -m "feat: RunBehaviorChecks doctor tool + behavior-aware session"
```

---

### Task 2: Behavior-aware `run_doctor` + `tools_schema` + `SYSTEM_DOCTOR`

**Files:** Modify `crates/rustyfi-engine/src/agent_fix.rs`; update the existing `run_doctor` call in `pipeline.rs` (`phase_verify`) to pass `None`.

- [ ] **Step 1: Parameterize `tools_schema`.** Change `pub fn tools_schema() -> serde_json::Value` to `pub fn tools_schema(include_behavior: bool) -> serde_json::Value`; build the array, and when `include_behavior` push the extra tool def:
```rust
    let mut tools = serde_json::json!([ /* the existing 7 entries */ ]);
    if include_behavior {
        if let Some(arr) = tools.as_array_mut() {
            arr.push(serde_json::json!({
                "type": "function",
                "function": {
                    "name": "run_behavior_checks",
                    "description": "Build and run the translated crate against the behavioral corpus; \
                                    returns per-case stdout/stderr/exit diffs vs the original. Use after \
                                    cargo check is clean to find and fix behavioral divergences.",
                    "parameters": { "type": "object", "properties": {}, "required": [] }
                }
            }));
        }
    }
    tools
```
(Refactor the existing `json!([...])` so the 7 entries are built first, then conditionally appended — keep the 7 entries verbatim.)

- [ ] **Step 2: Extend `SYSTEM_DOCTOR`** — add a rule and the tool name:
```
"7. After cargo check is clean, if behavioral mismatches remain, call run_behavior_checks to see which \
    cases diverge from the original, fix the root cause (output formatting, stream routing, off-by-one, \
    logic), and call run_behavior_checks again to confirm. NEVER edit behavior.yaml or change the expected \
    values — the original's behavior is ground truth.\n",
```
and add `run_behavior_checks` to the valid-tool-names list in rule 6.

- [ ] **Step 3: Behavior-aware `run_doctor`.** Add a parameter:
```rust
pub fn run_doctor(
    ws: &Path,
    transport: &mut dyn DoctorTransport,
    budget: DoctorBudget,
    behavior: Option<(crate::behavior::BehaviorSpec, std::path::PathBuf)>,
    progress_cb: &mut dyn FnMut(String),
) -> DoctorReport
```
Inside: `let tools = tools_schema(behavior.is_some());` Construct the session with the corpus when present:
```rust
    let mut session = DoctorSession::new(ws, budget);
    if let Some((spec, work)) = behavior {
        session = session.with_behavior(spec, work);
    }
```
After the cargo-check seed, if the session has a corpus, also seed a behavior snapshot so the model starts informed:
```rust
    let behavior_seed = if session.has_behavior() {
        let outcome = session.execute(ToolCall::RunBehaviorChecks);
        format!("\n\nBehavioral check (the original is ground truth):\n```\n{}\n```", outcome.payload)
    } else {
        String::new()
    };
```
Append `behavior_seed` to `seed_user_msg`. Add a `pub fn has_behavior(&self) -> bool { self.behavior.is_some() }` accessor on `DoctorSession`.

- [ ] **Step 4: Fix the existing caller.** In `pipeline.rs` `phase_verify`, the `run_doctor(ws, &mut transport, budget, &mut cb)` call gains `None`: `run_doctor(ws, &mut transport, budget, None, &mut cb)`.

- [ ] **Step 5: Scripted behavioral-repair e2e** (`#[ignore]`, needs `sh`). Add to the `#[cfg(test)]` tests, mirroring the existing scripted doctor test but driving the behavior tool:
```rust
    #[test]
    #[ignore = "scripted behavioral repair e2e (needs sh + cargo); drives RunBehaviorChecks"]
    fn run_doctor_scripted_behavior_repair() {
        // A tiny cargo crate whose main prints "WRONG"; golden expects "OK".
        // Scripted turns: cargo_check (clean) → run_behavior_checks (mismatch) →
        // write_file (fix main to print OK) → run_behavior_checks (match) → done.
        // Assert the final RunBehaviorChecks payload shows 1/1 matched.
        // (Construct the crate + BehaviorSpec with a cargo target Side; see
        // the existing run_doctor_scripted_fixes_compile_error test for the
        // crate/tempdir + ScriptedTransport setup pattern.)
    }
```
Implement it concretely following the existing scripted test's structure (tempdir crate, `ScriptedTransport::from(vec![...])`, `run_doctor(ws, &mut t, budget, Some((spec, work)), &mut |m| log.push(m))`), asserting the loop reaches a matched state. Run it: `cargo test -p rustyfi-engine run_doctor_scripted_behavior_repair -- --ignored --nocapture`.

- [ ] **Step 6:** `cargo test -p rustyfi-engine` green (ignored excluded); fmt + clippy clean; `cargo build -p rustyfi-engine`. Commit:
```bash
git add crates/rustyfi-engine/src/agent_fix.rs crates/rustyfi-engine/src/pipeline.rs
git commit -m "feat: behavior-aware run_doctor (tool schema + system prompt + seed)"
```

---

### Task 3: `behavior_repair` + two-dimension acceptance, gated in `phase_behavior`

**Files:** Modify `crates/rustyfi-engine/src/pipeline.rs` (+ `BehaviorCheckpoint`/`BehaviorSummary` repair fields in `checkpoint.rs`/`pipeline.rs`).

- [ ] **Step 1: Add repair fields.** In `checkpoint.rs` `BehaviorCheckpoint`, add:
```rust
    /// Behavioral deep-fix: mismatches before/after + tool calls (None = repair not run).
    #[serde(default)]
    pub repair: Option<BehaviorRepair>,
```
and a type (in checkpoint.rs):
```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BehaviorRepair {
    pub start_mismatches: usize,
    pub end_mismatches: usize,
    pub tool_calls: usize,
    pub kept: bool,
}
```
Mirror an optional `repair` on `BehaviorSummary` (pipeline.rs) for `--json`. Update the round-trip test + any `BehaviorCheckpoint { … }` literals to include `repair: None`. (`#[serde(default)]` keeps old checkpoints loading.)

- [ ] **Step 2: `behavior_repair` helper** (pipeline.rs). TDD a pure-ish acceptance helper first:
```rust
/// Keep the doctor's edits iff the crate still compiles AND behavioral
/// mismatches strictly decreased.
fn behavior_repair_accept(compiles: bool, start_mismatches: usize, end_mismatches: usize) -> bool {
    compiles && end_mismatches < start_mismatches
}
```
with tests:
```rust
    #[test]
    fn behavior_repair_accepts_only_strict_improvement_that_compiles() {
        assert!(behavior_repair_accept(true, 3, 1));
        assert!(!behavior_repair_accept(true, 3, 3));   // no improvement
        assert!(!behavior_repair_accept(false, 3, 0));  // broke compile
        assert!(!behavior_repair_accept(true, 0, 0));   // nothing to do
    }
```

- [ ] **Step 3: The repair driver** (pipeline.rs). Implement:
```rust
fn behavior_repair<F: FnMut(Progress)>(
    workspace: &Path,
    spec: &crate::behavior::BehaviorSpec,
    fix_llm: &crate::llm::LlmClient,
    start_mismatches: usize,
    progress_cb: &mut F,
) -> crate::checkpoint::BehaviorRepair {
    use crate::agent_fix::{run_doctor, DoctorBudget, LlmTransport};

    let budget = DoctorBudget {
        max_tool_calls: std::env::var("RUSTYFI_DEEP_FIX_BUDGET").ok().and_then(|v| v.parse().ok()).unwrap_or(40),
        max_wall_secs: std::env::var("RUSTYFI_DEEP_FIX_TIMEOUT").ok().and_then(|v| v.parse().ok()).unwrap_or(1200),
    };
    let snap = match snapshot_src(workspace) {
        Ok(s) => s,
        Err(_) => return crate::checkpoint::BehaviorRepair { start_mismatches, end_mismatches: start_mismatches, tool_calls: 0, kept: false },
    };
    let work = match tempfile::tempdir() { Ok(d) => d, Err(_) =>
        return crate::checkpoint::BehaviorRepair { start_mismatches, end_mismatches: start_mismatches, tool_calls: 0, kept: false } };

    let mut transport = LlmTransport(fix_llm);
    let report = run_doctor(
        workspace, &mut transport, budget,
        Some((spec.clone(), work.path().to_path_buf())),
        &mut |m| emit(progress_cb, Progress::Note { message: m }),
    );

    // Re-verify: does it still compile AND did mismatches drop?
    let compiles = crate::behavior::build_target_ok(spec, workspace, work.path());
    let end_mismatches = match crate::behavior::verify(spec, workspace, work.path()) {
        Ok(r) => r.total - r.matched,
        Err(_) => start_mismatches, // couldn't run → treat as no improvement
    };
    let kept = behavior_repair_accept(compiles, start_mismatches, end_mismatches);
    if !kept {
        let _ = restore_src(workspace, &snap);
    }
    crate::checkpoint::BehaviorRepair { start_mismatches, end_mismatches: if kept { end_mismatches } else { start_mismatches }, tool_calls: report.tool_calls_used, kept }
}
```
Add a tiny `pub fn build_target_ok(spec, workspace, work) -> bool` to `behavior/mod.rs` that runs `build_side(&spec.target, "target", workspace, work).is_ok()` (so acceptance can check "still compiles" without a full verify). Unit-test it (sh recipe with empty/failing build).

- [ ] **Step 4: Gate + invoke in `phase_behavior`.** Thread `fix_llm: &LlmClient` into `phase_behavior`'s signature and the `run()` call site (pass `fix_llm.as_ref().unwrap_or(&llm)`). After the `verify` report is obtained and written, if `out.verified` and `matched < total` and `std::env::var("RUSTYFI_DEEP_FIX").is_ok()`:
  - compute `start_mismatches = total - matched`,
  - load the spec we just wrote (or reuse the in-memory one — pass it out of `generate_and_verify` via `BehaviorOutcome`; add `pub spec: Option<BehaviorSpec>` to `BehaviorOutcome` and set it),
  - call `behavior_repair(...)`,
  - re-run `verify` to refresh `behavior_report.json` + the checkpoint counts,
  - set `BehaviorCheckpoint.repair = Some(info)` and emit a Progress::Note.
  Keep it fail-open (a repair error never aborts the run).

- [ ] **Step 5:** `cargo test -p rustyfi-engine` green; fmt + clippy clean. Commit:
```bash
git add crates/rustyfi-engine/src/pipeline.rs crates/rustyfi-engine/src/checkpoint.rs crates/rustyfi-engine/src/behavior/mod.rs
git commit -m "feat: gated behavioral repair with two-dimension snapshot-revert"
```

---

### Task 4: Surface the repair in `--json` + NEXT_STEPS

**Files:** Modify `crates/rustyfi-cli/src/main.rs` (`behavior_to_json`), `crates/rustyfi-engine/src/pipeline.rs` (`behavior_section` + `BehaviorSummary` plumb).

- [ ] **Step 1:** Plumb `repair: Option<BehaviorRepair>` from `BehaviorCheckpoint` into `BehaviorSummary` at the `run()` reconstruction site.
- [ ] **Step 2:** Extend `behavior_to_json` (CLI) to include a `repair` object when present:
```rust
        "repair": b.repair.as_ref().map(|r| serde_json::json!({
            "start_mismatches": r.start_mismatches, "end_mismatches": r.end_mismatches,
            "tool_calls": r.tool_calls, "kept": r.kept,
        })).unwrap_or(serde_json::Value::Null),
```
Update the CLI json tests' `BehaviorSummary` stub with `repair: None` and add one asserting the repair block serializes.
- [ ] **Step 3:** Extend `behavior_section` (NEXT_STEPS): when `repair` is present and `kept`, add "Deep-fix improved behavior: N→M mismatches (K tool calls)."; when not kept, "Deep-fix attempted but did not improve behavior; reverted."
- [ ] **Step 4:** `cargo test --workspace` green; fmt + clippy clean. Commit:
```bash
git add crates/rustyfi-cli/src/main.rs crates/rustyfi-engine/src/pipeline.rs
git commit -m "feat: surface behavioral deep-fix in --json + NEXT_STEPS"
```

---

### Task 5: Final review + gate note

- [ ] **Step 1:** Final holistic review of the whole Plan-3 surface (the tool, behavior-aware doctor, two-dimension acceptance, gating, fail-open, json/NEXT_STEPS). Confirm the existing compile-only deep-fix path is unchanged (passes `None`), and that behavioral repair is gated on `RUSTYFI_DEEP_FIX` + `verify_behavior` + a configured fix model.
- [ ] **Step 2:** Full gates: `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --check`. Run BOTH ignored scripted e2es explicitly (`-- --ignored`) and confirm they pass.
- [ ] **Step 3:** Document the live gate as DEFERRED (needs a Claude-class fix model behind `RUSTYFI_FIX_MODEL`; same ~$10 blocker as the phase-3 headline run). Update RESULTS/README as appropriate. Commit.

## Self-review notes
- **Spec coverage (§12):** RunBehaviorChecks tool + session (T1), behavior-aware run_doctor/tools/prompt + scripted e2e (T2), two-dimension acceptance + gated repair + snapshot-revert (T3), honest reporting (T4), review + deferred live gate (T5).
- **The compile-only doctor is untouched** — `run_doctor(..., None, ...)` reproduces today's behavior; `tools_schema(false)` omits the behavior tool; the new system-prompt rule 7 is inert without the tool.
- **Type consistency:** `BehaviorRepair { start_mismatches, end_mismatches, tool_calls, kept }` flows checkpoint→summary→json/NEXT_STEPS. `behavior_repair_accept(compiles, start, end)` is the single acceptance predicate, unit-tested and reused by the driver.
- **Fail-open everywhere:** snapshot/tempdir/verify failures all degrade to `kept:false`/no-improvement, never abort `run()`.
- **No placeholders** except T2 step 5's scripted-test body, which references the concrete existing test (`run_doctor_scripted_fixes_compile_error`) to mirror — the implementer fills the crate/spec setup from that pattern.
