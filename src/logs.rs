//! `looper log list` and `looper log show`: read the `.jsonl` transcripts
//! written by `looper plan run` and print them in a human readable form.

use std::collections::HashMap;
use std::io::{IsTerminal, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::{first_line, truncate};

/// Tool output lines shown per tool call below `--detail full`.
const RESULT_LINES: usize = 8;
/// Diff lines shown per edit below `--detail full`.
const DIFF_LINES: usize = 30;
/// Error output lines shown per tool call below `--detail full`.
const ERROR_LINES: usize = 20;

/// ANSI styling that switches itself off when stdout isn't a terminal or
/// `NO_COLOR` is set, and on regardless when `CLICOLOR_FORCE` is set.
#[derive(Clone, Copy)]
pub(crate) struct Style {
    pub(crate) color: bool,
}

impl Style {
    pub(crate) fn detect() -> Self {
        Self {
            color: std::env::var_os("CLICOLOR_FORCE").is_some_and(|v| v != "0")
                || (std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()),
        }
    }

    fn paint(self, code: &str, text: &str) -> String {
        if self.color {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }

    pub(crate) fn bold(self, t: &str) -> String {
        self.paint("1", t)
    }
    pub(crate) fn dim(self, t: &str) -> String {
        self.paint("2", t)
    }
    pub(crate) fn red(self, t: &str) -> String {
        self.paint("31", t)
    }
    fn green(self, t: &str) -> String {
        self.paint("32", t)
    }
    pub(crate) fn yellow(self, t: &str) -> String {
        self.paint("33", t)
    }
    pub(crate) fn cyan(self, t: &str) -> String {
        self.paint("36", t)
    }
    fn magenta(self, t: &str) -> String {
        self.paint("35", t)
    }

    fn status(self, status: Status) -> String {
        match status {
            Status::Ok => self.green("✓ ok"),
            Status::Failed => self.red("✗ failed"),
            Status::Incomplete => self.yellow("… incomplete"),
        }
    }
}

/// How much of a transcript `looper log show` prints. Claude's own text is
/// always shown in full.
#[derive(Clone, Copy, PartialEq, Default, clap::ValueEnum)]
pub enum Detail {
    /// Each tool call as its short description and one line of command; no tool
    /// output, diffs or file contents (failures still get one line)
    Minimal,
    /// Like normal, but each tool call (command, file path, ...) is cut to a
    /// single line
    Compact,
    /// Tool calls in full, tool output and diffs shortened
    #[default]
    Normal,
    /// Everything: full prompt, all tool output, thinking, raw tool inputs
    Full,
}

#[derive(Clone, Copy, PartialEq)]
enum Status {
    Ok,
    Failed,
    Incomplete,
}

/// One task's transcript: looper's own start/exit events around claude's
/// stream-json events.
struct TaskLog {
    path: PathBuf,
    events: Vec<Value>,
}

impl TaskLog {
    fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let events = text
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        Ok(Self {
            path: path.to_path_buf(),
            events,
        })
    }

    fn find(&self, pred: impl Fn(&Value) -> bool) -> Option<&Value> {
        self.events.iter().find(|e| pred(e))
    }

    fn start(&self) -> Option<&Value> {
        self.find(|e| e["type"] == "looper" && e["event"] == "start")
    }

    fn exit(&self) -> Option<&Value> {
        self.find(|e| e["type"] == "looper" && e["event"] == "exit")
    }

    fn before_task(&self) -> Option<&Value> {
        self.find(|e| e["type"] == "looper" && e["event"] == "before_task")
    }

    /// Whether the `before_task` command failed, so claude never ran.
    fn before_task_failed(&self) -> bool {
        self.before_task().is_some_and(|e| e["success"] == false)
    }

    fn result(&self) -> Option<&Value> {
        self.find(|e| e["type"] == "result")
    }

    fn init(&self) -> Option<&Value> {
        self.find(|e| e["type"] == "system" && e["subtype"] == "init")
    }

    fn number(&self) -> String {
        match self.start().and_then(|s| s["task"].as_u64()) {
            Some(n) => n.to_string(),
            None => task_number_from_path(&self.path).unwrap_or_else(|| "?".into()),
        }
    }

    fn total(&self) -> Option<u64> {
        self.start().and_then(|s| s["total"].as_u64())
    }

    /// The generated title, or the first line of the task for logs without
    /// one, cut to `max` characters.
    fn title(&self, max: usize) -> String {
        self.find(|e| e["type"] == "looper" && e["event"] == "title")
            .and_then(|e| e["title"].as_str())
            .or_else(|| {
                self.start()
                    .and_then(|s| s["task_text"].as_str())
                    .map(first_line)
            })
            .map(|t| truncate(t, max))
            .unwrap_or_else(|| "(unknown task)".into())
    }

    fn status(&self) -> Status {
        let exit_ok = self.exit().map(|e| e["success"].as_bool().unwrap_or(false));
        let result_ok = self
            .result()
            .map(|r| !r["is_error"].as_bool().unwrap_or(false));
        if self.before_task_failed() {
            return Status::Failed;
        }
        match (exit_ok, result_ok) {
            (Some(false), _) | (_, Some(false)) => Status::Failed,
            (_, Some(true)) | (Some(true), None) => Status::Ok,
            (None, None) => Status::Incomplete,
        }
    }

    fn cost(&self) -> f64 {
        self.result()
            .and_then(|r| r["total_cost_usd"].as_f64())
            .unwrap_or_default()
    }

    fn duration_ms(&self) -> f64 {
        self.result()
            .and_then(|r| r["duration_ms"].as_f64())
            .unwrap_or_default()
    }

    fn tokens(&self) -> Tokens {
        self.result()
            .map(|r| Tokens::from_usage(&r["usage"]))
            .unwrap_or_default()
    }
}

fn task_number_from_path(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let n: u64 = stem.strip_prefix("task-")?.parse().ok()?;
    Some(n.to_string())
}

/// All run folders in `log_dir`, newest first.
fn runs(log_dir: &Path) -> Result<Vec<PathBuf>> {
    let entries = match std::fs::read_dir(log_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(e).with_context(|| format!("failed to read {}", log_dir.display()));
        }
    };
    let mut runs: Vec<(std::time::SystemTime, PathBuf)> = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().is_dir())
        .map(|entry| {
            let time = entry
                .metadata()
                .and_then(|m| m.created().or_else(|_| m.modified()))
                .unwrap_or(std::time::UNIX_EPOCH);
            (time, entry.path())
        })
        .collect();
    runs.sort_by(|a, b| b.0.cmp(&a.0));
    Ok(runs.into_iter().map(|(_, path)| path).collect())
}

