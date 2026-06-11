use crate::errors::{state_name, TransitionError};
use crate::events::StateEvent;
use crate::state::{
    CompletedContext, FailedContext, OptimizingContext, ParsingContext, RustyfiState,
    ScaffoldingContext, TranslatingContext, VerifyingContext,
};

/// The Rustyfi deterministic orchestrator.
///
/// Wraps a [`RustyfiState`] and enforces that all mutations go through
/// [`Self::transition`].  The current state is never mutated in place;
/// transitions always replace the entire state value, which makes the
/// transition table the single source of truth for what is structurally
/// possible.
#[derive(Debug)]
pub struct Orchestrator {
    state: RustyfiState,
}

impl Orchestrator {
    /// Creates a new orchestrator in [`RustyfiState::Idle`].
    pub fn new() -> Self {
        Self {
            state: RustyfiState::Idle,
        }
    }

    /// Returns a shared reference to the current state.
    pub fn state(&self) -> &RustyfiState {
        &self.state
    }

    /// Drives a state transition.
    ///
    /// # Errors
    ///
    /// Returns [`TransitionError`] when:
    /// * the event is not valid from the current state,
    /// * the machine is in `Failed` and `Reset` was not supplied,
    /// * or an event payload violates a semantic constraint (retry ceiling).
    pub fn transition(&mut self, event: StateEvent) -> Result<(), TransitionError> {
        // ----------------------------------------------------------------
        // Guard: Failed may only accept Reset.
        // ----------------------------------------------------------------
        if let RustyfiState::Failed(ref ctx) = self.state {
            match event {
                StateEvent::Reset => {
                    self.state = RustyfiState::Idle;
                    return Ok(());
                }
                _ => {
                    return Err(TransitionError::MustResetBeforeContinuing {
                        origin_state: ctx.originating_state.clone(),
                        recoverable: ctx.recoverable,
                    });
                }
            }
        }

        // ----------------------------------------------------------------
        // The Fail event is universally accepted from any non-Failed state.
        // ----------------------------------------------------------------
        if let StateEvent::Fail {
            reason,
            recoverable,
        } = event
        {
            let originating_state = state_name(&self.state).to_owned();
            self.state = RustyfiState::Failed(FailedContext {
                reason,
                originating_state,
                recoverable,
            });
            return Ok(());
        }

        // ----------------------------------------------------------------
        // Explicit transition table — no catch-all branches.
        // ----------------------------------------------------------------
        let next = match (&self.state, event) {
            // Idle --------------------------------------------------------
            (RustyfiState::Idle, StateEvent::StartParsing { manifest }) => {
                let ctx = ParsingContext {
                    source_targets: manifest.source_targets,
                    language_metadata: manifest.language_metadata,
                    parser_metadata: std::collections::HashMap::new(),
                };
                RustyfiState::Parsing(ctx)
            }

            (RustyfiState::Idle, event) => {
                return Err(TransitionError::IllegalTransition {
                    from: "Idle",
                    event: event.name(),
                });
            }

            // Parsing -----------------------------------------------------
            (
                RustyfiState::Parsing(_),
                StateEvent::ParseComplete {
                    workspace_path,
                    dependency_manifest,
                    module_layout_plan,
                },
            ) => RustyfiState::Scaffolding(ScaffoldingContext {
                workspace_path,
                dependency_manifest,
                module_layout_plan,
            }),

            (RustyfiState::Parsing(_), event) => {
                return Err(TransitionError::IllegalTransition {
                    from: "Parsing",
                    event: event.name(),
                });
            }

            // Scaffolding -------------------------------------------------
            (
                RustyfiState::Scaffolding(_),
                StateEvent::ScaffoldComplete {
                    first_file,
                    total_chunks,
                    retry_ceiling,
                },
            ) => RustyfiState::Translating(TranslatingContext {
                current_source_file: first_file,
                chunk_index: 0,
                total_chunks,
                generation_attempt: 0,
                retry_ceiling,
            }),

            (RustyfiState::Scaffolding(_), event) => {
                return Err(TransitionError::IllegalTransition {
                    from: "Scaffolding",
                    event: event.name(),
                });
            }

            // Translating -------------------------------------------------
            (
                RustyfiState::Translating(ref ctx),
                StateEvent::ChunkAccepted { next_chunk_index },
            ) => {
                let next_index = next_chunk_index;
                if next_index >= ctx.total_chunks {
                    return Err(TransitionError::InvalidPayload {
                        event: "ChunkAccepted",
                        reason:
                            "next_chunk_index >= total_chunks; emit TranslationComplete instead",
                    });
                }
                let updated = TranslatingContext {
                    chunk_index: next_index,
                    generation_attempt: 0,
                    ..ctx.clone()
                };
                RustyfiState::Translating(updated)
            }

            (RustyfiState::Translating(ref ctx), StateEvent::ChunkRetry { .. }) => {
                let next_attempt = ctx.generation_attempt + 1;
                if next_attempt > ctx.retry_ceiling {
                    return Err(TransitionError::RetryCeilingExceeded {
                        attempt: next_attempt,
                        ceiling: ctx.retry_ceiling,
                    });
                }
                let updated = TranslatingContext {
                    generation_attempt: next_attempt,
                    ..ctx.clone()
                };
                RustyfiState::Translating(updated)
            }

            (
                RustyfiState::Translating(_),
                StateEvent::TranslationComplete {
                    cargo_output,
                    retry_ceiling,
                },
            ) => RustyfiState::Verifying(VerifyingContext {
                cargo_output,
                diagnostics: Vec::new(),
                verification_attempt: 0,
                retry_ceiling,
            }),

            (RustyfiState::Translating(_), event) => {
                return Err(TransitionError::IllegalTransition {
                    from: "Translating",
                    event: event.name(),
                });
            }

            // Verifying ---------------------------------------------------
            (RustyfiState::Verifying(_), StateEvent::VerifyPassed { release_config }) => {
                RustyfiState::Optimizing(OptimizingContext {
                    release_config,
                    produced_artifacts: Vec::new(),
                    completed_passes: Vec::new(),
                })
            }

            (
                RustyfiState::Verifying(ref ctx),
                StateEvent::VerifyRetry {
                    target_file,
                    chunk_index,
                    total_chunks,
                    retry_ceiling,
                },
            ) => {
                let next_attempt = ctx.verification_attempt + 1;
                if next_attempt > ctx.retry_ceiling {
                    return Err(TransitionError::RetryCeilingExceeded {
                        attempt: next_attempt,
                        ceiling: ctx.retry_ceiling,
                    });
                }
                // Verifying → Translating (retry path)
                RustyfiState::Translating(TranslatingContext {
                    current_source_file: target_file,
                    chunk_index,
                    total_chunks,
                    generation_attempt: 0,
                    retry_ceiling,
                })
            }

            (RustyfiState::Verifying(_), event) => {
                return Err(TransitionError::IllegalTransition {
                    from: "Verifying",
                    event: event.name(),
                });
            }

            // Optimizing --------------------------------------------------
            (
                RustyfiState::Optimizing(_),
                StateEvent::OptimizationComplete {
                    artifact_locations,
                    build_metadata,
                },
            ) => RustyfiState::Completed(CompletedContext {
                artifact_locations,
                build_metadata,
            }),

            (RustyfiState::Optimizing(_), event) => {
                return Err(TransitionError::IllegalTransition {
                    from: "Optimizing",
                    event: event.name(),
                });
            }

            // Completed ---------------------------------------------------
            // The only universally accepted event (Fail) is handled above.
            // Completed is a terminal state; no further transitions are valid.
            (RustyfiState::Completed(_), event) => {
                return Err(TransitionError::IllegalTransition {
                    from: "Completed",
                    event: event.name(),
                });
            }

            // Failed ------------------------------------------------------
            // Handled by the guard at the top of this function; this arm is
            // structurally unreachable but required for exhaustiveness.
            (RustyfiState::Failed(_), _) => {
                return Err(TransitionError::InternalInvariant {
                    detail: "Failed guard at top of transition() should have caught this arm",
                });
            }
        };

        self.state = next;
        Ok(())
    }
}

