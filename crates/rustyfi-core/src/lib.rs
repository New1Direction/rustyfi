//! # Rustyfi
//!
//! Deterministic Rust control-plane that orchestrates the conversion of
//! non-Rust codebases into optimized compiled Rust applications.
//!
//! ## Architecture
//!
//! ```text
//! ContextManifest  ──►  Orchestrator::transition()
//!                              │
//!                    ┌─────────▼──────────┐
//!                    │   RustyfiState     │
//!                    │  Idle              │
//!                    │  Parsing           │
//!                    │  Scaffolding       │
//!                    │  Translating       │
//!                    │  Verifying   ◄─────┼── retry path
//!                    │  Optimizing        │
//!                    │  Completed         │
//!                    │  Failed            │
//!                    └────────────────────┘
//!                              │
//!                    compiler::run_cargo_check()
//! ```
//!
//! ## Modules
//!
//! | Module | Responsibility |
//! |--------|---------------|
//! | [`state`] | [`RustyfiState`] enum and all per-state context structs |
//! | [`events`] | [`StateEvent`] enum — the only way to drive transitions |
//! | [`transitions`] | [`Orchestrator`] — enforces the transition table |
//! | [`context`] | [`ContextManifest`] ingestion contract |
//! | [`compiler`] | `cargo check` harness and diagnostic parsing |
//! | [`errors`] | All typed error variants |

pub mod compiler;
pub mod context;
pub mod errors;
pub mod events;
pub mod state;
pub mod transitions;

// ---------------------------------------------------------------------------
// Convenience re-exports
// ---------------------------------------------------------------------------

pub use context::ContextManifest;
pub use errors::{CompilerError, ManifestError, TransitionError};
pub use events::StateEvent;
pub use state::{DiagnosticFamily, RustyfiState};
pub use transitions::Orchestrator;