fn task_files(run_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(run_dir)
        .with_context(|| format!("failed to read {}", run_dir.display()))?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .collect();
    files.sort();
    Ok(files)
}

fn load_run(run_dir: &Path) -> Result<Vec<TaskLog>> {
    task_files(run_dir)?
        .iter()
        .map(|p| TaskLog::load(p))
        .collect()
}

/// Find a run by name (a folder in `log_dir`) or by path. `None` means the
/// most recent run.
fn resolve_run(log_dir: &Path, run: Option<&str>) -> Result<PathBuf> {
    let Some(run) = run else {
        return runs(log_dir)?
            .into_iter()
            .next()
            .with_context(|| format!("no runs found in {}", log_dir.display()));
    };
    let by_name = log_dir.join(run);
    if by_name.is_dir() {
        return Ok(by_name);
    }
    let by_path = PathBuf::from(run);
    if by_path.exists() {
        return Ok(by_path);
    }
    bail!(
        "no run named {run} in {} (see `looper log list`)",
        log_dir.display()
    )
}

fn run_name(run_dir: &Path) -> String {
    run_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| run_dir.display().to_string())
}

/// Tokens by type, from a `usage` object.
#[derive(Clone, Copy, Default)]
pub struct Tokens {
    pub input: u64,
    pub cache_write: u64,
    pub cache_read: u64,
    pub output: u64,
}

