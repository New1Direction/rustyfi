//! $0 leave-one-out retrieval dry-run. Answers, for free, the only question
//! that gates the paid flywheel A/B: does the retriever return meaningful
//! same-language neighbors for a clean crate's files? Uses the public corpus
//! API directly on sentinel regions — no `cargo check`, no model, no key.
//!
//!   cargo run -p rustyfi-engine --example retrieve_dryrun -- \
//!     bench/.work/out/ky  bench/.work/src/ky
use std::collections::BTreeSet;
use std::path::Path;

use rustyfi_engine::corpus::harvest::regions;
use rustyfi_engine::corpus::retrieve::Retriever;
use rustyfi_engine::corpus::signal::{api_surface, jaccard};
use rustyfi_engine::corpus::{CorpusEntry, Tier};

fn lang_of(rel: &str) -> &'static str {
    if rel.ends_with(".ts") || rel.ends_with(".tsx") || rel.ends_with(".js") {
        "typescript"
    } else if rel.ends_with(".go") {
        "go"
    } else if rel.ends_with(".py") {
        "python"
    } else if rel.ends_with(".java") {
        "java"
    } else {
        "other"
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [out, src] = &args[..] else {
        eprintln!("usage: retrieve_dryrun <out_dir> <src_dir>");
        std::process::exit(2);
    };
    let out_dir = Path::new(out);
    let src_dir = Path::new(src);

    let mut entries: Vec<CorpusEntry> = Vec::new();
    for e in walkdir::WalkDir::new(out_dir.join("src"))
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if e.path().extension().and_then(|s| s.to_str()) != Some("rs") {
            continue;
        }
        let Ok(rust) = std::fs::read_to_string(e.path()) else {
            continue;
        };
        for (rel, rust_code) in regions(&rust) {
            let Ok(source_code) = std::fs::read_to_string(src_dir.join(&rel)) else {
                eprintln!("  (source missing on disk: {rel})");
                continue;
            };
            let api = api_surface(&source_code);
            entries.push(CorpusEntry {
                source_lang: lang_of(&rel).to_string(),
                source_api: api.into_iter().collect(),
                source_code,
                rust_code,
                crate_name: out_dir
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "crate".into()),
                file: rel,
                tier: Tier::Compile,
            });
        }
    }

    println!("Harvested {} sentinel regions from {out}", entries.len());
    println!("{:-<74}", "");

    let mut hits = 0usize;
    let mut sum_top1 = 0.0f64;
    for i in 0..entries.len() {
        let held = &entries[i];
        let others: Vec<CorpusEntry> = entries
            .iter()
            .enumerate()
            .filter(|(j, _)| *j != i)
            .map(|(_, e)| e.clone())
            .collect();
        let r = Retriever::from_sources(others, vec![]);
        let query: BTreeSet<String> = held.source_api.iter().cloned().collect();
        let top = r.top_k(&query, &held.source_lang, 3);
        println!(
            "held-out: {}   ({} API symbols)",
            held.file,
            held.source_api.len()
        );
        if top.is_empty() {
            println!("    NO NEIGHBORS (zero same-language overlap)");
        } else {
            hits += 1;
            for (rank, n) in top.iter().enumerate() {
                let nq: BTreeSet<String> = n.source_api.iter().cloned().collect();
                let j = jaccard(&query, &nq);
                if rank == 0 {
                    sum_top1 += j;
                }
                println!("    #{}  J={:.3}  {}", rank + 1, j, n.file);
            }
        }
    }
    println!("{:-<74}", "");
    println!(
        "{}/{} held-out files got >=1 neighbor; mean top-1 Jaccard = {:.3}",
        hits,
        entries.len(),
        if hits > 0 {
            sum_top1 / hits as f64
        } else {
            0.0
        }
    );
}
