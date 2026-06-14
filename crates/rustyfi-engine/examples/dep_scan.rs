//! Report which curated registry crates a crate's `src/` references but its
//! `Cargo.toml` doesn't declare — the deterministic missing-dependency lever.
//! Replicates what the translation-time dep pass would add.
//!
//!   cargo run -p rustyfi-engine --example dep_scan -- <crate-dir>

use std::collections::BTreeSet;
use std::path::Path;

use rustyfi_engine::deps::scan_crate_heads_for_registry;

fn main() {
    let dir = std::env::args()
        .nth(1)
        .expect("usage: dep_scan <crate-dir>");
    let root = Path::new(&dir);
    let cargo = std::fs::read_to_string(root.join("Cargo.toml")).unwrap_or_default();

    let mut found: BTreeSet<&'static str> = BTreeSet::new();
    for e in walkdir::WalkDir::new(root.join("src"))
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if e.path().extension().and_then(|s| s.to_str()) != Some("rs") {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(e.path()) {
            for spec in scan_crate_heads_for_registry(&text) {
                found.insert(spec.krate);
            }
        }
    }

    for krate in &found {
        let declared = cargo.contains(krate) || cargo.contains(&krate.replace('-', "_"));
        println!("{} {krate}", if declared { "  ok   " } else { "MISSING" });
    }
}
