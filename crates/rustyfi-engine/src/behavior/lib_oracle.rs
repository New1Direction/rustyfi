//! Automated library behavioral oracle.
//!
//! Verifies **libraries** (no CLI entrypoint) — where the recipe-based oracle
//! honestly skips (`recipe::source_side` → `None`). It synthesizes a thin driver
//! that exercises the public API in BOTH the source language and Rust, runs both,
//! and diffs stdout. Proven end-to-end on `itsdangerous` (sign/unsign/tamper,
//! byte-identical to Python) — see
//! `docs/superpowers/specs/2026-06-15-library-behavioral-oracle.md`.

use std::path::Path;
use std::process::Command;

use quote::ToTokens;

use crate::llm::{extract_rust_code, LlmClient};

const SYS_SRC: &str =
    "You are an expert programmer. Output only source code — no fences, no prose.";
const SYS_RS: &str =
    "You are an expert Rust programmer. Output only Rust code — no fences, no prose.";
const CONTRACT_BUDGET: usize = 20_000;
const REPAIR_ATTEMPTS: usize = 2;

/// Outcome of a library behavioral verification.
#[derive(Debug, Clone)]
pub struct LibReport {
    pub ran: bool,
    pub matched: bool,
    pub golden: String,
    pub actual: String,
    pub skipped_reason: Option<String>,
}

impl LibReport {
    fn skip(reason: impl Into<String>) -> Self {
        Self {
            ran: false,
            matched: false,
            golden: String::new(),
            actual: String::new(),
            skipped_reason: Some(reason.into()),
        }
    }
}

// ── pure helpers (unit-tested) ────────────────────────────────────────────────

/// Strip a single leading/trailing markdown code fence, if present.
pub(crate) fn strip_fences(s: &str) -> String {
    let s = s.trim();
    let s = match s.strip_prefix("```") {
        Some(rest) => rest.split_once('\n').map_or("", |(_, after)| after),
        None => s,
    };
    s.strip_suffix("```").unwrap_or(s).trim().to_string()
}

/// Canonicalize a capture for comparison: trim each line, drop blanks, and
/// normalize language-specific reprs that are NOT behavioral differences — e.g.
/// Python `True`/`False` vs Rust `true`/`false` on a `name=value` line.
pub(crate) fn canonicalize(s: &str) -> String {
    s.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| match l.split_once('=') {
            Some((k, v)) => {
                let v = match v.trim() {
                    "True" => "true",
                    "False" => "false",
                    other => other,
                };
                format!("{}={}", k.trim(), v)
            }
            None => l.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Two stdout captures are equivalent iff they match after canonicalization.
pub(crate) fn outputs_match(golden: &str, actual: &str) -> bool {
    canonicalize(golden) == canonicalize(actual)
}

pub(crate) fn source_driver_prompt(language: &str, api: &str) -> String {
    format!(
        "Write a small {language} program that exercises this library's CORE public API with 2-4 \
         DETERMINISTIC calls and prints one labeled line per call (format `name=<value>`; print any \
         bytes as lowercase hex).\n\
         Hard requirements:\n\
         - HAPPY PATH ONLY: every call must SUCCEED. Do NOT call anything that raises/errors, and do \
           NOT rely on exceptions. (This keeps output deterministic and the program exits 0.)\n\
         - Use only pure, deterministic functions. Do NOT use any timestamp/timed/expiry/clock API \
           (anything named *Timestamp*/*Timed*/*time* — its output changes every run); no randomness, \
           network, file, or environment I/O. Prefer the plainest, most fundamental primitive.\n\
         - Use fixed literal inputs. Print exactly one line per call; never let anything crash.\n\n\
         Library API:\n{api}\n\nOutput ONLY {language} code."
    )
}

pub(crate) fn rust_driver_prompt(source_driver: &str, contract: &str) -> String {
    format!(
        "Port this driver to Rust against the crate API below. Output ONLY a Rust `fn main() {{ ... }}` \
         (no `mod` declarations, no fences, no prose). Print the EXACT same line labels, order, and \
         format as the original; print any bytes as lowercase hex (e.g. `hex::encode(&v)`). Handle \
         every `Result`/`Option` by matching and printing the matching label — NEVER `panic!` on an \
         error path (mirror the source's caught-exception label).\n\
         - Reference EVERY crate type by its full path `crate::<module>::Type` (the module is shown \
           in the API header), or add `use` lines inside `fn main`. Import std types you use \
           (e.g. `use std::sync::Arc;`).\n\
         - Use the SIMPLEST constructor form: pass `None` for ALL optional parameters; do NOT build \
           custom digest/algorithm/closure objects.\n\n\
         Crate API (already exists — implement against it, do NOT redefine):\n{contract}\n\n\
         Driver to port:\n{source_driver}"
    )
}

fn truncate(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

// ── Rust API-surface extraction (the contract handed to the porter) ───────────

fn is_pub(v: &syn::Visibility) -> bool {
    matches!(v, syn::Visibility::Public(_))
}

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
                let mut m = String::new();
                for it2 in &im.items {
                    if let syn::ImplItem::Fn(mf) = it2 {
                        m.push_str(&mf.sig.to_token_stream().to_string());
                        m.push_str(";\n");
                    }
                }
                if !m.is_empty() {
                    out.push_str(&format!("impl {ty} {{\n{m}}}\n"));
                }
            }
            Item::Mod(md) => {
                if let Some((_, c)) = &md.content {
                    collect_sigs(c, out);
                }
            }
            _ => {}
        }
    }
}

