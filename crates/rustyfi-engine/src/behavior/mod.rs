//! Behavioral-equivalence harness: run a source project and its Rust
//! translation against the same inputs and diff observable behavior.
//!
//! The source binary is ground truth. This is differential testing against a
//! fixture corpus — it verifies sameness on the cases exercised, not
//! equivalence in general.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::BTreeMap;
use std::path::Path;

mod harness;
// Process/diff primitives are crate-internal — consumers go through the two
// real entry points (`capture_all`, `verify`) and the miners. `expand` and
// `normalize_text` are used only within their own submodules, so they are not
// re-exported here.
pub(crate) use harness::{build_side, run_case};

mod diff;
pub(crate) use diff::diff_case;

mod mine;
pub use mine::{help_case, mine_readme};

mod recipe;
#[allow(unused_imports)] // used by phase_behavior wired in a later task
pub(crate) use recipe::{source_side, target_side};

/// A full behavioral-equivalence spec (`behavior.yaml`). Self-contained after
/// golden capture: `cases[].expect` holds the source's captured output.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BehaviorSpec {
    pub name: String,
    pub source: Side,
    pub target: Side,
    #[serde(default)]
    pub compare: CompareSpec,
    #[serde(default)]
    pub normalize: Vec<Normalize>,
    #[serde(default)]
    pub cases: Vec<Case>,
}

/// One side (source or target): how to build it and how to invoke its binary.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Side {
    pub lang: String,
    pub dir: String,
    pub build: Vec<String>,
    pub run: Vec<String>,
}

/// Per-stream comparison policy. Defaults to exact on every stream.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct CompareSpec {
    #[serde(default)]
    pub stdout: StreamMode,
    #[serde(default)]
    pub stderr: StreamMode,
    #[serde(default)]
    pub exit_code: StreamMode,
}

