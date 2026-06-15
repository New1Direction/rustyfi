//! Self-improving translation: a corpus of `cargo check`-verified
//! (source → Rust) pairs, retrieved as few-shot examples at translate time.
//!
//! The flywheel: every translation that passes the oracle is ground truth.
//! Harvest those pairs, retrieve the ones whose source uses a similar API
//! surface, and feed them back into the next translation. More runs → more
//! verified pairs → better first-shot translations.
use serde::{Deserialize, Serialize};

pub mod harvest;
pub mod retrieve;
pub mod signal;
pub mod store;

/// Trust level of a verified pair. `Compile` = valid Rust but behaviour
/// unverified (it can still encode a behavioural bug, e.g. wrong float
/// formatting — proven by the divergence probe). `Behavior` = also passed the
/// behavioural oracle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    Compile,
    Behavior,
}

/// One verified (source → Rust) translation pair.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CorpusEntry {
    pub source_lang: String,
    pub source_api: Vec<String>,
    pub source_code: String,
    pub rust_code: String,
    pub crate_name: String,
    pub file: String,
    pub tier: Tier,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_jsonl_round_trips() {
        let e = CorpusEntry {
            source_lang: "go".into(),
            source_api: vec!["fmt.Printf".into()],
            source_code: "package main".into(),
            rust_code: "fn main() {}".into(),
            crate_name: "calc".into(),
            file: "main.go".into(),
            tier: Tier::Behavior,
        };
        let line = serde_json::to_string(&e).unwrap();
        let back: CorpusEntry = serde_json::from_str(&line).unwrap();
        assert_eq!(back.source_api, e.source_api);
        assert_eq!(back.tier, Tier::Behavior);
        assert!(line.contains("\"tier\":\"behavior\""));
    }
}
