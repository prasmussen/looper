//! `looper logs list` and `looper logs show`: read the `.jsonl` transcripts
//! written by `looper run` and print them in a human readable form.

use std::collections::HashMap;
use std::io::{IsTerminal, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::{first_line, truncate};

/// Tool output lines shown per tool call unless `--full` is given.
const RESULT_LINES: usize = 8;
/// Diff lines shown per edit unless `--full` is given.
const DIFF_LINES: usize = 30;
/// Error output lines shown per tool call unless `--full` is given.
const ERROR_LINES: usize = 20;

/// ANSI styling that switches itself off when stdout isn't a terminal or
/// `NO_COLOR` is set, and on regardless when `CLICOLOR_FORCE` is set.
#[derive(Clone, Copy)]
struct Style {
    color: bool,
}

impl Style {
    fn detect() -> Self {
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

    fn bold(self, t: &str) -> String {
        self.paint("1", t)
    }
    fn dim(self, t: &str) -> String {
        self.paint("2", t)
    }
    fn red(self, t: &str) -> String {
        self.paint("31", t)
    }
    fn green(self, t: &str) -> String {
        self.paint("32", t)
    }
    fn yellow(self, t: &str) -> String {
        self.paint("33", t)
    }
    fn cyan(self, t: &str) -> String {
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

    fn title(&self) -> String {
        self.start()
            .and_then(|s| s["task_text"].as_str())
            .map(|t| truncate(first_line(t), 70))
            .unwrap_or_else(|| "(unknown task)".into())
    }

    fn status(&self) -> Status {
        let exit_ok = self.exit().map(|e| e["success"].as_bool().unwrap_or(false));
        let result_ok = self
            .result()
            .map(|r| !r["is_error"].as_bool().unwrap_or(false));
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

    fn turns(&self) -> u64 {
        self.result()
            .and_then(|r| r["num_turns"].as_u64())
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
        "no run named {run} in {} (see `looper logs list`)",
        log_dir.display()
    )
}

fn run_name(run_dir: &Path) -> String {
    run_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| run_dir.display().to_string())
}

fn format_duration(ms: f64) -> String {
    let secs = (ms / 1000.0).round() as u64;
    match secs {
        0..60 => format!("{:.1}s", ms / 1000.0),
        60..3600 => format!("{}m {:02}s", secs / 60, secs % 60),
        _ => format!("{}h {:02}m", secs / 3600, secs % 3600 / 60),
    }
}

fn terminal_width() -> usize {
    (termimad::terminal_size().0 as usize).clamp(40, 120)
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
                Cell::plain(t.title()),
            ]
        })
        .collect();
    println!("{}\n", style.bold(&run_dir.display().to_string()));
    print_table(style, &["#", "STATUS", "TIME", "COST", "TASK"], &rows);
    Ok(())
}

/// A table cell: its visible text (for measuring) and what to print.
struct Cell {
    plain: String,
    styled: String,
}

impl Cell {
    fn plain(text: impl Into<String>) -> Self {
        let text = text.into();
        Self {
            styled: text.clone(),
            plain: text,
        }
    }

    fn styled(plain: impl Into<String>, styled: String) -> Self {
        Self {
            plain: plain.into(),
            styled,
        }
    }
}

fn print_table(style: Style, header: &[&str], rows: &[Vec<Cell>]) {
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
    full: bool,
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

    let mut r = Renderer {
        out: String::new(),
        style: Style::detect(),
        width: terminal_width(),
        full,
        cwd: None,
        tools: HashMap::new(),
    };
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
fn page(text: &str, pager: bool) -> Result<()> {
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

struct Renderer {
    out: String,
    style: Style,
    width: usize,
    full: bool,
    /// Claude's working directory for the current task, used to shorten paths.
    cwd: Option<String>,
    /// Tool name by tool_use id, to know what a tool result belongs to.
    tools: HashMap<String, String>,
}

impl Renderer {
    fn line(&mut self, text: &str) {
        self.out.push_str(text);
        self.out.push('\n');
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
            stats.push(format!("{} turns", log.turns()));
            stats.push(format!("${:.2}", log.cost()));
        }
        let head = format!("Task {}{total}", log.number());
        self.line(&format!(
            "{} {}  {}",
            s.cyan("╭─"),
            s.bold(&head),
            stats.join(&s.dim(" · "))
        ));
        let title = log
            .start()
            .and_then(|st| st["task_text"].as_str())
            .map(|t| truncate(first_line(t), self.width.saturating_sub(3)))
            .unwrap_or_else(|| log.title());
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
                self.markdown(text.trim());
                self.line("");
            }
        }

        self.section("transcript");
        for event in &log.events {
            match event["type"].as_str() {
                Some("assistant") => self.assistant(event),
                Some("user") => self.tool_results(event),
                Some("result") => self.result(event),
                _ => {}
            }
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
    fn markdown(&mut self, text: &str) {
        if self.style.color {
            let skin = termimad::MadSkin::default();
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
                        self.markdown(text);
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
        let mut lines = summary.lines();
        let first = truncate(
            lines.next().unwrap_or_default(),
            self.width.saturating_sub(name.len() + 3).max(20),
        );
        self.line(&format!("{} {} {first}", s.cyan("⏺"), s.bold(name)));
        for line in lines {
            self.line(&format!("  {line}"));
        }
        if let Some(desc) = input["description"].as_str()
            && name == "Bash"
        {
            self.line(&s.dim(&format!("  # {desc}")));
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
            self.line(&s.dim(&format!("  … {hidden} more diff lines (--full)")));
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
                    "  │ … {} more lines (--full)",
                    lines.len() - limit
                )));
            }
            self.line("");
        }
    }

    fn result(&mut self, event: &Value) {
        let s = self.style;
        let ok = !event["is_error"].as_bool().unwrap_or(false);
        let subtype = event["subtype"].as_str().unwrap_or("done");
        let secs = event["duration_ms"].as_f64().unwrap_or_default();
        let turns = event["num_turns"].as_u64().unwrap_or_default();
        let cost = event["total_cost_usd"].as_f64().unwrap_or_default();
        let summary = format!(
            "{subtype} · {} · {turns} turns · ${cost:.4}",
            format_duration(secs)
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
                let what = truncate(&tool_summary(name, &denial["tool_input"]), 100);
                self.line(&s.yellow(&format!("  {name} {what}")));
            }
        }
    }
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
/// (a file path, a diff, a todo list), so it's hidden unless `--full`.
fn is_quiet(name: &str) -> bool {
    matches!(
        name,
        "Read" | "Edit" | "MultiEdit" | "Write" | "NotebookEdit" | "TodoWrite"
    )
}

/// Whether the summary (plus any diff) already shows everything worth seeing,
/// so `--full` doesn't need to print the raw input as well.
fn summary_is_complete(name: &str) -> bool {
    matches!(
        name,
        "Bash" | "Read" | "TodoWrite" | "Skill" | "Edit" | "MultiEdit" | "Write"
    )
}