impl Tokens {
    pub fn from_usage(usage: &Value) -> Self {
        let get = |key: &str| usage[key].as_u64().unwrap_or_default();
        Self {
            input: get("input_tokens"),
            cache_write: get("cache_creation_input_tokens"),
            cache_read: get("cache_read_input_tokens"),
            output: get("output_tokens"),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.input + self.cache_write + self.cache_read + self.output == 0
    }

    /// Every type, e.g. `tokens 174 in / 43k out · cache: 7.3M read / 117k write`.
    pub fn format(&self) -> String {
        format!(
            "tokens {} in / {} out · cache: {} read / {} write",
            format_count(self.input),
            format_count(self.output),
            format_count(self.cache_read),
            format_count(self.cache_write)
        )
    }

    /// Input of every kind against output, e.g. `tokens 7.4M in / 43k out`,
    /// for where the full split doesn't fit.
    pub fn format_short(&self) -> String {
        format!(
            "tokens {} in / {} out",
            format_count(self.input + self.cache_write + self.cache_read),
            format_count(self.output)
        )
    }
}

impl std::ops::AddAssign for Tokens {
    fn add_assign(&mut self, other: Self) {
        self.input += other.input;
        self.cache_write += other.cache_write;
        self.cache_read += other.cache_read;
        self.output += other.output;
    }
}

/// Format a count as e.g. `850`, `12.3k`, `123k` or `7.4M`.
fn format_count(n: u64) -> String {
    let f = n as f64;
    match n {
        0..1_000 => n.to_string(),
        1_000..10_000 => format!("{:.1}k", f / 1e3),
        10_000..1_000_000 => format!("{:.0}k", f / 1e3),
        _ => format!("{:.1}M", f / 1e6),
    }
}

/// Format a duration as e.g. `45s`, `17m 57s`, `2h 0m 13s` or `1d 3h 4m 5s`,
/// dropping the milliseconds.
pub fn format_duration(ms: f64) -> String {
    let secs = (ms / 1000.0) as u64;
    let parts = [
        (secs / 86_400, "d"),
        (secs % 86_400 / 3600, "h"),
        (secs % 3600 / 60, "m"),
        (secs % 60, "s"),
    ];
    // Start at the largest non-zero unit and keep every unit below it.
    let first = parts.iter().position(|(n, _)| *n > 0).unwrap_or(3);
    parts[first..]
        .iter()
        .map(|(n, unit)| format!("{n}{unit}"))
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn terminal_width() -> usize {
    (termimad::terminal_size().0 as usize).clamp(40, 120)
}

/// Delete runs, given as folder names in `log_dir`. All are looked up before
/// any is deleted, so a typo deletes nothing.
pub fn delete(log_dir: &Path, names: &[String]) -> Result<()> {
    let dirs = names
        .iter()
        .map(|run| {
            let dir = log_dir.join(run);
            if run.is_empty() || run.contains(['/', '\\']) || run.starts_with('.') || !dir.is_dir()
            {
                bail!(
                    "no run named {run} in {} (see `looper log list`)",
                    log_dir.display()
                );
            }
            Ok(dir)
        })
        .collect::<Result<Vec<_>>>()?;
    for dir in dirs {
        std::fs::remove_dir_all(&dir)
            .with_context(|| format!("failed to delete {}", dir.display()))?;
        eprintln!("deleted {}", dir.display());
    }
    Ok(())
}

/// Delete every run folder in `log_dir`, keeping the folder itself.
pub fn clean(log_dir: &Path) -> Result<()> {
    let runs = runs(log_dir)?;
    for run in &runs {
        std::fs::remove_dir_all(run)
            .with_context(|| format!("failed to delete {}", run.display()))?;
    }
    eprintln!(
        "deleted {} from {}",
        plural(runs.len(), "run"),
        log_dir.display()
    );
    Ok(())
}

pub(crate) fn plural(n: usize, word: &str) -> String {
    match n {
        1 => format!("1 {word}"),
        n => format!("{n} {word}s"),
    }
}

pub fn list(log_dir: &Path, run: Option<&str>) -> Result<()> {
    let style = Style::detect();
    match run {
        Some(run) => list_tasks(&resolve_run(log_dir, Some(run))?, style),
        None => list_runs(log_dir, style),
    }
}

fn list_runs(log_dir: &Path, style: Style) -> Result<()> {
    let runs = runs(log_dir)?;
    if runs.is_empty() {
        eprintln!("no runs found in {}", log_dir.display());
        return Ok(());
    }

    let rows: Vec<Vec<Cell>> = runs
        .iter()
        .map(|run_dir| {
            let tasks = load_run(run_dir)?;
            let total = tasks
                .iter()
                .find_map(TaskLog::total)
                .map_or(tasks.len(), |n| n as usize);
            let failed = tasks
                .iter()
                .filter(|t| t.status() == Status::Failed)
                .count();
            let incomplete =
                tasks.len() < total || tasks.iter().any(|t| t.status() == Status::Incomplete);
            let status = if failed > 0 {
                Cell::styled(
                    format!("✗ {failed} failed"),
                    style.red(&format!("✗ {failed} failed")),
                )
            } else if incomplete {
                Cell::styled("… incomplete", style.status(Status::Incomplete))
            } else {
                Cell::styled("✓ ok", style.status(Status::Ok))
            };
            let ms: f64 = tasks.iter().map(TaskLog::duration_ms).sum();
            let cost: f64 = tasks.iter().map(TaskLog::cost).sum();
            Ok(vec![
                Cell::styled(run_name(run_dir), style.bold(&run_name(run_dir))),
                Cell::plain(format!("{}/{total}", tasks.len())),
                status,
                Cell::plain(format_duration(ms)),
                Cell::plain(format!("${cost:.2}")),
            ])
        })
        .collect::<Result<_>>()?;

    print_table(style, &["RUN", "TASKS", "STATUS", "TIME", "COST"], &rows);
    Ok(())
}

fn list_tasks(run_dir: &Path, style: Style) -> Result<()> {
    let rows: Vec<Vec<Cell>> = load_run(run_dir)?
        .iter()
        .map(|t| {
            let status = t.status();
            let plain = match status {
                Status::Ok => "✓ ok",
                Status::Failed => "✗ failed",
                Status::Incomplete => "… incomplete",
            };
            vec![
                Cell::plain(t.number()),
                Cell::styled(plain, style.status(status)),
                Cell::plain(format_duration(t.duration_ms())),
                Cell::plain(format!("${:.2}", t.cost())),
                Cell::plain(t.title(70)),
            ]
        })
        .collect();
    println!("{}\n", style.bold(&run_dir.display().to_string()));
    print_table(style, &["#", "STATUS", "TIME", "COST", "TASK"], &rows);
    Ok(())
}

/// A table cell: its visible text (for measuring) and what to print.
pub(crate) struct Cell {
    plain: String,
    styled: String,
}

impl Cell {
    pub(crate) fn plain(text: impl Into<String>) -> Self {
        let text = text.into();
        Self {
            styled: text.clone(),
            plain: text,
        }
    }

    pub(crate) fn styled(plain: impl Into<String>, styled: String) -> Self {
        Self {
            plain: plain.into(),
            styled,
        }
    }
}

pub(crate) fn print_table(style: Style, header: &[&str], rows: &[Vec<Cell>]) {
    let mut widths: Vec<usize> = header.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.plain.chars().count());
        }
    }
    let header: Vec<String> = header
        .iter()
        .zip(&widths)
        .map(|(h, w)| format!("{h:w$}"))
        .collect();
    println!("{}", style.dim(header.join("  ").trim_end()));
    for row in rows {
        let line: Vec<String> = row
            .iter()
            .zip(&widths)
            .map(|(cell, w)| {
                let pad = w.saturating_sub(cell.plain.chars().count());
                format!("{}{}", cell.styled, " ".repeat(pad))
            })
            .collect();
        println!("{}", line.join("  ").trim_end());
    }
}

