use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Multipart, Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response, Sse};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Serialize;
use tower_http::cors::{Any, CorsLayer};
use tower_http::services::ServeDir;
use tracing::info;

use rustyfi_engine::pipeline::{run, Progress, RunConfig};

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

struct AppState {
    output_base: PathBuf,
    upload_base: PathBuf,
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

    let tmp          = std::env::temp_dir();
    let output_base  = tmp.join("rustyfi_output");
    let upload_base  = tmp.join("rustyfi_uploads");
    std::fs::create_dir_all(&output_base).expect("cannot create output_base");
    std::fs::create_dir_all(&upload_base).expect("cannot create upload_base");

    let web_dir = find_web_dir();
    info!("Serving web assets from: {}", web_dir.display());

    let state = Arc::new(AppState { output_base, upload_base });

    let app = Router::new()
        .route("/health",                    get(health))
        .route("/api/translate",             post(translate_handler))
        .route("/api/download/{crate_name}", get(download_handler))
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods(Any)
                .allow_headers(Any),
        )
        .nest_service("/", ServeDir::new(&web_dir))
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
    Json(HealthResponse { status: "ok", version: env!("CARGO_PKG_VERSION") })
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

    while let Ok(Some(field)) = multipart.next_field().await {
        if field.name() == Some("archive") {
            filename_hint = field
                .file_name()
                .map(|n| n.trim_end_matches(".zip").to_string())
                .unwrap_or_else(|| "app".to_string());
            match field.bytes().await {
                Ok(b) => zip_bytes = Some(b.to_vec()),
                Err(e) => return err_response(StatusCode::BAD_REQUEST, e.to_string()),
            }
        }
    }

    let zip_bytes = match zip_bytes {
        Some(b) => b,
        None => return err_response(StatusCode::BAD_REQUEST, "No archive field".into()),
    };

    let run_id     = uuid::Uuid::new_v4().to_string();
    let source_dir = state.upload_base.join(&run_id);
    let output_dir = state.output_base.join(&run_id);
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::create_dir_all(&output_dir).unwrap();

    if let Err(e) = extract_zip(&zip_bytes, &source_dir) {
        return err_response(StatusCode::BAD_REQUEST, format!("ZIP extraction: {e}"));
    }

    let source_dir = unwrap_single_dir(source_dir);
    let crate_name = sanitise_crate_name(&filename_hint);
    let zip_dest   = state.output_base.join(format!("{crate_name}.zip"));

    let config = RunConfig {
        source_dir,
        output_dir,
        translate_retries: 3,
        verify_retries: 2,
        max_chunk_tokens: std::env::var("RUSTYFI_CHUNK_TOKENS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(5_000),
    };


    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let tx_clone     = tx.clone();

    tokio::task::spawn_blocking(move || {
        let result = run(config, |p| {
            let json = serde_json::to_string(&p).unwrap_or_default();
            let _ = tx.send(json);
        });
        match result {
            Ok(r) => {
                let _ = std::fs::write(&zip_dest, &r.zip);
                let done_event = serde_json::to_string(&Progress::Done {
                    zip_bytes: r.zip.len(),
                })
                .unwrap_or_default();
                let _ = tx.send(done_event);
            }
            Err(e) => {
                let fail_event = serde_json::to_string(&Progress::Failed {
                    reason: e.to_string(),
                })
                .unwrap_or_default();
                let _ = tx.send(fail_event);
            }
        }
        drop(tx_clone); // ensure sender is closed
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
    let zip_path  = state.output_base.join(format!("{safe_name}.zip"));

    if !zip_path.exists() {
        return err_response(StatusCode::NOT_FOUND, format!("{safe_name}.zip not found"));
    }

    let data = match std::fs::read(&zip_path) {
        Ok(d) => d,
        Err(e) => return err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };

    let headers = [
        (header::CONTENT_TYPE, "application/zip"),
        (
            header::CONTENT_DISPOSITION,
            Box::leak(
                format!("attachment; filename=\"{safe_name}_rust.zip\"").into_boxed_str()
            ),
        ),
    ];

    (StatusCode::OK, headers, data).into_response()
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn sanitise_crate_name(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '_' { c } else { '_' })
        .collect::<String>()
        .to_lowercase();
    if s.starts_with(|c: char| c.is_ascii_digit()) {
        format!("crate_{s}")
    } else {
        s
    }
}

fn extract_zip(data: &[u8], dest: &std::path::Path) -> Result<(), String> {
    let cursor  = std::io::Cursor::new(data);
    let mut arc = zip::ZipArchive::new(cursor).map_err(|e| e.to_string())?;
    for i in 0..arc.len() {
        let mut f   = arc.by_index(i).map_err(|e| e.to_string())?;
        let outpath = dest.join(f.mangled_name());
        if f.name().ends_with('/') {
            std::fs::create_dir_all(&outpath).map_err(|e| e.to_string())?;
        } else {
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
        if p.is_dir() { return p; }
    }
    dir
}

fn find_web_dir() -> PathBuf {
    // Walk up from binary location to find web/
    let exe = std::env::current_exe().unwrap_or_default();
    let mut dir = exe.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    for _ in 0..8 {
        let c = dir.join("web");
        if c.exists() && c.is_dir() { return c; }
        match dir.parent() {
            Some(p) => dir = p.to_path_buf(),
            None    => break,
        }
    }
    std::env::current_dir().unwrap_or_default().join("web")
}