impl Default for CompareSpec {
    fn default() -> Self {
        Self {
            stdout: StreamMode::Exact,
            stderr: StreamMode::Exact,
            exit_code: StreamMode::Exact,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
pub enum StreamMode {
    #[default]
    Exact,
    Ignore,
    Normalized,
}

/// A normalization transform applied to BOTH sides before an exact compare.
///
/// YAML representations:
/// - `strip_trailing_ws`  (plain string → unit variant)
/// - `mask: { pattern: '...', token: '...' }` (map → struct variant)
#[derive(Debug, Clone, PartialEq)]
pub enum Normalize {
    /// Strip trailing whitespace from every line.
    StripTrailingWs,
    /// Replace every regex match with a fixed token.
    Mask { pattern: String, token: String },
}

impl Serialize for Normalize {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        match self {
            Normalize::StripTrailingWs => s.serialize_str("strip_trailing_ws"),
            Normalize::Mask { pattern, token } => {
                // Must emit {mask: {pattern, token}} — NOT the YAML-tag form
                // that #[derive(Serialize)] produces, so it round-trips with
                // the custom Deserialize impl.
                let mut body = std::collections::BTreeMap::new();
                body.insert("pattern", pattern.as_str());
                body.insert("token", token.as_str());
                let mut map = s.serialize_map(Some(1))?;
                map.serialize_entry("mask", &body)?;
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for Normalize {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::{self, MapAccess, Visitor};
        use std::fmt;

        struct NormalizeVisitor;

        impl<'de> Visitor<'de> for NormalizeVisitor {
            type Value = Normalize;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("`strip_trailing_ws` or `mask: {pattern, token}`")
            }

            // Unit variant encoded as a plain YAML string
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Normalize, E> {
                match v {
                    "strip_trailing_ws" => Ok(Normalize::StripTrailingWs),
                    other => Err(de::Error::unknown_variant(other, &["strip_trailing_ws"])),
                }
            }

            // Struct variant encoded as `mask: { pattern: ..., token: ... }`
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Normalize, A::Error> {
                let key: String = map
                    .next_key()?
                    .ok_or_else(|| de::Error::invalid_length(0, &"a map with exactly one key"))?;
                match key.as_str() {
                    "mask" => {
                        #[derive(Deserialize)]
                        struct MaskBody {
                            pattern: String,
                            token: String,
                        }
                        let body: MaskBody = map.next_value()?;
                        Ok(Normalize::Mask {
                            pattern: body.pattern,
                            token: body.token,
                        })
                    }
                    other => Err(de::Error::unknown_variant(other, &["mask"])),
                }
            }
        }

        d.deserialize_any(NormalizeVisitor)
    }
}

/// Where a mined case came from (informational; preserved through round-trips).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Provenance {
    #[default]
    Manual,
    Readme,
    Help,
    Fixture,
}

/// One behavioral case: an invocation + (after capture) its golden output.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Case {
    pub name: String,
    /// Provenance (`source:` key in YAML — distinct from the spec's `source` Side).
    #[serde(rename = "source", default)]
    pub provenance: Provenance,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdin: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Golden output captured from the source. `None` until capture runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect: Option<Expect>,
    /// Set when the source disagreed with itself across two runs; quarantined.
    #[serde(default, skip_serializing_if = "is_false")]
    pub nondeterministic: bool,
    /// Per-case compare override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compare: Option<CompareSpec>,
}

/// Golden output captured from the source binary.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Expect {
    #[serde(default)]
    pub stdout: String,
    #[serde(default)]
    pub stderr: String,
    #[serde(default)]
    pub exit_code: i32,
}

/// What one binary actually did on one case (runtime capture).
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

/// `skip_serializing_if` helper: serde hands the field by reference, so this
/// takes `&bool` (unlike `std::ops::Not::not`, which takes `bool` by value).
fn is_false(b: &bool) -> bool {
    !*b
}

/// Build the source once, then capture golden output for every case by running
/// the source binary. Each case is run TWICE: if the source disagrees with
/// itself on any compared stream, the case is flagged `nondeterministic` and
/// quarantined (its `expect` is left as the first run for visibility, but it is
/// excluded from gating/repair by `verify`). "Compared stream" is evaluated
/// under the case's own compare policy — a stream set to `ignore`, or whose
/// noise is masked by a `normalize` rule, is intentionally not flagged, and
/// `verify` later applies the identical policy.
pub fn capture_all(spec: &mut BehaviorSpec, root: &Path, work: &Path) -> Result<(), String> {
    build_side(&spec.source, "source", root, work)?;
    let default_compare = spec.compare;
    let rules = spec.normalize.clone();

    for case in &mut spec.cases {
        let first = run_case(&spec.source, case, root, work);
        let second = run_case(&spec.source, case, root, work);

        let compare = case.compare.unwrap_or(default_compare);
        let first_expect = Expect {
            stdout: first.stdout.clone(),
            stderr: first.stderr.clone(),
            exit_code: first.exit_code,
        };
        // Compare the two source runs against each other using the same diff
        // policy: any difference == nondeterministic.
        let self_diffs = diff_case(&first_expect, &second, &compare, &rules);
        case.nondeterministic = !self_diffs.is_empty();
        case.expect = Some(first_expect);
    }
    Ok(())
}

/// Per-case verification result.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CaseResult {
    pub name: String,
    pub matched: bool,
    pub diffs: Vec<String>,
}

/// Full verification report (also the on-disk `behavior_report.json` shape).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BehaviorReport {
    pub name: String,
    /// True when every non-quarantined case matched.
    pub passed: bool,
    /// Non-quarantined cases that matched.
    pub matched: usize,
    /// Non-quarantined cases (the gate denominator).
    pub total: usize,
    /// Cases excluded as nondeterministic.
    pub quarantined: usize,
    pub cases: Vec<CaseResult>,
}