pub fn show(
    log_dir: &Path,
    run: Option<&str>,
    task: Option<usize>,
    detail: Detail,
    pager: bool,
) -> Result<()> {
    let target = resolve_run(log_dir, run)?;
    let files = if target.is_file() {
        vec![target]
    } else {
        let files = task_files(&target)?;
        match task {
            Some(n) => {
                let wanted = n.to_string();
                let file = files
                    .into_iter()
                    .find(|f| task_number_from_path(f).as_deref() == Some(wanted.as_str()))
                    .with_context(|| format!("no task {n} in {}", target.display()))?;
                vec![file]
            }
            None => files,
        }
    };
    if files.is_empty() {
        bail!("no task logs found");
    }

    let mut r = Renderer::new(detail);
    for (i, file) in files.iter().enumerate() {
        if i > 0 {
            r.out.push('\n');
        }
        r.task(&TaskLog::load(file)?);
    }
    page(&r.out, pager)
}

/// Print through `$PAGER` (default `less -FRX`) when stdout is a terminal,
/// like git does.
pub(crate) fn page(text: &str, pager: bool) -> Result<()> {
    if !pager || !std::io::stdout().is_terminal() {
        print!("{text}");
        return Ok(());
    }
    let pager_cmd = std::env::var("PAGER").unwrap_or_else(|_| "less".into());
    let mut parts = pager_cmd.split_whitespace();
    let Some(program) = parts.next() else {
        print!("{text}");
        return Ok(());
    };
    let mut cmd = Command::new(program);
    cmd.args(parts).stdin(Stdio::piped());
    if std::env::var_os("LESS").is_none() {
        cmd.env("LESS", "FRX");
    }
    let Ok(mut child) = cmd.spawn() else {
        print!("{text}");
        return Ok(());
    };
    if let Some(mut stdin) = child.stdin.take() {
        // The user quitting the pager early closes the pipe; that's fine.
        let _ = stdin.write_all(text.as_bytes());
    }
    child.wait()?;
    Ok(())
}

/// Renders claude's events as they arrive during `looper plan run`, in the same
/// format as `looper log show --detail minimal`.
pub struct LiveRenderer {
    renderer: Renderer,
    /// How much of `renderer.out` has been printed. The output is kept so
    /// blank-line handling can see what came before.
    printed: usize,
}

impl LiveRenderer {
    pub fn new() -> Self {
        Self {
            renderer: Renderer {
                // Leave room for the timestamp in front of each line.
                width: terminal_width().saturating_sub(crate::status::STAMP_WIDTH),
                ..Renderer::new(Detail::Minimal)
            },
            printed: 0,
        }
    }

    /// Render one event and return the output it added, ready to print.
    pub fn event(&mut self, event: &Value) -> &str {
        self.renderer.event(event);
        let start = std::mem::replace(&mut self.printed, self.renderer.out.len());
        &self.renderer.out[start..]
    }
}

