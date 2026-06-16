//! File-level leave-one-out flywheel A/B on a clean crate.
//!
//! For each sentinel region (= one source file), translate it with the model
//! twice per seed: corpus ON (the *other* regions injected as verified few-shot,
//! via the real `build_corpus_context` logic) vs OFF. Splice each result back
//! into the otherwise-verified crate and count `cargo check` errors. The
//! verified original is the 0-error gold reference; the corpus *delta* is the
//! measurement. Only the corpus block differs between arms — same prompt
//! builder (`prompt_translate_with_context`), same system prompt, same model,
//! same temperature.
//!
//! Caveat (disclosed): runs WITHOUT the contract-context machinery, so absolute
//! error counts are higher than production and the corpus delta may be slightly
//! over-stated vs a full pipeline run. Both arms lack it equally → delta valid.
//!
//!   RUSTYFI_LLM_API_KEY=... RUSTYFI_LLM_BASE_URL=https://api.deepseek.com \
//!   RUSTYFI_LLM_MODEL=deepseek-chat RUSTYFI_NO_TIER=1 \
//!   cargo run -q -p rustyfi-engine --example flywheel_ab -- \
//!     bench/.work/out/ky bench/.work/src/ky <seeds> [relsubstr,relsubstr]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use quote::ToTokens;
use rustyfi_engine::corpus::retrieve::Retriever;
use rustyfi_engine::corpus::signal::api_surface;
use rustyfi_engine::corpus::{CorpusEntry, Tier};
use rustyfi_engine::llm::{
    extract_rust_code, prompt_translate_with_context, LlmClient, SYSTEM_TRANSLATE,
};

const LANG: &str = "typescript";
const CORPUS_CTX_BUDGET: usize = 8_000;
const CONTRACT_BUDGET: usize = 20_000;

fn is_pub(v: &syn::Visibility) -> bool {
    matches!(v, syn::Visibility::Public(_))
}

/// The crate's canonical type + signature surface — what the pipeline's contract
/// phase supplies to EVERY translate call. Built deterministically from the clean
/// Rust ($0). Given to BOTH arms so neither is type-blind; the corpus is the only
/// thing that differs.
fn collect_sigs(items: &[syn::Item], out: &mut String) {
    use syn::Item;
    for it in items {
        match it {
            Item::Struct(s) if is_pub(&s.vis) => {
                out.push_str(&s.to_token_stream().to_string());
                out.push('\n');
            }
            Item::Enum(s) if is_pub(&s.vis) => {
                out.push_str(&s.to_token_stream().to_string());
                out.push('\n');
            }
            Item::Union(s) if is_pub(&s.vis) => {
                out.push_str(&s.to_token_stream().to_string());
                out.push('\n');
            }
            Item::Type(s) if is_pub(&s.vis) => {
                out.push_str(&s.to_token_stream().to_string());
                out.push('\n');
            }
            Item::Trait(s) if is_pub(&s.vis) => {
                out.push_str(&s.to_token_stream().to_string());
                out.push('\n');
            }
            Item::Fn(f) if is_pub(&f.vis) => {
                out.push_str("pub ");
                out.push_str(&f.sig.to_token_stream().to_string());
                out.push_str(";\n");
            }
            Item::Impl(im) => {
                let ty = im.self_ty.to_token_stream().to_string();
                let mut methods = String::new();
                for m in &im.items {
                    if let syn::ImplItem::Fn(mf) = m {
                        methods.push_str(&mf.sig.to_token_stream().to_string());
                        methods.push_str(";\n");
                    }
                }
                if !methods.is_empty() {
                    out.push_str(&format!("impl {ty} {{\n{methods}}}\n"));
                }
            }
            Item::Mod(m) => {
                if let Some((_, content)) = &m.content {
                    collect_sigs(content, out);
                }
            }
            _ => {}
        }
    }
}

fn build_contract(src_root: &Path) -> String {
    let mut out = String::new();
    for e in walkdir::WalkDir::new(src_root)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if e.path().extension().and_then(|s| s.to_str()) != Some("rs") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(e.path()) else {
            continue;
        };
        if let Ok(file) = syn::parse_file(&text) {
            collect_sigs(&file.items, &mut out);
        }
    }
    if out.len() > CONTRACT_BUDGET {
        // truncate at a char boundary
        let mut end = CONTRACT_BUDGET;
        while !out.is_char_boundary(end) {
            end -= 1;
        }
        out.truncate(end);
        out.push_str("\n// (contract truncated)\n");
    }
    out
}

