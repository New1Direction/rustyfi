# Behavior Harness Library — Implementation Plan (Plan 1 of 3)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the self-contained `behavior` module in `rustyfi-engine` — the Rust port of the proven Python PoC: `behavior.yaml` types, build/run/diff, golden capture with nondeterminism self-detection, and a README miner. Pure library, no pipeline/CLI/model dependencies, fully unit-testable.

**Architecture:** A new `crates/rustyfi-engine/src/behavior/` module directory (idiomatic Rust domain module). `mod.rs` holds the serde types + the public `capture_all`/`verify` orchestration; `harness.rs` holds process exec + diff; `mine.rs` holds the README miner. The source binary is ground truth: `capture_all` runs the source per case to fill golden `expect` values (running twice to quarantine nondeterministic cases); `verify` runs the target and diffs against golden. This is differential testing — sameness on tested cases, not equivalence.

**Tech Stack:** Rust, `serde` + `serde_yaml` (YAML parse), `regex` (already a dep; for `mask` normalize), `wait-timeout` (per-process timeout), `shlex` (shell-split mined commands). TDD with `#[cfg(test)]` modules + `tempfile` (already a dep).

**Spec:** `docs/superpowers/specs/2026-06-12-behavioral-equivalence-design.md` (§5–§8).
**Conventions:** branch `feat/behavioral-oracle` (PoC + spec already committed). Per task: TDD, `cargo test -p rustyfi-engine` green, `cargo clippy -p rustyfi-engine --all-targets -- -D warnings` clean, `cargo fmt` clean. One commit per task, `<type>: <description>`, **no attribution footer** (user's global rule).

**Plans 2 & 3 (out of scope here):** pipeline/checkpoint/CLI/bench integration (Plan 2) and the `RunBehaviorChecks` repair doctor (Plan 3) are written after this lands, against these real types.

---

### Task 1: Dependencies, module skeleton, and `behavior.yaml` serde types

**Files:**
- Modify: `crates/rustyfi-engine/Cargo.toml` (add `serde_yaml`, `wait-timeout`, `shlex`)
- Modify: `crates/rustyfi-engine/src/lib.rs` (add `pub mod behavior;`)
- Create: `crates/rustyfi-engine/src/behavior/mod.rs`

- [ ] **Step 1: Add dependencies**

In `crates/rustyfi-engine/Cargo.toml`, under `[dependencies]`, after the `quote = "1"` line, add:

```toml
serde_yaml   = "0.9"
wait-timeout = "0.2"
shlex        = "1"
```

(`serde_yaml` 0.9 is the de-facto standard; it is in maintenance but stable. If a future maintainer prefers the active fork, `serde_yml` is a drop-in.)

- [ ] **Step 2: Declare the module**

In `crates/rustyfi-engine/src/lib.rs`, add `pub mod behavior;` to the module list (keep the list alphabetical — insert after `pub mod analysis;`).

- [ ] **Step 3: Write the failing test for type round-tripping**

Create `crates/rustyfi-engine/src/behavior/mod.rs` with ONLY the test module first (so it fails to compile → the RED state):

```rust
//! Behavioral-equivalence harness: run a source project and its Rust
//! translation against the same inputs and diff observable behavior.
//!
//! The source binary is ground truth. This is differential testing against a
//! fixture corpus — it verifies sameness on the cases exercised, not
//! equivalence in general.

#[cfg(test)]
mod tests {
    use super::*;

    const CALCULATOR_YAML: &str = r#"
name: calculator
source: { lang: go,   dir: ., build: ["go", "build", "-o", "{work}/calc-go", "."], run: ["{work}/calc-go"] }
target: { lang: rust, dir: ., build: ["cargo", "build", "-q"], run: ["target/debug/calculator"] }
compare: { stdout: exact, stderr: exact, exit_code: exact }
normalize:
  - strip_trailing_ws
  - mask: { pattern: '\d{4}-\d{2}-\d{2}', token: '<DATE>' }
cases:
  - { name: precedence, source: readme, args: ["2 + 3 * (4 - 1)"], expect: { stdout: "11\n", stderr: "", exit_code: 0 } }
  - { name: now, source: fixture, args: ["now"], nondeterministic: true }
"#;

    #[test]
    fn parses_calculator_spec() {
        let spec: BehaviorSpec = serde_yaml::from_str(CALCULATOR_YAML).unwrap();
        assert_eq!(spec.name, "calculator");
        assert_eq!(spec.source.lang, "go");
        assert_eq!(spec.cases.len(), 2);
        assert_eq!(spec.cases[0].name, "precedence");
        assert_eq!(spec.cases[0].provenance, Provenance::Readme);
        assert_eq!(
            spec.cases[0].expect.as_ref().unwrap().stdout,
            "11\n"
        );
        assert!(!spec.cases[0].nondeterministic);
        assert!(spec.cases[1].nondeterministic);
        assert_eq!(spec.normalize.len(), 2);
        assert_eq!(spec.compare.stdout, StreamMode::Exact);
    }

    #[test]
    fn compare_defaults_to_exact_when_absent() {
        let spec: BehaviorSpec =
            serde_yaml::from_str("name: x\nsource: {lang: go, dir: ., build: [], run: []}\ntarget: {lang: rust, dir: ., build: [], run: []}\n").unwrap();
        assert_eq!(spec.compare.stdout, StreamMode::Exact);
        assert_eq!(spec.compare.exit_code, StreamMode::Exact);
        assert!(spec.cases.is_empty());
    }

    #[test]
    fn case_provenance_defaults_to_manual() {
        let c: Case = serde_yaml::from_str("name: c\nargs: [\"x\"]\n").unwrap();
        assert_eq!(c.provenance, Provenance::Manual);
        assert!(c.expect.is_none());
        assert!(c.env.is_empty());
    }
}
```

- [ ] **Step 4: Run the test to verify it fails**

Run: `cargo test -p rustyfi-engine behavior::tests 2>&1 | head -20`
Expected: FAIL to compile — `cannot find type BehaviorSpec`.

- [ ] **Step 5: Implement the types**

Prepend to `crates/rustyfi-engine/src/behavior/mod.rs` (above the test module):

```rust
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A full behavioral-equivalence spec (`behavior.yaml`). Self-contained after
/// golden capture: `cases[].expect` holds the source's captured output.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BehaviorSpec {
    pub name: String,
    pub source: Side,
    pub target: Side,
    #[serde(default)]
    pub compare: CompareSpec,
    #[serde(default)]
    pub normalize: Vec<Normalize>,
    #[serde(default)]
    pub cases: Vec<Case>,
}

/// One side (source or target): how to build it and how to invoke its binary.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Side {
    pub lang: String,
    pub dir: String,
    pub build: Vec<String>,
    pub run: Vec<String>,
}

/// Per-stream comparison policy. Defaults to exact on every stream.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct CompareSpec {
    #[serde(default)]
    pub stdout: StreamMode,
    #[serde(default)]
    pub stderr: StreamMode,
    #[serde(default)]
    pub exit_code: StreamMode,
}

impl Default for CompareSpec {
    fn default() -> Self {
        Self { stdout: StreamMode::Exact, stderr: StreamMode::Exact, exit_code: StreamMode::Exact }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
pub enum StreamMode {
    #[default]
    Exact,
    Ignore,
    Normalized,
}

/// A normalization transform applied to BOTH sides before an exact compare.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Normalize {
    /// Strip trailing whitespace from every line.
    StripTrailingWs,
    /// Replace every regex match with a fixed token.
    Mask { pattern: String, token: String },
}

/// Where a mined case came from (informational; preserved through round-trips).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Provenance {
    #[default]
    Manual,
    Readme,
    Help,
    Fixture,
}

/// One behavioral case: an invocation + (after capture) its golden output.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Case {
    pub name: String,
    /// Provenance (`source:` key in YAML — distinct from the spec's `source` Side).
    #[serde(rename = "source", default)]
    pub provenance: Provenance,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdin: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Golden output captured from the source. `None` until capture runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect: Option<Expect>,
    /// Set when the source disagreed with itself across two runs; quarantined.
    #[serde(default, skip_serializing_if = "is_false")]
    pub nondeterministic: bool,
    /// Per-case compare override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compare: Option<CompareSpec>,
}

/// Golden output captured from the source binary.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Expect {
    #[serde(default)]
    pub stdout: String,
    #[serde(default)]
    pub stderr: String,
    #[serde(default)]
    pub exit_code: i32,
}

/// `skip_serializing_if` helper: serde hands the field by reference, so this
/// takes `&bool` (unlike `std::ops::Not::not`, which takes `bool` by value).
fn is_false(b: &bool) -> bool {
    !*b
}
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p rustyfi-engine behavior::tests`
Expected: PASS (3 tests). Then `cargo fmt` and `cargo clippy -p rustyfi-engine --all-targets -- -D warnings`.

- [ ] **Step 7: Commit**

```bash
git add crates/rustyfi-engine/Cargo.toml crates/rustyfi-engine/Cargo.lock crates/rustyfi-engine/src/lib.rs crates/rustyfi-engine/src/behavior/mod.rs
git commit -m "feat: behavior.yaml serde types"
```

---

### Task 2: Process execution — `{work}` expansion, build, and run with timeout

**Files:**
- Create: `crates/rustyfi-engine/src/behavior/harness.rs`
- Modify: `crates/rustyfi-engine/src/behavior/mod.rs` (add `mod harness; pub use harness::*;` near the top, and the `Outcome` type)

- [ ] **Step 1: Declare the submodule and the runtime Outcome type**

In `crates/rustyfi-engine/src/behavior/mod.rs`, directly under the `use` lines at the top, add:

```rust
mod harness;
pub use harness::{build_side, expand, run_case, Outcome};
```

And add the `Outcome` type after the `Expect` struct:

```rust
/// What one binary actually did on one case (runtime capture).
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}
```

- [ ] **Step 2: Write failing tests for `expand`, `build_side`, and `run_case`**

Create `crates/rustyfi-engine/src/behavior/harness.rs`:

```rust
//! Process execution for the behavior harness: build a side, run a case, and
//! capture its observable behavior with a hard timeout.

use super::{Case, Outcome, Side};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;
use wait_timeout::ChildExt;

/// Maximum wall-clock seconds for a single binary invocation.
const RUN_TIMEOUT_SECS: u64 = 30;
/// Exit code reported when a run is killed for exceeding the timeout.
const TIMEOUT_EXIT_CODE: i32 = 124;

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn echo_side() -> Side {
        // A POSIX `sh -c` shim is a stable, language-agnostic test binary.
        Side {
            lang: "sh".into(),
            dir: ".".into(),
            build: vec!["true".into()],
            run: vec!["sh".into(), "-c".into(),
                      "printf '%s' \"$1\"; printf 'E' 1>&2; exit 3".into(), "sh".into()],
        }
    }

    #[test]
    fn expand_substitutes_work_placeholder() {
        let out = expand(&["{work}/bin".into(), "x".into()], Path::new("/tmp/w"));
        assert_eq!(out, vec!["/tmp/w/bin".to_string(), "x".to_string()]);
    }

    #[test]
    fn run_case_captures_streams_and_exit() {
        let root = std::env::current_dir().unwrap();
        let work = root.clone();
        let case = Case {
            name: "t".into(),
            provenance: Default::default(),
            args: vec!["hello".into()],
            stdin: None,
            env: BTreeMap::new(),
            expect: None,
            nondeterministic: false,
            compare: None,
        };
        let out = run_case(&echo_side(), &case, &root, &work);
        assert_eq!(out.stdout, "hello");
        assert_eq!(out.stderr, "E");
        assert_eq!(out.exit_code, 3);
    }

    #[test]
    fn run_case_feeds_stdin() {
        let root = std::env::current_dir().unwrap();
        let side = Side {
            lang: "sh".into(),
            dir: ".".into(),
            build: vec!["true".into()],
            run: vec!["cat".into()],
        };
        let case = Case {
            name: "t".into(),
            provenance: Default::default(),
            args: vec![],
            stdin: Some("piped-in\n".into()),
            env: BTreeMap::new(),
            expect: None,
            nondeterministic: false,
            compare: None,
        };
        let out = run_case(&side, &case, &root, &root);
        assert_eq!(out.stdout, "piped-in\n");
        assert_eq!(out.exit_code, 0);
    }

    #[test]
    fn build_side_reports_failure() {
        let side = Side {
            lang: "sh".into(),
            dir: ".".into(),
            build: vec!["false".into()],
            run: vec!["true".into()],
        };
        let root = std::env::current_dir().unwrap();
        let err = build_side(&side, "source", &root, &root).unwrap_err();
        assert!(err.contains("source build failed"));
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p rustyfi-engine behavior::harness 2>&1 | head -20`
Expected: FAIL to compile — `cannot find function expand`.

- [ ] **Step 4: Implement `expand`, `build_side`, `run_case`**

Add to `crates/rustyfi-engine/src/behavior/harness.rs` (above the test module):

```rust
/// Substitute the `{work}` scratch-dir placeholder in a command vector.
pub fn expand(cmd: &[String], work: &Path) -> Vec<String> {
    let w = work.to_string_lossy();
    cmd.iter().map(|p| p.replace("{work}", &w)).collect()
}

/// Run a side's build command. Returns `Err(message)` (with captured output)
/// on a non-zero exit so the caller can surface it honestly.
pub fn build_side(side: &Side, label: &str, root: &Path, work: &Path) -> Result<(), String> {
    let cmd = expand(&side.build, work);
    if cmd.is_empty() {
        return Ok(()); // nothing to build (e.g. interpreted source)
    }
    let cwd = root.join(&side.dir);
    let output = Command::new(&cmd[0])
        .args(&cmd[1..])
        .current_dir(&cwd)
        .output()
        .map_err(|e| format!("{label} build failed to spawn ({}): {e}", cmd.join(" ")))?;
    if !output.status.success() {
        return Err(format!(
            "{label} build failed ({}):\n{}\n{}",
            cmd.join(" "),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(())
}

/// Invoke one side's binary for one case and capture its behavior. Reads
/// stdout/stderr on background threads to avoid pipe-buffer deadlock, and
/// kills the process if it exceeds `RUN_TIMEOUT_SECS`.
pub fn run_case(side: &Side, case: &Case, root: &Path, work: &Path) -> Outcome {
    let mut cmd = expand(&side.run, work);
    cmd.extend(case.args.iter().cloned());
    let cwd = root.join(&side.dir);

    let mut command = Command::new(&cmd[0]);
    command
        .args(&cmd[1..])
        .current_dir(&cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in &case.env {
        command.env(k, v);
    }

    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => {
            return Outcome { stdout: String::new(), stderr: format!("<spawn failed: {e}>"), exit_code: TIMEOUT_EXIT_CODE }
        }
    };

    // Feed stdin (drop the handle to signal EOF).
    if let Some(input) = &case.stdin {
        if let Some(mut sin) = child.stdin.take() {
            let _ = sin.write_all(input.as_bytes());
        }
    } else {
        drop(child.stdin.take());
    }

    // Drain stdout/stderr concurrently so a full pipe can't block `wait`.
    let mut out_pipe = child.stdout.take();
    let mut err_pipe = child.stderr.take();
    let out_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(p) = out_pipe.as_mut() {
            use std::io::Read;
            let _ = p.read_to_end(&mut buf);
        }
        buf
    });
    let err_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(p) = err_pipe.as_mut() {
            use std::io::Read;
            let _ = p.read_to_end(&mut buf);
        }
        buf
    });

    let status = match child.wait_timeout(Duration::from_secs(RUN_TIMEOUT_SECS)) {
        Ok(Some(status)) => status.code().unwrap_or(TIMEOUT_EXIT_CODE),
        Ok(None) => {
            let _ = child.kill();
            let _ = child.wait();
            TIMEOUT_EXIT_CODE
        }
        Err(_) => TIMEOUT_EXIT_CODE,
    };

    let stdout = String::from_utf8_lossy(&out_handle.join().unwrap_or_default()).into_owned();
    let stderr = String::from_utf8_lossy(&err_handle.join().unwrap_or_default()).into_owned();
    Outcome { stdout, stderr, exit_code: status }
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p rustyfi-engine behavior::harness`
Expected: PASS (4 tests). Then `cargo fmt` + clippy clean.

- [ ] **Step 6: Commit**

```bash
git add crates/rustyfi-engine/src/behavior/
git commit -m "feat: behavior harness process exec (build/run/timeout)"
```

---

### Task 3: Normalization and the diff engine

**Files:**
- Create: `crates/rustyfi-engine/src/behavior/diff.rs`
- Modify: `crates/rustyfi-engine/src/behavior/mod.rs` (declare `mod diff; pub use diff::*;`)

- [ ] **Step 1: Declare the submodule**

In `crates/rustyfi-engine/src/behavior/mod.rs`, extend the harness `use` block:

```rust
mod diff;
pub use diff::{diff_case, normalize_text};
```

- [ ] **Step 2: Write failing tests**

Create `crates/rustyfi-engine/src/behavior/diff.rs`:

```rust
//! Normalization transforms and the per-case diff engine.

use super::{CompareSpec, Expect, Normalize, Outcome, StreamMode};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_trailing_ws_per_line() {
        let rules = vec![Normalize::StripTrailingWs];
        assert_eq!(normalize_text("a  \nb\t\n", &rules), "a\nb\n");
    }

    #[test]
    fn mask_replaces_regex_matches() {
        let rules = vec![Normalize::Mask {
            pattern: r"\d{4}-\d{2}-\d{2}".into(),
            token: "<DATE>".into(),
        }];
        assert_eq!(normalize_text("on 2026-06-13 ok", &rules), "on <DATE> ok");
    }

    #[test]
    fn matching_case_yields_no_diffs() {
        let expect = Expect { stdout: "11\n".into(), stderr: String::new(), exit_code: 0 };
        let actual = Outcome { stdout: "11\n".into(), stderr: String::new(), exit_code: 0 };
        let diffs = diff_case(&expect, &actual, &CompareSpec::default(), &[]);
        assert!(diffs.is_empty());
    }

    #[test]
    fn divergent_stdout_and_exit_are_reported() {
        let expect = Expect { stdout: "11\n".into(), stderr: String::new(), exit_code: 0 };
        let actual = Outcome { stdout: "11.0000000000\n".into(), stderr: String::new(), exit_code: 1 };
        let diffs = diff_case(&expect, &actual, &CompareSpec::default(), &[]);
        assert_eq!(diffs.len(), 2);
        assert!(diffs[0].starts_with("stdout:"));
        assert!(diffs[1].starts_with("exit_code:"));
    }

    #[test]
    fn ignore_mode_skips_a_stream() {
        let expect = Expect { stdout: "x".into(), stderr: "noise-a".into(), exit_code: 0 };
        let actual = Outcome { stdout: "x".into(), stderr: "noise-b".into(), exit_code: 0 };
        let compare = CompareSpec { stderr: StreamMode::Ignore, ..CompareSpec::default() };
        assert!(diff_case(&expect, &actual, &compare, &[]).is_empty());
    }

    #[test]
    fn normalized_mode_applies_rules_both_sides() {
        let expect = Expect { stdout: "built 2026-06-13\n".into(), stderr: String::new(), exit_code: 0 };
        let actual = Outcome { stdout: "built 2026-01-01 \n".into(), stderr: String::new(), exit_code: 0 };
        let compare = CompareSpec { stdout: StreamMode::Normalized, ..CompareSpec::default() };
        let rules = vec![
            Normalize::Mask { pattern: r"\d{4}-\d{2}-\d{2}".into(), token: "<D>".into() },
            Normalize::StripTrailingWs,
        ];
        assert!(diff_case(&expect, &actual, &compare, &rules).is_empty());
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p rustyfi-engine behavior::diff 2>&1 | head -20`
Expected: FAIL to compile — `cannot find function normalize_text`.

- [ ] **Step 4: Implement normalization + diff**

Add to `crates/rustyfi-engine/src/behavior/diff.rs` (above the tests):

```rust
/// Apply normalization rules in order. `StripTrailingWs` trims each line's
/// trailing whitespace; `Mask` replaces regex matches with a token. A bad
/// regex is skipped (treated as a no-op) rather than panicking.
pub fn normalize_text(text: &str, rules: &[Normalize]) -> String {
    let mut out = text.to_string();
    for rule in rules {
        match rule {
            Normalize::StripTrailingWs => {
                let had_trailing_newline = out.ends_with('\n');
                let mut joined = out
                    .lines()
                    .map(|l| l.trim_end())
                    .collect::<Vec<_>>()
                    .join("\n");
                if had_trailing_newline {
                    joined.push('\n');
                }
                out = joined;
            }
            Normalize::Mask { pattern, token } => {
                if let Ok(re) = regex::Regex::new(pattern) {
                    out = re.replace_all(&out, token.as_str()).into_owned();
                }
            }
        }
    }
    out
}

/// Compare one captured `Outcome` against golden `Expect` under a compare
/// policy. Returns human-readable mismatch lines (empty == behaviors match).
pub fn diff_case(
    expect: &Expect,
    actual: &Outcome,
    compare: &CompareSpec,
    rules: &[Normalize],
) -> Vec<String> {
    let mut diffs = Vec::new();

    for (name, mode, want, got) in [
        ("stdout", compare.stdout, &expect.stdout, &actual.stdout),
        ("stderr", compare.stderr, &expect.stderr, &actual.stderr),
    ] {
        let (w, g) = match mode {
            StreamMode::Ignore => continue,
            StreamMode::Exact => (want.clone(), got.clone()),
            StreamMode::Normalized => (normalize_text(want, rules), normalize_text(got, rules)),
        };
        if w != g {
            diffs.push(format!("{name}: source={w:?} target={g:?}"));
        }
    }

    if compare.exit_code != StreamMode::Ignore && expect.exit_code != actual.exit_code {
        diffs.push(format!(
            "exit_code: source={} target={}",
            expect.exit_code, actual.exit_code
        ));
    }
    diffs
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p rustyfi-engine behavior::diff`
Expected: PASS (6 tests). Then `cargo fmt` + clippy clean.

- [ ] **Step 6: Commit**

```bash
git add crates/rustyfi-engine/src/behavior/
git commit -m "feat: behavior normalization + diff engine"
```

---

### Task 4: Golden capture with nondeterminism self-detection

**Files:**
- Modify: `crates/rustyfi-engine/src/behavior/mod.rs` (add `capture_all` + helper)

- [ ] **Step 1: Write failing tests**

Add a test to the `tests` module in `crates/rustyfi-engine/src/behavior/mod.rs`:

```rust
    #[test]
    fn capture_fills_golden_and_flags_nondeterminism() {
        use std::path::Path;
        // Deterministic source: echoes its arg. Nondeterministic source: prints
        // a nanosecond clock. Both via `sh -c`.
        let mut spec = BehaviorSpec {
            name: "t".into(),
            source: Side {
                lang: "sh".into(),
                dir: ".".into(),
                build: vec![],
                run: vec!["sh".into(), "-c".into(),
                          "if [ \"$1\" = det ]; then printf D; else date +%N; fi".into(), "sh".into()],
            },
            target: Side { lang: "rust".into(), dir: ".".into(), build: vec![], run: vec![] },
            compare: CompareSpec::default(),
            normalize: vec![],
            cases: vec![
                Case { name: "det".into(), provenance: Provenance::Manual, args: vec!["det".into()],
                       stdin: None, env: Default::default(), expect: None, nondeterministic: false, compare: None },
                Case { name: "clock".into(), provenance: Provenance::Manual, args: vec!["clock".into()],
                       stdin: None, env: Default::default(), expect: None, nondeterministic: false, compare: None },
            ],
        };
        let root = std::env::current_dir().unwrap();
        capture_all(&mut spec, &root, Path::new("/tmp")).unwrap();

        let det = &spec.cases[0];
        assert!(!det.nondeterministic);
        assert_eq!(det.expect.as_ref().unwrap().stdout, "D");

        let clock = &spec.cases[1];
        assert!(clock.nondeterministic, "clock case should self-detect as nondeterministic");
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p rustyfi-engine behavior::tests::capture 2>&1 | head -20`
Expected: FAIL to compile — `cannot find function capture_all`.

- [ ] **Step 3: Implement capture**

Add to `crates/rustyfi-engine/src/behavior/mod.rs` (after the type definitions, before the test module):

```rust
use std::path::Path;

/// Build the source once, then capture golden output for every case by running
/// the source binary. Each case is run TWICE: if the source disagrees with
/// itself on any compared stream, the case is flagged `nondeterministic` and
/// quarantined (its `expect` is left as the first run for visibility, but it is
/// excluded from gating/repair by `verify`).
pub fn capture_all(spec: &mut BehaviorSpec, root: &Path, work: &Path) -> Result<(), String> {
    build_side(&spec.source, "source", root, work)?;
    let default_compare = spec.compare;
    let rules = spec.normalize.clone();

    for case in &mut spec.cases {
        let first = run_case(&spec.source, case, root, work);
        let second = run_case(&spec.source, case, root, work);

        let compare = case.compare.unwrap_or(default_compare);
        let first_expect = Expect {
            stdout: first.stdout.clone(),
            stderr: first.stderr.clone(),
            exit_code: first.exit_code,
        };
        // Compare the two source runs against each other using the same diff
        // policy: any difference == nondeterministic.
        let self_diffs = diff_case(&first_expect, &second, &compare, &rules);
        case.nondeterministic = !self_diffs.is_empty();
        case.expect = Some(first_expect);
    }
    Ok(())
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p rustyfi-engine behavior::tests::capture`
Expected: PASS. Then `cargo fmt` + clippy clean.

- [ ] **Step 5: Commit**

```bash
git add crates/rustyfi-engine/src/behavior/mod.rs
git commit -m "feat: golden capture with nondeterminism self-detection"
```

---

### Task 5: Verification + the behavior report

**Files:**
- Modify: `crates/rustyfi-engine/src/behavior/mod.rs` (add report types + `verify`)

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `mod.rs`:

```rust
    #[test]
    fn verify_diffs_target_against_golden_and_skips_quarantined() {
        use std::path::Path;
        let spec = BehaviorSpec {
            name: "t".into(),
            source: Side { lang: "sh".into(), dir: ".".into(), build: vec![], run: vec![] },
            // target echoes its arg with a trailing '!' — so it MISMATCHES golden.
            target: Side {
                lang: "sh".into(),
                dir: ".".into(),
                build: vec![],
                run: vec!["sh".into(), "-c".into(), "printf '%s!' \"$1\"".into(), "sh".into()],
            },
            compare: CompareSpec::default(),
            normalize: vec![],
            cases: vec![
                Case { name: "ok".into(), provenance: Provenance::Manual, args: vec!["hi".into()],
                       stdin: None, env: Default::default(),
                       expect: Some(Expect { stdout: "hi!".into(), stderr: String::new(), exit_code: 0 }),
                       nondeterministic: false, compare: None },
                Case { name: "bad".into(), provenance: Provenance::Manual, args: vec!["hi".into()],
                       stdin: None, env: Default::default(),
                       expect: Some(Expect { stdout: "hi".into(), stderr: String::new(), exit_code: 0 }),
                       nondeterministic: false, compare: None },
                Case { name: "skip".into(), provenance: Provenance::Manual, args: vec!["x".into()],
                       stdin: None, env: Default::default(),
                       expect: Some(Expect { stdout: "anything".into(), stderr: String::new(), exit_code: 0 }),
                       nondeterministic: true, compare: None },
            ],
        };
        let root = std::env::current_dir().unwrap();
        let report = verify(&spec, &root, Path::new("/tmp")).unwrap();
        assert_eq!(report.total, 2, "quarantined case excluded from total");
        assert_eq!(report.matched, 1);
        assert_eq!(report.quarantined, 1);
        assert!(!report.passed);
        let bad = report.cases.iter().find(|c| c.name == "bad").unwrap();
        assert!(!bad.matched);
        assert_eq!(bad.diffs.len(), 1);
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p rustyfi-engine behavior::tests::verify 2>&1 | head -20`
Expected: FAIL to compile — `cannot find function verify`.

- [ ] **Step 3: Implement the report types and `verify`**

Add to `mod.rs` (after `capture_all`):

```rust
/// Per-case verification result.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CaseResult {
    pub name: String,
    pub matched: bool,
    pub diffs: Vec<String>,
}

/// Full verification report (also the on-disk `behavior_report.json` shape).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BehaviorReport {
    pub name: String,
    /// True when every non-quarantined case matched.
    pub passed: bool,
    /// Non-quarantined cases that matched.
    pub matched: usize,
    /// Non-quarantined cases (the gate denominator).
    pub total: usize,
    /// Cases excluded as nondeterministic.
    pub quarantined: usize,
    pub cases: Vec<CaseResult>,
}

/// Build the target, run every NON-quarantined case, and diff against golden.
/// Quarantined cases are counted in `quarantined` but excluded from
/// `total`/`matched`/`passed`. A case with no captured `expect` is skipped with
/// a diagnostic diff (it should never happen after `capture_all`).
pub fn verify(spec: &BehaviorSpec, root: &Path, work: &Path) -> Result<BehaviorReport, String> {
    build_side(&spec.target, "target", root, work)?;
    let default_compare = spec.compare;

    let mut cases = Vec::new();
    let mut matched = 0usize;
    let mut total = 0usize;
    let mut quarantined = 0usize;

    for case in &spec.cases {
        if case.nondeterministic {
            quarantined += 1;
            continue;
        }
        total += 1;
        let diffs = match &case.expect {
            None => vec!["no golden expect captured for this case".to_string()],
            Some(expect) => {
                let actual = run_case(&spec.target, case, root, work);
                let compare = case.compare.unwrap_or(default_compare);
                diff_case(expect, &actual, &compare, &spec.normalize)
            }
        };
        let ok = diffs.is_empty();
        if ok {
            matched += 1;
        }
        cases.push(CaseResult { name: case.name.clone(), matched: ok, diffs });
    }

    Ok(BehaviorReport {
        name: spec.name.clone(),
        passed: matched == total,
        matched,
        total,
        quarantined,
        cases,
    })
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p rustyfi-engine behavior::tests::verify`
Expected: PASS. Then `cargo fmt` + clippy clean.

- [ ] **Step 5: Commit**

```bash
git add crates/rustyfi-engine/src/behavior/mod.rs
git commit -m "feat: behavior verification + report"
```

---

### Task 6: README command-block miner

**Files:**
- Create: `crates/rustyfi-engine/src/behavior/mine.rs`
- Modify: `crates/rustyfi-engine/src/behavior/mod.rs` (declare `mod mine; pub use mine::*;`)

- [ ] **Step 1: Declare the submodule**

In `mod.rs`, extend the submodule block:

```rust
mod mine;
pub use mine::{help_case, mine_readme};
```

- [ ] **Step 2: Write failing tests**

Create `crates/rustyfi-engine/src/behavior/mine.rs`:

```rust
//! Mine candidate CLI invocations from a project's README and `--help`.
//!
//! Recall is best-effort: the hybrid review loop (the user extends
//! `behavior.yaml`) is the mitigation, not a guarantee. Golden values are
//! filled later by `capture_all`.

use super::{Case, Provenance};

#[cfg(test)]
mod tests {
    use super::*;

    const README: &str = r#"
# calc

Run it:

```
$ calc "2 + 3 * (4 - 1)"
11
$ calc "2 ^ 10"
1024
```

Some prose, then a non-matching block:

```
echo unrelated
```
"#;

    #[test]
    fn mines_binary_invocations_from_fenced_blocks() {
        let cases = mine_readme(README, "calc");
        let names: Vec<&str> = cases.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(cases.len(), 2, "two `calc ...` lines, echo line ignored");
        assert_eq!(cases[0].args, vec!["2 + 3 * (4 - 1)".to_string()]);
        assert_eq!(cases[1].args, vec!["2 ^ 10".to_string()]);
        assert_eq!(cases[0].provenance, Provenance::Readme);
        assert!(names[0].starts_with("readme_"));
    }

    #[test]
    fn dedupes_identical_invocations() {
        let md = "```\n$ calc x\n$ calc x\n```\n";
        assert_eq!(mine_readme(md, "calc").len(), 1);
    }

    #[test]
    fn help_case_is_a_help_invocation() {
        let c = help_case();
        assert_eq!(c.args, vec!["--help".to_string()]);
        assert_eq!(c.provenance, Provenance::Help);
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p rustyfi-engine behavior::mine 2>&1 | head -20`
Expected: FAIL to compile — `cannot find function mine_readme`.

- [ ] **Step 4: Implement the miner**

Add to `crates/rustyfi-engine/src/behavior/mine.rs` (above the tests):

```rust
/// Extract candidate invocations of `binary` from fenced code blocks in README
/// markdown. A line is a candidate if, after stripping an optional `$ ` prompt,
/// its first shell token equals `binary`. Args are shell-split; duplicates are
/// dropped. Golden output is NOT captured here (`capture_all` does that).
pub fn mine_readme(markdown: &str, binary: &str) -> Vec<Case> {
    let mut cases = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    let mut in_block = false;

    for line in markdown.lines() {
        if line.trim_start().starts_with("```") {
            in_block = !in_block;
            continue;
        }
        if !in_block {
            continue;
        }
        let cmd = line.trim().strip_prefix("$ ").unwrap_or(line.trim());
        let tokens = match shlex::split(cmd) {
            Some(t) if !t.is_empty() => t,
            _ => continue,
        };
        if tokens[0] != binary {
            continue;
        }
        let args: Vec<String> = tokens[1..].to_vec();
        if !seen.insert(args.clone()) {
            continue;
        }
        cases.push(Case {
            name: format!("readme_{}", cases.len() + 1),
            provenance: Provenance::Readme,
            args,
            stdin: None,
            env: Default::default(),
            expect: None,
            nondeterministic: false,
            compare: None,
        });
    }
    cases
}

