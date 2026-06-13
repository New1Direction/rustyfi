use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use tempfile::TempDir;

use rustyfi_core::compiler::parse_cargo_diagnostics;
use rustyfi_core::context::LanguageMetadata;
use rustyfi_core::state::{CargoOutput, DiagnosticFamily, LtoMode, ReleaseConfig};
use rustyfi_core::{ContextManifest, Orchestrator, StateEvent};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::analysis::SourceAnalyser;
use crate::checkpoint::{
    AnalysisCheckpoint, CheckpointStore, ContractCheckpoint, FileTranslation, FixCycleSummary,
    PackageContract, PackagingCheckpoint, ScaffoldCheckpoint, TranslationCheckpoint,
    VerificationCheckpoint,
};
use crate::chunker::SemanticChunker;
use crate::fix_context;
use crate::graph::{EdgeRecord, ModuleGraph};
use crate::llm::{
    extract_rust_code, prompt_extract_contract, prompt_extract_contract_retry, prompt_fix_targeted,
    prompt_translate_with_context, LlmClient, SYSTEM_CONTRACT, SYSTEM_FIX, SYSTEM_TRANSLATE,
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
    /// Crate name for the generated project. When `None`, the name is derived
    /// from `source_dir`'s directory name — which for server uploads can be a
    /// UUID, so the server should always pass the real project name.
    pub crate_name: Option<String>,
    /// Maximum LLM translation retries per source file.
    pub translate_retries: u32,
    /// Maximum `cargo check` fix cycles.
    pub verify_retries: u32,
    /// Token budget per semantic chunk (default: 5 000).
    pub max_chunk_tokens: usize,
    /// Number of files to translate simultaneously (default: 16).
    pub parallel: usize,
    /// Token threshold below which the fast/cheap model tier is used (default: 400).
    pub tier_fast_tokens: usize,
    /// Token threshold below which the mid model tier is used (default: 3000).
    pub tier_mid_tokens: usize,
    /// When true, run the behavioral-equivalence phase (mine + capture golden
    /// from the source + verify the target). Requires the SOURCE toolchain and
    /// executes the source project, so callers enable it only where that is
    /// trusted (CLI/local). The server leaves it false (no executing uploads).
    pub verify_behavior: bool,
}

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            source_dir: PathBuf::new(),
            output_dir: PathBuf::new(),
            crate_name: None,
            translate_retries: 3,
            verify_retries: 5,
            max_chunk_tokens: 5_000,
            parallel: 16,
            tier_fast_tokens: 400,
            tier_mid_tokens: 3_000,
            verify_behavior: false,
        }
    }
}

/// Progress events emitted during a run, streamed to the browser via SSE.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Progress {
    StateChanged {
        state: &'static str,
    },
    PhaseResumed {
        phase: String,
    },
    FileStarted {
        file: String,
        index: usize,
        total: usize,
    },
    ChunkStarted {
        file: String,
        chunk: usize,
        total: usize,
        symbols: Vec<String>,
    },
    FileComplete {
        file: String,
        chunks: usize,
        signatures: usize,
    },
    CompilerError {
        message: String,
        families: Vec<String>,
    },
    FixCycle {
        attempt: u32,
    },
    /// Informational line for the UI terminal (warnings, skips, hints).
    Note {
        message: String,
    },
    /// Emitted by the *server* once the result ZIP is on disk and downloadable
    /// — the pipeline itself does not emit Done, so the UI can never see
    /// "done" before the download is actually ready.
    Done {
        zip_bytes: usize,
        crate_name: String,
        files_failed: usize,
        cargo_clean: bool,
        /// Remaining `cargo check` errors (0 when clean).
        error_count: usize,
        /// `todo!()` placeholders the model left behind.
        todo_count: usize,
        /// Source files translated successfully (excludes stubs/failures).
        files_translated: usize,
    },
    Failed {
        reason: String,
    },
}

/// Summary of the agentic deep-fix pass (populated when `--deep` / `RUSTYFI_DEEP_FIX=1`).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct DeepFixSummary {
    /// Always `true` when this struct is present; included for self-documenting JSON.
    pub ran: bool,
    /// Error count before the doctor session started.
    pub start_errors: usize,
    /// Error count after the doctor session (post-revert if reverted).
    pub end_errors: usize,
    /// Total tool calls consumed by the doctor session (including the seeding check).
    pub tool_calls: usize,
}

/// Summary of the behavioral-equivalence phase (populated when `verify_behavior`).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
// populated in Task 3 / read by the CLI in Task 5
#[allow(dead_code)]
pub struct BehaviorSummary {
    /// The phase ran (mined + captured). False when gated off / skipped.
    pub ran: bool,
    /// The target was built and run against golden (false if it did not compile).
    pub verified: bool,
    pub matched: usize,
    pub total: usize,
    pub quarantined: usize,
}

/// Output of a completed pipeline run.
pub struct RunResult {
    pub zip: Vec<u8>,
    pub crate_name: String,
    /// Detected primary source language (e.g. "python").
    pub language: String,
    /// Files whose translation fell back to a TODO placeholder.
    pub files_failed: usize,
    /// Whether the generated crate passed `cargo check`.
    pub cargo_clean: bool,
    /// Remaining `cargo check` errors after the fix loop.
    pub error_count: usize,
    /// `todo!()` placeholders left in the generated crate.
    pub todo_count: usize,
    /// Source files translated successfully.
    pub files_translated: usize,
    /// Present when the agentic deep-fix pass ran (`--deep` / `RUSTYFI_DEEP_FIX=1`).
    pub deep_fix: Option<DeepFixSummary>,
    /// Present when the behavioral phase ran (`verify_behavior`).
    pub behavior: Option<BehaviorSummary>,
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
    F: FnMut(Progress) + Send,
{
    let run_dir = config.output_dir.clone();
    fs::create_dir_all(&run_dir).map_err(|e| EngineError::Io(e.to_string()))?;

    let store = CheckpointStore::new(&run_dir)?;
    let llm = LlmClient::from_env()?;
    // Separate (typically stronger) model for the verification fix loop.
    // Falls back to the translation client if RUSTYFI_FIX_* isn't configured
    // or its config is invalid.
    let fix_llm = LlmClient::for_fixing().ok();
    let mut orch = Orchestrator::new();

    // ── Phase 1: Analysis ─────────────────────────────────────────────────
    let analysis_cp = if let Some(cp) = store.read::<AnalysisCheckpoint>("analysis") {
        emit(
            &mut progress_cb,
            Progress::PhaseResumed {
                phase: "analysis".into(),
            },
        );
        cp
    } else {
        emit(
            &mut progress_cb,
            Progress::StateChanged { state: "Parsing" },
        );
        let cp = phase_analyse(&config, &store, &mut orch)?;
        store.write("analysis", &cp)?;
        cp
    };

    // ── Phase 2: Scaffold ─────────────────────────────────────────────────
    let scaffold_cp = if let Some(cp) = store.read::<ScaffoldCheckpoint>("scaffold") {
        emit(
            &mut progress_cb,
            Progress::PhaseResumed {
                phase: "scaffold".into(),
            },
        );
        cp
    } else {
        emit(
            &mut progress_cb,
            Progress::StateChanged {
                state: "Scaffolding",
            },
        );
        let cp = phase_scaffold(&config, &analysis_cp, &mut orch)?;
        store.write("scaffold", &cp)?;
        cp
    };

    // Directory-as-package module map — drives where each file's translation
    // lands and how cross-package references are rewritten. Pure function of
    // the analysis, so it's identical on resume.
    let package_map = build_package_map_from_analysis(&analysis_cp);

    // ── Phase 2.5: Contract — canonical per-package Rust API ──────────────
    // Pin each package's type/signature surface BEFORE translating bodies, so
    // every file (and importer) agrees on field sets and return types — fixing
    // the cross-file E0609/E0308/E0061/E0599 divergence at its source.
    let contract_cp = if let Some(cp) = store.read::<ContractCheckpoint>("contract") {
        emit(
            &mut progress_cb,
            Progress::PhaseResumed {
                phase: "contract".into(),
            },
        );
        cp
    } else {
        let cp = phase_contract(
            &config,
            &llm,
            &analysis_cp,
            &scaffold_cp,
            &package_map,
            &mut progress_cb,
        )?;
        store.write("contract", &cp)?;
        cp
    };

    // ── Phase 3: Translation (graph-scheduled, semantically-chunked) ──────
    let translation_cp = {
        let existing: Option<TranslationCheckpoint> = store.read("translation");
        let resume_from = existing.as_ref().map(|c| c.next_index).unwrap_or(0);

        if resume_from == 0 {
            emit(
                &mut progress_cb,
                Progress::StateChanged {
                    state: "Translating",
                },
            );
        } else {
            info!("Resuming translation from index {resume_from}");
            emit(
                &mut progress_cb,
                Progress::PhaseResumed {
                    phase: format!("translation (file {resume_from})"),
                },
            );
        }

        phase_translate(
            &config,
            &store,
            &llm,
            &analysis_cp,
            &scaffold_cp,
            &package_map,
            &contract_cp,
            existing,
            &mut progress_cb,
            &mut orch,
        )?
    };

    // ── Phase 4: Verification + targeted fix loop ─────────────────────────
    let (verification_cp, deep_fix) =
        if let Some(cp) = store.read::<VerificationCheckpoint>("verification") {
            emit(
                &mut progress_cb,
                Progress::PhaseResumed {
                    phase: "verification".into(),
                },
            );
            // On a resumed run the doctor pass was already complete (or skipped);
            // we do not re-run it.  deep_fix is only set on a fresh verification.
            (cp, None)
        } else {
            emit(
                &mut progress_cb,
                Progress::StateChanged { state: "Verifying" },
            );
            let (cp, df) = phase_verify(
                &config,
                fix_llm.as_ref().unwrap_or(&llm),
                &scaffold_cp,
                &translation_cp,
                &package_map,
                &mut progress_cb,
            )?;
            store.write("verification", &cp)?;
            (cp, df)
        };

    // ── Completion report ─────────────────────────────────────────────────
    // Tell the user exactly what's left to make the crate compile — system
    // libraries to install, inferred deps to verify, stubs and todo!() gaps.
    // Written into the workspace root BEFORE packaging so it ships in the ZIP.
    let report = build_next_steps(
        &analysis_cp,
        &scaffold_cp,
        &translation_cp,
        &verification_cp,
        deep_fix.as_ref(),
    );
    let _ = fs::write(
        scaffold_cp.workspace_path.join("NEXT_STEPS.md"),
        &report.markdown,
    );
    // Packaging may be checkpoint-skipped on a fully-resumed run; invalidate it
    // so the freshly-written NEXT_STEPS.md is actually included in the ZIP.
    store.invalidate("packaging");
    for line in report.summary_lines {
        emit(&mut progress_cb, Progress::Note { message: line });
    }

    // ── Phase 5: Packaging ────────────────────────────────────────────────
    let packaging_cp = if let Some(cp) = store.read::<PackagingCheckpoint>("packaging") {
        emit(
            &mut progress_cb,
            Progress::PhaseResumed {
                phase: "packaging".into(),
            },
        );
        cp
    } else {
        emit(
            &mut progress_cb,
            Progress::StateChanged {
                state: "Optimizing",
            },
        );
        let cp = phase_package(&scaffold_cp, &run_dir)?;
        store.write("packaging", &cp)?;
        cp
    };
    emit(
        &mut progress_cb,
        Progress::StateChanged { state: "Completed" },
    );

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
            ("zip_bytes".into(), packaging_cp.zip_bytes.to_string()),
            (
                "fix_cycles".into(),
                verification_cp.fix_cycles.len().to_string(),
            ),
            (
                "chunk_total".into(),
                translation_cp.total_chunks_processed.to_string(),
            ),
        ]),
    })
    .ok();

    // If the packaged ZIP vanished (e.g. OS temp cleanup since the checkpoint
    // was written), re-package from the workspace instead of failing.
    let (zip, packaging_cp) = match fs::read(&packaging_cp.zip_path) {
        Ok(z) => (z, packaging_cp),
        Err(_) => {
            warn!(
                "Packaged ZIP missing at {} — re-packaging",
                packaging_cp.zip_path.display()
            );
            store.invalidate("packaging");
            let cp = phase_package(&scaffold_cp, &run_dir)?;
            store.write("packaging", &cp)?;
            let z =
                fs::read(&cp.zip_path).map_err(|e| EngineError::Io(format!("read ZIP: {e}")))?;
            (z, cp)
        }
    };

    let files_failed = translation_cp
        .completed
        .iter()
        .filter(|f| !f.succeeded)
        .count();

    info!(
        "Run complete. crate={} zip={} bytes chunks={} fix_cycles={} failed_files={}",
        packaging_cp.crate_name,
        packaging_cp.zip_bytes,
        translation_cp.total_chunks_processed,
        verification_cp.fix_cycles.len(),
        files_failed,
    );

    Ok(RunResult {
        zip,
        crate_name: packaging_cp.crate_name,
        language: analysis_cp.language.clone(),
        files_failed,
        cargo_clean: verification_cp.exit_clean,
        error_count: verification_cp.final_error_count,
        todo_count: report.todo_count,
        files_translated: report.translated,
        deep_fix,
        behavior: None, // populated in Task 3
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

    let crate_name = sanitise_crate_name(config.crate_name.clone().unwrap_or_else(|| {
        config
            .source_dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "app".to_string())
    }));

    let lang = lang_display(&analysis.language_metadata);

    // Convert edges to serialisable EdgeRecords.
    let edges: Vec<EdgeRecord> = analysis
        .dependency_edges
        .iter()
        .map(EdgeRecord::from)
        .collect();

    let manifest = ContextManifest {
        run_id: Uuid::new_v4().to_string(),
        workspace_root: config.source_dir.clone(),
        source_targets: analysis.targets.clone(),
        dependency_edges: analysis.dependency_edges.clone(),
        external_packages: vec![],
        filesystem_boundaries: vec![],
        external_io_boundaries: vec![],
        inferred_entrypoints: analysis.inferred_entrypoints.clone(),
        parser_warnings: analysis.warnings.clone(),
        language_metadata: analysis.language_metadata.clone(),
        produced_at: unix_now(),
    };

    orch.transition(StateEvent::StartParsing {
        manifest: Box::new(manifest),
    })
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
        source_dir: config.source_dir.clone(),
        crate_name,
        language: lang,
        target_paths: analysis.targets.iter().map(|t| t.path.clone()).collect(),
        inferred_entrypoints: analysis.inferred_entrypoints,
        edges,
        warning_count: analysis.warnings.len(),
        produced_at: unix_now(),
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

    let first_file = analysis
        .target_paths
        .first()
        .cloned()
        .unwrap_or_else(|| analysis.source_dir.join("main.py"));

    // Best-effort: on a resumed run the orchestrator starts back at Idle (the
    // analysis phase that would have sent StartParsing was skipped), so these
    // transitions can legitimately be rejected. A hard error here would wedge
    // resumption permanently ("ParseComplete is not valid from state Idle").
    if let Err(e) = orch.transition(StateEvent::ParseComplete {
        workspace_path: workspace_path.clone(),
        dependency_manifest: HashMap::new(),
        module_layout_plan: module_plan.clone(),
    }) {
        warn!("orchestrator: {e} (continuing — resumed run)");
    }

    if let Err(e) = orch.transition(StateEvent::ScaffoldComplete {
        first_file,
        total_chunks: analysis.target_paths.len() as u32,
        retry_ceiling: config.translate_retries,
    }) {
        warn!("orchestrator: {e} (continuing — resumed run)");
    }

    Ok(ScaffoldCheckpoint {
        workspace_path,
        crate_name: analysis.crate_name.clone(),
        module_plan,
    })
}