/// The crate's public type + signature surface — the contract the porter writes
/// against. Deterministic, `$0` (syn, no model).
pub fn rust_api_surface(src_root: &Path) -> String {
    let mut out = String::new();
    for e in walkdir::WalkDir::new(src_root)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if e.path().extension().and_then(|s| s.to_str()) != Some("rs") {
            continue;
        }
        if let Ok(t) = std::fs::read_to_string(e.path()) {
            if let Ok(f) = syn::parse_file(&t) {
                collect_sigs(&f.items, &mut out);
            }
        }
    }
    if out.len() > CONTRACT_BUDGET {
        let mut end = CONTRACT_BUDGET;
        while !out.is_char_boundary(end) {
            end -= 1;
        }
        out.truncate(end);
        out.push_str("\n// (contract truncated)\n");
    }
    out
}

/// The crate's top-level `mod` declarations, so a driver `main` can reach them.
fn read_mod_decls(out_dir: &Path) -> String {
    std::fs::read_to_string(out_dir.join("src/main.rs"))
        .unwrap_or_default()
        .lines()
        .filter(|l| {
            let t = l.trim_start();
            t.starts_with("pub mod ") || t.starts_with("mod ")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ── runners (I/O) ─────────────────────────────────────────────────────────────

/// Run the generated source driver, returning its stdout (the golden output).
/// Currently supports python; other languages yield an error → caller skips.
fn run_source_driver(language: &str, src_dir: &Path, driver: &str) -> Result<String, String> {
    // Absolute paths throughout: the driver runs with cwd = src root, so relative
    // arg/PYTHONPATH entries would resolve under src root twice (path doubling).
    let src_abs = std::fs::canonicalize(src_dir).map_err(|e| e.to_string())?;
    match language {
        "python" => {
            let path = src_abs.join("_rustyfi_driver.py");
            std::fs::write(&path, driver).map_err(|e| e.to_string())?;
            // PYTHONPATH heuristic: project root and a conventional `src/` layout.
            let pythonpath = format!("{}:{}", src_abs.display(), src_abs.join("src").display());
            let result = Command::new("python3")
                .arg(&path)
                .current_dir(&src_abs)
                .env("PYTHONPATH", pythonpath)
                .output()
                .map_err(|e| e.to_string());
            let _ = std::fs::remove_file(&path);
            let out = result?;
            if out.status.success() {
                Ok(String::from_utf8_lossy(&out.stdout).into_owned())
            } else {
                Err(format!(
                    "source driver failed: {}",
                    String::from_utf8_lossy(&out.stderr)
                ))
            }
        }
        other => Err(format!(
            "source-driver runner not implemented for `{other}`"
        )),
    }
}

/// Build + run the Rust driver as the crate's `main`, restoring `main.rs` after.
fn run_rust_driver(out_dir: &Path, mod_decls: &str, driver: &str) -> Result<String, String> {
    let main = out_dir.join("src/main.rs");
    let backup = std::fs::read_to_string(&main).map_err(|e| e.to_string())?;
    std::fs::write(&main, format!("{mod_decls}\n{driver}\n")).map_err(|e| e.to_string())?;
    let result = Command::new("cargo")
        .args(["run", "--quiet"])
        .current_dir(out_dir)
        .output();
    let _ = std::fs::write(&main, &backup); // ALWAYS restore the original main.rs
    let out = result.map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).into_owned())
    }
}

// ── orchestration ─────────────────────────────────────────────────────────────