/// A `--help` behavioral case (the help text itself must match). Golden output
/// is captured later from the source.
pub fn help_case() -> Case {
    Case {
        name: "help".to_string(),
        provenance: Provenance::Help,
        args: vec!["--help".to_string()],
        stdin: None,
        env: Default::default(),
        expect: None,
        nondeterministic: false,
        compare: None,
    }
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p rustyfi-engine behavior::mine`
Expected: PASS (3 tests). Then `cargo fmt` + clippy clean.

- [ ] **Step 6: Full-crate gates + commit**

```bash
cargo test -p rustyfi-engine
cargo clippy -p rustyfi-engine --all-targets -- -D warnings
cargo fmt --check
git add crates/rustyfi-engine/src/behavior/
git commit -m "feat: README + --help invocation miner"
```

---

## Self-review notes

- **Spec coverage (§5–§8):** types (Task 1), build/run/timeout (Task 2), normalize + diff (Task 3), golden capture + nondeterminism self-detection §8.1 (Task 4), verify + report §13 shape (Task 5), README + `--help` mining §7.1/§7.2 (Task 6). **Deferred to Plan 2** (integration): fixture mining §7.3 (best-effort), `phase_behavior`, checkpoint, CLI, bench. **Plan 3:** repair doctor.
- **Type consistency:** `Provenance` field is `provenance` in Rust, `source` in YAML (via `#[serde(rename)]`) — used consistently across Tasks 1/4/5/6. `CompareSpec`/`StreamMode`/`Normalize`/`Outcome`/`Expect` signatures match across tasks. `capture_all`/`verify` share `(spec, root, work)` ordering.
- **No placeholders:** every step has complete code or an exact command.
- **`Cargo.lock`:** committed in Task 1 because the workspace keeps the lock (binaries → reproducible builds, per `.gitignore`).
