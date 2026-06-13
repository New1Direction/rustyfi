//! Behavioral-equivalence harness: run a source project and its Rust
//! translation against the same inputs and diff observable behavior.
//!
//! The source binary is ground truth. This is differential testing against a
//! fixture corpus — it verifies sameness on the cases exercised, not
//! equivalence in general.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::BTreeMap;

mod harness;
pub use harness::{build_side, expand, run_case};

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
}
