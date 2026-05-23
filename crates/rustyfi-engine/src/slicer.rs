/// Symbol Ownership Graph — tracks which symbols each file exports and
/// accumulates their Rust signatures as translation progresses.
///
/// ## Why this matters
/// When translating file B that imports `MyClass` from file A:
/// - Without ownership: LLM guesses the Rust type for `MyClass`.
/// - With ownership:    LLM receives `pub struct MyClass { ... }` from A's
///   already-translated output.
///
/// Result: type-compatible interfaces across the module boundary, fewer
/// borrow/lifetime errors in the fix loop, and dramatically smaller repair
/// scope.
///
/// ## Signature extraction
/// After each file's translation, `OwnershipGraph::record_rust_signatures`
/// extracts `pub` declarations from the generated Rust code.  These are then
/// injected into the prompt for any file that imports from the translated one.
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::debug;

// ---------------------------------------------------------------------------
// Symbol types
// ---------------------------------------------------------------------------

/// The kind of a top-level source symbol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SymbolKind {
    Function,
    AsyncFunction,
    Class,
    Struct,
    Trait,
    Enum,
    TypeAlias,
    Constant,
    Variable,
    Module,
    Other,
}

/// A single symbol exported from a source file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolEntry {
    /// Identifier name in the *source* language.
    pub name: String,
    pub kind: SymbolKind,
    /// Absolute path to the file that owns this symbol.
    pub file: PathBuf,
    /// 1-based line where this symbol starts in the source file.
    pub line: usize,
    /// Rust signature extracted after translation (e.g. `pub fn foo(x: i32) -> String`).
    /// `None` until the owning file has been translated.
    pub rust_signature: Option<String>,
}

// ---------------------------------------------------------------------------
// OwnershipGraph
// ---------------------------------------------------------------------------

/// Bidirectional symbol ↔ file ownership index.
///
/// Persisted inside `TranslationCheckpoint` so that resume works correctly:
/// a resumed translation run has access to all signatures from previously
/// translated files.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OwnershipGraph {
    /// file → list of exported symbols.
    exports: HashMap<PathBuf, Vec<SymbolEntry>>,
    /// file → list of (symbol_name, source_file) pairs it imports.
    imports: HashMap<PathBuf, Vec<(String, PathBuf)>>,
}

impl OwnershipGraph {
    pub fn new() -> Self {
        Self::default()
    }

    // ── Symbol registration ──────────────────────────────────────────────

    /// Register symbols exported by `file` extracted from its *source* code.
    pub fn register_source_symbols(&mut self, file: &Path, symbols: Vec<SymbolEntry>) {
        self.exports.insert(file.to_path_buf(), symbols);
    }

    /// Register import relationships: `file` imports `symbol` from `source_file`.
    pub fn register_import(
        &mut self,
        file: &Path,
        symbol: impl Into<String>,
        source_file: PathBuf,
    ) {
        self.imports
            .entry(file.to_path_buf())
            .or_default()
            .push((symbol.into(), source_file));
    }

    // ── Signature recording ──────────────────────────────────────────────

    /// After `file` has been translated to `rust_code`, extract its public
    /// Rust declarations and store them as signatures.
    ///
    /// Extracted signatures are then available to any importer of `file`.
    pub fn record_rust_signatures(&mut self, file: &Path, rust_code: &str) {
        let sigs = extract_pub_declarations(rust_code);
        debug!(
            "OwnershipGraph: {} → {} Rust signatures",
            file.display(),
            sigs.len()
        );

        if let Some(entries) = self.exports.get_mut(file) {
            // Try to match signatures to existing symbol entries by name.
            for entry in entries.iter_mut() {
                if let Some(sig) = sigs.iter().find(|s| s.contains(&entry.name)) {
                    entry.rust_signature = Some(sig.clone());
                }
            }
        } else {
            // File had no registered symbols — create entries from signatures.
            let entries: Vec<SymbolEntry> = sigs
                .iter()
                .enumerate()
                .map(|(i, sig)| {
                    let name = extract_name_from_sig(sig).unwrap_or_else(|| format!("sym_{i}"));
                    SymbolEntry {
                        name,
                        kind: SymbolKind::Other,
                        file: file.to_path_buf(),
                        line: 0,
                        rust_signature: Some(sig.clone()),
                    }
                })
                .collect();
            self.exports.insert(file.to_path_buf(), entries);
        }
    }

    // ── Context generation ───────────────────────────────────────────────

