use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::extract::{DefaultBodyLimit, Multipart, Path, State};
use axum::http::{header, StatusCode};
use axum::response::{AppendHeaders, IntoResponse, Response, Sse};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tower_http::cors::{Any, CorsLayer};
use tower_http::services::ServeDir;
use tracing::{info, warn};

use rustyfi_engine::llm::{grok_device_code_poll, grok_device_code_start};
use rustyfi_engine::pipeline::{run, Progress, RunConfig};

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

struct AppState {
    output_base: PathBuf,
    upload_base: PathBuf,
    history_lock: Mutex<()>, // serialise history.json writes
    /// Crate names with a translation currently in flight. Two concurrent
    /// runs of the same project would share an output/checkpoint directory
    /// and corrupt each other.
    active_runs: Mutex<HashSet<String>>,
}

// ---------------------------------------------------------------------------
// History types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HistoryEntry {
    crate_name: String,
    original_name: String,
    zip_bytes: usize,
    files: usize,
    language: String,
    timestamp: u64, // unix seconds
}

// ---------------------------------------------------------------------------
// Response types
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    version: &'static str,
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
}

fn err_response(status: StatusCode, msg: String) -> Response {
    (status, Json(ErrorBody { error: msg })).into_response()
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG")
                .unwrap_or_else(|_| "rustyfi_server=info,tower_http=warn".to_string()),
        )
        .init();

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(7410);

    let tmp = std::env::temp_dir();
    let output_base = tmp.join("rustyfi_output");
    let upload_base = tmp.join("rustyfi_uploads");
    std::fs::create_dir_all(&output_base).expect("cannot create output_base");
    std::fs::create_dir_all(&upload_base).expect("cannot create upload_base");

    let web_dir = find_web_dir();
    info!("Serving web assets from: {}", web_dir.display());

    let state = Arc::new(AppState {
        output_base,
        upload_base,
        history_lock: Mutex::new(()),
        active_runs: Mutex::new(HashSet::new()),
    });

    // Friendly startup check: warn loudly (but keep serving) when no LLM
    // provider is configured, so the user finds out now — not after they've
    // zipped, dragged and clicked Translate.
    match provider_status() {
        Ok(desc) => info!("LLM provider ready: {desc}"),
        Err(hint) => warn!("⚠ LLM provider NOT configured — translations will fail. {hint}"),
    }

    let app = Router::new()
        .route("/health", get(health))
        .route("/api/status", get(status_handler))
        .route("/api/translate", post(translate_handler))
        // axum 0.7 route params use `:name` — `{name}` would be a literal
        // segment and the download route would never match.
        .route("/api/download/:crate_name", get(download_handler))
        .route("/api/history", get(history_handler))
        .route("/api/community", get(community_handler))
        .route("/api/grok/login", post(grok_login_handler))
        .route("/api/grok/poll", post(grok_poll_handler))
        .fallback_service(ServeDir::new(&web_dir))
        .layer(DefaultBodyLimit::max(200 * 1024 * 1024)) // 200 MB
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods(Any)
                .allow_headers(Any),
        )
        .with_state(state);

    let addr = format!("0.0.0.0:{port}");
    info!("Rustyfi listening on http://{addr}");

    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

// ---------------------------------------------------------------------------
// Health
// ---------------------------------------------------------------------------

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
    })
}

// ---------------------------------------------------------------------------
// Status — provider / model info for the UI
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct StatusResponse {
    version: &'static str,
    provider: String,
    model: String,
    base_url: String,
    grok_authed: bool,
    /// True when the active provider has usable credentials — the UI uses
    /// this to warn *before* the user uploads anything.
    key_configured: bool,
    /// Model used for the verification fix loop (may differ from `model`).
    fix_model: String,
    /// True when the fix loop uses a *different* model than translation.
    fix_distinct: bool,
}

