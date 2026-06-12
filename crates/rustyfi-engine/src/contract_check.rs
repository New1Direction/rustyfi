//! Compiler-validate package contracts BEFORE translation fan-out.
//!
//! The contract is the highest-leverage LLM output in the pipeline: every file
//! inherits it, so a structurally broken contract (e.g. a dyn-incompatible
//! trait used behind Box<dyn …>) multiplies into dozens of errors. cargo is
//! the oracle — same principle as the rustfix pass, moved upstream.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use rustyfi_core::compiler::{parse_cargo_diagnostics, run_cargo_check};
use rustyfi_core::state::{CompilerDiagnostic, DiagnosticLevel};

use crate::checkpoint::PackageContract;
use crate::EngineError;

// Error codes that indicate unresolved external-world items (import/path/trait/type
// resolution) — not the contract's structural fault; ignore them.
const IMPORT_CODES: &[&str] = &["E0432", "E0433", "E0405", "E0412"];

// ---------------------------------------------------------------------------
// Item-inventory helpers (item-preserving contract regeneration)
// ---------------------------------------------------------------------------

/// Parse `contract_rust` with `syn` and return the names of all top-level
/// items (fn, struct, enum, trait, type alias, const) plus trait method names
/// as `"TraitName::method_name"`.
///
/// On parse failure returns an empty set — callers must treat that as "unknown"
/// and should not reject the regeneration solely on this basis.
pub fn item_names(contract_rust: &str) -> BTreeSet<String> {
    let file = match syn::parse_str::<syn::File>(contract_rust) {
        Ok(f) => f,
        Err(_) => return BTreeSet::new(),
    };

    let mut names = BTreeSet::new();
    for item in &file.items {
        match item {
            syn::Item::Fn(f) => {
                names.insert(f.sig.ident.to_string());
            }
            syn::Item::Struct(s) => {
                names.insert(s.ident.to_string());
            }
            syn::Item::Enum(e) => {
                names.insert(e.ident.to_string());
            }
            syn::Item::Trait(t) => {
                let trait_name = t.ident.to_string();
                names.insert(trait_name.clone());
                // Collect trait method names as "TraitName::method_name".
                for trait_item in &t.items {
                    if let syn::TraitItem::Fn(m) = trait_item {
                        names.insert(format!("{}::{}", trait_name, m.sig.ident));
                    }
                }
            }
            syn::Item::Type(t) => {
                names.insert(t.ident.to_string());
            }
            syn::Item::Const(c) => {
                names.insert(c.ident.to_string());
            }
            _ => {}
        }
    }
    names
}

/// Returns `false` iff the old item set was non-empty AND the regenerated
/// contract dropped more than 10% of the original items (additions are fine;
/// exactly at the 10% threshold is acceptable).
///
/// Formally: `false` when `old.len() > 0 && dropped > old.len() / 10`
/// (integer division, so 1-of-10 = drop of 1 item is ACCEPTED).
pub fn regeneration_acceptable(old: &BTreeSet<String>, new: &BTreeSet<String>) -> bool {
    if old.is_empty() {
        return true;
    }
    let dropped = old.difference(new).count();
    // Threshold: dropped must NOT exceed 10% of original count.
    // Integer arithmetic: `dropped * 10 > old.len()` ↔ dropped > old.len() / 10.
    dropped * 10 <= old.len()
}

/// A compiler-validation failure for one package's contract.
pub struct ContractIssue {
    pub root_segment: String,
    /// Rendered compiler errors attributable to this package's mod.rs (capped at 4_000 bytes).
    pub errors: String,
}

