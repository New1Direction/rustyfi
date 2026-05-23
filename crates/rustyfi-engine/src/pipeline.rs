use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use rustyfi_core::compiler::parse_cargo_diagnostics;
use rustyfi_core::context::LanguageMetadata;
use rustyfi_core::state::{CargoOutput, DiagnosticFamily, LtoMode, ReleaseConfig};
use rustyfi_core::{ContextManifest, Orchestrator, StateEvent};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::analysis::SourceAnalyser;
use crate::checkpoint::{
    AnalysisCheckpoint, CheckpointStore, FileTranslation, FixCycleSummary,
    PackagingCheckpoint, ScaffoldCheckpoint, TranslationCheckpoint, VerificationCheckpoint,
};
use crate::chunker::SemanticChunker;
use crate::graph::{EdgeRecord, ModuleGraph};
use crate::llm::{
    extract_rust_code, prompt_fix_targeted, prompt_translate_with_context,
    LlmClient, SYSTEM_FIX, SYSTEM_TRANSLATE,
};
use crate::scaffold::{package_to_zip, Scaffolder};
use crate::slicer::OwnershipGraph;
use crate::EngineError;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Configuration for a translation run.
#[derive(Debug, Clone)]
pub struct RunConfig {
    /// Absolute path to the source directory to translate.
    pub source_dir: PathBuf,
    /// Absolute path where the generated Cargo project and checkpoints live.
    pub output_dir: PathBuf,
    /// Maximum LLM translation retries per source file.
    pub translate_retries: u32,
    /// Maximum `cargo check` fix cycles.
    pub verify_retries: u32,
    /// Token budget per semantic chunk (default: 5 000).
    pub max_chunk_tokens: usize,
}

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            source_dir:        PathBuf::new(),
            output_dir:        PathBuf::new(),
            translate_retries: 3,
            verify_retries:    5,
            max_chunk_tokens:  5_000,
        }
    }
}

/// Progress events emitted during a run, streamed to the browser via SSE.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Progress {
    StateChanged      { state: &'static str },
    PhaseResumed      { phase: String },
    FileStarted       { file: String, index: usize, total: usize },
    ChunkStarted      { file: String, chunk: usize, total: usize, symbols: Vec<String> },
    FileComplete      { file: String, chunks: usize, signatures: usize },
    CompilerError     { message: String, families: Vec<String> },
    FixCycle          { attempt: u32 },
    Done              { zip_bytes: usize },
    Failed            { reason: String },
}

