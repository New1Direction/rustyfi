//! JSONL persistence for the corpus: a shipped seed plus a local growth cache.
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::corpus::CorpusEntry;

/// Parse JSONL text into corpus entries. Blank/unparseable lines are skipped
/// (fail-open). Used for both on-disk reads and the binary-embedded seed.
pub fn parse_jsonl(text: &str) -> Vec<CorpusEntry> {
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<CorpusEntry>(l).ok())
        .collect()
}

/// Read a JSONL corpus. A missing file or any unparseable line is skipped —
/// the corpus is an enhancement, never a hard dependency (fail-open).
pub fn read_jsonl(path: &Path) -> Vec<CorpusEntry> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    parse_jsonl(&text)
}

/// Append entries as JSONL, creating the file and parent dirs as needed.
pub fn append_jsonl(path: &Path, entries: &[CorpusEntry]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    for e in entries {
        let line = serde_json::to_string(e).map_err(std::io::Error::other)?;
        writeln!(f, "{line}")?;
    }
    Ok(())
}

/// Local growth cache: `$XDG_CACHE_HOME/rustyfi/corpus.jsonl`, else
/// `~/.cache/rustyfi/corpus.jsonl`. `None` if neither base is resolvable.
pub fn local_cache_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
    Some(base.join("rustyfi").join("corpus.jsonl"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::corpus::{CorpusEntry, Tier};

    fn e(name: &str) -> CorpusEntry {
        CorpusEntry {
            source_lang: "go".into(),
            source_api: vec![],
            source_code: "s".into(),
            rust_code: "r".into(),
            crate_name: name.into(),
            file: "f".into(),
            tier: Tier::Compile,
        }
    }

    #[test]
    fn append_then_read_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("c.jsonl");
        append_jsonl(&p, &[e("a"), e("b")]).unwrap();
        append_jsonl(&p, &[e("c")]).unwrap();
        let all = read_jsonl(&p);
        assert_eq!(all.len(), 3);
        assert_eq!(all[2].crate_name, "c");
    }

    #[test]
    fn missing_file_reads_empty() {
        assert!(read_jsonl(Path::new("/no/such/corpus.jsonl")).is_empty());
    }
}
