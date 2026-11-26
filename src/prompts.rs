//! What the agent pages offer besides free text: prompt snippets (built in + your own), model
//! suggestions per agent, and profiles (separate accounts / configs per agent CLI).

use crate::agents::AgentKind;
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Snippet {
    pub name: String,
    pub text: String,
}

impl Snippet {
    fn new(name: &str, text: &str) -> Self {
        Snippet {
            name: name.into(),
            text: text.into(),
        }
    }
}

/// Ready-made messages shown as chips above the message box.
pub fn builtin() -> Vec<Snippet> {
    vec![
        Snippet::new("Plan first", "Before changing anything, write a short step-by-step plan and wait for my go."),
        Snippet::new("Write tests", "Add tests for the changes you made and run them. Fix anything that fails."),
        Snippet::new("Run & fix tests", "Run the project's test suite, find the failing tests and fix them. Don't weaken the tests."),
        Snippet::new("Review yourself", "Review your changes like a strict senior engineer: bugs, edge cases, error handling, naming, dead code. Fix what you find and list what you changed."),
        Snippet::new("Explain", "Explain what you changed and why, file by file, in a few bullet points."),
        Snippet::new("Simplify", "Make your changes smaller and simpler without changing behaviour."),
        Snippet::new("Lint & format", "Run the project's linter and formatter and fix every warning in the files you touched."),
        Snippet::new("Commit", "Commit your work with a clear commit message in this repository's style."),
        Snippet::new("Continue", "Continue."),
    ]
}

/// Model suggestions (value, label). The empty value means the agent's default.
pub fn models(kind: AgentKind) -> &'static [(&'static str, &'static str)] {
    match kind {
        AgentKind::Claude => &[
            ("", "Default"),
            ("opus", "Opus — most capable"),
            ("sonnet", "Sonnet — balanced"),
            ("haiku", "Haiku — fastest"),
        ],
        AgentKind::Codex => &[("", "Default")],
        AgentKind::Hermes => &[("", "Default")],
    }
}

/// A separate account / configuration of an agent CLI (its own config folder).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Profile {
    pub name: String,
    pub kind: AgentKind,
    pub dir: PathBuf,
}

impl Profile {
    /// The variable each CLI reads its config folder from.
    pub fn env_var(kind: AgentKind) -> &'static str {
        match kind {
            AgentKind::Claude => "CLAUDE_CONFIG_DIR",
            AgentKind::Codex => "CODEX_HOME",
            AgentKind::Hermes => "HERMES_HOME",
        }
    }

    pub fn env(&self) -> Vec<(String, String)> {
        vec![(
            Self::env_var(self.kind).to_string(),
            self.dir.display().to_string(),
        )]
    }

    /// Where a new profile's folder goes by default.
    pub fn default_dir(kind: AgentKind, name: &str) -> PathBuf {
        let safe: String = name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        eframe::storage_dir(crate::APP_NAME)
            .unwrap_or_else(std::env::temp_dir)
            .join("profiles")
            .join(format!(
                "{}-{}",
                kind.name().to_lowercase().replace(' ', "-"),
                safe
            ))
    }
}

/// Lines starting with `@` that point at files: `@src/main.rs` etc. (for the attach picker).
pub fn tracked_files(worktree: &std::path::Path) -> Vec<String> {
    let mut c = std::process::Command::new("git");
    c.args(["ls-files", "-co", "--exclude-standard"])
        .current_dir(worktree)
        .stdin(std::process::Stdio::null());
    if let Some(p) = crate::shellenv::path() {
        c.env("PATH", p);
    }
    c.output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

/// `@path` mention, quoted when the path has spaces.
pub fn mention(path: &str) -> String {
    if path.contains(char::is_whitespace) {
        format!("@\"{path}\"")
    } else {
        format!("@{path}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiles_point_each_cli_at_its_folder() {
        let p = Profile {
            name: "work".into(),
            kind: AgentKind::Claude,
            dir: PathBuf::from("/x/claude-work"),
        };
        assert_eq!(
            p.env(),
            [(
                "CLAUDE_CONFIG_DIR".to_string(),
                "/x/claude-work".to_string()
            )]
        );
        assert_eq!(Profile::env_var(AgentKind::Codex), "CODEX_HOME");
        assert!(Profile::default_dir(AgentKind::Claude, "my work").ends_with("claude-code-my-work"));
    }

    #[test]
    fn mentions() {
        assert_eq!(mention("src/a.rs"), "@src/a.rs");
        assert_eq!(mention("docs/my file.md"), "@\"docs/my file.md\"");
        assert!(builtin().iter().any(|s| s.name == "Write tests"));
    }
}
