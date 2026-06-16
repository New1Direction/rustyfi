# Translation Flywheel Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make rustyfi self-improving — feed its own `cargo check`-verified `(source → Rust)` pairs back into translation as retrieved few-shot examples.

**Architecture:** A new `crates/rustyfi-engine/src/corpus/` module (types, signal, store, retrieve, harvest). Harvest mines clean crates on disk into `(source → Rust)` pairs via the `// <<<rustyfi:src=…>>>` sentinels; retrieval ranks pairs by source-API-surface Jaccard; `phase_translate` injects the top-K into the existing `rust_context` slot. Fail-open everywhere.

**Tech Stack:** Rust, `serde`/`serde_json` (JSONL), existing `analysis::detect_language`, `scaffold` sentinels. No new deps. No model/key required for v1.

**Spec:** `docs/superpowers/specs/2026-06-14-translation-flywheel-design.md`

---

## File Structure

- Create `crates/rustyfi-engine/src/corpus/mod.rs` — `CorpusEntry`, `Tier`, re-exports.
- Create `crates/rustyfi-engine/src/corpus/signal.rs` — `api_surface`, `jaccard`.
- Create `crates/rustyfi-engine/src/corpus/store.rs` — JSONL read/append, cache path.
- Create `crates/rustyfi-engine/src/corpus/retrieve.rs` — `Retriever`, `top_k`.
- Create `crates/rustyfi-engine/src/corpus/harvest.rs` — `harvest_crate`.
- Create `crates/rustyfi-engine/examples/harvest_corpus.rs` — regenerate `corpus/seed.jsonl`.
- Modify `crates/rustyfi-engine/src/lib.rs` — add `pub mod corpus;`.
- Modify `crates/rustyfi-engine/src/pipeline.rs` — `CORPUS_CTX_BUDGET`, `build_corpus_context`, inject in `phase_translate`, append after a clean run.
- Create `corpus/seed.jsonl` (repo root) — the committed seed, produced by the example.

---

## Task 1: Types + module wiring

**Files:** Create `corpus/mod.rs`; Modify `lib.rs:17`.

- [ ] **Step 1: Write the failing test** (in `corpus/mod.rs`)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn entry_jsonl_round_trips() {
        let e = CorpusEntry {
            source_lang: "go".into(),
            source_api: vec!["fmt.Printf".into()],
            source_code: "package main".into(),
            rust_code: "fn main() {}".into(),
            crate_name: "calc".into(),
            file: "main.go".into(),
            tier: Tier::Behavior,
        };
        let line = serde_json::to_string(&e).unwrap();
        let back: CorpusEntry = serde_json::from_str(&line).unwrap();
        assert_eq!(back.source_api, e.source_api);
        assert_eq!(back.tier, Tier::Behavior);
        assert!(line.contains("\"tier\":\"behavior\""));
    }
}
```

- [ ] **Step 2: Implement types** (top of `corpus/mod.rs`)

```rust
//! Self-improving translation: a corpus of `cargo check`-verified
//! (source → Rust) pairs, retrieved as few-shot examples at translate time.
use serde::{Deserialize, Serialize};

pub mod harvest;
pub mod retrieve;
pub mod signal;
pub mod store;

/// Trust level of a verified pair. `Compile` = valid Rust but behaviour
/// unverified (it can still encode a behavioural bug, e.g. wrong float
/// formatting). `Behavior` = also passed the behavioural oracle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    Compile,
    Behavior,
}