// ---------------------------------------------------------------------------
// Phase 2.5: Contract — canonical per-package Rust API surface
// ---------------------------------------------------------------------------

/// Max source bytes fed to one contract-extraction call.
const PKG_SRC_BUDGET: usize = 24_000;
/// Max bytes of contract context injected into a body-translation prompt.
const CONTRACT_CTX_BUDGET: usize = 8_000;

fn phase_contract<F>(
    config: &RunConfig,
    llm: &LlmClient,
    analysis: &AnalysisCheckpoint,
    scaffold: &ScaffoldCheckpoint,
    package_map: &crate::scaffold::PackageMap,
    progress_cb: &mut F,
) -> Result<ContractCheckpoint, EngineError>
where
    F: FnMut(Progress),
{
    emit(
        progress_cb,
        Progress::StateChanged {
            state: "Scaffolding",
        },
    );
    emit(
        progress_cb,
        Progress::Note {
            message: "Building a canonical type contract so every file agrees on shapes \
                  and signatures (cross-file consistency)…"
                .into(),
        },
    );

    let scaffolder = Scaffolder::new(scaffold.workspace_path.clone(), scaffold.crate_name.clone());
    let lang = analysis.language.clone();

    // Group non-entrypoint, non-stub source files by package root segment.
    let mut by_pkg: std::collections::BTreeMap<String, (String, Vec<PathBuf>)> =
        std::collections::BTreeMap::new();
    for abs in &analysis.target_paths {
        let rel = abs
            .strip_prefix(&analysis.source_dir)
            .unwrap_or(abs)
            .to_path_buf();
        let Some(cm) = package_map.get(&rel) else {
            continue;
        };
        if cm.is_entrypoint {
            continue;
        }
        if let Ok(src) = fs::read_to_string(abs) {
            if classify_stub(abs, &src).is_some() {
                continue;
            }
        }
        by_pkg
            .entry(cm.root_segment.clone())
            .or_insert_with(|| (cm.package.clone(), Vec::new()))
            .1
            .push(abs.clone());
    }

    // Phase 1: generate all contracts in memory (no writes yet), retaining the
    // labeled source per package for use in retry prompts.
    let mut contracts: Vec<PackageContract> = Vec::new();
    // Keyed by root_segment: the labeled source used to generate this contract.
    let mut labeled_by_root: HashMap<String, String> = HashMap::new();

    for (root, (pkg, files)) in by_pkg {
        // Concatenate the package's source (path-labelled) up to the budget.
        let mut labeled = String::new();
        for f in &files {
            if labeled.len() >= PKG_SRC_BUDGET {
                break;
            }
            if let Ok(s) = fs::read_to_string(f) {
                let rel = f.strip_prefix(&analysis.source_dir).unwrap_or(f);
                labeled.push_str(&format!("// FILE: {}\n", rel.display()));
                labeled.push_str(truncate_utf8(
                    &s,
                    PKG_SRC_BUDGET.saturating_sub(labeled.len()),
                ));
                labeled.push('\n');
            }
        }
        if labeled.trim().is_empty() {
            continue;
        }

        let prompt = prompt_extract_contract(&pkg, &lang, &labeled);
        // Tier-route small packages to the fast model.
        let model = if std::env::var("RUSTYFI_NO_TIER").is_ok() {
            None
        } else {
            tier_for_tokens(
                estimate_tokens(&labeled),
                config.tier_fast_tokens,
                config.tier_mid_tokens,
            )
        };
        let raw = match model {
            Some(ref m) => llm.complete_with_model(SYSTEM_CONTRACT, &prompt, m),
            None => llm.complete(SYSTEM_CONTRACT, &prompt),
        };
        let contract_rust = match raw {
            Ok(r) => extract_rust_code(&r),
            Err(EngineError::Config(msg)) => return Err(EngineError::Config(msg)),
            Err(e) => {
                warn!("contract extraction failed for `{root}`: {e}");
                continue;
            }
        };

        let (data_surface, signatures) = crate::slicer::split_contract(&contract_rust);
        // The data surface is authoritative: rewrite cross-package refs, dedup.
        // Do NOT write yet — validation happens first.
        let repaired = crate::scaffold::repair_module_refs(&data_surface, &pkg, package_map);
        let data = crate::dedup_items::dedup_top_level_items(&repaired);

        labeled_by_root.insert(root.clone(), labeled);
        contracts.push(PackageContract {
            root_segment: root,
            package: pkg,
            is_entrypoint: false,
            data_surface: data,
            signatures,
        });
    }

    // Phase 2: compiler-validate all contracts via a throwaway skeleton crate.
    // Rounds 1–3: initial check + up to 2 regenerate→recheck retries (3 cargo
    // runs total).  After each check we record (issue_count, snapshot); when the
    // loop ends without reaching zero issues we restore the snapshot with the
    // fewest issues (ties → earliest round).
    emit(
        progress_cb,
        Progress::Note {
            message: "Compiler-checking the type contract before translation…".into(),
        },
    );

    let mut best_contracts = contracts.clone();
    // (issue_count, failing_root_names, contracts_snapshot) for every round that produced issues.
    let mut round_snapshots: Vec<(usize, Vec<String>, Vec<PackageContract>)> = Vec::new();

    'validation: for round in 1..=3usize {
        let issues =
            match crate::contract_check::check_contracts(&best_contracts, &scaffold.crate_name) {
                Ok(v) => v,
                Err(e) => {
                    warn!("contract validation failed (round {round}): {e}");
                    break 'validation;
                }
            };

        if issues.is_empty() {
            emit(
                progress_cb,
                Progress::Note {
                    message: "Contract validated clean ✓".into(),
                },
            );
            break 'validation;
        }

        // Record this round's snapshot so we can restore the best one later.
        let round_failing: Vec<String> = issues.iter().map(|i| i.root_segment.clone()).collect();
        round_snapshots.push((issues.len(), round_failing, best_contracts.clone()));

        if round == 3 {
            // All retries exhausted — restore the round with the fewest issues
            // (ties → earliest round, i.e. the minimum by stable sort).
            if let Some((_, best_failing, best_snapshot)) =
                round_snapshots.iter().min_by_key(|(count, _, _)| *count)
            {
                best_contracts = best_snapshot.clone();
                // Report failing packages from the best round (not the last round).
                let failing: Vec<&str> = best_failing.iter().map(String::as_str).collect();
                emit(
                    progress_cb,
                    Progress::Note {
                        message: format!(
                            "⚠ contract for {} couldn't be fully validated — proceeding",
                            failing.join(", ")
                        ),
                    },
                );
            }
            break 'validation;
        }

        // Regenerate failing packages with error feedback.
        let mut new_contracts = best_contracts.clone();
        for issue in &issues {
            let Some(contract) = best_contracts
                .iter()
                .find(|c| c.root_segment == issue.root_segment)
            else {
                continue;
            };
            let labeled = labeled_by_root
                .get(&issue.root_segment)
                .map(String::as_str)
                .unwrap_or("");
            let prev_contract = format!("{}\n{}", contract.data_surface, contract.signatures);

            // Compute the item inventory of the CURRENT (old) contract so we can
            // (a) hand it to the model as a "must not drop" list, and
            // (b) reject the regeneration if it drops >10% of the API surface.
            let old_names = crate::contract_check::item_names(&prev_contract);
            // BTreeSet iteration is already sorted — deterministic prompt text.
            let original_items_list = old_names
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join("\n");

            emit(
                progress_cb,
                Progress::Note {
                    message: format!(
                        "Contract for '{}' failed validation — regenerating (round {round})…",
                        contract.package
                    ),
                },
            );

            let retry_prompt = prompt_extract_contract_retry(
                &contract.package,
                &lang,
                labeled,
                &prev_contract,
                &issue.errors,
                &original_items_list,
            );
            let model = if std::env::var("RUSTYFI_NO_TIER").is_ok() {
                None
            } else {
                tier_for_tokens(
                    estimate_tokens(labeled),
                    config.tier_fast_tokens,
                    config.tier_mid_tokens,
                )
            };
            let raw = match model {
                Some(ref m) => llm.complete_with_model(SYSTEM_CONTRACT, &retry_prompt, m),
                None => llm.complete(SYSTEM_CONTRACT, &retry_prompt),
            };
            let new_rust = match raw {
                Ok(r) => extract_rust_code(&r),
                Err(EngineError::Config(msg)) => return Err(EngineError::Config(msg)),
                Err(e) => {
                    warn!("contract retry failed for `{}`: {e}", contract.root_segment);
                    continue;
                }
            };

            let (data_surface, signatures) = crate::slicer::split_contract(&new_rust);
            let repaired =
                crate::scaffold::repair_module_refs(&data_surface, &contract.package, package_map);
            let data = crate::dedup_items::dedup_top_level_items(&repaired);

            // Acceptance check: reject the regeneration if it dropped >10% of items.
            let new_combined = format!("{data}\n{signatures}");
            let new_names = crate::contract_check::item_names(&new_combined);
            if !crate::contract_check::regeneration_acceptable(&old_names, &new_names) {
                let dropped = old_names.difference(&new_names).count();
                emit(
                    progress_cb,
                    Progress::Note {
                        message: format!(
                            "Regenerated contract for '{}' dropped {dropped} item(s) — keeping the original.",
                            contract.package
                        ),
                    },
                );
                // Do NOT update new_contracts — keep the old contract for this package.
                continue;
            }

            if let Some(entry) = new_contracts
                .iter_mut()
                .find(|c| c.root_segment == issue.root_segment)
            {
                entry.data_surface = data;
                entry.signatures = signatures;
            }
        }
        best_contracts = new_contracts;
    }

    // Phase 3: write all validated contract surfaces and checkpoint.
    for contract in &best_contracts {
        let _ = scaffolder.write_package_contract(&contract.root_segment, &contract.data_surface);
    }

    emit(
        progress_cb,
        Progress::Note {
            message: format!(
                "Type contract ready for {} package(s).",
                best_contracts.len()
            ),
        },
    );
    Ok(ContractCheckpoint {
        contracts: best_contracts,
    })
}

