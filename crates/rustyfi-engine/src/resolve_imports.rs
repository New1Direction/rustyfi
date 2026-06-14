//! Deterministic cross-module import resolution (zero LLM tokens).
//!
//! Translation flattens many source packages/namespaces into flat
//! `src/<module>/mod.rs` modules, but the model emits imports against the
//! *source* namespaces, which produces a storm of `E0432`/`E0433`:
//!
//! - **separator / casing drift** — `use crate::specs_base::X` when the module
//!   is `specsbase`;
//! - **symbol relocation** — `use crate::foo::Bar` when `Bar` is actually
//!   defined in module `baz`;
//! - **lost sub-namespaces** — `use crate::foo::sub::Bar` when `sub` was
//!   flattened away.
//!
//! The fix is one rule: **resolve a `use crate::…::Sym` to the module that
//! actually defines `Sym`**, ignoring the model's (broken) path. A syn-built
//! symbol→module index makes this targeted: it only rewrites when exactly one
//! module defines the symbol, never deletes an import, and is idempotent — a
//! path that already points at the right module is left untouched.
//!
//! This is NOT safe by construction, though: expanding a `use {a, b, c}` group
//! into per-leaf lines turns one unresolved-group error into several when some
//! leaves still don't resolve, and a unique symbol's home module may not be
//! wired into the crate root. So the pass is **gated on the compiler** by its
//! caller — applied against a snapshot, re-checked, and kept only when the
//! error count strictly drops, else reverted (see `phase_verify`). It runs as a
//! deterministic pass before the LLM fix loop, alongside `rustfix` and
//! dependency repair.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use syn::spanned::Spanned;
use syn::{UseTree, Visibility};
use walkdir::WalkDir;

use crate::fix_context::ItemIndex;

/// What a resolution pass changed.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ResolveReport {
    pub files_changed: usize,
    pub imports_rewritten: usize,
}

/// Index of where every top-level symbol lives, plus the set of module names
/// and a normalized lookup for separator/casing drift.
struct ImportIndex {
    /// All first-level module names (`src/<module>/mod.rs` or `src/<module>.rs`).
    modules: BTreeSet<String>,
    /// symbol name → the set of modules that define it.
    symbol_to_modules: BTreeMap<String, BTreeSet<String>>,
    /// normalized module name (lowercase, `_` removed) → the actual module,
    /// only when that normalization is unambiguous.
    norm_to_module: BTreeMap<String, String>,
}

/// Normalize a path segment for drift matching: lowercase, drop underscores.
fn normalize(seg: &str) -> String {
    seg.chars()
        .filter(|c| *c != '_')
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// Derive the first-level module name a `src/...`-relative path belongs to.
/// `src/foo/mod.rs` → `foo`; `src/foo.rs` → `foo`; `src/main.rs`/`lib.rs` → None.
fn module_of_relpath(rel: &str) -> Option<String> {
    let rest = rel.strip_prefix("src/")?;
    let first = rest.split('/').next()?;
    if let Some(dir) = first.strip_suffix(".rs") {
        // A top-level file module: `src/foo.rs` (first already includes `.rs`
        // only when there is no nested dir).
        if matches!(dir, "main" | "lib" | "mod") {
            return None;
        }
        return Some(dir.to_string());
    }
    // Nested: `src/foo/....` → `foo` is the module.
    Some(first.to_string())
}

impl ImportIndex {
    fn build(workspace: &Path) -> ImportIndex {
        let src_root = workspace.join("src");

        // 1. Enumerate first-level modules from the directory structure.
        let mut modules: BTreeSet<String> = BTreeSet::new();
        if let Ok(entries) = std::fs::read_dir(&src_root) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if e.path().is_dir() {
                    if src_root.join(&name).join("mod.rs").exists() {
                        modules.insert(name);
                    }
                } else if let Some(stem) = name.strip_suffix(".rs") {
                    if !matches!(stem, "main" | "lib" | "mod") {
                        modules.insert(stem.to_string());
                    }
                }
            }
        }

        // 2. symbol → modules, reusing the syn item index.
        let item_index = ItemIndex::build(workspace);
        let mut symbol_to_modules: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for (name, defs) in &item_index.items {
            for d in defs {
                if let Some(m) = module_of_relpath(&d.rel_path) {
                    symbol_to_modules.entry(name.clone()).or_default().insert(m);
                }
            }
        }

        // 3. normalized module lookup (drop ambiguous collisions).
        let mut norm_counts: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for m in &modules {
            norm_counts.entry(normalize(m)).or_default().push(m.clone());
        }
        let norm_to_module = norm_counts
            .into_iter()
            .filter_map(|(k, v)| (v.len() == 1).then(|| (k, v.into_iter().next().unwrap())))
            .collect();

        ImportIndex {
            modules,
            symbol_to_modules,
            norm_to_module,
        }
    }

    /// The unique module that defines `symbol`, if exactly one does.
    fn unique_module_for(&self, symbol: &str) -> Option<&str> {
        let set = self.symbol_to_modules.get(symbol)?;
        if set.len() == 1 {
            set.iter().next().map(|s| s.as_str())
        } else {
            None
        }
    }
}

