use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use tracing::{info, warn};
use zip::write::FileOptions;

use crate::EngineError;

// ---------------------------------------------------------------------------
// Package map — directory-as-namespace module layout
// ---------------------------------------------------------------------------
//
// Languages like Go/Python/JS treat a DIRECTORY as the namespace: every file in
// `internal/storage/` is package `storage`, they reference each other's symbols
// unqualified, and outsiders write `storage.Store`. Rust makes the FILE a
// module, which breaks that. The fix: map each source directory to ONE Rust
// module (named after the dir = the package) by CONCATENATING all its files
// into `src/<pkg>/mod.rs`. Same-package unqualified refs then resolve with zero
// machinery — exactly like Go. A deterministic rewrite pass (`repair_module_refs`)
// turns the LLM's `storage::Store` into `crate::storage::Store`.

/// Where one source file's translation lands in the generated crate.
#[derive(Debug, Clone)]
pub struct CanonicalModule {
    /// Source-facing package name the LLM uses (e.g. `storage`).
    pub package: String,
    /// Crate-root module name (usually == package; disambiguated on collision).
    pub root_segment: String,
    /// True if this file belongs to the entrypoint package (lands in main.rs).
    pub is_entrypoint: bool,
}

/// Deterministic mapping from source files to Rust modules for one project.
#[derive(Debug, Clone, Default)]
pub struct PackageMap {
    /// Keyed by source-relative path (relative to the source root).
    pub by_file: HashMap<PathBuf, CanonicalModule>,
    /// Source package name → the crate-root segment(s) it maps to. More than
    /// one entry means a name collision (two dirs with the same basename).
    pub root_of: HashMap<String, Vec<String>>,
    /// Unique non-entrypoint root segments, in first-seen order — for write_main.
    pub roots: Vec<String>,
}

impl PackageMap {
    /// The canonical module for a source-relative path, if known.
    pub fn get(&self, rel: &Path) -> Option<&CanonicalModule> {
        self.by_file.get(rel)
    }

    /// True if `name` is a project package (vs an external crate / std).
    pub fn is_package(&self, name: &str) -> bool {
        self.root_of.contains_key(name)
    }

    /// The single root segment for a package name, or None if unknown/ambiguous.
    pub fn unique_root(&self, name: &str) -> Option<&str> {
        match self.root_of.get(name) {
            Some(v) if v.len() == 1 => Some(v[0].as_str()),
            _ => None,
        }
    }
}

/// Languages where a directory is the namespace unit (package == directory).
pub fn dir_namespaced(language: &str) -> bool {
    matches!(
        language,
        "go" | "python" | "javascript" | "typescript" | "java" | "ruby"
    )
}

/// Sanitise an arbitrary string into a valid lowercase Rust identifier.
fn sanitise_ident(s: &str) -> String {
    let safe: String = s
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if safe.is_empty() {
        "module".to_string()
    } else if safe.starts_with(|c: char| c.is_ascii_digit()) {
        format!("m_{safe}")
    } else {
        safe
    }
}

/// The package name + collision-identity for a source-relative file.
fn package_identity(rel: &Path, dir_namespaced: bool) -> (String, PathBuf) {
    if dir_namespaced {
        match rel.parent() {
            Some(p) if !p.as_os_str().is_empty() => {
                let base = p
                    .file_name()
                    .map(|s| sanitise_ident(&s.to_string_lossy()))
                    .unwrap_or_else(|| "module".into());
                return (base, p.to_path_buf());
            }
            // File at the source root → its own package (named by stem).
            _ => {}
        }
    }
    // File-as-module (C/C++/C#/Ruby, or a root-level file): stem, with the
    // `_h` header suffix preserved so foo.c and foo.h don't collide.
    let ext = rel
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let mut stem = sanitise_ident(
        &rel.file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "module".into()),
    );
    if matches!(ext.as_str(), "h" | "hpp" | "hh" | "hxx") {
        stem.push_str("_h");
    }
    (stem, rel.to_path_buf())
}

/// Build the package map from the project's source-relative file list.
pub fn build_package_map(
    target_rel: &[PathBuf],
    entrypoints_rel: &HashSet<PathBuf>,
    is_dir_namespaced: bool,
) -> PackageMap {
    // Directories that contain an entrypoint file → the whole package is the
    // entrypoint (all its files concatenate into main.rs).
    let entry_identities: HashSet<PathBuf> = entrypoints_rel
        .iter()
        .map(|rel| package_identity(rel, is_dir_namespaced).1)
        .collect();

    // Assign a unique crate-root segment per identity (dir or file).
    let mut root_for_identity: HashMap<PathBuf, String> = HashMap::new();
    let mut used_roots: HashSet<String> = HashSet::new();

    let mut map = PackageMap::default();

    for rel in target_rel {
        let (pkg_name, identity) = package_identity(rel, is_dir_namespaced);
        let is_entrypoint = entry_identities.contains(&identity);

        let root_segment = root_for_identity
            .entry(identity.clone())
            .or_insert_with(|| {
                let seg = disambiguate_root(&pkg_name, &identity, &used_roots);
                used_roots.insert(seg.clone());
                seg
            })
            .clone();

        map.by_file.insert(
            rel.clone(),
            CanonicalModule {
                package: pkg_name.clone(),
                root_segment: root_segment.clone(),
                is_entrypoint,
            },
        );

        let roots = map.root_of.entry(pkg_name).or_default();
        if !roots.contains(&root_segment) {
            roots.push(root_segment.clone());
        }

        if !is_entrypoint && !map.roots.contains(&root_segment) {
            map.roots.push(root_segment.clone());
        }
    }
    map
}

