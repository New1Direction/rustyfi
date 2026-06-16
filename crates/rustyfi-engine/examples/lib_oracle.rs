//! Run the automated library behavioral oracle on a real crate (model spend).
//!
//!   RUSTYFI_LLM_API_KEY=... RUSTYFI_LLM_BASE_URL=https://api.deepseek.com \
//!   RUSTYFI_LLM_MODEL=deepseek-chat RUSTYFI_NO_TIER=1 \
//!   cargo run -q -p rustyfi-engine --example lib_oracle -- \
//!     bench/.work/out/itsdangerous bench/.work/src/itsdangerous python
use std::path::Path;

use rustyfi_engine::behavior::lib_oracle::verify_library;
use rustyfi_engine::llm::LlmClient;

/// Concatenate the library's own source (minus tests/docs) as the API context
/// handed to the source-driver generator. An optional `filter` substring scopes
/// it to one module (e.g. "signer") — the oracle verifies a given API surface.
fn read_source_api(src_dir: &Path, ext: &str, budget: usize, filter: &str) -> String {
    let mut out = String::new();
    for e in walkdir::WalkDir::new(src_dir)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if e.path().extension().and_then(|s| s.to_str()) != Some(ext) {
            continue;
        }
        let p = e.path().to_string_lossy();
        if p.contains("/test")
            || p.contains("/docs")
            || p.contains("conftest")
            || p.contains("setup")
        {
            continue;
        }
        if !filter.is_empty() && !p.contains(filter) {
            continue;
        }
        if let Ok(t) = std::fs::read_to_string(e.path()) {
            out.push_str(&format!("// FILE: {}\n{t}\n\n", e.path().display()));
            if out.len() > budget {
                break;
            }
        }
    }
    out
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let out_dir = Path::new(args.first().expect("out_dir"));
    let src_dir = Path::new(args.get(1).expect("src_dir"));
    let language = args.get(2).map(|s| s.as_str()).unwrap_or("python");
    let ext = match language {
        "python" => "py",
        "javascript" | "typescript" => "js",
        "go" => "go",
        _ => "txt",
    };

    let filter = args.get(3).map(|s| s.as_str()).unwrap_or("");
    let api = read_source_api(src_dir, ext, 12_000, filter);
    let llm = LlmClient::from_env().expect("set RUSTYFI_LLM_* env");
    let report = verify_library(out_dir, src_dir, language, &api, &llm);

    println!("=== GOLDEN (source) ===\n{}", report.golden.trim());
    println!("\n=== ACTUAL (Rust)   ===\n{}", report.actual.trim());
    if let Some(r) = &report.skipped_reason {
        println!("\n⏭  SKIPPED: {r}");
    }
    println!("\nran={} matched={}", report.ran, report.matched);
    println!(
        "{}",
        if report.matched {
            "✅ LIBRARY BEHAVIORALLY VERIFIED"
        } else if report.skipped_reason.is_some() {
            "⏭  skipped (fail-open, no false mismatch)"
        } else {
            "❌ diverged (real behavioral difference or incomplete translation)"
        }
    );
}
