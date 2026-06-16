//! Regenerate the seed corpus from clean crates already on disk ($0, no key):
//!
//!   cargo run -p rustyfi-engine --example harvest_corpus -- \
//!     bench/.work/out/ky          bench/.work/src/ky \
//!     bench/.work/out/calculator  examples/calculator  > corpus/seed.jsonl
//!
//! Takes (out_dir, src_dir) pairs on argv; harvests each crate that passes
//! `cargo check`; writes one JSON line per verified (source → Rust) pair to
//! stdout, per-crate counts to stderr.
use rustyfi_engine::corpus::harvest::harvest_crate;
use std::path::Path;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut all = Vec::new();
    for pair in args.chunks(2) {
        let [out, src] = pair else {
            eprintln!("skipping unpaired arg: {pair:?}");
            continue;
        };
        let pairs = harvest_crate(Path::new(out), Path::new(src));
        eprintln!("{out}: {} pairs", pairs.len());
        all.extend(pairs);
    }
    for e in &all {
        println!("{}", serde_json::to_string(e).expect("serialize entry"));
    }
    eprintln!("total: {} pairs", all.len());
}
