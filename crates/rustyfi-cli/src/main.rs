//! `rustyfi` — translate any codebase to Rust from the command line.
//!
//! No server, no browser: point it at a directory (or a `.zip`), and it drives
//! the same engine the web UI uses, writing a Cargo crate to disk. `cargo check`
//! is the oracle, so the exit code tells the truth:
//!   0 = compiles clean · 1 = compiles with errors (head-start + NEXT_STEPS) · 2 = failed.

mod progress;
mod unzip;

use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;
use console::style;
use rustyfi_engine::pipeline::{run, BehaviorSummary, DeepFixSummary, RunConfig, RunResult};
use rustyfi_engine::EngineError;

#[derive(Parser)]
#[command(
    name = "rustyfi",
    version,
    about = "Translate any codebase to Rust. cargo check is the oracle. 🎺🦀",
    long_about = None,
    subcommand_negates_reqs = true,
)]
struct Cli {
    /// Source project: a directory, or a .zip archive. (translate mode)
    #[arg(required = true)]
    source: Option<PathBuf>,

    /// Output directory for the generated Rust crate [default: <name>-rust].
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// Crate name [default: derived from the source].
    #[arg(short, long)]
    name: Option<String>,

    /// Files translated concurrently.
    #[arg(long)]
    parallel: Option<usize>,

    /// Maximum `cargo check` fix cycles.
    #[arg(long)]
    retries: Option<u32>,

    /// Suppress the live progress display (still prints the final summary).
    #[arg(short, long)]
    quiet: bool,

    /// Ignore cached checkpoints and translate from scratch.
    #[arg(long)]
    fresh: bool,

    /// Print a machine-readable JSON summary to stdout (implies --quiet).
    #[arg(long)]
    json: bool,

    /// Engage the deep-fix agent on residual errors (slower, costs more tokens).
    ///
    /// Runs a budget-capped agentic session after the standard fix loop if the
    /// crate is still not clean.  The session is automatically reverted if it
    /// does not improve the error count.  Budget is capped at 40 tool calls and
    /// 1200s by default; override with RUSTYFI_DEEP_FIX_BUDGET and
    /// RUSTYFI_DEEP_FIX_TIMEOUT.
    #[arg(long)]
    deep: bool,

    /// Skip the behavioral-equivalence phase (no source build/run).
    #[arg(long)]
    no_behavior: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(clap::Subcommand)]
enum Commands {
    /// Verify a translated crate's runtime behavior against its behavior.yaml.
    VerifyBehavior {
        /// Path to the translated Rust crate directory (containing behavior.yaml).
        crate_dir: PathBuf,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match real_main(cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("\n{} {e}", style("error:").red().bold());
            ExitCode::from(2)
        }
    }
}