/// Build the contract context injected into one file's translation prompt:
/// its own package's surface first, then the surfaces of packages it imports
/// (via DAG edges or a permissive source-name scan), within the budget.
fn build_contract_context(
    file: &Path,
    source_dir: &Path,
    graph: &ModuleGraph,
    package_map: &crate::scaffold::PackageMap,
    contract_map: &HashMap<&str, &PackageContract>,
) -> String {
    let rel = file.strip_prefix(source_dir).unwrap_or(file);
    let this_root = package_map.get(rel).map(|m| m.root_segment.clone());

    let mut roots: Vec<String> = Vec::new();
    if let Some(tr) = &this_root {
        roots.push(tr.clone());
    }
    // Imported packages via dependency edges (when present).
    for dep in graph.deps_of(file) {
        let drel = dep.strip_prefix(source_dir).unwrap_or(dep);
        if let Some(cm) = package_map.get(drel) {
            if !roots.contains(&cm.root_segment) {
                roots.push(cm.root_segment.clone());
            }
        }
    }
    // Source-name fallback (Go has no inferred edges): include any package whose
    // name appears in this file's source. Over-inclusion only costs a few tokens.
    if let Ok(src) = fs::read_to_string(file) {
        for (pkg_name, segs) in &package_map.root_of {
            if segs.len() == 1 && src.contains(pkg_name.as_str()) && !roots.contains(&segs[0]) {
                roots.push(segs[0].clone());
            }
        }
    }

    let mut ctx = String::new();
    for r in &roots {
        let Some(c) = contract_map.get(r.as_str()) else {
            continue;
        };
        if ctx.len() >= CONTRACT_CTX_BUDGET {
            ctx.push_str("// (more package contracts omitted for length)\n");
            break;
        }
        let path = format!("crate::{}", c.root_segment);
        ctx.push_str(&format!(
            "// package `{}` at {}:\n{}\n{}\n\n",
            c.package, path, c.data_surface, c.signatures
        ));
    }
    truncate_utf8(&ctx, CONTRACT_CTX_BUDGET).to_string()
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
    package_map: &crate::scaffold::PackageMap,
    contract: &ContractCheckpoint,
    existing: Option<TranslationCheckpoint>,
    progress_cb: &mut F,
    orch: &mut Orchestrator,
) -> Result<TranslationCheckpoint, EngineError>
where
    F: FnMut(Progress) + Send,
{
    // ── Build Module DAG from analysis edges ────────────────────────────────
    let graph = ModuleGraph::build(&analysis.target_paths, &analysis.edges);
    let ordered_paths = graph.translation_order();

    // Index the per-package contracts by root segment for context injection.
    let contract_map: HashMap<&str, &PackageContract> = contract
        .contracts
        .iter()
        .map(|c| (c.root_segment.as_str(), c))
        .collect();

    debug!(
        "ModuleGraph: {} nodes, {} edges → scheduled {} files",
        graph.len(),
        analysis.edges.len(),
        ordered_paths.len(),
    );

    // ── Restore or init checkpoint ──────────────────────────────────────────
    let mut checkpoint = existing.unwrap_or_else(|| TranslationCheckpoint {
        completed: vec![],
        next_index: 0,
        module_names: vec![],
        ownership: OwnershipGraph::new(),
        total_chunks_processed: 0,
    });

    let scaffolder = Scaffolder::new(scaffold.workspace_path.clone(), scaffold.crate_name.clone());
    let chunker = SemanticChunker::new(config.max_chunk_tokens);
    let total = analysis.target_paths.len();

    let already_done: std::collections::HashSet<PathBuf> = checkpoint
        .completed
        .iter()
        .map(|f| f.source_path.clone())
        .collect();

    let pending_paths: Vec<PathBuf> = ordered_paths
        .iter()
        .filter(|p| analysis.target_paths.contains(p) && !already_done.contains(*p))
        .cloned()
        .collect();

    let already_done_count = already_done.len();

    info!(
        "Translation: {total} total, {already_done_count} done, {} pending, {} parallel workers",
        pending_paths.len(),
        config.parallel,
    );

    // Build a rayon thread pool sized to our parallelism setting.
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(config.parallel)
        .build()
        .unwrap_or_else(|_| rayon::ThreadPoolBuilder::new().build().unwrap());

    // Batch size = parallel workers. Each completed batch writes a checkpoint
    // and emits UI progress. Using 1× (not 2×) means a single slow file can
    // only delay N-1 others instead of 2N-1.
    let batch_size = config.parallel.max(8);
    let batches: Vec<&[PathBuf]> = pending_paths.chunks(batch_size).collect();

    // Global rate-limiter: space requests evenly so we stay under the RPM cap.
    // Each thread acquires the gate, waits until min_gap_ms has elapsed since
    // the last request, then fires. Prevents the burst→429→sleep→burst loop.
    let rpm_limit = std::env::var("RUSTYFI_RPM")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(25); // 25 RPM default — leaves headroom under Cerebras 30 RPM
    let min_gap_ms = 60_000u64 / rpm_limit.max(1);
    let rate_gate: std::sync::Arc<std::sync::Mutex<std::time::Instant>> =
        std::sync::Arc::new(std::sync::Mutex::new(
            std::time::Instant::now() - std::time::Duration::from_millis(min_gap_ms),
        ));

    // Live progress: workers emit FileStarted/FileComplete the moment they
    // happen, not after the whole batch lands. Without this the UI shows a
    // frozen bar for the entire first batch (minutes at low RPM caps).
    let cb = std::sync::Mutex::new(progress_cb);
    let emit_live = |p: Progress| {
        if let Ok(mut f) = cb.lock() {
            (**f)(p);
        }
    };

    use std::sync::atomic::{AtomicUsize, Ordering};
    let started = AtomicUsize::new(already_done_count);
    // A Config error (bad API key, missing auth) fails every request the same
    // way — record it once and stop instead of burning the full retry budget
    // on every chunk of every file.
    let fatal: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
    let mut stub_count = 0usize;

    struct FileResult {
        source_path: PathBuf,
        rust_code: String,
        n_chunks: usize,
        succeeded: bool,
        total_attempts: u32,
        is_stub: bool,
    }

    for batch in batches {
        if fatal.lock().unwrap().is_some() {
            break;
        }

        // Canonical contract context for each file: its OWN package's type/sig
        // surface plus those of packages it imports, so every file agrees on
        // shapes and signatures (the cross-file consistency fix).
        let contexts: Vec<String> = batch
            .iter()
            .map(|p| {
                build_contract_context(p, &analysis.source_dir, &graph, package_map, &contract_map)
            })
            .collect();

        let rate_gate_batch = std::sync::Arc::clone(&rate_gate);
        let results: Vec<FileResult> = pool.install(|| {
            use rayon::prelude::*;
            batch
                .par_iter()
                .zip(contexts.par_iter())
                .map(|(source_path, rust_context)| {
                    let rate_gate = std::sync::Arc::clone(&rate_gate_batch);
                    let failed = |code: String, attempts: u32| FileResult {
                        source_path: source_path.clone(),
                        rust_code: code,
                        n_chunks: 0,
                        succeeded: false,
                        total_attempts: attempts,
                        is_stub: false,
                    };

                    if fatal.lock().unwrap().is_some() {
                        return failed(
                            "// TODO: run aborted before this file was translated\n".into(),
                            0,
                        );
                    }

                    let index = started.fetch_add(1, Ordering::Relaxed);
                    emit_live(Progress::FileStarted {
                        file: source_path.to_string_lossy().to_string(),
                        index,
                        total,
                    });

                    let source_code = match fs::read_to_string(source_path) {
                        Ok(s) => s,
                        Err(e) => {
                            warn!("Cannot read {}: {e}", source_path.display());
                            emit_live(Progress::FileComplete {
                                file: source_path.to_string_lossy().to_string(),
                                chunks: 0,
                                signatures: 0,
                            });
                            return failed(format!("// read error: {e}\n"), 0);
                        }
                    };

                    let file_name = source_path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| "file".into());

                    // ── STUB FILTER (IntentGate) ────────────────────────────────
                    // Skip the LLM entirely for test/mock/fixture/generated/config
                    // files. Emit a typed stub with a TODO and move on instantly.
                    if let Some(stub_reason) = classify_stub(source_path, &source_code) {
                        let stub = format!(
                            "// [STUB] Skipped by Rustyfi stub filter: {stub_reason}\n\
                         // Original: {}\n\
                         // Re-run with RUSTYFI_NO_STUB=1 to force full translation.\n\n\
                         // TODO: implement this module\n",
                            source_path.display()
                        );
                        debug!("[stub] {} → {stub_reason}", source_path.display());
                        emit_live(Progress::FileComplete {
                            file: source_path.to_string_lossy().to_string(),
                            chunks: 0,
                            signatures: 0,
                        });
                        return FileResult {
                            source_path: source_path.clone(),
                            rust_code: stub,
                            n_chunks: 0,
                            succeeded: true,
                            total_attempts: 0,
                            is_stub: true,
                        };
                    }

                    let chunks = chunker.chunk(source_path, &source_code, &analysis.language);
                    let n_chunks = chunks.len();
                    let mut combined_rust = String::new();
                    let mut file_succeeded = true;
                    let mut total_attempts = 0u32;

                    // ── TIERED MODEL ROUTING ────────────────────────────────────
                    let file_token_est = estimate_tokens(&source_code);
                    let model_override = if std::env::var("RUSTYFI_NO_TIER").is_ok() {
                        None
                    } else {
                        tier_for_tokens(
                            file_token_est,
                            config.tier_fast_tokens,
                            config.tier_mid_tokens,
                        )
                    };

                    if let Some(ref m) = model_override {
                        debug!(
                            "[tier] {} tokens={file_token_est} → {m}",
                            source_path.display()
                        );
                    }

                    'chunks: for chunk in &chunks {
                        let prompt = prompt_translate_with_context(
                            &chunk.content,
                            &analysis.language,
                            &file_name,
                            chunk.chunk_index,
                            chunk.total_chunks,
                            rust_context,
                            &chunk.symbol_names,
                        );

                        let mut chunk_rust = String::new();
                        let mut chunk_ok = false;

                        for attempt in 0..=config.translate_retries {
                            if fatal.lock().unwrap().is_some() {
                                file_succeeded = false;
                                break 'chunks;
                            }
                            total_attempts += 1;

                            // ── RATE GATE ───────────────────────────────────────
                            // Acquire the global token gate before each request so
                            // all threads together stay under RUSTYFI_RPM req/min.
                            {
                                let mut last = rate_gate.lock().unwrap();
                                let elapsed = last.elapsed().as_millis() as u64;
                                if elapsed < min_gap_ms {
                                    std::thread::sleep(std::time::Duration::from_millis(
                                        min_gap_ms - elapsed,
                                    ));
                                }
                                *last = std::time::Instant::now();
                            }

                            let result = match &model_override {
                                Some(m) => llm.complete_with_model(SYSTEM_TRANSLATE, &prompt, m),
                                None => llm.complete(SYSTEM_TRANSLATE, &prompt),
                            };
                            match result {
                                Ok(raw) => {
                                    chunk_rust = extract_rust_code(&raw);
                                    chunk_ok = true;
                                    break;
                                }
                                Err(EngineError::Config(msg)) => {
                                    // Unrecoverable (auth/config) — abort the run.
                                    *fatal.lock().unwrap() = Some(msg);
                                    file_succeeded = false;
                                    break 'chunks;
                                }
                                Err(e) => {
                                    warn!(
                                        "[parallel] chunk {}/{} attempt {attempt}: {e}",
                                        chunk.chunk_index + 1,
                                        chunk.total_chunks
                                    );
                                    // Extra back-off on rate-limit (gate should prevent
                                    // most 429s but add extra sleep if one slips through)
                                    if e.to_string().contains("429") {
                                        std::thread::sleep(std::time::Duration::from_secs(
                                            (attempt as u64 + 1) * 5,
                                        ));
                                    }
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
                    }

                    emit_live(Progress::FileComplete {
                        file: source_path.to_string_lossy().to_string(),
                        chunks: n_chunks,
                        signatures: 0,
                    });

                    FileResult {
                        source_path: source_path.clone(),
                        rust_code: combined_rust,
                        n_chunks,
                        succeeded: file_succeeded,
                        total_attempts,
                        is_stub: false,
                    }
                })
                .collect()
        });

        // ── SEQUENTIAL: write files + update checkpoint ──────────────────────
        let abort_msg = fatal.lock().unwrap().clone();
        for result in results.into_iter() {
            // Don't checkpoint files that never ran because of a fatal abort —
            // they must be re-attempted on resume.
            if abort_msg.is_some() && !result.succeeded {
                continue;
            }

            if result.is_stub {
                stub_count += 1;
            }

            let rel = result
                .source_path
                .strip_prefix(&analysis.source_dir)
                .unwrap_or(&result.source_path);

            // Repair module references so the package layout resolves
            // (`storage::Store` → `crate::storage::Store`, strip stray `mod`
            // decls, auto-pub items). Deterministic — runs before dep hints
            // and before the file is written.
            let this_pkg = package_map
                .get(rel)
                .map(|m| m.package.clone())
                .unwrap_or_default();
            let repaired =
                crate::scaffold::repair_module_refs(&result.rust_code, &this_pkg, package_map);
            let extra_deps = extract_dep_hints(&repaired);
            let dest = scaffolder.write_module(rel, &repaired, &extra_deps, package_map)?;

            checkpoint
                .ownership
                .record_rust_signatures(&result.source_path, &repaired);

            let mod_name = package_map
                .get(rel)
                .map(|m| m.root_segment.clone())
                .unwrap_or_else(|| {
                    dest.file_stem()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_else(|| "mod_unknown".into())
                });
            if mod_name != "main" && !checkpoint.module_names.contains(&mod_name) {
                checkpoint.module_names.push(mod_name.clone());
            }

            checkpoint.completed.push(FileTranslation {
                source_path: result.source_path.clone(),
                rust_path: dest,
                module_name: mod_name,
                attempt_count: result.total_attempts,
                succeeded: result.succeeded,
            });

            orch.transition(StateEvent::ChunkAccepted {
                next_chunk_index: checkpoint.completed.len() as u32,
            })
            .ok();

            checkpoint.total_chunks_processed += result.n_chunks;
            checkpoint.next_index = checkpoint.completed.len();
        }

        // Write checkpoint once per batch (not per file)
        store.write("translation", &checkpoint)?;
        info!("Batch done: {}/{total} files", checkpoint.completed.len());

        if let Some(msg) = abort_msg {
            return Err(EngineError::Config(msg));
        }
    }

    if stub_count > 0 {
        emit_live(Progress::Note {
            message: format!(
                "{stub_count} test/fixture/generated file(s) were stubbed instead of translated \
                 (set RUSTYFI_NO_STUB=1 to translate everything)"
            ),
        });
    }

    // Add any curated crates the translated code references but never declared
    // (e.g. tower_http, futures_util) so the first cargo check can resolve.
    let missing = crate::deps::detect_missing_deps(&scaffold.workspace_path, package_map);
    if !missing.is_empty() {
        let names: Vec<&str> = missing.iter().map(|s| s.krate).collect();
        let _ = scaffolder.add_registry_deps(&missing);
        emit_live(Progress::Note {
            message: format!(
                "Added {} missing dependency(ies) the code uses but didn't declare: {}",
                missing.len(),
                names.join(", "),
            ),
        });
    }

    // Wire up main.rs with the package module declarations.
    scaffolder.write_main(package_map)?;

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
    package_map: &crate::scaffold::PackageMap,
    progress_cb: &mut F,
) -> Result<(VerificationCheckpoint, Option<DeepFixSummary>), EngineError>
where
    F: FnMut(Progress),
{
    let ws = &scaffold.workspace_path;
    let verify_scaffolder = Scaffolder::new(ws.clone(), scaffold.crate_name.clone());

    // Reconcile missing deps before the very first check so it resolves.
    {
        let missing = crate::deps::detect_missing_deps(ws, package_map);
        let _ = verify_scaffolder.add_registry_deps(&missing);
    }

    emit(
        progress_cb,
        Progress::Note {
            message: format!(
                "Compiling with cargo check — the first run may download dependencies. \
             Compile fixes will use model `{}`.",
                llm.model(),
            ),
        },
    );

    // cargo missing entirely → skip verification with an honest note instead
    // of feeding "cargo not available" to the LLM for N pointless fix cycles.
    let Some(mut current) = cargo_check_opt(ws) else {
        emit(
            progress_cb,
            Progress::Note {
                message: "cargo was not found on this machine — skipping compile verification. \
                      The generated crate was NOT compile-checked."
                    .into(),
            },
        );
        return Ok((
            VerificationCheckpoint {
                exit_clean: false,
                fix_cycles: vec![],
                final_error_count: 0,
            },
            None,
        ));
    };

    // ── Dependency repair (runs BEFORE the source fix loop) ──────────────
    // `cargo check` fails at *resolution* — before compiling anything — when
    // the LLM mapped a library to a crate name/version that doesn't exist on
    // crates.io (e.g. Go's badger/v4 → `badger = "4.0.0"`). The source fix
    // loop can't help: it only edits .rs files. So strip the unresolvable
    // deps that cargo itself names, until the dependency graph builds and the
    // *real* compile errors become visible.
    let mut stripped_deps: Vec<String> = Vec::new();
    for _ in 0..8 {
        if current.exit_code == Some(0) {
            break;
        }
        let bad = unresolvable_deps(&current);
        if bad.is_empty() {
            break; // not a resolution problem — these are real compile errors
        }
        if !strip_deps_from_cargo(ws, &bad) {
            break; // cargo named a dep we can't strip (e.g. transitive) — give up
        }
        emit(
            progress_cb,
            Progress::Note {
                message: format!(
                    "Removed {} unresolved dependency(ies) from Cargo.toml: {} — they \
                 aren't real crates. Code using them needs a real equivalent \
                 (flagged in NEXT_STEPS.md).",
                    bad.len(),
                    bad.join(", "),
                ),
            },
        );
        stripped_deps.extend(bad);
        current = match cargo_check_opt(ws) {
            Some(o) => o,
            None => break,
        };
    }
    let deps_unresolved = !unresolvable_deps(&current).is_empty();

    // ── Deterministic rustfix pass ───────────────────────────────────────
    // Apply rustc's own machine-applicable suggestions (similar-name fns,
    // arg-count fixes, missing `&`/derives) before spending any LLM tokens.
    // Free, exact, and clears a large fraction of the "mechanical" errors.
    if !deps_unresolved && current.exit_code != Some(0) {
        let fixed = crate::rustfix::apply_machine_suggestions(ws, 6);
        if fixed.applied > 0 {
            emit(
                progress_cb,
                Progress::Note {
                    message: format!(
                        "Auto-fixed {} compile error(s) using the compiler's own suggestions \
                     (no AI needed) over {} pass(es).",
                        fixed.applied, fixed.passes,
                    ),
                },
            );
            current = cargo_check_opt(ws).unwrap_or(current);
        }
    }

    let mut exit_clean = current.exit_code == Some(0);
    let mut fix_cycles: Vec<FixCycleSummary> = vec![];
    // Diagnostics of the most recent check — refreshed every cycle so the LLM
    // always fixes against the errors that are actually present.
    let mut diags = parse_cargo_diagnostics(&current).unwrap_or_default();

    if !exit_clean {
        for attempt in 1..=config.verify_retries {
            emit(progress_cb, Progress::FixCycle { attempt });

            // Rebuild the item index each cycle — fixes rewrite files.
            // Failure produces an empty index; the fix loop continues without context.
            let item_index = fix_context::ItemIndex::build(ws);

            let families = classify_and_rank(&diags);
            let family_names: Vec<String> = families.iter().map(|(n, _)| n.to_string()).collect();

            // In --message-format=json mode the human-readable errors live in
            // each diagnostic's `rendered` field, not in stderr.
            let errors_text = render_errors(&diags, &current);
            let errors_summary = truncate_utf8(&errors_text, 8_000);

            emit(
                progress_cb,
                Progress::CompilerError {
                    message: errors_summary.to_string(),
                    families: family_names.clone(),
                },
            );

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
                    let ctx = item_index.context_for(path, &diags, fix_context::FIX_CTX_BUDGET);
                    let prompt = prompt_fix_targeted(&code, errors_summary, &top_families, &ctx);
                    match llm.complete(SYSTEM_FIX, &prompt) {
                        Ok(raw) => {
                            // Re-apply module repair: the LLM rewrites the whole
                            // file and may reintroduce bare `pkg::` paths or
                            // stray `mod` decls. Derive the file's own package
                            // (its dir name) so self-references aren't rewritten.
                            let this_pkg = if path.ends_with("mod.rs") {
                                path.parent()
                                    .and_then(|p| p.file_name())
                                    .map(|s| s.to_string_lossy().to_string())
                                    .unwrap_or_default()
                            } else {
                                String::new()
                            };
                            let repaired = crate::scaffold::repair_module_refs(
                                &extract_rust_code(&raw),
                                &this_pkg,
                                package_map,
                            );
                            // Normalize: a whole-file rewrite (esp. by a strong
                            // model) can re-introduce duplicate definitions/uses
                            // that the translation-time dedup would have caught.
                            let fixed = crate::scaffold::normalize_module_content(&repaired);
                            let _ = fs::write(path, fixed);
                        }
                        Err(EngineError::Config(msg)) => {
                            // Auth died mid-verify — stop here, the crate is
                            // still packaged in its current state.
                            return Err(EngineError::Config(msg));
                        }
                        Err(e) => warn!("fix-loop LLM error on {}: {e}", path.display()),
                    }
                }
            }

            // Re-assert the crate-root module wiring. The fix loop rewrites
            // whole files and `repair_module_refs` strips `pub mod <pkg>;` lines
            // (correct for package files, but main.rs OWNS those declarations).
            // write_main is idempotent and re-adds any that were dropped.
            let _ = verify_scaffolder.write_main(package_map);
            // The LLM may reference new crates while fixing — declare them.
            let missing = crate::deps::detect_missing_deps(ws, package_map);
            let _ = verify_scaffolder.add_registry_deps(&missing);

            let error_count_before = diags
                .iter()
                .filter(|d| d.level >= rustyfi_core::state::DiagnosticLevel::Error)
                .count();

            // Sweep up anything the LLM left that rustc can mechanically fix.
            let _ = crate::rustfix::apply_machine_suggestions(ws, 3);

            current = match cargo_check_opt(ws) {
                Some(o) => o,
                None => break, // cargo disappeared mid-run; keep last state
            };
            exit_clean = current.exit_code == Some(0);
            diags = parse_cargo_diagnostics(&current).unwrap_or_default();

            fix_cycles.push(FixCycleSummary {
                attempt,
                error_count: error_count_before,
                dominant_families: family_names,
                resolved: exit_clean,
            });

            if exit_clean {
                info!("cargo check clean after fix cycle {attempt}");
                break;
            }
        }
    }

    let mut final_error_count = diags
        .iter()
        .filter(|d| d.level >= rustyfi_core::state::DiagnosticLevel::Error)
        .count();

    // ── Optional agentic deep-fix pass (RUSTYFI_DEEP_FIX=1 / --deep) ────────
    // Engages after the cheap loop + rustfix sweep if the crate is still not
    // clean.  The deep-fix pass is snapshot-reverted at this level if it
    // does not strictly improve the error count.
    let mut deep_fix_summary: Option<DeepFixSummary> = None;

    if !exit_clean && std::env::var("RUSTYFI_DEEP_FIX").is_ok() {
        let max_tool_calls = std::env::var("RUSTYFI_DEEP_FIX_BUDGET")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(40);
        let max_wall_secs = std::env::var("RUSTYFI_DEEP_FIX_TIMEOUT")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(1200);

        let budget = crate::agent_fix::DoctorBudget {
            max_tool_calls,
            max_wall_secs,
        };

        emit(
            progress_cb,
            Progress::Note {
                message: format!(
                    "🩺 Deep-fix: engaging the doctor (budget: {} tool calls / {}s)…",
                    max_tool_calls, max_wall_secs,
                ),
            },
        );

        // Capture pre-doctor error count using the same counting method as
        // phase_verify (>= Error) so the keep/revert comparison is consistent.
        let pre_doctor_errors = final_error_count;

        // Take a snapshot of <ws>/src so we can revert if the doctor makes
        // things worse.  If snapshotting fails we skip the deep-fix pass
        // (we cannot safely revert without a snapshot).
        match snapshot_src(ws) {
            Err(e) => {
                emit(
                    progress_cb,
                    Progress::Note {
                        message: format!("🩺 doctor: skipping — could not snapshot workspace: {e}"),
                    },
                );
            }
            Ok(snap) => {
                let mut transport = crate::agent_fix::LlmTransport(llm);
                let report = crate::agent_fix::run_doctor(ws, &mut transport, budget, &mut |msg| {
                    emit(progress_cb, Progress::Note { message: msg })
                });

                // After run_doctor the workspace is in the doctor's final state.
                // Run a fresh cargo check so our local `exit_clean`,
                // `diags`, and `final_error_count` are authoritative
                // (run_doctor's internal check uses == Error; we use >= Error).
                if let Some(fresh) = cargo_check_opt(ws) {
                    let fresh_diags = parse_cargo_diagnostics(&fresh).unwrap_or_default();
                    let post_doctor_errors = fresh_diags
                        .iter()
                        .filter(|d| d.level >= rustyfi_core::state::DiagnosticLevel::Error)
                        .count();

                    if post_doctor_errors < pre_doctor_errors {
                        // Doctor improved things — keep the changes.
                        exit_clean = fresh.exit_code == Some(0);
                        final_error_count = post_doctor_errors;
                        emit(
                            progress_cb,
                            Progress::Note {
                                message: format!(
                                    "🩺 doctor: {pre_doctor_errors} → {post_doctor_errors} errors (kept)"
                                ),
                            },
                        );
                        // Suppress unused-variable warnings — diags and current are
                        // fully superseded; only exit_clean and final_error_count flow forward.
                        let _ = (fresh_diags, fresh);
                    } else {
                        // Doctor made no improvement — restore the snapshot.
                        if let Err(e) = restore_src(ws, &snap) {
                            emit(
                                progress_cb,
                                Progress::Note {
                                    message: format!(
                                        "🩺 doctor: restore failed ({e}) — workspace may be in a bad state"
                                    ),
                                },
                            );
                        } else {
                            emit(
                                progress_cb,
                                Progress::Note {
                                    message: "🩺 doctor made no improvement — reverted".to_string(),
                                },
                            );
                        }
                        // Re-run cargo check after restore so downstream counts are truthful.
                        // SAFETY: this runs after the restore copy loop completes;
                        // if restore_src failed we still re-check to get an honest count.
                        if let Some(post_revert) = cargo_check_opt(ws) {
                            let post_diags =
                                parse_cargo_diagnostics(&post_revert).unwrap_or_default();
                            final_error_count = post_diags
                                .iter()
                                .filter(|d| d.level >= rustyfi_core::state::DiagnosticLevel::Error)
                                .count();
                            exit_clean = post_revert.exit_code == Some(0);
                            // Suppress unused-variable warnings.
                            let _ = (post_diags, post_revert);
                        }
                    }

                    // Record the deep-fix summary regardless of keep/revert (honest).
                    deep_fix_summary = Some(DeepFixSummary {
                        ran: true,
                        start_errors: pre_doctor_errors,
                        end_errors: final_error_count,
                        tool_calls: report.tool_calls_used,
                    });
                } else {
                    // cargo check unavailable after doctor — restore to be safe.
                    emit(
                        progress_cb,
                        Progress::Note {
                            message: "🩺 doctor: couldn't re-verify with cargo afterwards — \
                                      changes reverted, pre-doctor state kept."
                                .into(),
                        },
                    );
                    let _ = restore_src(ws, &snap);
                }
            }
        }
    }

    if exit_clean {
        emit(
            progress_cb,
            Progress::Note {
                message: "cargo check passed — the generated crate compiles ✓".into(),
            },
        );
    } else if deps_unresolved {
        // Resolution still fails → there are no compiler diagnostics to count,
        // so don't claim "0 errors". Be explicit about the real blocker.
        emit(
            progress_cb,
            Progress::Note {
                message: "cargo couldn't resolve all dependencies, so the crate wasn't \
                      compile-checked. Fix the remaining crate names in Cargo.toml \
                      (see NEXT_STEPS.md), then run `cargo build`."
                    .into(),
            },
        );
    } else {
        emit(
            progress_cb,
            Progress::Note {
                message: format!(
                    "cargo check reports {final_error_count} compile error(s){} — the crate \
                 ships with TODO markers where manual attention is needed (see NEXT_STEPS.md).",
                    if stripped_deps.is_empty() {
                        String::new()
                    } else {
                        format!(" after removing {} unresolved dep(s)", stripped_deps.len())
                    },
                ),
            },
        );
    }

    Ok((
        VerificationCheckpoint {
            exit_clean,
            fix_cycles,
            final_error_count,
        },
        deep_fix_summary,
    ))
}

