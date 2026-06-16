//! $0 measurement of the deterministic verify passes, RAW (un-gated), on broken
//! crates already on disk. Reports the error-count cascade: resolver → rustfix →
//! auto-derive. Production gates each step (keeps only if errors strictly drop);
//! this shows what each pass *attempts* and its raw effect. Operates on a COPY so
//! the originals are untouched.
//!
//!   cargo run -q -p rustyfi-engine --example passes_measure -- \
//!     bench/.work/out/clifx bench/.work/out/paint bench/.work/out/cobra

use std::path::{Path, PathBuf};
use std::process::Command;

fn count_errors(dir: &Path) -> usize {
    let out = Command::new("cargo")
        .args(["check", "--message-format=json", "--quiet"])
        .current_dir(dir)
        .output()
        .expect("cargo check");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| {
            v.get("reason").and_then(|r| r.as_str()) == Some("compiler-message")
                && v.get("message")
                    .and_then(|m| m.get("level"))
                    .and_then(|l| l.as_str())
                    == Some("error")
        })
        .count()
}

fn e0277_messages(dir: &Path) -> Vec<String> {
    let out = Command::new("cargo")
        .args(["check", "--message-format=json", "--quiet"])
        .current_dir(dir)
        .output()
        .expect("cargo check");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter_map(|v| {
            let m = v.get("message")?;
            let code = m.get("code")?.get("code")?.as_str()?;
            if code == "E0277" {
                Some(m.get("message")?.as_str()?.to_string())
            } else {
                None
            }
        })
        .collect()
}

fn copy_dir(src: &Path, dst: &Path) {
    for e in walkdir::WalkDir::new(src)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let rel = e.path().strip_prefix(src).unwrap();
        if rel.iter().any(|c| c == "target" || c == ".git") {
            continue;
        }
        let t = dst.join(rel);
        if e.file_type().is_dir() {
            std::fs::create_dir_all(&t).ok();
        } else if e.file_type().is_file() {
            if let Some(p) = t.parent() {
                std::fs::create_dir_all(p).ok();
            }
            std::fs::copy(e.path(), &t).ok();
        }
    }
}

fn main() {
    let crates: Vec<String> = std::env::args().skip(1).collect();
    println!(
        "{:<10} {:>7} {:>14} {:>14} {:>14}",
        "crate", "before", "after-resolve", "after-rustfix", "after-derive"
    );
    println!("{:-<64}", "");
    for c in &crates {
        let src = PathBuf::from(c);
        let name = src.file_name().unwrap().to_string_lossy().into_owned();
        let ws = std::env::temp_dir().join(format!("rustyfi-pm-{name}"));
        let _ = std::fs::remove_dir_all(&ws);
        copy_dir(&src, &ws);

        let before = count_errors(&ws);

        let r = rustyfi_engine::resolve_imports::resolve_crate_imports(&ws);
        let after_resolve = count_errors(&ws);

        let f = rustyfi_engine::rustfix::apply_machine_suggestions(&ws, 6);
        let after_rustfix = count_errors(&ws);

        let e0277 = e0277_messages(&ws);
        let want = rustyfi_engine::auto_derive::needed_derives(&e0277);
        let d = rustyfi_engine::auto_derive::add_missing_derives(&ws, &want);
        let after_derive = count_errors(&ws);

        println!(
            "{:<10} {:>7} {:>14} {:>14} {:>14}",
            name, before, after_resolve, after_rustfix, after_derive
        );
        println!(
            "           (resolve: {} files; rustfix: {} fixes/{} passes; derive: {} added)",
            r.files_changed, f.applied, f.passes, d.derives_added
        );
        let _ = std::fs::remove_dir_all(&ws);
    }
    println!("{:-<64}", "");
    println!("NOTE: raw/un-gated. Production keeps a step only if it strictly lowers errors.");
}