/// Build a throwaway skeleton crate from ALL contracts and cargo-check it.
/// Returns per-package issues (empty vec = all structurally sound).
pub fn check_contracts(
    contracts: &[PackageContract],
    crate_name: &str,
) -> Result<Vec<ContractIssue>, EngineError> {
    // Nothing to check if all contracts are entrypoints (no non-entrypoint mods).
    let non_entry: Vec<&PackageContract> = contracts.iter().filter(|c| !c.is_entrypoint).collect();
    if non_entry.is_empty() {
        return Ok(Vec::new());
    }

    let tmp = tempfile::TempDir::new()
        .map_err(|e| EngineError::Io(format!("failed to create temp dir: {e}")))?;

    // Detect external deps referenced by any contract.
    let all_contract_text: String = contracts
        .iter()
        .map(|c| format!("{}\n{}", c.data_surface, c.signatures))
        .collect::<Vec<_>>()
        .join("\n");
    let missing_specs = crate::deps::scan_crate_heads_for_registry(&all_contract_text);

    // Build Cargo.toml with base deps + any detected registry deps.
    let skeleton_name = format!("{crate_name}_skeleton");
    let cargo_toml = build_skeleton_cargo_toml(&skeleton_name, &missing_specs);
    fs::write(tmp.path().join("Cargo.toml"), cargo_toml)
        .map_err(|e| EngineError::Io(format!("failed to write Cargo.toml: {e}")))?;

    // Write src/lib.rs and src/<root>/mod.rs files.
    let layout = skeleton_layout(contracts);
    for (rel_path, content) in &layout {
        let abs = tmp.path().join(rel_path);
        if let Some(parent) = abs.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| EngineError::Io(format!("failed to create dir: {e}")))?;
        }
        fs::write(&abs, content)
            .map_err(|e| EngineError::Io(format!("failed to write skeleton file: {e}")))?;
    }

    let output = run_cargo_check(tmp.path())
        .map_err(|e| EngineError::Compiler(format!("skeleton cargo check failed: {e}")))?;
    let diags = parse_cargo_diagnostics(&output)
        .map_err(|e| EngineError::Compiler(format!("failed to parse diagnostics: {e}")))?;

    let roots: Vec<String> = non_entry.iter().map(|c| c.root_segment.clone()).collect();
    Ok(attribute_issues(&diags, &roots))
}

/// Build the skeleton Cargo.toml content with base deps and any detected registry deps.
fn build_skeleton_cargo_toml(name: &str, extra_specs: &[&crate::deps::CrateSpec]) -> String {
    let mut content = format!(
        r#"[package]
name = "{name}"
version = "0.1.0"
edition = "2021"

# empty table: keep this crate out of any enclosing workspace
[workspace]

[dependencies]
serde       = {{ version = "1", features = ["derive"] }}
serde_json  = "1"
thiserror   = "1"
anyhow      = "1"
tokio       = {{ version = "1", features = ["full"] }}
reqwest     = {{ version = "0.12", features = ["json"] }}
tracing     = "0.1"
tracing-subscriber = {{ version = "0.3", features = ["env-filter"] }}
"#
    );
    for spec in extra_specs {
        content.push_str(&render_dep_line(spec));
    }
    content
}

// ---------------------------------------------------------------------------
// Pure helpers (exposed for unit tests)
// ---------------------------------------------------------------------------

/// Rewrite each `sig;`-terminated function signature into `sig { todo!() }`.
/// Non-`fn` lines pass through unchanged. Splits on `;` only at brace-depth 0.
pub fn stub_bodies(sigs: &str) -> String {
    let mut result = String::new();
    let mut current = String::new();
    let mut depth: i32 = 0;
    let mut in_str = false;
    let mut in_char = false;

    let chars: Vec<char> = sigs.chars().collect();
    let n = chars.len();
    let mut i = 0;

    while i < n {
        let c = chars[i];

        if in_str {
            current.push(c);
            if c == '\\' && i + 1 < n {
                current.push(chars[i + 1]);
                i += 2;
                continue;
            }
            if c == '"' {
                in_str = false;
            }
            i += 1;
            continue;
        }

        if in_char {
            current.push(c);
            if c == '\\' && i + 1 < n {
                current.push(chars[i + 1]);
                i += 2;
                continue;
            }
            if c == '\'' {
                in_char = false;
            }
            i += 1;
            continue;
        }

        match c {
            '"' => {
                in_str = true;
                current.push(c);
            }
            '\'' => {
                // Distinguish char literal from lifetime: char literals have
                // a closing ' within a short distance, or an escape following.
                if (i + 2 < n && chars[i + 2] == '\'') || (i + 1 < n && chars[i + 1] == '\\') {
                    in_char = true;
                }
                current.push(c);
            }
            '{' => {
                depth += 1;
                current.push(c);
            }
            '}' => {
                depth -= 1;
                current.push(c);
            }
            ';' if depth == 0 => {
                // End of a statement at depth 0 — process this segment.
                let seg = current.trim().to_string();
                if !seg.is_empty() {
                    result.push_str(&process_segment(&seg));
                    result.push('\n');
                }
                current.clear();
            }
            _ => {
                current.push(c);
            }
        }

        i += 1;
    }

    // Handle any trailing content without a final semicolon.
    let trailing = current.trim().to_string();
    if !trailing.is_empty() {
        result.push_str(&process_segment(&trailing));
        result.push('\n');
    }

    result
}