fn grok_auth_file_ok() -> bool {
    let home = std::env::var("HOME").unwrap_or_default();
    let path = std::path::PathBuf::from(&home)
        .join(".grok")
        .join("auth.json");
    path.exists()
        && std::fs::read_to_string(&path)
            .map(|s| s.contains("auth.x.ai") && s.contains("refresh_token"))
            .unwrap_or(false)
}

/// Human description of the configured provider, or a hint if unconfigured.
fn provider_status() -> Result<String, String> {
    let provider = std::env::var("RUSTYFI_PROVIDER")
        .unwrap_or_else(|_| "openai".to_string())
        .to_lowercase();
    if matches!(provider.as_str(), "grok" | "xai") {
        if grok_auth_file_ok() {
            Ok("xAI Grok via ~/.grok/auth.json".into())
        } else {
            Err(
                "RUSTYFI_PROVIDER=grok but ~/.grok/auth.json has no token — \
                 run `grok login` or use the Provider tab in the UI."
                    .into(),
            )
        }
    } else {
        let key_set = std::env::var("RUSTYFI_LLM_API_KEY")
            .map(|k| !k.trim().is_empty())
            .unwrap_or(false);
        if key_set {
            let base = std::env::var("RUSTYFI_LLM_BASE_URL")
                .unwrap_or_else(|_| "https://openrouter.ai/api/v1".into());
            Ok(format!("OpenAI-compatible endpoint at {base}"))
        } else {
            Err(
                "Set RUSTYFI_LLM_API_KEY (and optionally RUSTYFI_LLM_BASE_URL / \
                 RUSTYFI_LLM_MODEL) and restart."
                    .into(),
            )
        }
    }
}