struct Renderer {
    out: String,
    style: Style,
    width: usize,
    full: bool,
    /// Cut tool calls to one line.
    compact: bool,
    /// Show tool calls without anything that follows them.
    minimal: bool,
    /// Claude's working directory for the current task, used to shorten paths.
    cwd: Option<String>,
    /// Tool name by tool_use id, to know what a tool result belongs to.
    tools: HashMap<String, String>,
}

impl Renderer {
    fn new(detail: Detail) -> Self {
        Self {
            out: String::new(),
            style: Style::detect(),
            width: terminal_width(),
            full: detail == Detail::Full,
            compact: matches!(detail, Detail::Compact | Detail::Minimal),
            minimal: detail == Detail::Minimal,
            cwd: None,
            tools: HashMap::new(),
        }
    }

    /// Render one stream-json event from claude.
    fn event(&mut self, event: &Value) {
        match event["type"].as_str() {
            Some("system") if event["subtype"] == "init" => {
                if let Some(cwd) = event["cwd"].as_str() {
                    self.cwd = Some(format!("{}/", cwd.trim_end_matches('/')));
                }
            }
            Some("assistant") => self.assistant(event),
            Some("user") => self.tool_results(event),
            Some("result") => self.result(event),
            _ => {}
        }
    }

    fn line(&mut self, text: &str) {
        self.out.push_str(text);
        self.out.push('\n');
    }

    /// Make sure what comes next is separated by a blank line, without
    /// doubling up blank lines already there.
    fn blank_line(&mut self) {
        if !self.out.is_empty() && !self.out.ends_with("\n\n") {
            self.out.push('\n');
        }
    }

    fn task(&mut self, log: &TaskLog) {
        let s = self.style;
        self.cwd = log
            .init()
            .and_then(|i| i["cwd"].as_str())
            .map(|cwd| format!("{}/", cwd.trim_end_matches('/')));
        self.tools.clear();
        let status = log.status();
        let total = log.total().map(|n| format!("/{n}")).unwrap_or_default();

        // Header box.
        let mut stats = vec![s.status(status)];
        if log.result().is_some() {
            stats.push(format_duration(log.duration_ms()));
            stats.push(log.tokens().format());
            stats.push(format!("${:.2}", log.cost()));
        }
        let head = format!("Task {}{total}", log.number());
        self.line(&format!(
            "{} {}  {}",
            s.cyan("╭─"),
            s.bold(&head),
            stats.join(&s.dim(" · "))
        ));
        let title = log.title(self.width.saturating_sub(3));
        self.line(&format!("{}  {}", s.cyan("│"), s.bold(&title)));

        let mut meta = Vec::new();
        if let Some(start) = log.start() {
            meta.push(start["task_file"].as_str().unwrap_or("?").to_string());
            meta.push(start["started_at"].as_str().unwrap_or("?").to_string());
        }
        if let Some(init) = log.init() {
            meta.push(init["model"].as_str().unwrap_or("?").to_string());
            meta.push(format!(
                "{} mode",
                init["permissionMode"].as_str().unwrap_or("?")
            ));
        }
        if !meta.is_empty() {
            self.line(&format!("{}  {}", s.cyan("│"), s.dim(&meta.join(" · "))));
        }
        let session = log
            .init()
            .and_then(|i| i["session_id"].as_str())
            .map(|id| format!(" · claude --resume {id}"))
            .unwrap_or_default();
        self.line(&format!(
            "{} {}",
            s.cyan("╰─"),
            s.dim(&format!("{}{session}", log.path.display()))
        ));
        self.line("");

        // What Claude was asked.
        if let Some(start) = log.start() {
            let (heading, text) = if self.full {
                ("prompt", start["prompt"].as_str())
            } else {
                ("task", start["task_text"].as_str())
            };
            if let Some(text) = text {
                self.section(heading);
                self.markdown(text.trim(), false);
                self.line("");
            }
        }

        if let Some(before) = log.before_task() {
            self.section("before task");
            let command = before["command"].as_str().unwrap_or_default();
            self.line(&s.dim(&format!("$ {}", command.trim())));
            let output = before["output"].as_str().unwrap_or_default();
            if !output.trim().is_empty() {
                self.line(output.trim_end());
            }
            if before["success"] == false {
                let code = before["exit_code"]
                    .as_i64()
                    .map_or("unknown".into(), |c| c.to_string());
                self.line(&s.red(&format!(
                    "✗ before_task exited with code {code}; claude didn't run"
                )));
                return;
            }
            self.line("");
        }

        self.section("transcript");
        for event in &log.events {
            self.event(event);
        }

        match log.exit() {
            Some(exit) if exit["success"] == false => {
                let code = exit["exit_code"]
                    .as_i64()
                    .map_or("unknown".into(), |c| c.to_string());
                self.line(&s.red(&format!("✗ claude exited with code {code}")));
            }
            None if log.result().is_none() => {
                self.line(&s.yellow("… no result: the run was interrupted or is still going"));
            }
            _ => {}
        }
    }