/// One imported leaf: the path segments after `crate`, the item name, and an
/// optional `as` alias.
struct Leaf {
    /// Module path segments between `crate` and the leaf (e.g. `["foo","sub"]`).
    path: Vec<String>,
    name: String,
    alias: Option<String>,
    glob: bool,
}

/// Walk a `use` tree, collecting every `crate::…` leaf. Returns `None` for a
/// tree that does not start at `crate` (std/external imports are left alone).
fn collect_crate_leaves(tree: &UseTree) -> Option<Vec<Leaf>> {
    // The first segment must be `crate`.
    if let UseTree::Path(p) = tree {
        if p.ident == "crate" {
            let mut out = Vec::new();
            walk(&p.tree, &mut Vec::new(), &mut out);
            return Some(out);
        }
    }
    None
}

fn walk(tree: &UseTree, prefix: &mut Vec<String>, out: &mut Vec<Leaf>) {
    match tree {
        UseTree::Path(p) => {
            prefix.push(p.ident.to_string());
            walk(&p.tree, prefix, out);
            prefix.pop();
        }
        UseTree::Name(n) => out.push(Leaf {
            path: prefix.clone(),
            name: n.ident.to_string(),
            alias: None,
            glob: false,
        }),
        UseTree::Rename(r) => out.push(Leaf {
            path: prefix.clone(),
            name: r.ident.to_string(),
            alias: Some(r.rename.to_string()),
            glob: false,
        }),
        UseTree::Glob(_) => out.push(Leaf {
            path: prefix.clone(),
            name: String::new(),
            alias: None,
            glob: true,
        }),
        UseTree::Group(g) => {
            for t in &g.items {
                walk(t, prefix, out);
            }
        }
    }
}

/// Render a single-leaf `use` line for `leaf`, routed through `module` (when
/// `Some`) or its original path (when `None`).
fn render_leaf(vis: &str, module: Option<&str>, leaf: &Leaf) -> String {
    let body = match module {
        Some(m) => format!("crate::{m}::{}", leaf.name),
        None => {
            let mut segs = vec!["crate".to_string()];
            segs.extend(leaf.path.iter().cloned());
            if leaf.glob {
                format!("{}::*", segs.join("::"))
            } else {
                segs.push(leaf.name.clone());
                segs.join("::")
            }
        }
    };
    match &leaf.alias {
        Some(a) => format!("{vis}use {body} as {a};"),
        None => format!("{vis}use {body};"),
    }
}