// ---------------------------------------------------------------------------
// Snapshot / restore helpers for the deep-fix pass
// ---------------------------------------------------------------------------

/// Recursively copy `<ws>/src` to a temporary directory.
/// Returns the `TempDir` which must be kept alive until `restore_src` is called
/// (dropping it deletes the snapshot).
pub fn snapshot_src(ws: &Path) -> Result<TempDir, EngineError> {
    let src = ws.join("src");
    let snap =
        TempDir::new().map_err(|e| EngineError::Io(format!("snapshot: create tempdir: {e}")))?;
    let snap_src = snap.path().join("src");
    fs::create_dir_all(&snap_src)
        .map_err(|e| EngineError::Io(format!("snapshot: create snap/src: {e}")))?;

    if src.is_dir() {
        for entry in walkdir::WalkDir::new(&src).into_iter() {
            let entry = entry.map_err(|e| EngineError::Io(format!("snapshot: walkdir: {e}")))?;
            let rel = entry
                .path()
                .strip_prefix(&src)
                .map_err(|e| EngineError::Io(format!("snapshot: strip prefix: {e}")))?;
            let dest = snap_src.join(rel);

            if entry.file_type().is_dir() {
                fs::create_dir_all(&dest).map_err(|e| {
                    EngineError::Io(format!("snapshot: mkdir {}: {e}", dest.display()))
                })?;
            } else if entry.file_type().is_file() {
                if let Some(parent) = dest.parent() {
                    fs::create_dir_all(parent)
                        .map_err(|e| EngineError::Io(format!("snapshot: mkdir parent: {e}")))?;
                }
                fs::copy(entry.path(), &dest).map_err(|e| {
                    EngineError::Io(format!("snapshot: copy {}: {e}", entry.path().display()))
                })?;
            }
        }
    }

    Ok(snap)
}

