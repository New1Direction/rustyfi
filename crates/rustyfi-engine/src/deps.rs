//! Missing-dependency auto-detection.
//!
//! LLM-translated code routinely references crates it never declared (e.g.
//! `tower_http::cors::CorsLayer`, `futures_util::StreamExt`), producing
//! `E0432`/`E0433`. This module scans the generated source for external crate
//! heads and, for any in a **curated allowlist**, adds the correct crates.io
//! crate (+ version + features) to `Cargo.toml`. Allowlist-only by design: a
//! hallucinated dependency would fail at *resolution* and hide every real
//! compile error from the fix loop — strictly worse than a visible `E0433`.

use std::collections::BTreeSet;
use std::path::Path;

/// One curated crate the translator commonly needs.
#[derive(Debug, Clone, Copy)]
pub struct CrateSpec {
    /// The import head as it appears in code (snake_case, may differ from the
    /// crate name — e.g. `tower_http` for the `tower-http` crate).
    pub head: &'static str,
    /// The crates.io crate name (hyphenated).
    pub krate: &'static str,
    pub version: &'static str,
    pub features: &'static [&'static str],
    pub default_features: bool,
}

/// Curated registry — pure-Rust, compile-clean crates only. Native/`-sys`
/// crates are deliberately excluded (they fail at *compile*, which the dep-strip
/// net can't undo); those stay the LLM's `// [DEPS]` job. Versions favour the
/// workspace's own pins where they overlap (axum 0.7 → tower-http 0.5).
pub static REGISTRY: &[CrateSpec] = &[
    spec("tower_http", "tower-http", "0.5", &["cors", "fs", "trace"]),
    spec("tower", "tower", "0.5", &["util"]),
    spec("tokio_util", "tokio-util", "0.7", &["codec", "io"]),
    spec("tokio_stream", "tokio-stream", "0.1", &[]),
    spec("futures", "futures", "0.3", &[]),
    spec("futures_util", "futures-util", "0.3", &[]),
    spec("futures_core", "futures-core", "0.3", &[]),
    spec("futures_channel", "futures-channel", "0.3", &[]),
    spec("async_stream", "async-stream", "0.3", &[]),
    spec("pin_project", "pin-project", "1", &[]),
    spec("pin_project_lite", "pin-project-lite", "0.2", &[]),
    spec("bytemuck", "bytemuck", "1", &[]),
    spec("sled", "sled", "0.34", &[]),
    spec("async_trait", "async-trait", "0.1", &[]),
    spec("bytes", "bytes", "1", &[]),
    spec("http", "http", "1", &[]),
    spec("http_body", "http-body", "1", &[]),
    spec("mime", "mime", "0.3", &[]),
    spec("chrono", "chrono", "0.4", &[]),
    spec("time", "time", "0.3", &[]),
    spec("uuid", "uuid", "1", &["v4"]),
    spec("rand", "rand", "0.8", &[]),
    spec("regex", "regex", "1", &[]),
    spec("sha2", "sha2", "0.10", &[]),
    spec("sha1", "sha1", "0.10", &[]),
    spec("md5", "md-5", "0.10", &[]),
    spec("hmac", "hmac", "0.12", &[]),
    spec("base64", "base64", "0.22", &[]),
    spec("hex", "hex", "0.4", &[]),
    spec("serde_yaml", "serde_yaml", "0.9", &[]),
    spec("toml", "toml", "0.8", &[]),
    spec("dashmap", "dashmap", "6", &[]),
    spec("parking_lot", "parking_lot", "0.12", &[]),
    spec("once_cell", "once_cell", "1", &[]),
    spec("lazy_static", "lazy_static", "1", &[]),
    spec("itertools", "itertools", "0.13", &[]),
    spec("url", "url", "2", &[]),
    spec("percent_encoding", "percent-encoding", "2", &[]),
    spec("urlencoding", "urlencoding", "2", &[]),
    spec("log", "log", "0.4", &[]),
    spec("bitflags", "bitflags", "2", &[]),
    spec("smallvec", "smallvec", "1", &[]),
    spec("indexmap", "indexmap", "2", &[]),
    spec("num_traits", "num-traits", "0.2", &[]),
    spec("num_cpus", "num_cpus", "1", &[]),
    spec("crossbeam", "crossbeam", "0.8", &[]),
    spec("crossbeam_channel", "crossbeam-channel", "0.5", &[]),
    spec("flate2", "flate2", "1", &[]),
    spec("csv", "csv", "1", &[]),
    spec("clap", "clap", "4", &["derive"]),
    spec("ndarray", "ndarray", "0.16", &[]),
];