/// Decide the target module for `leaf`, or `None` to leave it unchanged.
/// Returns `Some(module)` only when the rewrite is unambiguous AND differs from
/// the leaf's current first path segment.
fn resolve_leaf<'a>(idx: &'a ImportIndex, leaf: &Leaf) -> Option<&'a str> {
    if leaf.glob {
        return None;
    }
    let current_mod = leaf.path.first().map(|s| s.as_str());

    // Importing a bare module: `use crate::errors;` (no path before the name).
    if leaf.path.is_empty() {
        if idx.modules.contains(&leaf.name) {
            return None; // already valid
        }
        if let Some(m) = idx.norm_to_module.get(&normalize(&leaf.name)) {
            if m != &leaf.name {
                return Some(m);
            }
        }
        return None;
    }

    // Symbol-first: route to the module that actually defines this item.
    if let Some(m) = idx.unique_module_for(&leaf.name) {
        if Some(m) != current_mod {
            return Some(m);
        }
        return None; // already correct
    }

    // Symbol not found anywhere (hallucinated / stubbed): try only to fix a
    // drifted top module segment, and only if that real module DEFINES the
    // symbol — otherwise leave it for the doctor rather than mask the error.
    if let Some(cur) = current_mod {
        if !idx.modules.contains(cur) {
            if let Some(real) = idx.norm_to_module.get(&normalize(cur)) {
                if idx
                    .symbol_to_modules
                    .get(&leaf.name)
                    .is_some_and(|s| s.contains(real))
                {
                    return Some(real);
                }
            }
        }
    }
    None
}

