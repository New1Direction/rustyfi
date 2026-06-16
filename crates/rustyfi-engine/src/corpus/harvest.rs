//! Mine `cargo check`-clean crates into verified (source → Rust) pairs.
use std::path::Path;
use std::process::Command;

use crate::analysis::detect_language_key;
use crate::corpus::signal::api_surface;
use crate::corpus::{CorpusEntry, Tier};

/// Parse `// <<<rustyfi:src=PATH>>> … // <<<rustyfi:end src=PATH>>>` regions out
/// of one generated Rust file into `(source_rel_path, rust_body)` pairs.
pub fn regions(rust: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut lines = rust.lines();
    while let Some(line) = lines.next() {
        let t = line.trim_start();
        if let Some(rest) = t.strip_prefix("// <<<rustyfi:src=") {
            let Some(path) = rest.strip_suffix(">>>") else {
                continue;
            };
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
/// `out_dir` = generated crate root, `src_dir` = original source root.
pub fn harvest_crate(out_dir: &Path, src_dir: &Path) -> Vec<CorpusEntry> {
    if !cargo_check_clean(out_dir) {
        return Vec::new();
    }
    let tier = behavior_tier(out_dir);
    let crate_name = out_dir
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("crate")
        .to_string();
    let mut entries = Vec::new();
    for entry in walkdir::WalkDir::new(out_dir.join("src"))
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if entry.path().extension().and_then(|s| s.to_str()) != Some("rs") {
            continue;
        }
        let Ok(rust_text) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        for (rel, rust_code) in regions(&rust_text) {
            let Ok(source_code) = std::fs::read_to_string(src_dir.join(&rel)) else {
                continue;
            };
            let Some(lang) = detect_language_key(Path::new(&rel)) else {
                continue;
            };
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
    if passed {
        Tier::Behavior
    } else {
        Tier::Compile
    }
}

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

    #[test]
    fn ignores_text_without_sentinels() {
        assert!(regions("fn main() {}\n").is_empty());
    }
}
