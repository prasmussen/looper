//! The status line `looper run` keeps at the bottom of the terminal: which task
//! is running, how many are left, and how long this task and the whole run
//! have taken. It ticks every second, and everything `looper run` prints goes
//! through it so output scrolls above the line instead of over it.

use std::io::{IsTerminal, Write};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::logs::{Style, format_duration};

pub struct StatusLine {
    shared: Arc<Mutex<State>>,
}

struct State {
    /// Draw the line at all; off when stdout or stderr isn't a terminal.
    enabled: bool,
    style: Style,
    total: usize,
    run_started: Instant,
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
    pub fn start(total: usize, run_started: Instant) -> Self {
        let enabled = std::io::stdout().is_terminal()
            && std::io::stderr().is_terminal()
            && std::env::var_os("TERM").is_none_or(|t| t != "dumb");
        let shared = Arc::new(Mutex::new(State {
            enabled,
            style: Style::detect(),
            total,
            run_started,
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

    /// Show `title` as task `n`, with its clock starting now. A title from
    /// `generated` replaces it once it arrives.
    pub fn set_task(&self, n: usize, title: &str, generated: Option<Receiver<String>>) {
        let mut state = self.shared.lock().unwrap();
        state.task = Some(Task {
            n,
            title: title.to_string(),
            started: Instant::now(),
            pending: generated,
            generated: false,
        });
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

    /// Remove the status line for good, e.g. before the final summary.
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
        let s = self.style;
        let left = self.total - n;
        let counts = format!("[{n}/{}] {left} left", self.total);
        let times = format!(
            "task {} · total {}",
            format_duration(task_started.elapsed().as_millis() as f64),
            format_duration(self.run_started.elapsed().as_millis() as f64)
        );
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
