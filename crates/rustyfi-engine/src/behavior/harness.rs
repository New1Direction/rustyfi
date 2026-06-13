//! Process execution for the behavior harness: build a side, run a case, and
//! capture its observable behavior with a hard timeout.

use super::{Case, Outcome, Side};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;
use wait_timeout::ChildExt;

/// Maximum wall-clock seconds for a single binary invocation.
const RUN_TIMEOUT_SECS: u64 = 30;
/// Exit code reported when a run is killed for exceeding the timeout.
const TIMEOUT_EXIT_CODE: i32 = 124;
/// Exit code reported when the binary could not be spawned (not found, etc.).
const SPAWN_FAILED_EXIT_CODE: i32 = 127;

/// Substitute the `{work}` scratch-dir placeholder in a command vector.
pub fn expand(cmd: &[String], work: &Path) -> Vec<String> {
    let w = work.to_string_lossy();
    cmd.iter().map(|p| p.replace("{work}", &w)).collect()
}

/// Run a side's build command. Returns `Err(message)` (with captured output)
/// on a non-zero exit so the caller can surface it honestly.
pub fn build_side(side: &Side, label: &str, root: &Path, work: &Path) -> Result<(), String> {
    let cmd = expand(&side.build, work);
    if cmd.is_empty() {
        return Ok(()); // nothing to build (e.g. interpreted source)
    }
    let cwd = root.join(&side.dir);
    let output = Command::new(&cmd[0])
        .args(&cmd[1..])
        .current_dir(&cwd)
        .output()
        .map_err(|e| format!("{label} build failed to spawn ({}): {e}", cmd.join(" ")))?;
    if !output.status.success() {
        return Err(format!(
            "{label} build failed ({}):\n{}\n{}",
            cmd.join(" "),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(())
}

/// Invoke one side's binary for one case and capture its behavior. Reads
/// stdout/stderr on background threads to avoid pipe-buffer deadlock, and
/// kills the process if it exceeds `RUN_TIMEOUT_SECS`.
pub fn run_case(side: &Side, case: &Case, root: &Path, work: &Path) -> Outcome {
    let mut cmd = expand(&side.run, work);
    if cmd.is_empty() {
        return Outcome {
            stdout: String::new(),
            stderr: "<empty run command>".to_string(),
            exit_code: SPAWN_FAILED_EXIT_CODE,
        };
    }
    cmd.extend(case.args.iter().cloned());
    let cwd = root.join(&side.dir);

    let mut command = Command::new(&cmd[0]);
    command
        .args(&cmd[1..])
        .current_dir(&cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in &case.env {
        command.env(k, v);
    }

    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => {
            return Outcome {
                stdout: String::new(),
                stderr: format!("<spawn failed: {e}>"),
                exit_code: SPAWN_FAILED_EXIT_CODE,
            }
        }
    };

    // Feed stdin (drop the handle to signal EOF).
    if let Some(input) = &case.stdin {
        if let Some(mut sin) = child.stdin.take() {
            let _ = sin.write_all(input.as_bytes());
        }
    } else {
        drop(child.stdin.take());
    }

    // Drain stdout/stderr concurrently so a full pipe can't block `wait`.
    let mut out_pipe = child.stdout.take();
    let mut err_pipe = child.stderr.take();
    let (tx_out, rx_out) = mpsc::channel();
    let (tx_err, rx_err) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(p) = out_pipe.as_mut() {
            use std::io::Read;
            let _ = p.read_to_end(&mut buf);
        }
        let _ = tx_out.send(buf);
    });
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(p) = err_pipe.as_mut() {
            use std::io::Read;
            let _ = p.read_to_end(&mut buf);
        }
        let _ = tx_err.send(buf);
    });

    let status = match child.wait_timeout(Duration::from_secs(RUN_TIMEOUT_SECS)) {
        Ok(Some(status)) => status.code().unwrap_or(TIMEOUT_EXIT_CODE),
        Ok(None) => {
            let _ = child.kill();
            let _ = child.wait();
            TIMEOUT_EXIT_CODE
        }
        Err(_) => TIMEOUT_EXIT_CODE,
    };

    // Bound the wait for the reader threads: if a surviving grandchild keeps a
    // pipe write-end open, read_to_end can block forever — recv_timeout caps it
    // (the orphaned reader thread leaks harmlessly and dies when the FD closes).
    let drain = Duration::from_secs(5);
    let stdout =
        String::from_utf8_lossy(&rx_out.recv_timeout(drain).unwrap_or_default()).into_owned();
    let stderr =
        String::from_utf8_lossy(&rx_err.recv_timeout(drain).unwrap_or_default()).into_owned();
    Outcome {
        stdout,
        stderr,
        exit_code: status,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn echo_side() -> Side {
        // A POSIX `sh -c` shim is a stable, language-agnostic test binary.
        Side {
            lang: "sh".into(),
            dir: ".".into(),
            build: vec!["true".into()],
            run: vec![
                "sh".into(),
                "-c".into(),
                "printf '%s' \"$1\"; printf 'E' 1>&2; exit 3".into(),
                "sh".into(),
            ],
        }
    }

    #[test]
    fn expand_substitutes_work_placeholder() {
        let out = expand(&["{work}/bin".into(), "x".into()], Path::new("/tmp/w"));
        assert_eq!(out, vec!["/tmp/w/bin".to_string(), "x".to_string()]);
    }

    #[test]
    fn run_case_captures_streams_and_exit() {
        let root = std::env::current_dir().unwrap();
        let work = root.clone();
        let case = Case {
            name: "t".into(),
            provenance: Default::default(),
            args: vec!["hello".into()],
            stdin: None,
            env: BTreeMap::new(),
            expect: None,
            nondeterministic: false,
            compare: None,
        };
        let out = run_case(&echo_side(), &case, &root, &work);
        assert_eq!(out.stdout, "hello");
        assert_eq!(out.stderr, "E");
        assert_eq!(out.exit_code, 3);
    }

    #[test]
    fn run_case_feeds_stdin() {
        let root = std::env::current_dir().unwrap();
        let side = Side {
            lang: "sh".into(),
            dir: ".".into(),
            build: vec!["true".into()],
            run: vec!["cat".into()],
        };
        let case = Case {
            name: "t".into(),
            provenance: Default::default(),
            args: vec![],
            stdin: Some("piped-in\n".into()),
            env: BTreeMap::new(),
            expect: None,
            nondeterministic: false,
            compare: None,
        };
        let out = run_case(&side, &case, &root, &root);
        assert_eq!(out.stdout, "piped-in\n");
        assert_eq!(out.exit_code, 0);
    }

    #[test]
    fn build_side_reports_failure() {
        let side = Side {
            lang: "sh".into(),
            dir: ".".into(),
            build: vec!["false".into()],
            run: vec!["true".into()],
        };
        let root = std::env::current_dir().unwrap();
        let err = build_side(&side, "source", &root, &root).unwrap_err();
        assert!(err.contains("source build failed"));
    }

    #[test]
    fn run_case_with_empty_run_does_not_panic() {
        let side = Side {
            lang: "x".into(),
            dir: ".".into(),
            build: vec![],
            run: vec![],
        };
        let root = std::env::current_dir().unwrap();
        let case = Case {
            name: "t".into(),
            provenance: Default::default(),
            args: vec![],
            stdin: None,
            env: std::collections::BTreeMap::new(),
            expect: None,
            nondeterministic: false,
            compare: None,
        };
        let out = run_case(&side, &case, &root, &root);
        assert_eq!(out.exit_code, 127);
    }
}