/// Verbatim reuse of pipeline::build_corpus_context (public corpus APIs only).
fn corpus_context(source: &str, retriever: &Retriever) -> String {
    let q = api_surface(source);
    let pairs = retriever.top_k(&q, LANG, 3);
    if pairs.is_empty() {
        return String::new();
    }
    let mut ctx = String::from(
        "// Verified examples — prior translations of source with a similar API \
         surface that passed `cargo check`. Follow their structure.\n",
    );
    for p in pairs {
        let note = match p.tier {
            Tier::Behavior => "verified: compiles AND behaves identically",
            Tier::Compile => {
                "verified: compiles (behavior not verified — match structure, not every literal)"
            }
        };
        let block = format!(
            "// [{note}] {LANG} -> rust\n// SOURCE:\n{}\n// RUST:\n{}\n\n",
            p.source_code, p.rust_code
        );
        if ctx.len() + block.len() > CORPUS_CTX_BUDGET {
            break;
        }
        ctx.push_str(&block);
    }
    ctx
}

struct Region {
    rel: String,
    module_file: PathBuf, // .rs file in out_dir/src containing this region
    source_code: String,  // original source file (src_dir/rel)
    rust_body: String,    // verified Rust body between the sentinels
}

/// Replace the body between `// <<<rustyfi:src=REL>>>` and its `end` sentinel.
fn splice_region(module_text: &str, rel: &str, new_body: &str) -> String {
    let start = format!("// <<<rustyfi:src={rel}>>>");
    let end = format!("// <<<rustyfi:end src={rel}>>>");
    let mut out = String::new();
    let mut lines = module_text.lines();
    let mut spliced = false;
    while let Some(l) = lines.next() {
        out.push_str(l);
        out.push('\n');
        if l.trim_start() == start {
            // emit new body, then skip original lines up to (not incl.) end
            out.push_str(new_body);
            if !new_body.ends_with('\n') {
                out.push('\n');
            }
            for inner in lines.by_ref() {
                if inner.trim_start() == end {
                    out.push_str(inner);
                    out.push('\n');
                    break;
                }
            }
            spliced = true;
        }
    }
    assert!(spliced, "sentinel for {rel} not found");
    out
}

fn copy_dir(src: &Path, dst: &Path) {
    for entry in walkdir::WalkDir::new(src)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let rel = entry.path().strip_prefix(src).unwrap();
        let comps: Vec<_> = rel.iter().map(|s| s.to_string_lossy()).collect();
        if comps.iter().any(|c| c == "target" || c == ".git") {
            continue;
        }
        let target = dst.join(rel);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&target).ok();
        } else if entry.file_type().is_file() {
            if let Some(p) = target.parent() {
                std::fs::create_dir_all(p).ok();
            }
            std::fs::copy(entry.path(), &target).ok();
        }
    }
}

/// (errors, passed)
fn cargo_check_errors(dir: &Path) -> (usize, bool) {
    let out = Command::new("cargo")
        .args(["check", "--message-format=json", "--quiet"])
        .current_dir(dir)
        .output()
        .expect("run cargo check");
    let mut errors = 0usize;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v.get("reason").and_then(|r| r.as_str()) == Some("compiler-message")
            && v.get("message")
                .and_then(|m| m.get("level"))
                .and_then(|l| l.as_str())
                == Some("error")
        {
            errors += 1;
        }
    }
    (errors, out.status.success())
}