impl Default for Orchestrator {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{ContextManifest, LanguageMetadata, SourceLanguage};
    use crate::events::StateEvent;
    use crate::state::{CargoOutput, FailureReason, LtoMode, ReleaseConfig};
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn minimal_manifest() -> ContextManifest {
        ContextManifest {
            run_id: "test-run-001".into(),
            workspace_root: PathBuf::from("/workspace"),
            source_targets: vec![],
            dependency_edges: vec![],
            external_packages: vec![],
            filesystem_boundaries: vec![],
            external_io_boundaries: vec![],
            inferred_entrypoints: vec![],
            parser_warnings: vec![],
            language_metadata: LanguageMetadata {
                primary_language: SourceLanguage::Python,
                runtime_version: Some("3.11.4".into()),
                package_manager: Some("pip 23.1".into()),
                is_dynamically_typed: true,
                extra: HashMap::new(),
            },
            produced_at: "2024-01-01T00:00:00Z".into(),
        }
    }

    fn cargo_output() -> CargoOutput {
        CargoOutput {
            stdout_lines: vec![],
            stderr_lines: vec![],
            exit_code: Some(0),
        }
    }

    fn release_config() -> ReleaseConfig {
        ReleaseConfig {
            opt_level: "3".into(),
            lto: LtoMode::Thin,
            codegen_units: 1,
            strip_debug: true,
        }
    }

