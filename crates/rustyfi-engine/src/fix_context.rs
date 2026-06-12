//! Per-file context for the compile-fix prompt: the fixer finally SEES the
//! trait the compiler says is unsatisfied, the type defined two modules away,
//! and rustc's own explanation — instead of one file + a bare error string.

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;
use std::sync::{Mutex, OnceLock};

use rustyfi_core::state::CompilerDiagnostic;
use syn::spanned::Spanned;
use walkdir::WalkDir;

/// Budget cap for the context block injected into fix prompts.
pub const FIX_CTX_BUDGET: usize = 8_000;

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

/// A single top-level named item extracted from a source file.
#[derive(Debug, Clone)]
pub struct ItemDef {
    pub kind: &'static str,
    pub source_text: String,
    pub rel_path: String,
}

/// A top-level impl block extracted from a source file.
#[derive(Debug, Clone)]
pub struct ImplDef {
    pub trait_name: Option<String>,
    pub type_name: String,
    pub source_text: String,
    pub rel_path: String,
}

/// Index of all top-level items across the workspace src tree.
pub struct ItemIndex {
    /// name → definitions (may come from multiple files)
    pub items: HashMap<String, Vec<ItemDef>>,
    /// all impl blocks
    pub impls: Vec<ImplDef>,
}

// ---------------------------------------------------------------------------
// Process-global rustc --explain cache
// ---------------------------------------------------------------------------

fn explain_cache() -> &'static Mutex<HashMap<String, String>> {
    static CACHE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Return a cached `rustc --explain` excerpt for `code`, or `None`.
