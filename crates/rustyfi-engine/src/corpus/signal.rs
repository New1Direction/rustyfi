//! The retrieval signal: a source file's external API surface.
use std::collections::BTreeSet;

/// The external API surface of a source file: qualified accesses like
/// `fmt.Printf` / `axios.get` / `os::path` (`::` normalised to `.`). This is
/// the emergent ontology used as the retrieval key — what a translation must
/// get right, learned from data rather than a hand-authored table.
///
/// Heuristic and language-agnostic: a head is a lowercase-initial identifier
/// (packages/modules, not types) followed by `.` or `::` and a member. Starting
/// only at a token boundary avoids matching mid-identifier.
pub fn api_surface(source: &str) -> BTreeSet<String> {
    let b = source.as_bytes();
    let mut out = BTreeSet::new();
    let is_id = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_lowercase() && (i == 0 || !is_id(b[i - 1])) {
            let hs = i;
            while i < b.len() && is_id(b[i]) {
                i += 1;
            }
            let head = &source[hs..i];
            let mut j = i;
            let sep = if j < b.len() && b[j] == b'.' {
                j += 1;
                true
            } else if j + 1 < b.len() && b[j] == b':' && b[j + 1] == b':' {
                j += 2;
                true
            } else {
                false
            };
            if sep && j < b.len() && (b[j].is_ascii_alphabetic() || b[j] == b'_') {
                let ms = j;
                while j < b.len() && is_id(b[j]) {
                    j += 1;
                }
                out.insert(format!("{head}.{}", &source[ms..j]));
                i = j;
            }
            continue;
        }
        i += 1;
    }
    out
}

/// Jaccard similarity of two API-surface sets (0.0..=1.0). Empty/empty is 0.0.
pub fn jaccard(a: &BTreeSet<String>, b: &BTreeSet<String>) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 0.0;
    }
    let inter = a.intersection(b).count();
    let union = a.len() + b.len() - inter;
    inter as f64 / union as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_dotted_and_scoped_api() {
        let s = "fmt.Printf(\"%g\", x); v := strconv.ParseFloat(t); a::b()";
        let api = api_surface(s);
        assert!(api.contains("fmt.Printf"));
        assert!(api.contains("strconv.ParseFloat"));
        assert!(api.contains("a.b"));
    }

    #[test]
    fn does_not_start_mid_identifier() {
        // `ParseFloat` alone (no leading package) must not yield a pair from
        // its internal lowercase runs.
        let api = api_surface("ParseFloat(x)");
        assert!(api.is_empty());
    }

    #[test]
    fn jaccard_basic() {
        let a: BTreeSet<String> = ["x".into(), "y".into()].into();
        let b: BTreeSet<String> = ["y".into(), "z".into()].into();
        assert!((jaccard(&a, &b) - 1.0 / 3.0).abs() < 1e-9);
        assert_eq!(jaccard(&a, &a), 1.0);
    }
}
