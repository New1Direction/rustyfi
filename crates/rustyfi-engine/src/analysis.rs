use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use rustyfi_core::context::{
    DependencyEdge, LanguageMetadata, ParserWarning, SourceLanguage, SourceTarget, WarningSeverity,
};
use tracing::debug;
use walkdir::WalkDir;

use crate::EngineError;

/// Analyses a source directory and produces the raw materials needed to
/// construct a `ContextManifest`.
pub struct SourceAnalyser {
    pub workspace_root: PathBuf,
}

impl SourceAnalyser {
    pub fn new(workspace_root: PathBuf) -> Self {
        Self { workspace_root }
    }

    /// Walk the directory, classify files, and return analysis results.
    pub fn analyse(&self) -> Result<AnalysisResult, EngineError> {
        let mut targets = Vec::new();
        let mut warnings = Vec::new();
        let mut language_counts: HashMap<String, usize> = HashMap::new();

        let skip_dirs = [
            "node_modules",
            ".git",
            "__pycache__",
            "__MACOSX",
            ".venv",
            "venv",
            "dist",
            "build",
            ".next",
            "target",
            ".tox",
            ".pytest_cache",
        ];

        for entry in WalkDir::new(&self.workspace_root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| {
                // depth 0 is the workspace root itself — never filter it,
                // even if the directory happens to have a dotted name.
                if e.depth() == 0 {
                    return true;
                }
                let name = e.file_name().to_string_lossy();
                // Skip hidden entries and macOS AppleDouble files (`._foo.py`)
                // which would otherwise be detected as source files.
                !name.starts_with('.') && !skip_dirs.iter().any(|s| *s == name.as_ref())
            })
        {
            let entry = match entry {
                Ok(e) => e,
                Err(err) => {
                    warnings.push(ParserWarning {
                        file: PathBuf::from(err.path().unwrap_or(Path::new("?"))),
                        line: None,
                        message: format!("Walk error: {err}"),
                        severity: WarningSeverity::Low,
                    });
                    continue;
                }
            };

            if !entry.file_type().is_file() {
                continue;
            }

            // Skip large binary / lock files.
            let path = entry.path().to_path_buf();
            let meta = fs::metadata(&path).unwrap_or_else(|_| {
                fs::metadata(&self.workspace_root).expect("workspace root must be stat-able")
            });
            let size = meta.len();
            if size > 500_000 {
                debug!("Skipping large file: {}", path.display());
                continue;
            }

            let lang = detect_language(&path);
            if lang.is_none() {
                continue; // skip non-source files
            }
            let language = lang.unwrap();

            // Count hash for primary language detection.
            let key = language_key(&language);
            *language_counts.entry(key).or_insert(0) += 1;

            // SHA-256 via raw stdlib (no external dep).
            let content = match fs::read(&path) {
                Ok(b) => b,
                Err(e) => {
                    warnings.push(ParserWarning {
                        file: path.clone(),
                        line: None,
                        message: format!("Cannot read file: {e}"),
                        severity: WarningSeverity::High,
                    });
                    continue;
                }
            };
            let content_hash = hex_sha256(&content);

            let is_entrypoint = is_likely_entrypoint(&path, &language);

            targets.push(SourceTarget {
                path: path.clone(),
                language,
                size_bytes: size,
                content_hash,
                is_entrypoint,
            });
        }

        if targets.is_empty() {
            return Err(EngineError::NoSourceFiles {
                path: self.workspace_root.clone(),
            });
        }

        // Determine primary language.
        let primary_lang_key = language_counts
            .iter()
            .max_by_key(|(_, c)| *c)
            .map(|(k, _)| k.clone())
            .unwrap_or_else(|| "python".to_string());

        let primary_language = key_to_language(&primary_lang_key);

        // Infer simple dependency edges from import statements.
        let dependency_edges = infer_edges(&targets, &self.workspace_root);

        let language_metadata = LanguageMetadata {
            primary_language: primary_language.clone(),
            runtime_version: None,
            package_manager: detect_package_manager(&self.workspace_root),
            is_dynamically_typed: is_dynamic(&primary_language),
            extra: HashMap::new(),
        };

        let inferred_entrypoints: Vec<PathBuf> = targets
            .iter()
            .filter(|t| t.is_entrypoint)
            .map(|t| t.path.clone())
            .collect();

        Ok(AnalysisResult {
            targets,
            dependency_edges,
            language_metadata,
            inferred_entrypoints,
            warnings,
        })
    }
}

// ---------------------------------------------------------------------------
// Public output type
// ---------------------------------------------------------------------------

