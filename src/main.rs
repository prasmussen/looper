use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde::Deserialize;
use serde_json::{Value, json};

use logs::plural;

use status::StatusLine;

mod logs;
mod plans;
mod status;
mod tasks;
mod title;

/// The task text in a fresh `looper plan new` file; `looper plan run` refuses to send it.
const PLACEHOLDER: &str = "REPLACE ME";
const LOOPER_DIR: &str = ".looper";
// Inside the .looper folder.
const PLANS_DIR: &str = "plans";
const TASKS_DIR: &str = "tasks";
const LOGS_DIR: &str = "logs";
const DEFAULTS_FILE: &str = "config.toml";
/// Replaced with the plan's name in the prefix, suffix and tasks.
const PLAN_VAR: &str = "{{plan}}";

/// The top of `.looper/config.toml`, left out of the plans copied from it.
const DEFAULTS_HEADER: &str = "\
# Defaults for new plans: `looper plan new` starts every plan in .looper/plans/
# with a copy of this file. Changing it doesn't change existing plans.

";

/// The first `.looper/config.toml`, after `DEFAULTS_HEADER`. `looper plan new`
/// starts every plan with a copy of that file, followed by `TASKS_TEMPLATE`.
const DEFAULTS_TEMPLATE: &str = r#"# Flags passed to every `claude` call. The prompt is sent on stdin. Each flag
# and each value is its own string; keep a flag and its value on the same line.
claude_args = [
  "-p",
  "--permission-mode", "auto",
]

# Shell command run with `sh -c` before every task, from the project folder,
# with LOOPER_PLAN, LOOPER_TASK, LOOPER_TOTAL and LOOPER_LOG_DIR set. If it
# fails, looper warns and runs the task anyway.
# before_task = "git pull --ff-only"

# Text added before every task. `/goal` makes Claude keep working until the
# task and suffix are done; it must be the very first thing in the prompt.
prefix = """
/goal
"""