/// Output of a completed pipeline run.
pub struct RunResult {
    pub zip:        Vec<u8>,
    pub crate_name: String,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Run the full Rustyfi pipeline with graph-scheduled, semantically-chunked,
/// context-injected translation.
///
/// ## Resumption
/// Each phase writes a checkpoint on success.  If a prior run completed
/// "analysis" and "scaffold" but crashed mid-translation, this function will
/// skip those phases and resume from the last completed file.
pub fn run<F>(config: RunConfig, mut progress_cb: F) -> Result<RunResult, EngineError>
where
    F: FnMut(Progress),
{
    let run_dir = config.output_dir.clone();
    fs::create_dir_all(&run_dir).map_err(|e| EngineError::Io(e.to_string()))?;

    let store = CheckpointStore::new(&run_dir)?;
    let llm   = LlmClient::from_env()?;
    let mut orch = Orchestrator::new();

    // ── Phase 1: Analysis ─────────────────────────────────────────────────
    let analysis_cp = if let Some(cp) = store.read::<AnalysisCheckpoint>("analysis") {
        emit(&mut progress_cb, Progress::PhaseResumed { phase: "analysis".into() });
        cp
    } else {
        emit(&mut progress_cb, Progress::StateChanged { state: "Parsing" });
        let cp = phase_analyse(&config, &store, &mut orch)?;
        store.write("analysis", &cp)?;
        cp
    };

    // ── Phase 2: Scaffold ─────────────────────────────────────────────────
    let scaffold_cp = if let Some(cp) = store.read::<ScaffoldCheckpoint>("scaffold") {
        emit(&mut progress_cb, Progress::PhaseResumed { phase: "scaffold".into() });
        cp
    } else {
        emit(&mut progress_cb, Progress::StateChanged { state: "Scaffolding" });
        let cp = phase_scaffold(&config, &analysis_cp, &mut orch)?;
        store.write("scaffold", &cp)?;
        cp
    };

    // ── Phase 3: Translation (graph-scheduled, semantically-chunked) ──────
    let translation_cp = {
        let existing: Option<TranslationCheckpoint> = store.read("translation");
        let resume_from = existing.as_ref().map(|c| c.next_index).unwrap_or(0);

        if resume_from == 0 {
            emit(&mut progress_cb, Progress::StateChanged { state: "Translating" });
        } else {
            info!("Resuming translation from index {resume_from}");
            emit(&mut progress_cb, Progress::PhaseResumed {
                phase: format!("translation (file {resume_from})"),
            });
        }

        phase_translate(
            &config,
            &store,
            &llm,
            &analysis_cp,
            &scaffold_cp,
            existing,
            &mut progress_cb,
            &mut orch,
        )?
    };

    // ── Phase 4: Verification + targeted fix loop ─────────────────────────
    let verification_cp = if let Some(cp) = store.read::<VerificationCheckpoint>("verification") {
        emit(&mut progress_cb, Progress::PhaseResumed { phase: "verification".into() });
        cp
    } else {
        emit(&mut progress_cb, Progress::StateChanged { state: "Verifying" });
        let cp = phase_verify(
            &config,
            &llm,
            &scaffold_cp,
            &translation_cp,
            &mut progress_cb,
        )?;
        store.write("verification", &cp)?;
        cp
    };

    // ── Phase 5: Packaging ────────────────────────────────────────────────
    let packaging_cp = if let Some(cp) = store.read::<PackagingCheckpoint>("packaging") {
        emit(&mut progress_cb, Progress::PhaseResumed { phase: "packaging".into() });
        cp
    } else {
        emit(&mut progress_cb, Progress::StateChanged { state: "Completed" });
        let cp = phase_package(&scaffold_cp, &run_dir)?;
        store.write("packaging", &cp)?;
        cp
    };

    // Advance orchestrator to Completed.
    orch.transition(StateEvent::VerifyPassed {
        release_config: ReleaseConfig {
            opt_level: "3".into(),
            lto: LtoMode::Thin,
            codegen_units: 1,
            strip_debug: false,
        },
    })
    .ok();
    orch.transition(StateEvent::OptimizationComplete {
        artifact_locations: vec![scaffold_cp.workspace_path.clone()],
        build_metadata: HashMap::from([
            ("crate_name".into(), packaging_cp.crate_name.clone()),
            ("zip_bytes".into(),  packaging_cp.zip_bytes.to_string()),
            ("fix_cycles".into(), verification_cp.fix_cycles.len().to_string()),
            (
                "chunk_total".into(),
                translation_cp.total_chunks_processed.to_string(),
            ),
        ]),
    })
    .ok();

    let zip = fs::read(&packaging_cp.zip_path)
        .map_err(|e| EngineError::Io(format!("read ZIP: {e}")))?;

    emit(&mut progress_cb, Progress::Done { zip_bytes: packaging_cp.zip_bytes });
    info!(
        "Run complete. crate={} zip={} bytes chunks={} fix_cycles={}",
        packaging_cp.crate_name,
        packaging_cp.zip_bytes,
        translation_cp.total_chunks_processed,
        verification_cp.fix_cycles.len(),
    );

    Ok(RunResult {
        zip,
        crate_name: packaging_cp.crate_name,
    })
}

// ---------------------------------------------------------------------------
// Phase 1: Analysis
// ---------------------------------------------------------------------------

fn phase_analyse(
    config: &RunConfig,
    store: &CheckpointStore,
    orch: &mut Orchestrator,
) -> Result<AnalysisCheckpoint, EngineError> {
    let analyser = SourceAnalyser::new(config.source_dir.clone());
    let analysis = analyser.analyse()?;

    for w in &analysis.warnings {
        warn!("[parser] {:?}: {}", w.file, w.message);
    }

    let crate_name = sanitise_crate_name(
        config.source_dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "app".to_string()),
    );

    let lang = lang_display(&analysis.language_metadata);

    // Convert edges to serialisable EdgeRecords.
    let edges: Vec<EdgeRecord> = analysis.dependency_edges.iter().map(EdgeRecord::from).collect();