    /// Drive a machine through the full happy-path sequence.
    #[test]
    fn happy_path_completes() {
        let mut orch = Orchestrator::new();

        orch.transition(StateEvent::StartParsing {
            manifest: Box::new(minimal_manifest()),
        })
        .unwrap();
        assert!(matches!(orch.state(), RustyfiState::Parsing(_)));

        orch.transition(StateEvent::ParseComplete {
            workspace_path: PathBuf::from("/out/workspace"),
            dependency_manifest: HashMap::new(),
            module_layout_plan: vec!["main".into()],
        })
        .unwrap();
        assert!(matches!(orch.state(), RustyfiState::Scaffolding(_)));

        orch.transition(StateEvent::ScaffoldComplete {
            first_file: PathBuf::from("/src/main.py"),
            total_chunks: 3,
            retry_ceiling: 2,
        })
        .unwrap();
        assert!(matches!(orch.state(), RustyfiState::Translating(_)));

        // Advance two chunks.
        orch.transition(StateEvent::ChunkAccepted {
            next_chunk_index: 1,
        })
        .unwrap();
        orch.transition(StateEvent::ChunkAccepted {
            next_chunk_index: 2,
        })
        .unwrap();

        orch.transition(StateEvent::TranslationComplete {
            cargo_output: cargo_output(),
            retry_ceiling: 2,
        })
        .unwrap();
        assert!(matches!(orch.state(), RustyfiState::Verifying(_)));

        orch.transition(StateEvent::VerifyPassed {
            release_config: release_config(),
        })
        .unwrap();
        assert!(matches!(orch.state(), RustyfiState::Optimizing(_)));

        orch.transition(StateEvent::OptimizationComplete {
            artifact_locations: vec![PathBuf::from("/out/bin/app")],
            build_metadata: HashMap::new(),
        })
        .unwrap();
        assert!(matches!(orch.state(), RustyfiState::Completed(_)));
    }

    /// An illegal transition from `Idle` must be rejected.
    #[test]
    fn idle_rejects_parse_complete() {
        let mut orch = Orchestrator::new();
        let result = orch.transition(StateEvent::ParseComplete {
            workspace_path: PathBuf::from("/out"),
            dependency_manifest: HashMap::new(),
            module_layout_plan: vec![],
        });
        assert!(matches!(
            result,
            Err(TransitionError::IllegalTransition { .. })
        ));
    }