/// One verified (source → Rust) translation pair.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CorpusEntry {
    pub source_lang: String,
    pub source_api: Vec<String>,
    pub source_code: String,
    pub rust_code: String,
    pub crate_name: String,
    pub file: String,
    pub tier: Tier,
}
```

- [ ] **Step 3: Wire module** — add `pub mod corpus;` to `lib.rs` after `pub mod contract_check;`.
- [ ] **Step 4: Run** `cargo test -p rustyfi-engine corpus::tests::entry_jsonl_round_trips` → PASS.
- [ ] **Step 5: Commit** `feat(corpus): verified-pair types + JSONL round-trip`.

## Task 2: signal.rs — API surface + Jaccard

**Files:** Create `corpus/signal.rs`.

- [ ] **Step 1: Failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn extracts_dotted_and_scoped_api() {
        let s = "fmt.Printf(\"%g\", x); v := strconv.ParseFloat(t); a::b()";
        let api = api_surface(s);
        assert!(api.contains("fmt.Printf"));
        assert!(api.contains("strconv.ParseFloat"));
        assert!(api.contains("a.b")); // :: normalised to .
    }
    #[test]
    fn jaccard_basic() {
        let a: std::collections::BTreeSet<String> = ["x".into(), "y".into()].into();
        let b: std::collections::BTreeSet<String> = ["y".into(), "z".into()].into();
        assert!((jaccard(&a, &b) - 1.0 / 3.0).abs() < 1e-9);
        assert_eq!(jaccard(&a, &a), 1.0);
    }
}
```

- [ ] **Step 2: Implement** — language-agnostic heuristic: collect `head.member` / `head::member` tokens where `head` is a lowercase-initial identifier (packages/modules, not types). `::` normalised to `.`.

```rust
use std::collections::BTreeSet;

/// The external API surface of a source file: qualified accesses like
/// `fmt.Printf` / `axios.get` / `os::path` (`::` normalised to `.`). This is
/// the emergent ontology used as the retrieval key — what a translation must
/// get right, learned from data rather than a hand-authored table.
pub fn api_surface(source: &str) -> BTreeSet<String> {
    let b = source.as_bytes();
    let mut out = BTreeSet::new();
    let is_id = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    let mut i = 0;
    while i < b.len() {
        // start of an identifier (head)
        if (b[i].is_ascii_lowercase()) && (i == 0 || !is_id(b[i - 1])) {
            let hs = i;
            while i < b.len() && is_id(b[i]) {
                i += 1;
            }
            let head = &source[hs..i];
            // a `.` or `::` separator?
            let mut j = i;
            let sep = if j < b.len() && b[j] == b'.' {
                j += 1;
                true
            } else if j + 1 < b.len() && b[j] == b':' && b[j + 1] == b':' {
                j += 2;
                true
            } else {
                false
            };
            if sep && j < b.len() && (b[j].is_ascii_alphabetic() || b[j] == b'_') {
                let ms = j;
                while j < b.len() && is_id(b[j]) {
                    j += 1;
                }
                out.insert(format!("{head}.{}", &source[ms..j]));
                i = j;
            }
            continue;
        }
        i += 1;
    }
    out
}

/// Jaccard similarity of two API-surface sets (0.0..=1.0).
pub fn jaccard(a: &BTreeSet<String>, b: &BTreeSet<String>) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 0.0;
    }
    let inter = a.intersection(b).count();
    let union = a.len() + b.len() - inter;
    inter as f64 / union as f64
}
```

- [ ] **Step 3: Run** `cargo test -p rustyfi-engine corpus::signal` → PASS.
- [ ] **Step 4: Commit** `feat(corpus): source API-surface extraction + Jaccard`.

## Task 3: store.rs — JSONL persistence + cache path

**Files:** Create `corpus/store.rs`.

- [ ] **Step 1: Failing tests** — round-trip via tempfile; missing file → `[]`; `local_cache_path` honours `XDG_CACHE_HOME`.

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::corpus::{CorpusEntry, Tier};
    fn e(name: &str) -> CorpusEntry {
        CorpusEntry { source_lang: "go".into(), source_api: vec![], source_code: "s".into(),
            rust_code: "r".into(), crate_name: name.into(), file: "f".into(), tier: Tier::Compile }
    }
    #[test]
    fn append_then_read_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("c.jsonl");
        append_jsonl(&p, &[e("a"), e("b")]).unwrap();
        append_jsonl(&p, &[e("c")]).unwrap();
        let all = read_jsonl(&p);
        assert_eq!(all.len(), 3);
        assert_eq!(all[2].crate_name, "c");
    }
    #[test]
    fn missing_file_reads_empty() {
        assert!(read_jsonl(std::path::Path::new("/no/such/corpus.jsonl")).is_empty());
    }
}
```

- [ ] **Step 2: Implement**

```rust
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use crate::corpus::CorpusEntry;