# Text added after every task. In the prefix, suffix and tasks, {{plan}} is
# replaced with the plan's name.
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
file in .looper/tasks/{{plan}}/. Keep each file short: a title, a line with
`Priority: LOW`, `Priority: MEDIUM` or `Priority: HIGH`, and a few sentences
describing what needs to be done and why. The .looper/ folder is
git-ignored on purpose; don't commit these files.
"""
"#;

/// The end of every new plan, after the defaults from `.looper/config.toml`.
const TASKS_TEMPLATE: &str = r#"# Each task becomes one `claude` call: prefix + task + suffix.
# Tasks run in order, one at a time. Each task starts with a fresh context,
# without the conversations of earlier tasks, so keep work that needs the same
# context together in one task.
tasks = [
  """
  REPLACE ME
  """,
]
"#;

/// Run claude once per task defined in a plan file, in order.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create, list, show, run and delete the plans in .looper/plans
    Plan {
        #[command(subcommand)]
        command: PlanCmd,
    },

    /// Browse the logs of earlier runs
    Log {
        /// Folder with the logs [default: .looper/logs]
        #[arg(long, global = true)]
        log_dir: Option<PathBuf>,

        #[command(subcommand)]
        command: LogCmd,
    },

    /// Browse the follow-up tasks Claude left in .looper/tasks
    Task {
        /// Folder with the task files [default: .looper/tasks]
        #[arg(long, global = true)]
        task_dir: Option<PathBuf>,

        #[command(subcommand)]
        command: TaskCmd,
    },
}

#[derive(Subcommand)]
enum PlanCmd {
    /// Create a plan in .looper/plans/<NAME>.toml from a template
    New {
        /// Name of the plan
        name: String,
    },

    /// List plans, most recently changed first
    List,

    /// Show a plan's settings and tasks
    Show {
        /// Plan to show: a name from .looper/plans, or a path to a plan file
        plan: String,

        /// Print directly instead of through a pager
        #[arg(long)]
        no_pager: bool,
    },

    /// Run every task in a plan
    Run(RunArgs),

    /// Make a running plan stop once its current task is done
    Stop {
        /// Plan to stop: a name from .looper/plans, or a path to a plan file
        plan: String,
    },

    /// Delete plans
    Delete {
        /// Plans to delete: names from .looper/plans, or paths to plan files
        #[arg(required = true)]
        plans: Vec<String>,
    },
}

#[derive(Subcommand)]
enum TaskCmd {
    /// List tasks, newest first
    List {
        /// Only list the tasks of this plan
        plan: Option<String>,
    },

    /// Show one task or all of them
    Show {
        /// Task to show (a number or file name from `looper task list`, or a
        /// path to a task file) [default: all tasks]
        task: Option<String>,

        /// Print directly instead of through a pager
        #[arg(long)]
        no_pager: bool,
    },

    /// Delete tasks
    Delete {
        /// Tasks to delete (numbers or file names from `looper task list`)
        #[arg(required = true)]
        tasks: Vec<String>,
    },

    /// Delete all tasks, or those of one plan
    Clean {
        /// Only delete the tasks of this plan
        plan: Option<String>,
    },
}

#[derive(Subcommand)]
enum LogCmd {
    /// List runs, newest first, or the tasks of one run
    List {
        /// Run to list the tasks of (a folder name from `looper log list`)
        run: Option<String>,
    },

    /// Show a readable transcript of a run or a single task
    Show {
        /// Run to show (a folder name from `looper log list`, or a path to a
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

    /// Delete the logs of runs
    Delete {
        /// Runs to delete (folder names from `looper log list`)
        #[arg(required = true)]
        runs: Vec<String>,
    },

    /// Delete the logs of all runs
    Clean,
}

#[derive(clap::Args)]
struct RunArgs {
    /// Plan to run: a name from .looper/plans, or a path to a plan file
    plan: String,

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

/// The settings `.looper/config.toml` may hold; everything a plan has except
/// its tasks.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)] // Only parsed to check the file before copying it.
struct Defaults {
    #[serde(default)]
    before_task: String,
    #[serde(default)]
    prefix: String,
    #[serde(default)]
    suffix: String,
    #[serde(default)]
    claude_args: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    #[serde(default)]
    before_task: String,
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

    /// The prompt for one task: prefix + task + suffix, with `{{plan}}`
    /// replaced by `plan`.
    fn prompt(&self, task: &str, plan: &str) -> String {
        [&self.prefix, task, &self.suffix]
            .iter()
            .map(|part| part.trim())
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n")
            .replace(PLAN_VAR, plan)
    }
}

/// The `.looper` folder of the current directory or the nearest parent that
/// has one, as a relative path like `../../.looper`, so paths in messages stay
/// short. Without one, `.looper` in the current directory.
fn find_looper_dir() -> Result<PathBuf> {
    let cwd = std::env::current_dir().context("failed to get the current directory")?;
    let mut path = PathBuf::new();
    for dir in cwd.ancestors() {
        if dir.join(LOOPER_DIR).is_dir() {
            return Ok(path.join(LOOPER_DIR));
        }
        path.push("..");
    }
    Ok(PathBuf::from(LOOPER_DIR))
}

/// The project folder that holds `looper`, where claude runs.
fn project_dir(looper: &Path) -> &Path {
    looper.parent().unwrap_or(Path::new(""))
}

/// The path of plan `name` in .looper/plans.
fn plan_path(looper: &Path, name: &str) -> PathBuf {
    looper.join(PLANS_DIR).join(format!("{name}.toml"))
}

/// The file whose presence asks a run of `plan` to stop after its current
/// task: the plan's path with `.stop` in place of `.toml`.
fn stop_path(plan: &Path) -> PathBuf {
    plan.with_extension("stop")
}

/// Ask the run of `plan`, a name or path, to stop after its current task.
fn stop(looper: &Path, plan: &str) -> Result<()> {
    let plan = &resolve_plan(looper, plan);
    if !plan.is_file() {
        bail!("no plan {} (see `looper plan list`)", plan_name(plan));
    }
    let path = stop_path(plan);
    std::fs::write(&path, "").with_context(|| format!("failed to create {}", path.display()))?;
    eprintln!("{} will stop after its current task", plan_name(plan));
    Ok(())
}

/// The name of a plan: its file name without `.toml`.
fn plan_name(plan: &Path) -> String {
    plan.file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

/// Resolve the plan argument of `looper plan run`, `stop`, `show` and `delete`: an existing file is used as is,
/// anything else is looked up by name in .looper/plans.
fn resolve_plan(looper: &Path, plan: &str) -> PathBuf {
    let path = Path::new(plan);
    if path.is_file() {
        path.to_path_buf()
    } else {
        plan_path(looper, plan)
    }
}

fn new(looper: &Path, name: &str) -> Result<()> {
    if name.is_empty() || name.contains(['/', '\\']) || name.starts_with('.') {
        bail!("invalid plan name {name:?}; use a plain name like `refactor`");
    }
    create_looper_dir(looper)?;
    for dir in [PLANS_DIR, TASKS_DIR, LOGS_DIR] {
        let dir = looper.join(dir);
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create directory {}", dir.display()))?;
    }

    let path = plan_path(looper, name);
    let mut file = match OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {
            bail!("{} already exists", path.display())
        }
        Err(e) => return Err(e).context(format!("failed to create {}", path.display())),
    };
    file.write_all(plan_template(looper)?.as_bytes())?;
    eprintln!("created {}", path.display());
    eprintln!("edit it, then start it with: looper plan run {name}");
    Ok(())
}

/// A new plan: the text of `.looper/config.toml`, comments and all but its
/// header, followed by a placeholder task.
fn plan_template(looper: &Path) -> Result<String> {
    let path = looper.join(DEFAULTS_FILE);
    let defaults = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    toml::from_str::<Defaults>(&defaults)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    let defaults = defaults.strip_prefix(DEFAULTS_HEADER).unwrap_or(&defaults);
    Ok(format!("{}\n\n{TASKS_TEMPLATE}", defaults.trim_end()))
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

/// Run claude without a log, printing its output above the status line.
fn run_plain(mut cmd: Command, prompt: &str, status: &StatusLine) -> Result<ExitStatus> {
    cmd.stdout(Stdio::piped());
    let mut child = spawn_claude(cmd, prompt)?;
    let stdout = child.stdout.take().expect("stdout is piped");
    for line in BufReader::new(stdout).lines() {
        let line = line.context("failed to read claude output")?;
        status.out(&format!("{line}\n"));
    }
    Ok(child.wait()?)
}

/// Tokens and cost of one task, from claude's final `result` event.
#[derive(Default)]
struct TaskStats {
    tokens: logs::Tokens,
    cost: f64,
}

/// Run the plan's `before_task` command with `sh -c`, printing its output
/// (stdout and stderr together, in order) above the status line. Returns how
/// it exited and what it printed.
fn run_before_task(mut cmd: Command, status: &StatusLine) -> Result<(ExitStatus, String)> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .context("failed to run the before_task command with sh")?;
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout is piped"));
    let mut output = String::new();
    let mut buf = Vec::new();
    while stdout
        .read_until(b'\n', &mut buf)
        .context("failed to read before_task output")?
        > 0
    {
        let line = String::from_utf8_lossy(&buf);
        let line = line.trim_end_matches(['\n', '\r']);
        status.out(&format!("{line}\n"));
        output.push_str(line);
        output.push('\n');
        buf.clear();
    }
    Ok((child.wait()?, output))
}

/// Run claude with stream-json output, writing every event to `log` and
/// printing a readable version of the conversation to the terminal. The log
/// already starts with a looper `start` event; this adds an `exit` event at
/// the end, so it records what was asked and how claude exited.
///
/// Models claude reports are added to `models`, and announced the first time
/// each one is seen in the run. The status line's token count follows along
/// as claude replies; `result` then replaces it with the exact total.
fn run_logged(
    mut cmd: Command,
    prompt: &str,
    log: &mut File,
    models: &mut Vec<String>,
    status: &StatusLine,
) -> Result<(ExitStatus, Option<TaskStats>)> {
    cmd.args(["--output-format", "stream-json", "--verbose"])
        .stdout(Stdio::piped());
    let mut child = spawn_claude(cmd, prompt)?;

    let stdout = child.stdout.take().expect("stdout is piped");
    let mut renderer = logs::LiveRenderer::new();
    let mut stats = None;
    // Claude repeats a reply's usage on every content block of it, so count
    // each reply once. Output tokens are only partly known until `result`.
    let mut replies = HashSet::new();
    let mut tokens = logs::Tokens::default();
    for line in BufReader::new(stdout).lines() {
        let line = line.context("failed to read claude output")?;
        writeln!(log, "{line}")?;
        match serde_json::from_str::<Value>(&line) {
            Ok(event) => {
                if event["type"] == "system"
                    && event["subtype"] == "init"
                    && let Some(model) = event["model"].as_str()
                    && !models.iter().any(|m| m == model)
                {
                    status.err(&format!("==> model: {model}"));
                    models.push(model.to_string());
                }
                if event["type"] == "assistant"
                    && let Some(id) = event["message"]["id"].as_str()
                    && replies.insert(id.to_string())
                {
                    tokens += logs::Tokens::from_usage(&event["message"]["usage"]);
                    status.set_tokens(tokens);
                }
                if event["type"] == "result" {
                    let tokens = logs::Tokens::from_usage(&event["usage"]);
                    status.set_tokens(tokens);
                    stats = Some(TaskStats {
                        tokens,
                        cost: event["total_cost_usd"].as_f64().unwrap_or_default(),
                    });
                }
                status.out(renderer.event(&event));
            }
            Err(_) => status.out(&format!("{line}\n")),
        }
    }

    let exit = child.wait()?;
    if let Some(title) = status.generated_title() {
        writeln!(
            log,
            "{}",
            json!({"type": "looper", "event": "title", "title": title})
        )?;
    }
    let event = json!({
        "type": "looper",
        "event": "exit",
        "success": exit.success(),
        "exit_code": exit.code(),
    });
    writeln!(log, "{event}")?;
    Ok((exit, stats))
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
fn create_run_log_dir(looper: &Path, base: &Path, config: &Path) -> Result<PathBuf> {
    let stem = plan_name(config);
    let timestamp = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S");
    create_logs_dir(looper, base)?;

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

/// Create the logs directory, git-ignored so Claude's commits don't pick up
/// the logs: through `.looper/.gitignore` for the default location, or a
/// `.gitignore` of its own for a custom `--log-dir`.
fn create_logs_dir(looper: &Path, dir: &Path) -> Result<()> {
    if dir.starts_with(looper) {
        create_looper_dir(looper)?;
    }
    std::fs::create_dir_all(dir)
        .with_context(|| format!("failed to create log directory {}", dir.display()))?;
    if !dir.starts_with(looper) {
        write_gitignore(dir)?;
    }
    Ok(())
}

/// Create `.looper/` with a `.gitignore` that ignores everything in it, so
/// Claude's commits don't pick up logs or follow-up tasks, and a
/// `config.toml` with the defaults for new plans.
fn create_looper_dir(looper: &Path) -> Result<()> {
    std::fs::create_dir_all(looper)
        .with_context(|| format!("failed to create directory {}", looper.display()))?;
    write_gitignore(looper)?;
    let path = looper.join(DEFAULTS_FILE);
    match OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(mut file) => {
            file.write_all(format!("{DEFAULTS_HEADER}{DEFAULTS_TEMPLATE}").as_bytes())?;
            eprintln!("created {} (defaults for new plans)", path.display());
            Ok(())
        }
        Err(e) if e.kind() == ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e).context(format!("failed to create {}", path.display())),
    }
}

/// Put a `.gitignore` that ignores everything in `dir`, unless one exists.
fn write_gitignore(dir: &Path) -> Result<()> {
    let gitignore = dir.join(".gitignore");
    if !gitignore.exists() {
        std::fs::write(&gitignore, "*\n")
            .with_context(|| format!("failed to create {}", gitignore.display()))?;
    }
    Ok(())
}

fn run(looper: &Path, args: RunArgs) -> Result<()> {
    let plan = resolve_plan(looper, &args.plan);
    let mut config = Config::load(&plan)?;
    if config.tasks.is_empty() {
        bail!("no tasks defined in {}", plan.display());
    }
    if let Some(i) = config.tasks.iter().position(|t| t.contains(PLACEHOLDER))
        && !args.dry_run
    {
        bail!(
            "task {} in {} still says {PLACEHOLDER}; write the task first",
            i + 1,
            plan.display()
        );
    }

    if args.dry_run {
        if !config.before_task.trim().is_empty() {
            eprintln!(
                "==> before each task: sh -c {:?}",
                config.before_task.trim()
            );
        }
        eprintln!("==> claude {} < prompt", config.claude_args.join(" "));
    }

    let name = plan_name(&plan);
    if !args.dry_run {
        // Claude writes follow-ups for this plan to .looper/tasks/<plan>/.
        let dir = looper.join(TASKS_DIR).join(&name);
        create_looper_dir(looper)?;
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create directory {}", dir.display()))?;
    }

    // A stop asked for before this run started isn't meant for it.
    let stop_file = stop_path(&plan);
    if !args.dry_run {
        remove_stop_file(&stop_file)?;
    }

    let run_started = Instant::now();
    let status = StatusLine::start(run_started, (!args.dry_run).then(|| stop_file.clone()));

    let log_dir = match (&args.log_dir, args.no_log || args.dry_run) {
        (_, true) => None,
        (Some(base), false) => Some(create_run_log_dir(looper, base, &plan)?),
        (None, false) => Some(create_run_log_dir(looper, &looper.join(LOGS_DIR), &plan)?),
    };
    if let Some(log_dir) = &log_dir {
        status.err(&format!("==> logging to {}", log_dir.display()));
    }

    let mut failures = Vec::new();
    // The text of every task taken so far, run or not.
    let mut done: Vec<String> = Vec::new();
    let mut ran = 0;
    let mut totals = TaskStats::default();
    let mut models = Vec::new();
    let mut placeholder = false;
    let mut stopped = false;
    let mut total;

    loop {
        // Re-read the plan before every task after the first, so it can be
        // changed while it runs. A plan that doesn't load keeps the last one.
        if !done.is_empty() && !args.dry_run {
            match Config::load(&plan) {
                Ok(new) => config = new,
                Err(err) => status.err(&format!(
                    "==> keeping the plan as it was, since it no longer loads: {err:#}"
                )),
            }
        }
        let remaining = remaining(&config.tasks, &done);
        total = done.len() + remaining.len();
        let Some(task) = remaining.first().map(|t| t.to_string()) else {
            break;
        };
        if !done.is_empty() && !args.dry_run && stop_file.exists() {
            status.err(&format!(
                "==> stopping as asked, with {} left",
                plural(remaining.len(), "task")
            ));
            stopped = true;
            break;
        }
        let n = done.len() + 1;
        done.push(task.clone());
        let task = task.as_str();

        let prompt = config.prompt(task, &name);
        let title = first_line(task);
        if args.dry_run {
            eprintln!("==> [{n}/{total}] {}", truncate(title, 80));
            eprintln!("{prompt}\n");
            continue;
        }

        if task.contains(PLACEHOLDER) {
            status.err(&format!("==> task {n} still says {PLACEHOLDER}; stopping"));
            placeholder = true;
            break;
        }

        status.err(&format!("==> [{n}/{total}] {}", truncate(title, 80)));
        status.set_task(n, total, title, Some(title::generate(task)));
        let task_started = Instant::now();

        let mut log = match &log_dir {
            Some(log_dir) => {
                let log_path = log_dir.join(format!("task-{n:02}.jsonl"));
                let mut log = File::create(&log_path)
                    .with_context(|| format!("failed to create log file {}", log_path.display()))?;
                let header = json!({
                    "type": "looper",
                    "event": "start",
                    "task": n,
                    "total": total,
                    "task_file": plan.display().to_string(),
                    "task_text": task.trim(),
                    "prompt": prompt,
                    "started_at": chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
                });
                writeln!(log, "{header}")?;
                Some(log)
            }
            None => None,
        };

        let before_task = config.before_task.trim();
        if !before_task.is_empty() {
            status.err(&format!(
                "==> before task: {}",
                truncate(first_line(before_task), 80)
            ));
            let mut cmd = Command::new("sh");
            // Send stderr to stdout, so the two stay in order in the output.
            cmd.arg("-c")
                .arg(format!("exec 2>&1\n{before_task}"))
                .env("LOOPER_PLAN", &name)
                .env("LOOPER_TASK", n.to_string())
                .env("LOOPER_TOTAL", total.to_string());
            if let Some(log_dir) = &log_dir {
                // Absolute, since the command runs from the project folder.
                cmd.env("LOOPER_LOG_DIR", std::path::absolute(log_dir)?);
            }
            if !project_dir(looper).as_os_str().is_empty() {
                cmd.current_dir(project_dir(looper));
            }
            let (exit, output) = run_before_task(cmd, &status)?;
            if let Some(log) = &mut log {
                let event = json!({
                    "type": "looper",
                    "event": "before_task",
                    "command": before_task,
                    "output": output,
                    "success": exit.success(),
                    "exit_code": exit.code(),
                });
                writeln!(log, "{event}")?;
            }
            if !exit.success() {
                status.err(&format!(
                    "==> before_task failed on task {n} ({exit}); running the task anyway"
                ));
            }
        }

        let mut cmd = Command::new("claude");
        cmd.args(&config.claude_args);
        // Run from the project folder, so paths like .looper/tasks/ in the
        // prompt point at the right place.
        if !project_dir(looper).as_os_str().is_empty() {
            cmd.current_dir(project_dir(looper));
        }
        let (exit, stats) = match &mut log {
            Some(log) => run_logged(cmd, &prompt, log, &mut models, &status)?,
            None => (run_plain(cmd, &prompt, &status)?, None),
        };
        ran += 1;
        let elapsed = format_elapsed(task_started);
        if let Some(stats) = &stats {
            totals.tokens += stats.tokens;
            totals.cost += stats.cost;
        }

        if exit.success() {
            // With logging, the transcript already ends with claude's own
            // summary line; without it, say how long the task took.
            if stats.is_none() {
                status.err(&format!("==> task {n} done in {elapsed}"));
            }
        } else {
            status.err(&format!(
                "==> claude failed on task {n} after {elapsed} ({exit})"
            ));
            failures.push((n, task.to_string()));
            if args.stop_on_failure {
                status.err(&format!("==> stopping after failure on task {n}"));
                break;
            }
        }
    }

    status.stop();
    if args.dry_run {
        return Ok(());
    }
    remove_stop_file(&stop_file)?;

    let mut summary = format!(
        "==> {} {ran} of {total} tasks in {}",
        if stopped { "stopped after" } else { "finished" },
        format_elapsed(run_started)
    );
    if !models.is_empty() {
        summary.push_str(&format!(" · {}", models.join(", ")));
    }
    if log_dir.is_some() {
        summary.push_str(&format!(
            " · {} · ${:.2}",
            totals.tokens.format(),
            totals.cost
        ));
    }
    if !failures.is_empty() {
        summary.push_str(&format!(" · {} failed", failures.len()));
    }
    status.err(&summary);

    if !failures.is_empty() {
        for (n, task) in &failures {
            status.err(&format!("    ✗ {n}: {}", truncate(first_line(task), 80)));
        }
    }
    if !failures.is_empty() || placeholder {
        std::process::exit(1);
    }

    Ok(())
}

fn remove_stop_file(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != ErrorKind::NotFound => {
            Err(e).with_context(|| format!("failed to delete {}", path.display()))
        }
        _ => Ok(()),
    }
}

/// The tasks of `tasks` that haven't been taken yet, in plan order. Each task
/// in `done` accounts for one task with the same text, so a task listed twice
/// runs twice, and tasks added, removed or moved anywhere in the plan are
/// picked up.
fn remaining<'a>(tasks: &'a [String], done: &[String]) -> Vec<&'a String> {
    let mut done: Vec<&str> = done.iter().map(|t| t.trim()).collect();
    tasks
        .iter()
        .filter(|task| match done.iter().position(|d| *d == task.trim()) {
            Some(i) => {
                done.swap_remove(i);
                false
            }
            None => true,
        })
        .collect()
}

fn format_elapsed(started: Instant) -> String {
    logs::format_duration(started.elapsed().as_millis() as f64)
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let looper = find_looper_dir()?;
    let looper = looper.as_path();
    match cli.command {
        Cmd::Plan { command } => match command {
            PlanCmd::New { name } => new(looper, &name),
            PlanCmd::List => plans::list(&looper.join(PLANS_DIR)),
            PlanCmd::Show { plan, no_pager } => {
                plans::show(&resolve_plan(looper, &plan), !no_pager)
            }
            PlanCmd::Run(args) => run(looper, args),
            PlanCmd::Stop { plan } => stop(looper, &plan),
            PlanCmd::Delete { plans } => plans::delete(looper, &plans),
        },
        Cmd::Log { log_dir, command } => {
            let log_dir = log_dir.unwrap_or_else(|| looper.join(LOGS_DIR));
            match command {
                LogCmd::List { run } => logs::list(&log_dir, run.as_deref()),
                LogCmd::Show {
                    run,
                    task,
                    detail,
                    no_pager,
                } => logs::show(&log_dir, run.as_deref(), task, detail, !no_pager),
                LogCmd::Delete { runs } => logs::delete(&log_dir, &runs),
                LogCmd::Clean => logs::clean(&log_dir),
            }
        }
        Cmd::Task { task_dir, command } => {
            let task_dir = task_dir.unwrap_or_else(|| looper.join(TASKS_DIR));
            match command {
                TaskCmd::List { plan } => tasks::list(&task_dir, plan.as_deref()),
                TaskCmd::Delete { tasks } => tasks::delete(&task_dir, &tasks),
                TaskCmd::Clean { plan } => tasks::clean(&task_dir, plan.as_deref()),
                TaskCmd::Show { task, no_pager } => {
                    tasks::show(&task_dir, task.as_deref(), !no_pager)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Config, remaining};

    fn strings(tasks: &[&str]) -> Vec<String> {
        tasks.iter().map(|t| t.to_string()).collect()
    }

    #[test]
    fn remaining_skips_done_tasks_wherever_they_are() {
        let tasks = strings(&["new", "b", "a", "c"]);
        let done = strings(&["a", "b"]);
        assert_eq!(remaining(&tasks, &done), ["new", "c"]);
    }

    #[test]
    fn remaining_counts_repeated_tasks() {
        let tasks = strings(&["a", "a", "b"]);
        assert_eq!(remaining(&tasks, &strings(&["a"])), ["a", "b"]);
        assert_eq!(remaining(&tasks, &strings(&["a", "a"])), ["b"]);
    }

    #[test]
    fn remaining_ignores_surrounding_whitespace() {
        let tasks = strings(&["  a\n", "b"]);
        assert_eq!(remaining(&tasks, &strings(&["a"])), ["b"]);
    }

    #[test]
    fn prompt_replaces_plan() {
        let config = Config {
            before_task: String::new(),
            prefix: "/goal".into(),
            suffix: "Write follow-ups to .looper/tasks/{{plan}}/.".into(),
            claude_args: Vec::new(),
            tasks: Vec::new(),
        };
        assert_eq!(
            config.prompt("  Do {{plan}} things.\n", "refactor"),
            "/goal\n\nDo refactor things.\n\nWrite follow-ups to .looper/tasks/refactor/."
        );
    }
}