const fn spec(
    head: &'static str,
    krate: &'static str,
    version: &'static str,
    features: &'static [&'static str],
) -> CrateSpec {
    CrateSpec {
        head,
        krate,
        version,
        features,
        default_features: true,
    }
}

/// Crate-name heads in the registry — used by `repair_module_refs` to keep them
/// from being rewritten to `crate::` (single source of truth for "external").
pub fn registry_heads() -> impl Iterator<Item = &'static str> {
    REGISTRY.iter().map(|s| s.head)
}

/// Base deps the scaffolder always writes — already satisfied.
const BASE_HEADS: &[&str] = &[
    "serde",
    "serde_json",
    "thiserror",
    "anyhow",
    "tokio",
    "reqwest",
    "tracing",
    "tracing_subscriber",
];

const STD_HEADS: &[&str] = &[
    "std",
    "core",
    "alloc",
    "crate",
    "self",
    "super",
    "proc_macro",
];

/// Detect curated crates referenced in the workspace's `src/` but not yet
/// declared in `Cargo.toml`.
pub fn detect_missing_deps(
    workspace: &Path,
    map: &crate::scaffold::PackageMap,
) -> Vec<&'static CrateSpec> {
    let src = workspace.join("src");
    let mut heads: BTreeSet<String> = BTreeSet::new();
    for entry in walkdir::WalkDir::new(&src)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if entry.path().extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(entry.path()) {
            heads.extend(scan_crate_heads(&text));
        }
    }

    // Everything already available: std, base deps, current Cargo.toml deps,
    // and project package modules.
    let cargo = std::fs::read_to_string(workspace.join("Cargo.toml")).unwrap_or_default();
    let mut satisfied: BTreeSet<String> = BTreeSet::new();
    satisfied.extend(BASE_HEADS.iter().map(|s| s.to_string()));
    satisfied.extend(STD_HEADS.iter().map(|s| s.to_string()));
    for name in declared_dep_names(&cargo) {
        // A declared `tower-http` satisfies the `tower_http` head.
        satisfied.insert(name.replace('-', "_"));
    }
    // Don't re-add a dep we just stripped this run.
    let stripped: BTreeSet<String> = cargo
        .lines()
        .filter_map(|l| l.trim().strip_prefix("# [rustyfi] removed unresolved dep:"))
        .filter_map(|r| r.trim().split(['=', ' ']).next())
        .map(|n| n.trim().replace('-', "_"))
        .collect();

    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for head in &heads {
        if satisfied.contains(head) || stripped.contains(head) || map.is_package(head) {
            continue;
        }
        if let Some(spec) = REGISTRY.iter().find(|s| s.head == head) {
            if seen.insert(spec.krate) {
                out.push(spec);
            }
        }
    }
    out
}

/// Scan a source text for curated registry crates referenced in it.
///
/// Unlike `detect_missing_deps` (which walks a workspace directory), this
/// function works on an in-memory string and ignores the base/std heads that
/// are already present in the skeleton Cargo.toml. Used by `contract_check` to
/// add registry deps to the throwaway skeleton crate so contracts that use
/// serde/tokio/etc. are checkable.
pub fn scan_crate_heads_for_registry(src: &str) -> Vec<&'static CrateSpec> {
    let heads = scan_crate_heads(src);
    let mut satisfied: BTreeSet<String> = BTreeSet::new();
    satisfied.extend(BASE_HEADS.iter().map(|s| s.to_string()));
    satisfied.extend(STD_HEADS.iter().map(|s| s.to_string()));

    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for head in &heads {
        if satisfied.contains(head) {
            continue;
        }
        if let Some(spec) = REGISTRY.iter().find(|s| s.head == *head) {
            if seen.insert(spec.krate) {
                out.push(spec);
            }
        }
    }
    out
}

/// Parse the `name` of each entry under `[dependencies]` in a Cargo.toml.
fn declared_dep_names(cargo_toml: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut in_deps = false;
    for line in cargo_toml.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_deps = t == "[dependencies]";
            continue;
        }
        if !in_deps || t.is_empty() || t.starts_with('#') {
            continue;
        }
        if let Some((name, _)) = t.split_once('=') {
            let name = name.trim().trim_matches('"');
            if !name.is_empty() {
                names.push(name.to_string());
            }
        }
    }
    names
}