/// Read a JSONL corpus. Missing file or any unparseable line is skipped —
/// the corpus is an enhancement, never a hard dependency (fail-open).
pub fn read_jsonl(path: &Path) -> Vec<CorpusEntry> {
    let Ok(text) = fs::read_to_string(path) else { return Vec::new() };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<CorpusEntry>(l).ok())
        .collect()
}

/// Append entries as JSONL (creating the file + parent dirs as needed).
pub fn append_jsonl(path: &Path, entries: &[CorpusEntry]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    for e in entries {
        let line = serde_json::to_string(e).map_err(std::io::Error::other)?;
        writeln!(f, "{line}")?;
    }
    Ok(())
}

/// Local growth cache: `$XDG_CACHE_HOME/rustyfi/corpus.jsonl`, else
/// `~/.cache/rustyfi/corpus.jsonl`. `None` if neither is resolvable.
pub fn local_cache_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
    Some(base.join("rustyfi").join("corpus.jsonl"))
}
```

- [ ] **Step 3: Run** `cargo test -p rustyfi-engine corpus::store` → PASS.
- [ ] **Step 4: Commit** `feat(corpus): JSONL store + local cache path`.

## Task 4: retrieve.rs — ranked retrieval

**Files:** Create `corpus/retrieve.rs`.

- [ ] **Step 1: Failing test** — ranking honours Jaccard, then behavior>compile, then local>seed.

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::corpus::{CorpusEntry, Tier};
    fn mk(name: &str, api: &[&str], tier: Tier) -> CorpusEntry {
        CorpusEntry { source_lang: "go".into(), source_api: api.iter().map(|s| s.to_string()).collect(),
            source_code: "s".into(), rust_code: "r".into(), crate_name: name.into(), file: "f".into(), tier }
    }
    #[test]
    fn ranks_by_overlap_then_tier() {
        let seed = vec![
            mk("low", &["a.x"], Tier::Behavior),
            mk("high_compile", &["a.x", "b.y"], Tier::Compile),
        ];
        let local = vec![mk("high_behavior", &["a.x", "b.y"], Tier::Behavior)];
        let r = Retriever::from_sources(seed, local);
        let q: std::collections::BTreeSet<String> = ["a.x".into(), "b.y".into()].into();
        let top = r.top_k(&q, "go", 3);
        assert_eq!(top[0].crate_name, "high_behavior"); // best overlap + behavior + local
        assert_eq!(top[1].crate_name, "high_compile");
        assert_eq!(top[2].crate_name, "low");
    }
    #[test]
    fn filters_language_and_zero_overlap() {
        let r = Retriever::from_sources(vec![mk("x", &["z.z"], Tier::Compile)], vec![]);
        let q: std::collections::BTreeSet<String> = ["a.x".into()].into();
        assert!(r.top_k(&q, "go", 3).is_empty());   // zero overlap dropped
        assert!(r.top_k(&q, "python", 3).is_empty()); // wrong language dropped
    }
}
```

- [ ] **Step 2: Implement**

```rust
use std::collections::BTreeSet;
use crate::corpus::signal::jaccard;
use crate::corpus::CorpusEntry;

struct Ranked {
    entry: CorpusEntry,
    api: BTreeSet<String>,
    is_local: bool,
}

/// Ranks verified pairs against a query file's API surface.
pub struct Retriever {
    items: Vec<Ranked>,
}

impl Retriever {
    pub fn from_sources(seed: Vec<CorpusEntry>, local: Vec<CorpusEntry>) -> Self {
        let mut items = Vec::new();
        for (entries, is_local) in [(seed, false), (local, true)] {
            for e in entries {
                let api = e.source_api.iter().cloned().collect();
                items.push(Ranked { entry: e, api, is_local });
            }
        }
        Self { items }
    }

    /// Top-K pairs for `query` in `lang`, best first. Drops wrong-language and
    /// zero-overlap entries. Order: Jaccard desc, behavior>compile, local>seed,
    /// shorter source first.
    pub fn top_k(&self, query: &BTreeSet<String>, lang: &str, k: usize) -> Vec<&CorpusEntry> {
        use crate::corpus::Tier;
        let mut scored: Vec<(f64, &Ranked)> = self
            .items
            .iter()
            .filter(|r| r.entry.source_lang == lang)
            .map(|r| (jaccard(query, &r.api), r))
            .filter(|(s, _)| *s > 0.0)
            .collect();
        scored.sort_by(|(sa, a), (sb, b)| {
            sb.partial_cmp(sa)
                .unwrap()
                .then(tier_rank(b.entry.tier).cmp(&tier_rank(a.entry.tier)))
                .then(b.is_local.cmp(&a.is_local))
                .then(a.entry.source_code.len().cmp(&b.entry.source_code.len()))
        });
        scored.into_iter().take(k).map(|(_, r)| &r.entry).collect()
    }
}

fn tier_rank(t: crate::corpus::Tier) -> u8 {
    match t {
        crate::corpus::Tier::Behavior => 1,
        crate::corpus::Tier::Compile => 0,
    }
}
```