    let manifest = ContextManifest {
        run_id:               Uuid::new_v4().to_string(),
        workspace_root:       config.source_dir.clone(),
        source_targets:       analysis.targets.clone(),
        dependency_edges:     analysis.dependency_edges.clone(),
        external_packages:    vec![],
        filesystem_boundaries: vec![],
        external_io_boundaries: vec![],
        inferred_entrypoints: analysis.inferred_entrypoints.clone(),
        parser_warnings:      analysis.warnings.clone(),
        language_metadata:    analysis.language_metadata.clone(),
        produced_at:          unix_now(),
    };

    orch.transition(StateEvent::StartParsing { manifest: Box::new(manifest) })
        .map_err(|e| EngineError::Orchestrator(e.to_string()))?;

    // Write analysis summary artifact.
    let artifact_dir = store.artifact_dir("analysis");
    let _ = fs::write(
        artifact_dir.join("summary.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "targets":  analysis.targets.len(),
            "language": lang,
            "edges":    edges.len(),
            "warnings": analysis.warnings.len(),
        }))
        .unwrap_or_default(),
    );

    Ok(AnalysisCheckpoint {
        source_dir:           config.source_dir.clone(),
        crate_name,
        language:             lang,
        target_paths:         analysis.targets.iter().map(|t| t.path.clone()).collect(),
        inferred_entrypoints: analysis.inferred_entrypoints,
        edges,
        warning_count:        analysis.warnings.len(),
        produced_at:          unix_now(),
    })
}

// ---------------------------------------------------------------------------
// Phase 2: Scaffold
// ---------------------------------------------------------------------------

fn phase_scaffold(
    config: &RunConfig,
    analysis: &AnalysisCheckpoint,
    orch: &mut Orchestrator,
) -> Result<ScaffoldCheckpoint, EngineError> {
    let workspace_path = config.output_dir.join(&analysis.crate_name);
    let scaffolder = Scaffolder::new(workspace_path.clone(), analysis.crate_name.clone());
    scaffolder.scaffold()?;

    let module_plan: Vec<String> = analysis
        .target_paths
        .iter()
        .map(|p| module_label(p, &analysis.source_dir))
        .collect();

    let first_file = analysis.target_paths.first().cloned()
        .unwrap_or_else(|| analysis.source_dir.join("main.py"));

    orch.transition(StateEvent::ParseComplete {
        workspace_path: workspace_path.clone(),
        dependency_manifest: HashMap::new(),
        module_layout_plan: module_plan.clone(),
    })
    .map_err(|e| EngineError::Orchestrator(e.to_string()))?;

    orch.transition(StateEvent::ScaffoldComplete {
        first_file,
        total_chunks: analysis.target_paths.len() as u32,
        retry_ceiling: config.translate_retries,
    })
    .map_err(|e| EngineError::Orchestrator(e.to_string()))?;

    Ok(ScaffoldCheckpoint {
        workspace_path,
        crate_name: analysis.crate_name.clone(),
        module_plan,
    })
}

