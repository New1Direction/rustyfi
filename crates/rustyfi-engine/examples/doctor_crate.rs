//! Measure the `--deep` doctor on an EXISTING crate — e.g. a bench output that
//! compiles with residual errors — without re-translating (no translation key
//! needed, only the local Claude Code CLI for the fix model).
//!
//! The crate is copied to a temp dir first, so the original on disk is never
//! mutated and stays a pristine baseline for re-runs. Reports start → end
//! `cargo check` error counts, tool calls, and wall time.
//!
//!   RUSTYFI_FIX_PROVIDER=claude_cli RUSTYFI_FIX_MODEL=sonnet \
//!     cargo run -p rustyfi-engine --example doctor_crate -- <crate-dir>

use std::path::Path;
use std::process::Command;

use rustyfi_engine::agent_fix::{run_doctor, DoctorBudget, LlmTransport};
use rustyfi_engine::llm::LlmClient;

/// Copy the minimum a crate needs to `cargo check`: `Cargo.toml`, an optional
/// `Cargo.lock`, and the whole `src/` tree. `run_doctor` only edits `src/`.
fn copy_crate(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).expect("create temp crate dir");
    for name in ["Cargo.toml", "Cargo.lock"] {
        let p = src.join(name);
        if p.exists() {
            std::fs::copy(&p, dst.join(name)).expect("copy manifest");
        }
    }
    let status = Command::new("cp")
        .arg("-R")
        .arg(src.join("src"))
        .arg(dst.join("src"))
        .status()
        .expect("spawn cp");
    assert!(
        status.success(),
        "failed to copy src/ from {}",
        src.display()
    );
}

fn main() {
    let dir = std::env::args()
        .nth(1)
        .expect("usage: doctor_crate <crate-dir>");
    let src = Path::new(&dir);

    let tmp = tempfile::TempDir::new().expect("temp dir");
    let ws = tmp.path();
    copy_crate(src, ws);

    let client = LlmClient::for_fixing()
        .expect("build fix client — set RUSTYFI_FIX_PROVIDER=claude_cli RUSTYFI_FIX_MODEL=sonnet");
    eprintln!("=== doctor: model={} on {} ===", client.model(), dir);

    let mut transport = LlmTransport(&client);
    let budget = DoctorBudget {
        max_tool_calls: 60,
        max_wall_secs: 1800,
    };

    let report = run_doctor(ws, &mut transport, budget, None, &mut |m| {
        eprintln!("  · {m}");
    });

    eprintln!(
        "  start={} end={} calls={} secs={}",
        report.start_errors, report.end_errors, report.tool_calls_used, report.wall_secs
    );
    // One machine-readable line to stdout for tabulating across crates.
    println!(
        "{dir}\tstart={}\tend={}\tcalls={}\tsecs={}",
        report.start_errors, report.end_errors, report.tool_calls_used, report.wall_secs
    );
}
