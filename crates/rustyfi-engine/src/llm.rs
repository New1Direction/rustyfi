use serde_json::json;
use std::io::{Read as _, Write as _};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use tracing::{debug, info, warn};

use crate::EngineError;

// ---------------------------------------------------------------------------
// Provider enum
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum Provider {
    /// Any OpenAI-compatible endpoint — supports multiple keys for round-robin rotation.
    OpenAi {
        base_url: String,
        api_keys: Vec<String>,
        key_idx: AtomicUsize,
    },
    /// xAI Grok via grok-build OAuth — reads ~/.grok/auth.json
    Grok,
    /// Local Claude Code CLI (`claude -p`) as a completion backend. Drives the
    /// machine's own Claude Code login (subscription/OAuth) rather than an API
    /// key, so the compile-fix doctor can run on a Claude-class model with no
    /// separate key/spend. The child process is spawned with `ANTHROPIC_API_KEY`
    /// removed (unless `RUSTYFI_CLAUDE_KEEP_KEY` is set) so `claude` falls back
    /// to its own login instead of a possibly gateway-scoped inherited key.
    ClaudeCli {
        /// Binary to invoke (override with `RUSTYFI_CLAUDE_BIN`; default `claude`).
        bin: String,
    },
}

/// Read env `primary`; if unset/empty, fall back to `fallback`.
fn env_or(primary: &str, fallback: &str) -> Option<String> {
    std::env::var(primary)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            std::env::var(fallback)
                .ok()
                .filter(|s| !s.trim().is_empty())
        })
}

impl Provider {
    /// Provider for the translation client (the `RUSTYFI_*` vars).
    pub fn from_env() -> Result<Self, EngineError> {
        Self::build(
            std::env::var("RUSTYFI_PROVIDER").ok(),
            std::env::var("RUSTYFI_LLM_API_KEY").ok(),
            std::env::var("RUSTYFI_LLM_BASE_URL").ok(),
        )
    }

    /// Provider for the verification fix loop — `RUSTYFI_FIX_*` overrides,
    /// falling back to the translation config so it's a no-op unless opted in.
    pub fn from_fix_env() -> Result<Self, EngineError> {
        Self::build(
            env_or("RUSTYFI_FIX_PROVIDER", "RUSTYFI_PROVIDER"),
            env_or("RUSTYFI_FIX_API_KEY", "RUSTYFI_LLM_API_KEY"),
            env_or("RUSTYFI_FIX_BASE_URL", "RUSTYFI_LLM_BASE_URL"),
        )
    }