/// Process one segment: if it starts with `pub fn` or `pub async fn`, rewrite
/// `sig;` → `sig { todo!() }`. Otherwise pass through unchanged (adding back `;`).
fn process_segment(seg: &str) -> String {
    let trimmed = seg.trim();
    let is_fn = trimmed.starts_with("pub fn ")
        || trimmed.starts_with("pub async fn ")
        || trimmed.starts_with("fn ")
        || trimmed.starts_with("async fn ");

    if is_fn {
        // Strip trailing `;` if present, then add body.
        let without_semi = trimmed.trim_end_matches(';').trim_end();
        format!("{} {{ todo!() }}", without_semi)
    } else {
        // Non-fn: re-emit as-is with semicolon (it was stripped by the splitter).
        format!("{};", trimmed)
    }
}

/// Build the skeleton file layout: returns (relative-path, content) pairs.
pub fn skeleton_layout(contracts: &[PackageContract]) -> Vec<(PathBuf, String)> {
    let mut files: Vec<(PathBuf, String)> = Vec::new();

    // --- src/lib.rs ---
    let mut lib_rs = "#![allow(unused, dead_code)]\n".to_string();
    for contract in contracts {
        if !contract.is_entrypoint {
            lib_rs.push_str(&format!("pub mod {};\n", contract.root_segment));
        }
    }
    files.push((PathBuf::from("src/lib.rs"), lib_rs));

    // --- src/<root>/mod.rs per non-entrypoint contract ---
    for contract in contracts {
        if contract.is_entrypoint {
            continue;
        }
        let mut mod_content = contract.data_surface.clone();
        mod_content.push_str("\n// --- signatures ---\n");
        mod_content.push_str(&stub_bodies(&contract.signatures));
        files.push((
            PathBuf::from(format!("src/{}/mod.rs", contract.root_segment)),
            mod_content,
        ));
    }

    files
}

