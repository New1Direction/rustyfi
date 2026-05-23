use serde_json::json;
use tracing::{debug, warn};

use crate::EngineError;

// ---------------------------------------------------------------------------
// LLM client — synchronous blocking HTTP via reqwest::blocking
// ---------------------------------------------------------------------------

/// A minimal blocking Gemini/OpenAI-compatible LLM client.
///
/// Supports any OpenAI-compatible endpoint.  Configured via environment
/// variables so no secrets appear in source.
pub struct LlmClient {
    client: reqwest::blocking::Client,
    api_key: String,
    base_url: String,
    model: String,
}

impl LlmClient {
    /// Construct from environment.
    ///
    /// Reads:
    /// * `RUSTYFI_LLM_API_KEY`   — API key (required)
    /// * `RUSTYFI_LLM_BASE_URL`  — Base URL (default: Gemini OpenAI compat)
    /// * `RUSTYFI_LLM_MODEL`     — Model name (default: gemini-2.0-flash)
    pub fn from_env() -> Result<Self, EngineError> {
        let api_key = std::env::var("RUSTYFI_LLM_API_KEY").map_err(|_| {
            EngineError::Config("RUSTYFI_LLM_API_KEY environment variable not set".into())
        })?;

        let base_url = std::env::var("RUSTYFI_LLM_BASE_URL").unwrap_or_else(|_| {
            "https://generativelanguage.googleapis.com/v1beta/openai".to_string()
        });

        let model = std::env::var("RUSTYFI_LLM_MODEL")
            .unwrap_or_else(|_| "gemini-2.0-flash".to_string());

        Ok(Self {
            client: reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(120))
                .build()
                .map_err(|e| EngineError::Config(e.to_string()))?,
            api_key,
            base_url,
            model,
        })
    }

    /// Send a single-turn completion request and return the assistant text.
    pub fn complete(&self, system: &str, user: &str) -> Result<String, EngineError> {
        let url = format!("{}/chat/completions", self.base_url);

        let body = json!({
            "model": self.model,
            "temperature": 0.1,
            "max_tokens": 16384,
            "messages": [
                { "role": "system", "content": system },
                { "role": "user",   "content": user   }
            ]
        });

        debug!("POST {} (model={})", url, self.model);

        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .map_err(|e| EngineError::Llm(format!("HTTP error: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().unwrap_or_default();
            return Err(EngineError::Llm(format!(
                "LLM HTTP {status}: {body}"
            )));
        }

        let val: serde_json::Value = resp
            .json()
            .map_err(|e| EngineError::Llm(format!("JSON parse error: {e}")))?;

        let content = val
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|a| a.first())
            .and_then(|c| c.get("message"))
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string();

        if content.is_empty() {
            warn!("LLM returned empty content; full response: {val}");
        }

        Ok(content)
    }
}

// ---------------------------------------------------------------------------
// Prompt builders
// ---------------------------------------------------------------------------

pub const SYSTEM_TRANSLATE: &str = r#"You are an expert Rust systems programmer and code translator.
Your task is to translate source code from another language into idiomatic, production-quality Rust.

Rules:
1. Output ONLY valid Rust source code — no markdown, no explanation, no comments outside the code.
2. The output must be a single complete Rust file (crate root or module).
3. Use idiomatic Rust: ownership, borrowing, error propagation with `?`, `Result<T, E>`.
4. Replace dynamic typing with strong types. Replace exceptions with `thiserror` error enums.
5. Replace runtime reflection / eval with compile-time constructs wherever possible.
6. External library calls: map to equivalent crates (see mapping below).
7. If a construct cannot be directly mapped, emit a `todo!("reason")` placeholder.
8. Include all necessary `use` statements at the top.
9. Add a `Cargo.toml` dependencies comment block at the very top as:
   // [DEPS] crate = "version", crate2 = "version"

Language → Rust crate mapping:
- requests / httpx / fetch / axios → reqwest
- flask / fastapi / express / koa → axum
- sqlalchemy / sqlite3 / pg → sqlx
- redis-py / ioredis → redis
- numpy / torch tensors → ndarray
- json / JSON.parse → serde_json
- os.path / path / fs → std::path, std::fs
- datetime / Date → chrono
- argparse / yargs → clap
- logging / winston → tracing
- pydantic / zod → serde + validator
- asyncio / async/await → tokio
"#;

pub fn prompt_translate(source_code: &str, source_lang: &str, file_name: &str) -> String {
    prompt_translate_with_context(source_code, source_lang, file_name, 0, 1, "", &[])
}

