//! Rank verified pairs against a query file's API surface.
use std::cmp::Ordering;
use std::collections::BTreeSet;

use crate::corpus::signal::jaccard;
use crate::corpus::{CorpusEntry, Tier};

struct Ranked {
    entry: CorpusEntry,
    api: BTreeSet<String>,
    is_local: bool,
}

/// Ranks verified pairs against a query file's API surface.
pub struct Retriever {
    items: Vec<Ranked>,
}

impl Retriever {
    /// Build from a shipped seed and a local-growth set. Local entries are
    /// preferred over seed entries on ties.
    pub fn from_sources(seed: Vec<CorpusEntry>, local: Vec<CorpusEntry>) -> Self {
        let mut items = Vec::new();
        for (entries, is_local) in [(seed, false), (local, true)] {
            for e in entries {
                let api = e.source_api.iter().cloned().collect();
                items.push(Ranked {
                    entry: e,
                    api,
                    is_local,
                });
            }
        }
        Self { items }
    }

    /// Top-K pairs for `query` in `lang`, best first. Drops wrong-language and
    /// zero-overlap entries. Order: Jaccard desc, behavior > compile,
    /// local > seed, shorter source first.
    pub fn top_k(&self, query: &BTreeSet<String>, lang: &str, k: usize) -> Vec<&CorpusEntry> {
        let mut scored: Vec<(f64, &Ranked)> = self
            .items
            .iter()
            .filter(|r| r.entry.source_lang == lang)
            .map(|r| (jaccard(query, &r.api), r))
            .filter(|(s, _)| *s > 0.0)
            .collect();
        scored.sort_by(|(sa, a), (sb, b)| {
            sb.partial_cmp(sa)
                .unwrap_or(Ordering::Equal)
                .then(tier_rank(b.entry.tier).cmp(&tier_rank(a.entry.tier)))
                .then(b.is_local.cmp(&a.is_local))
                .then(a.entry.source_code.len().cmp(&b.entry.source_code.len()))
        });
        scored.into_iter().take(k).map(|(_, r)| &r.entry).collect()
    }
}

fn tier_rank(t: Tier) -> u8 {
    match t {
        Tier::Behavior => 1,
        Tier::Compile => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::corpus::{CorpusEntry, Tier};

    fn mk(name: &str, api: &[&str], tier: Tier) -> CorpusEntry {
        CorpusEntry {
            source_lang: "go".into(),
            source_api: api.iter().map(|s| s.to_string()).collect(),
            source_code: "s".into(),
            rust_code: "r".into(),
            crate_name: name.into(),
            file: "f".into(),
            tier,
        }
    }

    #[test]
    fn ranks_by_overlap_then_tier() {
        let seed = vec![
            mk("low", &["a.x"], Tier::Behavior),
            mk("high_compile", &["a.x", "b.y"], Tier::Compile),
        ];
        let local = vec![mk("high_behavior", &["a.x", "b.y"], Tier::Behavior)];
        let r = Retriever::from_sources(seed, local);
        let q: BTreeSet<String> = ["a.x".into(), "b.y".into()].into();
        let top = r.top_k(&q, "go", 3);
        assert_eq!(top[0].crate_name, "high_behavior");
        assert_eq!(top[1].crate_name, "high_compile");
        assert_eq!(top[2].crate_name, "low");
    }

    #[test]
    fn filters_language_and_zero_overlap() {
        let r = Retriever::from_sources(vec![mk("x", &["z.z"], Tier::Compile)], vec![]);
        let q: BTreeSet<String> = ["a.x".into()].into();
        assert!(r.top_k(&q, "go", 3).is_empty());
        assert!(r.top_k(&q, "python", 3).is_empty());
    }
}
