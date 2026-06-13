//! Per-language build/run recipes that turn a detected source language + a
//! target crate into `behavior.yaml` `Side`s. The target is always Rust; the
//! source recipe is best-effort and language-specific. Unknown languages yield
//! `None`, which makes `phase_behavior` skip with an honest note.

use super::Side;

/// Build the `Side` for the Rust target crate. Always `cargo build` + the debug
/// binary (the pipeline only ran `cargo check`, so the binary must be built).
#[allow(dead_code)] // wired into phase_behavior in a later task
pub(crate) fn target_side(crate_name: &str) -> Side {
    Side {
        lang: "rust".to_string(),
        dir: ".".to_string(),
        build: vec!["cargo".into(), "build".into(), "--quiet".into()],
        run: vec![format!("target/debug/{crate_name}")],
    }
}

/// Build the `Side` for the source project, keyed on the detected language.
/// `bin_name` is the basename used for the built source binary. Returns `None`
/// for languages we cannot yet build/run, so the caller skips behavior.
#[allow(dead_code)] // wired into phase_behavior in a later task
pub(crate) fn source_side(language: &str, bin_name: &str) -> Option<Side> {
    let side = match language {
        "go" => Side {
            lang: "go".into(),
            dir: ".".into(),
            build: vec![
                "go".into(),
                "build".into(),
                "-o".into(),
                format!("{{work}}/{bin_name}-src"),
                ".".into(),
            ],
            run: vec![format!("{{work}}/{bin_name}-src")],
        },
        "python" => Side {
            lang: "python".into(),
            dir: ".".into(),
            build: vec![],
            run: vec!["python3".into(), "main.py".into()],
        },
        "javascript" | "typescript" => Side {
            lang: language.into(),
            dir: ".".into(),
            build: vec![],
            run: vec!["node".into(), "index.js".into()],
        },
        _ => return None,
    };
    Some(side)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go_source_builds_and_runs_a_binary() {
        let s = source_side("go", "calc").expect("go supported");
        assert_eq!(s.lang, "go");
        assert!(s.build.iter().any(|a| a == "build"));
        assert!(s.run.iter().any(|a| a.contains("{work}")));
    }

    #[test]
    fn unsupported_language_yields_none() {
        assert!(source_side("haskell", "x").is_none());
    }

    #[test]
    fn target_is_rust_cargo_build_then_debug_binary() {
        let t = target_side("my_crate");
        assert_eq!(t.lang, "rust");
        assert_eq!(
            t.build,
            vec![
                "cargo".to_string(),
                "build".to_string(),
                "--quiet".to_string()
            ]
        );
        assert_eq!(t.run, vec!["target/debug/my_crate".to_string()]);
    }
}