/// Pick a crate-root segment for `pkg_name`; if already taken by another
/// identity, prefix parent path segments until unique.
fn disambiguate_root(pkg_name: &str, identity: &Path, used: &HashSet<String>) -> String {
    if !used.contains(pkg_name) {
        return pkg_name.to_string();
    }
    // Walk up the identity path, prefixing segments: a/b/util → b_util → a_b_util.
    let segs: Vec<String> = identity
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => Some(sanitise_ident(&s.to_string_lossy())),
            _ => None,
        })
        .collect();
    for take in 2..=segs.len() {
        let candidate = segs[segs.len() - take..].join("_");
        if !used.contains(&candidate) {
            return candidate;
        }
    }
    // Last resort: append a counter.
    let mut n = 2;
    loop {
        let candidate = format!("{pkg_name}_{n}");
        if !used.contains(&candidate) {
            return candidate;
        }
        n += 1;
    }
}

/// Destination of a file's translation, relative to `src/`.
fn package_dest(rel: &Path, map: &PackageMap) -> PathBuf {
    match map.get(rel) {
        Some(m) if m.is_entrypoint => PathBuf::from("main.rs"),
        Some(m) => Path::new(&m.root_segment).join("mod.rs"),
        // Unknown file → fall back to the old flattened single-file module.
        None => rust_module_name(rel),
    }
}

/// Path heads that must never be rewritten to `crate::` — std, the base deps,
/// and every crate in the curated dependency registry (single source of truth,
/// so `repair_module_refs` and the missing-dep detector never disagree).
fn repair_allowlist() -> &'static std::collections::HashSet<&'static str> {
    static ALLOW: std::sync::OnceLock<std::collections::HashSet<&'static str>> =
        std::sync::OnceLock::new();
    ALLOW.get_or_init(|| {
        let mut s: std::collections::HashSet<&'static str> = [
            "std",
            "core",
            "alloc",
            "crate",
            "self",
            "super",
            "proc_macro",
            "Self",
            "serde",
            "serde_json",
            "serde_derive",
            "thiserror",
            "anyhow",
            "tokio",
            "reqwest",
            "tracing",
            "tracing_subscriber",
            "axum",
            "sqlx",
            "redis",
            "validator",
            "octocrab",
            "libc",
            "nix",
            "sled",
        ]
        .into_iter()
        .collect();
        s.extend(crate::deps::registry_heads());
        s
    })
}

#[derive(Clone, Copy, PartialEq)]
enum Scan {
    Code,
    Str,
    Char,
    Line,
    Block,
}

fn is_ident_start(c: char) -> bool {
    c == '_' || c.is_alphabetic()
}
fn is_ident_continue(c: char) -> bool {
    c == '_' || c.is_alphanumeric()
}

/// Deterministically repair module references in one translated file so it
/// compiles against the directory-as-package layout:
///   - rewrite `pkg::` heads (where `pkg` is a known project package) to
///     `crate::<root_segment>::` — fully-qualified, so no `use` is needed;
///   - skip heads inside strings, char literals and comments (and lifetimes);
///   - strip `mod`/`pub mod` declarations the LLM emitted for project packages
///     (the build system owns module wiring);
///   - promote column-0 item definitions to `pub` so cross-module
///     `crate::<pkg>::Item` references can see them.
///
/// `this_pkg` is the package the file belongs to (its directory basename / root
/// segment); a `pkg::` head equal to it is NOT rewritten — inside package
/// `middleware`, a `middleware::from_fn` means the EXTERNAL module (e.g. axum's),
/// never the package referring to itself.
pub fn repair_module_refs(rust_code: &str, this_pkg: &str, map: &PackageMap) -> String {
    let rewritten = rewrite_heads(rust_code, this_pkg, map);

    let mut out: Vec<String> = Vec::new();
    let mut in_block = false;
    for line in rewritten.lines() {
        // Track (approximate) block-comment spans so we don't touch their lines.
        let had_block = in_block;
        update_block_comment_state(line, &mut in_block);
        if had_block {
            out.push(line.to_string());
            continue;
        }

        let trimmed = line.trim_start();
        // Strip `mod x;` / `pub mod x;` for project packages — wiring is ours.
        if let Some(name) = parse_mod_decl(trimmed) {
            if map.is_package(&name) {
                continue;
            }
        }
        out.push(auto_pub_line(line));
    }
    out.join("\n")
}

