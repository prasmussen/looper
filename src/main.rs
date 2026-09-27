use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde::Deserialize;
use serde_json::{Value, json};

mod logs;

const CONFIG_FILE: &str = "looper.toml";
const TASKS_DIR: &str = ".looper/tasks";
const LOGS_DIR: &str = ".looper/logs";

const TEMPLATE: &str = r#"# Flags passed to every `claude` call. The prompt is sent on stdin. Each flag
# and each value is its own string; keep a flag and its value on the same line.
claude_args = [
  "-p",
  "--remote-control",
  "--permission-mode", "auto",
]

# Text added before every task. `/goal` makes Claude keep working until the
# task and suffix are done; it must be the very first thing in the prompt.
prefix = """
/goal
"""

# Text added after every task.
suffix = """
Nobody is available to answer questions while you work. If a question comes
up, don't ask it; go with what you would have suggested and continue.

Commit your work in several small, focused git commits as you go, rather
than one big commit at the end. If the project has a formatter (e.g.
cargo fmt, prettier, gofmt, ruff format), format the files you changed
before each commit.

When you are done, look for gaps or follow-ups. If one is small enough and
fits within this task, complete it straight away instead of leaving it for
later.

Then run the tests and make sure they pass.

Finally, for each gap or follow-up you did not complete, create one markdown
file in .looper/tasks/. Keep each file short: a title, a line with
`Priority: LOW`, `Priority: MEDIUM` or `Priority: HIGH`, and a few sentences
describing what needs to be done and why.
"""

# Each task becomes one `claude` call: prefix + task + suffix.
# Tasks run in order, one at a time.
tasks = [
  """
  Add a --verbose flag to the CLI.
  """,
  """
  Write a README that explains how to install and use the tool.
  """,
]
"#;

/// Run claude once per task defined in a looper.toml, in order.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create a task file from a template
    New {
        /// Path of the file to create
        #[arg(default_value = CONFIG_FILE)]
        path: PathBuf,
    },

    /// Run every task in a looper.toml
    Run(RunArgs),

    /// Browse the logs of earlier runs
    Logs {
        /// Folder with the logs
        #[arg(long, global = true, default_value = LOGS_DIR)]
        log_dir: PathBuf,

        #[command(subcommand)]
        command: LogsCmd,
    },
}

#[derive(Subcommand)]
enum LogsCmd {
    /// List runs, newest first, or the tasks of one run
    List {
        /// Run to list the tasks of (a folder name from `looper logs list`)
        run: Option<String>,
    },

    /// Show a readable transcript of a run or a single task
    Show {
        /// Run to show (a folder name from `looper logs list`, or a path to a
        /// run folder or .jsonl file) [default: the latest run]
        run: Option<String>,

        /// Only show this task number
        #[arg(long)]
        task: Option<usize>,

        /// How much to show
        #[arg(long, value_enum, default_value_t)]
        detail: logs::Detail,

        /// Print directly instead of through a pager
        #[arg(long)]
        no_pager: bool,
    },
}

#[derive(clap::Args)]
struct RunArgs {
    /// Path to the task file
    #[arg(default_value = CONFIG_FILE)]
    config: PathBuf,

    /// Where to save the full transcript of each task (messages, tool calls,
    /// results). Each run gets its own folder inside it
    /// [default: .looper/logs]
    #[arg(long, conflicts_with = "no_log")]
    log_dir: Option<PathBuf>,

    /// Don't save transcripts; only show claude's final reply for each task
    #[arg(long)]
    no_log: bool,

    /// Stop at the first task that fails instead of continuing with the rest
    #[arg(long)]
    stop_on_failure: bool,

    /// Print the prompts without running them
    #[arg(long)]
    dry_run: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    #[serde(default)]
    prefix: String,
    #[serde(default)]
    suffix: String,
    #[serde(default)]
    claude_args: Vec<String>,
    tasks: Vec<String>,
}