    /// A retry that exceeds the ceiling must return `RetryCeilingExceeded`.
    #[test]
    fn chunk_retry_ceiling() {
        let mut orch = Orchestrator::new();
        orch.transition(StateEvent::StartParsing {
            manifest: Box::new(minimal_manifest()),
        })
        .unwrap();
        orch.transition(StateEvent::ParseComplete {
            workspace_path: PathBuf::from("/out"),
            dependency_manifest: HashMap::new(),
            module_layout_plan: vec![],
        })
        .unwrap();
        orch.transition(StateEvent::ScaffoldComplete {
            first_file: PathBuf::from("/src/main.py"),
            total_chunks: 1,
            retry_ceiling: 1,
        })
        .unwrap();

        // First retry: accepted (attempt 1 <= ceiling 1).
        orch.transition(StateEvent::ChunkRetry {
            reason: "bad output".into(),
        })
        .unwrap();

        // Second retry: attempt 2 > ceiling 1 → error.
        let result = orch.transition(StateEvent::ChunkRetry {
            reason: "bad output again".into(),
        });
        assert!(matches!(
            result,
            Err(TransitionError::RetryCeilingExceeded { .. })
        ));
    }

    /// `Failed` → any non-Reset event must return `MustResetBeforeContinuing`.
    #[test]
    fn failed_requires_reset() {
        let mut orch = Orchestrator::new();
        orch.transition(StateEvent::StartParsing {
            manifest: Box::new(minimal_manifest()),
        })
        .unwrap();
        orch.transition(StateEvent::Fail {
            reason: FailureReason::InternalInvariant {
                detail: "test failure".into(),
            },
            recoverable: false,
        })
        .unwrap();
        assert!(matches!(orch.state(), RustyfiState::Failed(_)));

        let result = orch.transition(StateEvent::StartParsing {
            manifest: Box::new(minimal_manifest()),
        });
        assert!(matches!(
            result,
            Err(TransitionError::MustResetBeforeContinuing { .. })
        ));

        // Reset must succeed and restore Idle.
        orch.transition(StateEvent::Reset).unwrap();
        assert!(matches!(orch.state(), RustyfiState::Idle));
    }

    /// `Parsing` must reject events it does not accept (no silent catch-all).
    #[test]
    fn parsing_rejects_scaffold_complete() {
        let mut orch = Orchestrator::new();
        orch.transition(StateEvent::StartParsing {
            manifest: Box::new(minimal_manifest()),
        })
        .unwrap();
        let result = orch.transition(StateEvent::ScaffoldComplete {
            first_file: PathBuf::from("/src/main.py"),
            total_chunks: 1,
            retry_ceiling: 1,
        });
        assert!(matches!(
            result,
            Err(TransitionError::IllegalTransition { .. })
        ));
    }

    /// Verifying → Translating (retry path) must succeed within ceiling.
    #[test]
    fn verify_retry_transitions_to_translating() {
        let mut orch = Orchestrator::new();
        orch.transition(StateEvent::StartParsing {
            manifest: Box::new(minimal_manifest()),
        })
        .unwrap();
        orch.transition(StateEvent::ParseComplete {
            workspace_path: PathBuf::from("/out"),
            dependency_manifest: HashMap::new(),
            module_layout_plan: vec![],
        })
        .unwrap();
        orch.transition(StateEvent::ScaffoldComplete {
            first_file: PathBuf::from("/src/main.py"),
            total_chunks: 1,
            retry_ceiling: 2,
        })
        .unwrap();
        orch.transition(StateEvent::TranslationComplete {
            cargo_output: cargo_output(),
            retry_ceiling: 2,
        })
        .unwrap();
        assert!(matches!(orch.state(), RustyfiState::Verifying(_)));

        orch.transition(StateEvent::VerifyRetry {
            target_file: PathBuf::from("/src/main.py"),
            chunk_index: 0,
            total_chunks: 1,
            retry_ceiling: 2,
        })
        .unwrap();
        assert!(matches!(orch.state(), RustyfiState::Translating(_)));
    }
}