/// Rewrite `pkg::` path heads to `crate::<root>::`, skipping strings/comments,
/// `use`-group sub-paths, and the file's own package name.
fn rewrite_heads(code: &str, this_pkg: &str, map: &PackageMap) -> String {
    let chars: Vec<char> = code.chars().collect();
    let n = chars.len();
    let mut out = String::with_capacity(code.len() + 64);
    let mut i = 0;
    let mut state = Scan::Code;
    let mut prev: char = '\n'; // last emitted non-space code char (head detection)
                               // Track `use … { … }` so we never inject `crate::` into a use-group leaf
                               // (`use a::{b, pkg::c}` → `crate::` there is an E0433 "not in start position").
    let mut in_use = false;
    let mut use_brace_depth: i32 = 0;

    while i < n {
        let c = chars[i];
        match state {
            Scan::Code => {
                // String / comment entries.
                if c == '"' {
                    out.push(c);
                    state = Scan::Str;
                    i += 1;
                    continue;
                }
                if c == '/' && i + 1 < n && chars[i + 1] == '/' {
                    out.push(c);
                    state = Scan::Line;
                    i += 1;
                    continue;
                }
                if c == '/' && i + 1 < n && chars[i + 1] == '*' {
                    out.push(c);
                    state = Scan::Block;
                    i += 1;
                    continue;
                }
                if c == '\'' {
                    // Distinguish a char literal ('x', '\n') from a lifetime ('a).
                    let (emit, next_i, next_state) = handle_quote(&chars, i);
                    out.push_str(&emit);
                    i = next_i;
                    state = next_state;
                    if !emit.is_empty() {
                        prev = emit.chars().last().unwrap();
                    }
                    continue;
                }
                if is_ident_start(c) {
                    let start = i;
                    let mut j = i + 1;
                    while j < n && is_ident_continue(chars[j]) {
                        j += 1;
                    }
                    let ident: String = chars[start..j].iter().collect();
                    if ident == "use" && (prev == '\n' || prev == ';' || prev == '{' || prev == '}')
                    {
                        in_use = true;
                        use_brace_depth = 0;
                    }
                    let followed_by_path = j + 1 < n && chars[j] == ':' && chars[j + 1] == ':';
                    let is_head = prev != ':' && prev != '.';
                    // Inside `use a::{ … }` the segment is relative to the prefix,
                    // not the crate root — and a package can't qualify itself.
                    let in_use_group = in_use && use_brace_depth > 0;
                    if followed_by_path
                        && is_head
                        && !in_use_group
                        && ident != this_pkg
                        && !repair_allowlist().contains(ident.as_str())
                    {
                        if let Some(root) = map.unique_root(&ident) {
                            out.push_str("crate::");
                            out.push_str(root);
                            prev = root.chars().last().unwrap_or('a');
                            i = j; // resume at the `::`
                            continue;
                        }
                    }
                    out.push_str(&ident);
                    prev = ident.chars().last().unwrap_or(c);
                    i = j;
                    continue;
                }
                if in_use {
                    match c {
                        '{' => use_brace_depth += 1,
                        '}' => use_brace_depth -= 1,
                        ';' => {
                            in_use = false;
                            use_brace_depth = 0;
                        }
                        _ => {}
                    }
                }
                out.push(c);
                if !c.is_whitespace() {
                    prev = c;
                }
                i += 1;
            }
            Scan::Str => {
                out.push(c);
                if c == '\\' && i + 1 < n {
                    out.push(chars[i + 1]);
                    i += 2;
                    continue;
                }
                if c == '"' {
                    state = Scan::Code;
                    prev = '"';
                }
                i += 1;
            }
            Scan::Char => {
                out.push(c);
                if c == '\\' && i + 1 < n {
                    out.push(chars[i + 1]);
                    i += 2;
                    continue;
                }
                if c == '\'' {
                    state = Scan::Code;
                    prev = '\'';
                }
                i += 1;
            }
            Scan::Line => {
                out.push(c);
                if c == '\n' {
                    state = Scan::Code;
                    prev = '\n';
                }
                i += 1;
            }
            Scan::Block => {
                out.push(c);
                if c == '*' && i + 1 < n && chars[i + 1] == '/' {
                    out.push('/');
                    i += 2;
                    state = Scan::Code;
                    prev = '/';
                    continue;
                }
                i += 1;
            }
        }
    }
    out
}

/// Handle a `'` in code: returns (text_to_emit, next_index, next_state).
/// A char literal stays consumed here; a lifetime emits just the `'` and
/// returns to Code so the following identifier is scanned normally.
fn handle_quote(chars: &[char], i: usize) -> (String, usize, Scan) {
    let n = chars.len();
    // Escape → definitely a char literal: '\n', '\'', '\\'.
    if i + 1 < n && chars[i + 1] == '\\' {
        return ("'".to_string(), i + 1, Scan::Char);
    }
    // 'x' form → char literal (ident-like char followed by closing quote).
    if i + 2 < n && chars[i + 2] == '\'' {
        return ("'".to_string(), i + 1, Scan::Char);
    }
    // 'name (no near closing quote) → lifetime: emit the quote, resume Code.
    ("'".to_string(), i + 1, Scan::Code)
}

/// Update `in_block` for an approximate per-line block-comment tracker.
fn update_block_comment_state(line: &str, in_block: &mut bool) {
    let bytes: Vec<char> = line.chars().collect();
    let mut k = 0;
    while k < bytes.len() {
        if *in_block {
            if bytes[k] == '*' && k + 1 < bytes.len() && bytes[k + 1] == '/' {
                *in_block = false;
                k += 2;
                continue;
            }
        } else if bytes[k] == '/' && k + 1 < bytes.len() && bytes[k + 1] == '*' {
            *in_block = true;
            k += 2;
            continue;
        } else if bytes[k] == '/' && k + 1 < bytes.len() && bytes[k + 1] == '/' {
            break; // line comment — rest is irrelevant to block state
        }
        k += 1;
    }
}

/// If `trimmed` is a `mod x;` / `pub mod x;` declaration, return `x`.
fn parse_mod_decl(trimmed: &str) -> Option<String> {
    let rest = trimmed
        .strip_prefix("pub mod ")
        .or_else(|| trimmed.strip_prefix("mod "))?;
    let name: String = rest.chars().take_while(|c| is_ident_continue(*c)).collect();
    // Only a bare declaration `mod x;` — not `mod x { ... }` inline modules.
    let after = rest[name.len()..].trim_start();
    if after.starts_with(';') && !name.is_empty() {
        Some(name)
    } else {
        None
    }
}