- [ ] **Step 3: Run** `cargo test -p rustyfi-engine corpus::retrieve` → PASS.
- [ ] **Step 4: Commit** `feat(corpus): API-surface retrieval with tier/locality ranking`.

## Task 5: harvest.rs — mine clean crates into pairs

**Files:** Create `corpus/harvest.rs`.

- [ ] **Step 1: Failing test** — a fixture crate dir with a sentinel-delimited Rust module + a matching source file → one pair; non-clean → empty.

```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn splits_sentinels_into_pairs() {
        let rust = "// <<<rustyfi:src=calc/lexer.go>>>\nfn lex() {}\n// <<<rustyfi:end src=calc/lexer.go>>>\n";
        let pairs = regions(rust);
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].0, "calc/lexer.go");
        assert!(pairs[0].1.contains("fn lex"));
    }
}
```

- [ ] **Step 2: Implement region splitter + harvest**

```rust
use std::path::Path;
use std::process::Command;
use crate::analysis::detect_language_key; // see Step 2b
use crate::corpus::signal::api_surface;
use crate::corpus::{CorpusEntry, Tier};

/// Parse `// <<<rustyfi:src=PATH>>> … // <<<rustyfi:end src=PATH>>>` regions
/// out of one generated Rust file → (source_rel_path, rust_body) pairs.
pub fn regions(rust: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut lines = rust.lines().peekable();
    while let Some(line) = lines.next() {
        let t = line.trim_start();
        if let Some(rest) = t.strip_prefix("// <<<rustyfi:src=") {
            let Some(path) = rest.strip_suffix(">>>") else { continue };
            let end = format!("// <<<rustyfi:end src={path}>>>");
            let mut body = String::new();
            for l in lines.by_ref() {
                if l.trim_start() == end {
                    break;
                }
                body.push_str(l);
                body.push('\n');
            }
            if !body.trim().is_empty() {
                out.push((path.to_string(), body));
            }
        }
    }
    out
}

/// Harvest verified pairs from a crate that passes `cargo check`.
/// `out_dir` = generated crate, `src_dir` = original source root.
pub fn harvest_crate(out_dir: &Path, src_dir: &Path) -> Vec<CorpusEntry> {
    if !cargo_check_clean(out_dir) {
        return Vec::new();
    }
    let tier = behavior_tier(out_dir);
    let crate_name = out_dir.file_name().and_then(|s| s.to_str()).unwrap_or("crate").to_string();
    let mut entries = Vec::new();
    for entry in walkdir::WalkDir::new(out_dir.join("src")).into_iter().filter_map(|e| e.ok()) {
        if entry.path().extension().and_then(|s| s.to_str()) != Some("rs") {
            continue;
        }
        let Ok(rust_text) = std::fs::read_to_string(entry.path()) else { continue };
        for (rel, rust_code) in regions(&rust_text) {
            let Ok(source_code) = std::fs::read_to_string(src_dir.join(&rel)) else { continue };
            let Some(lang) = detect_language_key(Path::new(&rel)) else { continue };
            entries.push(CorpusEntry {
                source_lang: lang,
                source_api: api_surface(&source_code).into_iter().collect(),
                source_code,
                rust_code,
                crate_name: crate_name.clone(),
                file: rel,
                tier,
            });
        }
    }
    entries
}

