//! `looper plans list` and `looper plans show`: show the plan files in
//! `.looper/plans/`.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};

use crate::logs::{Cell, Style, page, plural, print_table};
use crate::{Config, PLAN_VAR, first_line, plan_name, truncate};

/// One plan file, with its tasks if it parses.
struct PlanFile {
    path: PathBuf,
    modified: SystemTime,
    config: Result<Config>,
}

/// All plan files in `plan_dir`, most recently changed first.
fn load_all(plan_dir: &Path) -> Result<Vec<PlanFile>> {
    let entries = match std::fs::read_dir(plan_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(e).with_context(|| format!("failed to read {}", plan_dir.display()));
        }
    };
    let mut plans: Vec<PlanFile> = entries
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
        .map(|path| PlanFile {
            modified: std::fs::metadata(&path)
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH),
            config: Config::load(&path),
            path,
        })
        .collect();
    plans.sort_by(|a, b| (b.modified, &b.path).cmp(&(a.modified, &a.path)));
    Ok(plans)
}

pub fn list(plan_dir: &Path) -> Result<()> {
    let style = Style::detect();
    let plans = load_all(plan_dir)?;
    if plans.is_empty() {
        eprintln!("no plans found in {}", plan_dir.display());
        return Ok(());
    }

    let rows: Vec<Vec<Cell>> = plans
        .iter()
        .map(|p| {
            let name = p.path.file_stem().unwrap_or_default().to_string_lossy();
            let modified = chrono::DateTime::<chrono::Local>::from(p.modified)
                .format("%Y-%m-%d %H:%M")
                .to_string();
            let (tasks, first) = match &p.config {
                Ok(config) => (
                    Cell::plain(config.tasks.len().to_string()),
                    Cell::plain(truncate(
                        config.tasks.first().map_or("", |t| first_line(t)),
                        70,
                    )),
                ),
                Err(_) => (
                    Cell::styled("-", style.red("-")),
                    Cell::styled("invalid plan file", style.red("invalid plan file")),
                ),
            };
            vec![
                Cell::plain(name.into_owned()),
                tasks,
                Cell::plain(modified),
                first,
            ]
        })
        .collect();
    print_table(style, &["NAME", "TASKS", "MODIFIED", "FIRST TASK"], &rows);
    Ok(())
}

/// Show a plan: its claude flags, then the prefix, each task and the suffix,
/// with `{{plan}}` replaced as in the prompts claude gets.
pub fn show(plan: &Path, pager: bool) -> Result<()> {
    let config = Config::load(plan)?;
    let s = Style::detect();
    let modified = std::fs::metadata(plan)
        .and_then(|m| m.modified())
        .map(|t| {
            chrono::DateTime::<chrono::Local>::from(t)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_default();

    let mut out = String::new();
    let mut line = |text: &str| {
        out.push_str(text);
        out.push('\n');
    };
    let name = plan_name(plan);
    line(&format!(
        "{} {}  {}",
        s.cyan("╭─"),
        s.bold(&format!("Plan {name}")),
        s.dim(&plural(config.tasks.len(), "task"))
    ));
    line(&format!(
        "{}  claude {}",
        s.cyan("│"),
        config.claude_args.join(" ")
    ));
    line(&format!(
        "{} {}",
        s.cyan("╰─"),
        s.dim(&format!("{} · {modified}", plan.display()))
    ));

    let mut section = |title: &str, text: &str| {
        line("");
        line(&s.cyan(&format!("── {title}")));
        let text = text.trim().replace(PLAN_VAR, &name);
        if text.is_empty() {
            line(&s.dim("(empty)"));
        } else {
            line(&text);
        }
    };
    section("Prefix", &config.prefix);
    for (i, task) in config.tasks.iter().enumerate() {
        section(&format!("Task {}", i + 1), task);
    }
    section("Suffix", &config.suffix);
    page(&out, pager)
}