// ---------------------------------------------------------------------------
// Phase 3: Translation — DAG-scheduled, semantically chunked, context-injected
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn phase_translate<F>(
    config: &RunConfig,
    store: &CheckpointStore,
    llm: &LlmClient,
    analysis: &AnalysisCheckpoint,
    scaffold: &ScaffoldCheckpoint,
    existing: Option<TranslationCheckpoint>,
    progress_cb: &mut F,
    orch: &mut Orchestrator,
) -> Result<TranslationCheckpoint, EngineError>
where
    F: FnMut(Progress),
{
    // ── Build Module DAG from analysis edges ────────────────────────────────
    let graph = ModuleGraph::build(&analysis.target_paths, &analysis.edges);
    let ordered_paths = graph.translation_order();

    debug!(
        "ModuleGraph: {} nodes, {} edges → scheduled {} files",
        graph.len(),
        analysis.edges.len(),
        ordered_paths.len(),
    );

    // ── Restore or init checkpoint ──────────────────────────────────────────
    let mut checkpoint = existing.unwrap_or_else(|| TranslationCheckpoint {
        completed:              vec![],
        next_index:             0,
        module_names:           vec![],
        ownership:              OwnershipGraph::new(),
        total_chunks_processed: 0,
    });

    // The ordered_paths list may differ from target_paths in ordering.
    // next_index tracks how many files in ordered_paths have been completed.
    let scaffolder = Scaffolder::new(
        scaffold.workspace_path.clone(),
        scaffold.crate_name.clone(),
    );
    let chunker = SemanticChunker::new(config.max_chunk_tokens);
    let total   = ordered_paths.len();

    // Skip already-completed files.
    let already_done: std::collections::HashSet<PathBuf> =
        checkpoint.completed.iter().map(|f| f.source_path.clone()).collect();

    let pending_paths: Vec<&PathBuf> = ordered_paths
        .iter()
        .filter(|p| analysis.target_paths.contains(p) && !already_done.contains(*p))
        .collect();

    for (pos, source_path) in pending_paths.into_iter().enumerate() {
        let display_idx = checkpoint.next_index + pos;
        emit(
            progress_cb,
            Progress::FileStarted {
                file:  source_path.to_string_lossy().to_string(),
                index: display_idx,
                total,
            },
        );

        let source_code = match fs::read_to_string(source_path) {
            Ok(s)  => s,
            Err(e) => {
                warn!("Cannot read {}: {e}", source_path.display());
                placeholder_for(source_path, &analysis.language, 0, e.to_string())
            }
        };

        // Get dependency context for this file from the ownership graph.
        let dep_paths: Vec<&PathBuf> = graph.deps_of(source_path);
        let rust_context = checkpoint.ownership.translation_context_for(source_path, &dep_paths);

        // Chunk the source file.
        let chunks = chunker.chunk(source_path, &source_code, &analysis.language);
        let n_chunks = chunks.len();
        debug!(
            "{} → {} chunk(s), context_tokens={}",
            source_path.display(),
            n_chunks,
            rust_context.len() / 4,
        );

        // Translate each chunk.
        let mut combined_rust = String::new();
        let mut file_succeeded = true;
        let mut total_attempts = 0u32;

        for chunk in &chunks {
            emit(
                progress_cb,
                Progress::ChunkStarted {
                    file:    source_path.to_string_lossy().to_string(),
                    chunk:   chunk.chunk_index,
                    total:   chunk.total_chunks,
                    symbols: chunk.symbol_names.clone(),
                },
            );

            let file_name = source_path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "file".into());

            let mut chunk_rust = String::new();
            let mut chunk_ok = false;

            for attempt in 0..=config.translate_retries {
                total_attempts += 1;
                let prompt = prompt_translate_with_context(
                    &chunk.content,
                    &analysis.language,
                    &file_name,
                    chunk.chunk_index,
                    chunk.total_chunks,
                    &rust_context,
                    &chunk.symbol_names,
                );
                match llm.complete(SYSTEM_TRANSLATE, &prompt) {
                    Ok(raw) => {
                        chunk_rust = extract_rust_code(&raw);
                        chunk_ok   = true;
                        break;
                    }
                    Err(e) => {
                        warn!("LLM error chunk {}/{} attempt {attempt}: {e}", chunk.chunk_index + 1, chunk.total_chunks);
                        orch.transition(StateEvent::ChunkRetry { reason: e.to_string() }).ok();
                    }
                }
            }

            if !chunk_ok {
                file_succeeded = false;
                chunk_rust = format!(
                    "// TODO: translation failed for chunk {}/{} of `{file_name}`\n",
                    chunk.chunk_index + 1,
                    chunk.total_chunks,
                );
            }

            combined_rust.push_str(&chunk_rust);
            combined_rust.push('\n');
            checkpoint.total_chunks_processed += 1;
        }

        // Write the assembled Rust file.
        let extra_deps = extract_dep_hints(&combined_rust);
        let rel = source_path.strip_prefix(&analysis.source_dir).unwrap_or(source_path);
        let dest = scaffolder.write_module(rel, &combined_rust, &extra_deps)?;

        // Record Rust signatures for downstream files.
        checkpoint.ownership.record_rust_signatures(source_path, &combined_rust);
        let sig_count = checkpoint.ownership.signatures_for(source_path).len();

        let mod_name = dest
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "mod_unknown".into());

        if mod_name != "main" {
            checkpoint.module_names.push(mod_name.clone());
        }

        checkpoint.completed.push(FileTranslation {
            source_path: source_path.clone(),
            rust_path:   dest,
            module_name: mod_name.clone(),
            attempt_count: total_attempts,
            succeeded: file_succeeded,
        });

        orch.transition(StateEvent::ChunkAccepted {
            next_chunk_index: (display_idx + 1) as u32,
        })
        .ok();

        checkpoint.next_index = display_idx + 1;
        store.write("translation", &checkpoint)?;

        emit(
            progress_cb,
            Progress::FileComplete {
                file:       source_path.to_string_lossy().to_string(),
                chunks:     n_chunks,
                signatures: sig_count,
            },
        );
    }

    // Wire up main.rs with all module declarations.
    scaffolder.write_main(&checkpoint.module_names)?;

    Ok(checkpoint)
}