fn cargo_check_clean(dir: &Path) -> bool {
    Command::new("cargo")
        .args(["check", "--quiet"])
        .current_dir(dir)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn behavior_tier(out_dir: &Path) -> Tier {
    let p = out_dir.join("behavior_report.json");
    let passed = std::fs::read_to_string(&p)
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| v.get("passed").and_then(|b| b.as_bool()))
        .unwrap_or(false);
    if passed { Tier::Behavior } else { Tier::Compile }
}
```

- [ ] **Step 2b: Add `detect_language_key`** to `analysis.rs` — a small pub helper wrapping the existing `detect_language` + `language_key` to return a lowercase key string (e.g. "go", "python", "typescript") for a path, or `None`.

```rust
/// Lowercase language key for a path (e.g. "go"), or None if unrecognised.
pub fn detect_language_key(path: &std::path::Path) -> Option<String> {
    detect_language(path).map(|l| language_key(&l))
}
```

- [ ] **Step 3: Run** `cargo test -p rustyfi-engine corpus::harvest` → PASS.
- [ ] **Step 4: Commit** `feat(corpus): harvest verified pairs from clean crates via sentinels`.

## Task 6: examples/harvest_corpus.rs + generate the real seed

**Files:** Create `crates/rustyfi-engine/examples/harvest_corpus.rs`; Create `corpus/seed.jsonl`.

- [ ] **Step 1: Implement the example** — takes `(out_dir, src_dir)` pairs on argv, harvests each, writes JSONL to stdout (redirected into `corpus/seed.jsonl`).

```rust
//! Regenerate the seed corpus from clean crates already on disk ($0, no key):
//!   cargo run -p rustyfi-engine --example harvest_corpus -- \
//!     bench/.work/out/ky bench/.work/src/ky \
//!     bench/.work/out/calculator examples/calculator  > corpus/seed.jsonl
use rustyfi_engine::corpus::harvest::harvest_crate;
use std::path::Path;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut all = Vec::new();
    for pair in args.chunks(2) {
        let [out, src] = pair else { continue };
        let n = harvest_crate(Path::new(out), Path::new(src));
        eprintln!("{out}: {} pairs", n.len());
        all.extend(n);
    }
    for e in &all {
        println!("{}", serde_json::to_string(e).unwrap());
    }
    eprintln!("total: {} pairs", all.len());
}
```

- [ ] **Step 2: Run it** against the clean crates on disk (ky, itsdangerous, emoji-java, calculator) → write `corpus/seed.jsonl`. Verify line count > 0 and `eprintln` per-crate counts are non-zero for at least one crate.
- [ ] **Step 3: Commit** `feat(corpus): harvest_corpus example + generated seed corpus`.

## Task 7: Wire retrieval into translation + close the loop

**Files:** Modify `pipeline.rs` (`CORPUS_CTX_BUDGET`, `build_corpus_context`, `phase_translate` injection, post-run append).

- [ ] **Step 1: Failing test** — `build_corpus_context` renders top-K pairs with a tier-honesty label and respects the budget.

```rust
#[test]
fn corpus_context_labels_compile_tier() {
    use crate::corpus::{retrieve::Retriever, CorpusEntry, Tier};
    let r = Retriever::from_sources(vec![CorpusEntry{
        source_lang:"go".into(), source_api:vec!["fmt.Printf".into()], source_code:"fmt.Printf()".into(),
        rust_code:"println!()".into(), crate_name:"c".into(), file:"m.go".into(), tier:Tier::Compile,
    }], vec![]);
    let ctx = build_corpus_context("fmt.Printf(\"x\")", "go", &r);
    assert!(ctx.contains("println!"));
    assert!(ctx.to_lowercase().contains("behavior not verified")); // honesty label
}
```

- [ ] **Step 2: Implement** the budget const + builder near `build_contract_context` (pipeline.rs ~638/946).

```rust
const CORPUS_CTX_BUDGET: usize = 8_000;

