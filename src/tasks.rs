//! `looper tasks list` and `looper tasks show`: read the follow-up task files
//! Claude leaves in `.looper/tasks/` and print them in a human readable form.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result, bail};

use crate::logs::{Cell, Style, page, print_table, terminal_width};
use crate::truncate;

#[derive(Clone, Copy, PartialEq)]
enum Priority {
    High,
    Medium,
    Low,
    Unknown,
}

impl Priority {
    fn parse(text: &str) -> Self {
        match text.to_ascii_uppercase().as_str() {
            "HIGH" => Self::High,
            "MEDIUM" => Self::Medium,
            "LOW" => Self::Low,
            _ => Self::Unknown,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::High => "HIGH",
            Self::Medium => "MEDIUM",
            Self::Low => "LOW",
            Self::Unknown => "-",
        }
    }

    fn styled(self, style: Style) -> String {
        match self {
            Self::High => style.red(self.label()),
            Self::Medium => style.yellow(self.label()),
            Self::Low | Self::Unknown => style.dim(self.label()),
        }
    }
}

/// One follow-up task file: a title, a `Priority: ...` line and a description.
struct TaskFile {
    path: PathBuf,
    title: String,
    priority: Priority,
    /// The file without its title and priority lines.
    body: String,
    created: SystemTime,
}

impl TaskFile {
    fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let created = std::fs::metadata(path)
            .and_then(|m| m.created().or_else(|_| m.modified()))
            .unwrap_or(SystemTime::UNIX_EPOCH);

        let mut title = None;
        let mut priority = Priority::Unknown;
        let mut body = Vec::new();
        for line in text.lines() {
            if title.is_none() && !line.trim().is_empty() {
                title = Some(line.trim().trim_start_matches('#').trim().to_string());
            } else if priority == Priority::Unknown
                && let Some(value) = priority_value(line)
            {
                priority = Priority::parse(value);
            } else {
                body.push(line);
            }
        }
        Ok(Self {
            path: path.to_path_buf(),
            title: title.unwrap_or_else(|| name(path)),
            priority,
            body: body.join("\n").trim().to_string(),
            created,
        })
    }

    fn created_at(&self) -> String {
        chrono::DateTime::<chrono::Local>::from(self.created)
            .format("%Y-%m-%d %H:%M")
            .to_string()
    }
}

/// The value of a `Priority: HIGH` line, also when it's written in markdown
/// like `**Priority:** HIGH` or `- Priority: \`HIGH\``.
fn priority_value(line: &str) -> Option<&str> {
    let (key, value) = line.split_once(':')?;
    clean(key)
        .eq_ignore_ascii_case("priority")
        .then(|| clean(value))
}

/// Strip whitespace and markdown emphasis around `s`.
fn clean(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_whitespace() || "*_`-".contains(c))
}

fn name(path: &Path) -> String {
    path.file_stem()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// All task files in `task_dir`, newest first. `looper tasks show` numbers
/// them in this order.
fn load_all(task_dir: &Path) -> Result<Vec<TaskFile>> {
    let entries = match std::fs::read_dir(task_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(e).with_context(|| format!("failed to read {}", task_dir.display()));
        }
    };
    let mut tasks = entries
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
        .map(|path| TaskFile::load(&path))
        .collect::<Result<Vec<_>>>()?;
    tasks.sort_by(|a, b| (b.created, &b.path).cmp(&(a.created, &a.path)));
    Ok(tasks)
}

pub fn list(task_dir: &Path) -> Result<()> {
    let style = Style::detect();
    let tasks = load_all(task_dir)?;
    if tasks.is_empty() {
        eprintln!("no tasks found in {}", task_dir.display());
        return Ok(());
    }

    let rows: Vec<Vec<Cell>> = tasks
        .iter()
        .enumerate()
        .map(|(i, t)| {
            vec![
                Cell::plain((i + 1).to_string()),
                Cell::styled(t.priority.label(), t.priority.styled(style)),
                Cell::plain(t.created_at()),
                Cell::plain(name(&t.path)),
                Cell::plain(truncate(&t.title, 70)),
            ]
        })
        .collect();
    print_table(style, &["#", "PRIORITY", "CREATED", "FILE", "TITLE"], &rows);
    Ok(())
}

/// Show one task (a number from `looper tasks list`, a file name, or a path),
/// or all of them.
pub fn show(task_dir: &Path, task: Option<&str>, pager: bool) -> Result<()> {
    let tasks = load_all(task_dir)?;
    let style = Style::detect();
    let width = terminal_width();
    let mut out = String::new();
    match task {
        None => {
            if tasks.is_empty() {
                bail!("no tasks found in {}", task_dir.display());
            }
            for (i, t) in tasks.iter().enumerate() {
                if i > 0 {
                    out.push('\n');
                }
                render(&mut out, style, width, Some(i + 1), t);
            }
        }
        Some(task) => match find(&tasks, task) {
            Some(i) => render(&mut out, style, width, Some(i + 1), &tasks[i]),
            None if Path::new(task).is_file() => {
                render(
                    &mut out,
                    style,
                    width,
                    None,
                    &TaskFile::load(Path::new(task))?,
                );
            }
            None => bail!(
                "no task {task} in {} (see `looper tasks list`)",
                task_dir.display()
            ),
        },
    }
    page(&out, pager)
}

/// Index of a task given as a number from `looper tasks list`, a file name or
/// a path.
fn find(tasks: &[TaskFile], task: &str) -> Option<usize> {
    if let Ok(n) = task.parse::<usize>()
        && (1..=tasks.len()).contains(&n)
    {
        return Some(n - 1);
    }
    let stem = task.strip_suffix(".md").unwrap_or(task);
    if let Some(i) = tasks.iter().position(|t| name(&t.path) == stem) {
        return Some(i);
    }
    let path = Path::new(task).canonicalize().ok()?;
    tasks
        .iter()
        .position(|t| t.path.canonicalize().is_ok_and(|p| p == path))
}

fn render(out: &mut String, s: Style, width: usize, n: Option<usize>, t: &TaskFile) {
    let mut line = |text: &str| {
        out.push_str(text);
        out.push('\n');
    };
    let head = n.map_or("Task".to_string(), |n| format!("Task {n}"));
    line(&format!(
        "{} {}  {}",
        s.cyan("╭─"),
        s.bold(&head),
        t.priority.styled(s)
    ));
    line(&format!(
        "{}  {}",
        s.cyan("│"),
        s.bold(&truncate(&t.title, width.saturating_sub(3)))
    ));
    line(&format!(
        "{} {}",
        s.cyan("╰─"),
        s.dim(&format!("{} · {}", t.path.display(), t.created_at()))
    ));
    line("");

    if t.body.is_empty() {
        return;
    }
    if s.color {
        let rendered = termimad::MadSkin::default()
            .text(&t.body, Some(width))
            .to_string();
        out.push_str(&rendered);
        if !rendered.ends_with('\n') {
            out.push('\n');
        }
    } else {
        out.push_str(&t.body);
        out.push('\n');
    }
}

#[cfg(test)]
mod tests {
    use super::priority_value;

    #[test]
    fn reads_priority_lines() {
        assert_eq!(priority_value("Priority: HIGH"), Some("HIGH"));
        assert_eq!(priority_value("**Priority:** medium"), Some("medium"));
        assert_eq!(priority_value("- Priority: `LOW`"), Some("LOW"));
        assert_eq!(priority_value("Note: something"), None);
        assert_eq!(priority_value("no colon here"), None);
    }
}