// ---------------------------------------------------------------------------
// Phase 4: Verification — DiagnosticFamily-targeted fix loop
// ---------------------------------------------------------------------------

fn phase_verify<F>(
    config: &RunConfig,
    llm: &LlmClient,
    scaffold: &ScaffoldCheckpoint,
    _translation: &TranslationCheckpoint,
    progress_cb: &mut F,
) -> Result<VerificationCheckpoint, EngineError>
where
    F: FnMut(Progress),
{
    let ws = &scaffold.workspace_path;
    let initial_output = cargo_check(ws);
    let mut exit_clean = initial_output.exit_code == Some(0);
    let mut fix_cycles: Vec<FixCycleSummary> = vec![];

    if !exit_clean {
        for attempt in 1..=config.verify_retries {
            emit(progress_cb, Progress::FixCycle { attempt });

            let diags = parse_cargo_diagnostics(&initial_output).unwrap_or_default();
            let families = classify_and_rank(&diags);
            let family_names: Vec<String> =
                families.iter().map(|(n, _)| n.to_string()).collect();

            let errors_text    = initial_output.stderr_lines.join("\n");
            let errors_summary = &errors_text[..errors_text.len().min(8_000)];

            emit(progress_cb, Progress::CompilerError {
                message:  errors_summary.to_string(),
                families: family_names.clone(),
            });

            let top_families: Vec<(&str, &str)> =
                families.iter().take(3).map(|(n, h)| (*n, *h)).collect();

            let errored_files = files_with_errors(&diags, ws);
            let files_to_fix: Vec<PathBuf> = if errored_files.is_empty() {
                vec![ws.join("src").join("main.rs")]
            } else {
                errored_files
            };

            for path in &files_to_fix {
                if let Ok(code) = fs::read_to_string(path) {
                    let prompt = prompt_fix_targeted(&code, errors_summary, &top_families);
                    if let Ok(raw) = llm.complete(SYSTEM_FIX, &prompt) {
                        let fixed = extract_rust_code(&raw);
                        let _ = fs::write(path, fixed);
                    }
                }
            }

            let new_output = cargo_check(ws);
            exit_clean = new_output.exit_code == Some(0);

            fix_cycles.push(FixCycleSummary {
                attempt,
                error_count: diags.iter()
                    .filter(|d| d.level >= rustyfi_core::state::DiagnosticLevel::Error)
                    .count(),
                dominant_families: family_names,
                resolved: exit_clean,
            });

            if exit_clean {
                info!("cargo check clean after fix cycle {attempt}");
                break;
            }
        }
    }

    let final_diags = parse_cargo_diagnostics(&cargo_check(ws)).unwrap_or_default();
    let final_error_count = final_diags.iter()
        .filter(|d| d.level >= rustyfi_core::state::DiagnosticLevel::Error)
        .count();

    Ok(VerificationCheckpoint { exit_clean, fix_cycles, final_error_count })
}

// ---------------------------------------------------------------------------
// Phase 5: Packaging
// ---------------------------------------------------------------------------