pub struct AnalysisResult {
    pub targets: Vec<SourceTarget>,
    pub dependency_edges: Vec<DependencyEdge>,
    pub language_metadata: LanguageMetadata,
    pub inferred_entrypoints: Vec<PathBuf>,
    pub warnings: Vec<ParserWarning>,
}

// ---------------------------------------------------------------------------
// Language detection
// ---------------------------------------------------------------------------

fn detect_language(path: &Path) -> Option<SourceLanguage> {
    let ext = path.extension()?.to_string_lossy().to_lowercase();
    match ext.as_str() {
        "py" => Some(SourceLanguage::Python),
        "ts" | "tsx" | "mts" => Some(SourceLanguage::TypeScript),
        "js" | "mjs" | "cjs" | "jsx" => Some(SourceLanguage::JavaScript),
        "go" => Some(SourceLanguage::Go),
        "cpp" | "cxx" | "cc" | "c++" => Some(SourceLanguage::Cpp),
        "c" | "h" => Some(SourceLanguage::C),
        "java" => Some(SourceLanguage::Java),
        "cs" => Some(SourceLanguage::CSharp),
        "rb" => Some(SourceLanguage::Ruby),
        _ => None,
    }
}

fn language_key(lang: &SourceLanguage) -> String {
    match lang {
        SourceLanguage::Python => "python",
        SourceLanguage::TypeScript => "typescript",
        SourceLanguage::JavaScript => "javascript",
        SourceLanguage::Go => "go",
        SourceLanguage::Cpp => "cpp",
        SourceLanguage::C => "c",
        SourceLanguage::Java => "java",
        SourceLanguage::CSharp => "csharp",
        SourceLanguage::Ruby => "ruby",
        SourceLanguage::Other(s) => return s.clone(),
    }
    .to_string()
}

fn key_to_language(key: &str) -> SourceLanguage {
    match key {
        "python" => SourceLanguage::Python,
        "typescript" => SourceLanguage::TypeScript,
        "javascript" => SourceLanguage::JavaScript,
        "go" => SourceLanguage::Go,
        "cpp" => SourceLanguage::Cpp,
        "c" => SourceLanguage::C,
        "java" => SourceLanguage::Java,
        "csharp" => SourceLanguage::CSharp,
        "ruby" => SourceLanguage::Ruby,
        other => SourceLanguage::Other(other.to_string()),
    }
}

fn is_dynamic(lang: &SourceLanguage) -> bool {
    matches!(
        lang,
        SourceLanguage::Python | SourceLanguage::JavaScript | SourceLanguage::Ruby
    )
}

fn is_likely_entrypoint(path: &Path, lang: &SourceLanguage) -> bool {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    match lang {
        SourceLanguage::Python => {
            stem == "main" || stem == "__main__" || stem == "app" || stem == "run"
        }
        SourceLanguage::Go => stem == "main",
        SourceLanguage::JavaScript | SourceLanguage::TypeScript => {
            stem == "index" || stem == "main" || stem == "server" || stem == "app"
        }
        SourceLanguage::Java => stem == "main" || stem == "app" || stem == "application",
        _ => stem == "main",
    }
}

// ---------------------------------------------------------------------------
// Simple import edge inference (line scanning, no AST)
// ---------------------------------------------------------------------------

fn infer_edges(targets: &[SourceTarget], root: &Path) -> Vec<DependencyEdge> {
    use std::collections::HashSet;
    let known: HashSet<&Path> = targets.iter().map(|t| t.path.as_path()).collect();

    let mut edges = Vec::new();
    for target in targets {
        let Ok(text) = std::fs::read_to_string(&target.path) else {
            continue;
        };
        for line in text.lines().take(100) {
            let trimmed = line.trim();
            if let Some(symbol) = extract_import(trimmed, &target.language) {
                // Only record edges that resolve to a real file in this
                // project — external packages (requests, lodash, …) would
                // otherwise pollute the DAG with phantom nodes.
                let Some(to) = resolve_import(&symbol, target, root, &known) else {
                    continue;
                };
                if to == target.path {
                    continue; // self-import, ignore
                }
                edges.push(DependencyEdge {
                    from: target.path.clone(),
                    to,
                    import_symbol: symbol,
                    is_internal: true,
                });
            }
        }
    }
    edges
}

