use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde::Deserialize;
use serde_json::Value;

const CONFIG_FILE: &str = "looper.toml";

const TEMPLATE: &str = r#"# Flags passed to every `claude` call. The prompt is sent on stdin. Each flag
# and each value is its own string; keep a flag and its value on the same line.
claude_args = [
  "-p",
  "--remote-control",
  "--permission-mode", "auto",
]

# Text added before every task.
prefix = """
"""

# Text added after every task.
suffix = """
Commit your work in several small, focused git commits as you go, rather
than one big commit at the end. If the project has a formatter (e.g.
cargo fmt, prettier, gofmt, ruff format), format the files you changed
before each commit.

When you are done, run the tests and make sure they pass.

Then, if there are any gaps or follow-ups, create one markdown
file per item in docs/tasks/. Keep each file short: a title, a line with
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
    /// Create a looper.toml template in the current directory
    New,

    /// Run every task in a looper.toml
    Run(RunArgs),
}

#[derive(clap::Args)]
struct RunArgs {
    /// Path to the task file
    #[arg(default_value = CONFIG_FILE)]
    config: PathBuf,

    /// Save the full transcript of each run (messages, tool calls, results) as
    /// `<LOG_DIR>/task-NN.jsonl`
    #[arg(long)]
    log_dir: Option<PathBuf>,

    /// Keep going when a claude invocation fails instead of stopping
    #[arg(long)]
    keep_going: bool,

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

fn new() -> Result<()> {
    let mut file = match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(CONFIG_FILE)
    {
        Ok(file) => file,
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {
            bail!("{CONFIG_FILE} already exists in this directory")
        }
        Err(e) => return Err(e).context(format!("failed to create {CONFIG_FILE}")),
    };
    file.write_all(TEMPLATE.as_bytes())?;
    eprintln!("created {CONFIG_FILE}");
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
/// printing a readable version of the conversation to the terminal.
fn run_logged(mut cmd: Command, prompt: &str, log_path: &Path) -> Result<ExitStatus> {
    let mut log = File::create(log_path)
        .with_context(|| format!("failed to create log file {}", log_path.display()))?;

    cmd.args(["--output-format", "stream-json", "--verbose"])
        .stdout(Stdio::piped());
    let mut child = spawn_claude(cmd, prompt)?;

    let stdout = child.stdout.take().expect("stdout is piped");
    for line in BufReader::new(stdout).lines() {
        let line = line.context("failed to read claude output")?;
        writeln!(log, "{line}")?;
        match serde_json::from_str::<Value>(&line) {
            Ok(event) => print_event(&event),
            Err(_) => println!("{line}"),
        }
    }

    Ok(child.wait()?)
}

fn print_event(event: &Value) {
    match event["type"].as_str() {
        Some("assistant") => {
            let Some(content) = event["message"]["content"].as_array() else {
                return;
            };
            for block in content {
                match block["type"].as_str() {
                    Some("text") => println!("{}", block["text"].as_str().unwrap_or_default()),
                    Some("tool_use") => println!(
                        "  -> {} {}",
                        block["name"].as_str().unwrap_or("tool"),
                        truncate(&block["input"].to_string(), 120)
                    ),
                    _ => {}
                }
            }
        }
        Some("result") => {
            let cost = event["total_cost_usd"].as_f64().unwrap_or_default();
            let secs = event["duration_ms"].as_f64().unwrap_or_default() / 1000.0;
            let turns = event["num_turns"].as_u64().unwrap_or_default();
            eprintln!("==> done in {secs:.1}s, {turns} turns, ${cost:.4}");
        }
        _ => {}
    }
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

fn run(args: RunArgs) -> Result<()> {
    let config = Config::load(&args.config)?;
    if config.tasks.is_empty() {
        bail!("no tasks defined in {}", args.config.display());
    }

    if let Some(log_dir) = &args.log_dir {
        std::fs::create_dir_all(log_dir)
            .with_context(|| format!("failed to create log directory {}", log_dir.display()))?;
    }

    if args.dry_run {
        eprintln!("==> claude {} < prompt", config.claude_args.join(" "));
    }

    let total = config.tasks.len();
    let mut failures = Vec::new();

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
        let status = match &args.log_dir {
            Some(log_dir) => {
                let log_path = log_dir.join(format!("task-{n:02}.jsonl"));
                eprintln!("==> logging to {}", log_path.display());
                run_logged(cmd, &prompt, &log_path)?
            }
            None => spawn_claude(cmd, &prompt)?.wait()?,
        };

        if !status.success() {
            eprintln!("==> claude failed on task {n} ({status})");
            if !args.keep_going {
                bail!("stopping after failure on task {n}");
            }
            failures.push((n, task));
        }
    }

    if !failures.is_empty() {
        eprintln!("==> {} of {total} failed:", failures.len());
        for (n, task) in &failures {
            eprintln!("    {n}: {}", truncate(first_line(task), 80));
        }
        std::process::exit(1);
    }

    Ok(())
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Cmd::New => new(),
        Cmd::Run(args) => run(args),
    }
}