/// Normalize a module's content: drop exact-duplicate `use` lines, then run the
/// syn-based pass that removes duplicate top-level items/impls and redundant
/// use-trees (E0428/E0252). Returns input unchanged if it doesn't parse. Used
/// both when concatenating translations AND on every fix-loop rewrite — an
/// aggressive model that re-emits a duplicate definition gets cleaned up.
pub fn normalize_module_content(content: &str) -> String {
    crate::dedup_items::dedup_top_level_items(&dedup_module_uses(content))
}

/// Remove duplicate top-level `use …;` lines from a concatenated module.
/// Each source file in a package brings its own imports; merging them produces
/// `use serde::{Serialize, Deserialize};` (etc.) twice → E0252. Keeps the first
/// occurrence of each exact single-line `use`; only column-0 lines count, so
/// imports inside functions are never touched.
fn dedup_module_uses(content: &str) -> String {
    let mut seen: HashSet<&str> = HashSet::new();
    let mut out: Vec<&str> = Vec::with_capacity(content.lines().count());
    for line in content.lines() {
        let is_top_use = !line.starts_with(char::is_whitespace)
            && line.starts_with("use ")
            && line.trim_end().ends_with(';');
        if is_top_use && !seen.insert(line.trim_end()) {
            continue; // exact duplicate import → drop
        }
        out.push(line);
    }
    out.join("\n")
}

/// Promote a column-0 item definition to `pub` so cross-module references
/// (`crate::<pkg>::Item`) can see it. Leaves already-`pub`, indented (in-fn),
/// and non-item lines untouched.
fn auto_pub_line(line: &str) -> String {
    // Only column-0 definitions (no leading whitespace).
    if line.starts_with(char::is_whitespace) || line.is_empty() {
        return line.to_string();
    }
    const ITEM_KW: &[&str] = &[
        "fn ", "struct ", "enum ", "trait ", "type ", "const ", "static ", "union ",
    ];
    for kw in ITEM_KW {
        if line.starts_with(kw) {
            return format!("pub {line}");
        }
    }
    // `async fn` / `unsafe fn` at column 0.
    if line.starts_with("async fn ") || line.starts_with("unsafe fn ") {
        return format!("pub {line}");
    }
    line.to_string()
}

// ---------------------------------------------------------------------------
// Cargo project scaffolder
// ---------------------------------------------------------------------------

/// Writes a bare-bones Cargo workspace / binary crate to `workspace_path`.
pub struct Scaffolder {
    pub workspace_path: PathBuf,
    pub crate_name: String,
}

impl Scaffolder {
    pub fn new(workspace_path: PathBuf, crate_name: String) -> Self {
        Self {
            workspace_path,
            crate_name,
        }
    }

    /// Create the directory skeleton.
    pub fn scaffold(&self) -> Result<(), EngineError> {
        let src = self.workspace_path.join("src");
        fs::create_dir_all(&src).map_err(|e| EngineError::Io(e.to_string()))?;

        // Minimal Cargo.toml
        let cargo_toml = format!(
            r#"[package]
name = "{name}"
version = "0.1.0"
edition = "2021"

[dependencies]
serde       = {{ version = "1", features = ["derive"] }}
serde_json  = "1"
thiserror   = "1"
anyhow      = "1"
tokio       = {{ version = "1", features = ["full"] }}
reqwest     = {{ version = "0.12", features = ["json"] }}
tracing     = "0.1"
tracing-subscriber = {{ version = "0.3", features = ["env-filter"] }}

# empty table: keep this crate out of any enclosing workspace
[workspace]
"#,
            name = self.crate_name
        );
        fs::write(self.workspace_path.join("Cargo.toml"), cargo_toml)
            .map_err(|e| EngineError::Io(e.to_string()))?;

        // Placeholder main so the project compiles before translation.
        let placeholder = "fn main() { println!(\"Rustyfi: translation in progress\"); }\n";
        fs::write(src.join("main.rs"), placeholder).map_err(|e| EngineError::Io(e.to_string()))?;

        info!("Scaffolded crate at {}", self.workspace_path.display());
        Ok(())
    }

    /// Write one file's translation into its package module.
    ///
    /// Files that share a source directory (= the same package) are
    /// CONCATENATED into one `src/<pkg>/mod.rs`, each delimited by sentinels so
    /// a resumed or fix-loop re-write replaces just that file's contribution
    /// instead of duplicating it. The entrypoint package lands in `src/main.rs`.
    pub fn write_module(
        &self,
        rel_path: &Path,
        rust_code: &str,
        extra_deps: &HashMap<String, String>,
        map: &PackageMap,
    ) -> Result<PathBuf, EngineError> {
        let dest = self
            .workspace_path
            .join("src")
            .join(package_dest(rel_path, map));

        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(|e| EngineError::Io(e.to_string()))?;
        }

        let rel_disp = rel_path.to_string_lossy();
        let start = format!("// <<<rustyfi:src={rel_disp}>>>");
        let end = format!("// <<<rustyfi:end src={rel_disp}>>>");
        let contribution = format!("{start}\n{rust_code}\n{end}\n");

        let existing = fs::read_to_string(&dest).unwrap_or_default();
        let is_placeholder = existing.contains("Rustyfi: translation in progress");

        let merged = if existing.trim().is_empty() || is_placeholder {
            // Fresh file, or the scaffold's placeholder main.rs → replace it.
            contribution
        } else if let (Some(s), Some(e)) = (existing.find(&start), existing.find(&end)) {
            // This file already contributed → replace its region (idempotent).
            let e_end = e + end.len();
            format!(
                "{}{}{}",
                &existing[..s],
                contribution.trim_end_matches('\n'),
                &existing[e_end..]
            )
        } else {
            // A different file already owns this module → append.
            format!("{}\n{contribution}", existing.trim_end())
        };