/// Resolve an import symbol to an actual source file in the project.
/// Conservative: returns `None` for anything that doesn't map to a known file.
fn resolve_import(
    symbol: &str,
    target: &SourceTarget,
    root: &Path,
    known: &std::collections::HashSet<&Path>,
) -> Option<PathBuf> {
    let file_dir = target.path.parent().unwrap_or(root);
    let pick =
        |candidates: Vec<PathBuf>| candidates.into_iter().find(|c| known.contains(c.as_path()));

    match &target.language {
        SourceLanguage::Python => {
            // `import a.b` / `from a.b import x` → a/b.py or a/b/__init__.py,
            // tried relative to the file's directory and the project root.
            // `from . import x` anchors at the file's dir; each extra leading
            // dot climbs one parent (`..pkg` → ../pkg).
            let rel = symbol.trim_start_matches('.');
            let as_path = rel.replace('.', "/");
            let bases = if symbol.starts_with('.') {
                let dots = symbol.len() - rel.len();
                let mut base = file_dir.to_path_buf();
                for _ in 1..dots {
                    base.pop();
                }
                vec![base]
            } else {
                vec![file_dir.to_path_buf(), root.to_path_buf()]
            };
            let mut candidates = Vec::new();
            for base in bases {
                candidates.push(base.join(format!("{as_path}.py")));
                candidates.push(base.join(&as_path).join("__init__.py"));
            }
            pick(candidates)
        }
        SourceLanguage::JavaScript | SourceLanguage::TypeScript => {
            // Only relative specifiers can be project files.
            if !symbol.starts_with('.') {
                return None;
            }
            let base = file_dir.join(symbol);
            let mut candidates = vec![base.clone()];
            for ext in ["ts", "tsx", "mts", "js", "mjs", "cjs", "jsx"] {
                candidates.push(PathBuf::from(format!("{}.{ext}", base.display())));
                candidates.push(base.join(format!("index.{ext}")));
            }
            // Normalise `a/./b` and `a/x/../b` segments so set lookup works.
            pick(candidates.into_iter().map(normalise_path).collect())
        }
        SourceLanguage::C | SourceLanguage::Cpp => {
            // `#include "foo.h"` → sibling or root-relative file.
            pick(
                vec![file_dir.join(symbol), root.join(symbol)]
                    .into_iter()
                    .map(normalise_path)
                    .collect(),
            )
        }
        _ => None,
    }
}

/// Resolve `.` and `..` components lexically (no filesystem access).
fn normalise_path(p: PathBuf) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

fn extract_import(line: &str, lang: &SourceLanguage) -> Option<String> {
    match lang {
        SourceLanguage::Python => {
            if line.starts_with("import ") {
                Some(
                    line.strip_prefix("import ")?
                        .split_whitespace()
                        .next()?
                        .to_string(),
                )
            } else if line.starts_with("from ") {
                Some(
                    line.strip_prefix("from ")?
                        .split_whitespace()
                        .next()?
                        .to_string(),
                )
            } else {
                None
            }
        }
        SourceLanguage::JavaScript | SourceLanguage::TypeScript => {
            if line.contains("require(") || line.starts_with("import ") {
                let start = line.find('"').or_else(|| line.find('\''))?;
                let rest = &line[start + 1..];
                let end = rest.find('"').or_else(|| rest.find('\''))?;
                Some(rest[..end].to_string())
            } else {
                None
            }
        }
        SourceLanguage::Go => {
            if line.starts_with('"') && line.ends_with('"') {
                Some(line.trim_matches('"').to_string())
            } else {
                None
            }
        }
        SourceLanguage::C | SourceLanguage::Cpp => {
            // Only quoted includes — angle brackets are system headers.
            let rest = line.strip_prefix("#include")?.trim_start();
            let inner = rest.strip_prefix('"')?;
            let end = inner.find('"')?;
            Some(inner[..end].to_string())
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Package manager detection
// ---------------------------------------------------------------------------

fn detect_package_manager(root: &Path) -> Option<String> {
    let markers = [
        ("requirements.txt", "pip"),
        ("pyproject.toml", "pip/poetry"),
        ("Pipfile", "pipenv"),
        ("package.json", "npm/yarn"),
        ("go.mod", "go modules"),
        ("Gemfile", "bundler"),
        ("pom.xml", "maven"),
        ("build.gradle", "gradle"),
    ];
    for (file, name) in &markers {
        if root.join(file).exists() {
            return Some((*name).to_string());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Minimal SHA-256 via std (no external dep)
// We use a stable pure-Rust approach: encode as hex of a simple hash.
// For production this would use the `sha2` crate; here we use a FNV-inspired
// 64-bit XOR accumulation formatted as hex — sufficient for content change
// detection, NOT cryptographic use.
// ---------------------------------------------------------------------------

fn hex_sha256(data: &[u8]) -> String {
    // FNV-1a 64-bit, good enough for content-change fingerprinting.
    let mut hash: u64 = 14695981039346656037u64;
    for &byte in data {
        hash ^= u64(byte);
        hash = hash.wrapping_mul(1099511628211);
    }
    format!("{hash:016x}")
}

#[allow(non_snake_case)]
fn u64(b: u8) -> u64 {
    b as u64
}
