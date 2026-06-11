/// Semantic Chunker — splits source files into token-budget-bounded units.
///
/// ## The problem
/// Large files (>400 lines, >6 k tokens) exceed practical LLM context budgets
/// when combined with system prompts, dependency context, and expected Rust
/// output.  Naïve line-splitting breaks functions across chunks, destroying
/// semantic coherence and making the fix loop nearly impossible.
///
/// ## The solution
/// Detect top-level semantic *boundaries* (function/class/etc. definitions)
/// using language-specific line patterns, then greedily bin symbols into
/// chunks that stay under `max_tokens`.  A chunk always contains complete
/// top-level definitions — never a partial function or class.
///
/// ## Token budget strategy
/// - Estimate: 1 token ≈ 4 chars (GPT-4 / Gemini rough average).
/// - Default budget: 5 000 tokens per chunk (conservative — leaves room for
///   the system prompt, dependency context injection, and response).
/// - Files under budget are returned as a single chunk (common case).
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A translatable semantic unit: a contiguous slice of a source file that
/// contains one or more complete top-level definitions and fits within the
/// configured token budget.
#[derive(Debug, Clone)]
pub struct SemanticChunk {
    /// Absolute path of the source file this chunk comes from.
    pub source_file: PathBuf,
    /// 0-based index of this chunk within the file (0 if file is not split).
    pub chunk_index: usize,
    /// Total number of chunks this file was split into.
    pub total_chunks: usize,
    /// Source code content of this chunk (complete, compilable snippet).
    pub content: String,
    /// Top-level symbol names (functions, classes, etc.) contained here.
    pub symbol_names: Vec<String>,
    /// Estimated token count (chars / 4).
    pub token_estimate: usize,
    /// First line number in the original file (1-based, inclusive).
    pub line_start: usize,
    /// Last line number in the original file (1-based, inclusive).
    pub line_end: usize,
}

impl SemanticChunk {
    /// Whether this chunk is the first in its file.
    pub fn is_first(&self) -> bool {
        self.chunk_index == 0
    }

    /// Whether this chunk is the last in its file.
    pub fn is_last(&self) -> bool {
        self.chunk_index + 1 == self.total_chunks
    }
}

// ---------------------------------------------------------------------------
// Boundary detection
// ---------------------------------------------------------------------------

/// A detected top-level semantic boundary inside a source file.
#[derive(Debug, Clone)]
struct Boundary {
    /// 0-based line index.
    line_idx: usize,
    /// Human-readable name of the symbol starting here.
    symbol_name: Option<String>,
    /// Estimated token contribution of *all lines from this boundary to the
    /// next one* (inclusive of the boundary line itself).
    token_span: usize,
}