/// Render retrieved verified pairs as few-shot guidance for the translate
/// prompt. Compile-tier pairs are labelled honestly: they prove the Rust
/// COMPILES, not that it behaves identically (see the divergence probe).
fn build_corpus_context(source: &str, lang: &str, retriever: &crate::corpus::retrieve::Retriever) -> String {
    use crate::corpus::{signal::api_surface, Tier};
    let q = api_surface(source);
    let pairs = retriever.top_k(&q, lang, 3);
    if pairs.is_empty() {
        return String::new();
    }
    let mut ctx = String::from(
        "Verified examples — prior translations of source with a similar API \
         surface that passed `cargo check`. Follow their structure.\n",
    );
    for p in pairs {
        let note = match p.tier {
            Tier::Behavior => "verified: compiles AND behaves identically",
            Tier::Compile => "verified: compiles (behavior not verified — match structure, not every literal)",
        };
        let block = format!(
            "\n// [{note}] {lang} → rust\n// SOURCE:\n{}\n// RUST:\n{}\n",
            p.source_code, p.rust_code
        );
        if ctx.len() + block.len() > CORPUS_CTX_BUDGET {
            break;
        }
        ctx.push_str(&block);
    }
    ctx
}
```

- [ ] **Step 3: Build the retriever once in `run()`** and inject in `phase_translate`. In `run()` (pipeline.rs ~201), load unless disabled:

```rust
let retriever = if std::env::var_os("RUSTYFI_NO_FLYWHEEL").is_some() {
    None
} else {
    let seed = crate::corpus::store::read_jsonl(std::path::Path::new("corpus/seed.jsonl"));
    let local = crate::corpus::store::local_cache_path()
        .map(|p| crate::corpus::store::read_jsonl(&p))
        .unwrap_or_default();
    (!seed.is_empty() || !local.is_empty())
        .then(|| crate::corpus::retrieve::Retriever::from_sources(seed, local))
};
```

Thread `retriever.as_ref()` into `phase_translate`; where the per-file `rust_context` is assembled (pipeline.rs ~1131-1141), append `build_corpus_context(source, lang, r)` (truncate combined to the existing budget). Fail-open: `None` → unchanged behaviour.

- [ ] **Step 4: Close the loop** — after a run reaches the oracle bar (the existing clean/verified check in `run()`), harvest the just-produced crate and append to the local cache:

```rust
if result.cargo_clean {
    if let Some(path) = crate::corpus::store::local_cache_path() {
        let pairs = crate::corpus::harvest::harvest_crate(&out_dir, &analysis.source_dir);
        let _ = crate::corpus::store::append_jsonl(&path, &pairs); // fail-open
    }
}
```

- [ ] **Step 5: Run** `cargo test -p rustyfi-engine` (full suite) + `cargo clippy -p rustyfi-engine --all-targets -- -D warnings` → PASS.
- [ ] **Step 6: Commit** `feat(corpus): inject verified pairs at translate-time + close the flywheel loop`.

## Task 8: Final verification

- [ ] **Step 1:** `cargo fmt --all` ; `cargo clippy --workspace --all-targets -- -D warnings` ; `cargo test --workspace` → all green.
- [ ] **Step 2:** `RUSTYFI_NO_FLYWHEEL=1` path still translates (fail-open sanity, covered by existing tests passing).
- [ ] **Step 3: Commit** any fmt/clippy fixups.

---

## Self-Review notes
- **Spec coverage:** harvest (T5/6), signal/ontology (T2), retrieve (T4), store seed+local (T3), translate-injection (T7), tier-honesty (T1 type + T7 label), $0 seed artifact (T6), fail-open (T3/T7), lift A/B explicitly deferred (not a task — needs a key). All covered.
- **Type consistency:** `CorpusEntry`/`Tier` fields identical across T1/T3/T4/T5/T7; `api_surface`/`jaccard`/`Retriever::from_sources`/`top_k`/`harvest_crate`/`regions`/`read_jsonl`/`append_jsonl`/`local_cache_path`/`detect_language_key`/`build_corpus_context` signatures consistent across tasks.
- **Deferred (post-v1):** item-level pairs, doctor-loop injection, coverage scoring, library pairs.