/// Uses a process-global OnceLock cache to avoid redundant command invocations.
pub fn explain_excerpt(code: &str) -> Option<String> {
    // Check cache first.
    {
        let cache = explain_cache().lock().ok()?;
        if let Some(cached) = cache.get(code) {
            return if cached.is_empty() {
                None
            } else {
                Some(cached.clone())
            };
        }
    }

    let output = Command::new("rustc")
        .args(["--explain", code])
        .output()
        .ok()?;

    let text: String = if output.status.success() {
        let raw = String::from_utf8_lossy(&output.stdout);
        raw.lines().take(40).collect::<Vec<_>>().join("\n")
    } else {
        String::new()
    };

    let mut cache = explain_cache().lock().ok()?;
    cache.insert(code.to_string(), text.clone());
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

// ---------------------------------------------------------------------------
// ItemIndex: building the index
// ---------------------------------------------------------------------------

impl ItemIndex {
    /// Parse every `.rs` under `<workspace>/src` with `syn`; collect top-level
    /// items by name using span byte ranges over the original source (the
    /// `dedup_items` pattern; files that fail to parse are skipped). Build
    /// once per fix cycle.  Never panics; I/O and parse errors skip the file.
    pub fn build(workspace: &Path) -> ItemIndex {
        let src_root = workspace.join("src");
        let mut items: HashMap<String, Vec<ItemDef>> = HashMap::new();
        let mut impls: Vec<ImplDef> = Vec::new();

        for entry in WalkDir::new(&src_root)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("rs"))
        {
            let path = entry.path();
            let source = match std::fs::read_to_string(path) {
                Ok(s) => s,
                Err(_) => continue,
            };
            let file = match syn::parse_file(&source) {
                Ok(f) => f,
                Err(_) => continue,
            };

            // Relative path for display (relative to workspace root).
            let rel_path = path
                .strip_prefix(workspace)
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/");

            for syn_item in &file.items {
                match syn_item {
                    syn::Item::Impl(imp) => {
                        let type_name = type_ident_of(&imp.self_ty);
                        let trait_name = imp.trait_.as_ref().map(|(_, path, _)| {
                            path.segments
                                .last()
                                .map(|s| s.ident.to_string())
                                .unwrap_or_default()
                        });
                        if let Some(src_text) = item_source(&source, syn_item) {
                            impls.push(ImplDef {
                                trait_name,
                                type_name,
                                source_text: src_text,
                                rel_path: rel_path.clone(),
                            });
                        }
                    }
                    _ => {
                        if let Some((kind, name)) = top_level_kind_name(syn_item) {
                            if let Some(src_text) = item_source(&source, syn_item) {
                                items.entry(name).or_default().push(ItemDef {
                                    kind,
                                    source_text: src_text,
                                    rel_path: rel_path.clone(),
                                });
                            }
                        }
                    }
                }
            }
        }

        ItemIndex { items, impls }
    }

    /// Context block for fixing `file`, given its diagnostics.  Budget-capped.
    pub fn context_for(&self, file: &Path, diags: &[CompilerDiagnostic], budget: usize) -> String {
        // ── 1. Gather diagnostics whose primary span matches this file ────────
        let file_str = file.to_string_lossy();
        // Normalize: strip workspace-relative prefix; we match on suffix.
        let relevant_diags: Vec<&CompilerDiagnostic> = diags
            .iter()
            .filter(|d| {
                d.spans
                    .iter()
                    .any(|s| s.is_primary && path_suffix_matches(&s.file_name, &file_str))
            })
            .collect();

        if relevant_diags.is_empty() {
            return String::new();
        }

        // ── 2. Collect identifiers defined in the target file itself ──────────
        let defined_in_file: std::collections::HashSet<String> = self
            .items
            .iter()
            .filter(|(_, defs)| {
                defs.iter()
                    .any(|d| path_suffix_matches(&d.rel_path, &file_str))
            })
            .map(|(name, _)| name.clone())
            .collect();

        // ── 3. Harvest backticked identifiers from diagnostic messages ────────
        let mut harvested: Vec<String> = Vec::new();
        let mut seen_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
        for diag in &relevant_diags {
            for ident in extract_backtick_idents(&diag.message) {
                // Take only the last path segment (a::b::C → C).
                let leaf = ident.split("::").last().unwrap_or(&ident).to_string();
                if !defined_in_file.contains(&leaf) && seen_ids.insert(leaf.clone()) {
                    harvested.push(leaf);
                }
            }
        }

        if harvested.is_empty() {
            return String::new();
        }

        // ── 4. Determine which diags call for E0277/E0038/E0599 (trait context) ─
        let needs_trait_ctx = relevant_diags.iter().any(|d| {
            matches!(
                d.code.as_deref(),
                Some("E0277") | Some("E0038") | Some("E0599")
            )
        });

        // ── 5. Collect distinct error codes for rustc --explain ───────────────
        let mut distinct_codes: Vec<String> = Vec::new();
        let mut code_seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for diag in &relevant_diags {
            if let Some(code) = &diag.code {
                if code_seen.insert(code.clone()) {
                    distinct_codes.push(code.clone());
                }
            }
        }

        // ── 6. Build definition blocks ────────────────────────────────────────
        let mut def_blocks: Vec<String> = Vec::new();
        let mut impl_blocks: Vec<String> = Vec::new();
        let mut explain_blocks: Vec<String> = Vec::new();

        for name in &harvested {
            if let Some(defs) = self.items.get(name) {
                for def in defs {
                    def_blocks.push(format!(
                        "// definition of {name} (from {})\n{}",
                        def.rel_path, def.source_text
                    ));
                }

                // For trait-related errors, also append impls.
                if needs_trait_ctx {
                    let is_trait = defs.iter().any(|d| d.kind == "trait");
                    if is_trait {
                        for imp in &self.impls {
                            let matches = imp
                                .trait_name
                                .as_deref()
                                .map(|t| t == name)
                                .unwrap_or(false)
                                || imp.type_name == *name;
                            if matches {
                                impl_blocks.push(format!(
                                    "// existing impls involving {name} (from {})\n{}",
                                    imp.rel_path, imp.source_text
                                ));
                            }
                        }
                    }
                }
            }
        }

        // ── 7. rustc --explain excerpts ───────────────────────────────────────
        for code in &distinct_codes {
            if let Some(explanation) = explain_excerpt(code) {
                explain_blocks.push(format!("// rustc --explain {code}\n{explanation}"));
            }
        }

        // ── 8. Assemble under budget (whole-section truncation) ───────────────
        let mut out = String::new();
        let candidate_groups: [&[String]; 3] = [&def_blocks, &impl_blocks, &explain_blocks];
        for group in candidate_groups {
            for block in group.iter() {
                if out.len() + block.len() < budget {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(block);
                } else {
                    // Whole section truncation: stop adding from this group.
                    break;
                }
            }
        }

        out
    }
}

// ---------------------------------------------------------------------------
// Helper: extract kind + name for named top-level items
// ---------------------------------------------------------------------------

