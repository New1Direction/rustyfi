//! Normalization transforms and the per-case diff engine.

use super::{CompareSpec, Expect, Normalize, Outcome, StreamMode};

/// Apply normalization rules in order. `StripTrailingWs` trims each line's
/// trailing whitespace; `Mask` replaces regex matches with a token. A bad
/// regex is skipped (treated as a no-op) rather than panicking.
pub fn normalize_text(text: &str, rules: &[Normalize]) -> String {
    let mut out = text.to_string();
    for rule in rules {
        match rule {
            Normalize::StripTrailingWs => {
                let had_trailing_newline = out.ends_with('\n');
                let mut joined = out
                    .lines()
                    .map(|l| l.trim_end())
                    .collect::<Vec<_>>()
                    .join("\n");
                if had_trailing_newline {
                    joined.push('\n');
                }
                out = joined;
            }
            Normalize::Mask { pattern, token } => {
                if let Ok(re) = regex::Regex::new(pattern) {
                    out = re.replace_all(&out, token.as_str()).into_owned();
                }
            }
        }
    }
    out
}

/// Compare one captured `Outcome` against golden `Expect` under a compare
/// policy. Returns human-readable mismatch lines (empty == behaviors match).
pub fn diff_case(
    expect: &Expect,
    actual: &Outcome,
    compare: &CompareSpec,
    rules: &[Normalize],
) -> Vec<String> {
    let mut diffs = Vec::new();

    for (name, mode, want, got) in [
        ("stdout", compare.stdout, &expect.stdout, &actual.stdout),
        ("stderr", compare.stderr, &expect.stderr, &actual.stderr),
    ] {
        let (w, g) = match mode {
            StreamMode::Ignore => continue,
            StreamMode::Exact => (want.clone(), got.clone()),
            StreamMode::Normalized => (normalize_text(want, rules), normalize_text(got, rules)),
        };
        if w != g {
            diffs.push(format!("{name}: source={w:?} target={g:?}"));
        }
    }

    if compare.exit_code != StreamMode::Ignore && expect.exit_code != actual.exit_code {
        diffs.push(format!(
            "exit_code: source={} target={}",
            expect.exit_code, actual.exit_code
        ));
    }
    diffs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_trailing_ws_per_line() {
        let rules = vec![Normalize::StripTrailingWs];
        assert_eq!(normalize_text("a  \nb\t\n", &rules), "a\nb\n");
    }

    #[test]
    fn mask_replaces_regex_matches() {
        let rules = vec![Normalize::Mask {
            pattern: r"\d{4}-\d{2}-\d{2}".into(),
            token: "<DATE>".into(),
        }];
        assert_eq!(normalize_text("on 2026-06-13 ok", &rules), "on <DATE> ok");
    }

    #[test]
    fn matching_case_yields_no_diffs() {
        let expect = Expect {
            stdout: "11\n".into(),
            stderr: String::new(),
            exit_code: 0,
        };
        let actual = Outcome {
            stdout: "11\n".into(),
            stderr: String::new(),
            exit_code: 0,
        };
        let diffs = diff_case(&expect, &actual, &CompareSpec::default(), &[]);
        assert!(diffs.is_empty());
    }

    #[test]
    fn divergent_stdout_and_exit_are_reported() {
        let expect = Expect {
            stdout: "11\n".into(),
            stderr: String::new(),
            exit_code: 0,
        };
        let actual = Outcome {
            stdout: "11.0000000000\n".into(),
            stderr: String::new(),
            exit_code: 1,
        };
        let diffs = diff_case(&expect, &actual, &CompareSpec::default(), &[]);
        assert_eq!(diffs.len(), 2);
        assert!(diffs[0].starts_with("stdout:"));
        assert!(diffs[1].starts_with("exit_code:"));
    }

    #[test]
    fn ignore_mode_skips_a_stream() {
        let expect = Expect {
            stdout: "x".into(),
            stderr: "noise-a".into(),
            exit_code: 0,
        };
        let actual = Outcome {
            stdout: "x".into(),
            stderr: "noise-b".into(),
            exit_code: 0,
        };
        let compare = CompareSpec {
            stderr: StreamMode::Ignore,
            ..CompareSpec::default()
        };
        assert!(diff_case(&expect, &actual, &compare, &[]).is_empty());
    }

    #[test]
    fn normalized_mode_applies_rules_both_sides() {
        let expect = Expect {
            stdout: "built 2026-06-13\n".into(),
            stderr: String::new(),
            exit_code: 0,
        };
        let actual = Outcome {
            stdout: "built 2026-01-01 \n".into(),
            stderr: String::new(),
            exit_code: 0,
        };
        let compare = CompareSpec {
            stdout: StreamMode::Normalized,
            ..CompareSpec::default()
        };
        let rules = vec![
            Normalize::Mask {
                pattern: r"\d{4}-\d{2}-\d{2}".into(),
                token: "<D>".into(),
            },
            Normalize::StripTrailingWs,
        ];
        assert!(diff_case(&expect, &actual, &compare, &rules).is_empty());
    }
}