fn phase_package(
    scaffold: &ScaffoldCheckpoint,
    run_dir: &Path,
) -> Result<PackagingCheckpoint, EngineError> {
    let zip_bytes = package_to_zip(&scaffold.workspace_path)?;
    let zip_len   = zip_bytes.len();
    let zip_name  = format!("{}.zip", scaffold.crate_name);
    let zip_path  = run_dir.join(&zip_name);
    fs::write(&zip_path, &zip_bytes).map_err(|e| EngineError::Io(e.to_string()))?;
    info!("Packaged: {} ({zip_len} bytes)", zip_path.display());
    Ok(PackagingCheckpoint {
        zip_path,
        zip_bytes: zip_len,
        crate_name: scaffold.crate_name.clone(),
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn emit<F: FnMut(Progress)>(cb: &mut F, p: Progress) { cb(p); }

fn cargo_check(workspace: &Path) -> CargoOutput {
    rustyfi_core::compiler::run_cargo_check(workspace).unwrap_or_else(|_| CargoOutput {
        stdout_lines: vec![],
        stderr_lines: vec!["cargo not available".into()],
        exit_code: Some(1),
    })
}

/// Classify diagnostics into deduplicated, priority-ranked family/hint pairs.
fn classify_and_rank<'a>(
    diags: &[rustyfi_core::state::CompilerDiagnostic],
) -> Vec<(&'a str, &'a str)> {
    use std::collections::BTreeMap;

    let mut counts: BTreeMap<u8, (DiagnosticFamily, usize)> = BTreeMap::new();
    for d in diags {
        let f    = d.family();
        let prio = f.retry_priority();
        counts.entry(prio)
            .and_modify(|(_, c)| *c += 1)
            .or_insert((f, 1));
    }

    let mut ranked: Vec<_> = counts.into_values().collect();
    ranked.sort_by_key(|(f, _): &(DiagnosticFamily, usize)| std::cmp::Reverse(f.retry_priority()));

    ranked.into_iter()
        .map(|(f, _)| (family_name_static(&f), f.repair_hint()))
        .collect()
}

fn family_name_static(f: &DiagnosticFamily) -> &'static str {
    match f {
        DiagnosticFamily::MissingLifetime       => "MissingLifetime",
        DiagnosticFamily::TraitBoundFailure      => "TraitBoundFailure",
        DiagnosticFamily::OwnershipMove          => "OwnershipMove",
        DiagnosticFamily::BorrowConflict         => "BorrowConflict",
        DiagnosticFamily::TypeMismatch           => "TypeMismatch",
        DiagnosticFamily::MissingImport          => "MissingImport",
        DiagnosticFamily::AsyncMismatch          => "AsyncMismatch",
        DiagnosticFamily::MacroError             => "MacroError",
        DiagnosticFamily::PatternExhaustiveness  => "PatternExhaustiveness",
        DiagnosticFamily::IntegerOverflow        => "IntegerOverflow",
        DiagnosticFamily::UnusedCode             => "UnusedCode",
        DiagnosticFamily::InternalCompilerError  => "InternalCompilerError",
        DiagnosticFamily::Other(_)               => "Other",
    }
}

fn files_with_errors(
    diags: &[rustyfi_core::state::CompilerDiagnostic],
    workspace: &Path,
) -> Vec<PathBuf> {
    use std::collections::HashSet;
    let mut seen = HashSet::new();
    diags.iter()
        .filter(|d| d.level >= rustyfi_core::state::DiagnosticLevel::Error)
        .flat_map(|d| d.spans.iter())
        .filter(|s| s.is_primary)
        .map(|s| workspace.join(&s.file_name))
        .filter(|p| p.exists() && seen.insert(p.clone()))
        .collect()
}

fn placeholder_for(path: &Path, lang: &str, attempts: u32, reason: String) -> String {
    format!(
        "// TODO: could not read `{}` after {attempts} attempts\n\
         // Original language: {lang}\n// Error: {reason}\n",
        path.display()
    )
}

fn sanitise_crate_name(name: String) -> String {
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

fn module_label(path: &Path, root: &Path) -> String {
    path.strip_prefix(root).unwrap_or(path).to_string_lossy().to_string()
}

fn lang_display(meta: &LanguageMetadata) -> String {
    use rustyfi_core::context::SourceLanguage;
    match &meta.primary_language {
        SourceLanguage::Python     => "python".into(),
        SourceLanguage::TypeScript => "typescript".into(),
        SourceLanguage::JavaScript => "javascript".into(),
        SourceLanguage::Go         => "go".into(),
        SourceLanguage::Cpp        => "cpp".into(),
        SourceLanguage::C          => "c".into(),
        SourceLanguage::Java       => "java".into(),
        SourceLanguage::CSharp     => "csharp".into(),
        SourceLanguage::Ruby       => "ruby".into(),
        SourceLanguage::Other(s)   => s.clone(),
    }
}

/// Extract `// [DEPS] crate = "version"` comments emitted by the LLM.
fn extract_dep_hints(code: &str) -> HashMap<String, String> {
    let mut deps = HashMap::new();
    for line in code.lines() {
        if let Some(rest) = line.trim().strip_prefix("// [DEPS]") {
            for part in rest.split(',') {
                let kv: Vec<&str> = part.splitn(2, '=').collect();
                if kv.len() == 2 {
                    let k = kv[0].trim().to_string();
                    let v = kv[1].trim().trim_matches('"').to_string();
                    if !k.is_empty() { deps.insert(k, v); }
                }
            }
        }
    }
    deps
}

fn unix_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("unix:{secs}")
}