/// Restore `<ws>/src` from a snapshot previously produced by `snapshot_src`.
///
/// The workspace `src/` directory is wiped first, then the snapshot is copied
/// back.  This operation is NOT atomic — if the process is killed between the
/// wipe and the full copy the workspace will be in a partial state.  The caller
/// should always run `cargo_check_opt` after this call to get an honest error
/// count.
pub fn restore_src(ws: &Path, snap: &TempDir) -> Result<(), EngineError> {
    let src = ws.join("src");
    let snap_src = snap.path().join("src");

    // Wipe the workspace src directory.
    if src.exists() {
        fs::remove_dir_all(&src)
            .map_err(|e| EngineError::Io(format!("restore: remove_dir_all src: {e}")))?;
    }
    fs::create_dir_all(&src).map_err(|e| EngineError::Io(format!("restore: create src: {e}")))?;

    // Copy the snapshot back.
    if snap_src.is_dir() {
        for entry in walkdir::WalkDir::new(&snap_src).into_iter() {
            let entry = entry.map_err(|e| EngineError::Io(format!("restore: walkdir: {e}")))?;
            let rel = entry
                .path()
                .strip_prefix(&snap_src)
                .map_err(|e| EngineError::Io(format!("restore: strip prefix: {e}")))?;
            let dest = src.join(rel);

            if entry.file_type().is_dir() {
                fs::create_dir_all(&dest).map_err(|e| {
                    EngineError::Io(format!("restore: mkdir {}: {e}", dest.display()))
                })?;
            } else if entry.file_type().is_file() {
                if let Some(parent) = dest.parent() {
                    fs::create_dir_all(parent)
                        .map_err(|e| EngineError::Io(format!("restore: mkdir parent: {e}")))?;
                }
                fs::copy(entry.path(), &dest).map_err(|e| {
                    EngineError::Io(format!("restore: copy {}: {e}", entry.path().display()))
                })?;
            }
        }
    }

    Ok(())
}

/// Extract crate names that `cargo` could not resolve from a failed check.
/// These errors live in **stderr** (resolution happens before compilation, so
/// there are no JSON compiler-messages for them):
///   error: failed to select a version for the requirement `badger = "^4.0.0"`
///   error: no matching package named `foo` found
fn unresolvable_deps(output: &CargoOutput) -> Vec<String> {
    use std::collections::BTreeSet;
    let mut names = BTreeSet::new();
    for line in &output.stderr_lines {
        let l = line.to_lowercase();
        let is_resolution_err = l.contains("failed to select a version")
            || l.contains("no matching package")
            || l.contains("failed to load source for dependency")
            || (l.contains("failed to get") && l.contains("dependency"));
        if !is_resolution_err {
            continue;
        }
        if let Some(inner) = between_backticks(line) {
            // Backtick content is `name` or `name = "ver"` — take the name.
            let name = inner
                .split(['=', ' '])
                .next()
                .unwrap_or(&inner)
                .trim()
                .to_string();
            if !name.is_empty() {
                names.insert(name);
            }
        }
    }
    names.into_iter().collect()
}

fn between_backticks(s: &str) -> Option<String> {
    let start = s.find('`')? + 1;
    let rest = &s[start..];
    let end = rest.find('`')?;
    Some(rest[..end].to_string())
}

/// Comment out the named direct dependencies in `Cargo.toml` so resolution can
/// proceed. Returns true if at least one line changed. A removed dep stays
/// visible as a `# [rustyfi] removed unresolved dep:` line for the user and the
/// completion report.
fn strip_deps_from_cargo(workspace: &Path, names: &[String]) -> bool {
    let path = workspace.join("Cargo.toml");
    let Ok(content) = fs::read_to_string(&path) else {
        return false;
    };
    let mut changed = false;
    let out = content
        .lines()
        .map(|line| {
            let t = line.trim_start();
            if t.starts_with('#') {
                return line.to_string();
            }
            for name in names {
                if t.starts_with(&format!("{name} ")) || t.starts_with(&format!("{name}=")) {
                    changed = true;
                    return format!("# [rustyfi] removed unresolved dep: {line}");
                }
            }
            line.to_string()
        })
        .collect::<Vec<_>>()
        .join("\n");
    if changed {
        let _ = fs::write(&path, format!("{out}\n"));
    }
    changed
}

/// Human-readable error text for the LLM and the UI: prefer the `rendered`
/// diagnostics (where rustc puts the real messages in JSON mode), fall back
/// to stderr.
fn render_errors(
    diags: &[rustyfi_core::state::CompilerDiagnostic],
    output: &CargoOutput,
) -> String {
    let rendered: Vec<&str> = diags
        .iter()
        .filter(|d| d.level >= rustyfi_core::state::DiagnosticLevel::Error)
        .filter_map(|d| d.rendered.as_deref())
        .collect();
    if rendered.is_empty() {
        output.stderr_lines.join("\n")
    } else {
        rendered.join("\n")
    }
}

// ---------------------------------------------------------------------------
// Phase 5: Packaging
// ---------------------------------------------------------------------------

fn phase_package(
    scaffold: &ScaffoldCheckpoint,
    run_dir: &Path,
) -> Result<PackagingCheckpoint, EngineError> {
    let zip_bytes = package_to_zip(&scaffold.workspace_path)?;
    let zip_len = zip_bytes.len();
    let zip_name = format!("{}.zip", scaffold.crate_name);
    let zip_path = run_dir.join(&zip_name);
    fs::write(&zip_path, &zip_bytes).map_err(|e| EngineError::Io(e.to_string()))?;
    info!("Packaged: {} ({zip_len} bytes)", zip_path.display());
    Ok(PackagingCheckpoint {
        zip_path,
        zip_bytes: zip_len,
        crate_name: scaffold.crate_name.clone(),
    })
}