/// Language-specific boundary patterns.
/// Returns `Some(symbol_name)` when the line starts a top-level definition.
fn detect_boundary(line: &str, lang: &str) -> Option<String> {
    let trimmed = line.trim();

    match lang {
        "python" => {
            for prefix in ["async def ", "def ", "class "] {
                if let Some(rest) = trimmed.strip_prefix(prefix) {
                    let name = rest
                        .split('(')
                        .next()
                        .unwrap_or("?")
                        .trim_end_matches(':')
                        .trim()
                        .to_string();
                    return Some(name);
                }
            }
            // Module-level assignments that look like constants.
            if trimmed.starts_with(|c: char| c.is_uppercase())
                && trimmed.contains('=')
                && !trimmed.starts_with('#')
            {
                let name = trimmed.split('=').next().unwrap_or("?").trim().to_string();
                return Some(name);
            }
            None
        }

        "typescript" | "javascript" => {
            for prefix in [
                "export async function ",
                "export function ",
                "async function ",
                "function ",
                "export class ",
                "class ",
                "export const ",
                "export let ",
                "export default function ",
                "export default class ",
                "export type ",
                "export interface ",
                "const ",
                "let ",
            ] {
                if let Some(rest) = trimmed.strip_prefix(prefix) {
                    let name = rest
                        .split(['(', '<', '=', ' '])
                        .next()
                        .unwrap_or("?")
                        .trim()
                        .to_string();
                    if !name.is_empty() {
                        return Some(name);
                    }
                }
            }
            None
        }

        "go" => {
            for prefix in ["func ", "type ", "var ", "const "] {
                if let Some(rest) = trimmed.strip_prefix(prefix) {
                    let name = rest
                        .split(['(', ' '])
                        .next()
                        .unwrap_or("?")
                        .trim()
                        .to_string();
                    return Some(name);
                }
            }
            None
        }

        "java" | "csharp" => {
            // Simple: any non-indented line that looks like a declaration.
            if !line.starts_with(' ')
                && !line.starts_with('\t')
                && !trimmed.is_empty()
                && !trimmed.starts_with("//")
                && !trimmed.starts_with("import ")
                && !trimmed.starts_with("package ")
                && !trimmed.starts_with("using ")
                && !trimmed.starts_with("namespace ")
                && (trimmed.contains("class ")
                    || trimmed.contains("interface ")
                    || trimmed.contains("enum ")
                    || (trimmed.contains('(') && trimmed.contains(')') && !trimmed.contains(';')))
            {
                // Best-effort: grab identifier after last space before '('
                let name = trimmed
                    .split_whitespace()
                    .find(|w| {
                        w.chars()
                            .next()
                            .is_some_and(|c| c.is_alphabetic() && !w.contains('('))
                    })
                    .unwrap_or("?")
                    .to_string();
                return Some(name);
            }
            None
        }

        "c" | "cpp" => {
            // Non-indented line that looks like a function or type definition.
            if !line.starts_with(' ')
                && !line.starts_with('\t')
                && !trimmed.is_empty()
                && !trimmed.starts_with("//")
                && !trimmed.starts_with('#')
                && !trimmed.starts_with("typedef")
                && (trimmed.contains('(')
                    || trimmed.starts_with("struct ")
                    || trimmed.starts_with("class ")
                    || trimmed.starts_with("enum "))
            {
                let name = trimmed
                    .split(['(', ' '])
                    .find(|w| !w.is_empty() && w.chars().all(|c| c.is_alphanumeric() || c == '_'))
                    .unwrap_or("?")
                    .to_string();
                return Some(name);
            }
            None
        }

        "ruby" => {
            for prefix in ["def ", "class ", "module "] {
                if let Some(rest) = trimmed.strip_prefix(prefix) {
                    let name = rest.split_whitespace().next().unwrap_or("?").to_string();
                    return Some(name);
                }
            }
            None
        }

        _ => None,
    }
}

// ---------------------------------------------------------------------------
// SemanticChunker
// ---------------------------------------------------------------------------

/// Splits a source file into [`SemanticChunk`]s that fit within a token budget.
pub struct SemanticChunker {
    /// Maximum number of tokens per chunk.
    pub max_tokens: usize,
}

impl Default for SemanticChunker {
    fn default() -> Self {
        Self { max_tokens: 5_000 }
    }
}

impl SemanticChunker {
    pub fn new(max_tokens: usize) -> Self {
        Self { max_tokens }
    }

    /// Split `source` into [`SemanticChunk`]s using language `lang`.
    ///
    /// If the entire file fits within the budget, a single chunk is returned.
    /// If no semantic boundaries are detected (unknown language, binary-like
    /// content), the file is split by raw line count into equal-ish pieces.
    pub fn chunk(&self, source: &Path, content: &str, lang: &str) -> Vec<SemanticChunk> {
        let lines: Vec<&str> = content.lines().collect();
        let total_tokens = estimate_tokens(content);

        // Fast path: fits in one chunk.
        if total_tokens <= self.max_tokens {
            return vec![SemanticChunk {
                source_file: source.to_path_buf(),
                chunk_index: 0,
                total_chunks: 1,
                content: content.to_string(),
                symbol_names: extract_all_symbols(content, lang),
                token_estimate: total_tokens,
                line_start: 1,
                line_end: lines.len().max(1),
            }];
        }

        // Detect boundaries.
        let boundaries = self.detect_boundaries(&lines, lang);
        if boundaries.is_empty() {
            return self.split_by_lines(source, content, &lines);
        }

        // Greedily bin boundaries into chunks.
        self.bin_boundaries(source, &lines, &boundaries, content)
    }