        let new_content = normalize_module_content(&merged);

        fs::write(&dest, new_content).map_err(|e| EngineError::Io(e.to_string()))?;

        if !extra_deps.is_empty() {
            self.merge_deps(extra_deps)?;
        }
        Ok(dest)
    }

    /// Declare every non-entrypoint package module in `main.rs`.
    ///
    /// If a translated entrypoint already lives at `src/main.rs`, its body is
    /// preserved and the `pub mod` declarations are prepended — overwriting it
    /// would ship a crate whose main does nothing. Idempotent on resume.
    pub fn write_main(&self, map: &PackageMap) -> Result<(), EngineError> {
        let path = self.workspace_path.join("src").join("main.rs");
        let raw = fs::read_to_string(&path).unwrap_or_default();
        // `pub mod cache;` collides with `use crate::cache::{self, …}` / bare
        // `use crate::cache;` (both bind `cache`) → E0255. Strip those imports;
        // the module is already in scope from the declaration.
        let existing = strip_module_self_imports(&raw, &map.roots);

        let mut decls = String::new();
        for seg in &map.roots {
            if existing.contains(&format!("pub mod {seg};")) {
                continue;
            }
            decls.push_str(&format!("pub mod {seg};\n"));
        }

        let is_translated_main =
            existing.contains("fn main") && !existing.contains("Rustyfi: translation in progress");

        let src = if is_translated_main {
            if decls.is_empty() && existing == raw {
                return Ok(()); // nothing to add or strip
            }
            format!("{decls}\n{existing}")
        } else {
            format!("{decls}\nfn main() {{\n    // Generated by Rustyfi\n    println!(\"Rustyfi translation complete\");\n}}\n")
        };
        fs::write(&path, src).map_err(|e| EngineError::Io(e.to_string()))?;
        Ok(())
    }

    /// Append extra dependencies to the project's Cargo.toml.
    fn merge_deps(&self, extra: &HashMap<String, String>) -> Result<(), EngineError> {
        let path = self.workspace_path.join("Cargo.toml");
        let mut content = fs::read_to_string(&path).map_err(|e| EngineError::Io(e.to_string()))?;

        for (krate, ver) in extra {
            // Skip if already present (match on the `name =` key, not a loose
            // substring — otherwise `time` would spuriously match `runtime`).
            if content.contains(&format!("\n{krate} ")) || content.contains(&format!("\n{krate}="))
            {
                continue;
            }
            warn!("Adding dep from LLM hint: {krate} = \"{ver}\"");
            content.push_str(&format!("{krate} = \"{ver}\"\n"));
        }

        fs::write(&path, content).map_err(|e| EngineError::Io(e.to_string()))?;
        Ok(())
    }

    /// Write a package's authoritative contract (its canonical type/enum/trait
    /// definitions) into `src/<root_segment>/mod.rs`, ahead of any translated
    /// bodies, under a dedicated sentinel. Idempotent (resume re-writes the same
    /// region). Bodies appended later by `write_module` coexist; the dedup pass
    /// collapses any type a body re-emits in favour of this canonical copy.
    pub fn write_package_contract(
        &self,
        root_segment: &str,
        data_surface: &str,
    ) -> Result<(), EngineError> {
        if data_surface.trim().is_empty() {
            return Ok(());
        }
        let dest = self
            .workspace_path
            .join("src")
            .join(root_segment)
            .join("mod.rs");
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(|e| EngineError::Io(e.to_string()))?;
        }
        const START: &str = "// <<<rustyfi:contract>>>";
        const END: &str = "// <<<rustyfi:end contract>>>";
        let block = format!("{START}\n{data_surface}\n{END}\n");
        let existing = fs::read_to_string(&dest).unwrap_or_default();

        let new_content = if let (Some(s), Some(e)) = (existing.find(START), existing.find(END)) {
            let e_end = e + END.len();
            format!(
                "{}{}{}",
                &existing[..s],
                block.trim_end_matches('\n'),
                &existing[e_end..]
            )
        } else if existing.trim().is_empty() {
            block
        } else {
            format!("{block}\n{existing}")
        };
        fs::write(&dest, new_content).map_err(|e| EngineError::Io(e.to_string()))?;
        Ok(())
    }

    /// Add curated registry dependencies (with feature tables) to Cargo.toml.
    /// Presence-gated, so re-running adds nothing. Tagged `# [rustyfi] auto-dep`.
    pub fn add_registry_deps(&self, specs: &[&crate::deps::CrateSpec]) -> Result<(), EngineError> {
        if specs.is_empty() {
            return Ok(());
        }
        let path = self.workspace_path.join("Cargo.toml");
        let mut content = fs::read_to_string(&path).map_err(|e| EngineError::Io(e.to_string()))?;
        for spec in specs {
            if content.contains(&format!("\n{} ", spec.krate))
                || content.contains(&format!("\n{}=", spec.krate))
            {
                continue;
            }
            warn!(
                "Adding missing dep from registry: {} = \"{}\"",
                spec.krate, spec.version
            );
            content.push_str(&render_dep_line(spec));
            content.push('\n');
        }
        fs::write(&path, content).map_err(|e| EngineError::Io(e.to_string()))?;
        Ok(())
    }
}