/// Attribute compiler diagnostics to packages by primary span path.
/// Filters out import-related error codes and lib.rs diagnostics.
/// Groups by root; caps each errors string at 4_000 bytes.
pub fn attribute_issues(diags: &[CompilerDiagnostic], roots: &[String]) -> Vec<ContractIssue> {
    let mut by_root: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();

    for diag in diags {
        // Only errors (not warnings/notes).
        if diag.level != DiagnosticLevel::Error {
            continue;
        }
        // Skip import-resolution codes.
        if let Some(ref code) = diag.code {
            if IMPORT_CODES.contains(&code.as_str()) {
                continue;
            }
        }
        // Find the primary span and check if it belongs to a known root.
        let primary_span = diag.spans.iter().find(|s| s.is_primary);
        let Some(span) = primary_span else {
            continue;
        };
        // The span file_name should be `src/<root>/mod.rs`.
        // Skip anything in src/lib.rs.
        if span.file_name == "src/lib.rs" {
            continue;
        }
        let matched_root = roots
            .iter()
            .find(|r| span.file_name == format!("src/{}/mod.rs", r));
        let Some(root) = matched_root else {
            continue;
        };

        let rendered = diag
            .rendered
            .as_deref()
            .unwrap_or(&diag.message)
            .to_string();

        let entry = by_root.entry(root.clone()).or_default();
        // Cap at 4_000 bytes.
        if entry.len() < 4_000 {
            let remaining = 4_000 - entry.len();
            if rendered.len() <= remaining {
                entry.push_str(&rendered);
            } else {
                entry.push_str(&rendered[..remaining]);
            }
            entry.push('\n');
        }
    }

    by_root
        .into_iter()
        .map(|(root_segment, errors)| ContractIssue {
            root_segment,
            errors,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Internal: dep detection for skeleton Cargo.toml
// ---------------------------------------------------------------------------

/// Render a registry crate spec as a Cargo.toml dependency line (no trailing newline).
fn render_dep_line(spec: &crate::deps::CrateSpec) -> String {
    if spec.features.is_empty() && spec.default_features {
        return format!(
            "{} = \"{}\" # [rustyfi] skeleton-dep\n",
            spec.krate, spec.version
        );
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
        "{} = {{ {} }} # [rustyfi] skeleton-dep\n",
        spec.krate,
        parts.join(", ")
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use rustyfi_core::state::{DiagnosticLevel, DiagnosticSpan};

    fn make_diag(
        level: DiagnosticLevel,
        code: Option<&str>,
        message: &str,
        rendered: Option<&str>,
        primary_file: Option<&str>,
    ) -> CompilerDiagnostic {
        let spans = if let Some(file) = primary_file {
            vec![DiagnosticSpan {
                file_name: file.to_string(),
                line_start: 1,
                line_end: 1,
                column_start: 1,
                column_end: 1,
                is_primary: true,
                label: None,
            }]
        } else {
            vec![]
        };
        CompilerDiagnostic {
            level,
            message: message.to_string(),
            code: code.map(str::to_owned),
            spans,
            rendered: rendered.map(str::to_owned),
        }
    }

    // ── B1: stub_bodies ────────────────────────────────────────────────────

    #[test]
    fn stub_bodies_rewrites_fn_signature() {
        let input = "pub fn get(&self, k: &str) -> Option<String>;";
        let output = stub_bodies(input);
        assert!(
            output.contains("pub fn get(&self, k: &str) -> Option<String> { todo!() }"),
            "got: {output:?}"
        );
        assert!(
            !output.contains("Option<String>;"),
            "should not have trailing semi: {output:?}"
        );
    }

    #[test]
    fn stub_bodies_passes_struct_through() {
        let input = "pub struct Config { pub host: String, pub port: u16 };";
        let output = stub_bodies(input);
        // Structs are not fn — should pass through with semicolon.
        assert!(
            output.contains("pub struct Config"),
            "struct should pass through: {output:?}"
        );
        assert!(
            !output.contains("todo!()"),
            "struct should not get todo!(): {output:?}"
        );
    }

    #[test]
    fn stub_bodies_handles_multiple_sigs() {
        let input = "pub fn foo() -> i32;\npub fn bar(x: String) -> bool;";
        let output = stub_bodies(input);
        assert!(
            output.contains("pub fn foo() -> i32 { todo!() }"),
            "got: {output:?}"
        );
        assert!(
            output.contains("pub fn bar(x: String) -> bool { todo!() }"),
            "got: {output:?}"
        );
    }

    #[test]
    fn stub_bodies_handles_async_fn() {
        let input = "pub async fn fetch(&self) -> Result<Vec<u8>, String>;";
        let output = stub_bodies(input);
        assert!(
            output.contains("pub async fn fetch(&self) -> Result<Vec<u8>, String> { todo!() }"),
            "got: {output:?}"
        );
    }

    // ── B1: skeleton_layout ────────────────────────────────────────────────

    #[test]
    fn skeleton_layout_includes_mod_decls_in_lib_rs() {
        let contracts = vec![
            PackageContract {
                root_segment: "storage".to_string(),
                package: "storage".to_string(),
                is_entrypoint: false,
                data_surface: "pub struct Store { pub name: String }".to_string(),
                signatures: "pub fn get(&self) -> Option<String>;".to_string(),
            },
            PackageContract {
                root_segment: "main_pkg".to_string(),
                package: "main_pkg".to_string(),
                is_entrypoint: true,
                data_surface: String::new(),
                signatures: String::new(),
            },
        ];

        let layout = skeleton_layout(&contracts);

        // lib.rs should exist and have mod decl for non-entrypoint only.
        let lib = layout
            .iter()
            .find(|(p, _)| p == &PathBuf::from("src/lib.rs"))
            .expect("lib.rs should be present");
        assert!(lib.1.contains("pub mod storage;"), "lib.rs: {}", lib.1);
        assert!(
            !lib.1.contains("pub mod main_pkg;"),
            "entrypoint leaked: {}",
            lib.1
        );

        // mod.rs for storage should exist with data_surface + stubs.
        let mod_rs = layout
            .iter()
            .find(|(p, _)| p == &PathBuf::from("src/storage/mod.rs"))
            .expect("storage/mod.rs should be present");
        assert!(
            mod_rs.1.contains("pub struct Store"),
            "mod.rs: {}",
            mod_rs.1
        );
        assert!(
            mod_rs.1.contains("// --- signatures ---"),
            "mod.rs: {}",
            mod_rs.1
        );
        assert!(
            mod_rs
                .1
                .contains("pub fn get(&self) -> Option<String> { todo!() }"),
            "mod.rs: {}",
            mod_rs.1
        );

        // No mod.rs for entrypoint.
        assert!(
            !layout
                .iter()
                .any(|(p, _)| p == &PathBuf::from("src/main_pkg/mod.rs")),
            "entrypoint should not have mod.rs"
        );
    }

    // ── B1: attribute_issues ───────────────────────────────────────────────

    #[test]
    fn attribute_issues_maps_e0038_to_storage_root() {
        let diags = vec![make_diag(
            DiagnosticLevel::Error,
            Some("E0038"),
            "the trait `Provider` is not object-safe",
            Some("error[E0038]: the trait `Provider` is not object-safe"),
            Some("src/storage/mod.rs"),
        )];
        let roots = vec!["storage".to_string()];
        let issues = attribute_issues(&diags, &roots);
        assert_eq!(issues.len(), 1, "expected exactly one issue");
        assert_eq!(issues[0].root_segment, "storage");
        assert!(
            issues[0].errors.contains("E0038"),
            "errors: {}",
            issues[0].errors
        );
    }

    #[test]
    fn attribute_issues_drops_e0433() {
        let diags = vec![make_diag(
            DiagnosticLevel::Error,
            Some("E0433"),
            "unresolved import `serde`",
            Some("error[E0433]: unresolved import"),
            Some("src/storage/mod.rs"),
        )];
        let roots = vec!["storage".to_string()];
        let issues = attribute_issues(&diags, &roots);
        assert!(
            issues.is_empty(),
            "E0433 should be filtered: {issues:?}",
            issues = issues.len()
        );
    }

    #[test]
    fn attribute_issues_drops_import_codes() {
        for code in IMPORT_CODES {
            let diags = vec![make_diag(
                DiagnosticLevel::Error,
                Some(code),
                "import error",
                None,
                Some("src/storage/mod.rs"),
            )];
            let roots = vec!["storage".to_string()];
            let issues = attribute_issues(&diags, &roots);
            assert!(
                issues.is_empty(),
                "code {code} should be filtered, got {} issue(s)",
                issues.len()
            );
        }
    }

    #[test]
    fn attribute_issues_drops_lib_rs_diagnostics() {
        let diags = vec![make_diag(
            DiagnosticLevel::Error,
            Some("E0038"),
            "object-safety error",
            Some("error[E0038]: not object safe"),
            Some("src/lib.rs"),
        )];
        let roots = vec!["storage".to_string()];
        let issues = attribute_issues(&diags, &roots);
        assert!(issues.is_empty(), "lib.rs errors should be filtered");
    }

    #[test]
    fn attribute_issues_caps_at_4000_bytes() {
        // Generate a rendered string > 4000 bytes.
        let big_rendered = "x".repeat(5000);
        let diags = vec![make_diag(
            DiagnosticLevel::Error,
            Some("E0038"),
            "big error",
            Some(&big_rendered),
            Some("src/pkg/mod.rs"),
        )];
        let roots = vec!["pkg".to_string()];
        let issues = attribute_issues(&diags, &roots);
        assert_eq!(issues.len(), 1);
        assert!(
            issues[0].errors.len() <= 4_001,
            "should be capped at 4000 + newline"
        );
    }

    #[test]
    fn attribute_issues_ignores_warnings() {
        let diags = vec![make_diag(
            DiagnosticLevel::Warning,
            Some("E0038"),
            "this is just a warning",
            None,
            Some("src/storage/mod.rs"),
        )];
        let roots = vec!["storage".to_string()];
        let issues = attribute_issues(&diags, &roots);
        assert!(issues.is_empty(), "warnings should be ignored");
    }

    // ── item_names ─────────────────────────────────────────────────────────

    #[test]
    fn item_names_collects_all_kinds() {
        let contract = r#"
pub struct Foo { pub x: i32 }
pub enum Bar { A, B }
pub trait Baz { fn baz_method(&self) -> i32; }
pub fn standalone() -> bool { true }
pub type MyAlias = String;
pub const MY_CONST: u32 = 42;
"#;
        let names = item_names(contract);
        assert!(names.contains("Foo"), "should contain Foo; got {names:?}");
        assert!(names.contains("Bar"), "should contain Bar; got {names:?}");
        assert!(names.contains("Baz"), "should contain Baz; got {names:?}");
        assert!(
            names.contains("Baz::baz_method"),
            "should contain Baz::baz_method; got {names:?}"
        );
        assert!(
            names.contains("standalone"),
            "should contain standalone; got {names:?}"
        );
        assert!(
            names.contains("MyAlias"),
            "should contain MyAlias; got {names:?}"
        );
        assert!(
            names.contains("MY_CONST"),
            "should contain MY_CONST; got {names:?}"
        );
    }

    #[test]
    fn item_names_returns_empty_on_parse_failure() {
        let names = item_names("this is not valid rust @@@@");
        assert!(
            names.is_empty(),
            "parse failure should yield empty set; got {names:?}"
        );
    }

    #[test]
    fn item_names_collects_multiple_trait_methods() {
        let contract = r#"
pub trait Provider {
    fn get(&self) -> String;
    fn set(&mut self, v: String);
}
"#;
        let names = item_names(contract);
        assert!(names.contains("Provider"), "{names:?}");
        assert!(names.contains("Provider::get"), "{names:?}");
        assert!(names.contains("Provider::set"), "{names:?}");
    }

    // ── regeneration_acceptable ────────────────────────────────────────────

    #[test]
    fn regen_empty_old_is_always_acceptable() {
        let old = BTreeSet::new();
        let new = BTreeSet::from(["Foo".to_string()]);
        assert!(
            regeneration_acceptable(&old, &new),
            "empty old should always be acceptable"
        );
    }

    #[test]
    fn regen_superset_is_acceptable() {
        let old: BTreeSet<String> = ["Foo", "Bar", "Baz"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        // new has everything old had plus extra
        let new: BTreeSet<String> = ["Foo", "Bar", "Baz", "Extra"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(
            regeneration_acceptable(&old, &new),
            "superset should be acceptable"
        );
    }

    #[test]
    fn regen_drop_1_of_20_is_acceptable() {
        // 1/20 = 5% < 10% → acceptable
        let old: BTreeSet<String> = (0..20).map(|i| format!("Item{i}")).collect();
        let mut new = old.clone();
        new.remove("Item0");
        assert_eq!(old.difference(&new).count(), 1);
        assert!(
            regeneration_acceptable(&old, &new),
            "1 of 20 dropped (5%) should be acceptable"
        );
    }

    #[test]
    fn regen_drop_3_of_20_is_rejected() {
        // 3/20 = 15% > 10% → rejected
        let old: BTreeSet<String> = (0..20).map(|i| format!("Item{i}")).collect();
        let mut new = old.clone();
        new.remove("Item0");
        new.remove("Item1");
        new.remove("Item2");
        assert_eq!(old.difference(&new).count(), 3);
        assert!(
            !regeneration_acceptable(&old, &new),
            "3 of 20 dropped (15%) should be rejected"
        );
    }

    #[test]
    fn regen_exactly_at_10_percent_is_acceptable() {
        // 1 of 10 = exactly 10% → acceptable (threshold is strict >10%)
        let old: BTreeSet<String> = (0..10).map(|i| format!("Item{i}")).collect();
        let mut new = old.clone();
        new.remove("Item0");
        assert_eq!(old.difference(&new).count(), 1);
        assert!(
            regeneration_acceptable(&old, &new),
            "1 of 10 (exactly 10%) should be acceptable"
        );
    }

    // ── B3: e2e test (requires cargo) ─────────────────────────────────────

    #[test]
    #[ignore]
    fn check_contracts_bad_pkg_fails_good_pkg_passes() {
        // pkg "bad": has a trait with a generic method (dyn-incompatible) and a
        // signature `pub fn make() -> Box<dyn Provider>;`
        let bad_contract = PackageContract {
            root_segment: "bad".to_string(),
            package: "bad".to_string(),
            is_entrypoint: false,
            data_surface: "pub trait Provider { fn get<T>(&self) -> T; }\npub struct S;"
                .to_string(),
            signatures: "pub fn make() -> Box<dyn Provider>;".to_string(),
        };

        // pkg "good": plain struct + plain sig (no dyn issues)
        let good_contract = PackageContract {
            root_segment: "good".to_string(),
            package: "good".to_string(),
            is_entrypoint: false,
            data_surface: "pub struct Config { pub name: String }".to_string(),
            signatures: "pub fn new(name: String) -> Config;".to_string(),
        };

        let contracts = vec![bad_contract, good_contract];
        let issues = check_contracts(&contracts, "test_skeleton").expect("check_contracts failed");

        // Should have exactly one issue for "bad".
        let bad_issues: Vec<_> = issues.iter().filter(|i| i.root_segment == "bad").collect();
        let good_issues: Vec<_> = issues.iter().filter(|i| i.root_segment == "good").collect();

        assert_eq!(
            bad_issues.len(),
            1,
            "expected exactly 1 issue for 'bad', got {}: {:?}",
            bad_issues.len(),
            bad_issues.iter().map(|i| &i.errors).collect::<Vec<_>>()
        );
        assert!(
            good_issues.is_empty(),
            "expected no issues for 'good', got: {:?}",
            good_issues.iter().map(|i| &i.errors).collect::<Vec<_>>()
        );
    }
}