impl Config {
    fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("failed to parse {}", path.display()))
    }

    fn prompt(&self, task: &str) -> String {
        [&self.prefix, task, &self.suffix]
            .iter()
            .map(|part| part.trim())
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

fn new(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create directory {}", parent.display()))?;
    }
    let mut file = match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {
            bail!("{} already exists", path.display())
        }
        Err(e) => return Err(e).context(format!("failed to create {}", path.display())),
    };
    file.write_all(TEMPLATE.as_bytes())?;
    eprintln!("created {}", path.display());

    std::fs::create_dir_all(TASKS_DIR)
        .with_context(|| format!("failed to create directory {TASKS_DIR}"))?;
    create_logs_dir(Path::new(LOGS_DIR))?;
    eprintln!("created {TASKS_DIR}/ and {LOGS_DIR}/");
    Ok(())
}

/// Start claude and send it the prompt on stdin. The prompt isn't passed as an
/// argument because a flag with an optional value (like `--remote-control`)
/// would swallow it.
fn spawn_claude(mut cmd: Command, prompt: &str) -> Result<Child> {
    let mut child = cmd
        .stdin(Stdio::piped())
        .spawn()
        .context("failed to run claude (is it on your PATH?)")?;
    let mut stdin = child.stdin.take().expect("stdin is piped");
    stdin
        .write_all(prompt.as_bytes())
        .context("failed to send the prompt to claude")?;
    Ok(child)
}

/// Run claude with stream-json output, writing every event to `log_path` and
/// printing a readable version of the conversation to the terminal. The log
/// starts with a looper `start` event (`header`) and ends with an `exit` event,
/// so it records what was asked and how claude exited.
/// Turns and cost of one task, from claude's final `result` event.
#[derive(Default)]
struct TaskStats {
    turns: u64,
    cost: f64,
}

fn run_logged(
    mut cmd: Command,
    prompt: &str,
    header: Value,
    log_path: &Path,
) -> Result<(ExitStatus, Option<TaskStats>)> {
    let mut log = File::create(log_path)
        .with_context(|| format!("failed to create log file {}", log_path.display()))?;
    writeln!(log, "{header}")?;

    cmd.args(["--output-format", "stream-json", "--verbose"])
        .stdout(Stdio::piped());
    let mut child = spawn_claude(cmd, prompt)?;

    let stdout = child.stdout.take().expect("stdout is piped");
    let mut renderer = logs::LiveRenderer::new();
    let mut stats = None;
    for line in BufReader::new(stdout).lines() {
        let line = line.context("failed to read claude output")?;
        writeln!(log, "{line}")?;
        match serde_json::from_str::<Value>(&line) {
            Ok(event) => {
                if event["type"] == "result" {
                    stats = Some(TaskStats {
                        turns: event["num_turns"].as_u64().unwrap_or_default(),
                        cost: event["total_cost_usd"].as_f64().unwrap_or_default(),
                    });
                }
                renderer.event(&event);
            }
            Err(_) => println!("{line}"),
        }
    }

    let status = child.wait()?;
    let exit = json!({
        "type": "looper",
        "event": "exit",
        "success": status.success(),
        "exit_code": status.code(),
    });
    writeln!(log, "{exit}")?;
    Ok((status, stats))
}

fn truncate(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

fn first_line(s: &str) -> &str {
    s.trim().lines().next().unwrap_or_default()
}

/// Create `<base>/<task file stem>-<timestamp>/`, so runs never overwrite each
/// other.
fn create_run_log_dir(base: &Path, config: &Path) -> Result<PathBuf> {
    let stem = config.file_stem().unwrap_or_default().to_string_lossy();
    let timestamp = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S");
    create_logs_dir(base)?;

    // Add -2, -3, ... if a run already started in the same second.
    let mut dir = base.join(format!("{stem}-{timestamp}"));
    let mut n = 1;
    loop {
        match std::fs::create_dir(&dir) {
            Ok(()) => break,
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                n += 1;
                dir = base.join(format!("{stem}-{timestamp}-{n}"));
            }
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("failed to create log directory {}", dir.display()));
            }
        }
    }
    Ok(dir)
}

