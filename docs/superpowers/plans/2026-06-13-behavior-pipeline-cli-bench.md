# Behavior Pipeline + CLI + Bench Integration — Implementation Plan (Plan 2 of 3)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Wire the Plan-1 behavior library into the pipeline as a gated `phase_behavior` stage, expose it through the CLI (`verify-behavior` subcommand + `--no-behavior` + `--json`), and add a bench behavior column — so a translation run auto-emits a tool-verified `behavior.yaml` + `behavior_report.json` and the calculator demo becomes compiler-AND-behavior verified.

**Architecture:** A new `phase_behavior` runs in `run()` after verification, before `build_next_steps`. It is GATED on `RunConfig.verify_behavior` (CLI sets it; the server never does → no executing untrusted uploads, honoring the spec's §4 boundary). It builds a `Side` for the source via a per-language recipe, mines a starter corpus, captures golden from the source, writes `behavior.yaml` into the crate, and — only if the target compiled (`verification_cp.exit_clean`) — `cargo build`s + verifies the target, writing `behavior_report.json`. Fail-open: any error (missing toolchain, unsupported language) skips with an honest note, never fails the run.

**Tech Stack:** Rust (engine + CLI), Python (bench aggregator). Builds on Plan 1's `behavior` module (`BehaviorSpec`, `Side`, `Case`, `capture_all`, `verify`, `BehaviorReport`, `mine_readme`, `help_case`).

**Spec:** `docs/superpowers/specs/2026-06-12-behavioral-equivalence-design.md` (§9–§11, §13).
**Recon facts (verified 2026-06-13):** `analysis_cp.source_dir` = original source (persists); `scaffold_cp.workspace_path` / `scaffold_cp.crate_name` = target crate; `VerificationCheckpoint { exit_clean: bool, final_error_count: usize }`; insert point = pipeline.rs after `store.write("verification", &cp)?;` before `build_next_steps`; `build_next_steps(analysis, scaffold, translation, verification, doctor: Option<&DeepFixSummary>) -> NextSteps { markdown, summary_lines, todo_count, translated }`; pipeline runs only `cargo check` (no `cargo build`); `emit(cb, Progress::Note { message })`; phase_order in checkpoint.rs lists analysis→scaffold→contract→translation→verification→packaging.
**Conventions:** branch `feat/behavioral-oracle` (Plan 1 landed). Per task: TDD, `cargo test`/`clippy -D warnings`/`fmt` clean, one commit, NO attribution footer. Subagent-driven: implementer → spec review → quality review on the risky integration tasks (1, 3, 5); lighter touch + final holistic review on the mechanical ones.

**Plan 3 (out of scope):** the `RunBehaviorChecks` repair doctor (key-gated).
**Deferred within this plan (note, don't silently drop):** server-side STATIC mining + skeleton `behavior.yaml` emission (the same miner without capture, run when `verify_behavior` is off) — a small follow-up once the gated full flow lands.

---

### Task 1: Per-language source recipe + `RunConfig.verify_behavior` gate

**Files:**
- Create: `crates/rustyfi-engine/src/behavior/recipe.rs`
- Modify: `crates/rustyfi-engine/src/behavior/mod.rs` (declare submodule)
- Modify: `crates/rustyfi-engine/src/pipeline.rs` (`RunConfig` + `Default`)

The recipe builds the `Side` (build+run commands) for each end. The target is always Rust; the source depends on the detected language. `{work}` is the harness scratch dir. v1 supports the languages we can actually run; unknown → `None` → phase skips.

- [ ] **Step 1: Declare submodule** in `behavior/mod.rs` submodule block:
```rust
mod recipe;
pub(crate) use recipe::{source_side, target_side};
```

- [ ] **Step 2: Write failing tests.** Create `crates/rustyfi-engine/src/behavior/recipe.rs`:
```rust
//! Per-language build/run recipes that turn a detected source language + a
//! target crate into `behavior.yaml` `Side`s. The target is always Rust; the
//! source recipe is best-effort and language-specific. Unknown languages yield
//! `None`, which makes `phase_behavior` skip with an honest note.

use super::Side;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go_source_builds_and_runs_a_binary() {
        let s = source_side("go", "calc").expect("go supported");
        assert_eq!(s.lang, "go");
        assert!(s.build.iter().any(|a| a == "build"));
        // run vector points at the built binary in {work}
        assert!(s.run.iter().any(|a| a.contains("{work}")));
    }

    #[test]
    fn unsupported_language_yields_none() {
        assert!(source_side("haskell", "x").is_none());
    }

    #[test]
    fn target_is_rust_cargo_build_then_debug_binary() {
        let t = target_side("my_crate");
        assert_eq!(t.lang, "rust");
        assert_eq!(t.build, vec!["cargo".to_string(), "build".to_string(), "--quiet".to_string()]);
        assert_eq!(t.run, vec!["target/debug/my_crate".to_string()]);
    }
}
```

- [ ] **Step 3:** Confirm RED (`cargo test -p rustyfi-engine behavior::recipe 2>&1 | head -20`).

- [ ] **Step 4: Implement.** Add above the tests:
```rust
/// Build the `Side` for the Rust target crate. Always `cargo build` + the debug
/// binary (the pipeline only ran `cargo check`, so the binary must be built).
pub(crate) fn target_side(crate_name: &str) -> Side {
    Side {
        lang: "rust".to_string(),
        dir: ".".to_string(),
        build: vec!["cargo".into(), "build".into(), "--quiet".into()],
        run: vec![format!("target/debug/{crate_name}")],
    }
}

/// Build the `Side` for the source project, keyed on the detected language.
/// `bin_name` is the basename used for the built source binary. Returns `None`
/// for languages we cannot yet build/run, so the caller skips behavior.
pub(crate) fn source_side(language: &str, bin_name: &str) -> Option<Side> {
    let side = match language {
        "go" => Side {
            lang: "go".into(),
            dir: ".".into(),
            build: vec!["go".into(), "build".into(), "-o".into(), format!("{{work}}/{bin_name}-src"), ".".into()],
            run: vec![format!("{{work}}/{bin_name}-src")],
        },
        "python" => Side {
            lang: "python".into(),
            dir: ".".into(),
            build: vec![], // interpreted — nothing to build
            run: vec!["python3".into(), "main.py".into()],
        },
        "javascript" | "typescript" => Side {
            lang: language.into(),
            dir: ".".into(),
            build: vec![],
            run: vec!["node".into(), "index.js".into()],
        },
        _ => return None,
    };
    Some(side)
}
```
(Note: `python`/`node` entrypoints `main.py`/`index.js` are best-effort defaults; the hybrid review loop lets users correct `behavior.yaml`. Go — our only behavior-verifiable bench CLIs — is the solid path.)

- [ ] **Step 5:** `cargo test -p rustyfi-engine behavior::recipe` → 3 PASS. `cargo fmt` + clippy clean.

- [ ] **Step 6: Add the gate flag.** In `crates/rustyfi-engine/src/pipeline.rs`, add to `RunConfig` (after `tier_mid_tokens`):
```rust
    /// When true, run the behavioral-equivalence phase (mine + capture golden
    /// from the source + verify the target). Requires the SOURCE toolchain and
    /// executes the source project, so callers enable it only where that is
    /// trusted (CLI/local). The server leaves it false (no executing uploads).
    pub verify_behavior: bool,
```
and in `impl Default for RunConfig`, add `verify_behavior: false,`.

- [ ] **Step 7:** `cargo build -p rustyfi-engine` clean. Commit:
```bash
git add crates/rustyfi-engine/src/behavior/recipe.rs crates/rustyfi-engine/src/behavior/mod.rs crates/rustyfi-engine/src/pipeline.rs
git commit -m "feat: source/target behavior recipes + verify_behavior gate flag"
```

---

### Task 2: `BehaviorCheckpoint` + `BehaviorSummary` + phase ordering

**Files:**
- Modify: `crates/rustyfi-engine/src/checkpoint.rs` (new checkpoint type + phase_order)
- Modify: `crates/rustyfi-engine/src/pipeline.rs` (`BehaviorSummary` + `RunResult.behavior`)

- [ ] **Step 1: Write failing tests** in `checkpoint.rs` `#[cfg(test)] mod tests` (or add one if patterns exist; mirror `ContractCheckpoint`/`VerificationCheckpoint` tests):
```rust
    #[test]
    fn behavior_in_phase_order_between_verification_and_packaging() {
        let order = phase_order();
        let v = order.iter().position(|&p| p == "verification").unwrap();
        let b = order.iter().position(|&p| p == "behavior").unwrap();
        let p = order.iter().position(|&p| p == "packaging").unwrap();
        assert!(v < b && b < p);
    }

    #[test]
    fn behavior_checkpoint_round_trips() {
        let cp = BehaviorCheckpoint {
            ran: true,
            verified: true,
            mined: 3,
            matched: 2,
            total: 3,
            quarantined: 1,
            skipped_reason: None,
        };
        let json = serde_json::to_string(&cp).unwrap();
        let back: BehaviorCheckpoint = serde_json::from_str(&json).unwrap();
        assert_eq!(cp, back);
    }
```

- [ ] **Step 2:** Confirm RED.

- [ ] **Step 3: Implement.** In `checkpoint.rs`, add `"behavior"` to `phase_order()` between `"verification"` and `"packaging"`:
```rust
        "verification",
        "behavior",
        "packaging",
```
and add the checkpoint type next to the other per-phase types:
```rust
/// Serializable output of the Behavior phase. `ran=false` means the phase was
/// gated off or skipped (see `skipped_reason`); `verified=false` means golden
/// was captured but the target was not run (it did not compile).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BehaviorCheckpoint {
    pub ran: bool,
    pub verified: bool,
    pub mined: usize,
    pub matched: usize,
    pub total: usize,
    pub quarantined: usize,
    pub skipped_reason: Option<String>,
}
```

- [ ] **Step 4:** `cargo test -p rustyfi-engine checkpoint` → PASS. fmt + clippy clean.

- [ ] **Step 5: Add `BehaviorSummary` + `RunResult.behavior`.** In `pipeline.rs`, after `DeepFixSummary`:
```rust
/// Summary of the behavioral-equivalence phase (populated when `verify_behavior`).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct BehaviorSummary {
    /// The phase ran (mined + captured). False when gated off / skipped.
    pub ran: bool,
    /// The target was built and run against golden (false if it did not compile).
    pub verified: bool,
    pub matched: usize,
    pub total: usize,
    pub quarantined: usize,
}
```
and add to `RunResult` (after `deep_fix`):
```rust
    /// Present when the behavioral phase ran (`verify_behavior`).
    pub behavior: Option<BehaviorSummary>,
```
This will break `RunResult` construction sites — fix them in Task 3 (where the value is produced) and any test stubs by adding `behavior: None`.

- [ ] **Step 6:** `cargo build -p rustyfi-engine` (expect the RunResult construction error; note it for Task 3). Commit:
```bash
git add crates/rustyfi-engine/src/checkpoint.rs crates/rustyfi-engine/src/pipeline.rs
git commit -m "feat: BehaviorCheckpoint + BehaviorSummary + phase ordering"
```

---

### Task 3: `phase_behavior` orchestration + wire into `run()`

**Files:**
- Modify: `crates/rustyfi-engine/src/behavior/mod.rs` (orchestration helper `generate_and_verify`)
- Modify: `crates/rustyfi-engine/src/pipeline.rs` (`phase_behavior` wrapper + `run()` wiring + RunResult construction)

The library gets a testable orchestration entry; `pipeline.rs` adapts checkpoints to it and handles Progress/checkpoint/summary.

- [ ] **Step 1: Library orchestration — failing test** in `behavior/mod.rs` tests:
```rust
    #[test]
    fn generate_and_verify_end_to_end_with_sh_recipe() {
        use std::path::Path;
        // Use a tempdir as both "source" and "target" with sh recipes injected
        // directly (bypassing language recipes) to exercise the orchestration:
        // mine nothing, capture from source, verify target.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("README.md"), "```\n$ tool ping\n```\n").unwrap();
        let source = Side { lang: "sh".into(), dir: ".".into(), build: vec![],
            run: vec!["sh".into(), "-c".into(), "printf pong".into(), "sh".into()] };
        let target = Side { lang: "sh".into(), dir: ".".into(), build: vec![],
            run: vec!["sh".into(), "-c".into(), "printf pong".into(), "sh".into()] };
        let out = generate_and_verify(
            dir.path(), dir.path(), "tool", source, target, true, Path::new("/tmp"),
        );
        assert!(out.ran);
        assert!(out.verified);
        // one mined case ("tool ping") + the help case
        assert!(out.report.is_some());
        let r = out.report.unwrap();
        assert!(r.total >= 1);
    }
```

- [ ] **Step 2:** Confirm RED.

- [ ] **Step 3: Implement the orchestration helper** in `behavior/mod.rs`:
```rust
/// Outcome of the behavioral phase orchestration.
pub struct BehaviorOutcome {
    pub ran: bool,
    pub verified: bool,
    pub mined: usize,
    pub report: Option<BehaviorReport>,
    pub spec_yaml: Option<String>,
    pub skipped_reason: Option<String>,
}

/// Mine a corpus from the source README, capture golden from the source, and
/// (if `verify_target`) build + verify the target. Writes nothing to disk — the
/// caller persists `spec_yaml` / the report. Fail-open: a capture/build error
/// is returned as `skipped_reason`, never panics.
#[allow(clippy::too_many_arguments)]
pub fn generate_and_verify(
    source_dir: &Path,
    workspace: &Path,
    bin_name: &str,
    source: Side,
    target: Side,
    verify_target: bool,
    work: &Path,
) -> BehaviorOutcome {
    // 1. Mine the corpus (README invocations + a --help case).
    let readme = ["README.md", "readme.md", "README"]
        .iter()
        .find_map(|f| std::fs::read_to_string(source_dir.join(f)).ok())
        .unwrap_or_default();
    let mut cases = mine_readme(&readme, bin_name);
    cases.push(help_case());
    let mined = cases.len();

    let mut spec = BehaviorSpec {
        name: bin_name.to_string(),
        source,
        target,
        compare: CompareSpec::default(),
        normalize: vec![],
        cases,
    };

    // 2. Capture golden from the source (fail-open).
    if let Err(e) = capture_all(&mut spec, source_dir, work) {
        return BehaviorOutcome {
            ran: false, verified: false, mined,
            report: None, spec_yaml: None,
            skipped_reason: Some(format!("could not run source: {e}")),
        };
    }
    let spec_yaml = serde_yaml::to_string(&spec).ok();

    // 3. Verify the target only if requested (target compiles).
    if !verify_target {
        return BehaviorOutcome {
            ran: true, verified: false, mined,
            report: None, spec_yaml,
            skipped_reason: Some("target did not compile — behavior unverified".into()),
        };
    }
    match verify(&spec, workspace, work) {
        Ok(report) => BehaviorOutcome {
            ran: true, verified: true, mined,
            report: Some(report), spec_yaml, skipped_reason: None,
        },
        Err(e) => BehaviorOutcome {
            ran: true, verified: false, mined,
            report: None, spec_yaml,
            skipped_reason: Some(format!("target build/run failed: {e}")),
        },
    }
}
```
Ensure `mine_readme`, `help_case`, `BehaviorSpec`, `CompareSpec`, `capture_all`, `verify`, `BehaviorReport`, `Side` are in scope (they are, in `mod.rs`). Add `pub use` for `BehaviorOutcome`/`generate_and_verify` is unnecessary (already in `mod.rs` root).

- [ ] **Step 4:** `cargo test -p rustyfi-engine behavior::tests::generate_and_verify` → PASS. fmt + clippy clean.

- [ ] **Step 5: `phase_behavior` wrapper** in `pipeline.rs`. Add the function (near `phase_verify`):
```rust
fn phase_behavior<F: FnMut(Progress)>(
    config: &RunConfig,
    analysis: &AnalysisCheckpoint,
    scaffold: &ScaffoldCheckpoint,
    verification: &VerificationCheckpoint,
    progress_cb: &mut F,
) -> crate::checkpoint::BehaviorCheckpoint {
    use crate::behavior::{generate_and_verify, source_side, target_side};
    use crate::checkpoint::BehaviorCheckpoint;

    let skip = |reason: &str| BehaviorCheckpoint {
        ran: false, verified: false, mined: 0, matched: 0, total: 0,
        quarantined: 0, skipped_reason: Some(reason.to_string()),
    };

    if !config.verify_behavior {
        return skip("behavior verification not enabled");
    }
    let source = match source_side(&analysis.language, &scaffold.crate_name) {
        Some(s) => s,
        None => {
            emit(progress_cb, Progress::Note {
                message: format!("Behavior: source language `{}` not yet supported — skipping.", analysis.language),
            });
            return skip(&format!("unsupported source language: {}", analysis.language));
        }
    };
    let target = target_side(&scaffold.crate_name);

    emit(progress_cb, Progress::Note {
        message: "Behavior: mining cases + capturing golden output from the source…".into(),
    });

    // A per-run scratch dir for built source/target binaries.
    let work = scaffold.workspace_path.join(".behavior-work");
    let _ = std::fs::create_dir_all(&work);

    let out = generate_and_verify(
        &analysis.source_dir,
        &scaffold.workspace_path,
        &scaffold.crate_name,
        source,
        target,
        verification.exit_clean,
        &work,
    );

    // Persist behavior.yaml + behavior_report.json into the crate.
    if let Some(yaml) = &out.spec_yaml {
        let _ = std::fs::write(scaffold.workspace_path.join("behavior.yaml"), yaml);
    }
    if let Some(report) = &out.report {
        if let Ok(json) = serde_json::to_string_pretty(report) {
            let _ = std::fs::write(scaffold.workspace_path.join("behavior_report.json"), json);
        }
        emit(progress_cb, Progress::Note {
            message: format!(
                "Behavior: {}/{} cases matched the original ({} quarantined as nondeterministic).",
                report.matched, report.total, report.quarantined
            ),
        });
    } else if let Some(reason) = &out.skipped_reason {
        emit(progress_cb, Progress::Note { message: format!("Behavior: {reason}") });
    }

    let (matched, total, quarantined) = out
        .report
        .map(|r| (r.matched, r.total, r.quarantined))
        .unwrap_or((0, 0, 0));
    BehaviorCheckpoint {
        ran: out.ran,
        verified: out.verified,
        mined: out.mined,
        matched, total, quarantined,
        skipped_reason: out.skipped_reason,
    }
}
```

- [ ] **Step 6: Wire into `run()`.** Right after `store.write("verification", &cp)?;` (the verification branch) and before the `// ── Completion report ──` block, add a behavior phase (checkpoint-resumable like the others):
```rust
    // ── Phase 5: Behavioral equivalence (gated on verify_behavior) ────────
    let behavior_cp = if let Some(cp) = store.read::<crate::checkpoint::BehaviorCheckpoint>("behavior") {
        cp
    } else {
        let cp = phase_behavior(&config, &analysis_cp, &scaffold_cp, &verification_cp, &mut progress_cb);
        store.write("behavior", &cp)?;
        cp
    };
```
Then build the `BehaviorSummary` and pass it into `RunResult`:
```rust
    let behavior = if behavior_cp.ran {
        Some(BehaviorSummary {
            ran: behavior_cp.ran,
            verified: behavior_cp.verified,
            matched: behavior_cp.matched,
            total: behavior_cp.total,
            quarantined: behavior_cp.quarantined,
        })
    } else {
        None
    };
```
Add `behavior,` to the `RunResult { … }` construction. Also pass `Some(&behavior_cp)` into `build_next_steps` (Task 4 adds the param).

- [ ] **Step 7:** Fix any other `RunResult { … }` construction sites / test stubs by adding `behavior: None`. `cargo test -p rustyfi-engine` green; fmt + clippy clean. Commit:
```bash
git add crates/rustyfi-engine/src/behavior/mod.rs crates/rustyfi-engine/src/pipeline.rs
git commit -m "feat: phase_behavior — gated mine/capture/verify in the pipeline"
```

---

### Task 4: NEXT_STEPS behavior section

**Files:** Modify `crates/rustyfi-engine/src/pipeline.rs` (`build_next_steps`).

- [ ] **Step 1:** Add a `behavior: Option<&crate::checkpoint::BehaviorCheckpoint>` parameter to `build_next_steps` (last param) and update its call site in `run()` to pass `Some(&behavior_cp)`.

- [ ] **Step 2: Write failing test** (mirror the existing `build_next_steps` / `NextSteps` tests if present; otherwise a focused unit test):
```rust
    #[test]
    fn next_steps_includes_behavior_section_when_verified() {
        let cp = crate::checkpoint::BehaviorCheckpoint {
            ran: true, verified: true, mined: 4, matched: 3, total: 4, quarantined: 1,
            skipped_reason: None,
        };
        let md = behavior_section(Some(&cp));
        assert!(md.contains("Behavior"));
        assert!(md.contains("3/4"));
        assert!(md.contains("behavior.yaml"));
    }

    #[test]
    fn next_steps_behavior_section_empty_when_absent() {
        assert!(behavior_section(None).is_empty());
    }
```

- [ ] **Step 3: Implement** a small helper `behavior_section(cp: Option<&BehaviorCheckpoint>) -> String` and call it inside `build_next_steps`, appending its output before the markdown footer:
```rust
fn behavior_section(cp: Option<&crate::checkpoint::BehaviorCheckpoint>) -> String {
    let Some(cp) = cp else { return String::new() };
    if !cp.ran {
        return String::new();
    }
    let mut s = String::from("\n## Behavior\n\n");
    if cp.verified {
        s.push_str(&format!(
            "Verified against the original: **{}/{} cases matched** \
             ({} quarantined as nondeterministic). See `behavior_report.json`.\n",
            cp.matched, cp.total, cp.quarantined
        ));
        if cp.matched < cp.total {
            s.push_str("\nReview the mismatches in `behavior_report.json`; \
                        run `rustyfi verify-behavior .` after fixes.\n");
        }
    } else {
        let reason = cp.skipped_reason.as_deref().unwrap_or("not verified");
        s.push_str(&format!("Behavioral spec mined to `behavior.yaml` but not verified ({reason}).\n"));
    }
    s.push_str("\nExtend `behavior.yaml` with more cases and re-run `rustyfi verify-behavior .`.\n");
    s
}
```
Append `md.push_str(&behavior_section(behavior));` in `build_next_steps` just before the trailing `---` footer.

- [ ] **Step 4:** `cargo test -p rustyfi-engine` green; fmt + clippy clean. Commit:
```bash
git add crates/rustyfi-engine/src/pipeline.rs
git commit -m "feat: NEXT_STEPS behavior section"
```

---

### Task 5: CLI — `verify-behavior` subcommand, `--no-behavior`, `--json` block

**Files:** Modify `crates/rustyfi-cli/src/main.rs`.

- [ ] **Step 1: Restructure to an optional subcommand** (recon-recommended pattern). Change the `Cli` struct: add `subcommand_negates_reqs = true` to `#[command(...)]`; make `source: Option<PathBuf>` with `#[arg(required_unless_present = "command")]`; add `#[arg(long)] no_behavior: bool` and the subcommand field:
```rust
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(clap::Subcommand)]
enum Commands {
    /// Verify a translated crate's runtime behavior against its behavior.yaml.
    VerifyBehavior {
        /// Path to the translated Rust crate directory (containing behavior.yaml).
        crate_dir: PathBuf,
    },
}
```

- [ ] **Step 2: Branch in `real_main`.** At the top of `real_main`, before the translate logic:
```rust
    if let Some(Commands::VerifyBehavior { crate_dir }) = &cli.command {
        return verify_behavior_cmd(crate_dir, cli.json);
    }
    let source = cli.source.clone().expect("clap guarantees source unless a subcommand is present");
```
Use `source` thereafter (replacing `cli.source`). Implement the standalone command:
```rust
fn verify_behavior_cmd(crate_dir: &std::path::Path, json: bool) -> Result<ExitCode, String> {
    use rustyfi_engine::behavior::{verify, BehaviorSpec};
    let spec_path = crate_dir.join("behavior.yaml");
    let yaml = std::fs::read_to_string(&spec_path)
        .map_err(|e| format!("no behavior.yaml in {}: {e}", crate_dir.display()))?;
    let spec: BehaviorSpec = serde_yaml::from_str(&yaml).map_err(|e| format!("invalid behavior.yaml: {e}"))?;
    let work = crate_dir.join(".behavior-work");
    let _ = std::fs::create_dir_all(&work);
    let report = verify(&spec, crate_dir, &work).map_err(|e| format!("verify failed: {e}"))?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?);
    } else {
        eprintln!("\nbehavior: {} ({}/{} matched, {} quarantined)\n", report.name, report.matched, report.total, report.quarantined);
        for c in &report.cases {
            eprintln!("  {} {}", if c.matched { "✓" } else { "✗" }, c.name);
            for d in &c.diffs { eprintln!("      {d}"); }
        }
    }
    Ok(if report.passed { ExitCode::SUCCESS } else { ExitCode::from(1) })
}
```
Add `serde_yaml` to the CLI's `Cargo.toml` deps (`serde_yaml = "0.9"`).

- [ ] **Step 3: Wire the gate flag + json block.** In the translate path, set `verify_behavior: !cli.no_behavior` in the `RunConfig { … }`. Add a `behavior` block to `build_json_summary`: a `behavior_to_json(&BehaviorSummary) -> serde_json::Value` mirroring `deep_fix_to_json`, and `"behavior": r.behavior.as_ref().map(behavior_to_json).unwrap_or(serde_json::Value::Null)` in the json object:
```rust
fn behavior_to_json(b: &BehaviorSummary) -> serde_json::Value {
    serde_json::json!({
        "ran": b.ran, "verified": b.verified,
        "matched": b.matched, "total": b.total, "quarantined": b.quarantined,
    })
}
```
Import `BehaviorSummary` from `rustyfi_engine::pipeline` alongside the existing `DeepFixSummary` import. Update the existing `json_summary_*` tests' `RunResult` stubs with `behavior: None`.

- [ ] **Step 4: Failing test then green** — add a CLI unit test that `build_json_summary` includes a null `behavior` when absent and an object when present (mirror the existing `json_summary_deep_fix_*` tests). Confirm RED for the new assertions, then GREEN.

- [ ] **Step 5:** `cargo test -p rustyfi-cli` green; `cargo build` clean; fmt + clippy `-D warnings` clean across the workspace. Manually verify help: `cargo run -p rustyfi-cli -- --help` shows the subcommand, and `cargo run -p rustyfi-cli -- verify-behavior --help` works. Commit:
```bash
git add crates/rustyfi-cli/
git commit -m "feat: rustyfi verify-behavior subcommand + --no-behavior + --json behavior block"
```

---

### Task 6: Bench behavior column

**Files:** Modify `bench/aggregate.py`, `bench/repos.toml`. (No `run.sh` change — `behavior` flows through `--json`.)

- [ ] **Step 1: Mark verifiable repos.** In `bench/repos.toml`, add `behavior_verifiable = true` to the `calculator` and `prompt-cache` entries (the only behavior-verifiable CLIs; cobra/itsdangerous/axios/paint/emoji-java/ky/clifx are libraries; thc-hydra is `impossible`).

- [ ] **Step 2: Failing self-test expectation.** Add a `behavior` key to one `bench/testdata/*.json` fixture (e.g. `alpha.json`: `"behavior": {"ran": true, "verified": true, "matched": 2, "total": 2, "quarantined": 0}`) and extend the self-test assertion in `aggregate.py` to check the rendered table contains `2/2` for that row. Run `python3 bench/aggregate.py --self-test` → expect FAIL (column not rendered yet).

- [ ] **Step 3: Implement the column.** In `render()`:
  - header: append ` behavior |`
  - separator: append `---|`
  - in each branch compute `behavior`: `res is None` / `pipeline_failed` → `"—"`; normal → derive from `res.get("behavior")`: if present and `verified` → `f"{b['matched']}/{b['total']}"`, present but not verified → `"unverified"`, absent → `"n/a"` when `m.get("behavior_verifiable")` else `"—"`.
  - row f-string: append ` {behavior} |`

- [ ] **Step 4:** `python3 bench/aggregate.py --self-test` → PASS. Commit:
```bash
git add bench/aggregate.py bench/repos.toml bench/testdata/
git commit -m "feat: bench behavior-match column"
```

---

### Task 7: Calculator real e2e + final review

**Files:** Add an `#[ignore]` integration test; final holistic review.

- [ ] **Step 1: Real e2e test** (`#[ignore]`, requires `go` + `cargo`) in `crates/rustyfi-engine/src/behavior/mod.rs` tests, exercising `generate_and_verify` against the real example: source = `examples/calculator` (Go recipe), target = `bench/.work/out/calculator` (the committed generated crate). Assert it builds the Go source, captures golden, builds + runs the Rust target, and produces a `BehaviorReport` whose `total >= 1`. (Document that this codifies the 0/6→… finding; exact matched count depends on the generated crate's state.)
```rust
    #[test]
    #[ignore = "requires go + cargo toolchains; real e2e"]
    fn calculator_real_behavior_e2e() {
        use std::path::Path;
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."); // workspace root
        let source = source_side("go", "calculator").unwrap();
        let target = target_side("calculator");
        let work = repo.join("bench/.work/.behavior-e2e");
        let _ = std::fs::create_dir_all(&work);
        let out = generate_and_verify(
            &repo.join("examples/calculator"),
            &repo.join("bench/.work/out/calculator"),
            "calculator",
            source,
            target,
            true,
            &work,
        );
        assert!(out.ran, "should mine + capture from the Go source");
        // verified depends on the committed crate building; assert report shape if verified
        if let Some(r) = out.report {
            assert!(r.total >= 1);
        }
    }
```
(If `target_side`/`source_side` are `pub(crate)`, this in-module test can call them directly.)

- [ ] **Step 2:** Run it explicitly to confirm it works end-to-end: `cargo test -p rustyfi-engine calculator_real_behavior_e2e -- --ignored --nocapture`. Capture the output for the report.

- [ ] **Step 3: Full gates:** `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --check`. Commit:
```bash
git add crates/rustyfi-engine/src/behavior/mod.rs
git commit -m "test: calculator real behavioral e2e (ignored)"
```

- [ ] **Step 4: Final holistic review** of the whole Plan-2 surface (engine phase + CLI + bench), then report.

---

## Self-review notes
- **Spec coverage (§9–§11, §13):** gate flag + recipes (T1), checkpoint + phase order + summary (T2), `phase_behavior` mine/capture/compile-gate/verify + behavior.yaml + behavior_report.json emission (T3), NEXT_STEPS (T4), `verify-behavior` + `--no-behavior` + `--json` (T5), bench column (T6), real e2e (T7). **Deferred & noted:** server static-mining skeleton emission (when `verify_behavior` is off).
- **Type consistency:** `verification_cp.exit_clean` (NOT `cargo_clean`); `BehaviorCheckpoint`/`BehaviorSummary` fields align across T2/T3/T4/T5; `build_next_steps` gains exactly one param threaded from `run()`.
- **Risky tasks (full two-stage review): T1 (recipes — the source-execution surface), T3 (pipeline wiring + RunResult break), T5 (clap restructure).** T2/T4/T6/T7 get lighter touch + the final holistic review.
- **No placeholders:** every step has concrete code or an exact command. The T7 e2e `target_side` double-assignment is written defensively in case visibility differs; the implementer should simplify to a single `let target = super::target_side("calculator");`.