async fn status_handler() -> Json<StatusResponse> {
    let provider = std::env::var("RUSTYFI_PROVIDER")
        .unwrap_or_else(|_| "openai".to_string())
        .to_lowercase();
    let is_grok = matches!(provider.as_str(), "grok" | "xai");
    let model = std::env::var("RUSTYFI_LLM_MODEL").unwrap_or_else(|_| {
        if is_grok {
            "grok-build".into()
        } else {
            "google/gemini-2.5-flash".into()
        }
    });
    let base_url = std::env::var("RUSTYFI_LLM_BASE_URL")
        .unwrap_or_else(|_| "https://openrouter.ai/api/v1".to_string());

    let grok_authed = grok_auth_file_ok();
    let key_configured = provider_status().is_ok();

    // Fix-loop model: RUSTYFI_FIX_MODEL overrides, else same as translation.
    let fix_model = std::env::var("RUSTYFI_FIX_MODEL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| model.clone());
    let fix_distinct = fix_model != model;

    Json(StatusResponse {
        version: env!("CARGO_PKG_VERSION"),
        provider,
        model,
        base_url,
        grok_authed,
        key_configured,
        fix_model,
        fix_distinct,
    })
}

// ---------------------------------------------------------------------------
// Grok OAuth device-code flow
// ---------------------------------------------------------------------------

async fn grok_login_handler() -> Response {
    match tokio::task::spawn_blocking(grok_device_code_start).await {
        Ok(Ok(dc)) => Json(dc).into_response(),
        Ok(Err(e)) => err_response(StatusCode::BAD_GATEWAY, e.to_string()),
        Err(e) => err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[derive(Deserialize)]
struct PollBody {
    device_code: String,
}

#[derive(Serialize)]
struct PollResponse {
    done: bool,
    access_token: Option<String>,
}

async fn grok_poll_handler(Json(body): Json<PollBody>) -> Response {
    let dc = body.device_code.clone();
    match tokio::task::spawn_blocking(move || grok_device_code_poll(&dc)).await {
        Ok(Ok(Some(tok))) => Json(PollResponse {
            done: true,
            access_token: Some(tok),
        })
        .into_response(),
        Ok(Ok(None)) => Json(PollResponse {
            done: false,
            access_token: None,
        })
        .into_response(),
        Ok(Err(e)) => err_response(StatusCode::BAD_GATEWAY, e.to_string()),
        Err(e) => err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

// ---------------------------------------------------------------------------
// History
// ---------------------------------------------------------------------------

async fn history_handler(State(state): State<Arc<AppState>>) -> Response {
    let history_path = state.output_base.join("history.json");
    let entries: Vec<HistoryEntry> = if history_path.exists() {
        std::fs::read_to_string(&history_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    } else {
        vec![]
    };
    // Return newest first
    let mut sorted = entries;
    sorted.sort_by_key(|x| std::cmp::Reverse(x.timestamp));
    (StatusCode::OK, Json(sorted)).into_response()
}

// ---------------------------------------------------------------------------
// Community (stub — returns curated list)
// ---------------------------------------------------------------------------

async fn community_handler() -> Response {
    // In the future this would fetch from a remote endpoint.
    // For now, return a hardcoded starter list of interesting repos people might try.
    let community: Vec<serde_json::Value> = vec![serde_json::json!({
        "name": "adOmnia",
        "description": "Multi-protocol API client (Go + Python)",
        "language": "go",
        "files": 100,
        "github": "https://github.com/Andrea-Cavallo/adOmnia",
        "submitted_by": "clubpenguin"
    })];
    (StatusCode::OK, Json(community)).into_response()
}

// ---------------------------------------------------------------------------
// Translate
// ---------------------------------------------------------------------------

async fn translate_handler(
    State(state): State<Arc<AppState>>,
    mut multipart: Multipart,
) -> Response {
    // Collect ZIP bytes.
    let mut zip_bytes: Option<Vec<u8>> = None;
    let mut filename_hint = "app".to_string();

    loop {
        match multipart.next_field().await {
            Ok(Some(field)) => {
                if field.name() == Some("archive") {
                    filename_hint = field
                        .file_name()
                        .map(|n| n.trim_end_matches(".zip").to_string())
                        .unwrap_or_else(|| "app".to_string());
                    match field.bytes().await {
                        Ok(b) => zip_bytes = Some(b.to_vec()),
                        Err(e) => {
                            return err_response(
                                StatusCode::PAYLOAD_TOO_LARGE,
                                format!(
                                    "Upload failed while reading the archive: {e}. \
                                     If your ZIP is over 200 MB, remove build artifacts \
                                     (node_modules, target, dist) and re-zip."
                                ),
                            )
                        }
                    }
                }
            }
            Ok(None) => break,
            Err(e) => {
                return err_response(
                    StatusCode::BAD_REQUEST,
                    format!(
                        "Upload failed: {e}. ZIPs over the 200 MB limit are rejected — \
                         trim build artifacts and try again."
                    ),
                )
            }
        }
    }

    let zip_bytes = match zip_bytes {
        Some(b) => b,
        None => return err_response(StatusCode::BAD_REQUEST, "No archive field".into()),
    };

    // Derive the crate name early so we can use it as a stable output dir.
    // Using the repo name (not a UUID) means the checkpoint survives server
    // restarts — re-submitting the same archive resumes from where it left off.
    //
    // The extraction dir is keyed by name + content fingerprint, NOT a random
    // UUID: checkpoints record absolute paths into this directory, so an
    // identical re-upload must extract to the same place for resume to find
    // the source files again.
    let crate_name = sanitise_crate_name(&filename_hint);
    let fingerprint = fnv1a_hex(&zip_bytes);
    let source_dir = state
        .upload_base
        .join(format!("{crate_name}-{fingerprint}"));
    let output_dir = state.output_base.join(&crate_name); // stable, name-based

    // One run per project at a time — concurrent runs would share the
    // checkpoint directory and corrupt each other.
    {
        let mut active = state.active_runs.lock().unwrap();
        if !active.insert(crate_name.clone()) {
            return err_response(
                StatusCode::CONFLICT,
                format!(
                    "`{crate_name}` is already being translated. Wait for that run to \
                     finish (or refresh to watch it)."
                ),
            );
        }
    }
    // From here on, every early return must release the lock.
    let release = |state: &AppState, name: &str| {
        state.active_runs.lock().unwrap().remove(name);
    };

    // Re-extract fresh each time (a previous partial extraction would be
    // indistinguishable from a complete one).
    let _ = std::fs::remove_dir_all(&source_dir);
    if let Err(e) =
        std::fs::create_dir_all(&source_dir).and_then(|_| std::fs::create_dir_all(&output_dir))
    {
        release(&state, &crate_name);
        return err_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Could not prepare working directories: {e}"),
        );
    }

    // Stale-checkpoint guard: the output dir is keyed by project NAME, but the
    // checkpoints inside are only valid for identical CONTENT. Same name +
    // different bytes → wipe and start fresh; same bytes → resume. A missing
    // fingerprint next to existing checkpoints is untrusted — reset too.
    let fp_path = output_dir.join("fingerprint");
    let previous = std::fs::read_to_string(&fp_path).unwrap_or_default();
    let untrusted = previous.is_empty() && output_dir.join("checkpoints").exists();
    if (!previous.is_empty() && previous != fingerprint) || untrusted {
        info!("`{crate_name}` re-uploaded with different content — resetting old run state");
        let _ = std::fs::remove_dir_all(&output_dir);
        if let Err(e) = std::fs::create_dir_all(&output_dir) {
            release(&state, &crate_name);
            return err_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Could not reset output directory: {e}"),
            );
        }
    }
    if let Err(e) = std::fs::write(&fp_path, &fingerprint) {
        release(&state, &crate_name);
        return err_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Could not record run state ({e}) — refusing to start an untrackable run."),
        );
    }

    if let Err(e) = extract_zip(&zip_bytes, &source_dir) {
        release(&state, &crate_name);
        return err_response(
            StatusCode::BAD_REQUEST,
            format!(
                "That ZIP couldn't be unpacked ({e}). Re-zip the project folder and try again."
            ),
        );
    }

    let upload_root = source_dir.clone(); // for cleanup after success
    let source_dir = unwrap_single_dir(source_dir);
    let zip_dest = state.output_base.join(format!("{crate_name}.zip"));
    let original_name = filename_hint.clone();

    // Count source files for history
    let file_count = count_source_files(&source_dir);

    let config = RunConfig {
        source_dir: source_dir.clone(),
        output_dir,
        crate_name: Some(crate_name.clone()),
        translate_retries: 3,
        // The fix loop visibly converges (e.g. 83→48→37 errors); 2 cycles
        // often cuts it off mid-descent. Allow more, overridable via env.
        verify_retries: std::env::var("RUSTYFI_VERIFY_RETRIES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(4),
        max_chunk_tokens: std::env::var("RUSTYFI_CHUNK_TOKENS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(5_000),
        parallel: std::env::var("RUSTYFI_PARALLEL")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(16),
        tier_fast_tokens: std::env::var("RUSTYFI_TIER_FAST")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(400),
        tier_mid_tokens: std::env::var("RUSTYFI_TIER_MID")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(3_000),
    };

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let tx_panic = tx.clone();
    let state_clone = state.clone();
    let state_mon = state.clone();
    let crate_name_c = crate_name.clone();
    let crate_name_m = crate_name.clone();

    let worker = tokio::task::spawn_blocking(move || {
        let result = run(config, |p| {
            let json = serde_json::to_string(&p).unwrap_or_default();
            let _ = tx.send(json);
        });
        match result {
            Ok(r) => {
                let zip_len = r.zip.len();
                if let Err(e) = std::fs::write(&zip_dest, &r.zip) {
                    // Done must never be sent unless the download will work.
                    let fail_event = serde_json::to_string(&Progress::Failed {
                        reason: format!(
                            "Couldn't save the result ZIP ({e}). Check disk space and try again."
                        ),
                    })
                    .unwrap_or_default();
                    let _ = tx.send(fail_event);
                    return;
                }

                // Append to history
                append_history(
                    &state_clone,
                    HistoryEntry {
                        crate_name: crate_name_c.clone(),
                        original_name,
                        zip_bytes: zip_len,
                        files: file_count,
                        language: r.language.clone(),
                        timestamp: unix_now(),
                    },
                );

                // Done is sent only AFTER the ZIP is on disk, so the download
                // button can never race the file write.
                let done_event = serde_json::to_string(&Progress::Done {
                    zip_bytes: zip_len,
                    crate_name: crate_name_c,
                    files_failed: r.files_failed,
                    cargo_clean: r.cargo_clean,
                    error_count: r.error_count,
                    todo_count: r.todo_count,
                    files_translated: r.files_translated,
                })
                .unwrap_or_default();
                let _ = tx.send(done_event);

                // The run finished — the extracted upload is no longer needed.
                // On failure it is kept: checkpoints point into it and resume
                // re-reads the pending source files from there.
                let _ = std::fs::remove_dir_all(&upload_root);
            }
            Err(e) => {
                let fail_event = serde_json::to_string(&Progress::Failed {
                    reason: e.to_string(),
                })
                .unwrap_or_default();
                let _ = tx.send(fail_event);
            }
        }
    });

    // Monitor: release the run lock and — if the engine panicked — make sure
    // the UI still receives a failed event instead of a silently dead stream.
    tokio::spawn(async move {
        let join = worker.await;
        if join.is_err() {
            let fail_event = serde_json::to_string(&Progress::Failed {
                reason: "Internal error in the translation engine (it crashed, sorry!). \
                         Your progress is checkpointed — drop the same ZIP to resume."
                    .to_string(),
            })
            .unwrap_or_default();
            let _ = tx_panic.send(fail_event);
        }
        state_mon.active_runs.lock().unwrap().remove(&crate_name_m);
    });

    let stream = async_stream::stream! {
        while let Some(json) = rx.recv().await {
            let is_done = json.contains("\"kind\":\"done\"") || json.contains("\"kind\":\"failed\"");
            yield Ok::<_, std::convert::Infallible>(
                axum::response::sse::Event::default().data(json)
            );
            if is_done { break; }
        }
    };

    Sse::new(stream)
        .keep_alive(axum::response::sse::KeepAlive::default())
        .into_response()
}

// ---------------------------------------------------------------------------
// Download
// ---------------------------------------------------------------------------

async fn download_handler(
    State(state): State<Arc<AppState>>,
    Path(crate_name): Path<String>,
) -> Response {
    let safe_name = sanitise_crate_name(&crate_name);
    let zip_path = state.output_base.join(format!("{safe_name}.zip"));

    if !zip_path.exists() {
        return err_response(StatusCode::NOT_FOUND, format!("{safe_name}.zip not found"));
    }

    let data = match std::fs::read(&zip_path) {
        Ok(d) => d,
        Err(e) => return err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };

    let headers = AppendHeaders([
        (header::CONTENT_TYPE, "application/zip".to_string()),
        (
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{safe_name}_rust.zip\""),
        ),
    ]);

    (StatusCode::OK, headers, data).into_response()
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

// Lowercase BEFORE filtering, and keep ASCII only: some Unicode lowercasings
// expand into combining marks (İ → i + U+0307), which would make the function
// non-idempotent — the Done event's crate_name would then re-sanitise to a
// different string in download_handler and 404.
fn sanitise_crate_name(name: &str) -> String {
    let s: String = name
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    // An empty name would make output_dir == output_base, and the stale-
    // checkpoint reset would then remove_dir_all the entire output base.
    if s.trim_matches('_').is_empty() {
        "app".to_string()
    } else if s.starts_with(|c: char| c.is_ascii_digit()) {
        format!("crate_{s}")
    } else {
        s
    }
}

/// Total decompressed bytes we're willing to extract (zip-bomb guard).
const MAX_EXTRACT_BYTES: u64 = 1024 * 1024 * 1024; // 1 GiB
const MAX_EXTRACT_FILES: usize = 50_000;

fn extract_zip(data: &[u8], dest: &std::path::Path) -> Result<(), String> {
    let cursor = std::io::Cursor::new(data);
    let mut arc = zip::ZipArchive::new(cursor).map_err(|e| e.to_string())?;
    if arc.len() > MAX_EXTRACT_FILES {
        return Err(format!(
            "archive contains {} entries (limit {MAX_EXTRACT_FILES})",
            arc.len()
        ));
    }
    let mut total: u64 = 0;
    for i in 0..arc.len() {
        let mut f = arc.by_index(i).map_err(|e| e.to_string())?;

        // enclosed_name() rejects path traversal and absolute paths outright.
        let Some(rel) = f.enclosed_name() else {
            continue;
        };

        // Skip macOS zip junk: __MACOSX/, AppleDouble (._foo) and .DS_Store —
        // AppleDouble files keep the source extension and would otherwise be
        // analysed and "translated" as garbage source files.
        let is_junk = rel.components().any(|c| {
            let name = c.as_os_str().to_string_lossy();
            name == "__MACOSX" || name == ".DS_Store" || name.starts_with("._")
        });
        if is_junk {
            continue;
        }

        let outpath = dest.join(rel);
        if f.name().ends_with('/') {
            std::fs::create_dir_all(&outpath).map_err(|e| e.to_string())?;
        } else {
            total = total.saturating_add(f.size());
            if total > MAX_EXTRACT_BYTES {
                return Err("archive expands beyond 1 GiB — that's a bit much for one drop".into());
            }
            if let Some(p) = outpath.parent() {
                std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
            }
            let mut out = std::fs::File::create(&outpath).map_err(|e| e.to_string())?;
            let mut buf = Vec::new();
            f.read_to_end(&mut buf).map_err(|e| e.to_string())?;
            out.write_all(&buf).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

fn unwrap_single_dir(dir: PathBuf) -> PathBuf {
    let entries: Vec<_> = std::fs::read_dir(&dir)
        .map(|rd| rd.flatten().collect())
        .unwrap_or_default();
    if entries.len() == 1 {
        let p = entries[0].path();
        if p.is_dir() {
            return p;
        }
    }
    dir
}

/// Deterministic content fingerprint (FNV-1a 64) — used to detect when a
/// re-uploaded ZIP with the same name has different content.
fn fnv1a_hex(data: &[u8]) -> String {
    let mut hash: u64 = 14695981039346656037;
    for &b in data {
        hash ^= b as u64;
        hash = hash.wrapping_mul(1099511628211);
    }
    format!("{hash:016x}")
}

fn find_web_dir() -> PathBuf {
    let exe = std::env::current_exe().unwrap_or_default();
    let mut dir = exe.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    for _ in 0..8 {
        let c = dir.join("web");
        if c.exists() && c.is_dir() {
            return c;
        }
        match dir.parent() {
            Some(p) => dir = p.to_path_buf(),
            None => break,
        }
    }
    std::env::current_dir().unwrap_or_default().join("web")
}

fn unix_now() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn count_source_files(dir: &std::path::Path) -> usize {
    walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_type().is_file()
                && e.path()
                    .extension()
                    .map(|x| {
                        matches!(
                            x.to_str(),
                            Some(
                                "go" | "py"
                                    | "ts"
                                    | "tsx"
                                    | "js"
                                    | "jsx"
                                    | "java"
                                    | "rb"
                                    | "c"
                                    | "cpp"
                                    | "cs"
                            )
                        )
                    })
                    .unwrap_or(false)
        })
        .count()
}

fn append_history(state: &AppState, entry: HistoryEntry) {
    let _lock = state.history_lock.lock();
    let path = state.output_base.join("history.json");
    let mut entries: Vec<HistoryEntry> = if path.exists() {
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    } else {
        vec![]
    };
    entries.push(entry);
    if let Ok(json) = serde_json::to_string_pretty(&entries) {
        let _ = std::fs::write(&path, json);
    }
}