    /// Cut multi-line text to its first non-empty line, fitting the terminal
    /// after `used` columns. Returns the line and a note like " (+3 lines)".
    fn one_line(&self, text: &str, used: usize) -> (String, String) {
        let mut lines = text.lines().filter(|l| !l.trim().is_empty());
        let first = lines.next().unwrap_or_default().trim_end();
        let rest = lines.count();
        let more = match rest {
            0 => String::new(),
            1 => " (+1 line)".to_string(),
            n => format!(" (+{n} lines)"),
        };
        let room = self
            .width
            .saturating_sub(used + more.chars().count())
            .max(20);
        (truncate(first, room), more)
    }

    /// Shorten absolute paths inside the task's working directory.
    fn relative(&self, text: &str) -> String {
        match &self.cwd {
            Some(cwd) => text.replace(cwd.as_str(), ""),
            None => text.to_string(),
        }
    }

    fn section(&mut self, name: &str) {
        let s = self.style;
        let rule = "─".repeat(self.width.saturating_sub(name.len() + 4));
        self.line(&s.dim(&format!("── {name} {rule}")));
    }

    /// Render markdown for the terminal (plain text when colors are off).
    /// Claude's own messages get their own color so they stand out from the
    /// tool calls around them.
    fn markdown(&mut self, text: &str, from_claude: bool) {
        if self.style.color {
            let mut skin = termimad::MadSkin::default();
            if from_claude {
                skin.set_fg(claude_blue());
            }
            let rendered = skin.text(text, Some(self.width)).to_string();
            self.out.push_str(&rendered);
            if !rendered.ends_with('\n') {
                self.out.push('\n');
            }
        } else {
            self.line(text);
        }
    }

    fn assistant(&mut self, event: &Value) {
        let s = self.style;
        let Some(content) = event["message"]["content"].as_array() else {
            return;
        };
        for block in content {
            match block["type"].as_str() {
                Some("text") => {
                    let text = block["text"].as_str().unwrap_or_default().trim();
                    if !text.is_empty() {
                        self.blank_line();
                        self.markdown(text, true);
                        self.line("");
                    }
                }
                Some("thinking") if self.full => {
                    let thinking = block["thinking"].as_str().unwrap_or_default().trim();
                    if !thinking.is_empty() {
                        self.line(&s.dim(&s.magenta("✻ thinking")));
                        for line in thinking.lines() {
                            self.line(&s.dim(&format!("  {line}")));
                        }
                        self.line("");
                    }
                }
                Some("tool_use") => self.tool_use(block),
                _ => {}
            }
        }
    }

    fn tool_use(&mut self, block: &Value) {
        let s = self.style;
        let name = block["name"].as_str().unwrap_or("tool");
        let input = &block["input"];
        if let Some(id) = block["id"].as_str() {
            self.tools.insert(id.to_string(), name.to_string());
        }
        let summary = self.relative(&tool_summary(name, input));
        let head = format!("{} {}", s.cyan("›"), name);
        let description = input["description"]
            .as_str()
            .map(str::trim)
            .filter(|d| name == "Bash" && !d.is_empty());

        if let Some(description) = description {
            // What Claude meant to do first, the command itself under it.
            self.line(&format!("{head} {} {description}", s.dim("·")));
            if self.compact {
                let (line, more) = self.one_line(&summary, 4);
                self.line(&s.dim(&format!("  $ {line}{more}")));
            } else {
                let mut lines = summary.lines();
                let first = lines.next().unwrap_or_default();
                self.line(&s.dim(&format!("  $ {first}")));
                for line in lines {
                    self.line(&s.dim(&format!("    {line}")));
                }
            }
        } else if self.compact {
            let (line, more) = self.one_line(&summary, name.len() + 3);
            self.line(&format!("{head} {line}{}", s.dim(&more)));
        } else {
            let mut lines = summary.lines();
            self.line(&format!("{head} {}", lines.next().unwrap_or_default()));
            for line in lines {
                self.line(&format!("  {line}"));
            }
        }
        if self.minimal {
            return;
        }

        match name {
            "Edit" => {
                let old = input["old_string"].as_str().unwrap_or_default();
                let new = input["new_string"].as_str().unwrap_or_default();
                self.diff(old, new);
            }
            "MultiEdit" => {
                for edit in input["edits"].as_array().into_iter().flatten() {
                    let old = edit["old_string"].as_str().unwrap_or_default();
                    let new = edit["new_string"].as_str().unwrap_or_default();
                    self.diff(old, new);
                }
            }
            "Write" => {
                let content = input["content"].as_str().unwrap_or_default();
                let lines: Vec<&str> = content.lines().collect();
                self.line(&s.dim(&format!("  {} lines", lines.len())));
                if self.full {
                    for line in lines {
                        self.line(&s.green(&format!("  + {line}")));
                    }
                }
            }
            _ if self.full && !summary_is_complete(name) => {
                let pretty = serde_json::to_string_pretty(input).unwrap_or_default();
                for line in pretty.lines() {
                    self.line(&s.dim(&format!("  {line}")));
                }
            }
            _ => {}
        }
        if is_quiet(name) && !self.full {
            self.line("");
        }
    }