// ---------------------------------------------------------------------------
// Completion report — "what's left to compile"
// ---------------------------------------------------------------------------

/// Base dependencies the scaffolder always writes (see scaffold.rs). These are
/// known-good, so the report doesn't ask the user to verify them.
const BASE_DEPS: &[&str] = &[
    "serde",
    "serde_json",
    "thiserror",
    "anyhow",
    "tokio",
    "reqwest",
    "tracing",
    "tracing-subscriber",
];

struct NextSteps {
    markdown: String,
    summary_lines: Vec<String>,
    todo_count: usize,
    translated: usize,
}

/// Map a crate name to a "you must install this system package first" hint.
/// Returns `None` for pure-Rust crates that need nothing extra.
fn system_dep_hint(crate_name: &str) -> Option<&'static str> {
    match crate_name.to_lowercase().as_str() {
        "openssl" | "openssl-sys" =>
            "OpenSSL dev headers — macOS: `brew install openssl`, Debian/Ubuntu: `apt install libssl-dev`",
        "ssh2" | "libssh2-sys" =>
            "libssh2 — macOS: `brew install libssh2`, Debian/Ubuntu: `apt install libssh2-1-dev`",
        "pq-sys" | "libpq-sys" | "postgres" | "tokio-postgres" =>
            "PostgreSQL client — macOS: `brew install libpq`, Debian/Ubuntu: `apt install libpq-dev`",
        "mysqlclient-sys" | "mysql" =>
            "MySQL/MariaDB client — Debian/Ubuntu: `apt install libmysqlclient-dev`",
        "libsqlite3-sys" =>
            "SQLite — usually bundled; if not: `apt install libsqlite3-dev`",
        "gtk" | "gtk4" | "gdk" | "gdk-pixbuf" | "cairo-rs" | "pango" | "glib" | "gio" =>
            "GTK stack — macOS: `brew install gtk4`, Debian/Ubuntu: `apt install libgtk-4-dev`",
        "zmq" =>
            "ZeroMQ — macOS: `brew install zeromq`, Debian/Ubuntu: `apt install libzmq3-dev`",
        "curl" | "curl-sys" =>
            "libcurl — Debian/Ubuntu: `apt install libcurl4-openssl-dev`",
        "libgit2-sys" | "git2" =>
            "libgit2 — macOS: `brew install libgit2`, Debian/Ubuntu: `apt install libgit2-dev`",
        _ => return None,
    }
    .into()
}

/// Parse the `name` of each entry under `[dependencies]` in a Cargo.toml.
fn parse_dependency_names(cargo_toml: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut in_deps = false;
    for line in cargo_toml.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_deps = t == "[dependencies]";
            continue;
        }
        if !in_deps || t.is_empty() || t.starts_with('#') {
            continue;
        }
        if let Some((name, _)) = t.split_once('=') {
            let name = name.trim().trim_matches('"');
            if !name.is_empty() {
                names.push(name.to_string());
            }
        }
    }
    names
}

/// Scan the generated `src/` tree once: classify stubbed/failed files and count
/// `todo!()` gaps the model left behind.
struct SrcScan {
    stub_files: Vec<String>,
    failed_files: Vec<String>,
    todo_count: usize,
}

fn scan_src(workspace: &Path) -> SrcScan {
    let src = workspace.join("src");
    let mut stub_files = Vec::new();
    let mut failed_files = Vec::new();
    let mut todo_count = 0usize;

    for entry in walkdir::WalkDir::new(&src)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let Ok(text) = fs::read_to_string(path) else {
            continue;
        };
        let rel = path
            .strip_prefix(&src)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();

        todo_count += text.matches("todo!(").count();

        if text.contains("// [STUB] Skipped by Rustyfi") {
            stub_files.push(rel);
        } else if text.contains("translation failed for chunk")
            || text.contains("run aborted before this file")
            || text.contains("// read error:")
        {
            failed_files.push(rel);
        }
    }

    stub_files.sort();
    failed_files.sort();
    SrcScan {
        stub_files,
        failed_files,
        todo_count,
    }
}

