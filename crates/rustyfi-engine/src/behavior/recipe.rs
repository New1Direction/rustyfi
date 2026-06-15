//! Per-language build/run recipes that turn a detected source language + a
//! target crate into `behavior.yaml` `Side`s. The target is always Rust; the
//! source recipe is best-effort and language-specific. Unknown languages yield
//! `None`, which makes `phase_behavior` skip with an honest note.

use std::path::Path;

use super::Side;

/// Build the `Side` for the Rust target crate. Always `cargo build` + the debug
/// binary (the pipeline only ran `cargo check`, so the binary must be built).
pub(crate) fn target_side(crate_name: &str) -> Side {
    Side {
        lang: "rust".to_string(),
        dir: ".".to_string(),
        build: vec!["cargo".into(), "build".into(), "--quiet".into()],
        run: vec![format!("target/debug/{crate_name}")],
    }
}

/// Build the `Side` for the source project, keyed on the detected language.
/// `bin_name` is the basename used for the built source binary; `source_dir` is
/// the project root, used to confirm a runnable entrypoint actually exists.
///
/// Returns `None` when we cannot run the source — either an unsupported language
/// OR (for interpreted languages) the conventional entrypoint file is absent,
/// which means the project is a library with no CLI. Skipping is the honest
/// outcome: inventing an entrypoint makes the interpreter emit a "can't open
/// file" error that gets captured as bogus golden output, flagging every
/// translation as a false behavioral mismatch (observed on `itsdangerous`).
pub(crate) fn source_side(language: &str, bin_name: &str, source_dir: &Path) -> Option<Side> {
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
        "python" => {
            if !source_dir.join("main.py").exists() {
                return None;
            }
            Side {
                lang: "python".into(),
                dir: ".".into(),
                build: vec![],
                run: vec!["python3".into(), "main.py".into()],
            }
        }
        "javascript" | "typescript" => {
            if !source_dir.join("index.js").exists() {
                return None;
            }
            Side {
                lang: language.into(),
                dir: ".".into(),
                build: vec![],
                run: vec!["node".into(), "index.js".into()],
            }
        }
        _ => return None,
    };
    Some(side)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go_source_builds_and_runs_a_binary() {
        let s = source_side("go", "calc", Path::new(".")).expect("go supported");
        assert_eq!(s.lang, "go");
        assert!(s.build.iter().any(|a| a == "build"));
        assert!(s.run.iter().any(|a| a.contains("{work}")));
    }

    #[test]
    fn unsupported_language_yields_none() {
        assert!(source_side("haskell", "x", Path::new(".")).is_none());
    }

    #[test]
    fn python_without_main_py_is_skipped_not_fabricated() {
        // A library (no main.py) must yield None so the caller skips behavior
        // honestly, instead of running `python3 main.py` and capturing the
        // interpreter's "can't open file" error as bogus golden output.
        let dir = tempfile::tempdir().unwrap();
        assert!(source_side("python", "lib", dir.path()).is_none());
    }

    #[test]
    fn python_with_main_py_runs() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("main.py"), "print('hi')\n").unwrap();
        let s = source_side("python", "app", dir.path()).expect("has main.py");
        assert_eq!(s.run, vec!["python3".to_string(), "main.py".to_string()]);
    }

    #[test]
    fn javascript_without_index_js_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        assert!(source_side("javascript", "lib", dir.path()).is_none());
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
