//! Run the deterministic cross-module import resolver on a crate directory.
//!
//!   cargo run -p rustyfi-engine --example resolve -- /path/to/crate

use std::path::PathBuf;

fn main() {
    let dir = std::env::args().nth(1).expect("usage: resolve <crate-dir>");
    let report = rustyfi_engine::resolve_imports::resolve_crate_imports(&PathBuf::from(dir));
    println!(
        "files_changed={} imports_rewritten={}",
        report.files_changed, report.imports_rewritten
    );
}