/// Context-aware translation prompt.
///
/// Injects:
/// - `rust_context`: Rust signatures of dependency files already translated.
/// - `chunk_meta`: chunk position within the file (e.g. "chunk 2/4").
/// - `symbol_names`: names of top-level symbols in this chunk.
pub fn prompt_translate_with_context(
    source_code: &str,
    source_lang: &str,
    file_name: &str,
    chunk_index: usize,
    total_chunks: usize,
    rust_context: &str,
    symbol_names: &[String],
) -> String {
    let chunk_info = if total_chunks > 1 {
        format!(
            " [chunk {}/{total_chunks}{}]",
            chunk_index + 1,
            if !symbol_names.is_empty() {
                format!(" — {}", symbol_names.join(", "))
            } else {
                String::new()
            }
        )
    } else {
        String::new()
    };

    let context_block = if rust_context.is_empty() {
        String::new()
    } else {
        format!(
            "\nDependency context (already-translated Rust signatures — use these exact types):\n\
             ```rust\n{rust_context}\n```\n"
        )
    };

    format!(
        "Translate the following {source_lang} file `{file_name}`{chunk_info} to idiomatic Rust.\
         {context_block}\n\
         Rules:\n\
         - Output ONLY Rust source code. No markdown fences, no explanation.\n\
         - Use the exact types from the dependency context above where applicable.\n\
         - Use `// [DEPS] crate_name = \"version\"` comments for any new Cargo dependencies needed.\n\
         - Preserve all comments and docstrings, translated to Rust doc-comment style.\n\n\
         Source ({source_lang}):\n```{source_lang}\n{source_code}\n```"
    )
}


pub const SYSTEM_FIX: &str = r#"You are an expert Rust programmer fixing compilation errors.
Given Rust source code and compiler errors from `cargo check`, output a corrected version.

Rules:
1. Output ONLY the corrected Rust source code — no markdown, no explanation.
2. Fix ALL listed errors. Do not introduce new ones.
3. Keep the same overall structure and logic. Only change what is broken.
4. Preserve all existing `use` statements and add new ones if needed.
"#;

pub fn prompt_fix(rust_code: &str, errors: &str) -> String {
    format!(
        "Fix the following Rust source file. All errors are from `cargo check`.\n\
         Rules:\n\
         - Output ONLY the corrected Rust source — no markdown, no explanation.\n\
         - Fix ALL listed errors. Do not introduce new ones.\n\
         - Preserve all logic, comments, and structure.\n\
         - Add missing `use` statements at the top if needed.\n\n\
         Current code:\n```rust\n{rust_code}\n```\n\n\
         Compiler errors:\n```\n{errors}\n```"
    )
}

/// Family-aware fix prompt.
///
/// `families` is a deduplicated, priority-sorted slice of
/// `(DiagnosticFamily, repair_hint)` pairs derived from the classified
/// diagnostics.  The prompt includes the top-N repair hints so the model
/// knows *exactly* what class of errors to focus on.
pub fn prompt_fix_targeted(
    rust_code: &str,
    errors: &str,
    families: &[(&str, &str)],
) -> String {
    let hint_block = if families.is_empty() {
        String::new()
    } else {
        let hints: String = families
            .iter()
            .enumerate()
            .map(|(i, (name, hint))| format!("{}. [{name}] {hint}", i + 1))
            .collect::<Vec<_>>()
            .join("\n");
        format!("\nDiagnostic families detected (fix in this order):\n{hints}\n")
    };

    format!(
        "Fix the following Rust source file. All errors are from `cargo check`.\n\
         {hint_block}\n\
         Rules:\n\
         - Output ONLY the corrected Rust source — no markdown, no explanation.\n\
         - Fix ALL listed errors. Do not introduce new ones.\n\
         - Preserve all logic, comments, and structure.\n\
         - Add missing `use` statements at the top if needed.\n\n\
         Current code:\n```rust\n{rust_code}\n```\n\n\
         Compiler errors:\n```\n{errors}\n```"
    )
}


// ---------------------------------------------------------------------------
// Code extraction helper
// ---------------------------------------------------------------------------

/// Strip markdown code fences from LLM output (model sometimes adds them
/// despite instructions).
pub fn extract_rust_code(raw: &str) -> String {
    // Try to find ```rust ... ``` or ``` ... ``` blocks.
    let fenced = extract_fenced(raw, "rust")
        .or_else(|| extract_fenced(raw, ""))
        .unwrap_or_else(|| raw.to_string());

    fenced.trim().to_string()
}

fn extract_fenced(text: &str, lang: &str) -> Option<String> {
    let open = format!("```{lang}");
    let start = text.find(&open)?;
    let after_open = &text[start + open.len()..];
    // Skip the rest of the opening line.
    let body_start = after_open.find('\n').map(|i| i + 1).unwrap_or(0);
    let body = &after_open[body_start..];
    let end = body.rfind("```")?;
    Some(body[..end].to_string())
}