/// Build the target, run every NON-quarantined case, and diff against golden.
/// Quarantined cases are counted in `quarantined` but excluded from
/// `total`/`matched`/`passed`. A case with no captured `expect` is skipped with
/// a diagnostic diff (it should never happen after `capture_all`).
pub fn verify(spec: &BehaviorSpec, root: &Path, work: &Path) -> Result<BehaviorReport, String> {
    build_side(&spec.target, "target", root, work)?;
    let default_compare = spec.compare;

    let mut cases = Vec::new();
    let mut matched = 0usize;
    let mut total = 0usize;
    let mut quarantined = 0usize;

    for case in &spec.cases {
        if case.nondeterministic {
            quarantined += 1;
            continue;
        }
        total += 1;
        let diffs = match &case.expect {
            None => vec!["no golden expect captured for this case".to_string()],
            Some(expect) => {
                let actual = run_case(&spec.target, case, root, work);
                let compare = case.compare.unwrap_or(default_compare);
                diff_case(expect, &actual, &compare, &spec.normalize)
            }
        };
        let ok = diffs.is_empty();
        if ok {
            matched += 1;
        }
        cases.push(CaseResult {
            name: case.name.clone(),
            matched: ok,
            diffs,
        });
    }

    Ok(BehaviorReport {
        name: spec.name.clone(),
        passed: matched == total,
        matched,
        total,
        quarantined,
        cases,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const CALCULATOR_YAML: &str = r#"
name: calculator
source: { lang: go,   dir: ., build: ["go", "build", "-o", "{work}/calc-go", "."], run: ["{work}/calc-go"] }
target: { lang: rust, dir: ., build: ["cargo", "build", "-q"], run: ["target/debug/calculator"] }
compare: { stdout: exact, stderr: exact, exit_code: exact }
normalize:
  - strip_trailing_ws
  - mask: { pattern: '\d{4}-\d{2}-\d{2}', token: '<DATE>' }
cases:
  - { name: precedence, source: readme, args: ["2 + 3 * (4 - 1)"], expect: { stdout: "11\n", stderr: "", exit_code: 0 } }
  - { name: now, source: fixture, args: ["now"], nondeterministic: true }
"#;

    #[test]
    fn parses_calculator_spec() {
        let spec: BehaviorSpec = serde_yaml::from_str(CALCULATOR_YAML).unwrap();
        assert_eq!(spec.name, "calculator");
        assert_eq!(spec.source.lang, "go");
        assert_eq!(spec.cases.len(), 2);
        assert_eq!(spec.cases[0].name, "precedence");
        assert_eq!(spec.cases[0].provenance, Provenance::Readme);
        assert_eq!(spec.cases[0].expect.as_ref().unwrap().stdout, "11\n");
        assert!(!spec.cases[0].nondeterministic);
        assert!(spec.cases[1].nondeterministic);
        assert_eq!(spec.normalize.len(), 2);
        assert_eq!(spec.compare.stdout, StreamMode::Exact);
    }

    #[test]
    fn compare_defaults_to_exact_when_absent() {
        let spec: BehaviorSpec =
            serde_yaml::from_str("name: x\nsource: {lang: go, dir: ., build: [], run: []}\ntarget: {lang: rust, dir: ., build: [], run: []}\n").unwrap();
        assert_eq!(spec.compare.stdout, StreamMode::Exact);
        assert_eq!(spec.compare.exit_code, StreamMode::Exact);
        assert!(spec.cases.is_empty());
    }

    #[test]
    fn case_provenance_defaults_to_manual() {
        let c: Case = serde_yaml::from_str("name: c\nargs: [\"x\"]\n").unwrap();
        assert_eq!(c.provenance, Provenance::Manual);
        assert!(c.expect.is_none());
        assert!(c.env.is_empty());
    }

    #[test]
    fn normalize_mask_round_trips_through_yaml() {
        let original = vec![
            Normalize::StripTrailingWs,
            Normalize::Mask {
                pattern: r"\d{4}-\d{2}-\d{2}".to_string(),
                token: "<DATE>".to_string(),
            },
        ];
        let yaml = serde_yaml::to_string(&original).expect("serialize");
        let recovered: Vec<Normalize> = serde_yaml::from_str(&yaml).expect("deserialize");
        assert_eq!(original, recovered);
    }

    #[test]
    fn capture_fills_golden_and_flags_nondeterminism() {
        use std::path::Path;
        // Deterministic source: prints "D" for arg `det`. Nondeterministic
        // source: prints the shell PID ($$), which differs each invocation.
        let mut spec = BehaviorSpec {
            name: "t".into(),
            source: Side {
                lang: "sh".into(),
                dir: ".".into(),
                build: vec![],
                run: vec![
                    "sh".into(),
                    "-c".into(),
                    "if [ \"$1\" = det ]; then printf D; else echo $$; fi".into(),
                    "sh".into(),
                ],
            },
            target: Side {
                lang: "rust".into(),
                dir: ".".into(),
                build: vec![],
                run: vec![],
            },
            compare: CompareSpec::default(),
            normalize: vec![],
            cases: vec![
                Case {
                    name: "det".into(),
                    provenance: Provenance::Manual,
                    args: vec!["det".into()],
                    stdin: None,
                    env: Default::default(),
                    expect: None,
                    nondeterministic: false,
                    compare: None,
                },
                Case {
                    name: "clock".into(),
                    provenance: Provenance::Manual,
                    args: vec!["clock".into()],
                    stdin: None,
                    env: Default::default(),
                    expect: None,
                    nondeterministic: false,
                    compare: None,
                },
            ],
        };
        let root = std::env::current_dir().unwrap();
        capture_all(&mut spec, &root, Path::new("/tmp")).unwrap();

        let det = &spec.cases[0];
        assert!(!det.nondeterministic);
        assert_eq!(det.expect.as_ref().unwrap().stdout, "D");

        let clock = &spec.cases[1];
        assert!(
            clock.nondeterministic,
            "clock case should self-detect as nondeterministic"
        );
    }

    #[test]
    fn verify_diffs_target_against_golden_and_skips_quarantined() {
        use std::path::Path;
        let spec = BehaviorSpec {
            name: "t".into(),
            source: Side {
                lang: "sh".into(),
                dir: ".".into(),
                build: vec![],
                run: vec![],
            },
            // target echoes its arg with a trailing '!' — so it MISMATCHES golden.
            target: Side {
                lang: "sh".into(),
                dir: ".".into(),
                build: vec![],
                run: vec![
                    "sh".into(),
                    "-c".into(),
                    "printf '%s!' \"$1\"".into(),
                    "sh".into(),
                ],
            },
            compare: CompareSpec::default(),
            normalize: vec![],
            cases: vec![
                Case {
                    name: "ok".into(),
                    provenance: Provenance::Manual,
                    args: vec!["hi".into()],
                    stdin: None,
                    env: Default::default(),
                    expect: Some(Expect {
                        stdout: "hi!".into(),
                        stderr: String::new(),
                        exit_code: 0,
                    }),
                    nondeterministic: false,
                    compare: None,
                },
                Case {
                    name: "bad".into(),
                    provenance: Provenance::Manual,
                    args: vec!["hi".into()],
                    stdin: None,
                    env: Default::default(),
                    expect: Some(Expect {
                        stdout: "hi".into(),
                        stderr: String::new(),
                        exit_code: 0,
                    }),
                    nondeterministic: false,
                    compare: None,
                },
                Case {
                    name: "skip".into(),
                    provenance: Provenance::Manual,
                    args: vec!["x".into()],
                    stdin: None,
                    env: Default::default(),
                    expect: Some(Expect {
                        stdout: "anything".into(),
                        stderr: String::new(),
                        exit_code: 0,
                    }),
                    nondeterministic: true,
                    compare: None,
                },
            ],
        };
        let root = std::env::current_dir().unwrap();
        let report = verify(&spec, &root, Path::new("/tmp")).unwrap();
        assert_eq!(report.total, 2, "quarantined case excluded from total");
        assert_eq!(report.matched, 1);
        assert_eq!(report.quarantined, 1);
        assert!(!report.passed);
        let bad = report.cases.iter().find(|c| c.name == "bad").unwrap();
        assert!(!bad.matched);
        assert_eq!(bad.diffs.len(), 1);
    }
}
