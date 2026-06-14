//! Compiler-guided derive insertion (zero LLM tokens).
//!
//! Machine translation routinely emits structs and enums without the standard
//! derives they need — `Debug`, `Clone`, `PartialEq`, … The compiler names the
//! exact `(type, trait)` pair in every `E0277` ("`Foo` doesn't implement
//! `Debug`"), so the fix is deterministic: add the derive.
//!
//! Conservative by construction, and gated by the caller on the compiler
//! (kept only if the error count drops, else reverted), so a derive that
//! cannot apply — e.g. a struct with a boxed-closure field that is not `Debug`
//! — simply gets rolled back. Only DERIVABLE std traits on LOCAL,
//! simple-named types are ever touched.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use walkdir::WalkDir;

/// Traits that `#[derive(...)]` can synthesize.
const DERIVABLE: &[&str] = &[
    "Debug",
    "Clone",
    "PartialEq",
    "Eq",
    "Hash",
    "Default",
    "PartialOrd",
    "Ord",
];

/// What a pass changed.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DeriveReport {
    pub files_changed: usize,
    pub derives_added: usize,
}

/// Extract `(type, trait)` requests from compiler messages, keeping only
/// derivable traits on simple-named local-looking types. `messages` are the raw
/// `E0277` diagnostic messages.
pub fn needed_derives(messages: &[String]) -> BTreeMap<String, BTreeSet<String>> {
    let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for msg in messages {
        for (ty, tr) in parse_pair(msg) {
            let trait_simple = tr.rsplit("::").next().unwrap_or(&tr).trim().to_string();
            if !DERIVABLE.contains(&trait_simple.as_str()) {
                continue;
            }
            let Some(ty_simple) = simple_type_name(&ty) else {
                continue;
            };
            out.entry(ty_simple).or_default().insert(trait_simple);
        }
    }
    out
}

/// Parse the two E0277 shapes:
///   "`Type` doesn't implement `Trait`"
///   "the trait bound `Type: Trait` is not satisfied"
fn parse_pair(msg: &str) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    if let Some((ty, tr)) = split_around(msg, "` doesn't implement `") {
        pairs.push((ty, tr));
    }
    if msg.contains("the trait bound") {
        if let Some(inner) = between_backticks(msg) {
            // Split on the `: ` between type and trait — `::` path separators
            // (e.g. `crate::a::Foo`) never contain a colon-space.
            if let Some((ty, tr)) = inner.split_once(": ") {
                pairs.push((ty.trim().to_string(), tr.trim().to_string()));
            }
        }
    }
    pairs
}

/// Split `…`A` doesn't implement `B`…` into (A, B), pulling the backtick-quoted
/// identifiers on either side of `mid`.
fn split_around(msg: &str, mid: &str) -> Option<(String, String)> {
    let pos = msg.find(mid)?;
    let left = msg[..pos].rfind('`').map(|i| msg[i + 1..pos].to_string())?;
    let rest = &msg[pos + mid.len()..];
    let right = rest.find('`').map(|i| rest[..i].to_string())?;
    Some((left, right))
}

fn between_backticks(s: &str) -> Option<String> {
    let start = s.find('`')? + 1;
    let rest = &s[start..];
    let end = rest.find('`')?;
    Some(rest[..end].to_string())
}

/// `KyResponse<T>` → `Some("KyResponse")`; reject fn pointers, refs, slices,
/// `dyn`, tuples, and anything not a bare path's last segment.
fn simple_type_name(ty: &str) -> Option<String> {
    let base = ty.split('<').next().unwrap_or(ty).trim();
    // Path types: take the last segment (`crate::types::Foo` → `Foo`).
    let last = base.rsplit("::").next().unwrap_or(base).trim();
    let ok = !last.is_empty()
        && last
            .chars()
            .next()
            .is_some_and(|c| c.is_alphabetic() || c == '_')
        && last.chars().all(|c| c.is_alphanumeric() || c == '_');
    ok.then(|| last.to_string())
}

/// Add the requested derives to every matching struct/enum across the
/// workspace's `src/`. Editing is textual to preserve sentinels and formatting.
pub fn add_missing_derives(
    workspace: &Path,
    want: &BTreeMap<String, BTreeSet<String>>,
) -> DeriveReport {
    let mut report = DeriveReport::default();
    if want.is_empty() {
        return report;
    }
    let src = workspace.join("src");
    for entry in WalkDir::new(&src)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("rs"))
    {
        let path = entry.path();
        let Ok(source) = std::fs::read_to_string(path) else {
            continue;
        };
        let (new_source, added) = apply_to_source(&source, want);
        if added > 0 && new_source != source && std::fs::write(path, new_source).is_ok() {
            report.files_changed += 1;
            report.derives_added += added;
        }
    }
    report
}

/// Add derives to matching type definitions in one file's text. Returns
/// `(new_text, derives_added)`.
fn apply_to_source(source: &str, want: &BTreeMap<String, BTreeSet<String>>) -> (String, usize) {
    let lines: Vec<&str> = source.lines().collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len() + 4);
    let mut added = 0usize;
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if let Some(ty) = struct_or_enum_name(line) {
            if let Some(traits) = want.get(&ty) {
                // Scan the already-emitted preceding attr/doc lines for an
                // existing #[derive(...)] to merge into.
                let merged = merge_into_existing_derive(&mut out, traits);
                if let Some(n) = merged {
                    added += n;
                } else {
                    let missing: Vec<&String> = traits.iter().collect();
                    if !missing.is_empty() {
                        let indent: String =
                            line.chars().take_while(|c| c.is_whitespace()).collect();
                        let list = missing
                            .iter()
                            .map(|s| s.as_str())
                            .collect::<Vec<_>>()
                            .join(", ");
                        out.push(format!("{indent}#[derive({list})]"));
                        added += missing.len();
                    }
                }
            }
        }
        out.push(line.to_string());
        i += 1;
    }
    let mut text = out.join("\n");
    if source.ends_with('\n') {
        text.push('\n');
    }
    (text, added)
}