/// Verify a library by synthesizing and running a driver pair. `source_api` is a
/// readable excerpt/description of the source library's public API. Fail-open:
/// any generation/run failure yields a `skip`, never a false mismatch.
pub fn verify_library(
    out_dir: &Path,
    src_dir: &Path,
    language: &str,
    source_api: &str,
    llm: &LlmClient,
) -> LibReport {
    // 1. generate + run the source driver → golden (bounded repair loop: a
    //    model-generated driver can have a runtime bug; feed the error back).
    let base_src_prompt = source_driver_prompt(language, source_api);
    let mut src_prompt = base_src_prompt.clone();
    let mut src_driver = String::new();
    let mut golden = String::new();
    for attempt in 0..=REPAIR_ATTEMPTS {
        src_driver = match llm.complete(SYS_SRC, &src_prompt) {
            Ok(r) => strip_fences(&r),
            Err(e) => return LibReport::skip(format!("source-driver generation failed: {e}")),
        };
        match run_source_driver(language, src_dir, &src_driver) {
            Ok(g) if !g.trim().is_empty() => {
                golden = g;
                break;
            }
            Ok(_) => {
                if attempt == REPAIR_ATTEMPTS {
                    return LibReport::skip("source driver produced no output");
                }
                src_prompt = format!(
                    "{base_src_prompt}\n\nYour previous program ran but printed nothing. \
                     Return a corrected program that prints one labeled line per check."
                );
            }
            Err(e) => {
                if attempt == REPAIR_ATTEMPTS {
                    return LibReport::skip(format!(
                        "source driver kept failing: {}",
                        truncate(&e, 800)
                    ));
                }
                src_prompt = format!(
                    "{base_src_prompt}\n\nYour previous program FAILED when run:\n{}\n\
                     Return a corrected, runnable program (fix the bug; keep it deterministic).",
                    truncate(&e, 1500)
                );
            }
        }
    }

    // 2. port to Rust + run, with a bounded compile-repair loop
    let mod_decls = read_mod_decls(out_dir);
    let contract = format!(
        "// crate root modules (reach items as crate::<module>::Item):\n{mod_decls}\n\n{}",
        rust_api_surface(&out_dir.join("src"))
    );
    let base_prompt = rust_driver_prompt(&src_driver, &contract);
    let mut prompt = base_prompt.clone();
    let mut actual = String::new();
    let mut ran = false;
    for attempt in 0..=REPAIR_ATTEMPTS {
        let rs = match llm.complete(SYS_RS, &prompt) {
            Ok(r) => extract_rust_code(&r),
            Err(e) => return LibReport::skip(format!("rust-driver generation failed: {e}")),
        };
        match run_rust_driver(out_dir, &mod_decls, &rs) {
            Ok(o) => {
                actual = o;
                ran = true;
                break;
            }
            Err(err) => {
                if attempt == REPAIR_ATTEMPTS {
                    actual = format!("<compile/run failed>\n{}", truncate(&err, 1500));
                    break;
                }
                prompt = format!(
                    "{base_prompt}\n\nYour previous attempt FAILED to compile/run:\n{}\n\
                     Return a corrected fn main().",
                    truncate(&err, 1500)
                );
            }
        }
    }

    let matched = ran && outputs_match(&golden, &actual);
    LibReport {
        ran,
        matched,
        golden,
        actual,
        skipped_reason: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_fences_handles_fenced_and_plain() {
        assert_eq!(strip_fences("```rust\nfn main() {}\n```"), "fn main() {}");
        assert_eq!(strip_fences("```\nx\n```"), "x");
        assert_eq!(strip_fences("  fn main() {}  "), "fn main() {}");
    }

    #[test]
    fn outputs_match_trims() {
        assert!(outputs_match("a=1\nb=2\n", "  a=1\nb=2"));
        assert!(!outputs_match("a=1", "a=2"));
    }

    #[test]
    fn outputs_match_normalizes_bool_repr() {
        // Python `True` vs Rust `true` is a repr difference, not a behavioral one.
        assert!(outputs_match("verify=True\nx=1", "verify=true\nx=1"));
        assert!(!outputs_match("verify=True", "verify=false"));
    }

    #[test]
    fn source_prompt_demands_determinism_and_names_language() {
        let p = source_driver_prompt("python", "Signer.sign(v)");
        assert!(p.contains("DETERMINISTIC"));
        assert!(p.contains("python"));
        assert!(p.contains("Signer.sign(v)"));
    }

    #[test]
    fn rust_prompt_carries_contract_and_demands_main() {
        let p = rust_driver_prompt("print(x)", "pub fn sign(&self) -> Vec<u8>;");
        assert!(p.contains("fn main()"));
        assert!(p.contains("pub fn sign"));
        assert!(p.contains("print(x)"));
    }

    #[test]
    fn api_surface_extracts_pub_items_only() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("m.rs"),
            "pub struct A { pub x: u8 }\nfn private_fn() {}\npub fn keep() -> u8 { 0 }\n",
        )
        .unwrap();
        let api = rust_api_surface(dir.path());
        assert!(api.contains("struct A"));
        assert!(api.contains("keep"));
        assert!(!api.contains("private_fn"));
    }

    #[test]
    fn read_mod_decls_picks_module_lines() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/main.rs"),
            "pub mod foo;\npub mod bar;\nfn main() {}\n",
        )
        .unwrap();
        let decls = read_mod_decls(dir.path());
        assert!(decls.contains("pub mod foo;"));
        assert!(decls.contains("pub mod bar;"));
        assert!(!decls.contains("fn main"));
    }
}
