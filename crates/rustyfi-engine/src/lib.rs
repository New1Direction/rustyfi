pub mod analysis;
pub mod checkpoint;
pub mod chunker;
pub mod contract_check;
pub mod dedup_items;
pub mod deps;
pub mod graph;
pub mod llm;
pub mod pipeline;
pub mod rustfix;
pub mod scaffold;
pub mod slicer;

use thiserror::Error;

/// Top-level error for the engine crate.
#[derive(Debug, Error)]
pub enum EngineError {
    #[error("no translatable source files found in {path}")]
    NoSourceFiles { path: std::path::PathBuf },

    #[error("configuration error: {0}")]
    Config(String),

    #[error("LLM error: {0}")]
    Llm(String),

    #[error("I/O error: {0}")]
    Io(String),

    #[error("orchestrator error: {0}")]
    Orchestrator(String),

    #[error("compiler error: {0}")]
    Compiler(String),

    #[error("packaging error: {0}")]
    Packaging(String),
}
