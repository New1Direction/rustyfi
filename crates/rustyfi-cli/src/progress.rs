//! Progress rendering. Rich, live, in-place display on a TTY; plain structured
//! lines when piped or in CI (so logs and exit codes stay scriptable).

use std::time::Duration;

use console::style;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use rustyfi_engine::pipeline::Progress;

pub struct Ui {
    rich: bool,
    mp: MultiProgress,
    phase: ProgressBar,
    files: Option<ProgressBar>,
    cur_state: String,
}

impl Ui {
    pub fn new(rich: bool) -> Self {
        let mp = MultiProgress::new();
        let phase = mp.add(ProgressBar::new_spinner());
        phase.set_style(spinner_style());
        if rich {
            phase.enable_steady_tick(Duration::from_millis(90));
        }
        Ui {
            rich,
            mp,
            phase,
            files: None,
            cur_state: String::new(),
        }
    }

    pub fn handle(&mut self, p: &Progress) {
        match p {
            Progress::StateChanged { state } => self.set_phase(state),
            Progress::PhaseResumed { phase } => {
                self.line(format!("{} resumed at {phase}", style("↻").cyan()))
            }
            Progress::FileStarted { total, .. } if self.rich && self.files.is_none() => {
                let bar = self.mp.add(ProgressBar::new(*total as u64));
                bar.set_style(bar_style());
                self.files = Some(bar);
            }
            Progress::FileComplete { file, .. } => {
                if let Some(b) = &self.files {
                    b.inc(1);
                    b.set_message(short(file));
                }
            }
            Progress::FixCycle { attempt } => {
                self.set_phase_msg(format!("repairing compile errors — cycle {attempt}"))
            }
            Progress::CompilerError { families, .. } if !families.is_empty() => {
                self.set_phase_msg(format!("compile errors: {}", families.join(", ")))
            }
            Progress::Note { message } => self.line(format!("{} {message}", style("·").dim())),
            Progress::Failed { reason } => {
                self.line(format!("{} {reason}", style("✗").red().bold()))
            }
            _ => {}
        }
    }

    /// Finish all live bars cleanly before the final summary is printed.
    pub fn finish(&mut self) {
        if let Some(b) = self.files.take() {
            b.finish_and_clear();
        }
        if !self.cur_state.is_empty() {
            self.phase
                .finish_with_message(format!("{} {}", style("✓").green(), self.cur_state));
        } else {
            self.phase.finish_and_clear();
        }
    }

    // ── internals ──────────────────────────────────────────────────────────

    fn set_phase(&mut self, state: &str) {
        // Finish the previous translation bar when we leave the translate phase.
        if state != "Translating" {
            if let Some(b) = self.files.take() {
                b.finish_and_clear();
            }
        }
        if self.rich {
            if !self.cur_state.is_empty() {
                self.phase.finish_with_message(format!(
                    "{} {}",
                    style("✓").green(),
                    self.cur_state
                ));
            }
            let next = self.mp.add(ProgressBar::new_spinner());
            next.set_style(spinner_style());
            next.enable_steady_tick(Duration::from_millis(90));
            next.set_message(label(state));
            self.phase = next;
        } else {
            eprintln!("{} {}", style("▸").yellow(), label(state));
        }
        self.cur_state = label(state);
    }

    fn set_phase_msg(&self, msg: String) {
        if self.rich {
            self.phase.set_message(msg);
        } else {
            eprintln!("  {}", style(msg).dim());
        }
    }

    fn line(&self, msg: String) {
        if self.rich {
            let _ = self.mp.println(msg);
        } else {
            eprintln!("{msg}");
        }
    }
}

fn label(state: &str) -> String {
    match state {
        "Parsing" => "Analyzing source",
        "Scaffolding" => "Scaffolding + pinning type contract",
        "Translating" => "Translating to Rust",
        "Verifying" => "Verifying with cargo check",
        "Optimizing" => "Packaging",
        "Completed" => "Done",
        other => other,
    }
    .to_string()
}

fn short(file: &str) -> String {
    file.rsplit(['/', '\\']).next().unwrap_or(file).to_string()
}

fn spinner_style() -> ProgressStyle {
    ProgressStyle::with_template("{spinner:.yellow} {msg}")
        .unwrap()
        .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏", "✓"])
}

fn bar_style() -> ProgressStyle {
    ProgressStyle::with_template("  {bar:26.green/dim} {pos:>3}/{len:<3} {msg:.dim}")
        .unwrap()
        .progress_chars("█▉▊▋▌▍▎▏ ")
}
