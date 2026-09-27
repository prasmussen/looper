//! `looper plans list`: show the plan files in `.looper/plans/`.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};

use crate::logs::{Cell, Style, print_table};
use crate::{Config, first_line, truncate};

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