/// Build the human-facing "what's left to compile" report.
fn build_next_steps(
    analysis: &AnalysisCheckpoint,
    scaffold: &ScaffoldCheckpoint,
    translation: &TranslationCheckpoint,
    verification: &VerificationCheckpoint,
    doctor: Option<&DeepFixSummary>,
) -> NextSteps {
    let ws = &scaffold.workspace_path;
    let scan = scan_src(ws);
    let cargo_toml = fs::read_to_string(ws.join("Cargo.toml")).unwrap_or_default();
    let dep_names = parse_dependency_names(&cargo_toml);

    // Deps the verify phase removed because they don't resolve on crates.io.
    let removed_deps: Vec<String> = cargo_toml
        .lines()
        .filter_map(|l| l.trim().strip_prefix("# [rustyfi] removed unresolved dep:"))
        .filter_map(|rest| rest.trim().split('=').next())
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
        .collect();

    let translated = translation.completed.iter().filter(|f| f.succeeded).count();

    // Inferred (non-base) deps split into native-library vs pure-Rust.
    let inferred: Vec<&String> = dep_names
        .iter()
        .filter(|n| !BASE_DEPS.contains(&n.as_str()))
        .collect();
    let mut native: Vec<(&String, &'static str)> = Vec::new();
    let mut pure: Vec<&String> = Vec::new();
    for n in &inferred {
        match system_dep_hint(n) {
            Some(h) => native.push((n, h)),
            None if n.ends_with("-sys") => {
                native.push((n, "links a native C library — install the matching system package and ensure a C toolchain is present"));
            }
            None => pure.push(n),
        }
    }

    let mut md = String::new();
    md.push_str("# What's left to compile 🦀\n\n");
    md.push_str(&format!(
        "Rustyfi translated **{translated} file(s)** of {} into Rust. \
         This crate is a **starting point**, not a finished build — here's exactly \
         what you need to do to get a clean `cargo build`.\n\n",
        analysis.language,
    ));

    // 0. Compile status
    md.push_str("## Compile status\n\n");
    if verification.exit_clean {
        md.push_str(
            "✅ `cargo check` **passed** on the generated crate. Nice — \
                     finish any `todo!()` gaps below and you're done.\n\n",
        );
    } else {
        md.push_str(&format!(
            "❌ `cargo check` still reports **{} error(s)** after \
             {} automated fix cycle(s). The steps below are how to clear them.\n\n",
            verification.final_error_count,
            verification.fix_cycles.len(),
        ));
    }

    // 1. System libraries
    md.push_str("## 1. Install system libraries\n\n");
    if native.is_empty() {
        md.push_str("No native-library dependencies were detected. Nothing to install. 🎉\n\n");
    } else {
        md.push_str(
            "These dependencies link to native C libraries — install the \
                     system package **before** building:\n\n",
        );
        for (name, hint) in &native {
            md.push_str(&format!("- **`{name}`** — {hint}\n"));
        }
        md.push('\n');
    }

    // 2. Verify inferred dependencies
    md.push_str("## 2. Verify the inferred dependencies\n\n");
    if !removed_deps.is_empty() {
        md.push_str(&format!(
            "⚠️ Rustyfi **removed {} dependency(ies)** that don't resolve on crates.io \
             (the model invented or mis-mapped them): {}. They're left as \
             `# [rustyfi] removed unresolved dep:` comments in `Cargo.toml`. \
             Any code importing them won't compile until you add a real crate \
             (e.g. a Go KV store like badger → the Rust `sled` crate) and fix the imports.\n\n",
            removed_deps.len(),
            removed_deps
                .iter()
                .map(|n| format!("`{n}`"))
                .collect::<Vec<_>>()
                .join(", "),
        ));
    }
    if inferred.is_empty() {
        md.push_str("Only the base runtime crates were used — nothing to verify.\n\n");
    } else {
        md.push_str(&format!(
            "Rustyfi added **{} dependency(ies)** that the model inferred from your \
             code. Model-inferred crate names are sometimes wrong or hallucinated. \
             Run `cargo build`; for any *“no matching package named …”* error, fix \
             the crate name or remove the line in `Cargo.toml`.\n\n",
            inferred.len(),
        ));
        if !pure.is_empty() {
            md.push_str("Pure-Rust deps to sanity-check: ");
            md.push_str(
                &pure
                    .iter()
                    .map(|n| format!("`{n}`"))
                    .collect::<Vec<_>>()
                    .join(", "),
            );
            md.push_str("\n\n");
        }
    }

    // 3. Stubs
    md.push_str("## 3. Stubbed & untranslated files\n\n");
    if scan.stub_files.is_empty() && scan.failed_files.is_empty() {
        md.push_str("Every source file was translated — no stubs. ✅\n\n");
    } else {
        if !scan.stub_files.is_empty() {
            md.push_str(&format!(
                "**{} file(s) were intentionally stubbed** (tests / fixtures / \
                 generated code). They contain only a `// TODO: implement this module` \
                 placeholder. Re-run with `RUSTYFI_NO_STUB=1` to translate them too:\n\n",
                scan.stub_files.len(),
            ));
            for f in scan.stub_files.iter().take(40) {
                md.push_str(&format!("- `src/{f}`\n"));
            }
            if scan.stub_files.len() > 40 {
                md.push_str(&format!("- …and {} more\n", scan.stub_files.len() - 40));
            }
            md.push('\n');
        }
        if !scan.failed_files.is_empty() {
            md.push_str(&format!(
                "**{} file(s) could not be translated** (the model errored or the file \
                 was unreadable). They contain `// TODO: translation failed` markers and \
                 need a manual pass:\n\n",
                scan.failed_files.len(),
            ));
            for f in scan.failed_files.iter().take(40) {
                md.push_str(&format!("- `src/{f}`\n"));
            }
            if scan.failed_files.len() > 40 {
                md.push_str(&format!("- …and {} more\n", scan.failed_files.len() - 40));
            }
            md.push('\n');
        }
    }

    // 4. todo!() gaps
    md.push_str("## 4. Fill the `todo!()` gaps\n\n");
    if scan.todo_count == 0 {
        md.push_str("No `todo!()` placeholders — the model mapped every construct it saw. ✅\n\n");
    } else {
        md.push_str(&format!(
            "The model left **{} `todo!()` placeholder(s)** where it couldn't map a \
             construct to Rust. These compile but panic at runtime — implement them:\n\n\
             ```sh\ngrep -rn 'todo!(' src/\n```\n\n",
            scan.todo_count,
        ));
    }

    // 5. Build
    md.push_str("## 5. Build it\n\n```sh\ncargo build\n```\n\n");
    md.push_str(
        "A first pass rarely compiles clean — fix errors top-down; each fix \
                 often clears several below it. The translated logic is the hard part; \
                 what remains is wiring, dependency names, and the gaps listed above.\n\n",
    );

    // 6. Doctor (optional — only present when the deep-fix pass ran)
    if let Some(doc) = doctor {
        md.push_str("## 6. Agentic deep-fix pass\n\n");
        let outcome = if doc.end_errors < doc.start_errors {
            format!(
                "The deep-fix agent reduced errors from **{}** to **{}** ({} tool calls used).",
                doc.start_errors, doc.end_errors, doc.tool_calls
            )
        } else {
            format!(
                "The deep-fix agent ran ({} tool calls) but did not reduce the error count \
                 ({} → {}) — changes were reverted. Try re-running with a stronger \
                 `RUSTYFI_FIX_MODEL`.",
                doc.tool_calls, doc.start_errors, doc.end_errors
            )
        };
        md.push_str(&outcome);
        md.push_str("\n\n");
    }

    md.push_str("---\n_Generated by Rustyfi 🎺 — `cargo check` is the truth, not the model._\n");

    // Concise terminal summary.
    let mut summary_lines = Vec::new();
    summary_lines
        .push("📋 Wrote NEXT_STEPS.md to the crate — open it for a full checklist.".to_string());
    if !native.is_empty() {
        summary_lines.push(format!(
            "🔧 {} native-library dep(s) need a system package first (see NEXT_STEPS.md §1).",
            native.len(),
        ));
    }
    if scan.todo_count > 0 {
        summary_lines.push(format!(
            "✏️  {} todo!() gap(s) to implement — `grep -rn 'todo!(' src/`.",
            scan.todo_count,
        ));
    }
    if !scan.failed_files.is_empty() {
        summary_lines.push(format!(
            "⚠️  {} file(s) failed to translate and need a manual pass.",
            scan.failed_files.len(),
        ));
    }

    NextSteps {
        markdown: md,
        summary_lines,
        todo_count: scan.todo_count,
        translated,
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn emit<F: FnMut(Progress)>(cb: &mut F, p: Progress) {
    cb(p);
}

/// Truncate a string to at most `max` bytes without splitting a UTF-8
/// character (a plain byte slice panics mid-codepoint).
fn truncate_utf8(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Run `cargo check`; `None` means cargo itself could not be spawned
/// (not installed / not on PATH) — callers should skip verification.
fn cargo_check_opt(workspace: &Path) -> Option<CargoOutput> {
    match rustyfi_core::compiler::run_cargo_check(workspace) {
        Ok(o) => Some(o),
        Err(e) => {
            warn!("cargo check unavailable: {e}");
            None
        }
    }
}

/// Classify error-level diagnostics into deduplicated, priority-ranked
/// family/hint pairs.
fn classify_and_rank<'a>(
    diags: &[rustyfi_core::state::CompilerDiagnostic],
) -> Vec<(&'a str, &'a str)> {
    use std::collections::BTreeMap;

    // Key by family name so distinct families with equal retry priority
    // don't collapse into one bucket; only errors drive repair.
    let mut counts: BTreeMap<&'static str, (DiagnosticFamily, usize)> = BTreeMap::new();
    for d in diags {
        if d.level < rustyfi_core::state::DiagnosticLevel::Error {
            continue;
        }
        let f = d.family();
        counts
            .entry(family_name_static(&f))
            .and_modify(|(_, c)| *c += 1)
            .or_insert((f, 1));
    }

    let mut ranked: Vec<_> = counts.into_values().collect();
    ranked.sort_by_key(|(f, count): &(DiagnosticFamily, usize)| {
        (
            std::cmp::Reverse(f.retry_priority()),
            std::cmp::Reverse(*count),
        )
    });

    ranked
        .into_iter()
        .map(|(f, _)| (family_name_static(&f), f.repair_hint()))
        .collect()
}

fn family_name_static(f: &DiagnosticFamily) -> &'static str {
    match f {
        DiagnosticFamily::MissingLifetime => "MissingLifetime",
        DiagnosticFamily::TraitBoundFailure => "TraitBoundFailure",
        DiagnosticFamily::OwnershipMove => "OwnershipMove",
        DiagnosticFamily::BorrowConflict => "BorrowConflict",
        DiagnosticFamily::TypeMismatch => "TypeMismatch",
        DiagnosticFamily::MissingImport => "MissingImport",
        DiagnosticFamily::AsyncMismatch => "AsyncMismatch",
        DiagnosticFamily::MacroError => "MacroError",
        DiagnosticFamily::PatternExhaustiveness => "PatternExhaustiveness",
        DiagnosticFamily::IntegerOverflow => "IntegerOverflow",
        DiagnosticFamily::UnusedCode => "UnusedCode",
        DiagnosticFamily::InternalCompilerError => "InternalCompilerError",
        DiagnosticFamily::Other(_) => "Other",
    }
}

fn files_with_errors(
    diags: &[rustyfi_core::state::CompilerDiagnostic],
    workspace: &Path,
) -> Vec<PathBuf> {
    use std::collections::HashSet;
    // CRITICAL: only ever touch generated files under <workspace>/src. A
    // diagnostic span can point into a dependency's source in ~/.cargo/registry
    // (e.g. a macro error blames the macro's definition); `workspace.join` on an
    // absolute path yields that absolute path, and the fix loop would then
    // OVERWRITE the user's cached crate source. Scoping to src/ prevents that.
    let src_dir = workspace.join("src");
    let mut seen = HashSet::new();
    diags
        .iter()
        .filter(|d| d.level >= rustyfi_core::state::DiagnosticLevel::Error)
        .flat_map(|d| d.spans.iter())
        .filter(|s| s.is_primary)
        .map(|s| workspace.join(&s.file_name))
        .filter(|p| p.starts_with(&src_dir) && p.exists() && seen.insert(p.clone()))
        .collect()
}

// ---------------------------------------------------------------------------
// Stub filter (IntentGate equivalent)
// ---------------------------------------------------------------------------

/// Returns `Some(reason)` if this file should be stubbed instead of translated.
/// Files that are stubs: test files, mock files, fixture files, generated files,
/// config-only files, and effectively empty files.
///
/// Opt out by setting `RUSTYFI_NO_STUB=1`.
fn classify_stub(path: &Path, source_code: &str) -> Option<&'static str> {
    if std::env::var("RUSTYFI_NO_STUB").is_ok() {
        return None;
    }

    let name_lower = path
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();

    let path_str = path.to_string_lossy().to_lowercase();

    // Empty / near-empty file
    let trimmed = source_code.trim();
    if trimmed.is_empty() || trimmed.len() < 16 {
        return Some("empty file");
    }

    // Test files — filename patterns
    if name_lower.starts_with("test_")
        || name_lower.ends_with("_test.go")
        || name_lower.ends_with("_test.py")
        || name_lower.ends_with(".test.ts")
        || name_lower.ends_with(".test.js")
        || name_lower.ends_with(".spec.ts")
        || name_lower.ends_with(".spec.js")
        || name_lower == "tests.py"
        || name_lower == "test.py"
    {
        return Some("test file");
    }

    // Test files — path patterns
    if path_str.contains("/test/")
        || path_str.contains("/tests/")
        || path_str.contains("/testdata/")
        || path_str.contains("/__tests__/")
        || path_str.contains("/test-support/")
        || path_str.contains("/e2e/")
        || path_str.contains("/fixtures/")
        || path_str.contains("/mocks/")
        || path_str.contains("/mock/")
        || path_str.contains("/stubs/")
        || path_str.contains("/fakes/")
        || path_str.contains("/fake/")
    {
        return Some("test/fixture path");
    }

    // Generated files — name patterns
    if name_lower.ends_with(".generated.ts")
        || name_lower.ends_with(".generated.js")
        || name_lower.ends_with(".generated.py")
        || name_lower.ends_with(".gen.go")
        || name_lower.ends_with(".pb.go")
        || name_lower.ends_with("_pb2.py")
        || name_lower.ends_with("_grpc.py")
        || name_lower.ends_with(".g.dart")
    {
        return Some("generated file");
    }

    // Generated files — content markers
    let first_500 = truncate_utf8(trimmed, 500);
    if first_500.contains("DO NOT EDIT")
        || first_500.contains("Code generated")
        || first_500.contains("AUTO-GENERATED")
        || first_500.contains("@generated")
        || first_500.contains("autogenerated")
        || first_500.contains("// AUTOGENERATED")
        || first_500.contains("# AUTOGENERATED")
    {
        return Some("auto-generated (header marker)");
    }

    // Config/data files with trivial source-language content
    if (name_lower == "setup.py" || name_lower == "setup.cfg") && trimmed.len() < 300 {
        return Some("trivial config");
    }

    None
}

// ---------------------------------------------------------------------------
// Tiered model routing
// ---------------------------------------------------------------------------

/// Estimate token count from raw source. Approximation: 1 token ≈ 4 chars.
/// This is faster than a real tokenizer and good enough for routing.
#[inline]
fn estimate_tokens(source: &str) -> usize {
    source.len() / 4
}

/// Map estimated token count to a model override string.
/// Returns `None` to use the default (flagship) model.
///
/// Tier logic (all overridable via env):
/// - `fast`:  tokens < `tier_fast` → cheap/high-RPM model  
/// - `mid`:   tokens < `tier_mid`  → standard model  
/// - `full`:  everything else      → default (no override)
fn tier_for_tokens(tokens: usize, tier_fast: usize, tier_mid: usize) -> Option<String> {
    // Grok-build only exposes grok-build and composer-2.5 — no lightweight
    // fast tier exists on that endpoint. Skip tiering entirely and let the
    // LlmClient use whatever model was configured (grok-build by default).
    // Tiering only makes sense on OpenRouter where flash-lite/flash/flash-2.5
    // are all available.
    let is_grok = matches!(
        std::env::var("RUSTYFI_PROVIDER").as_deref(),
        Ok("grok") | Ok("xai")
    );
    if is_grok {
        return None;
    }

    if tokens < tier_fast {
        // Fast tier: high-RPM, low-cost, good for tiny files
        let m = std::env::var("RUSTYFI_MODEL_FAST")
            .unwrap_or_else(|_| "google/gemini-2.5-flash".to_string());
        Some(m)
    } else if tokens < tier_mid {
        // Mid tier: balanced speed/quality
        let m = std::env::var("RUSTYFI_MODEL_MID")
            .unwrap_or_else(|_| "google/gemini-2.5-flash".to_string());
        Some(m)
    } else {
        // Full tier: use whatever the LlmClient was constructed with
        None
    }
}

// Lowercase before filtering, ASCII only — keeps the function idempotent even
// for Unicode names whose lowercase form expands into combining marks, and
// matches the server-side sanitiser exactly.
fn sanitise_crate_name(name: String) -> String {
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
    // Never return an empty crate name — downstream code joins it onto
    // output directories.
    if s.trim_matches('_').is_empty() {
        "app".to_string()
    } else if s.starts_with(|c: char| c.is_ascii_digit()) {
        format!("crate_{s}")
    } else {
        s
    }
}

fn module_label(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .to_string()
}

/// Build the directory-as-package module map from the analysis checkpoint.
/// Pure function of the file list + language, so it's identical on resume.
fn build_package_map_from_analysis(analysis: &AnalysisCheckpoint) -> crate::scaffold::PackageMap {
    use std::collections::HashSet;
    let src = &analysis.source_dir;
    let rels: Vec<PathBuf> = analysis
        .target_paths
        .iter()
        .map(|p| p.strip_prefix(src).unwrap_or(p).to_path_buf())
        .collect();
    let entries: HashSet<PathBuf> = analysis
        .inferred_entrypoints
        .iter()
        .map(|p| p.strip_prefix(src).unwrap_or(p).to_path_buf())
        .collect();
    crate::scaffold::build_package_map(
        &rels,
        &entries,
        crate::scaffold::dir_namespaced(&analysis.language),
    )
}

fn lang_display(meta: &LanguageMetadata) -> String {
    use rustyfi_core::context::SourceLanguage;
    match &meta.primary_language {
        SourceLanguage::Python => "python".into(),
        SourceLanguage::TypeScript => "typescript".into(),
        SourceLanguage::JavaScript => "javascript".into(),
        SourceLanguage::Go => "go".into(),
        SourceLanguage::Cpp => "cpp".into(),
        SourceLanguage::C => "c".into(),
        SourceLanguage::Java => "java".into(),
        SourceLanguage::CSharp => "csharp".into(),
        SourceLanguage::Ruby => "ruby".into(),
        SourceLanguage::Other(s) => s.clone(),
    }
}

/// Extract `// [DEPS] crate = "version"` comments emitted by the LLM.
/// Extract and **sanitise** `// [DEPS] crate = "version"` hints emitted by the
/// LLM. These hints are unreliable — real runs produce malformed versions
/// (`"0.2" (optional)`), inline comments, non-crates (`std`), and hallucinated
/// names. We only keep hints that yield a valid crate name + version token, so
/// the generated `Cargo.toml` is *always* parseable TOML (a malformed hint can
/// never wedge `cargo check` before it even sees the Rust code).
fn extract_dep_hints(code: &str) -> HashMap<String, String> {
    let mut deps = HashMap::new();
    for line in code.lines() {
        let Some(rest) = line.trim().strip_prefix("// [DEPS]") else {
            continue;
        };
        for part in rest.split(',') {
            let Some((raw_name, raw_ver)) = part.split_once('=') else {
                continue;
            };
            let Some(name) = sanitise_dep_name(raw_name) else {
                continue;
            };
            let Some(ver) = sanitise_dep_version(raw_ver) else {
                continue;
            };
            deps.insert(name, ver);
        }
    }
    deps
}

/// Valid crate name: starts with a letter, only `[A-Za-z0-9_-]`. Rejects
/// built-ins and the base deps already present in the scaffold manifest.
fn sanitise_dep_name(raw: &str) -> Option<String> {
    let name = raw.trim().trim_matches('"').trim();
    if name.is_empty() {
        return None;
    }
    if !name.chars().next()?.is_ascii_alphabetic() {
        return None;
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return None;
    }
    // Not real crates, or already in the base Cargo.toml (see scaffold.rs).
    const REJECT: &[&str] = &[
        "std",
        "core",
        "alloc",
        "proc_macro",
        "test",
        "serde",
        "serde_json",
        "thiserror",
        "anyhow",
        "tokio",
        "reqwest",
        "tracing",
        "tracing-subscriber",
    ];
    if REJECT.contains(&name) {
        return None;
    }
    Some(name.to_string())
}

/// Extract a clean semver requirement, discarding trailing comments and
/// parenthetical junk (`"0.2" (optional)` → `0.2`). Accepts `*`, an optional
/// leading `^`/`~`, then `N(.N)*`. Returns None if no version token is present.
fn sanitise_dep_version(raw: &str) -> Option<String> {
    let v = raw.trim().trim_matches('"').trim();
    if v.starts_with('*') {
        return Some("*".to_string());
    }
    let mut out = String::new();
    let mut rest = v;
    if let Some(stripped) = v.strip_prefix('^').or_else(|| v.strip_prefix('~')) {
        out.push(v.chars().next().unwrap());
        rest = stripped;
    }
    let mut saw_digit = false;
    for c in rest.chars() {
        if c.is_ascii_digit() {
            out.push(c);
            saw_digit = true;
        } else if c == '.' && saw_digit && !out.ends_with('.') {
            out.push(c);
        } else {
            break; // stop at the first non-version char (space, quote, '(', '/')
        }
    }
    if !saw_digit {
        return None;
    }
    Some(out.trim_end_matches('.').to_string())
}

fn unix_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("unix:{secs}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_sanitiser_strips_trailing_junk() {
        // The exact garbage that broke the THC Hydra run's Cargo.toml.
        assert_eq!(
            sanitise_dep_version(" \"0.2\" (optional)"),
            Some("0.2".into())
        );
        assert_eq!(
            sanitise_dep_version(" \"0.1\" // hypothetical binding"),
            Some("0.1".into())
        );
        assert_eq!(sanitise_dep_version("\"0.1.0\""), Some("0.1.0".into()));
        assert_eq!(sanitise_dep_version(" \"3\""), Some("3".into()));
        assert_eq!(sanitise_dep_version("^1.2"), Some("^1.2".into()));
        assert_eq!(sanitise_dep_version("*"), Some("*".into()));
        // No version token at all → dropped.
        assert_eq!(sanitise_dep_version("\"latest\""), None);
        assert_eq!(sanitise_dep_version(""), None);
    }

    #[test]
    fn name_sanitiser_rejects_non_crates() {
        assert_eq!(sanitise_dep_name(" libc"), Some("libc".into()));
        assert_eq!(sanitise_dep_name("ncp-sys"), Some("ncp-sys".into()));
        assert_eq!(sanitise_dep_name("std"), None); // not a crate
        assert_eq!(sanitise_dep_name("serde"), None); // already in base manifest
        assert_eq!(sanitise_dep_name("2foo"), None); // can't start with a digit
        assert_eq!(sanitise_dep_name("foo bar"), None); // space is illegal
        assert_eq!(sanitise_dep_name(""), None);
    }

    #[test]
    fn extract_dep_hints_yields_valid_toml_pairs() {
        let code = r#"
// [DEPS] libc = "0.2", pcre2 = "0.2" (optional), std = "1.0", sha2 = "0.10"
fn main() {}
"#;
        let deps = extract_dep_hints(code);
        // Garbage version is cleaned, std is dropped, valid ones kept.
        assert_eq!(deps.get("libc").map(String::as_str), Some("0.2"));
        assert_eq!(deps.get("pcre2").map(String::as_str), Some("0.2"));
        assert_eq!(deps.get("sha2").map(String::as_str), Some("0.10"));
        assert!(!deps.contains_key("std"));
        // Every produced pair must form a valid TOML dependency line.
        for (k, v) in &deps {
            let line = format!("{k} = \"{v}\"");
            assert!(
                toml_line_parses(&line),
                "produced invalid TOML dependency line: {line}"
            );
        }
    }

    /// Cheap check that `name = "ver"` is a clean single-token TOML pair:
    /// exactly one `=`, value wrapped in one pair of quotes with no inner quote.
    fn toml_line_parses(line: &str) -> bool {
        let Some((_, val)) = line.split_once('=') else {
            return false;
        };
        let val = val.trim();
        val.starts_with('"')
            && val.ends_with('"')
            && val.len() >= 2
            && !val[1..val.len() - 1].contains('"')
    }

    #[test]
    fn parse_dependency_names_reads_only_the_deps_section() {
        let toml = r#"
[package]
name = "demo"
version = "0.1.0"

[dependencies]
serde = { version = "1", features = ["derive"] }
openssl = "0.10"
# a comment
ssh2 = "0.9"

[dev-dependencies]
rstest = "0.18"
"#;
        let names = parse_dependency_names(toml);
        assert_eq!(names, vec!["serde", "openssl", "ssh2"]);
        // dev-dependencies are not pulled in
        assert!(!names.contains(&"rstest".to_string()));
    }

    #[test]
    fn system_dep_hint_flags_native_libs() {
        assert!(system_dep_hint("openssl").is_some());
        assert!(system_dep_hint("gtk").is_some());
        assert!(system_dep_hint("ssh2").is_some());
        // Pure-Rust crate → no system package needed
        assert!(system_dep_hint("serde").is_none());
        assert!(system_dep_hint("regex").is_none());
    }

    #[test]
    fn unresolvable_deps_extracts_offending_crate() {
        let out = CargoOutput {
            stdout_lines: vec![],
            stderr_lines: vec![
                "    Updating crates.io index".into(),
                "error: failed to select a version for the requirement `badger = \"^4.0.0\"`"
                    .into(),
                "error: no matching package named `firebird-sys` found".into(),
                "  location searched: registry `crates-io`".into(),
            ],
            exit_code: Some(101),
        };
        let bad = unresolvable_deps(&out);
        assert!(bad.contains(&"badger".to_string()), "got {bad:?}");
        assert!(bad.contains(&"firebird-sys".to_string()), "got {bad:?}");
        // A normal compile error must NOT be treated as a resolution failure.
        let compile = CargoOutput {
            stdout_lines: vec![],
            stderr_lines: vec!["error[E0425]: cannot find value `x` in this scope".into()],
            exit_code: Some(101),
        };
        assert!(unresolvable_deps(&compile).is_empty());
    }

    #[test]
    fn files_with_errors_never_touches_dependency_sources() {
        use rustyfi_core::state::{CompilerDiagnostic, DiagnosticLevel, DiagnosticSpan};
        let ws = std::env::temp_dir().join(format!("rustyfi_fwe_{}", std::process::id()));
        let src = ws.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("main.rs"), "fn main(){}").unwrap();

        let span = |file: &str| DiagnosticSpan {
            file_name: file.to_string(),
            line_start: 1,
            line_end: 1,
            column_start: 1,
            column_end: 1,
            is_primary: true,
            label: None,
        };
        let diag = |file: &str| CompilerDiagnostic {
            level: DiagnosticLevel::Error,
            message: "boom".into(),
            code: None,
            spans: vec![span(file)],
            rendered: None,
        };
        // One real workspace file, one absolute registry path that must be ignored.
        let diags = vec![
            diag("src/main.rs"),
            diag("/Users/x/.cargo/registry/src/index.crates.io/tracing-0.1.44/src/macros.rs"),
        ];
        let files = files_with_errors(&diags, &ws);
        assert_eq!(
            files.len(),
            1,
            "only the workspace file should be returned: {files:?}"
        );
        assert!(files[0].ends_with("src/main.rs"));
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn strip_deps_comments_out_only_the_named_dep() {
        let dir = std::env::temp_dir().join(format!("rustyfi_strip_test_{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let toml = "[dependencies]\nserde = \"1\"\nbadger = \"4.0.0\"\nserde_json = \"1\"\n";
        fs::write(dir.join("Cargo.toml"), toml).unwrap();

        let changed = strip_deps_from_cargo(&dir, &["badger".to_string()]);
        assert!(changed);
        let after = fs::read_to_string(dir.join("Cargo.toml")).unwrap();
        assert!(after.contains("# [rustyfi] removed unresolved dep: badger = \"4.0.0\""));
        // Sibling deps with the name as a prefix must be untouched.
        assert!(after.contains("\nserde = \"1\""));
        assert!(after.contains("\nserde_json = \"1\""));
        let _ = fs::remove_dir_all(&dir);
    }

    // ── snapshot_src / restore_src ───────────────────────────────────────────

    /// Build a minimal workspace-like directory with a nested src tree.
    /// Does NOT create a Cargo.toml or anything that would require cargo to run.
    fn make_src_tree() -> (tempfile::TempDir, std::path::PathBuf) {
        let tmp = tempfile::TempDir::new().unwrap();
        let ws = tmp.path().to_path_buf();
        let src = ws.join("src");
        fs::create_dir_all(src.join("sub")).unwrap();
        fs::write(src.join("lib.rs"), "pub fn hello() {}\n").unwrap();
        fs::write(src.join("util.rs"), "pub fn helper() {}\n").unwrap();
        fs::write(src.join("sub").join("mod.rs"), "pub fn sub_fn() {}\n").unwrap();
        (tmp, ws)
    }

    #[test]
    fn snapshot_captures_nested_files() {
        let (_tmp, ws) = make_src_tree();
        let snap = snapshot_src(&ws).expect("snapshot_src failed");
        // Snapshot must contain all three files.
        assert!(
            snap.path().join("src/lib.rs").exists(),
            "lib.rs missing from snapshot"
        );
        assert!(
            snap.path().join("src/util.rs").exists(),
            "util.rs missing from snapshot"
        );
        assert!(
            snap.path().join("src/sub/mod.rs").exists(),
            "sub/mod.rs missing from snapshot"
        );
        // Content must match.
        let lib_content = fs::read_to_string(snap.path().join("src/lib.rs")).unwrap();
        assert_eq!(lib_content, "pub fn hello() {}\n");
    }

    #[test]
    fn restore_src_replaces_modified_files() {
        let (_tmp, ws) = make_src_tree();
        // Take snapshot of original state.
        let snap = snapshot_src(&ws).expect("snapshot_src failed");

        // Modify workspace: overwrite lib.rs and add a new file.
        fs::write(ws.join("src/lib.rs"), "// modified\n").unwrap();
        fs::write(ws.join("src/new_file.rs"), "// new\n").unwrap();

        // Restore from snapshot.
        restore_src(&ws, &snap).expect("restore_src failed");

        // lib.rs must be back to original.
        let lib_content = fs::read_to_string(ws.join("src/lib.rs")).unwrap();
        assert_eq!(
            lib_content, "pub fn hello() {}\n",
            "lib.rs not restored correctly"
        );
        // new_file.rs must be gone (the restore wipes src/ first).
        assert!(
            !ws.join("src/new_file.rs").exists(),
            "new_file.rs should have been wiped by restore"
        );
        // Original nested file must still be present.
        assert!(
            ws.join("src/sub/mod.rs").exists(),
            "sub/mod.rs missing after restore"
        );
        let sub_content = fs::read_to_string(ws.join("src/sub/mod.rs")).unwrap();
        assert_eq!(sub_content, "pub fn sub_fn() {}\n");
    }

    #[test]
    fn restore_src_after_delete_all_recovers() {
        let (_tmp, ws) = make_src_tree();
        let snap = snapshot_src(&ws).expect("snapshot_src failed");

        // Simulate a catastrophic delete (what happens if a bad doctor run removes files).
        fs::remove_dir_all(ws.join("src")).unwrap();
        assert!(
            !ws.join("src").exists(),
            "src should be gone before restore"
        );

        restore_src(&ws, &snap).expect("restore_src failed");

        // All files must be restored.
        assert!(
            ws.join("src/lib.rs").exists(),
            "lib.rs missing after full restore"
        );
        assert!(
            ws.join("src/util.rs").exists(),
            "util.rs missing after full restore"
        );
        assert!(
            ws.join("src/sub/mod.rs").exists(),
            "sub/mod.rs missing after full restore"
        );
    }

    #[test]
    fn deep_fix_summary_serializes_to_expected_json() {
        let s = DeepFixSummary {
            ran: true,
            start_errors: 10,
            end_errors: 3,
            tool_calls: 12,
        };
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["ran"], true);
        assert_eq!(v["start_errors"], 10);
        assert_eq!(v["end_errors"], 3);
        assert_eq!(v["tool_calls"], 12);
    }
}
