//! Live measurement: does the agentic `--deep` doctor, driven by a Claude-class
//! model through the local Claude Code CLI, actually reduce `cargo check` errors?
//!
//! This isolates the doctor from the full translation pipeline. It builds a tiny
//! std-only crate with realistic machine-translation errors (a cross-file type
//! mismatch + a method that exists under a different name), then runs the real
//! `run_doctor` loop against it and reports start → end error counts.
//!
//! Run it explicitly (it spends your Claude subscription and needs network):
//!
//! ```bash
//! RUSTYFI_FIX_PROVIDER=claude_cli RUSTYFI_FIX_MODEL=sonnet \
//!   cargo test -p rustyfi-engine --test claude_doctor_live -- --ignored --nocapture
//! ```

use std::fs;

use rustyfi_engine::agent_fix::{run_doctor, DoctorBudget, LlmTransport};
use rustyfi_engine::llm::LlmClient;

/// Two files. `lib.rs` has two errors that can only be fixed by understanding
/// `money.rs` (which the doctor reaches via its cross-file `search` tool):
///   1. `m.as_string()` returns `String`, but it's bound to `i64`  → E0308.
///   2. `m.formatted()` does not exist; `Money` has `display()`    → E0599.
fn write_broken_crate(ws: &std::path::Path) {
    fs::create_dir_all(ws.join("src")).unwrap();
    fs::write(
        ws.join("Cargo.toml"),
        "[package]\nname = \"shop\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n",
    )
    .unwrap();
    fs::write(
        ws.join("src/lib.rs"),
        r#"pub mod money;

use money::Money;

/// Render a checkout line. (Machine-translated — contains real errors.)
pub fn checkout() -> String {
    let m = Money::new(1099);
    let dollars: i64 = m.as_string(); // as_string() returns String, not i64
    let _ = dollars;
    format!("${}", m.formatted())     // Money has no `formatted` — it has `display`
}
"#,
    )
    .unwrap();
    fs::write(
        ws.join("src/money.rs"),
        r#"pub struct Money {
    cents: i64,
}

impl Money {
    pub fn new(cents: i64) -> Self {
        Money { cents }
    }
    pub fn as_string(&self) -> String {
        format!("{}", self.cents)
    }
    pub fn display(&self) -> String {
        format!("{}.{:02}", self.cents / 100, self.cents % 100)
    }
}
"#,
    )
    .unwrap();
}

#[test]
#[ignore = "live: drives the local Claude Code CLI (subscription) + network — run explicitly"]
fn claude_doctor_closes_real_errors() {
    let tmp = tempfile::TempDir::new().unwrap();
    let ws = tmp.path();
    write_broken_crate(ws);

    // Uses RUSTYFI_FIX_PROVIDER / RUSTYFI_FIX_MODEL from the environment.
    // Set RUSTYFI_FIX_PROVIDER=claude_cli (+ RUSTYFI_FIX_MODEL=sonnet|opus) to
    // measure the Claude-class doctor.
    let client = LlmClient::for_fixing().expect("failed to build the fix-loop LLM client");
    eprintln!("\n=== doctor backend: model={} ===", client.model());

    let mut transport = LlmTransport(&client);
    let budget = DoctorBudget {
        max_tool_calls: 20,
        max_wall_secs: 600,
    };

    let report = run_doctor(ws, &mut transport, budget, None, &mut |msg| {
        eprintln!("  · {msg}");
    });

    eprintln!("\n=== DOCTOR REPORT ===");
    eprintln!("  start_errors : {}", report.start_errors);
    eprintln!("  end_errors   : {}", report.end_errors);
    eprintln!("  tool_calls   : {}", report.tool_calls_used);
    eprintln!("  wall_secs    : {}", report.wall_secs);
    eprintln!("  summary      : {}", report.summary);
    eprintln!("=====================\n");

    assert!(
        report.start_errors > 0,
        "fixture should begin with compile errors (got {})",
        report.start_errors
    );
    assert!(
        report.end_errors < report.start_errors,
        "doctor did not reduce the error count: {} -> {}",
        report.start_errors,
        report.end_errors
    );
}