/// Remove imports that collide with the crate-root `pub mod <root>;`
/// declarations: bare `use crate::<root>;` and the `self` leaf of
/// `use crate::<root>::{self, …}` (both re-bind the module name → E0255).
fn strip_module_self_imports(body: &str, roots: &[String]) -> String {
    let mut out: Vec<String> = Vec::with_capacity(body.lines().count());
    'lines: for line in body.lines() {
        let t = line.trim();
        for r in roots {
            // Bare module re-import → drop entirely.
            if t == format!("use crate::{r};") || t == format!("pub use crate::{r};") {
                continue 'lines;
            }
            // `use crate::<r>::{ self, … }` → drop the `self` leaf.
            if t.starts_with(&format!("use crate::{r}::{{"))
                || t.starts_with(&format!("pub use crate::{r}::{{"))
            {
                if let Some(rewritten) = remove_self_leaf(line) {
                    out.push(rewritten);
                }
                // else: the group was only `{self}` → drop the line entirely.
                continue 'lines;
            }
        }
        out.push(line.to_string());
    }
    out.join("\n")
}

/// Drop the `self` leaf from a `use …::{ … }` group, preserving indentation.
/// Returns None if `self` was the only leaf (caller drops the line).
fn remove_self_leaf(line: &str) -> Option<String> {
    let open = line.find('{')?;
    let close = line.rfind('}')?;
    let kept: Vec<&str> = line[open + 1..close]
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty() && *s != "self")
        .collect();
    if kept.is_empty() {
        return None;
    }
    Some(format!(
        "{}{{{}}}{}",
        &line[..open],
        kept.join(", "),
        &line[close + 1..]
    ))
}

/// Render a registry crate as a Cargo.toml dependency line.
fn render_dep_line(spec: &crate::deps::CrateSpec) -> String {
    if spec.features.is_empty() && spec.default_features {
        return format!("{} = \"{}\" # [rustyfi] auto-dep", spec.krate, spec.version);
    }
    let mut parts = vec![format!("version = \"{}\"", spec.version)];
    if !spec.features.is_empty() {
        let f = spec
            .features
            .iter()
            .map(|x| format!("\"{x}\""))
            .collect::<Vec<_>>()
            .join(", ");
        parts.push(format!("features = [{f}]"));
    }
    if !spec.default_features {
        parts.push("default-features = false".into());
    }
    format!(
        "{} = {{ {} }} # [rustyfi] auto-dep",
        spec.krate,
        parts.join(", ")
    )
}

// ---------------------------------------------------------------------------
// Module name mapper
// ---------------------------------------------------------------------------

fn rust_module_name(source_path: &Path) -> PathBuf {
    // Collect all Normal path components into sanitised segments.
    let mut segments: Vec<String> = source_path
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => Some(s.to_string_lossy().to_string()),
            _ => None,
        })
        .collect();

    // Replace the last segment (filename) with its stem (no extension).
    // C/C++ headers keep an `_h` suffix so `main.c` + `main.h` don't both
    // map to `main.rs` and clobber each other.
    if let Some(last) = segments.last_mut() {
        let p = Path::new(last.as_str());
        let ext = p
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        let mut stem = p
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "module".to_string());
        if matches!(ext.as_str(), "h" | "hpp" | "hh" | "hxx") {
            stem.push_str("_h");
        }
        *last = stem;
    }

    // Join all segments with underscore and sanitise to a valid Rust identifier.
    let joined = segments.join("_").to_lowercase();
    let safe: String = joined
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();

    let safe = if safe.starts_with(|c: char| c.is_ascii_digit()) {
        format!("m_{safe}")
    } else if safe.is_empty() {
        "module".to_string()
    } else {
        safe
    };

    PathBuf::from(format!("{safe}.rs"))
}

// ---------------------------------------------------------------------------
// ZIP packager
// ---------------------------------------------------------------------------