fn complete_retry(llm: &LlmClient, user: &str) -> Option<String> {
    for _ in 0..3 {
        if let Ok(raw) = llm.complete(SYSTEM_TRANSLATE, user) {
            return Some(raw);
        }
    }
    None
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let out_dir = PathBuf::from(args.first().expect("out_dir"));
    let src_dir = PathBuf::from(args.get(1).expect("src_dir"));
    let seeds: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(3);
    let filters: Vec<String> = args
        .get(3)
        .map(|s| s.split(',').map(|x| x.to_string()).collect())
        .unwrap_or_default();

    // ── discover regions ────────────────────────────────────────────────────
    let mut regions: Vec<Region> = Vec::new();
    for e in walkdir::WalkDir::new(out_dir.join("src"))
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if e.path().extension().and_then(|s| s.to_str()) != Some("rs") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(e.path()) else {
            continue;
        };
        for (rel, rust_body) in rustyfi_engine::corpus::harvest::regions(&text) {
            let Ok(source_code) = std::fs::read_to_string(src_dir.join(&rel)) else {
                continue;
            };
            regions.push(Region {
                rel,
                module_file: e.path().to_path_buf(),
                source_code,
                rust_body,
            });
        }
    }
    println!(
        "Discovered {} regions in {}",
        regions.len(),
        out_dir.display()
    );

    let all_entries: Vec<CorpusEntry> = regions
        .iter()
        .map(|r| CorpusEntry {
            source_lang: LANG.into(),
            source_api: api_surface(&r.source_code).into_iter().collect(),
            source_code: r.source_code.clone(),
            rust_code: r.rust_body.clone(),
            crate_name: "ky".into(),
            file: r.rel.clone(),
            tier: Tier::Compile,
        })
        .collect();

    // ── scratch crate + pristine module texts ───────────────────────────────
    let scratch = out_dir
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("ab-scratch");
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch).unwrap();
    let scratch_crate = scratch.join("crate");
    copy_dir(&out_dir, &scratch_crate);

    let mut pristine: BTreeMap<PathBuf, String> = BTreeMap::new();
    for r in &regions {
        let rel = r.module_file.strip_prefix(&out_dir).unwrap();
        let sp = scratch_crate.join(rel);
        if let std::collections::btree_map::Entry::Vacant(v) = pristine.entry(sp.clone()) {
            v.insert(std::fs::read_to_string(&sp).unwrap());
        }
    }
    let restore = |pristine: &BTreeMap<PathBuf, String>| {
        for (p, t) in pristine {
            std::fs::write(p, t).unwrap();
        }
    };

    restore(&pristine);
    let (base_err, base_pass) = cargo_check_errors(&scratch_crate);
    println!(
        "Baseline (verified, warm-up): errors={base_err} pass={base_pass}\n{:-<74}",
        ""
    );

    let llm = LlmClient::from_env().expect("LlmClient::from_env (set RUSTYFI_LLM_* )");
    let contract = build_contract(&out_dir.join("src"));
    println!(
        "Contract surface: {} chars (given to BOTH arms — fair type baseline)\n{:-<74}",
        contract.len(),
        ""
    );

    // results[rel] = (off_errs, on_errs)
    let mut results: BTreeMap<String, (Vec<i64>, Vec<i64>)> = BTreeMap::new();

    for (i, held) in regions.iter().enumerate() {
        if !filters.is_empty() && !filters.iter().any(|f| held.rel.contains(f.as_str())) {
            continue;
        }
        let others: Vec<CorpusEntry> = all_entries
            .iter()
            .enumerate()
            .filter(|(j, _)| *j != i)
            .map(|(_, e)| e.clone())
            .collect();
        let retriever = Retriever::from_sources(others, vec![]);
        let corpus_ctx = corpus_context(&held.source_code, &retriever);
        let on_ctx = format!("{contract}\n{corpus_ctx}");
        let file_name = Path::new(&held.rel)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| held.rel.clone());
        let module_sp = scratch_crate.join(held.module_file.strip_prefix(&out_dir).unwrap());

        let entry = results.entry(held.rel.clone()).or_default();
        for (arm, ctx) in [("OFF", contract.as_str()), ("ON", on_ctx.as_str())] {
            for s in 0..seeds {
                let prompt = prompt_translate_with_context(
                    &held.source_code,
                    "TypeScript",
                    &file_name,
                    0,
                    1,
                    ctx,
                    &[],
                );
                let rec = match complete_retry(&llm, &prompt) {
                    Some(raw) => {
                        let rust = extract_rust_code(&raw);
                        restore(&pristine);
                        let spliced = splice_region(&pristine[&module_sp], &held.rel, &rust);
                        std::fs::write(&module_sp, &spliced).unwrap();
                        let (errs, _pass) = cargo_check_errors(&scratch_crate);
                        errs as i64
                    }
                    None => {
                        eprintln!("  [{arm} s{s}] {} — LLM failed, recording -1", held.rel);
                        -1
                    }
                };
                println!("  {:<34} {arm} seed{s}: errors={rec}", held.rel);
                if arm == "OFF" {
                    entry.0.push(rec);
                } else {
                    entry.1.push(rec);
                }
            }
        }
        restore(&pristine);
    }

    // ── summary ─────────────────────────────────────────────────────────────
    let stat = |v: &[i64]| -> (f64, i64, i64) {
        let ok: Vec<i64> = v.iter().copied().filter(|x| *x >= 0).collect();
        if ok.is_empty() {
            return (f64::NAN, -1, -1);
        }
        let mean = ok.iter().sum::<i64>() as f64 / ok.len() as f64;
        (mean, *ok.iter().min().unwrap(), *ok.iter().max().unwrap())
    };
    println!("\n{:=<86}", "");
    println!(
        "{:<34} {:>18} {:>18}",
        "file (held out)", "OFF mean[min..max]", "ON mean[min..max]"
    );
    println!("{:-<86}", "");
    let (mut sum_off, mut sum_on, mut n) = (0.0, 0.0, 0.0);
    for (rel, (off, on)) in &results {
        let (om, oi, oa) = stat(off);
        let (nm, ni, na) = stat(on);
        println!(
            "{:<34} {:>8.1} [{:>3}..{:>3}] {:>8.1} [{:>3}..{:>3}]",
            rel, om, oi, oa, nm, ni, na
        );
        if om.is_finite() && nm.is_finite() {
            sum_off += om;
            sum_on += nm;
            n += 1.0;
        }
    }
    println!("{:-<86}", "");
    if n > 0.0 {
        println!(
            "MEAN over {n} files:    OFF={:.2}    ON={:.2}    delta(ON-OFF)={:+.2}",
            sum_off / n,
            sum_on / n,
            (sum_on - sum_off) / n
        );
        println!("(negative delta = corpus reduced errors)");
    }
    let _ = std::fs::remove_dir_all(&scratch);
}
