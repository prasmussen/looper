//! The status line `looper plan run` keeps at the bottom of the terminal: which task
//! is running out of how many, how long this task and the whole run have
//! taken, and how many tokens the run has used. It ticks every second, and
//! everything `looper plan run` prints goes through it so output scrolls above the
//! line instead of over it, with the time at the start of every line.

use std::io::{IsTerminal, Write};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::logs::{Style, Tokens, format_duration};

/// Columns the `12:34:56 ` timestamp adds in front of each line.
pub const STAMP_WIDTH: usize = 9;

pub struct StatusLine {
    shared: Arc<Mutex<State>>,
}

struct State {
    /// Draw the line at all; off when stdout or stderr isn't a terminal.
    enabled: bool,
    style: Style,
    total: usize,
    run_started: Instant,
    /// Tokens used by the tasks before the current one.
    done_tokens: Tokens,
    task: Option<Task>,
    /// Whether the line is on screen right now.
    shown: bool,
    /// Whether the last output ended mid-line; the line waits for the newline
    /// so it doesn't overwrite the text.
    mid_line: bool,
    stopped: bool,
}

struct Task {
    n: usize,
    title: String,
    started: Instant,
    tokens: Tokens,
    /// A generated title on its way, replacing `title` when it arrives.
    pending: Option<Receiver<String>>,
    /// Whether `title` is the generated one.
    generated: bool,
}

impl Task {
    fn poll_title(&mut self) {
        if let Some(title) = self.pending.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.title = title;
            self.generated = true;
            self.pending = None;
        }
    }
}

impl StatusLine {
    pub fn start(run_started: Instant) -> Self {
        let enabled = std::io::stdout().is_terminal()
            && std::io::stderr().is_terminal()
            && std::env::var_os("TERM").is_none_or(|t| t != "dumb");
        let shared = Arc::new(Mutex::new(State {
            enabled,
            style: Style::detect(),
            total: 0,
            run_started,
            done_tokens: Tokens::default(),
            task: None,
            shown: false,
            mid_line: false,
            stopped: false,
        }));
        if enabled {
            let shared = Arc::clone(&shared);
            std::thread::spawn(move || {
                loop {
                    std::thread::sleep(Duration::from_secs(1));
                    let mut state = shared.lock().unwrap();
                    if state.stopped {
                        break;
                    }
                    state.draw();
                }
            });
        }
        Self { shared }
    }

    /// Show `title` as task `n` of `total`, with its clock starting now. A
    /// title from `generated` replaces it once it arrives.
    pub fn set_task(
        &self,
        n: usize,
        total: usize,
        title: &str,
        generated: Option<Receiver<String>>,
    ) {
        let mut state = self.shared.lock().unwrap();
        state.total = total;
        if let Some(tokens) = state.task.as_ref().map(|t| t.tokens) {
            state.done_tokens += tokens;
        }
        state.task = Some(Task {
            n,
            title: title.to_string(),
            started: Instant::now(),
            tokens: Tokens::default(),
            pending: generated,
            generated: false,
        });
        state.draw();
    }

    /// Set the tokens the current task has used so far.
    pub fn set_tokens(&self, tokens: Tokens) {
        let mut state = self.shared.lock().unwrap();
        if let Some(task) = &mut state.task {
            task.tokens = tokens;
        }
        state.draw();
    }

    /// The current task's generated title, if it has arrived.
    pub fn generated_title(&self) -> Option<String> {
        let mut state = self.shared.lock().unwrap();
        let task = state.task.as_mut()?;
        task.poll_title();
        task.generated.then(|| task.title.clone())
    }

    /// Print to stdout above the status line.
    pub fn out(&self, text: &str) {
        self.shared.lock().unwrap().write(text, false);
    }

    /// Print a line to stderr above the status line.
    pub fn err(&self, line: &str) {
        self.shared
            .lock()
            .unwrap()
            .write(&format!("{line}\n"), true);
    }

    /// Remove the status line for good, e.g. before the final summary. Output
    /// still gets timestamps after this.
    pub fn stop(&self) {
        let mut state = self.shared.lock().unwrap();
        state.clear();
        state.stopped = true;
    }
}

impl Drop for StatusLine {
    fn drop(&mut self) {
        self.stop();
    }
}

impl State {
    fn write(&mut self, text: &str, stderr: bool) {
        if text.is_empty() {
            return;
        }
        self.clear();
        let text = self.stamp(text);
        if stderr {
            let _ = std::io::stdout().flush();
            let _ = std::io::stderr().write_all(text.as_bytes());
        } else {
            let _ = std::io::stdout().write_all(text.as_bytes());
            let _ = std::io::stdout().flush();
        }
        self.mid_line = !text.ends_with('\n');
        self.draw();
    }

    /// Put the time at the start of each line in `text`, leaving blank lines
    /// and the rest of a line that was started earlier alone.
    fn stamp(&self, text: &str) -> String {
        let now = chrono::Local::now().format("%H:%M:%S").to_string();
        let now = self.style.dim(&now);
        let mut out = String::with_capacity(text.len());
        let mut line_start = !self.mid_line;
        for piece in text.split_inclusive('\n') {
            if line_start && piece != "\n" {
                out.push_str(&now);
                out.push(' ');
            }
            out.push_str(piece);
            line_start = piece.ends_with('\n');
        }
        out
    }

    fn clear(&mut self) {
        if self.shown {
            let _ = write!(std::io::stderr(), "\r\x1b[2K");
            self.shown = false;
        }
    }

    fn draw(&mut self) {
        if !self.enabled || self.stopped || self.mid_line {
            return;
        }
        let Some(task) = &mut self.task else {
            return;
        };
        task.poll_title();
        let (n, title, task_started) = (task.n, &task.title, task.started);
        let mut tokens = self.done_tokens;
        tokens += task.tokens;
        let s = self.style;
        let counts = format!("[{n}/{}]", self.total);
        let mut times = format!(
            "task {} · total {}",
            format_duration(task_started.elapsed().as_millis() as f64),
            format_duration(self.run_started.elapsed().as_millis() as f64)
        );
        if !tokens.is_empty() {
            times.push_str(&format!(" · {}", tokens.format_short()));
        }
        // Stay a column short of the edge so the terminal never wraps the
        // line, which would leave a copy behind on every redraw.
        let width = match termimad::terminal_size().0 {
            0 => 80,
            w => w as usize,
        }
        .saturating_sub(1);
        let used = counts.chars().count() + times.chars().count() + 6;
        let title = crate::truncate(title, width.saturating_sub(used + 1));
        let line = format!("{counts} · {times} · {title}");
        let line = crate::truncate(&line, width.saturating_sub(1));
        let _ = write!(std::io::stderr(), "\r\x1b[2K{}", s.cyan(&line));
        let _ = std::io::stderr().flush();
        self.shown = true;
    }
}