fn real_main(cli: Cli) -> Result<ExitCode, String> {
    // Handle subcommands before any LLM preflight.
    if let Some(Commands::VerifyBehavior { crate_dir }) = &cli.command {
        return verify_behavior_cmd(crate_dir, cli.json);
    }

    // Set the deep-fix flag before preflight and run so the engine picks it up.
    // SAFETY: the CLI is single-threaded at this point; no other thread can
    // observe the env var until after `run` (which is called below).
    if cli.deep {
        unsafe { std::env::set_var("RUSTYFI_DEEP_FIX", "1") };
    }

    preflight_env()?;

    let source = cli
        .source
        .clone()
        .expect("clap guarantees source unless a subcommand is present");

    // Resolve the source into a directory (extracting a .zip if needed).
    let raw = source
        .canonicalize()
        .map_err(|_| format!("source not found: {}", source.display()))?;
    let _scratch; // keeps a tempdir alive for the whole run
    let source_dir = if raw.is_file() {
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        unzip::extract_file(&raw, dir.path())
            .map_err(|e| format!("reading {}: {e}", raw.display()))?;
        let path = descend_single(dir.path());
        _scratch = dir;
        path
    } else {
        raw.clone()
    };

    let name = cli
        .name
        .clone()
        .unwrap_or_else(|| derive_name(&source_dir, &source));
    let output = cli
        .output
        .clone()
        .unwrap_or_else(|| PathBuf::from(format!("{name}-rust")));

    // Work dir keyed by a content fingerprint → identical re-runs resume,
    // changed sources start fresh automatically (no stale-resume footgun).
    let fp = fingerprint(&source_dir);
    let work = std::env::temp_dir()
        .join("rustyfi-cli")
        .join(format!("{name}-{fp}"));
    if cli.fresh {
        let _ = fs::remove_dir_all(&work);
    }

    if !cli.json {
        print_banner(&source_dir, &output, &name);
    }

    let rich = !cli.quiet && !cli.json && std::io::stderr().is_terminal();
    let mut ui = progress::Ui::new(rich);

    let config = RunConfig {
        source_dir,
        output_dir: work,
        crate_name: Some(name.clone()),
        translate_retries: 3,
        verify_retries: cli
            .retries
            .or_else(|| env_u32("RUSTYFI_VERIFY_RETRIES"))
            .unwrap_or(4),
        max_chunk_tokens: env_usize("RUSTYFI_CHUNK_TOKENS").unwrap_or(5000),
        parallel: cli
            .parallel
            .or_else(|| env_usize("RUSTYFI_PARALLEL"))
            .unwrap_or(16),
        tier_fast_tokens: env_usize("RUSTYFI_TIER_FAST").unwrap_or(400),
        tier_mid_tokens: env_usize("RUSTYFI_TIER_MID").unwrap_or(3000),
        verify_behavior: !cli.no_behavior,
    };

    let started = std::time::Instant::now();
    let outcome = run(config, |p| ui.handle(&p));
    ui.finish();
    let result = outcome.map_err(friendly_engine_err)?;

    fs::create_dir_all(&output).map_err(|e| e.to_string())?;
    let files = unzip::extract_bytes_stripping_root(&result.zip, &output)
        .map_err(|e| format!("writing output crate: {e}"))?;

    if cli.json {
        let translate_model = rustyfi_engine::llm::LlmClient::from_env()
            .map(|c| c.model().to_string())
            .unwrap_or_else(|_| "unknown".into());
        let fix_model = rustyfi_engine::llm::LlmClient::for_fixing()
            .map(|c| c.model().to_string())
            .unwrap_or_else(|_| translate_model.clone());
        let summary = build_json_summary(
            &result,
            &output,
            files,
            started.elapsed().as_secs_f64(),
            &translate_model,
            &fix_model,
        );
        println!("{summary}");
    } else {
        print_summary(&result, &output, files);
    }
    Ok(if result.cargo_clean {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

fn verify_behavior_cmd(crate_dir: &std::path::Path, json: bool) -> Result<ExitCode, String> {
    use rustyfi_engine::behavior::{verify, BehaviorSpec};
    let spec_path = crate_dir.join("behavior.yaml");
    let yaml = std::fs::read_to_string(&spec_path)
        .map_err(|e| format!("no behavior.yaml in {}: {e}", crate_dir.display()))?;
    let spec: BehaviorSpec =
        serde_yaml::from_str(&yaml).map_err(|e| format!("invalid behavior.yaml: {e}"))?;
    let work = crate_dir.join(".behavior-work");
    let _ = std::fs::create_dir_all(&work);
    let report = verify(&spec, crate_dir, &work).map_err(|e| format!("verify failed: {e}"))?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?
        );
    } else {
        eprintln!(
            "\nbehavior: {} ({}/{} matched, {} quarantined)\n",
            report.name, report.matched, report.total, report.quarantined
        );
        for c in &report.cases {
            eprintln!("  {} {}", if c.matched { "✓" } else { "✗" }, c.name);
            for d in &c.diffs {
                eprintln!("      {d}");
            }
        }
    }
    Ok(if report.passed {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

// ── output ──────────────────────────────────────────────────────────────────

fn print_banner(source: &Path, output: &Path, name: &str) {
    eprintln!();
    eprintln!(
        "  {} {}",
        style("rustyfi").bold().yellow(),
        style("🎺🦀").dim()
    );
    eprintln!("  {}  {}", style("source").dim(), source.display());
    eprintln!(
        "  {}  {}  {}",
        style("output").dim(),
        output.display(),
        style(format!("(crate: {name})")).dim()
    );
    eprintln!();
}

fn print_summary(r: &RunResult, output: &Path, files: usize) {
    eprintln!();
    let header = if r.cargo_clean {
        format!("{} compiles clean", style("✓").green().bold())
    } else if r.error_count > 0 {
        format!(
            "{} compiles with {} error(s) remaining",
            style("⚠").yellow().bold(),
            r.error_count
        )
    } else {
        format!("{} done", style("✓").green().bold())
    };
    eprintln!("  {header}");
    eprintln!(
        "  {} {} from {} · {} file(s) written · {} todo!() stub(s)",
        style("·").dim(),
        style(format!("{} translated", r.files_translated)).bold(),
        r.language,
        files,
        r.todo_count,
    );
    if r.files_failed > 0 {
        eprintln!(
            "  {} {} file(s) fell back to a stub",
            style("·").dim(),
            r.files_failed
        );
    }
    eprintln!();
    // The crate path goes to STDOUT so it can be captured/piped.
    println!("{}", output.display());
    if !r.cargo_clean {
        eprintln!(
            "  {} read {} for what's left to finish.",
            style("→").cyan(),
            style(output.join("NEXT_STEPS.md").display()).underlined(),
        );
    } else {
        eprintln!(
            "  {} cd {} && cargo run",
            style("→").cyan(),
            output.display(),
        );
    }
    eprintln!();
}

/// One-line machine-readable run summary (the `--json` contract).
/// Schema documented in bench/README.md; exit_code mirrors the process exit.
fn build_json_summary(
    r: &RunResult,
    output: &Path,
    files_written: usize,
    duration_secs: f64,
    translate_model: &str,
    fix_model: &str,
) -> serde_json::Value {
    let deep_fix = r
        .deep_fix
        .as_ref()
        .map(deep_fix_to_json)
        .unwrap_or(serde_json::Value::Null);

    let behavior = r
        .behavior
        .as_ref()
        .map(behavior_to_json)
        .unwrap_or(serde_json::Value::Null);

    serde_json::json!({
        "crate_name": r.crate_name,
        "crate_path": output.to_string_lossy(),
        "language": r.language,
        "files_total": r.files_translated + r.files_failed,
        "files_translated": r.files_translated,
        "files_failed": r.files_failed,
        "files_written": files_written,
        "errors": r.error_count,
        "todos": r.todo_count,
        "cargo_clean": r.cargo_clean,
        "duration_secs": duration_secs,
        "translate_model": translate_model,
        "fix_model": fix_model,
        "exit_code": if r.cargo_clean { 0 } else { 1 },
        "deep_fix": deep_fix,
        "behavior": behavior,
    })
}

fn deep_fix_to_json(s: &DeepFixSummary) -> serde_json::Value {
    serde_json::json!({
        "ran": s.ran,
        "start_errors": s.start_errors,
        "end_errors": s.end_errors,
        "tool_calls": s.tool_calls,
    })
}

fn behavior_to_json(b: &BehaviorSummary) -> serde_json::Value {
    serde_json::json!({
        "ran": b.ran,
        "verified": b.verified,
        "matched": b.matched,
        "total": b.total,
        "quarantined": b.quarantined,
    })
}

// ── env / preflight ──────────────────────────────────────────────────────────

fn preflight_env() -> Result<(), String> {
    let provider = std::env::var("RUSTYFI_PROVIDER")
        .unwrap_or_default()
        .to_lowercase();
    if provider == "grok" || provider == "xai" {
        return Ok(());
    }
    let has_key = std::env::var("RUSTYFI_LLM_API_KEY")
        .map(|k| !k.trim().is_empty())
        .unwrap_or(false);
    if has_key {
        return Ok(());
    }
    Err(format!(
        "no LLM provider configured.\n\n  Set an API key (any OpenAI-compatible endpoint):\n\
         \n    {}\n    {}\n    {}\n\n  …or use Grok OAuth with {}.",
        style("export RUSTYFI_LLM_API_KEY=\"sk-…\"").cyan(),
        style("export RUSTYFI_LLM_BASE_URL=\"https://api.deepseek.com\"").cyan(),
        style("export RUSTYFI_LLM_MODEL=\"deepseek-chat\"").cyan(),
        style("RUSTYFI_PROVIDER=grok").cyan(),
    ))
}

fn friendly_engine_err(e: EngineError) -> String {
    match e {
        EngineError::Config(msg) => {
            format!("{msg}\n  (check your RUSTYFI_LLM_* environment variables)")
        }
        EngineError::NoSourceFiles { path } => {
            format!("no translatable source files found in {}", path.display())
        }
        other => other.to_string(),
    }
}

fn env_usize(key: &str) -> Option<usize> {
    std::env::var(key).ok()?.trim().parse().ok()
}
fn env_u32(key: &str) -> Option<u32> {
    std::env::var(key).ok()?.trim().parse().ok()
}

// ── helpers ──────────────────────────────────────────────────────────────────

/// If `dir` contains exactly one entry and it's a directory, descend into it
/// (a zip of `myapp/` extracts to `tmp/myapp/`).
fn descend_single(dir: &Path) -> PathBuf {
    let mut entries: Vec<PathBuf> = match fs::read_dir(dir) {
        Ok(rd) => rd.filter_map(|e| e.ok()).map(|e| e.path()).collect(),
        Err(_) => return dir.to_path_buf(),
    };
    entries.retain(|p| p.file_name().map(|n| n != "__MACOSX").unwrap_or(true));
    if entries.len() == 1 && entries[0].is_dir() {
        return entries[0].clone();
    }
    dir.to_path_buf()
}

/// Crate-name sanitiser: lowercase, ASCII alnum + `_`, never empty. Mirrors the
/// server so the same project yields the same name on either entry point.
fn derive_name(source_dir: &Path, original: &Path) -> String {
    let raw = source_dir
        .file_name()
        .or_else(|| original.file_stem())
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let mut out: String = raw
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    out = out.trim_matches('_').to_string();
    if out.is_empty()
        || !out
            .chars()
            .next()
            .map(|c| c.is_ascii_alphabetic())
            .unwrap_or(false)
    {
        out = format!("project_{out}");
    }
    out.trim_matches('_').to_string()
}

/// Deterministic content fingerprint of a source tree (path + bytes of each
/// file, in sorted order). `DefaultHasher` has fixed keys → stable across runs.
fn fingerprint(dir: &Path) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    let mut files: Vec<PathBuf> = walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.into_path())
        .collect();
    files.sort();
    for f in &files {
        f.strip_prefix(dir)
            .unwrap_or(f)
            .to_string_lossy()
            .hash(&mut hasher);
        if let Ok(bytes) = fs::read(f) {
            bytes.hash(&mut hasher);
        }
    }
    format!("{:016x}", hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_name_sanitises() {
        assert_eq!(
            derive_name(Path::new("/x/My-App"), Path::new("My-App")),
            "my_app"
        );
        assert_eq!(
            derive_name(Path::new("/x/cool.thing"), Path::new("cool.thing")),
            "cool_thing"
        );
        // must start with a letter
        assert_eq!(
            derive_name(Path::new("/x/123go"), Path::new("123go")),
            "project_123go"
        );
    }

    #[test]
    fn json_summary_has_all_contract_fields() {
        let r = RunResult {
            zip: vec![],
            crate_name: "demo".into(),
            language: "go".into(),
            files_failed: 1,
            cargo_clean: false,
            error_count: 42,
            todo_count: 12,
            files_translated: 23,
            deep_fix: None,
            behavior: None,
        };
        let v = build_json_summary(
            &r,
            Path::new("/out/demo-rust"),
            13,
            271.5,
            "deepseek-chat",
            "deepseek-reasoner",
        );
        assert_eq!(v["crate_name"], "demo");
        assert_eq!(v["crate_path"], "/out/demo-rust");
        assert_eq!(v["language"], "go");
        assert_eq!(v["files_total"], 24); // translated + failed
        assert_eq!(v["files_translated"], 23);
        assert_eq!(v["files_failed"], 1);
        assert_eq!(v["files_written"], 13);
        assert_eq!(v["errors"], 42);
        assert_eq!(v["todos"], 12);
        assert_eq!(v["cargo_clean"], false);
        assert_eq!(v["duration_secs"], 271.5);
        assert_eq!(v["translate_model"], "deepseek-chat");
        assert_eq!(v["fix_model"], "deepseek-reasoner");
        assert_eq!(v["exit_code"], 1);
        // deep_fix absent → null
        assert_eq!(v["deep_fix"], serde_json::Value::Null);
        // behavior absent → null
        assert_eq!(v["behavior"], serde_json::Value::Null);
    }

    #[test]
    fn json_summary_deep_fix_field_when_present() {
        use rustyfi_engine::pipeline::DeepFixSummary;
        let r = RunResult {
            zip: vec![],
            crate_name: "demo".into(),
            language: "go".into(),
            files_failed: 0,
            cargo_clean: true,
            error_count: 0,
            todo_count: 0,
            files_translated: 5,
            deep_fix: Some(DeepFixSummary {
                ran: true,
                start_errors: 10,
                end_errors: 0,
                tool_calls: 8,
            }),
            behavior: None,
        };
        let v = build_json_summary(
            &r,
            Path::new("/out/demo-rust"),
            5,
            60.0,
            "deepseek-chat",
            "deepseek-reasoner",
        );
        let df = &v["deep_fix"];
        assert_eq!(df["ran"], true);
        assert_eq!(df["start_errors"], 10);
        assert_eq!(df["end_errors"], 0);
        assert_eq!(df["tool_calls"], 8);
    }

    #[test]
    fn json_summary_behavior_null_when_absent() {
        let r = RunResult {
            zip: vec![],
            crate_name: "demo".into(),
            language: "python".into(),
            files_failed: 0,
            cargo_clean: true,
            error_count: 0,
            todo_count: 0,
            files_translated: 3,
            deep_fix: None,
            behavior: None,
        };
        let v = build_json_summary(
            &r,
            Path::new("/out/demo-rust"),
            3,
            10.0,
            "deepseek-chat",
            "deepseek-chat",
        );
        assert_eq!(v["behavior"], serde_json::Value::Null);
    }

    #[test]
    fn json_summary_behavior_fields_when_present() {
        let r = RunResult {
            zip: vec![],
            crate_name: "demo".into(),
            language: "python".into(),
            files_failed: 0,
            cargo_clean: true,
            error_count: 0,
            todo_count: 0,
            files_translated: 3,
            deep_fix: None,
            behavior: Some(BehaviorSummary {
                ran: true,
                verified: true,
                matched: 2,
                total: 3,
                quarantined: 1,
            }),
        };
        let v = build_json_summary(
            &r,
            Path::new("/out/demo-rust"),
            3,
            10.0,
            "deepseek-chat",
            "deepseek-chat",
        );
        assert_eq!(v["behavior"]["matched"], 2);
        assert_eq!(v["behavior"]["verified"], true);
        assert_eq!(v["behavior"]["ran"], true);
        assert_eq!(v["behavior"]["total"], 3);
        assert_eq!(v["behavior"]["quarantined"], 1);
    }
}