fn top_level_kind_name(item: &syn::Item) -> Option<(&'static str, String)> {
    match item {
        syn::Item::Fn(f) => Some(("fn", f.sig.ident.to_string())),
        syn::Item::Struct(s) => Some(("struct", s.ident.to_string())),
        syn::Item::Enum(e) => Some(("enum", e.ident.to_string())),
        syn::Item::Union(u) => Some(("union", u.ident.to_string())),
        syn::Item::Type(t) => Some(("type", t.ident.to_string())),
        syn::Item::Trait(t) => Some(("trait", t.ident.to_string())),
        syn::Item::TraitAlias(t) => Some(("trait", t.ident.to_string())),
        syn::Item::Const(c) => Some(("const", c.ident.to_string())),
        syn::Item::Static(s) => Some(("const", s.ident.to_string())),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Helper: extract the type name from a `self_ty` path (best-effort)
// ---------------------------------------------------------------------------

fn type_ident_of(ty: &syn::Type) -> String {
    match ty {
        syn::Type::Path(tp) => tp
            .path
            .segments
            .last()
            .map(|s| s.ident.to_string())
            .unwrap_or_default(),
        _ => String::new(),
    }
}

// ---------------------------------------------------------------------------
// Helper: extract source text for an item using span byte ranges
// ---------------------------------------------------------------------------

fn item_source(src: &str, item: &syn::Item) -> Option<String> {
    let span = item.span();
    let r = span.byte_range();
    if r.start == 0 && r.end == 0 {
        return None;
    }
    if r.start > r.end || r.end > src.len() {
        return None;
    }
    // Snap to clean line boundaries (same pattern as dedup_items).
    let bytes = src.as_bytes();
    let mut start = r.start;
    while start > 0 {
        let b = bytes[start - 1];
        if b == b' ' || b == b'\t' {
            start -= 1;
        } else {
            break;
        }
    }
    let mut end = r.end;
    while end < src.len() && bytes[end] != b'\n' {
        end += 1;
    }
    if end < src.len() {
        end += 1;
    }
    if !src.is_char_boundary(start) || !src.is_char_boundary(end) {
        return Some(src[r].to_string());
    }
    Some(src[start..end].trim_end().to_string())
}

// ---------------------------------------------------------------------------
// Helper: extract all backtick-quoted identifiers from a string
// ---------------------------------------------------------------------------

fn extract_backtick_idents(msg: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = msg.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '`' {
            let tok: String = chars.by_ref().take_while(|&c| c != '`').collect();
            if !tok.is_empty() {
                out.push(tok);
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Helper: match a path by suffix (handles absolute vs relative mismatch)
// ---------------------------------------------------------------------------

fn path_suffix_matches(candidate: &str, target: &str) -> bool {
    // Normalize separators.
    let c = candidate.replace('\\', "/");
    let t = target.replace('\\', "/");
    // Exact match.
    if c == t {
        return true;
    }
    // Suffix match: "src/cache/mod.rs" ends with "src/cache/mod.rs".
    if c.ends_with(t.as_str()) || t.ends_with(c.as_str()) {
        return true;
    }
    false
}

// ---------------------------------------------------------------------------
// Tests (C1 — write first, then implement)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use rustyfi_core::state::{DiagnosticLevel, DiagnosticSpan};
    use std::fs;
    use std::path::PathBuf;

    // Helpers to build test CompilerDiagnostic values without touching JSON.
    fn make_span(file: &str, primary: bool) -> DiagnosticSpan {
        DiagnosticSpan {
            file_name: file.to_string(),
            line_start: 1,
            line_end: 1,
            column_start: 1,
            column_end: 1,
            is_primary: primary,
            label: None,
        }
    }

    fn make_diag(msg: &str, code: Option<&str>, file: &str) -> CompilerDiagnostic {
        CompilerDiagnostic {
            level: DiagnosticLevel::Error,
            message: msg.to_string(),
            code: code.map(|s| s.to_string()),
            spans: vec![make_span(file, true)],
            rendered: None,
        }
    }

    /// Create a temp workspace with two .rs source files and return the
    /// workspace path plus the path of the "error file".
    fn setup_tempdir() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::TempDir::new().unwrap();
        let ws = tmp.path().to_path_buf();
        let src = ws.join("src");
        fs::create_dir_all(&src).unwrap();

        // File 1: defines `CacheEntry` (struct) and `Provider` (trait + impl).
        let provider_src = ws.join("src/cache.rs");
        fs::write(
            &provider_src,
            r#"
pub struct CacheEntry {
    pub key: String,
    pub value: String,
}

pub trait Provider {
    fn get(&self, key: &str) -> Option<CacheEntry>;
    fn set(&mut self, entry: CacheEntry);
}

pub struct MemProvider;

impl Provider for MemProvider {
    fn get(&self, _key: &str) -> Option<CacheEntry> { None }
    fn set(&mut self, _entry: CacheEntry) {}
}
"#,
        )
        .unwrap();

        // File 2: the "error file" — defines nothing we care about.
        let error_file = ws.join("src/consumer.rs");
        fs::write(&error_file, "pub fn use_it() {}\n").unwrap();

        (tmp, error_file)
    }

    // C1a: index finds CacheEntry, Provider, MemProvider, and the impl.
    #[test]
    fn index_finds_named_items_and_impls() {
        let (tmp, _error_file) = setup_tempdir();
        let idx = ItemIndex::build(tmp.path());

        assert!(
            idx.items.contains_key("CacheEntry"),
            "CacheEntry not found in index; keys: {:?}",
            idx.items.keys().collect::<Vec<_>>()
        );
        assert!(
            idx.items.contains_key("Provider"),
            "Provider not found in index"
        );
        // At least one impl should be present (Provider for MemProvider).
        assert!(!idx.impls.is_empty(), "no impls found in index");
        let provider_impl = idx
            .impls
            .iter()
            .any(|i| i.trait_name.as_deref() == Some("Provider"));
        assert!(provider_impl, "Provider impl not found");
    }

    // C1b: harvest pulls the right idents from backtick messages; path segments stripped.
    #[test]
    fn context_harvests_backtick_idents_and_strips_path_prefix() {
        let (tmp, error_file) = setup_tempdir();
        let idx = ItemIndex::build(tmp.path());

        // Fabricate diagnostics whose primary span points to the error file.
        let rel_error = "src/consumer.rs";
        let diags = vec![
            make_diag(
                "trait bound `cache::CacheEntry: Debug` is not satisfied",
                Some("E0277"),
                rel_error,
            ),
            make_diag(
                "the trait `Provider` is not implemented for `SomeType`",
                Some("E0277"),
                rel_error,
            ),
        ];

        let ctx = idx.context_for(&error_file, &diags, FIX_CTX_BUDGET);
        // Should include CacheEntry definition (harvested from `cache::CacheEntry`).
        assert!(
            ctx.contains("CacheEntry"),
            "CacheEntry not in context:\n{ctx}"
        );
        // Should include Provider definition.
        assert!(ctx.contains("Provider"), "Provider not in context:\n{ctx}");
    }

    // C1c: E0277 context also includes the impl for Provider.
    #[test]
    fn e0277_context_includes_trait_impl() {
        let (tmp, error_file) = setup_tempdir();
        let idx = ItemIndex::build(tmp.path());

        let rel_error = "src/consumer.rs";
        let diags = vec![make_diag(
            "the trait `Provider` is not implemented",
            Some("E0277"),
            rel_error,
        )];

        let ctx = idx.context_for(&error_file, &diags, FIX_CTX_BUDGET);
        assert!(
            ctx.contains("impl Provider"),
            "impl Provider not in context:\n{ctx}"
        );
    }

    // C1d: tiny budget truncates whole sections (never mid-item).
    #[test]
    fn tiny_budget_truncates_whole_sections() {
        let (tmp, error_file) = setup_tempdir();
        let idx = ItemIndex::build(tmp.path());

        let rel_error = "src/consumer.rs";
        let diags = vec![make_diag(
            "trait `Provider` not satisfied",
            Some("E0277"),
            rel_error,
        )];

        // Budget of 50 is too small for any full block.
        let ctx = idx.context_for(&error_file, &diags, 50);
        // Either empty or contains only whole blocks (no half-rendered definitions).
        // The context must be ≤ budget bytes.
        assert!(
            ctx.len() <= 50,
            "context exceeds budget ({} > 50):\n{ctx}",
            ctx.len()
        );
    }

    // C1e: identifiers defined in the target file itself are excluded.
    #[test]
    fn definitions_in_target_file_are_excluded() {
        let (tmp, _error_file) = setup_tempdir();
        let idx = ItemIndex::build(tmp.path());

        // The error file is cache.rs — CacheEntry is defined there.
        let cache_file = tmp.path().join("src/cache.rs");
        let diags = vec![make_diag(
            "type `CacheEntry` not found",
            None,
            "src/cache.rs",
        )];

        let ctx = idx.context_for(&cache_file, &diags, FIX_CTX_BUDGET);
        // CacheEntry is in cache.rs itself → must NOT appear in context.
        assert!(
            !ctx.contains("definition of CacheEntry"),
            "CacheEntry (defined in target file) leaked into context:\n{ctx}"
        );
    }

    // C1f: no relevant diagnostics → empty string.
    #[test]
    fn empty_context_when_no_relevant_diags() {
        let (tmp, error_file) = setup_tempdir();
        let idx = ItemIndex::build(tmp.path());

        // Diagnostics point to a different file.
        let diags = vec![make_diag("boom", None, "src/other.rs")];
        let ctx = idx.context_for(&error_file, &diags, FIX_CTX_BUDGET);
        assert!(ctx.is_empty(), "expected empty context, got:\n{ctx}");
    }
}