/// Scan source for external crate-name heads: snake_case identifiers that begin
/// a `::` path or appear in `use X::…` / `extern crate X;`. Skips strings,
/// char literals, lifetimes and comments. PascalCase heads (types) are ignored.
fn scan_crate_heads(src: &str) -> BTreeSet<String> {
    let chars: Vec<char> = src.chars().collect();
    let n = chars.len();
    let mut out = BTreeSet::new();
    let mut i = 0;
    let mut prev: char = '\n';
    #[derive(PartialEq)]
    enum S {
        Code,
        Str,
        Ch,
        Line,
        Block,
    }
    let mut st = S::Code;

    while i < n {
        let c = chars[i];
        match st {
            S::Code => {
                if c == '"' {
                    st = S::Str;
                    i += 1;
                    continue;
                }
                if c == '/' && i + 1 < n && chars[i + 1] == '/' {
                    st = S::Line;
                    i += 2;
                    continue;
                }
                if c == '/' && i + 1 < n && chars[i + 1] == '*' {
                    st = S::Block;
                    i += 2;
                    continue;
                }
                if c == '\'' {
                    // char literal vs lifetime
                    if i + 1 < n && chars[i + 1] == '\\' {
                        st = S::Ch;
                        i += 1;
                        continue;
                    }
                    if i + 2 < n && chars[i + 2] == '\'' {
                        st = S::Ch;
                        i += 1;
                        continue;
                    }
                    prev = '\'';
                    i += 1;
                    continue; // lifetime — stay in code
                }
                if c == '_' || c.is_alphabetic() {
                    let start = i;
                    let mut j = i + 1;
                    while j < n && (chars[j] == '_' || chars[j].is_alphanumeric()) {
                        j += 1;
                    }
                    let ident: String = chars[start..j].iter().collect();
                    let followed_by_path = j + 1 < n && chars[j] == ':' && chars[j + 1] == ':';
                    let is_head = prev != ':' && prev != '.';
                    let lc_start = ident
                        .chars()
                        .next()
                        .map(|c| c.is_lowercase())
                        .unwrap_or(false);
                    if followed_by_path && is_head && lc_start {
                        out.insert(ident.clone());
                    }
                    prev = ident.chars().last().unwrap_or(c);
                    i = j;
                    continue;
                }
                if !c.is_whitespace() {
                    prev = c;
                }
                i += 1;
            }
            S::Str => {
                if c == '\\' && i + 1 < n {
                    i += 2;
                    continue;
                }
                if c == '"' {
                    st = S::Code;
                    prev = '"';
                }
                i += 1;
            }
            S::Ch => {
                if c == '\\' && i + 1 < n {
                    i += 2;
                    continue;
                }
                if c == '\'' {
                    st = S::Code;
                    prev = '\'';
                }
                i += 1;
            }
            S::Line => {
                if c == '\n' {
                    st = S::Code;
                    prev = '\n';
                }
                i += 1;
            }
            S::Block => {
                if c == '*' && i + 1 < n && chars[i + 1] == '/' {
                    st = S::Code;
                    i += 2;
                    prev = '/';
                    continue;
                }
                i += 1;
            }
        }
    }
    // `use X::...` and `extern crate X;` heads (line-based, complements the scan).
    for line in src.lines() {
        let t = line.trim_start();
        if let Some(rest) = t.strip_prefix("extern crate ") {
            if let Some(name) = rest.split([';', ' ']).next() {
                let name = name.trim();
                if !name.is_empty() {
                    out.insert(name.replace('-', "_"));
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn scan_finds_crate_heads_skips_types_and_strings() {
        let src = "use tower_http::cors::CorsLayer;\n\
                   let v = Vec::new();\n\
                   self.x::y();\n\
                   let s = \"tokio_util::nope\";\n\
                   futures_util::stream::iter(x);\n";
        let heads = scan_crate_heads(src);
        assert!(heads.contains("tower_http"), "{heads:?}");
        assert!(heads.contains("futures_util"), "{heads:?}");
        assert!(!heads.contains("Vec"), "PascalCase type leaked: {heads:?}");
        assert!(
            !heads.contains("tokio_util"),
            "string-literal head leaked: {heads:?}"
        );
        // `self.x::y` — x preceded by '.' is a field, not a head
        assert!(!heads.contains("x"), "{heads:?}");
    }

    #[test]
    fn registry_heads_unique_and_present() {
        let set: HashSet<_> = registry_heads().collect();
        assert!(set.contains("tower_http"));
        assert!(set.contains("uuid"));
        // every head resolves to a spec
        for h in registry_heads() {
            assert!(REGISTRY.iter().any(|s| s.head == h));
        }
    }
}