    /// Detect semantic boundaries and compute per-span token estimates.
    fn detect_boundaries(&self, lines: &[&str], lang: &str) -> Vec<Boundary> {
        let mut boundaries: Vec<Boundary> = vec![];

        for (i, &line) in lines.iter().enumerate() {
            if let Some(name) = detect_boundary(line, lang) {
                boundaries.push(Boundary {
                    line_idx: i,
                    symbol_name: Some(name),
                    token_span: 0, // computed below
                });
            }
        }

        if boundaries.is_empty() {
            return vec![];
        }

        // Compute token_span for each boundary (lines from here to next boundary).
        let n = boundaries.len();
        for i in 0..n {
            let start = boundaries[i].line_idx;
            let end = if i + 1 < n {
                boundaries[i + 1].line_idx
            } else {
                lines.len()
            };
            let span_content: String = lines[start..end].join("\n");
            boundaries[i].token_span = estimate_tokens(&span_content);
        }

        boundaries
    }

    /// Greedily group boundaries into chunks within `max_tokens`.
    fn bin_boundaries(
        &self,
        source: &Path,
        lines: &[&str],
        boundaries: &[Boundary],
        _content: &str,
    ) -> Vec<SemanticChunk> {
        let mut chunks: Vec<SemanticChunk> = vec![];
        let mut current_start_boundary = 0usize;
        let mut current_tokens = 0usize;
        let mut current_names: Vec<String> = vec![];

        for (idx, b) in boundaries.iter().enumerate() {
            if current_tokens + b.token_span > self.max_tokens && !current_names.is_empty() {
                // Flush current chunk.
                let line_start = boundaries[current_start_boundary].line_idx;
                let line_end = b.line_idx.saturating_sub(1);
                let chunk_content = lines[line_start..=line_end].join("\n");
                chunks.push(SemanticChunk {
                    source_file: source.to_path_buf(),
                    chunk_index: chunks.len(),
                    total_chunks: 0, // filled in below
                    content: chunk_content,
                    symbol_names: std::mem::take(&mut current_names),
                    token_estimate: current_tokens,
                    line_start: line_start + 1,
                    line_end: line_end + 1,
                });
                current_start_boundary = idx;
                current_tokens = 0;
            }
            current_tokens += b.token_span;
            if let Some(ref name) = b.symbol_name {
                current_names.push(name.clone());
            }
        }

        // Flush final chunk.
        if !current_names.is_empty() || current_tokens > 0 {
            let line_start = boundaries[current_start_boundary].line_idx;
            let line_end = lines.len().saturating_sub(1);
            let chunk_content = lines[line_start..=line_end].join("\n");
            chunks.push(SemanticChunk {
                source_file: source.to_path_buf(),
                chunk_index: chunks.len(),
                total_chunks: 0,
                content: chunk_content,
                symbol_names: current_names,
                token_estimate: current_tokens,
                line_start: line_start + 1,
                line_end: line_end + 1,
            });
        }

        // Prepend any lines before the first boundary (imports, module docstring, etc.).
        let preamble_end = boundaries.first().map(|b| b.line_idx).unwrap_or(0);
        if preamble_end > 0 {
            let preamble = lines[..preamble_end].join("\n");
            let preamble_tokens = estimate_tokens(&preamble);
            // If preamble fits in first chunk, prepend it.
            if !chunks.is_empty() && chunks[0].token_estimate + preamble_tokens <= self.max_tokens {
                let first = &mut chunks[0];
                first.content = format!("{preamble}\n{}", first.content);
                first.token_estimate += preamble_tokens;
                first.line_start = 1;
            } else {
                // Otherwise it's its own chunk.
                chunks.insert(
                    0,
                    SemanticChunk {
                        source_file: source.to_path_buf(),
                        chunk_index: 0,
                        total_chunks: 0,
                        content: preamble,
                        symbol_names: vec!["<preamble>".into()],
                        token_estimate: preamble_tokens,
                        line_start: 1,
                        line_end: preamble_end,
                    },
                );
            }
        }

        // Back-fill chunk_index and total_chunks.
        let total = chunks.len();
        for (i, c) in chunks.iter_mut().enumerate() {
            c.chunk_index = i;
            c.total_chunks = total;
        }

        chunks
    }