/// Create the logs directory with a `.gitignore` in it, so Claude's commits
/// don't pick up the logs.
fn create_logs_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("failed to create log directory {}", dir.display()))?;
    let gitignore = dir.join(".gitignore");
    if !gitignore.exists() {
        std::fs::write(&gitignore, "*\n")
            .with_context(|| format!("failed to create {}", gitignore.display()))?;
    }
    Ok(())
}

fn run(args: RunArgs) -> Result<()> {
    let config = Config::load(&args.config)?;
    if config.tasks.is_empty() {
        bail!("no tasks defined in {}", args.config.display());
    }

    if args.dry_run {
        eprintln!("==> claude {} < prompt", config.claude_args.join(" "));
    }

    let log_dir = match (&args.log_dir, args.no_log || args.dry_run) {
        (_, true) => None,
        (Some(base), false) => Some(create_run_log_dir(base, &args.config)?),
        (None, false) => Some(create_run_log_dir(Path::new(LOGS_DIR), &args.config)?),
    };
    if let Some(log_dir) = &log_dir {
        eprintln!("==> logging to {}", log_dir.display());
    }

    let total = config.tasks.len();
    let mut failures = Vec::new();
    let mut ran = 0;
    let mut totals = TaskStats::default();
    let run_started = Instant::now();

    for (i, task) in config.tasks.iter().enumerate() {
        let n = i + 1;
        let prompt = config.prompt(task);
        eprintln!("==> [{n}/{total}] {}", truncate(first_line(task), 80));

        if args.dry_run {
            eprintln!("{prompt}\n");
            continue;
        }

        let mut cmd = Command::new("claude");
        cmd.args(&config.claude_args);
        let task_started = Instant::now();
        let (status, stats) = match &log_dir {
            Some(log_dir) => {
                let log_path = log_dir.join(format!("task-{n:02}.jsonl"));
                let header = json!({
                    "type": "looper",
                    "event": "start",
                    "task": n,
                    "total": total,
                    "task_file": args.config.display().to_string(),
                    "task_text": task.trim(),
                    "prompt": prompt,
                    "started_at": chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
                });
                run_logged(cmd, &prompt, header, &log_path)?
            }
            None => (spawn_claude(cmd, &prompt)?.wait()?, None),
        };
        ran += 1;
        let elapsed = format_elapsed(task_started);
        if let Some(stats) = &stats {
            totals.turns += stats.turns;
            totals.cost += stats.cost;
        }

        if status.success() {
            // With logging, the transcript already ends with claude's own
            // summary line; without it, say how long the task took.
            if stats.is_none() {
                eprintln!("==> task {n} done in {elapsed}");
            }
        } else {
            eprintln!("==> claude failed on task {n} after {elapsed} ({status})");
            failures.push((n, task));
            if args.stop_on_failure {
                eprintln!("==> stopping after failure on task {n}");
                break;
            }
        }
    }

    if args.dry_run {
        return Ok(());
    }

    let mut summary = format!(
        "==> finished {ran} of {total} tasks in {}",
        format_elapsed(run_started)
    );
    if log_dir.is_some() {
        summary.push_str(&format!(
            " · {} · ${:.2}",
            logs::format_turns(totals.turns),
            totals.cost
        ));
    }
    if !failures.is_empty() {
        summary.push_str(&format!(" · {} failed", failures.len()));
    }
    eprintln!("{summary}");

    if !failures.is_empty() {
        for (n, task) in &failures {
            eprintln!("    ✗ {n}: {}", truncate(first_line(task), 80));
        }
        std::process::exit(1);
    }

    Ok(())
}

fn format_elapsed(started: Instant) -> String {
    logs::format_duration(started.elapsed().as_millis() as f64)
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Cmd::New { path } => new(&path),
        Cmd::Run(args) => run(args),
        Cmd::Logs { log_dir, command } => match command {
            LogsCmd::List { run } => logs::list(&log_dir, run.as_deref()),
            LogsCmd::Show {
                run,
                task,
                detail,
                no_pager,
            } => logs::show(&log_dir, run.as_deref(), task, detail, !no_pager),
        },
    }
}
