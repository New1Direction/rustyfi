//! Deterministic duplicate-item removal for concatenated package modules.
//!
//! Concatenating sibling files (and re-emitted LLM chunks) into one
//! `src/<pkg>/mod.rs` produces the same top-level item defined more than once
//! (`E0428`), the same impl block twice, and partially-overlapping `use`
//! statements (`E0252`). We parse the module with `syn`, locate each item's
//! byte range, and **textually excise** the losing duplicates from the original
//! string — never re-emitting via a pretty-printer, so the
//! `// <<<rustyfi:src=…>>>` sentinels and all translated `//` comments survive
//! byte-for-byte. On any parse failure the input is returned unchanged: the
//! pass can never make output worse.

use std::collections::BTreeMap;
use std::ops::Range;

use quote::ToTokens;
use syn::spanned::Spanned;

/// Remove duplicate top-level items from a concatenated module. Idempotent
/// (a deduped module re-parses with no remaining duplicates). Returns the input
/// unchanged if it does not parse as Rust.
pub fn dedup_top_level_items(module_src: &str) -> String {
    let file = match syn::parse_file(module_src) {
        Ok(f) => f,
        Err(_) => return module_src.to_string(),
    };

    let mut excise: Vec<Range<usize>> = Vec::new();

    // Buckets keyed by (kind, name [+ cfg]) → the items sharing that identity.
    let mut named: BTreeMap<String, Vec<Located>> = BTreeMap::new();
    // Verbatim-identical impl blocks (chunk re-emission).
    let mut impls: BTreeMap<String, Vec<Range<usize>>> = BTreeMap::new();
    // Top-level `use` statements, in order, with their flattened leaf paths.
    let mut uses: Vec<(Range<usize>, Vec<String>)> = Vec::new();

    for item in &file.items {
        let Some(range) = item_range(module_src, item) else {
            continue;
        };

        if let syn::Item::Impl(_) = item {
            let norm = item.to_token_stream().to_string();
            impls.entry(norm).or_default().push(range);
            continue;
        }
        if let syn::Item::Use(u) = item {
            uses.push((range, flatten_use(&u.tree)));
            continue;
        }
        if let Some(key) = item_key(item) {
            let src = &module_src[range.clone()];
            named.entry(key).or_default().push(Located { range, src });
        }
    }

    // ── Named items: keep the best, excise the rest ──────────────────────
    for members in named.values() {
        if members.len() < 2 {
            continue;
        }
        let winner = best_index(members);
        for (i, m) in members.iter().enumerate() {
            if i != winner {
                excise.push(m.range.clone());
            }
        }
    }

    // ── Verbatim-identical impls: keep first, drop later copies ──────────
    for copies in impls.values() {
        for r in copies.iter().skip(1) {
            excise.push(r.clone());
        }
    }

    // ── Redundant `use` statements: drop one whose leaves are all already
    //    imported by earlier statements (the chunk-re-emission overlap). ──
    let mut imported: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for (range, leaves) in &uses {
        if !leaves.is_empty() && leaves.iter().all(|l| imported.contains(l)) {
            excise.push(range.clone());
        } else {
            for l in leaves {
                imported.insert(l.clone());
            }
        }
    }

    if excise.is_empty() {
        return module_src.to_string();
    }

    // Excise descending so earlier offsets stay valid.
    excise.sort_by_key(|x| std::cmp::Reverse(x.start));
    let mut out = module_src.to_string();
    let mut last_start = usize::MAX;
    for r in excise {
        // Guard against overlapping/duplicate ranges.
        if r.end > last_start {
            continue;
        }
        if r.start <= r.end
            && r.end <= out.len()
            && out.is_char_boundary(r.start)
            && out.is_char_boundary(r.end)
        {
            out.replace_range(r.clone(), "");
            last_start = r.start;
        }
    }
    collapse_blank_lines(&out)
}

struct Located<'a> {
    range: Range<usize>,
    src: &'a str,
}

