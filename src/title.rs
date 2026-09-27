//! Short task titles written by a small, fast model, for the status line and
//! the logs. A task's first line often says little ("fix these issues:"), so
//! looper asks for a title while the task runs and uses it once it arrives.

use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};

const MODEL: &str = "haiku";
/// Longest title accepted; anything longer is likely not a title at all.
const MAX_CHARS: usize = 60;
/// How much of the task text to send; the start says what it's about.
const MAX_TASK_CHARS: usize = 4000;

/// Ask for a title for `task` in the background. The receiver gets one title,
/// or nothing if the call fails or the reply doesn't look like a title.
pub fn generate(task: &str) -> Receiver<String> {
    let (tx, rx) = mpsc::channel();
    let task: String = task.trim().chars().take(MAX_TASK_CHARS).collect();
    std::thread::spawn(move || {
        if let Some(title) = ask(&task) {
            let _ = tx.send(title);
        }
    });
    rx
}

fn ask(task: &str) -> Option<String> {
    let prompt = format!(
        "Write a short title (3 to 7 words) for the task below. Reply with the \
         title only, no quotes or punctuation at the end.\n\n<task>\n{task}\n</task>\n"
    );
    let mut child = Command::new("claude")
        .args(["-p", "--model", MODEL, "--tools", ""])
        .args(["--no-session-persistence", "--setting-sources", ""])
        .arg("--strict-mcp-config")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    child.stdin.take()?.write_all(prompt.as_bytes()).ok()?;
    let output = child.wait_with_output().ok()?;
    if !output.status.success() {
        return None;
    }
    clean(&String::from_utf8_lossy(&output.stdout))
}

/// The title in a reply, without markdown or quotes around it.
fn clean(reply: &str) -> Option<String> {
    let line = reply.lines().find(|l| !l.trim().is_empty())?;
    let title = line
        .trim()
        .trim_start_matches('#')
        .trim_matches(|c: char| c.is_whitespace() || "*_`\"'".contains(c))
        .trim_end_matches('.')
        .trim();
    let len = title.chars().count();
    (len > 0 && len <= MAX_CHARS && !title.contains(char::is_control)).then(|| title.to_string())
}

#[cfg(test)]
mod tests {
    use super::clean;

    #[test]
    fn cleans_replies() {
        assert_eq!(
            clean("Fix login redirect\n").as_deref(),
            Some("Fix login redirect")
        );
        assert_eq!(
            clean("\n\"Fix login redirect.\"").as_deref(),
            Some("Fix login redirect")
        );
        assert_eq!(
            clean("# **Fix login redirect**").as_deref(),
            Some("Fix login redirect")
        );
        assert_eq!(clean(""), None);
        assert_eq!(
            clean("I don't see a task description in your message. Could you provide it?"),
            None
        );
    }
}