    fn build(
        provider: Option<String>,
        api_key: Option<String>,
        base_url: Option<String>,
    ) -> Result<Self, EngineError> {
        let kind = provider
            .unwrap_or_else(|| "openai".to_string())
            .to_lowercase();
        match kind.as_str() {
            "grok" | "xai" => Ok(Provider::Grok),
            "claude" | "claude_cli" | "claude-cli" | "claudecode" | "claude-code" => {
                // No API key required — the CLI uses the machine's Claude Code login.
                let bin = std::env::var("RUSTYFI_CLAUDE_BIN")
                    .ok()
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| "claude".to_string());
                Ok(Provider::ClaudeCli { bin })
            }
            _ => {
                let raw = api_key.filter(|s| !s.trim().is_empty()).ok_or_else(|| {
                    EngineError::Config(
                        "RUSTYFI_LLM_API_KEY not set (required for openai provider)".into(),
                    )
                })?;
                // Support comma-separated keys for round-robin rotation across
                // multiple free-tier accounts (e.g. two Cerebras keys).
                let api_keys: Vec<String> = raw
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                if api_keys.is_empty() {
                    return Err(EngineError::Config("RUSTYFI_LLM_API_KEY is empty".into()));
                }
                let base_url = base_url
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| "https://openrouter.ai/api/v1".to_string());
                Ok(Provider::OpenAi {
                    base_url,
                    api_keys,
                    key_idx: AtomicUsize::new(0),
                })
            }
        }
    }

    /// Pick the next API key in round-robin order (thread-safe).
    fn next_key(&self) -> Option<&str> {
        match self {
            Provider::OpenAi {
                api_keys, key_idx, ..
            } => {
                let idx = key_idx.fetch_add(1, Ordering::Relaxed) % api_keys.len();
                Some(&api_keys[idx])
            }
            Provider::Grok => None,
            Provider::ClaudeCli { .. } => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Grok OAuth token state
// ---------------------------------------------------------------------------

const GROK_AUTH_JSON: &str = ".grok/auth.json";
const GROK_CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
const GROK_TOKEN_URL: &str = "https://auth.x.ai/oauth2/token";
const GROK_DEVICE_URL: &str = "https://auth.x.ai/oauth2/device/code";
const GROK_API_BASE: &str = "https://api.x.ai/v1";
const GROK_SCOPES: &str = "openid profile email offline_access grok-cli:access api:access";

#[derive(Debug)]
struct GrokToken {
    jwt: String,
    refresh_token: String,
    expires_at: f64, // unix epoch seconds
}

impl GrokToken {
    /// Load from ~/.grok/auth.json — matches the korgex GrokClient._load_token() logic.
    fn load() -> Option<Self> {
        let home = dirs_next().ok()?;
        let path = home.join(GROK_AUTH_JSON);
        let raw = std::fs::read_to_string(&path).ok()?;
        let data: serde_json::Value = serde_json::from_str(&raw).ok()?;

        for (_key, val) in data.as_object()? {
            if !_key.contains("auth.x.ai") {
                continue;
            }
            let obj = val.as_object()?;
            let jwt = obj.get("key")?.as_str()?.to_string();
            let rt = obj.get("refresh_token")?.as_str()?.to_string();
            let exp = parse_expires_at(obj.get("expires_at"));
            return Some(GrokToken {
                jwt,
                refresh_token: rt,
                expires_at: exp,
            });
        }
        None
    }

    fn is_expired(&self) -> bool {
        // Treat as expired 5 min early so we never send a stale token mid-request.
        unix_now_secs() >= self.expires_at - 300.0
    }

    /// Refresh via OAuth2 refresh_token grant and write back to auth.json.
    fn refresh(&mut self, client: &reqwest::blocking::Client) -> Result<(), EngineError> {
        info!("Refreshing Grok JWT via auth.x.ai");
        let resp = client
            .post(GROK_TOKEN_URL)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(format!(
                "grant_type=refresh_token&client_id={GROK_CLIENT_ID}&refresh_token={}",
                urlenccode(&self.refresh_token)
            ))
            .send()
            .map_err(|e| EngineError::Config(format!("Grok token refresh HTTP: {e}")))?;

        if !resp.status().is_success() {
            let body = resp.text().unwrap_or_default();
            return Err(EngineError::Config(format!(
                "Grok token refresh failed: {body}"
            )));
        }

        let val: serde_json::Value = resp
            .json()
            .map_err(|e| EngineError::Config(format!("Grok refresh JSON: {e}")))?;

        self.jwt = val["access_token"]
            .as_str()
            .ok_or_else(|| EngineError::Config("Grok refresh: no access_token".into()))?
            .to_string();

        if let Some(rt) = val["refresh_token"].as_str() {
            self.refresh_token = rt.to_string();
        }

        let expires_in = val["expires_in"].as_f64().unwrap_or(21600.0);
        self.expires_at = unix_now_secs() + expires_in - 300.0;

        // Write back to auth.json so the grok CLI stays in sync.
        self.save_back();
        info!("Grok JWT refreshed, expires in {expires_in:.0}s");
        Ok(())
    }

    fn save_back(&self) {
        if let Ok(home) = dirs_next() {
            let path = home.join(GROK_AUTH_JSON);
            if let Ok(raw) = std::fs::read_to_string(&path) {
                if let Ok(mut data) = serde_json::from_str::<serde_json::Value>(&raw) {
                    if let Some(obj) = data.as_object_mut() {
                        for (_key, val) in obj.iter_mut() {
                            if !_key.contains("auth.x.ai") {
                                continue;
                            }
                            if let Some(m) = val.as_object_mut() {
                                m.insert("key".into(), json!(self.jwt));
                                m.insert("refresh_token".into(), json!(self.refresh_token));
                                // Store as ISO string (matching grok CLI format)
                                let dt = chrono_iso(self.expires_at);
                                m.insert("expires_at".into(), json!(dt));
                            }
                            break;
                        }
                    }
                    let _ = std::fs::write(
                        &path,
                        serde_json::to_string_pretty(&data).unwrap_or_default(),
                    );
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tool-calling types and helpers
// ---------------------------------------------------------------------------

/// The output of one model turn in a tool-calling conversation.
#[derive(Debug, Clone)]
pub enum AssistantTurn {
    /// The model wants to call a tool.
    ToolInvocation {
        name: String,
        arguments_json: String,
    },
    /// The model produced a plain text reply.
    Text(String),
}

/// Build the JSON request body for a tool-calling completion request.
///
/// This is a pure function so that it can be unit-tested without network I/O.
/// `messages` should carry the running conversation (system message is added
/// first by this helper from the `system` argument).
pub fn build_tools_request_body(
    model: &str,
    system: &str,
    messages: &[serde_json::Value],
    tools: &serde_json::Value,
) -> serde_json::Value {
    let mut all_messages = Vec::with_capacity(messages.len() + 1);
    all_messages.push(json!({ "role": "system", "content": system }));
    all_messages.extend_from_slice(messages);

    json!({
        "model": model,
        "temperature": 0.1,
        "max_tokens": 8192,
        "tools": tools,
        "parallel_tool_calls": false,
        "messages": all_messages,
    })
}

/// Parse a raw OpenAI-format chat completion response into an `AssistantTurn`
/// plus the raw assistant message object (for verbatim re-insertion into the
/// conversation so that `tool_call_id` values round-trip correctly).
fn parse_tools_response(
    val: serde_json::Value,
) -> Result<(AssistantTurn, serde_json::Value), EngineError> {
    let message = val["choices"][0]["message"].clone();

    // Determine the turn type from the message object.
    let tool_calls = message.get("tool_calls");
    let turn = match tool_calls {
        Some(tc) if tc.is_array() && !tc.as_array().unwrap().is_empty() => {
            let first = &tc[0];
            let name = first["function"]["name"].as_str().unwrap_or("").to_string();
            let arguments_json = first["function"]["arguments"]
                .as_str()
                .unwrap_or("{}")
                .to_string();
            AssistantTurn::ToolInvocation {
                name,
                arguments_json,
            }
        }
        _ => {
            // No tool_calls (absent, null, or empty array) → text reply.
            let content = message["content"].as_str().unwrap_or("").to_string();
            AssistantTurn::Text(content)
        }
    };

    Ok((turn, message))
}

// ---------------------------------------------------------------------------
// Public LLM client
// ---------------------------------------------------------------------------

/// A blocking, thread-safe LLM client supporting OpenAI-compatible providers
/// and Grok via xAI OAuth (auto-refresh from ~/.grok/auth.json).
pub struct LlmClient {
    http: reqwest::blocking::Client,
    model: String,
    provider: Provider,
    grok_token: Mutex<Option<GrokToken>>,
    /// Per-request timeout (seconds). The fix client gets a longer one because
    /// reasoning models (e.g. deepseek-reasoner, o-series) take longer to think.
    timeout_secs: u64,
}

/// Default per-request timeout (seconds) before any env override.
///
/// HTTP providers return the first token quickly, so `http_default` (90s
/// translate / 180s fix) is fine. The Claude Code CLI is different: it spawns a
/// subprocess (cold start) and drives a reasoning model, which routinely needs
/// minutes — a 162-error doctor seed blew the 180s fix default on the *first*
/// call, and Opus translation runs ~180s/call. Give the CLI provider a roomier
/// default; `RUSTYFI_LLM_TIMEOUT` / `RUSTYFI_FIX_TIMEOUT` still override it.
fn default_timeout_for(provider: &Provider, http_default: u64) -> u64 {
    const CLAUDE_CLI_DEFAULT: u64 = 600;
    if matches!(provider, Provider::ClaudeCli { .. }) {
        CLAUDE_CLI_DEFAULT
    } else {
        http_default
    }
}

impl LlmClient {
    /// Translation client — the `RUSTYFI_*` vars.
    ///
    /// | Var | Default | Notes |
    /// |-----|---------|-------|
    /// | `RUSTYFI_PROVIDER` | `openai` | `grok` to use xAI OAuth |
    /// | `RUSTYFI_LLM_API_KEY` | — | Required for `openai` provider |
    /// | `RUSTYFI_LLM_BASE_URL` | `https://openrouter.ai/api/v1` | OpenAI compat endpoint |
    /// | `RUSTYFI_LLM_MODEL` | `google/gemini-2.5-flash` / `grok-build` | Model ID |
    pub fn from_env() -> Result<Self, EngineError> {
        let provider = Provider::from_env()?;
        let timeout = std::env::var("RUSTYFI_LLM_TIMEOUT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| default_timeout_for(&provider, 90));
        Self::build(
            provider,
            std::env::var("RUSTYFI_LLM_MODEL")
                .ok()
                .filter(|s| !s.trim().is_empty()),
            timeout,
        )
    }

    /// Verification fix-loop client. Bulk translation is voluminous and cheap;
    /// repairing compile errors is precision work that benefits from a stronger
    /// model. Configure independently via `RUSTYFI_FIX_MODEL` (and optionally
    /// `RUSTYFI_FIX_BASE_URL` / `RUSTYFI_FIX_API_KEY` / `RUSTYFI_FIX_PROVIDER`
    /// to point at a different endpoint entirely, e.g. Anthropic or OpenAI).
    /// Everything falls back to the translation config, so unset == no change.
    pub fn for_fixing() -> Result<Self, EngineError> {
        let provider = Provider::from_fix_env()?;
        let timeout = std::env::var("RUSTYFI_FIX_TIMEOUT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| default_timeout_for(&provider, 180)); // reasoning models think longer
        Self::build(
            provider,
            env_or("RUSTYFI_FIX_MODEL", "RUSTYFI_LLM_MODEL"),
            timeout,
        )
    }

    fn build(
        provider: Provider,
        model_override: Option<String>,
        timeout_secs: u64,
    ) -> Result<Self, EngineError> {
        let default_model = match &provider {
            Provider::Grok => "grok-build".to_string(),
            Provider::ClaudeCli { .. } => "opus".to_string(),
            Provider::OpenAi { .. } => "google/gemini-2.5-flash".to_string(),
        };
        let model = model_override.unwrap_or(default_model);

        let http = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(timeout_secs + 30))
            .build()
            .map_err(|e| EngineError::Config(e.to_string()))?;

        let grok_token = if matches!(provider, Provider::Grok) {
            let token = GrokToken::load().ok_or_else(|| {
                EngineError::Config(
                    "Grok provider selected but ~/.grok/auth.json not found or has no token. \
                     Run 'grok login' first or trigger /api/grok/login from the UI."
                        .into(),
                )
            })?;
            info!(
                "Grok OAuth: loaded token {}… (expires in {:.0}s)",
                token.jwt.get(..12).unwrap_or(&token.jwt),
                token.expires_at - unix_now_secs()
            );
            Mutex::new(Some(token))
        } else {
            Mutex::new(None)
        };

        Ok(Self {
            http,
            model,
            provider,
            grok_token,
            timeout_secs,
        })
    }

    /// Ensure we have a valid Grok JWT, refreshing if needed.
    fn grok_jwt(&self) -> Result<String, EngineError> {
        let mut guard = self.grok_token.lock().unwrap();
        let token = guard
            .as_mut()
            .ok_or_else(|| EngineError::Config("Grok token not loaded".into()))?;
        if token.is_expired() {
            token.refresh(&self.http)?;
        }
        Ok(token.jwt.clone())
    }

    /// Send a single-turn completion and return the assistant text.
    pub fn complete(&self, system: &str, user: &str) -> Result<String, EngineError> {
        self.complete_with_model(system, user, &self.model.clone())
    }

    /// Like `complete` but overrides the model for this one request.
    /// Used by tiered routing to downgrade small files to faster/cheaper models.
    pub fn complete_with_model(
        &self,
        system: &str,
        user: &str,
        model: &str,
    ) -> Result<String, EngineError> {
        // Local Claude Code CLI: shell out instead of making an HTTP request.
        if let Provider::ClaudeCli { bin } = &self.provider {
            return self.claude_cli_call(bin, system, user, model);
        }

        let (url, auth_header) = match &self.provider {
            Provider::OpenAi { base_url, .. } => {
                let key = self
                    .provider
                    .next_key()
                    .ok_or_else(|| EngineError::Config("No API keys configured".into()))?;
                (
                    format!("{base_url}/chat/completions"),
                    format!("Bearer {key}"),
                )
            }
            Provider::Grok => (
                format!("{GROK_API_BASE}/chat/completions"),
                format!("Bearer {}", self.grok_jwt()?),
            ),
            Provider::ClaudeCli { .. } => unreachable!("handled above"),
        };

        let body = json!({
            "model": model,
            "temperature": 0.1,
            "max_tokens": 16384,
            "messages": [
                { "role": "system", "content": system },
                { "role": "user",   "content": user   }
            ]
        });

        debug!("POST {} (model={})", url, model);

        let resp = self
            .http
            .post(&url)
            .header("Authorization", auth_header)
            .header("Content-Type", "application/json")
            .timeout(std::time::Duration::from_secs(self.timeout_secs))
            .json(&body)
            .send()
            .map_err(|e| {
                if e.is_timeout() {
                    EngineError::Llm(format!(
                        "timeout after {}s (model={model})",
                        self.timeout_secs
                    ))
                } else {
                    EngineError::Llm(format!("HTTP error: {e}"))
                }
            })?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().unwrap_or_default();
            // Auth failures will fail every request identically — surface them
            // as Config errors so the pipeline can abort early instead of
            // burning the whole retry budget per chunk.
            if status.as_u16() == 401 || status.as_u16() == 403 {
                return Err(EngineError::Config(format!(
                    "The LLM provider rejected the credentials (HTTP {status}). \
                     Check RUSTYFI_LLM_API_KEY / RUSTYFI_PROVIDER and restart the server. \
                     Provider said: {body}"
                )));
            }
            return Err(EngineError::Llm(format!("LLM HTTP {status}: {body}")));
        }

        let val: serde_json::Value = resp
            .json()
            .map_err(|e| EngineError::Llm(format!("JSON parse: {e}")))?;

        let content = val["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or("")
            .to_string();

        if content.trim().is_empty() {
            warn!("LLM returned empty content; full response: {val}");
            return Err(EngineError::Llm(format!(
                "empty response from model {model} — retrying"
            )));
        }
        Ok(content)
    }

    /// Multi-turn tool-calling completion (OpenAI tool-calling wire format).
    ///
    /// `messages` carries the running conversation including any prior
    /// `{role:"tool", tool_call_id, content}` result messages — the caller
    /// manages the conversation slice.  The `tools` value must be an OpenAI
    /// `tools` array JSON value.
    ///
    /// Returns `(AssistantTurn, raw_message_json)`.  The caller should append
    /// the raw message JSON to the conversation verbatim so that tool_call_id
    /// values round-trip correctly in subsequent turns.
    ///
    /// Note: the `tools` JSON value must stay alive for the duration of this
    /// call (it is borrowed for request-body construction only).
    pub fn complete_with_tools(
        &self,
        system: &str,
        messages: &[serde_json::Value],
        tools: &serde_json::Value,
    ) -> Result<(AssistantTurn, serde_json::Value), EngineError> {
        // Local Claude Code CLI: there is no OpenAI tool-calling wire protocol,
        // so flatten the conversation + tool catalogue into a single prompt and
        // return the model's reply as text. The doctor's driver loop already
        // parses a `{"tool": …, "args": …}` JSON action out of a text turn
        // (see SYSTEM_DOCTOR rule 6 and agent_fix::parse_action_reply), so the
        // whole agentic loop works unchanged with a text-only backend.
        if let Provider::ClaudeCli { bin } = &self.provider {
            let tools_block = render_tools_for_text(tools);
            let convo = render_conversation_for_text(messages);
            let prompt = format!(
                "{tools_block}\n\n--- CONVERSATION SO FAR ---\n{convo}\n\
                 Reply with your next action now as exactly one JSON object: \
                 {{\"tool\": \"<name>\", \"args\": {{ … }}}}."
            );
            let model = self.model.clone();
            let text = self.claude_cli_call(bin, system, &prompt, &model)?;
            let raw = json!({ "role": "assistant", "content": text.clone() });
            return Ok((AssistantTurn::Text(text), raw));
        }

        let (url, auth_header) = match &self.provider {
            Provider::OpenAi { base_url, .. } => {
                let key = self
                    .provider
                    .next_key()
                    .ok_or_else(|| EngineError::Config("No API keys configured".into()))?;
                (
                    format!("{base_url}/chat/completions"),
                    format!("Bearer {key}"),
                )
            }
            Provider::Grok => (
                format!("{GROK_API_BASE}/chat/completions"),
                format!("Bearer {}", self.grok_jwt()?),
            ),
            Provider::ClaudeCli { .. } => unreachable!("handled above"),
        };

        let body = build_tools_request_body(&self.model, system, messages, tools);

        debug!("POST {} (model={}, tools)", url, self.model);

        let resp = self
            .http
            .post(&url)
            .header("Authorization", auth_header)
            .header("Content-Type", "application/json")
            .timeout(std::time::Duration::from_secs(self.timeout_secs))
            .json(&body)
            .send()
            .map_err(|e| {
                if e.is_timeout() {
                    EngineError::Llm(format!(
                        "timeout after {}s (model={})",
                        self.timeout_secs, self.model
                    ))
                } else {
                    EngineError::Llm(format!("HTTP error: {e}"))
                }
            })?;

        let status = resp.status();
        if !status.is_success() {
            let body_text = resp.text().unwrap_or_default();
            if status.as_u16() == 401 || status.as_u16() == 403 {
                return Err(EngineError::Config(format!(
                    "The LLM provider rejected the credentials (HTTP {status}). \
                     Check RUSTYFI_LLM_API_KEY / RUSTYFI_PROVIDER and restart the server. \
                     Provider said: {body_text}"
                )));
            }
            return Err(EngineError::Llm(format!("LLM HTTP {status}: {body_text}")));
        }

        let val: serde_json::Value = resp
            .json()
            .map_err(|e| EngineError::Llm(format!("JSON parse: {e}")))?;

        parse_tools_response(val)
    }

    /// Drive the local Claude Code CLI as a one-shot completion backend.
    ///
    /// Spawns `claude -p --output-format json --model <model> --system-prompt
    /// <system>` with the user prompt on stdin, removes `ANTHROPIC_API_KEY` from
    /// the child environment (so `claude` uses the machine's own login rather
    /// than a possibly gateway-scoped inherited key — override with
    /// `RUSTYFI_CLAUDE_KEEP_KEY`), and parses the `result` field out of the JSON
    /// envelope. Runs from a temp dir so a project-local `CLAUDE.md` can't leak
    /// into the prompt. Bounded by `self.timeout_secs`.
    fn claude_cli_call(
        &self,
        bin: &str,
        system: &str,
        prompt: &str,
        model: &str,
    ) -> Result<String, EngineError> {
        let args = claude_command_args(model, system);
        debug!("spawn {bin} -p (model={model})");

        let mut cmd = Command::new(bin);
        cmd.args(&args)
            .current_dir(std::env::temp_dir())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if std::env::var("RUSTYFI_CLAUDE_KEEP_KEY").is_err() {
            cmd.env_remove("ANTHROPIC_API_KEY");
            cmd.env_remove("ANTHROPIC_AUTH_TOKEN");
        }

        let mut child = cmd.spawn().map_err(|e| {
            EngineError::Config(format!(
                "failed to launch `{bin}` — is the Claude Code CLI installed and on PATH? \
                 (set RUSTYFI_CLAUDE_BIN to override): {e}"
            ))
        })?;

        // Feed the prompt on a dedicated thread so a large stdin write can't
        // deadlock against the child filling its stdout pipe.
        let mut stdin = child.stdin.take().expect("stdin piped");
        let prompt_owned = prompt.to_string();
        let writer = std::thread::spawn(move || {
            let _ = stdin.write_all(prompt_owned.as_bytes());
            // stdin dropped here → EOF for the child.
        });
        let mut stdout = child.stdout.take().expect("stdout piped");
        let reader = std::thread::spawn(move || {
            let mut buf = String::new();
            let _ = stdout.read_to_string(&mut buf);
            buf
        });
        let mut stderr = child.stderr.take().expect("stderr piped");
        let err_reader = std::thread::spawn(move || {
            let mut buf = String::new();
            let _ = stderr.read_to_string(&mut buf);
            buf
        });

        // Poll for exit with a wall-clock cap; kill on timeout.
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_secs(self.timeout_secs);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => {
                    if std::time::Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(EngineError::Llm(format!(
                            "`claude` timed out after {}s (model={model})",
                            self.timeout_secs
                        )));
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                Err(e) => return Err(EngineError::Llm(format!("`claude` wait failed: {e}"))),
            }
        }

        let _ = writer.join();
        let out = reader.join().unwrap_or_default();
        let err = err_reader.join().unwrap_or_default();

        if out.trim().is_empty() {
            return Err(EngineError::Llm(format!(
                "`claude` produced no output (stderr: {})",
                err.trim().chars().take(400).collect::<String>()
            )));
        }
        parse_claude_result(&out)
    }

    /// Returns true if this client is using the Grok provider.
    pub fn is_grok(&self) -> bool {
        matches!(self.provider, Provider::Grok)
    }
    /// Returns true if this client drives the local Claude Code CLI.
    pub fn is_claude_cli(&self) -> bool {
        matches!(self.provider, Provider::ClaudeCli { .. })
    }
    pub fn model(&self) -> &str {
        &self.model
    }
}

// ---------------------------------------------------------------------------
// Claude Code CLI helpers (pure — unit-testable without spawning a process)
// ---------------------------------------------------------------------------

/// Build the argument vector for a one-shot `claude -p` completion call.
fn claude_command_args(model: &str, system: &str) -> Vec<String> {
    vec![
        "-p".to_string(),
        "--output-format".to_string(),
        "json".to_string(),
        "--model".to_string(),
        model.to_string(),
        "--no-session-persistence".to_string(),
        // Use the CLI as a pure model backend: don't load MCP servers or skills
        // (saves per-call startup and keeps the user's tool stack out of the
        // prompt). NOT `--bare` — that forces ANTHROPIC_API_KEY auth and would
        // break the subscription/OAuth login this provider relies on.
        "--strict-mcp-config".to_string(),
        "--disable-slash-commands".to_string(),
        // Replace the default Claude Code system prompt with ours so the model
        // behaves as a pure completion backend rather than a coding agent.
        "--system-prompt".to_string(),
        system.to_string(),
    ]
}

/// Parse the assistant text out of `claude -p --output-format json` output.
///
/// The envelope looks like `{"type":"result","subtype":"success","is_error":
/// false,"result":"…","api_error_status":…}`. Auth failures (401/403) become a
/// `Config` error so the pipeline aborts early instead of burning the retry
/// budget identically on every call.
fn parse_claude_result(stdout: &str) -> Result<String, EngineError> {
    let val: serde_json::Value = serde_json::from_str(stdout.trim())
        .or_else(|_| {
            // The CLI may emit a warning line before the JSON; take the last
            // line that parses as a JSON object.
            stdout
                .lines()
                .rev()
                .find_map(|l| serde_json::from_str::<serde_json::Value>(l.trim()).ok())
                .ok_or(())
        })
        .map_err(|_| {
            EngineError::Llm(format!(
                "could not parse `claude` JSON output: {}",
                stdout.chars().take(400).collect::<String>()
            ))
        })?;

    let result = val["result"].as_str().unwrap_or("").to_string();
    if val["is_error"].as_bool().unwrap_or(false) {
        let status = val["api_error_status"].as_u64();
        if matches!(status, Some(401) | Some(403)) {
            return Err(EngineError::Config(format!(
                "Claude Code CLI auth failed (HTTP {}). The child process drops ANTHROPIC_API_KEY \
                 so `claude` uses your Claude Code login — run `claude` once interactively to sign \
                 in, or set RUSTYFI_CLAUDE_KEEP_KEY=1 to use ANTHROPIC_API_KEY instead. Detail: {result}",
                status.unwrap_or(0)
            )));
        }
        return Err(EngineError::Llm(format!(
            "`claude` reported an error: {result}"
        )));
    }
    if result.trim().is_empty() {
        return Err(EngineError::Llm(
            "`claude` returned an empty result — retrying".to_string(),
        ));
    }
    Ok(result)
}

/// Render the doctor's OpenAI tool catalogue into a compact text description so
/// a text-only backend (the CLI) knows the tool names and their argument keys.
fn render_tools_for_text(tools: &serde_json::Value) -> String {
    let Some(arr) = tools.as_array() else {
        return String::new();
    };
    let mut lines = vec![
        "AVAILABLE TOOLS — choose exactly one per turn and call it as a JSON action:".to_string(),
    ];
    for t in arr {
        let f = &t["function"];
        let Some(name) = f["name"].as_str() else {
            continue;
        };
        let desc = f["description"].as_str().unwrap_or("");
        let arg_keys: Vec<&str> = f["parameters"]["properties"]
            .as_object()
            .map(|o| o.keys().map(|k| k.as_str()).collect())
            .unwrap_or_default();
        if arg_keys.is_empty() {
            lines.push(format!("- {name}() — {desc}"));
        } else {
            lines.push(format!("- {name}(args: {}) — {desc}", arg_keys.join(", ")));
        }
    }
    lines.join("\n")
}

/// Flatten an OpenAI-style message array into a plain-text transcript for the
/// CLI backend. Roles map to readable headers; non-string content is skipped.
fn render_conversation_for_text(messages: &[serde_json::Value]) -> String {
    let mut out = String::new();
    for m in messages {
        let role = m["role"].as_str().unwrap_or("user");
        let content = m["content"].as_str().unwrap_or("");
        if content.is_empty() {
            continue;
        }
        let label = match role {
            "assistant" => "ASSISTANT",
            "tool" => "TOOL RESULT",
            "system" => "SYSTEM",
            _ => "USER",
        };
        out.push_str(&format!("## {label}\n{content}\n\n"));
    }
    out
}

// ---------------------------------------------------------------------------
// Grok device-code login helper (called from the server OAuth endpoint)
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct DeviceCodeResponse {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: Option<String>,
    pub expires_in: u64,
    pub interval: u64,
}

/// Start a Grok device-code flow. Returns device code info to show to the user.
pub fn grok_device_code_start() -> Result<DeviceCodeResponse, EngineError> {
    let client = reqwest::blocking::Client::new();
    let resp = client
        .post(GROK_DEVICE_URL)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(format!(
            "client_id={GROK_CLIENT_ID}&scope={}",
            urlenccode(GROK_SCOPES)
        ))
        .send()
        .map_err(|e| EngineError::Config(format!("device code request: {e}")))?;

    if !resp.status().is_success() {
        let body = resp.text().unwrap_or_default();
        return Err(EngineError::Config(format!("device code failed: {body}")));
    }

    resp.json::<DeviceCodeResponse>()
        .map_err(|e| EngineError::Config(format!("device code parse: {e}")))
}

/// Poll for a completed device-code token exchange. Call in a loop every `interval` seconds.
/// Returns `Ok(Some(access_token))` when authenticated, `Ok(None)` when still pending.
pub fn grok_device_code_poll(device_code: &str) -> Result<Option<String>, EngineError> {
    let client = reqwest::blocking::Client::new();
    let resp = client
        .post(GROK_TOKEN_URL)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(format!(
            "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code\
             &device_code={device_code}&client_id={GROK_CLIENT_ID}"
        ))
        .send()
        .map_err(|e| EngineError::Config(format!("device poll: {e}")))?;

    let status = resp.status();
    let val: serde_json::Value = resp
        .json()
        .map_err(|e| EngineError::Config(format!("device poll JSON: {e}")))?;

    if status.is_success() {
        // Token granted — save it
        let access_token = val["access_token"].as_str().unwrap_or("").to_string();
        let refresh_token = val["refresh_token"].as_str().unwrap_or("").to_string();
        let expires_in = val["expires_in"].as_f64().unwrap_or(21600.0);

        let token = GrokToken {
            jwt: access_token.clone(),
            refresh_token,
            expires_at: unix_now_secs() + expires_in - 300.0,
        };
        token.save_back();
        info!("Grok device-code auth complete, token saved to ~/.grok/auth.json");
        return Ok(Some(access_token));
    }

    match val["error"].as_str() {
        Some("authorization_pending") | Some("slow_down") => Ok(None),
        Some(e) => Err(EngineError::Config(format!("device poll error: {e}"))),
        None => Err(EngineError::Config(format!(
            "device poll unexpected status: {status}"
        ))),
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
9. Do NOT emit `mod` or `pub mod` declarations for the project's own files — the
   build system wires modules. To reference another file's items in this project,
   use a `crate::<module>::Item` path (the build system normalises module paths).
10. Add a `Cargo.toml` dependencies comment block at the very top as:
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
- badger / boltdb / leveldb / rocksdb / bbolt (embedded KV stores) → sled
- gin / echo / fiber (Go web) → axum
- uuid / google/uuid → uuid (use `uuid::Uuid::new_v4()` with the `v4` feature)
- zerolog / zap / logrus → tracing
"#;

pub fn prompt_translate(source_code: &str, source_lang: &str, file_name: &str) -> String {
    prompt_translate_with_context(source_code, source_lang, file_name, 0, 1, "", &[])
}

/// Context-aware translation prompt.
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
            "\nProject API (these types/signatures ALREADY EXIST at the crate paths shown — \
             implement AGAINST them; do NOT redefine any struct/enum/trait listed here; \
             reproduce field names, parameter types and return/error types EXACTLY):\n\
             ```rust\n{rust_context}\n```\n"
        )
    };

    format!(
        "Translate the following {source_lang} file `{file_name}`{chunk_info} to idiomatic Rust.\
         {context_block}\n\
         Rules:\n\
         - Output ONLY Rust source code. No markdown fences, no explanation.\n\
         - The Project API block above is CANONICAL: use those exact types, fields, and \
           signatures; never invent a different shape for a project-owned type.\n\
         - Provide impl blocks and fn bodies; do NOT redefine types already shown in the Project API.\n\
         - Use `// [DEPS] crate_name = \"version\"` comments for any new Cargo dependencies needed.\n\
         - Preserve all comments and docstrings, translated to Rust doc-comment style.\n\n\
         Source ({source_lang}):\n```{source_lang}\n{source_code}\n```"
    )
}

pub const SYSTEM_CONTRACT: &str = r#"You are an expert Rust API designer. Given the source files of ONE package/module from another language, output the canonical Rust PUBLIC API surface for it — the shared contract that every file translating against this package must agree on.

Rules:
1. Output ONLY Rust. No markdown fences, no prose.
2. For EVERY exported type emit a `pub struct` with ALL of its public fields and a best-effort Rust field type. NEVER omit a field. If a field's type is unclear, use a reasonable Rust type (String, i64, Vec<u8>, serde_json::Value), but keep the field.
3. For every exported enum emit `pub enum` with ALL variants. For error types prefer a real `pub enum SomethingError { ... }`.
4. For every exported trait/interface emit `pub trait` with method signatures.
5. For every exported function/method emit a signature line ending in `;` — NO body, NO `todo!()`.
6. Use snake_case field/function names; PascalCase types. Be consistent.
7. Derive `#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]` on plain data structs.
8. Do NOT emit `use` statements, `mod` declarations, impl blocks, or any logic.
9. Reference another project package's types by their crate path when needed (e.g. `crate::storage::Store`).
10. If the API exposes a trait via `Box<dyn …>` or `&dyn …`, that trait MUST be object-safe (no generic methods).
This is a CONTRACT: it must be complete and stable, because all of this package's files and all its importers will be translated against it verbatim.
"#;

/// Build the contract-extraction prompt for one package.
pub fn prompt_extract_contract(pkg: &str, lang: &str, labeled_source: &str) -> String {
    format!(
        "Extract the canonical Rust public API surface for the `{pkg}` package \
         (written in {lang}). Emit complete `pub struct`/`enum`/`trait` definitions \
         (ALL fields/variants) and `pub fn` signature lines only — no bodies, no \
         markdown.\n\nPackage source:\n```{lang}\n{labeled_source}\n```"
    )
}

/// Build the contract-retry prompt: like `prompt_extract_contract` but includes
/// the previous (failed) contract, the compiler errors it produced, targeted
/// object-safety instructions, AND an explicit inventory of items the corrected
/// contract must not drop.
///
/// `original_items` is a pre-sorted, newline-separated list of item names
/// (as produced by `contract_check::item_names`) from the previous contract.
/// Pass an empty string when the inventory is unavailable.
pub fn prompt_extract_contract_retry(
    pkg: &str,
    lang: &str,
    labeled_source: &str,
    previous_contract: &str,
    compiler_errors: &str,
    original_items: &str,
) -> String {
    let items_section = if original_items.trim().is_empty() {
        String::new()
    } else {
        format!(
            "\nThe corrected API MUST still define ALL of these items (do not drop any):\n{original_items}\n"
        )
    };

    format!(
        "Extract the canonical Rust public API surface for the `{pkg}` package \
         (written in {lang}). Emit complete `pub struct`/`enum`/`trait` definitions \
         (ALL fields/variants) and `pub fn` signature lines only — no bodies, no \
         markdown.\n\nPackage source:\n```{lang}\n{labeled_source}\n```\n\n\
         Your previous API (did NOT compile):\n```rust\n{previous_contract}\n```\n\n\
         Compiler errors:\n```\n{compiler_errors}\n```\n\n\
         Fix ONLY the structural problems shown. If a trait is used as `dyn Trait` it \
         must be object-safe: no generic methods, no Self-returning methods without \
         `where Self: Sized`. Re-emit the COMPLETE corrected API.{items_section}"
    )
}

pub const SYSTEM_FIX: &str = r#"You are an expert Rust programmer fixing compilation errors.
Given Rust source code and compiler errors from `cargo check`, output a corrected version.

Rules:
1. Output ONLY the corrected Rust source code. NO markdown fences, NO ```rust, NO prose, NO explanation. Start your reply with the first line of code.
2. Fix ALL listed errors. Do not introduce new ones.
3. The compiler's `help:` suggestions are almost always correct — APPLY THEM EXACTLY (e.g. "a function with a similar name exists" → use that name; "remove the extra argument" → remove it; "consider borrowing here" → add the `&`).
4. Keep the same overall structure and logic. Only change what is broken.
5. Preserve all existing `use` statements; add new ones if needed.
6. Output the COMPLETE file — never truncate or abbreviate with `// ...`.
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
/// When `fix_context` is non-empty it is inserted after the family-hints block
/// under the heading `"Relevant project definitions (CANONICAL — match these
/// exactly, do not redefine):"`.
pub fn prompt_fix_targeted(
    rust_code: &str,
    errors: &str,
    families: &[(&str, &str)],
    fix_context: &str,
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

    let ctx_block = if fix_context.is_empty() {
        String::new()
    } else {
        format!(
            "\nRelevant project definitions (CANONICAL — match these exactly, do not redefine):\n{fix_context}\n"
        )
    };

    format!(
        "Fix the following Rust source file. All errors are from `cargo check`.\n\
         {hint_block}\
         {ctx_block}\n\
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

/// Strip markdown code fences from LLM output. Robust to reasoning models that
/// prepend prose and to TRUNCATED responses that emit an opening ```` ```rust ````
/// with no closing fence (which would otherwise leak the fence into the file
/// and break parsing with an "unclosed delimiter" error).
pub fn extract_rust_code(raw: &str) -> String {
    let trimmed = raw.trim();

    // 1. A complete fenced block → use its contents.
    if let Some(inner) = extract_fenced(trimmed, "rust")
        .or_else(|| extract_fenced(trimmed, "rs"))
        .or_else(|| extract_fenced(trimmed, ""))
    {
        return inner.trim().to_string();
    }

    // 2. A dangling opening fence (truncated response) → drop the fence line and
    //    any leading prose before it, plus a trailing fence if present.
    if let Some(pos) = trimmed.find("```") {
        let after_fence = &trimmed[pos..];
        let body = after_fence
            .find('\n')
            .map(|nl| &after_fence[nl + 1..])
            .unwrap_or("");
        return body.trim().trim_end_matches("```").trim().to_string();
    }

    trimmed.to_string()
}

fn extract_fenced(text: &str, lang: &str) -> Option<String> {
    let open = format!("```{lang}");
    let start = text.find(&open)?;
    let after_open = &text[start + open.len()..];
    let body_start = after_open.find('\n').map(|i| i + 1).unwrap_or(0);
    let body = &after_open[body_start..];
    let end = body.rfind("```")?;
    Some(body[..end].to_string())
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn unix_now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn dirs_next() -> Result<std::path::PathBuf, ()> {
    std::env::var("HOME")
        .map(std::path::PathBuf::from)
        .map_err(|_| ())
}

fn parse_expires_at(val: Option<&serde_json::Value>) -> f64 {
    let Some(v) = val else { return 0.0 };
    // Numeric (epoch seconds)
    if let Some(n) = v.as_f64() {
        return n;
    }
    // ISO string like "2026-06-09T20:41:05.898948Z"
    if let Some(s) = v.as_str() {
        // Simple parse: we just need epoch seconds
        if let Ok(secs) = s.parse::<f64>() {
            return secs;
        }
        // Use a rough parse: strip trailing Z, parse with chrono if available,
        // else fall back to 0 (will trigger a refresh on next use — safe).
        // We don't want to add chrono just for this, so we do a minimal parse.
        let s = s.trim_end_matches('Z').trim_end_matches('+').trim();
        if let Ok(dt) = chrono_parse_iso(s) {
            return dt;
        }
    }
    0.0
}

fn chrono_parse_iso(s: &str) -> Result<f64, ()> {
    // Minimal ISO 8601 parser: "2026-06-09T20:41:05"
    let parts: Vec<&str> = s.splitn(2, 'T').collect();
    if parts.len() != 2 {
        return Err(());
    }
    let date_parts: Vec<u32> = parts[0].split('-').filter_map(|p| p.parse().ok()).collect();
    let time_parts: Vec<u32> = parts[1]
        .split(':')
        .filter_map(|p| p.parse::<f64>().ok().map(|f| f as u32))
        .collect();
    if date_parts.len() < 3 || time_parts.len() < 3 {
        return Err(());
    }
    // Approximate: compute days since epoch (good enough for expiry checks)
    let y = date_parts[0] as i64;
    let m = date_parts[1] as i64;
    let d = date_parts[2] as i64;
    // Zeller-ish
    let days = (y - 1970) * 365 + (y - 1970) / 4 - (y - 1970) / 100
        + (y - 1970) / 400
        + [0i64, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334][(m - 1).max(0) as usize]
        + d
        - 1;
    let secs = days * 86400
        + time_parts[0] as i64 * 3600
        + time_parts[1] as i64 * 60
        + time_parts[2] as i64;
    Ok(secs as f64)
}

fn chrono_iso(epoch: f64) -> String {
    // Format epoch seconds as ISO 8601 UTC string
    let secs = epoch as u64;
    let s = secs % 60;
    let m = (secs / 60) % 60;
    let h = (secs / 3600) % 24;
    let days = secs / 86400;
    // Simple calendar from epoch days
    let (year, month, day) = days_to_ymd(days);
    format!("{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}Z")
}

fn days_to_ymd(mut days: u64) -> (u64, u64, u64) {
    let mut y = 1970u64;
    loop {
        let leap = (y.is_multiple_of(4) && !y.is_multiple_of(100)) || y.is_multiple_of(400);
        let dy = if leap { 366 } else { 365 };
        if days < dy {
            break;
        }
        days -= dy;
        y += 1;
    }
    let leap = (y.is_multiple_of(4) && !y.is_multiple_of(100)) || y.is_multiple_of(400);
    let month_days: &[u64] = if leap {
        &[31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        &[31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut m = 1u64;
    for &md in month_days {
        if days < md {
            break;
        }
        days -= md;
        m += 1;
    }
    (y, m, days + 1)
}

fn urlenccode(s: &str) -> String {
    s.chars()
        .flat_map(|c| {
            if c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') {
                vec![c]
            } else {
                format!("%{:02X}", c as u32).chars().collect()
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Provider-aware default timeout ────────────────────────────────────────

    #[test]
    fn claude_cli_gets_a_roomier_default_timeout() {
        let cli = Provider::ClaudeCli {
            bin: "claude".to_string(),
        };
        // The CLI provider ignores the HTTP default and uses its own roomier one.
        assert_eq!(default_timeout_for(&cli, 90), 600);
        assert_eq!(default_timeout_for(&cli, 180), 600);
        // HTTP providers keep the caller's default unchanged.
        assert_eq!(default_timeout_for(&Provider::Grok, 90), 90);
        assert_eq!(default_timeout_for(&Provider::Grok, 180), 180);
    }

    // ── Tool-calling request body ─────────────────────────────────────────────

    #[test]
    fn build_tools_request_body_includes_system_first() {
        let tools = json!([{"type": "function", "function": {"name": "list_files"}}]);
        let messages = vec![json!({"role": "user", "content": "hello"})];
        let body = build_tools_request_body("gpt-4o", "You are helpful.", &messages, &tools);

        // System message must be the first in the messages array.
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "system", "first message must be system");
        assert_eq!(msgs[0]["content"], "You are helpful.");
        // The user message follows.
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[1]["content"], "hello");
        // model, temperature, max_tokens present.
        assert_eq!(body["model"], "gpt-4o");
        assert_eq!(body["temperature"], 0.1);
        assert_eq!(body["max_tokens"], 8192);
        // parallel_tool_calls must be false to force single-tool responses.
        assert_eq!(
            body["parallel_tool_calls"], false,
            "parallel_tool_calls must be false"
        );
        // tools array present.
        assert!(body["tools"].is_array(), "tools must be an array");
    }

    #[test]
    fn build_tools_request_body_preserves_message_order() {
        let tools = json!([]);
        let messages = vec![
            json!({"role": "user",      "content": "first"}),
            json!({"role": "assistant", "content": "second"}),
            json!({"role": "user",      "content": "third"}),
        ];
        let body = build_tools_request_body("model", "sys", &messages, &tools);
        let msgs = body["messages"].as_array().unwrap();
        // 1 system + 3 conversation messages.
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[1]["content"], "first");
        assert_eq!(msgs[2]["content"], "second");
        assert_eq!(msgs[3]["content"], "third");
    }

    // ── Tool-calling response parsing ────────────────────────────────────────

    fn make_response_with_tool_call(name: &str, args: &str) -> serde_json::Value {
        json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_abc",
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": args
                        }
                    }]
                }
            }]
        })
    }

    fn make_response_text(content: &str) -> serde_json::Value {
        json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": content
                }
            }]
        })
    }

    #[test]
    fn parse_tool_call_present() {
        let val = make_response_with_tool_call("read_file", r#"{"path":"src/main.rs"}"#);
        let (turn, raw) = parse_tools_response(val).unwrap();
        match turn {
            AssistantTurn::ToolInvocation {
                name,
                arguments_json,
            } => {
                assert_eq!(name, "read_file");
                assert_eq!(arguments_json, r#"{"path":"src/main.rs"}"#);
            }
            AssistantTurn::Text(t) => panic!("expected ToolInvocation, got Text({t:?})"),
        }
        // Raw message must carry tool_calls for round-tripping.
        assert!(
            raw["tool_calls"].is_array(),
            "raw message must contain tool_calls"
        );
    }

    #[test]
    fn parse_text_when_no_tool_calls() {
        let val = make_response_text("Here is my analysis.");
        let (turn, _raw) = parse_tools_response(val).unwrap();
        match turn {
            AssistantTurn::Text(t) => assert_eq!(t, "Here is my analysis."),
            AssistantTurn::ToolInvocation { name, .. } => {
                panic!("expected Text, got ToolInvocation({name})")
            }
        }
    }

    #[test]
    fn parse_text_when_tool_calls_empty_array() {
        let val = json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "ok",
                    "tool_calls": []
                }
            }]
        });
        let (turn, _raw) = parse_tools_response(val).unwrap();
        match turn {
            AssistantTurn::Text(t) => assert_eq!(t, "ok"),
            AssistantTurn::ToolInvocation { name, .. } => {
                panic!("expected Text for empty tool_calls, got ToolInvocation({name})")
            }
        }
    }

    #[test]
    fn parse_text_when_content_missing() {
        // Some providers omit content entirely when there are no tool calls.
        let val = json!({
            "choices": [{
                "message": {
                    "role": "assistant"
                }
            }]
        });
        let (turn, _raw) = parse_tools_response(val).unwrap();
        match turn {
            AssistantTurn::Text(t) => assert_eq!(t, ""),
            AssistantTurn::ToolInvocation { name, .. } => {
                panic!("expected Text(\"\"), got ToolInvocation({name})")
            }
        }
    }

    // ── extract_rust_code ────────────────────────────────────────────────────

    #[test]
    fn extracts_clean_fenced_block() {
        let raw = "Here is the fix:\n```rust\nfn main() {}\n```\nDone.";
        assert_eq!(extract_rust_code(raw), "fn main() {}");
    }

    #[test]
    fn handles_truncated_dangling_fence() {
        // A truncated reasoning-model response: opening fence, no close.
        let raw = "Sure, here's the corrected file:\n```rust\nuse std::fmt;\nfn x() {}";
        let out = extract_rust_code(raw);
        assert!(!out.contains("```"), "leaked fence: {out}");
        assert!(out.starts_with("use std::fmt;"), "{out}");
    }

    #[test]
    fn passes_through_plain_code() {
        let raw = "fn a() -> i32 { 1 }\n";
        assert_eq!(extract_rust_code(raw), "fn a() -> i32 { 1 }");
    }

    // ── B4: prompt_extract_contract_retry ─────────────────────────────────

    #[test]
    fn retry_prompt_contains_previous_contract() {
        let p = prompt_extract_contract_retry(
            "storage",
            "go",
            "// go source",
            "pub trait Provider { fn get<T>(&self) -> T; }",
            "error[E0038]: the trait `Provider` is not object-safe",
            "",
        );
        assert!(
            p.contains("pub trait Provider"),
            "should contain previous contract: {p}"
        );
    }

    #[test]
    fn retry_prompt_contains_compiler_errors() {
        let p = prompt_extract_contract_retry(
            "storage",
            "go",
            "// go source",
            "pub trait Provider {}",
            "error[E0038]: object-safety violation",
            "",
        );
        assert!(
            p.contains("error[E0038]: object-safety violation"),
            "should contain compiler errors: {p}"
        );
    }

    #[test]
    fn retry_prompt_contains_object_safety_instruction() {
        let p = prompt_extract_contract_retry(
            "storage",
            "go",
            "// go source",
            "pub trait T {}",
            "some error",
            "",
        );
        assert!(
            p.contains("object-safe"),
            "should contain object-safety instruction: {p}"
        );
        assert!(
            p.contains("no generic methods"),
            "should mention generic methods: {p}"
        );
    }

    #[test]
    fn retry_prompt_contains_item_inventory_section() {
        let items = "Foo\nBar\nBaz::method";
        let p = prompt_extract_contract_retry(
            "storage",
            "go",
            "// go source",
            "pub struct Foo {}",
            "some error",
            items,
        );
        assert!(
            p.contains("The corrected API MUST still define ALL of these items"),
            "should contain items section header: {p}"
        );
        assert!(p.contains("Foo"), "should contain item Foo: {p}");
        assert!(p.contains("Bar"), "should contain item Bar: {p}");
        assert!(p.contains("Baz::method"), "should contain Baz::method: {p}");
    }

    #[test]
    fn retry_prompt_omits_item_section_when_inventory_empty() {
        let p = prompt_extract_contract_retry(
            "storage",
            "go",
            "// go source",
            "pub struct Foo {}",
            "some error",
            "",
        );
        assert!(
            !p.contains("MUST still define ALL of these items"),
            "items section should be absent when inventory is empty: {p}"
        );
    }

    #[test]
    fn system_contract_mentions_object_safety() {
        assert!(
            SYSTEM_CONTRACT.contains("object-safe"),
            "SYSTEM_CONTRACT should mention object-safe"
        );
        assert!(
            SYSTEM_CONTRACT.contains("Box<dyn"),
            "SYSTEM_CONTRACT should mention Box<dyn>"
        );
    }

    // ── C3: prompt_fix_targeted CANONICAL header ──────────────────────────

    #[test]
    fn fix_targeted_canonical_header_present_when_context_nonempty() {
        let p = prompt_fix_targeted(
            "fn foo() {}",
            "error: something",
            &[],
            "pub trait Provider { fn get(&self); }",
        );
        assert!(
            p.contains(
                "Relevant project definitions (CANONICAL — match these exactly, do not redefine):"
            ),
            "CANONICAL header missing when context is non-empty:\n{p}"
        );
        assert!(
            p.contains("pub trait Provider"),
            "context body missing from prompt:\n{p}"
        );
    }

    #[test]
    fn fix_targeted_canonical_header_absent_when_context_empty() {
        let p = prompt_fix_targeted("fn foo() {}", "error: something", &[], "");
        assert!(
            !p.contains("CANONICAL"),
            "CANONICAL header should be absent when context is empty:\n{p}"
        );
    }

    // ── Claude Code CLI provider ──────────────────────────────────────────────

    #[test]
    fn claude_provider_needs_no_api_key() {
        // The CLI uses the machine's own login, so no RUSTYFI_LLM_API_KEY.
        let p = Provider::build(Some("claude_cli".to_string()), None, None).unwrap();
        assert!(
            matches!(p, Provider::ClaudeCli { .. }),
            "claude_cli should select the ClaudeCli provider without a key"
        );
        // Aliases all resolve to the same provider.
        for alias in ["claude", "claude-cli", "claudecode", "claude-code"] {
            let p = Provider::build(Some(alias.to_string()), None, None).unwrap();
            assert!(
                matches!(p, Provider::ClaudeCli { .. }),
                "alias {alias} failed"
            );
        }
    }

    #[test]
    fn claude_command_args_are_well_formed() {
        let args = claude_command_args("opus", "be terse");
        assert_eq!(args[0], "-p");
        // Print mode with JSON output and an explicit model + replaced system prompt.
        assert!(args.iter().any(|a| a == "--output-format"));
        let oi = args.iter().position(|a| a == "--output-format").unwrap();
        assert_eq!(args[oi + 1], "json");
        let mi = args.iter().position(|a| a == "--model").unwrap();
        assert_eq!(args[mi + 1], "opus");
        let si = args.iter().position(|a| a == "--system-prompt").unwrap();
        assert_eq!(args[si + 1], "be terse");
        assert!(args.iter().any(|a| a == "--no-session-persistence"));
    }

    #[test]
    fn parse_claude_result_extracts_text_from_real_envelope() {
        // A real success envelope captured from `claude -p --output-format json`.
        let stdout = r#"{"type":"result","subtype":"success","is_error":false,"result":"fn main() {}","session_id":"abc","total_cost_usd":0.01,"usage":{}}"#;
        assert_eq!(parse_claude_result(stdout).unwrap(), "fn main() {}");
    }

    #[test]
    fn parse_claude_result_maps_401_to_config_error() {
        let stdout = r#"{"type":"result","subtype":"success","is_error":true,"api_error_status":401,"result":"Invalid API key · Fix external API key"}"#;
        match parse_claude_result(stdout) {
            Err(EngineError::Config(msg)) => {
                assert!(msg.contains("auth failed"), "msg: {msg}");
                assert!(
                    msg.contains("ANTHROPIC_API_KEY"),
                    "should explain the key fix: {msg}"
                );
            }
            other => panic!("expected Config error for 401, got {other:?}"),
        }
    }

    #[test]
    fn parse_claude_result_maps_other_error_to_llm_error() {
        let stdout =
            r#"{"type":"result","is_error":true,"api_error_status":529,"result":"overloaded"}"#;
        assert!(matches!(
            parse_claude_result(stdout),
            Err(EngineError::Llm(_))
        ));
    }

    #[test]
    fn parse_claude_result_tolerates_leading_warning_line() {
        let stdout = "warning: something noisy\n{\"is_error\":false,\"result\":\"ok\"}";
        assert_eq!(parse_claude_result(stdout).unwrap(), "ok");
    }

    #[test]
    fn parse_claude_result_rejects_empty_result() {
        let stdout = r#"{"is_error":false,"result":""}"#;
        assert!(matches!(
            parse_claude_result(stdout),
            Err(EngineError::Llm(_))
        ));
    }

    #[test]
    fn render_tools_lists_names_and_args() {
        let tools = json!([
            {"type":"function","function":{"name":"cargo_check","description":"run check","parameters":{"type":"object","properties":{}}}},
            {"type":"function","function":{"name":"write_file","description":"write","parameters":{"type":"object","properties":{"path":{},"content":{}}}}}
        ]);
        let rendered = render_tools_for_text(&tools);
        assert!(rendered.contains("cargo_check()"), "{rendered}");
        // Key order is serde_json's (alphabetical) — assert membership, not order.
        let write_line = rendered
            .lines()
            .find(|l| l.starts_with("- write_file(args:"))
            .unwrap_or_else(|| panic!("no write_file line: {rendered}"));
        assert!(write_line.contains("path"), "{write_line}");
        assert!(write_line.contains("content"), "{write_line}");
    }

    #[test]
    fn render_conversation_labels_roles_and_skips_empty() {
        let messages = vec![
            json!({"role":"user","content":"seed errors"}),
            json!({"role":"assistant","content":"{\"tool\":\"cargo_check\"}"}),
            json!({"role":"user","content":"tool result:\nerror count: 0"}),
            json!({"role":"assistant","content":null}),
        ];
        let t = render_conversation_for_text(&messages);
        assert!(t.contains("## USER\nseed errors"), "{t}");
        assert!(
            t.contains("## ASSISTANT\n{\"tool\":\"cargo_check\"}"),
            "{t}"
        );
        assert!(t.contains("## USER\ntool result:"), "{t}");
        // Null content is skipped, not rendered as an empty assistant block.
        assert_eq!(t.matches("## ASSISTANT").count(), 1, "{t}");
    }
}