    /// A simple line diff: common leading/trailing lines are shown as context,
    /// the middle as removed/added.
    fn diff(&mut self, old: &str, new: &str) {
        let s = self.style;
        let old: Vec<&str> = old.lines().collect();
        let new: Vec<&str> = new.lines().collect();
        let prefix = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
        let suffix = old[prefix..]
            .iter()
            .rev()
            .zip(new[prefix..].iter().rev())
            .take_while(|(a, b)| a == b)
            .count();

        let mut lines = Vec::new();
        let context = |line: &str| s.dim(&format!("    {line}"));
        if prefix > 0 {
            lines.push(context(old[prefix - 1]));
        }
        for line in &old[prefix..old.len() - suffix] {
            lines.push(s.red(&format!("  - {line}")));
        }
        for line in &new[prefix..new.len() - suffix] {
            lines.push(s.green(&format!("  + {line}")));
        }
        if suffix > 0 {
            lines.push(context(old[old.len() - suffix]));
        }

        let limit = if self.full { usize::MAX } else { DIFF_LINES };
        let hidden = lines.len().saturating_sub(limit);
        for line in lines.into_iter().take(limit) {
            self.line(&line);
        }
        if hidden > 0 {
            self.line(&s.dim(&format!("  … {hidden} more diff lines (--detail full)")));
        }
    }

    fn tool_results(&mut self, event: &Value) {
        let s = self.style;
        let Some(content) = event["message"]["content"].as_array() else {
            return;
        };
        for block in content.iter().filter(|b| b["type"] == "tool_result") {
            let text = match &block["content"] {
                Value::String(text) => text.clone(),
                Value::Array(parts) => parts
                    .iter()
                    .filter_map(|p| p["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n"),
                _ => String::new(),
            };
            let text = self.relative(text.trim_end());
            let is_error = block["is_error"].as_bool().unwrap_or(false);
            let lines: Vec<&str> = text.lines().collect();
            let tool = block["tool_use_id"]
                .as_str()
                .and_then(|id| self.tools.get(id))
                .map(String::as_str)
                .unwrap_or_default();

            if self.minimal {
                if is_error {
                    let first = lines.first().copied().unwrap_or("failed");
                    let first = truncate(first, self.width.saturating_sub(4).max(20));
                    self.line(&s.red(&format!("  ✗ {first}")));
                }
                continue;
            }

            // The call already showed what matters for these; only show their
            // result if something went wrong.
            if is_quiet(tool) && !is_error && !self.full {
                continue;
            }

            let limit = match (self.full, is_error) {
                (true, _) => usize::MAX,
                (false, true) => ERROR_LINES,
                (false, false) => RESULT_LINES,
            };
            let max_width = self.width.saturating_sub(4).max(20);
            for (i, line) in lines.iter().take(limit).enumerate() {
                let line = truncate(line, max_width);
                if !is_error {
                    self.line(&s.dim(&format!("  │ {line}")));
                } else if i == 0 {
                    self.line(&s.red(&format!("  ✗ {line}")));
                } else {
                    self.line(&format!("{} {line}", s.red("  │")));
                }
            }
            if lines.len() > limit {
                self.line(&s.dim(&format!(
                    "  │ … {} more lines (--detail full)",
                    lines.len() - limit
                )));
            }
            self.line("");
        }
    }

    fn result(&mut self, event: &Value) {
        let s = self.style;
        self.blank_line();
        let ok = !event["is_error"].as_bool().unwrap_or(false);
        let subtype = event["subtype"].as_str().unwrap_or("done");
        let secs = event["duration_ms"].as_f64().unwrap_or_default();
        let tokens = Tokens::from_usage(&event["usage"]);
        let cost = event["total_cost_usd"].as_f64().unwrap_or_default();
        let summary = format!(
            "{subtype} · {} · {} · ${cost:.2}",
            format_duration(secs),
            tokens.format()
        );
        self.line(&if ok {
            s.green(&format!("✓ {summary}"))
        } else {
            s.red(&format!("✗ {summary}"))
        });

        if let Some(denials) = event["permission_denials"].as_array()
            && !denials.is_empty()
        {
            self.line(&s.yellow(&format!("⚠ {} permission denials:", denials.len())));
            for denial in denials {
                let name = denial["tool_name"].as_str().unwrap_or("tool");
                let what = self.relative(&tool_summary(name, &denial["tool_input"]));
                let what = if self.compact {
                    let (line, more) = self.one_line(&what, name.len() + 3);
                    format!("{line}{more}")
                } else {
                    what.lines().collect::<Vec<_>>().join("\n    ")
                };
                self.line(&s.yellow(&format!("  {name} {what}")));
            }
        }
    }
}

/// Claude Code's mid-blue (`professionalBlue` in its themes), which reads well
/// on both light and dark backgrounds.
fn claude_blue() -> termimad::crossterm::style::Color {
    use termimad::crossterm::style::Color;

    if claude_theme().is_some_and(|theme| theme.ends_with("-ansi")) {
        return Color::Blue;
    }
    let truecolor =
        std::env::var("COLORTERM").is_ok_and(|v| v.contains("truecolor") || v.contains("24bit"));
    if truecolor {
        Color::Rgb {
            r: 106,
            g: 155,
            b: 204,
        }
    } else {
        // Closest color in the 256-color palette.
        Color::AnsiValue(68)
    }
}

/// The `theme` from Claude Code's settings (`dark`, `light`, `dark-ansi`, ...).
fn claude_theme() -> Option<String> {
    let dir = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".claude")))?;
    let text = std::fs::read_to_string(dir.join("settings.json")).ok()?;
    let settings: Value = serde_json::from_str(&text).ok()?;
    settings["theme"].as_str().map(str::to_string)
}