    /// Generate the Rust-signature context string to inject into the
    /// translation prompt for `file`.
    ///
    /// Returns only the signatures of files that `file` imports from, already
    /// translated.  Files not yet translated contribute nothing (context grows
    /// as translation progresses).
    pub fn translation_context_for(&self, _file: &Path, dep_files: &[&PathBuf]) -> String {
        let mut sections: Vec<String> = vec![];

        for dep in dep_files {
            let sigs: Vec<String> = self
                .exports
                .get(*dep)
                .map(|entries| {
                    entries
                        .iter()
                        .filter_map(|e| e.rust_signature.as_ref())
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();

            if !sigs.is_empty() {
                let dep_name = dep.file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| dep.to_string_lossy().to_string());
                sections.push(format!(
                    "// From `{dep_name}` (already translated):\n{}",
                    sigs.join("\n")
                ));
            }
        }

        sections.join("\n\n")
    }

    /// Total number of files with recorded symbols.
    pub fn file_count(&self) -> usize {
        self.exports.len()
    }

    /// Total number of recorded Rust signatures.
    pub fn signature_count(&self) -> usize {
        self.exports
            .values()
            .flat_map(|entries| entries.iter())
            .filter(|e| e.rust_signature.is_some())
            .count()
    }

    /// All Rust signatures for `file` (for use in cross-file verification context).
    pub fn signatures_for(&self, file: &Path) -> Vec<String> {
        self.exports
            .get(file)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|e| e.rust_signature.as_ref())
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------
// Helpers: pub declaration extraction from Rust code
// ---------------------------------------------------------------------------

/// Extract lines that look like `pub` declarations from generated Rust code.
///
/// These are used as the "interface contract" for downstream files to import.
/// Extracts signatures only (not bodies) to keep context size minimal.
fn extract_pub_declarations(rust_code: &str) -> Vec<String> {
    let mut sigs: Vec<String> = vec![];
    let mut in_block = false;
    let mut brace_depth: i32 = 0;
    let mut current_sig: Vec<String> = vec![];

    for line in rust_code.lines() {
        let trimmed = line.trim();

        // Detect start of a pub declaration.
        if !in_block && is_pub_declaration_start(trimmed) {
            in_block = true;
            brace_depth = 0;
            current_sig.clear();
        }

        if in_block {
            current_sig.push(line.to_string());
            for ch in line.chars() {
                match ch {
                    '{' => brace_depth += 1,
                    '}' => {
                        brace_depth -= 1;
                        if brace_depth <= 0 {
                            // End of declaration — extract just the signature line.
                            if let Some(sig) = current_sig.first() {
                                let clean = sig.trim()
                                    .trim_end_matches('{')
                                    .trim()
                                    .to_string();
                                if !clean.is_empty() {
                                    sigs.push(clean);
                                }
                            }
                            in_block = false;
                            current_sig.clear();
                            break;
                        }
                    }
                    _ => {}
                }
            }

            // Single-line declarations without braces (e.g. `pub type Foo = Bar;`).
            if in_block && brace_depth == 0 && trimmed.ends_with(';') {
                let clean = current_sig
                    .first()
                    .map(|s| s.trim().trim_end_matches(';').to_string())
                    .unwrap_or_default();
                if !clean.is_empty() {
                    sigs.push(clean);
                }
                in_block = false;
                current_sig.clear();
            }
        }
    }

    sigs
}

fn is_pub_declaration_start(trimmed: &str) -> bool {
    (trimmed.starts_with("pub fn ")
        || trimmed.starts_with("pub async fn ")
        || trimmed.starts_with("pub struct ")
        || trimmed.starts_with("pub enum ")
        || trimmed.starts_with("pub trait ")
        || trimmed.starts_with("pub type ")
        || trimmed.starts_with("pub const ")
        || trimmed.starts_with("pub static ")
        || trimmed.starts_with("pub mod ")
        || trimmed.starts_with("pub impl "))
        && !trimmed.starts_with("//")
}

fn extract_name_from_sig(sig: &str) -> Option<String> {
    // e.g. "pub fn foo(x: i32)" → "foo"
    //      "pub struct Bar"     → "Bar"
    let after_pub = sig.trim_start_matches("pub").trim();
    let after_kind = after_pub
        .trim_start_matches("async ")
        .trim_start_matches("fn ")
        .trim_start_matches("struct ")
        .trim_start_matches("enum ")
        .trim_start_matches("trait ")
        .trim_start_matches("type ")
        .trim_start_matches("const ")
        .trim_start_matches("static ");

    let name = after_kind
        .split(['(', '<', ':', ' '])
        .next()?
        .to_string();

    if name.is_empty() { None } else { Some(name) }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_fn_signature() {
        let code = "pub fn greet(name: &str) -> String {\n    format!(\"hello {name}\")\n}\n";
        let sigs = extract_pub_declarations(code);
        assert_eq!(sigs.len(), 1);
        assert!(sigs[0].contains("greet"), "got: {:?}", sigs[0]);
    }

    #[test]
    fn extract_struct_signature() {
        let code = "pub struct Config {\n    pub host: String,\n    pub port: u16,\n}\n";
        let sigs = extract_pub_declarations(code);
        assert_eq!(sigs.len(), 1);
        assert!(sigs[0].contains("Config"));
    }

    #[test]
    fn extract_multiple_signatures() {
        let code = r#"
pub struct Foo { pub x: i32 }
pub fn bar() -> Foo { Foo { x: 1 } }
pub enum Status { Ok, Err }
"#;
        let sigs = extract_pub_declarations(code);
        assert_eq!(sigs.len(), 3);
    }

    #[test]
    fn type_alias_extracted() {
        let code = "pub type Result<T> = std::result::Result<T, MyError>;\n";
        let sigs = extract_pub_declarations(code);
        assert_eq!(sigs.len(), 1);
        assert!(sigs[0].contains("Result"));
    }

    #[test]
    fn ownership_graph_context_empty_when_no_sigs() {
        let g = OwnershipGraph::new();
        let dep = PathBuf::from("dep.py");
        // No signatures recorded
        let ctx = g.translation_context_for(Path::new("main.py"), &[&dep]);
        assert!(ctx.is_empty());
    }

    #[test]
    fn ownership_graph_context_after_record() {
        let mut g = OwnershipGraph::new();
        let dep = PathBuf::from("utils.py");
        let code = "pub fn helper(x: i32) -> i32 {\n    x + 1\n}\n";
        g.record_rust_signatures(&dep, code);

        let ctx = g.translation_context_for(Path::new("main.py"), &[&dep]);
        assert!(ctx.contains("helper"), "context: {ctx}");
        assert!(ctx.contains("utils.py"));
    }

    #[test]
    fn extract_name_from_sig_fn() {
        assert_eq!(
            extract_name_from_sig("pub fn process(x: i32) -> bool"),
            Some("process".into())
        );
    }

    #[test]
    fn extract_name_from_sig_struct() {
        assert_eq!(
            extract_name_from_sig("pub struct Config"),
            Some("Config".into())
        );
    }
}