/// Byte range of an item including its attributes, snapped to clean line
/// boundaries (eat leading indentation, include the trailing newline).
fn item_range(src: &str, item: &syn::Item) -> Option<Range<usize>> {
    let span = item.span();
    let r = span.byte_range();
    if r.start == 0 && r.end == 0 {
        return None; // span-locations unavailable — degrade safely
    }
    if r.start > r.end || r.end > src.len() {
        return None;
    }
    let bytes = src.as_bytes();
    let mut start = r.start;
    // eat leading whitespace back to the line start (never past a newline)
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
        end += 1; // include the newline
    }
    if !src.is_char_boundary(start) || !src.is_char_boundary(end) {
        return Some(r); // fall back to the raw range if snapping desyncs
    }
    Some(start..end)
}

/// Canonical identity for a top-level item — same key ⇒ a redefinition (E0428).
/// `#[cfg(...)]`-gated twins get distinct keys so they're never merged.
fn item_key(item: &syn::Item) -> Option<String> {
    let (kind, ident, attrs): (&str, String, &[syn::Attribute]) = match item {
        syn::Item::Fn(f) => ("fn", f.sig.ident.to_string(), &f.attrs),
        syn::Item::Struct(s) => ("type", s.ident.to_string(), &s.attrs),
        syn::Item::Enum(e) => ("type", e.ident.to_string(), &e.attrs),
        syn::Item::Union(u) => ("type", u.ident.to_string(), &u.attrs),
        syn::Item::Type(t) => ("type", t.ident.to_string(), &t.attrs),
        syn::Item::Trait(t) => ("trait", t.ident.to_string(), &t.attrs),
        syn::Item::TraitAlias(t) => ("trait", t.ident.to_string(), &t.attrs),
        syn::Item::Const(c) => ("const", c.ident.to_string(), &c.attrs),
        syn::Item::Static(s) => ("const", s.ident.to_string(), &s.attrs),
        syn::Item::Mod(m) => ("mod", m.ident.to_string(), &m.attrs),
        _ => return None,
    };
    let mut key = format!("{kind}:{ident}");
    for attr in attrs {
        if attr.path().is_ident("cfg") {
            key.push('|');
            key.push_str(&attr.to_token_stream().to_string());
        }
    }
    Some(key)
}

/// Pick the survivor among same-keyed items: a real definition beats a
/// `todo!()`/empty stub; then documented; then longer; ties → earliest.
fn best_index(members: &[Located]) -> usize {
    let rank = |m: &Located| -> (bool, bool, usize) {
        let s = m.src;
        let is_real = !(s.contains("todo!(") || s.contains("unimplemented!(") || body_is_empty(s));
        let has_doc = s.contains("///") || s.contains("#[doc");
        (is_real, has_doc, s.trim().len())
    };
    let mut best = 0;
    for i in 1..members.len() {
        if rank(&members[i]) > rank(&members[best]) {
            best = i;
        }
    }
    best
}

/// Heuristic: does this item's body look empty (`{}` / `{ }`)?
fn body_is_empty(src: &str) -> bool {
    let trimmed = src.trim_end().trim_end_matches(';');
    trimmed.ends_with("{}") || trimmed.ends_with("{ }")
}

/// Flatten a `use` tree into its fully-qualified leaf paths (e.g.
/// `tracing::info`, `a::B as C`). Globs become `prefix::*`.
fn flatten_use(tree: &syn::UseTree) -> Vec<String> {
    fn walk(tree: &syn::UseTree, prefix: &str, out: &mut Vec<String>) {
        match tree {
            syn::UseTree::Path(p) => {
                let next = if prefix.is_empty() {
                    p.ident.to_string()
                } else {
                    format!("{prefix}::{}", p.ident)
                };
                walk(&p.tree, &next, out);
            }
            syn::UseTree::Name(n) => out.push(join(prefix, &n.ident.to_string())),
            syn::UseTree::Rename(r) => {
                out.push(format!(
                    "{} as {}",
                    join(prefix, &r.ident.to_string()),
                    r.rename
                ));
            }
            syn::UseTree::Glob(_) => out.push(join(prefix, "*")),
            syn::UseTree::Group(g) => {
                for t in &g.items {
                    walk(t, prefix, out);
                }
            }
        }
    }
    fn join(prefix: &str, leaf: &str) -> String {
        if prefix.is_empty() {
            leaf.to_string()
        } else {
            format!("{prefix}::{leaf}")
        }
    }
    let mut out = Vec::new();
    walk(tree, "", &mut out);
    out
}