/// Resolve cross-module imports across the workspace's `src/` tree, editing
/// files in place. Returns what changed.
pub fn resolve_crate_imports(workspace: &Path) -> ResolveReport {
    let idx = ImportIndex::build(workspace);
    let mut report = ResolveReport::default();

    let src_root = workspace.join("src");
    for entry in WalkDir::new(&src_root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("rs"))
    {
        let path = entry.path();
        let Ok(source) = std::fs::read_to_string(path) else {
            continue;
        };
        let Ok(file) = syn::parse_file(&source) else {
            continue;
        };

        // Collect (byte range, replacement) edits for each rewritable `use`.
        let mut edits: Vec<(std::ops::Range<usize>, String)> = Vec::new();
        for item in &file.items {
            let syn::Item::Use(use_item) = item else {
                continue;
            };
            let Some(leaves) = collect_crate_leaves(&use_item.tree) else {
                continue;
            };
            let vis = match &use_item.vis {
                Visibility::Public(_) => "pub ",
                _ => "",
            };

            let mut lines = Vec::with_capacity(leaves.len());
            let mut changed = false;
            for leaf in &leaves {
                match resolve_leaf(&idx, leaf) {
                    Some(m) => {
                        lines.push(render_leaf(vis, Some(m), leaf));
                        changed = true;
                    }
                    None => lines.push(render_leaf(vis, None, leaf)),
                }
            }
            if changed {
                let range = use_item.span().byte_range();
                if range.start < range.end && range.end <= source.len() {
                    report.imports_rewritten += lines.len();
                    edits.push((range, lines.join("\n")));
                }
            }
        }

        if edits.is_empty() {
            continue;
        }
        // Apply edits back-to-front so earlier ranges stay valid.
        edits.sort_by_key(|e| std::cmp::Reverse(e.0.start));
        let mut out = source.clone();
        for (range, replacement) in edits {
            out.replace_range(range, &replacement);
        }
        if out != source && std::fs::write(path, out).is_ok() {
            report.files_changed += 1;
        }
    }

    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn crate_at(tmp: &Path, files: &[(&str, &str)]) {
        fs::create_dir_all(tmp.join("src")).unwrap();
        fs::write(
            tmp.join("Cargo.toml"),
            "[package]\nname=\"t\"\nversion=\"0.1.0\"\nedition=\"2021\"\n[dependencies]\n",
        )
        .unwrap();
        for (rel, content) in files {
            let p = tmp.join("src").join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, content).unwrap();
        }
    }

    #[test]
    fn normalize_strips_underscores_and_case() {
        assert_eq!(normalize("Specs_Base"), "specsbase");
        assert_eq!(normalize("specsbase"), "specsbase");
    }

    #[test]
    fn module_of_relpath_handles_dir_and_file_modules() {
        assert_eq!(module_of_relpath("src/foo/mod.rs").as_deref(), Some("foo"));
        assert_eq!(module_of_relpath("src/bar.rs").as_deref(), Some("bar"));
        assert_eq!(module_of_relpath("src/main.rs"), None);
        assert_eq!(module_of_relpath("src/lib.rs"), None);
    }

    #[test]
    fn rewrites_symbol_relocation_to_real_module() {
        let tmp = tempfile::TempDir::new().unwrap();
        // `Bar` is defined in module `baz`, but imported from `foo`.
        crate_at(
            tmp.path(),
            &[
                ("foo/mod.rs", "use crate::foo::Bar;\npub fn use_it() {}\n"),
                ("baz/mod.rs", "pub struct Bar;\n"),
            ],
        );
        let report = resolve_crate_imports(tmp.path());
        assert_eq!(report.files_changed, 1);
        let foo = fs::read_to_string(tmp.path().join("src/foo/mod.rs")).unwrap();
        assert!(foo.contains("use crate::baz::Bar;"), "{foo}");
    }

    #[test]
    fn rewrites_separator_drift_via_symbol() {
        let tmp = tempfile::TempDir::new().unwrap();
        // module is `specsbase`; import says `specs_base`. `Thing` lives in specsbase.
        crate_at(
            tmp.path(),
            &[
                ("a/mod.rs", "use crate::specs_base::Thing;\n"),
                ("specsbase/mod.rs", "pub struct Thing;\n"),
            ],
        );
        let report = resolve_crate_imports(tmp.path());
        assert_eq!(report.files_changed, 1);
        let a = fs::read_to_string(tmp.path().join("src/a/mod.rs")).unwrap();
        assert!(a.contains("use crate::specsbase::Thing;"), "{a}");
    }

    #[test]
    fn expands_group_and_routes_each_leaf() {
        let tmp = tempfile::TempDir::new().unwrap();
        crate_at(
            tmp.path(),
            &[
                ("a/mod.rs", "use crate::wrong::{Alpha, Beta};\n"),
                ("m1/mod.rs", "pub struct Alpha;\n"),
                ("m2/mod.rs", "pub enum Beta { X }\n"),
            ],
        );
        resolve_crate_imports(tmp.path());
        let a = fs::read_to_string(tmp.path().join("src/a/mod.rs")).unwrap();
        assert!(a.contains("use crate::m1::Alpha;"), "{a}");
        assert!(a.contains("use crate::m2::Beta;"), "{a}");
    }

    #[test]
    fn leaves_ambiguous_symbol_untouched() {
        let tmp = tempfile::TempDir::new().unwrap();
        // `Dup` is defined in two modules → ambiguous → don't rewrite.
        crate_at(
            tmp.path(),
            &[
                ("a/mod.rs", "use crate::wrong::Dup;\n"),
                ("m1/mod.rs", "pub struct Dup;\n"),
                ("m2/mod.rs", "pub struct Dup;\n"),
            ],
        );
        let report = resolve_crate_imports(tmp.path());
        assert_eq!(
            report.files_changed, 0,
            "ambiguous symbol must be left alone"
        );
    }

    #[test]
    fn leaves_correct_import_untouched_idempotent() {
        let tmp = tempfile::TempDir::new().unwrap();
        crate_at(
            tmp.path(),
            &[
                ("a/mod.rs", "use crate::baz::Bar;\n"),
                ("baz/mod.rs", "pub struct Bar;\n"),
            ],
        );
        let report = resolve_crate_imports(tmp.path());
        assert_eq!(
            report.files_changed, 0,
            "already-correct import must not churn"
        );
    }

    #[test]
    fn does_not_touch_std_or_external_imports() {
        let tmp = tempfile::TempDir::new().unwrap();
        crate_at(
            tmp.path(),
            &[(
                "a/mod.rs",
                "use std::collections::HashMap;\nuse serde::Serialize;\n",
            )],
        );
        let report = resolve_crate_imports(tmp.path());
        assert_eq!(report.files_changed, 0);
    }

    #[test]
    fn leaves_hallucinated_symbol_for_the_doctor() {
        let tmp = tempfile::TempDir::new().unwrap();
        // `Ghost` is defined nowhere → leave the import (don't mask the error).
        crate_at(tmp.path(), &[("a/mod.rs", "use crate::nowhere::Ghost;\n")]);
        let report = resolve_crate_imports(tmp.path());
        assert_eq!(report.files_changed, 0);
    }
}