/// Zip up the entire workspace directory into a single bytes buffer.
pub fn package_to_zip(workspace_path: &Path) -> Result<Vec<u8>, EngineError> {
    use std::io::Write;
    use zip::CompressionMethod;

    let buf = Vec::new();
    let cursor = std::io::Cursor::new(buf);
    let mut zip = zip::ZipWriter::new(cursor);

    let file_opts: FileOptions<()> = FileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .unix_permissions(0o644);

    let dir_opts: FileOptions<()> = FileOptions::default()
        .compression_method(CompressionMethod::Stored)
        .unix_permissions(0o755); // dirs need execute bit or macOS blocks access

    let workspace_root = workspace_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "rustyfi_output".to_string());

    for entry in walkdir::WalkDir::new(workspace_path)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        let rel = path
            .strip_prefix(workspace_path)
            .unwrap_or(path)
            .to_string_lossy();
        let zip_path = format!("{workspace_root}/{rel}");

        if path.is_dir() {
            zip.add_directory(&zip_path, dir_opts)
                .map_err(|e| EngineError::Io(e.to_string()))?;
        } else {
            if zip_path.contains("/target/") {
                continue;
            }
            zip.start_file(&zip_path, file_opts)
                .map_err(|e| EngineError::Io(e.to_string()))?;
            let data = fs::read(path).map_err(|e| EngineError::Io(e.to_string()))?;
            zip.write_all(&data)
                .map_err(|e| EngineError::Io(e.to_string()))?;
        }
    }

    let cursor = zip.finish().map_err(|e| EngineError::Io(e.to_string()))?;
    Ok(cursor.into_inner())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    fn go_map() -> PackageMap {
        let targets = vec![
            p("internal/storage/storage.go"),
            p("internal/storage/badger.go"),
            p("internal/config/config.go"),
            p("cmd/api/main.go"),
        ];
        let entries: HashSet<PathBuf> = [p("cmd/api/main.go")].into_iter().collect();
        build_package_map(&targets, &entries, true)
    }

    #[test]
    fn groups_directory_into_one_package() {
        let m = go_map();
        // both storage files map to package `storage`, same root
        let a = m.get(&p("internal/storage/storage.go")).unwrap();
        let b = m.get(&p("internal/storage/badger.go")).unwrap();
        assert_eq!(a.package, "storage");
        assert_eq!(b.package, "storage");
        assert_eq!(a.root_segment, b.root_segment);
        assert_eq!(a.root_segment, "storage");
        assert!(!a.is_entrypoint);
    }

    #[test]
    fn entrypoint_package_is_flagged_and_excluded_from_roots() {
        let m = go_map();
        let main = m.get(&p("cmd/api/main.go")).unwrap();
        assert!(main.is_entrypoint);
        assert!(!m.roots.contains(&"api".to_string()));
        // non-entrypoint packages are declared
        assert!(m.roots.contains(&"storage".to_string()));
        assert!(m.roots.contains(&"config".to_string()));
    }

    #[test]
    fn package_dest_concatenates_and_routes_entrypoint() {
        let m = go_map();
        assert_eq!(
            package_dest(&p("internal/storage/storage.go"), &m),
            p("storage/mod.rs")
        );
        assert_eq!(
            package_dest(&p("internal/storage/badger.go"), &m),
            p("storage/mod.rs")
        );
        assert_eq!(
            package_dest(&p("internal/config/config.go"), &m),
            p("config/mod.rs")
        );
        assert_eq!(package_dest(&p("cmd/api/main.go"), &m), p("main.rs"));
    }

    #[test]
    fn collision_disambiguates_deterministically() {
        let targets = vec![p("a/util/u.go"), p("b/util/u.go")];
        let entries = HashSet::new();
        let m = build_package_map(&targets, &entries, true);
        let ra = &m.get(&p("a/util/u.go")).unwrap().root_segment;
        let rb = &m.get(&p("b/util/u.go")).unwrap().root_segment;
        assert_ne!(ra, rb, "colliding dirs must get distinct roots");
        // package name `util` now maps to two roots → ambiguous
        assert!(m.unique_root("util").is_none());
        // stable across calls
        let m2 = build_package_map(&targets, &entries, true);
        assert_eq!(
            m.get(&p("a/util/u.go")).unwrap().root_segment,
            m2.get(&p("a/util/u.go")).unwrap().root_segment
        );
    }

    #[test]
    fn repair_rewrites_cross_package_heads() {
        let m = go_map();
        let code = "fn run() {\n    let s = storage::Store::new();\n    let c = config::Config::load();\n}\n";
        let out = repair_module_refs(code, "", &m);
        assert!(out.contains("crate::storage::Store"), "got: {out}");
        assert!(out.contains("crate::config::Config"), "got: {out}");
    }

    #[test]
    fn repair_skips_self_package_name() {
        // Inside package `middleware`, `middleware::from_fn` means the EXTERNAL
        // module (axum's), not the package referring to itself.
        let m = go_map();
        let storage_map = {
            // build a map where `middleware` is a package
            let targets = vec![p("internal/middleware/mw.go"), p("cmd/api/main.go")];
            let entries: HashSet<PathBuf> = [p("cmd/api/main.go")].into_iter().collect();
            build_package_map(&targets, &entries, true)
        };
        let _ = m;
        let code = "fn f() { let x = middleware::from_fn(h); }\n";
        // this_pkg == middleware → do NOT rewrite
        let out = repair_module_refs(code, "middleware", &storage_map);
        assert!(
            out.contains("middleware::from_fn"),
            "self-pkg wrongly rewritten: {out}"
        );
        assert!(!out.contains("crate::middleware::from_fn"), "{out}");
        // from a different package, it IS rewritten
        let out2 = repair_module_refs(code, "storage", &storage_map);
        assert!(
            out2.contains("crate::middleware::from_fn"),
            "cross-pkg not rewritten: {out2}"
        );
    }

    #[test]
    fn repair_skips_use_group_subpaths() {
        let m = {
            let targets = vec![p("internal/storage/s.go"), p("cmd/api/main.go")];
            let entries: HashSet<PathBuf> = [p("cmd/api/main.go")].into_iter().collect();
            build_package_map(&targets, &entries, true)
        };
        // `storage` inside a use-group brace must NOT get a `crate::` (E0433).
        let code = "use foo::{bar, storage::Thing};\n";
        let out = repair_module_refs(code, "", &m);
        assert!(
            !out.contains("crate::storage"),
            "use-group leaf wrongly rewritten: {out}"
        );
        assert!(out.contains("storage::Thing"), "{out}");
        // but a top-level `use storage::X;` IS rewritten
        let code2 = "use storage::Thing;\n";
        let out2 = repair_module_refs(code2, "", &m);
        assert!(
            out2.contains("crate::storage::Thing"),
            "top-level use not rewritten: {out2}"
        );
    }

    #[test]
    fn repair_leaves_std_and_external_and_bare_idents() {
        let m = go_map();
        let code = "use std::fs;\nuse serde::Serialize;\nfn f() { tokio::spawn(x); let y = Store::new(); }\n";
        let out = repair_module_refs(code, "", &m);
        assert!(out.contains("std::fs"));
        assert!(out.contains("serde::Serialize"));
        assert!(out.contains("tokio::spawn"));
        assert!(out.contains("Store::new"));
        assert!(
            !out.contains("crate::"),
            "must not rewrite std/external/bare: {out}"
        );
    }

    #[test]
    fn repair_skips_already_qualified_and_strings_and_comments() {
        let m = go_map();
        let code =
            "let a = crate::storage::Store; // see storage::Old\nlet s = \"storage::Lit\";\n";
        let out = repair_module_refs(code, "", &m);
        assert!(out.contains("crate::storage::Store"));
        assert!(
            !out.contains("crate::crate::storage"),
            "double rewrite: {out}"
        );
        assert!(
            out.contains("// see storage::Old"),
            "comment changed: {out}"
        );
        assert!(
            out.contains("\"storage::Lit\""),
            "string literal changed: {out}"
        );
    }

    #[test]
    fn repair_strips_project_mod_decls_and_keeps_external() {
        let m = go_map();
        let code = "pub mod storage;\nmod config;\nmod helpers;\nfn x() {}\n";
        let out = repair_module_refs(code, "", &m);
        assert!(!out.contains("pub mod storage;"));
        assert!(!out.contains("mod config;"));
        assert!(
            out.contains("mod helpers;"),
            "non-package mod must stay: {out}"
        );
    }

    #[test]
    fn repair_auto_pubs_top_level_items() {
        let m = go_map();
        let code = "fn open() {}\nstruct Store {}\n    fn inner() {}\npub fn already() {}\n";
        let out = repair_module_refs(code, "", &m);
        assert!(out.contains("pub fn open()"));
        assert!(out.contains("pub struct Store"));
        assert!(
            out.contains("    fn inner()"),
            "indented item must NOT be pub'd: {out}"
        );
        assert!(
            !out.contains("pub pub fn already"),
            "already-pub double-pubbed: {out}"
        );
    }

    #[test]
    fn repair_handles_lifetimes_without_eating_code() {
        let m = go_map();
        let code = "fn f<'a>(x: &'a str) -> storage::Store { todo!() }\n";
        let out = repair_module_refs(code, "", &m);
        assert!(out.contains("<'a>"), "lifetime mangled: {out}");
        assert!(out.contains("&'a str"), "lifetime ref mangled: {out}");
        assert!(
            out.contains("crate::storage::Store"),
            "ref after lifetime missed: {out}"
        );
    }

    #[test]
    fn strips_module_imports_that_collide_with_pub_mod() {
        let roots = vec![
            "cache".to_string(),
            "logging".to_string(),
            "config".to_string(),
        ];
        let body = "use crate::cache::{self, Cache, CacheConfig};\n\
                    use crate::logging;\n\
                    use crate::config::Config;\n\
                    use crate::storage::Store;\n";
        let out = strip_module_self_imports(body, &roots);
        // `self` leaf removed, siblings kept
        assert!(
            out.contains("use crate::cache::{Cache, CacheConfig};"),
            "{out}"
        );
        assert!(!out.contains("self"), "self leaf survived: {out}");
        // bare `use crate::logging;` dropped
        assert!(!out.contains("use crate::logging;"), "{out}");
        // item imports (no self) untouched
        assert!(out.contains("use crate::config::Config;"), "{out}");
        assert!(out.contains("use crate::storage::Store;"), "{out}");
    }

    #[test]
    fn dedup_module_uses_drops_duplicate_imports() {
        let code = "use std::collections::HashMap;\nuse serde::{Serialize, Deserialize};\n\
                    pub fn a() {}\nuse std::collections::HashMap;\n\
                    fn b() {\n    use std::fmt::Write;\n    use std::fmt::Write;\n}\n";
        let out = dedup_module_uses(code);
        // top-level HashMap import appears once
        assert_eq!(out.matches("use std::collections::HashMap;").count(), 1);
        // distinct serde import kept
        assert!(out.contains("use serde::{Serialize, Deserialize};"));
        // indented (in-function) uses are NOT deduped (different scope)
        assert_eq!(out.matches("    use std::fmt::Write;").count(), 2);
    }

    #[test]
    fn c_language_is_file_as_module() {
        let targets = vec![p("main.c"), p("main.h"), p("util.c")];
        let entries = HashSet::new();
        let m = build_package_map(&targets, &entries, false);
        assert_eq!(m.get(&p("main.c")).unwrap().root_segment, "main");
        assert_eq!(m.get(&p("main.h")).unwrap().root_segment, "main_h");
        assert_eq!(m.get(&p("util.c")).unwrap().root_segment, "util");
        assert_eq!(package_dest(&p("util.c"), &m), p("util/mod.rs"));
    }

    #[test]
    fn generated_cargo_toml_opts_out_of_parent_workspace() {
        // Create a temporary directory to scaffold into
        let temp_dir = std::env::temp_dir().join("rustyfi_test_scaffold");
        if temp_dir.exists() {
            fs::remove_dir_all(&temp_dir).unwrap();
        }
        fs::create_dir_all(&temp_dir).unwrap();

        let scaffolder = Scaffolder::new(temp_dir.clone(), "test_crate".to_string());
        scaffolder.scaffold().unwrap();

        // Read back the Cargo.toml and verify it contains [workspace]
        let cargo_path = temp_dir.join("Cargo.toml");
        let content = fs::read_to_string(&cargo_path).unwrap();
        assert!(
            content.contains("[workspace]"),
            "Cargo.toml should contain [workspace] section to opt out of parent workspace. Got:\n{}",
            content
        );

        // Cleanup
        fs::remove_dir_all(&temp_dir).unwrap();
    }
}
