//! Mine candidate CLI invocations from a project's README and `--help`.
//!
//! Recall is best-effort: the hybrid review loop (the user extends
//! `behavior.yaml`) is the mitigation, not a guarantee. Golden values are
//! filled later by `capture_all`.

use super::{Case, Provenance};

/// Extract candidate invocations of `binary` from fenced code blocks in README
/// markdown. A line is a candidate if, after stripping an optional `$ ` prompt,
/// its first shell token equals `binary`. Args are shell-split; duplicates are
/// dropped. Golden output is NOT captured here (`capture_all` does that).
pub fn mine_readme(markdown: &str, binary: &str) -> Vec<Case> {
    let mut cases = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    let mut in_block = false;

    for line in markdown.lines() {
        if line.trim_start().starts_with("```") {
            in_block = !in_block;
            continue;
        }
        if !in_block {
            continue;
        }
        let cmd = line.trim().strip_prefix("$ ").unwrap_or(line.trim());
        let tokens = match shlex::split(cmd) {
            Some(t) if !t.is_empty() => t,
            _ => continue,
        };
        if tokens[0] != binary {
            continue;
        }
        let args: Vec<String> = tokens[1..].to_vec();
        if !seen.insert(args.clone()) {
            continue;
        }
        cases.push(Case {
            name: format!("readme_{}", cases.len() + 1),
            provenance: Provenance::Readme,
            args,
            stdin: None,
            env: Default::default(),
            expect: None,
            nondeterministic: false,
            compare: None,
        });
    }
    cases
}

/// A `--help` behavioral case (the help text itself must match). Golden output
/// is captured later from the source.
pub fn help_case() -> Case {
    Case {
        name: "help".to_string(),
        provenance: Provenance::Help,
        args: vec!["--help".to_string()],
        stdin: None,
        env: Default::default(),
        expect: None,
        nondeterministic: false,
        compare: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const README: &str = r#"
# calc

Run it:

```
$ calc "2 + 3 * (4 - 1)"
11
$ calc "2 ^ 10"
1024
```

Some prose, then a non-matching block:

```
echo unrelated
```
"#;

    #[test]
    fn mines_binary_invocations_from_fenced_blocks() {
        let cases = mine_readme(README, "calc");
        let names: Vec<&str> = cases.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(cases.len(), 2, "two `calc ...` lines, echo line ignored");
        assert_eq!(cases[0].args, vec!["2 + 3 * (4 - 1)".to_string()]);
        assert_eq!(cases[1].args, vec!["2 ^ 10".to_string()]);
        assert_eq!(cases[0].provenance, Provenance::Readme);
        assert!(names[0].starts_with("readme_"));
    }

    #[test]
    fn dedupes_identical_invocations() {
        let md = "```\n$ calc x\n$ calc x\n```\n";
        assert_eq!(mine_readme(md, "calc").len(), 1);
    }

    #[test]
    fn help_case_is_a_help_invocation() {
        let c = help_case();
        assert_eq!(c.args, vec!["--help".to_string()]);
        assert_eq!(c.provenance, Provenance::Help);
    }
}
