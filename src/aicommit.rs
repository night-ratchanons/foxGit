//! "✨ Write message": Claude Code (`claude -p`) writes a commit message from the staged diff,
//! in the style of the repository's recent commits.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const MAX_DIFF: usize = 60_000;
const TIMEOUT: Duration = Duration::from_secs(120);

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let mut c = Command::new("git");
    c.args(args).current_dir(dir).stdin(Stdio::null());
    if let Some(p) = crate::shellenv::path() {
        c.env("PATH", p);
    }
    let o = c.output().map_err(|e| format!("git: {e}"))?;
    if !o.status.success() {
        return Err(String::from_utf8_lossy(&o.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&o.stdout).into_owned())
}

/// Cut `s` to at most `max` bytes on a char boundary.
fn cut(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut i = max;
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    &s[..i]
}

pub fn prompt(stat: &str, diff: &str, recent: &str) -> String {
    let truncated = diff.len() > MAX_DIFF;
    format!(
        "Write a git commit message for the staged changes below.\n\
         Match the style and language of this repository's recent commit subjects:\n{recent}\n\
         Rules: a summary line under 72 characters in the imperative mood; if the change needs it, \
         a blank line and a short body explaining what and why. Output ONLY the commit message — \
         no code fences, no quotes, no preamble.\n\n\
         Files:\n{stat}\nDiff{}:\n{}",
        if truncated { " (truncated)" } else { "" },
        cut(diff, MAX_DIFF)
    )
}

/// Strip what models sometimes wrap around the message.
pub fn clean(out: &str) -> String {
    let mut t = out.trim();
    if let Some(rest) = t.strip_prefix("```") {
        // drop the fence line (``` or ```text)
        t = rest.split_once('\n').map(|x| x.1).unwrap_or("");
        t = t.trim_end().strip_suffix("```").unwrap_or(t);
    }
    let t = t.trim();
    let t = t
        .strip_prefix('"')
        .and_then(|x| x.strip_suffix('"'))
        .unwrap_or(t);
    let mut lines: Vec<&str> = t.lines().map(str::trim_end).collect();
    // "Commit message:" style preambles
    if lines.first().is_some_and(|l| {
        l.to_lowercase()
            .trim_end_matches(':')
            .ends_with("commit message")
    }) {
        lines.remove(0);
        while lines.first().is_some_and(|l| l.is_empty()) {
            lines.remove(0);
        }
    }
    lines.join("\n").trim().to_string()
}

pub fn write_message(claude: &Path, dir: &Path) -> Result<String, String> {
    let stat = git(dir, &["diff", "--cached", "--no-color", "--stat"])?;
    if stat.trim().is_empty() {
        return Err("Nothing is staged".into());
    }
    let diff = git(
        dir,
        &["diff", "--cached", "--no-color", "--no-ext-diff", "-U2"],
    )?;
    let recent = git(dir, &["log", "-10", "--format=- %s"]).unwrap_or_default();
    let input = prompt(&stat, &diff, &recent);

    let mut c = Command::new(claude);
    c.args(["-p", "--output-format", "text", "--model", "haiku"])
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(p) = crate::shellenv::path() {
        c.env("PATH", p);
    }
    let mut child = c
        .spawn()
        .map_err(|e| format!("Could not start Claude Code: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        std::thread::spawn(move || {
            let _ = stdin.write_all(input.as_bytes());
        });
    }
    let mut out = child.stdout.take().ok_or("no stdout")?;
    let mut err = child.stderr.take().ok_or("no stderr")?;
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = out.read_to_string(&mut s);
        s
    });
    let err_reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = err.read_to_string(&mut s);
        s
    });
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) if start.elapsed() < TIMEOUT => std::thread::sleep(Duration::from_millis(100)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("Claude Code took too long to answer".into());
            }
        }
    };
    let text = reader.join().unwrap_or_default();
    let stderr = err_reader.join().unwrap_or_default();
    if !status.success() {
        let why = stderr
            .lines()
            .chain(text.lines())
            .find(|l| !l.trim().is_empty())
            .unwrap_or("failed");
        return Err(format!("Claude Code: {why}"));
    }
    let msg = clean(&text);
    if msg.is_empty() {
        Err("Claude Code returned an empty message".into())
    } else {
        Ok(msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleans_fences_quotes_and_preambles() {
        assert_eq!(
            clean("```\nFix crash on start\n```\n"),
            "Fix crash on start"
        );
        assert_eq!(
            clean("```text\nAdd X\n\nBody line\n```"),
            "Add X\n\nBody line"
        );
        assert_eq!(clean("\"Update docs\""), "Update docs");
        assert_eq!(clean("Commit message:\n\nRename foo   \n"), "Rename foo");
        assert_eq!(clean("  แก้บั๊ก login  "), "แก้บั๊ก login");
    }

    #[test]
    fn prompt_truncates_on_a_char_boundary() {
        let diff = "ก".repeat(MAX_DIFF); // 3 bytes each
        let p = prompt("a | 1 +", &diff, "- x");
        assert!(p.contains("(truncated)"));
        assert!(p.len() < MAX_DIFF + 2000);
    }
}