    /// Fallback: split by raw line count when no boundaries are found.
    fn split_by_lines(&self, source: &Path, _content: &str, lines: &[&str]) -> Vec<SemanticChunk> {
        let lines_per_chunk = ((self.max_tokens * 4) / 80).max(50); // ~80 chars/line
        let chunks_raw: Vec<Vec<&str>> =
            lines.chunks(lines_per_chunk).map(|c| c.to_vec()).collect();
        let total = chunks_raw.len();
        chunks_raw
            .into_iter()
            .enumerate()
            .map(|(i, group)| {
                let content = group.join("\n");
                let tokens = estimate_tokens(&content);
                SemanticChunk {
                    source_file: source.to_path_buf(),
                    chunk_index: i,
                    total_chunks: total,
                    symbol_names: vec![format!("lines {}", i * lines_per_chunk + 1)],
                    token_estimate: tokens,
                    line_start: i * lines_per_chunk + 1,
                    line_end: (i + 1) * lines_per_chunk,
                    content,
                }
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Estimate token count: 1 token ≈ 4 chars (GPT/Gemini average).
pub fn estimate_tokens(text: &str) -> usize {
    text.len().div_ceil(4)
}

/// Extract all top-level symbol names from a file (used for single-chunk files).
fn extract_all_symbols(content: &str, lang: &str) -> Vec<String> {
    content
        .lines()
        .filter_map(|line| detect_boundary(line, lang))
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn p() -> PathBuf {
        PathBuf::from("test.py")
    }

    #[test]
    fn small_file_is_single_chunk() {
        let src = "def foo():\n    pass\n\ndef bar():\n    return 1\n";
        let chunker = SemanticChunker::new(5_000);
        let chunks = chunker.chunk(&p(), src, "python");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].chunk_index, 0);
        assert_eq!(chunks[0].total_chunks, 1);
    }

    #[test]
    fn python_symbols_extracted() {
        let src = "def alpha(): pass\nclass Beta:\n    pass\nasync def gamma(): pass\n";
        let chunker = SemanticChunker::new(5_000);
        let chunks = chunker.chunk(&p(), src, "python");
        let names: Vec<_> = chunks.iter().flat_map(|c| c.symbol_names.clone()).collect();
        assert!(names.contains(&"alpha".to_string()));
        assert!(names.contains(&"Beta".to_string()));
        assert!(names.contains(&"gamma".to_string()));
    }

    #[test]
    fn large_file_splits_into_multiple_chunks() {
        // Create a synthetic large file: 100 small functions, each ~40 chars = ~10 tokens.
        // Budget = 50 tokens → expect ~20 chunks.
        let src: String = (0..100)
            .map(|i| format!("def fn_{i}():\n    pass\n\n"))
            .collect();
        let chunker = SemanticChunker::new(50);
        let chunks = chunker.chunk(&p(), &src, "python");
        assert!(
            chunks.len() > 1,
            "Expected multiple chunks, got {}",
            chunks.len()
        );
        // All chunks have consistent total_chunks.
        let total = chunks[0].total_chunks;
        for c in &chunks {
            assert_eq!(c.total_chunks, total);
        }
    }

    #[test]
    fn chunk_indices_are_contiguous() {
        let src: String = (0..50)
            .map(|i| format!("def f_{i}():\n    pass\n\n"))
            .collect();
        let chunker = SemanticChunker::new(100);
        let chunks = chunker.chunk(&p(), &src, "python");
        for (i, c) in chunks.iter().enumerate() {
            assert_eq!(c.chunk_index, i);
        }
    }

    #[test]
    fn go_boundary_detected() {
        let src =
            "package main\n\nfunc Hello() string {\n    return \"hi\"\n}\n\nfunc main() {\n}\n";
        let chunker = SemanticChunker::new(5_000);
        let chunks = chunker.chunk(&PathBuf::from("main.go"), src, "go");
        let names: Vec<_> = chunks.iter().flat_map(|c| c.symbol_names.clone()).collect();
        assert!(names
            .iter()
            .any(|n| n.contains("Hello") || n.contains("main")));
    }

    #[test]
    fn estimate_tokens_sanity() {
        // "hello" = 5 chars → 1-2 tokens
        assert!(estimate_tokens("hello world") <= 3);
        // 400-char text → ~100 tokens
        let text = "a".repeat(400);
        assert_eq!(estimate_tokens(&text), 100);
    }
}