/// If the last non-doc emitted line is a `#[derive(...)]`, add any missing
/// traits to it. Returns the number added, or `None` if there was no derive
/// attribute to merge into (caller inserts a fresh one).
fn merge_into_existing_derive(out: &mut [String], traits: &BTreeSet<String>) -> Option<usize> {
    // Look back over attribute / doc-comment lines for an existing derive.
    for line in out.iter_mut().rev() {
        let t = line.trim_start();
        if t.starts_with("#[derive(") && t.ends_with(")]") {
            let open = line.find('(')? + 1;
            let close = line.rfind(')')?;
            let mut present: Vec<String> = line[open..close]
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            let mut added = 0;
            for tr in traits {
                if !present.iter().any(|p| p == tr) {
                    present.push(tr.clone());
                    added += 1;
                }
            }
            let rebuilt = format!(
                "{}#[derive({})]",
                &line[..line.find('#')?],
                present.join(", ")
            );
            *line = rebuilt;
            return Some(added);
        }
        // Keep scanning past doc comments and other attributes; stop at code.
        if t.starts_with("///")
            || t.starts_with("//!")
            || t.starts_with("//")
            || t.starts_with("#[")
        {
            continue;
        }
        break;
    }
    None
}

/// The struct/enum name declared on `line`, if any (`pub struct Foo<T>` → `Foo`).
fn struct_or_enum_name(line: &str) -> Option<String> {
    let t = line.trim_start();
    let rest = t
        .strip_prefix("pub struct ")
        .or_else(|| t.strip_prefix("pub enum "))
        .or_else(|| t.strip_prefix("struct "))
        .or_else(|| t.strip_prefix("enum "))
        .or_else(|| {
            // pub(crate)/pub(...) struct
            t.strip_prefix("pub(")
                .and_then(|r| r.find(") ").map(|i| &r[i + 2..]))
                .and_then(|r| {
                    r.strip_prefix("struct ")
                        .or_else(|| r.strip_prefix("enum "))
                })
        })?;
    let name: String = rest
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    (!name.is_empty()).then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_doesnt_implement_form() {
        let want = needed_derives(&["`KyResponse<T>` doesn't implement `Debug`".to_string()]);
        assert_eq!(
            want.get("KyResponse").unwrap().iter().next().unwrap(),
            "Debug"
        );
    }

    #[test]
    fn parses_trait_bound_form_and_strips_path() {
        let want = needed_derives(&[
            "the trait bound `crate::a::Foo: std::clone::Clone` is not satisfied".to_string(),
        ]);
        assert!(want.get("Foo").unwrap().contains("Clone"));
    }

    #[test]
    fn rejects_non_derivable_and_non_simple_types() {
        let want = needed_derives(&[
            "`Foo` doesn't implement `Serialize`".to_string(), // not derivable here
            "`fn() -> i32` doesn't implement `Debug`".to_string(), // fn pointer
            "`&str` doesn't implement `Debug`".to_string(),    // ref
            "`dyn Error` doesn't implement `Debug`".to_string(), // dyn
        ]);
        assert!(want.is_empty(), "got {want:?}");
    }

    #[test]
    fn inserts_derive_above_bare_struct() {
        let src = "pub struct Foo {\n    x: i32,\n}\n";
        let mut want = BTreeMap::new();
        want.insert(
            "Foo".to_string(),
            ["Debug"].iter().map(|s| s.to_string()).collect(),
        );
        let (out, n) = apply_to_source(src, &want);
        assert_eq!(n, 1);
        assert!(out.contains("#[derive(Debug)]\npub struct Foo"), "{out}");
        assert!(syn::parse_file(&out).is_ok(), "{out}");
    }

    #[test]
    fn merges_into_existing_derive() {
        let src = "#[derive(Clone)]\npub struct Foo {\n    x: i32,\n}\n";
        let mut want = BTreeMap::new();
        want.insert(
            "Foo".to_string(),
            ["Debug", "Clone"].iter().map(|s| s.to_string()).collect(),
        );
        let (out, n) = apply_to_source(src, &want);
        assert_eq!(n, 1, "only Debug is new; Clone already present");
        assert!(out.contains("#[derive(Clone, Debug)]"), "{out}");
        assert!(syn::parse_file(&out).is_ok(), "{out}");
    }

    #[test]
    fn inserts_after_doc_comment() {
        let src = "/// A widget.\npub struct Foo {\n    x: i32,\n}\n";
        let mut want = BTreeMap::new();
        want.insert(
            "Foo".to_string(),
            ["Debug"].iter().map(|s| s.to_string()).collect(),
        );
        let (out, _) = apply_to_source(src, &want);
        assert!(
            out.contains("/// A widget.\n#[derive(Debug)]\npub struct Foo"),
            "{out}"
        );
        assert!(syn::parse_file(&out).is_ok(), "{out}");
    }
}