/// A one-glance description of a tool call.
fn tool_summary(name: &str, input: &Value) -> String {
    let field = |key: &str| input[key].as_str().unwrap_or_default().to_string();
    match name {
        "Bash" => field("command"),
        "Read" | "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => field("file_path"),
        "Glob" | "Grep" => match input["path"].as_str() {
            Some(path) => format!("{} in {path}", field("pattern")),
            None => field("pattern"),
        },
        "WebFetch" => field("url"),
        "WebSearch" => field("query"),
        "Task" | "Agent" => field("description"),
        "Skill" => field("skill"),
        "TodoWrite" => input["todos"]
            .as_array()
            .map(|todos| {
                let lines: Vec<String> = todos
                    .iter()
                    .map(|t| {
                        let mark = match t["status"].as_str() {
                            Some("completed") => "☑",
                            Some("in_progress") => "▸",
                            _ => "☐",
                        };
                        format!("{mark} {}", t["content"].as_str().unwrap_or_default())
                    })
                    .collect();
                format!("\n{}", lines.join("\n"))
            })
            .unwrap_or_default(),
        _ => truncate(&input.to_string(), 200),
    }
}

/// Tools whose successful result adds nothing to what the call already shows
/// (a file path, a diff, a todo list), so it's hidden below `--detail full`.
fn is_quiet(name: &str) -> bool {
    matches!(
        name,
        "Read" | "Edit" | "MultiEdit" | "Write" | "NotebookEdit" | "TodoWrite"
    )
}

/// Whether the summary (plus any diff) already shows everything worth seeing,
/// so `--detail full` doesn't need to print the raw input as well.
fn summary_is_complete(name: &str) -> bool {
    matches!(
        name,
        "Bash" | "Read" | "TodoWrite" | "Skill" | "Edit" | "MultiEdit" | "Write"
    )
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde_json::json;

    use super::{Status, TaskLog, format_duration};

    fn task_log(events: Vec<serde_json::Value>) -> TaskLog {
        TaskLog {
            path: PathBuf::from("task-01.jsonl"),
            events,
        }
    }

    #[test]
    fn failed_before_task_fails_the_task() {
        let start = json!({"type": "looper", "event": "start", "task": 1});
        let before =
            |success| json!({"type": "looper", "event": "before_task", "success": success});
        assert!(task_log(vec![start.clone()]).status() == Status::Incomplete);
        assert!(task_log(vec![start.clone(), before(false)]).status() == Status::Failed);
        assert!(task_log(vec![start, before(true)]).status() == Status::Incomplete);
    }

    #[test]
    fn formats_durations_without_milliseconds() {
        assert_eq!(format_duration(0.0), "0s");
        assert_eq!(format_duration(999.0), "0s");
        assert_eq!(format_duration(45_700.0), "45s");
        assert_eq!(format_duration(1_077_100.0), "17m 57s");
        assert_eq!(format_duration(7_213_000.0), "2h 0m 13s");
        assert_eq!(format_duration(97_445_000.0), "1d 3h 4m 5s");
    }
}