/// Collapse runs of 3+ blank lines (left by excision) down to one.
fn collapse_blank_lines(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut blank_run = 0;
    for line in s.lines() {
        if line.trim().is_empty() {
            blank_run += 1;
            if blank_run <= 1 {
                out.push('\n');
            }
        } else {
            blank_run = 0;
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_duplicate_function_keeps_one_and_sentinels() {
        let code = "// <<<rustyfi:src=a.go>>>\npub fn cosine(x: f32) -> f32 { x }\n// <<<rustyfi:end src=a.go>>>\n\
                    // <<<rustyfi:src=b.go>>>\npub fn cosine(x: f32) -> f32 { x }\n// <<<rustyfi:end src=b.go>>>\n";
        let out = dedup_top_level_items(code);
        assert_eq!(out.matches("fn cosine").count(), 1, "got:\n{out}");
        assert!(
            out.contains("// <<<rustyfi:src=a.go>>>"),
            "sentinel lost:\n{out}"
        );
    }

    #[test]
    fn keeps_real_over_stub() {
        let code = "pub fn send() { todo!() }\npub fn send() -> i32 { 42 }\n";
        let out = dedup_top_level_items(code);
        assert_eq!(out.matches("fn send").count(), 1, "got:\n{out}");
        assert!(!out.contains("todo!("), "kept the stub:\n{out}");
        assert!(out.contains("42"), "lost the real impl:\n{out}");
    }

    #[test]
    fn cfg_twins_both_survive() {
        let code = "#[cfg(test)]\nfn helper() {}\nfn helper() { do_thing(); }\n";
        let out = dedup_top_level_items(code);
        assert_eq!(
            out.matches("fn helper").count(),
            2,
            "cfg twin removed:\n{out}"
        );
    }

    #[test]
    fn distinct_structs_preserved() {
        let code = "pub struct A { x: i32 }\npub struct B { y: i32 }\n";
        let out = dedup_top_level_items(code);
        assert!(out.contains("struct A"));
        assert!(out.contains("struct B"));
    }

    #[test]
    fn legal_sibling_impls_preserved() {
        let code = "impl Store { pub fn a(&self) {} }\nimpl Store { pub fn b(&self) {} }\n";
        let out = dedup_top_level_items(code);
        // different impls (not byte-identical) must both survive
        assert!(out.contains("fn a"), "lost impl a:\n{out}");
        assert!(out.contains("fn b"), "lost impl b:\n{out}");
    }

    #[test]
    fn verbatim_duplicate_impl_deduped() {
        let code = "impl Store { pub fn a(&self) {} }\nimpl Store { pub fn a(&self) {} }\n";
        let out = dedup_top_level_items(code);
        assert_eq!(out.matches("impl Store").count(), 1, "got:\n{out}");
    }

    #[test]
    fn redundant_use_subset_dropped() {
        let code = "use tracing::{info, error};\nuse tracing::info;\npub fn f() {}\n";
        let out = dedup_top_level_items(code);
        assert!(
            out.contains("use tracing::{info, error};"),
            "merged form lost:\n{out}"
        );
        assert_eq!(
            out.matches("use tracing::info;").count(),
            0,
            "redundant use kept:\n{out}"
        );
    }

    #[test]
    fn distinct_same_leaf_uses_preserved() {
        // a::Foo and b::Foo are different paths — must NOT drop b::Foo
        let code = "use a::Foo;\nuse b::Foo;\n";
        let out = dedup_top_level_items(code);
        assert!(out.contains("use a::Foo;"));
        assert!(out.contains("use b::Foo;"));
    }

    #[test]
    fn parse_failure_returns_input_unchanged() {
        let code = "fn broken( {{{ this is not rust";
        assert_eq!(dedup_top_level_items(code), code);
    }

    #[test]
    fn idempotent() {
        let code = "pub fn dup() {}\npub fn dup() {}\npub fn keep() {}\n";
        let once = dedup_top_level_items(code);
        let twice = dedup_top_level_items(&once);
        assert_eq!(once, twice, "not a fixpoint:\n{once}\n---\n{twice}");
    }
}
