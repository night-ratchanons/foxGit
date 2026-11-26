//! AI coding agents (Claude Code, Codex, Hermes) driven from FoxGit.
//!
//! A task is a chat with one agent. Its first message creates a git worktree on an
//! `agent/<slug>` branch (the user's own working copy is never touched) and runs the agent's
//! CLI there headless; every later message runs it again, resuming the same agent session, so
//! the conversation carries on. Messages typed while the agent is busy wait in a queue. Output
//! (JSON lines) is parsed into chat lines: the agent's words, its tool calls, errors.
//! When done, the work is reviewed (worktree opened as a tab), merged, or discarded.

use crate::git;
use crate::shellenv;
use crate::theme::{self, pal};
use eframe::egui;
use egui::{vec2, RichText};
use serde_json::Value;
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const MAX_LOG: usize = 3000;
const SAVED_LOG: usize = 300;

// ---------------------------------------------------------------------------
// Agents
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum AgentKind {
    Claude,
    Codex,
    Hermes,
}

impl AgentKind {
    pub const ALL: [AgentKind; 3] = [AgentKind::Claude, AgentKind::Codex, AgentKind::Hermes];

    pub fn name(self) -> &'static str {
        match self {
            AgentKind::Claude => "Claude Code",
            AgentKind::Codex => "Codex",
            AgentKind::Hermes => "Hermes",
        }
    }

    fn program(self) -> &'static str {
        match self {
            AgentKind::Claude => "claude",
            AgentKind::Codex => "codex",
            AgentKind::Hermes => "hermes",
        }
    }

    pub fn install_url(self) -> &'static str {
        match self {
            AgentKind::Claude => "https://www.anthropic.com/claude-code",
            AgentKind::Codex => "https://github.com/openai/codex",
            AgentKind::Hermes => "https://github.com/NousResearch/hermes-agent",
        }
    }

    /// Full path of the CLI if it is installed (looked up on the login-shell PATH).
    pub fn find(self) -> Option<PathBuf> {
        crate::desktop::which(self.program())
    }

    /// The headless command for one turn, and what to write to its stdin.
    fn command(
        self,
        exe: &Path,
        prompt: &str,
        cwd: &Path,
        resume: Option<&str>,
        run_commands: bool,
    ) -> (Command, Option<String>) {
        let mut c = Command::new(exe);
        match self {
            AgentKind::Claude => {
                c.args(["-p", "--output-format", "stream-json", "--verbose"]);
                c.args([
                    "--permission-mode",
                    if run_commands { "auto" } else { "acceptEdits" },
                ]);
                if let Some(s) = resume {
                    c.args(["--resume", s]);
                }
                // the prompt goes in on stdin: no length limit, nothing interpreted
                (c, Some(prompt.to_string()))
            }
            AgentKind::Codex => {
                // Codex sandboxes its commands to the working directory (--full-auto).
                c.args([
                    "exec",
                    "--json",
                    "--full-auto",
                    "--skip-git-repo-check",
                    "-C",
                ])
                .arg(cwd)
                .arg(prompt);
                (c, None)
            }
            AgentKind::Hermes => {
                c.args([
                    "chat",
                    "--query-file",
                    "-",
                    "--oneshot",
                    "--format",
                    "stream-json",
                ])
                .arg("--in")
                .arg(cwd);
                if let Some(s) = resume {
                    c.args(["--resume", s]);
                }
                if run_commands {
                    c.arg("--yolo");
                }
                (c, Some(prompt.to_string()))
            }
        }
    }

    /// Shell line that starts the agent's interactive interface (resuming the chat's session).
    fn interactive_line(self, exe: &Path, resume: Option<&str>) -> String {
        let q = shell_quote(&exe.to_string_lossy());
        match (self, resume) {
            (AgentKind::Claude, Some(s)) => format!("{q} --resume {}", shell_quote(s)),
            (AgentKind::Hermes, Some(s)) => format!("{q} chat --resume {}", shell_quote(s)),
            _ => q,
        }
    }

    /// Whether a later message can continue the agent's own session.
    fn can_resume(self) -> bool {
        !matches!(self, AgentKind::Codex)
    }
}

/// Quote for POSIX shells (and good enough for PowerShell when there are no quotes inside).
fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:@".contains(c))
    {
        s.to_string()
    } else if cfg!(windows) {
        format!("\"{s}\"")
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

// ---------------------------------------------------------------------------
// Parsing the agents' JSON lines
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub enum Ev {
    Session(String),
    /// Something the agent says.
    Say(String),
    /// A streamed piece of what the agent says.
    SayDelta(String),
    /// A tool call, command or file edit.
    Act(String),
    /// Anything else worth a faint line.
    Note(String),
    Result {
        ok: bool,
        text: Option<String>,
        cost: Option<f64>,
    },
    Fail(String),
}

fn s(v: &Value, k: &str) -> Option<String> {
    v.get(k)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .map(String::from)
}

/// `p` relative to the worktree. `cwd` may hold two spellings separated by a NUL: the path
/// FoxGit uses and its canonical form (agents report physical paths, e.g. /private/var on macOS).
fn short_path(p: &str, cwd: &str) -> String {
    cwd.split('\0')
        .filter(|c| !c.is_empty())
        .find_map(|c| p.strip_prefix(c))
        .map(|r| r.trim_start_matches('/').to_string())
        .unwrap_or_else(|| p.to_string())
}

fn first_line(t: &str, max: usize) -> String {
    let l = t.lines().next().unwrap_or("").trim();
    if l.chars().count() > max {
        format!("{}…", l.chars().take(max).collect::<String>())
    } else {
        l.to_string()
    }
}

/// Describe a Claude Code tool call in a few words.
fn describe_tool(name: &str, input: &Value, cwd: &str) -> String {
    let path = |k: &str| s(input, k).map(|p| short_path(&p, cwd)).unwrap_or_default();
    match name {
        "Bash" => format!(
            "$ {}",
            first_line(&s(input, "command").unwrap_or_default(), 120)
        ),
        "Edit" | "MultiEdit" | "Write" | "NotebookEdit" => format!("{name} {}", path("file_path")),
        "Read" => format!("Read {}", path("file_path")),
        "Glob" | "Grep" => format!("{name} {}", s(input, "pattern").unwrap_or_default()),
        "WebFetch" => format!("Fetch {}", s(input, "url").unwrap_or_default()),
        "WebSearch" => format!(
            "Search \u{201C}{}\u{201D}",
            s(input, "query").unwrap_or_default()
        ),
        "TodoWrite" => "Update the plan".into(),
        "Task" | "Agent" => format!("Sub-agent: {}", s(input, "description").unwrap_or_default()),
        other => other.to_string(),
    }
}

pub fn parse_line(kind: AgentKind, line: &str, cwd: &Path) -> Vec<Ev> {
    let line = line.trim();
    if line.is_empty() {
        return Vec::new();
    }
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return vec![Ev::Note(line.to_string())];
    };
    let canon = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let cwd = format!("{}\0{}", cwd.display(), canon.display());
    match kind {
        AgentKind::Claude => parse_claude(&v, &cwd),
        AgentKind::Codex => parse_codex(&v, &cwd),
        AgentKind::Hermes => parse_generic(&v),
    }
}

fn parse_claude(v: &Value, cwd: &str) -> Vec<Ev> {
    let mut out = Vec::new();
    match v["type"].as_str().unwrap_or("") {
        "system" if v["subtype"] == "init" => {
            if let Some(id) = s(v, "session_id") {
                out.push(Ev::Session(id));
            }
        }
        "assistant" => {
            for b in v["message"]["content"].as_array().into_iter().flatten() {
                match b["type"].as_str() {
                    Some("text") => {
                        if let Some(t) = s(b, "text") {
                            out.push(Ev::Say(t));
                        }
                    }
                    Some("tool_use") => out.push(Ev::Act(describe_tool(
                        b["name"].as_str().unwrap_or("tool"),
                        &b["input"],
                        cwd,
                    ))),
                    _ => {}
                }
            }
        }
        "user" => {
            for b in v["message"]["content"].as_array().into_iter().flatten() {
                if b["type"] == "tool_result" && b["is_error"] == true {
                    let text = match &b["content"] {
                        Value::String(t) => t.clone(),
                        other => other.to_string(),
                    };
                    out.push(Ev::Note(format!("Tool error: {}", first_line(&text, 160))));
                }
            }
        }
        "result" => {
            if let Some(id) = s(v, "session_id") {
                out.push(Ev::Session(id));
            }
            let ok =
                v["is_error"] != true && v["subtype"].as_str().unwrap_or("success") == "success";
            out.push(Ev::Result {
                ok,
                text: s(v, "result"),
                cost: v["total_cost_usd"].as_f64(),
            });
        }
        _ => {}
    }
    out
}

fn parse_codex(v: &Value, cwd: &str) -> Vec<Ev> {
    let mut out = Vec::new();
    let t = v["type"].as_str().unwrap_or("");
    match t {
        "thread.started" => {
            if let Some(id) = s(v, "thread_id") {
                out.push(Ev::Session(id));
            }
        }
        "item.started" | "item.completed" => {
            let item = &v["item"];
            let it = item["type"]
                .as_str()
                .or(item["item_type"].as_str())
                .unwrap_or("");
            match (it, t) {
                ("agent_message", "item.completed") => {
                    if let Some(x) = s(item, "text") {
                        out.push(Ev::Say(x));
                    }
                }
                ("command_execution", "item.started") => out.push(Ev::Act(format!(
                    "$ {}",
                    first_line(&s(item, "command").unwrap_or_default(), 120)
                ))),
                ("file_change", "item.completed") => {
                    for c in item["changes"].as_array().into_iter().flatten() {
                        if let Some(p) = s(c, "path") {
                            out.push(Ev::Act(format!("Edit {}", short_path(&p, cwd))));
                        }
                    }
                }
                ("error", _) => out.push(Ev::Fail(s(item, "message").unwrap_or_default())),
                _ => {}
            }
        }
        "turn.failed" => out.push(Ev::Fail(
            s(&v["error"], "message").unwrap_or_else(|| "The turn failed".into()),
        )),
        "error" => out.push(Ev::Fail(s(v, "message").unwrap_or_else(|| v.to_string()))),
        _ => {}
    }
    out
}

/// Tolerant parsing for agents whose event shapes vary (Hermes): pick out a session id, text,
/// tool calls, results and errors by their usual field names.
fn parse_generic(v: &Value) -> Vec<Ev> {
    let mut out = Vec::new();
    for k in ["session_id", "sessionId", "thread_id", "session"] {
        if let Some(id) = s(v, k) {
            out.push(Ev::Session(id));
            break;
        }
    }
    let t = v["type"]
        .as_str()
        .or(v["event"].as_str())
        .unwrap_or("")
        .to_lowercase();
    let text = || {
        [
            "text", "content", "message", "response", "result", "output", "delta",
        ]
        .iter()
        .find_map(|k| s(v, k))
    };
    if t.contains("error") || t.contains("fail") {
        out.push(Ev::Fail(
            s(v, "error")
                .or_else(|| s(&v["error"], "message"))
                .or_else(text)
                .unwrap_or_else(|| v.to_string()),
        ));
    } else if t.contains("delta") || t.contains("token") || t.contains("chunk") {
        if let Some(x) = ["delta", "text", "content", "token"].iter().find_map(|k| {
            v.get(*k)
                .and_then(Value::as_str)
                .filter(|x| !x.is_empty())
                .map(String::from)
        }) {
            out.push(Ev::SayDelta(x));
        }
    } else if t.contains("tool") || t.contains("command") {
        let name = s(v, "name")
            .or_else(|| s(v, "tool"))
            .or_else(|| s(v, "tool_name"))
            .unwrap_or_else(|| "tool".into());
        let arg = ["command", "path", "file", "query", "url"]
            .iter()
            .find_map(|k| {
                s(v, k)
                    .or_else(|| s(&v["args"], k))
                    .or_else(|| s(&v["input"], k))
            });
        if !t.contains("result") && !t.contains("output") {
            out.push(Ev::Act(match arg {
                Some(a) => format!("{name}: {}", first_line(&a, 120)),
                None => name,
            }));
        }
    } else if t.contains("result")
        || t.contains("final")
        || t.contains("done")
        || t.contains("complete")
    {
        out.push(Ev::Result {
            ok: v["is_error"] != true
                && v["success"] != false
                && v["exit_code"].as_i64().unwrap_or(0) == 0,
            text: text(),
            cost: v["total_cost_usd"].as_f64().or(v["cost"].as_f64()),
        });
    } else if let Some(x) = text() {
        out.push(Ev::Say(x));
    }
    out
}

// ---------------------------------------------------------------------------
// Tasks
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Status {
    Running,
    /// Waiting for your next message.
    Idle,
    Failed,
    Stopped,
    Merged,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Role {
    You,
    Agent,
    Tool,
    Info,
    Error,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Line {
    pub role: Role,
    pub text: String,
}

/// What a background run sends back.
enum Msg {
    Ev(Ev),
    Stderr(String),
    Failed(String),
    Exited(Option<i32>),
}

struct Runner {
    rx: Receiver<Msg>,
    child: Arc<Mutex<Option<Child>>>,
    /// Whether this run produced a result event (otherwise the exit code decides).
    got_result: bool,
}

pub struct Task {
    pub id: u64,
    pub kind: AgentKind,
    pub title: String,
    /// Main repository (where merges happen).
    pub repo: PathBuf,
    pub worktree: PathBuf,
    pub branch: String,
    pub base: String,
    pub run_commands: bool,
    pub status: Status,
    pub session: Option<String>,
    pub log: Vec<Line>,
    pub cost: f64,
    /// Files with changes in the worktree, commits on the branch.
    pub changes: (usize, usize),
    worktree_ready: bool,
    queue: VecDeque<String>,
    input: String,
    started: Option<Instant>,
    runner: Option<Runner>,
    /// Chat or Terminal.
    pub pane: Pane,
    terminal: Option<crate::terminal::Terminal>,
    term_error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Pane {
    #[default]
    Chat,
    Terminal,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct SavedTask {
    id: u64,
    kind: AgentKind,
    title: String,
    repo: PathBuf,
    worktree: PathBuf,
    branch: String,
    base: String,
    run_commands: bool,
    status: Status,
    session: Option<String>,
    #[serde(default)]
    log: Vec<Line>,
    #[serde(default)]
    cost: f64,
}

/// One row of the sidebar's AGENTS section.
pub struct RowInfo {
    pub id: u64,
    pub title: String,
    pub status: Status,
    pub kind: AgentKind,
    pub queued: usize,
}

/// Something the app has to do for a task (needs the job queue, tabs or a confirmation).
pub enum TaskAction {
    Review(PathBuf),
    Merge {
        task: u64,
        repo: PathBuf,
        worktree: PathBuf,
        branch: String,
        message: String,
        commit_first: bool,
        squash: bool,
    },
    Discard {
        task: u64,
        repo: PathBuf,
        worktree: PathBuf,
        branch: String,
    },
    /// Push the agent's branch (committing its work first) and open GitHub's "new pull request".
    PushPr {
        repo: PathBuf,
        worktree: PathBuf,
        branch: String,
        remote: String,
        message: String,
        commit_first: bool,
        compare_url: Option<String>,
        /// Branch the chat started from (the pull request's base).
        base: String,
        title: String,
    },
    Notify(String, bool),
}

struct NewTask {
    repo: PathBuf,
    repo_name: String,
    base: String,
    prompt: String,
    kind: AgentKind,
    run_commands: bool,
    installed: Vec<(AgentKind, Option<PathBuf>)>,
    focus: bool,
}

pub struct Agents {
    tasks: Vec<Task>,
    next_id: u64,
    pub last_kind: AgentKind,
    new_task: Option<NewTask>,
    ctx: egui::Context,
    notes: Vec<(String, bool)>,
    /// A task created this frame that the app should show.
    pub opened: Option<(PathBuf, u64)>,
    /// Where agents' worktrees go (`<app data>/worktrees`).
    worktree_root: PathBuf,
    /// Which agent CLIs are installed (re-checked every few seconds while shown).
    installed: Option<(Instant, Vec<(AgentKind, bool)>)>,
}

impl Agents {
    pub fn new(ctx: &egui::Context, saved: Vec<SavedTask>, last_kind: Option<AgentKind>) -> Self {
        let next_id = saved.iter().map(|t| t.id).max().unwrap_or(0) + 1;
        let tasks = saved
            .into_iter()
            .map(|t| {
                let mut log = t.log;
                let status = match t.status {
                    Status::Running => {
                        log.push(Line {
                            role: Role::Info,
                            text: "FoxGit was closed while the agent was working — send a message to carry on.".into(),
                        });
                        Status::Stopped
                    }
                    s => s,
                };
                let worktree_ready = t.worktree.exists();
                let mut task = Task {
                    id: t.id,
                    kind: t.kind,
                    title: t.title,
                    repo: t.repo,
                    worktree: t.worktree,
                    branch: t.branch,
                    base: t.base,
                    run_commands: t.run_commands,
                    status,
                    session: t.session,
                    log,
                    cost: t.cost,
                    changes: (0, 0),
                    worktree_ready,
                    queue: VecDeque::new(),
                    input: String::new(),
                    started: None,
                    runner: None,
                    pane: Pane::Chat,
                    terminal: None,
                    term_error: None,
                };
                task.refresh_changes();
                task
            })
            .collect();
        Self {
            tasks,
            next_id,
            last_kind: last_kind.unwrap_or(AgentKind::Claude),
            new_task: None,
            ctx: ctx.clone(),
            notes: Vec::new(),
            opened: None,
            installed: None,
            worktree_root: eframe::storage_dir(crate::APP_NAME)
                .unwrap_or_else(std::env::temp_dir)
                .join("worktrees"),
        }
    }

    pub fn saved(&self) -> Vec<SavedTask> {
        self.tasks
            .iter()
            .map(|t| SavedTask {
                id: t.id,
                kind: t.kind,
                title: t.title.clone(),
                repo: t.repo.clone(),
                worktree: t.worktree.clone(),
                branch: t.branch.clone(),
                base: t.base.clone(),
                run_commands: t.run_commands,
                status: t.status,
                session: t.session.clone(),
                log: t.log[t.log.len().saturating_sub(SAVED_LOG)..].to_vec(),
                cost: t.cost,
            })
            .collect()
    }

    pub fn any_running(&self) -> bool {
        self.tasks.iter().any(|t| t.runner.is_some())
    }

    pub fn rows_for(&self, repo: &Path) -> Vec<RowInfo> {
        self.tasks
            .iter()
            .filter(|t| t.repo == repo)
            .rev()
            .map(|t| RowInfo {
                id: t.id,
                title: t.title.clone(),
                status: t.status,
                kind: t.kind,
                queued: t.queue.len(),
            })
            .collect()
    }

    /// Every chat of every repository: (repository, row), newest first.
    pub fn all_rows(&self) -> Vec<(PathBuf, RowInfo)> {
        self.tasks
            .iter()
            .rev()
            .map(|t| {
                (
                    t.repo.clone(),
                    RowInfo {
                        id: t.id,
                        title: t.title.clone(),
                        status: t.status,
                        kind: t.kind,
                        queued: t.queue.len(),
                    },
                )
            })
            .collect()
    }

    pub fn has(&self, id: u64) -> bool {
        self.tasks.iter().any(|t| t.id == id)
    }

    fn installed(&mut self) -> Vec<(AgentKind, bool)> {
        let stale = self
            .installed
            .as_ref()
            .is_none_or(|(at, _)| at.elapsed() > Duration::from_secs(5));
        if stale {
            let found = AgentKind::ALL
                .iter()
                .map(|k| (*k, k.find().is_some()))
                .collect();
            self.installed = Some((Instant::now(), found));
        }
        self.installed
            .as_ref()
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    }

    /// The Agents view when no chat is open. Returns true when "New chat" is clicked.
    pub fn empty_view(&mut self, ui: &mut egui::Ui) -> bool {
        let installed = self.installed();
        let mut clicked = false;
        egui::CentralPanel::default().show(ui, |ui| {
            let p = pal(ui);
            ui.vertical_centered(|ui| {
                ui.add_space((ui.available_height() * 0.18).max(20.0));
                ui.label(RichText::new("✨").size(40.0).color(p.accent_hover));
                ui.add_space(4.0);
                ui.label(RichText::new("AI agents").size(22.0).strong().color(p.text_strong));
                ui.add_space(4.0);
                ui.label(
                    RichText::new("Give a coding task to an agent and chat with it while it works.\nIt uses its own copy of this repository — review its changes, then merge or discard.")
                        .color(p.subtext),
                );
                ui.add_space(16.0);
                let any = installed.iter().any(|(_, ok)| *ok);
                if theme::primary_button(ui, "✚  New chat", any).clicked() {
                    clicked = true;
                }
                ui.add_space(18.0);
                ui.horizontal(|ui| {
                    // centre the row of agents under the button
                    let w = 3.0 * 150.0;
                    ui.add_space(((ui.available_width() - w) / 2.0).max(0.0));
                    for (k, ok) in &installed {
                        egui::Frame::new()
                            .fill(p.surface0)
                            .stroke(egui::Stroke::new(1.0, p.border))
                            .corner_radius(10.0)
                            .inner_margin(egui::Margin::symmetric(12, 10))
                            .show(ui, |ui| {
                                ui.set_width(118.0);
                                ui.vertical_centered(|ui| {
                                    ui.label(RichText::new(k.name()).strong().color(p.text_strong));
                                    if *ok {
                                        ui.label(RichText::new("✔ installed").small().color(p.green));
                                    } else if ui.link(RichText::new("Install").small()).clicked() {
                                        ui.ctx().open_url(egui::OpenUrl::new_tab(k.install_url()));
                                    }
                                });
                            });
                    }
                });
                if !installed.iter().any(|(_, ok)| *ok) {
                    ui.add_space(10.0);
                    ui.label(
                        RichText::new("Install one of these command-line agents, then come back here.")
                            .small()
                            .color(p.faint),
                    );
                }
            });
        });
        clicked
    }

    pub fn mark_merged(&mut self, id: u64) {
        if let Some(t) = self.tasks.iter_mut().find(|t| t.id == id) {
            t.status = Status::Merged;
            t.log.push(Line {
                role: Role::Info,
                text: format!("Merged {} into {}.", t.branch, t.base),
            });
            t.refresh_changes();
        }
    }

    pub fn remove(&mut self, id: u64) {
        if let Some(i) = self.tasks.iter().position(|t| t.id == id) {
            let mut t = self.tasks.remove(i);
            t.stop();
        }
    }

    // ------------------------------------------------------------------ new task dialog

    /// Open the "New agent chat" dialog (`prompt` prefills the first message).
    pub fn open_new(&mut self, repo: &Path, repo_name: &str, base: &str, prompt: &str) {
        let installed: Vec<_> = AgentKind::ALL.iter().map(|k| (*k, k.find())).collect();
        let kind = if installed
            .iter()
            .any(|(k, p)| *k == self.last_kind && p.is_some())
        {
            self.last_kind
        } else {
            installed
                .iter()
                .find(|(_, p)| p.is_some())
                .map(|(k, _)| *k)
                .unwrap_or(self.last_kind)
        };
        self.new_task = Some(NewTask {
            repo: repo.to_path_buf(),
            repo_name: repo_name.to_string(),
            base: base.to_string(),
            prompt: prompt.to_string(),
            kind,
            run_commands: true,
            installed,
            focus: true,
        });
    }

    pub fn dialog_ui(&mut self, ctx: &egui::Context) {
        let Some(f) = &mut self.new_task else {
            return;
        };
        let mut close = false;
        let mut start = false;
        egui::Modal::new(egui::Id::unique("agent_new_task")).show(ctx, |ui| {
            ui.set_width(560.0);
            let p = pal(ui);
            ui.horizontal(|ui| {
                ui.label(RichText::new("✨").size(24.0).color(p.accent_hover));
                ui.vertical(|ui| {
                    ui.label(RichText::new("New agent chat").size(18.0).strong().color(p.text_strong));
                    ui.label(
                        RichText::new(format!("{} · starts from {}", f.repo_name, f.base)).color(p.subtext),
                    );
                });
            });
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                for (k, path) in &f.installed {
                    let on = f.kind == *k;
                    let label = if path.is_some() {
                        RichText::new(k.name())
                    } else {
                        RichText::new(format!("{} (not installed)", k.name())).color(p.faint)
                    };
                    let r = ui.add(egui::Button::selectable(on, label).min_size(vec2(0.0, 28.0)));
                    if r.clicked() {
                        f.kind = *k;
                    }
                }
            });
            let exe = f.installed.iter().find(|(k, _)| *k == f.kind).and_then(|(_, p)| p.clone());
            if exe.is_none() {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("{} isn't installed (or not on your PATH).", f.kind.name())).color(p.yellow));
                    if ui.link("How to install").clicked() {
                        ui.ctx().open_url(egui::OpenUrl::new_tab(f.kind.install_url()));
                    }
                });
            }
            ui.add_space(8.0);
            let r = ui.add(
                egui::TextEdit::multiline(&mut f.prompt)
                    .hint_text("What should the agent do?  e.g. Fix the login timeout bug and add a test")
                    .desired_rows(6)
                    .desired_width(f32::INFINITY)
                    .margin(vec2(10.0, 8.0)),
            );
            if f.focus {
                r.request_focus();
                f.focus = false;
            }
            let submit = r.has_focus() && ui.input(|i| i.modifiers.command && i.key_pressed(egui::Key::Enter));
            ui.add_space(6.0);
            ui.checkbox(&mut f.run_commands, "Let it run commands (tests, builds, installs)");
            if f.run_commands {
                let how = match f.kind {
                    AgentKind::Claude => "Claude Code's auto mode approves safe commands itself and refuses risky ones.",
                    AgentKind::Codex => "Codex runs commands in its sandbox, limited to the agent's copy of the repository.",
                    AgentKind::Hermes => "Hermes will run every command without asking (--yolo). Its own copy keeps your folder safe, but commands can still reach the rest of the computer.",
                };
                ui.label(RichText::new(how).small().color(if f.kind == AgentKind::Hermes { p.yellow } else { p.subtext }));
            }
            ui.label(
                RichText::new(
                    "It works in its own copy of the repository (a git worktree on a new agent/… branch) — \
                     your folder isn't touched. You can keep chatting with it, then review and merge.",
                )
                .small()
                .color(p.faint),
            );
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                let ok = exe.is_some() && !f.prompt.trim().is_empty();
                if theme::primary_button(ui, &format!("▶  Start  ({})", theme::shortcut("Enter")), ok).clicked()
                    || (submit && ok)
                {
                    start = true;
                }
                if ui.add(egui::Button::new("Cancel").min_size(vec2(0.0, 28.0))).clicked()
                    || ui.input(|i| i.key_pressed(egui::Key::Escape))
                {
                    close = true;
                }
            });
        });
        if start {
            let f = self.new_task.take().unwrap();
            let exe = f
                .installed
                .iter()
                .find(|(k, _)| *k == f.kind)
                .and_then(|(_, p)| p.clone());
            if let Some(exe) = exe {
                self.last_kind = f.kind;
                let id = self.create(f, exe);
                self.opened = self
                    .tasks
                    .iter()
                    .find(|t| t.id == id)
                    .map(|t| (t.repo.clone(), id));
            }
        } else if close {
            self.new_task = None;
        }
    }

    fn create(&mut self, f: NewTask, exe: PathBuf) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let slug = slug(&f.prompt, id);
        let repo_key = format!(
            "{}-{:06x}",
            sanitize(&f.repo_name),
            path_hash(&f.repo) & 0xff_ffff
        );
        let worktree = self.worktree_root.join(repo_key).join(&slug);
        let mut task = Task {
            id,
            kind: f.kind,
            title: first_line(&f.prompt, 60),
            repo: f.repo,
            worktree,
            branch: format!("agent/{slug}"),
            base: f.base,
            run_commands: f.run_commands,
            status: Status::Running,
            session: None,
            log: Vec::new(),
            cost: 0.0,
            changes: (0, 0),
            worktree_ready: false,
            queue: VecDeque::new(),
            input: String::new(),
            started: None,
            runner: None,
            pane: Pane::Chat,
            terminal: None,
            term_error: None,
        };
        task.send(f.prompt.trim().to_string(), exe, &self.ctx);
        self.tasks.push(task);
        id
    }

    // ------------------------------------------------------------------ background results

    /// Read what the agents printed. Returns toasts (text, ok).
    pub fn poll(&mut self) -> Vec<(String, bool)> {
        for t in &mut self.tasks {
            let msgs: Vec<Msg> = match &t.runner {
                Some(r) => r.rx.try_iter().collect(),
                None => continue,
            };
            let mut finished = None;
            for m in msgs {
                match m {
                    Msg::Ev(ev) => {
                        if matches!(ev, Ev::Result { .. }) {
                            if let Some(r) = &mut t.runner {
                                r.got_result = true;
                            }
                        }
                        t.apply(ev);
                    }
                    // CLIs print their session id on stderr; that's not chat material
                    Msg::Stderr(line) if line.to_lowercase().starts_with("session") => {}
                    Msg::Stderr(line) => t.push(Role::Info, line),
                    Msg::Failed(e) => {
                        t.push(Role::Error, e);
                        finished = Some(false);
                    }
                    Msg::Exited(code) => {
                        t.worktree_ready = t.worktree.exists();
                        let got_result = t.runner.as_ref().is_some_and(|r| r.got_result);
                        if t.status == Status::Stopped {
                            finished = Some(false);
                        } else if got_result {
                            finished = Some(t.status != Status::Failed);
                        } else {
                            let ok = code == Some(0);
                            if !ok {
                                t.push(
                                    Role::Error,
                                    match code {
                                        Some(c) => {
                                            format!("{} exited with code {c}", t.kind.name())
                                        }
                                        None => format!("{} ended unexpectedly", t.kind.name()),
                                    },
                                );
                            }
                            finished = Some(ok);
                        }
                    }
                }
            }
            if let Some(ok) = finished {
                t.runner = None;
                t.refresh_changes();
                if t.status != Status::Stopped {
                    t.status = if ok { Status::Idle } else { Status::Failed };
                }
                // messages typed meanwhile go next
                if let Some(next) = t.queue.pop_front() {
                    if let Some(exe) = t.kind.find() {
                        t.send(next, exe, &self.ctx);
                        continue;
                    }
                }
                if t.status == Status::Stopped {
                    continue;
                }
                let (files, commits) = t.changes;
                self.notes.push((
                    if ok {
                        format!(
                            "{} finished “{}” — {files} file(s) changed, {commits} commit(s)",
                            t.kind.name(),
                            t.title
                        )
                    } else {
                        format!("{} stopped on “{}” — see the chat", t.kind.name(), t.title)
                    },
                    ok,
                ));
            }
        }
        std::mem::take(&mut self.notes)
    }

    // ------------------------------------------------------------------ task view

    /// The chat with one agent (shown in the main area of the repository tab).
    pub fn task_view(&mut self, ui: &mut egui::Ui, id: u64) -> Vec<TaskAction> {
        let mut out = Vec::new();
        let ctx = self.ctx.clone();
        let Some(t) = self.tasks.iter_mut().find(|t| t.id == id) else {
            egui::CentralPanel::default().show(ui, |ui| {
                ui.centered_and_justified(|ui| {
                    ui.label(RichText::new("This agent chat was removed.").weak())
                });
            });
            return out;
        };
        let p = pal(ui);
        let running = t.runner.is_some();

        // ---- header
        egui::Panel::top("agent_header")
            .frame(theme::panel(p.base, 14, 10))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("✨").size(18.0).color(p.accent_hover));
                    ui.label(
                        RichText::new(&t.title)
                            .size(16.0)
                            .strong()
                            .color(p.text_strong),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if running {
                            if ui
                                .button("⏹  Stop")
                                .on_hover_text("Stop the agent")
                                .clicked()
                            {
                                t.stop();
                                t.queue.clear();
                            }
                            ui.add(egui::Spinner::new().size(14.0));
                            let secs = t.started.map(|s| s.elapsed().as_secs()).unwrap_or(0);
                            ui.label(
                                RichText::new(format!("working · {}:{:02}", secs / 60, secs % 60))
                                    .color(p.accent_hover),
                            );
                            ctx.request_repaint_after(Duration::from_millis(500));
                        } else {
                            let (txt, c) = status_label(t.status, p);
                            ui.label(RichText::new(txt).color(c));
                        }
                    });
                });
                ui.horizontal(|ui| {
                    let mut info = format!("{} · {} · from {}", t.kind.name(), t.branch, t.base);
                    if t.cost > 0.0 {
                        info.push_str(&format!(" · ≈ ${:.2} at API prices", t.cost));
                    }
                    ui.label(RichText::new(info).small().color(p.faint));
                });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    let mut idx = usize::from(t.pane == Pane::Terminal);
                    if theme::segmented(ui, &["Chat", "Terminal"], &mut idx) {
                        t.pane = if idx == 1 { Pane::Terminal } else { Pane::Chat };
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let ready = t.worktree_ready;
                        let more = ui.add_enabled(ready, egui::Button::new("⋯")).on_hover_text("Open the agent's copy elsewhere");
                        egui::Popup::menu(&more).show(|ui| {
                            if ui.button("Open in Terminal app").clicked() {
                                if let Err(e) = crate::desktop::open_terminal_app(&t.worktree) {
                                    out.push(TaskAction::Notify(e, false));
                                }
                                ui.close();
                            }
                            if ui.button("Show in file manager").clicked() {
                                crate::tabs::reveal_in_file_manager(&t.worktree);
                                ui.close();
                            }
                            if ui.button("Copy folder path").clicked() {
                                ui.ctx().copy_text(t.worktree.display().to_string());
                                ui.close();
                            }
                        });
                        let editor = crate::desktop::editor();
                        let label = match &editor {
                            Some((name, _)) => format!("Open in {name}"),
                            None => "Open in editor".to_string(),
                        };
                        if ui
                            .add_enabled(ready, egui::Button::new(label))
                            .on_hover_text("Open the agent's copy of the repository in your code editor")
                            .clicked()
                        {
                            match editor {
                                Some((name, exe)) => {
                                    if let Err(e) = crate::desktop::spawn_detached(&exe, &[t.worktree.as_os_str()]) {
                                        out.push(TaskAction::Notify(format!("Could not start {name}: {e}"), false));
                                    }
                                }
                                None => {
                                    out.push(TaskAction::Notify(
                                        "No editor found (Cursor, VS Code, Zed or Sublime Text with its command-line tool) — opening the folder instead".into(),
                                        false,
                                    ));
                                    crate::tabs::reveal_in_file_manager(&t.worktree);
                                }
                            }
                        }
                    });
                });
            });

        // ---- actions + message box
        egui::Panel::bottom("agent_input")
            .frame(theme::panel(p.mantle, 14, 10))
            .show(ui, |ui| {
                let (files, commits) = t.changes;
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(if files + commits == 0 {
                            "No changes yet".to_string()
                        } else {
                            format!("{files} file(s) changed · {commits} commit(s) on {}", t.branch)
                        })
                        .color(p.subtext),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let has_work = t.worktree_ready && files + commits > 0;
                        ui.add_enabled_ui(!running && t.worktree_ready, |ui| {
                            if ui.button("Discard…").on_hover_text("Delete the agent's copy and its branch").clicked() {
                                out.push(TaskAction::Discard {
                                    task: t.id,
                                    repo: t.repo.clone(),
                                    worktree: t.worktree.clone(),
                                    branch: t.branch.clone(),
                                });
                            }
                        });
                        ui.add_enabled_ui(!running && has_work && t.status != Status::Merged, |ui| {
                            let merge = theme::primary_button(ui, &format!("Merge into {}", t.base), true)
                                .on_hover_text("Commit the agent's changes on its branch, then merge the branch");
                            if merge.clicked() {
                                out.push(t.merge_action(false));
                            }
                            let more = ui.button("⏷").on_hover_text("More merge options");
                            egui::Popup::menu(&more).show(|ui| {
                                if ui.button("Squash into staged changes").clicked() {
                                    out.push(t.merge_action(true));
                                    ui.close();
                                }
                            });
                        });
                        let push = t.push_action();
                        ui.add_enabled_ui(!running && has_work && push.is_some(), |ui| {
                            let r = ui.button("Push & PR").on_hover_text(if push.is_some() {
                                "Commit the agent's work, push its branch and open GitHub's new pull request page"
                            } else {
                                "Add a remote to this repository first"
                            });
                            if r.clicked() {
                                if let Some(a) = push {
                                    out.push(a);
                                }
                            }
                        });
                        ui.add_enabled_ui(t.worktree_ready, |ui| {
                            if ui.button("Review changes").on_hover_text("Open the agent's copy as a tab — stage, discard or edit like any repository").clicked() {
                                out.push(TaskAction::Review(t.worktree.clone()));
                            }
                        });
                    });
                });
                if t.pane == Pane::Terminal {
                    return;
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    let w = ui.available_width() - 90.0;
                    let r = ui.add(
                        egui::TextEdit::multiline(&mut t.input)
                            .hint_text(if running {
                                "Type a message — it's sent when the agent finishes this step"
                            } else {
                                "Reply to the agent…"
                            })
                            .desired_rows(2)
                            .desired_width(w)
                            .margin(vec2(10.0, 8.0)),
                    );
                    let send_key = r.has_focus()
                        && ui.input(|i| i.key_pressed(egui::Key::Enter) && (i.modifiers.command || !i.modifiers.shift));
                    let can = !t.input.trim().is_empty();
                    if (theme::primary_button(ui, "▶  Send", can).clicked() || send_key) && can {
                        let msg = t.input.trim().to_string();
                        t.input.clear();
                        if running {
                            t.queue.push_back(msg.clone());
                            t.push(Role::You, format!("{msg}\n(queued — sent when the agent finishes this step)"));
                        } else if let Some(exe) = t.kind.find() {
                            t.send(msg, exe, &ctx);
                        } else {
                            t.push(Role::Error, format!("{} isn't installed any more.", t.kind.name()));
                        }
                    }
                });
                ui.label(
                    RichText::new("Enter sends · Shift+Enter new line")
                        .small()
                        .color(p.faint),
                );
            });

        // ---- the terminal
        if t.pane == Pane::Terminal {
            egui::CentralPanel::default()
                .frame(theme::panel(p.base, 14, 8))
                .show(ui, |ui| t.terminal_ui(ui, running, &ctx));
            return out;
        }

        // ---- the conversation
        egui::CentralPanel::default()
            .frame(theme::panel(p.base, 14, 8))
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .stick_to_bottom(true)
                    .auto_shrink(false)
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        for l in &t.log {
                            chat_line(ui, l, t.kind);
                        }
                        if running {
                            ui.add_space(4.0);
                            ui.horizontal(|ui| {
                                ui.add(egui::Spinner::new().size(12.0));
                                ui.label(
                                    RichText::new(format!("{} is working…", t.kind.name()))
                                        .color(p.faint),
                                );
                            });
                        }
                    });
            });
        out
    }
}

fn status_label(s: Status, p: &theme::Palette) -> (&'static str, egui::Color32) {
    match s {
        Status::Running => ("working", p.accent_hover),
        Status::Idle => ("✔ waiting for you", p.green),
        Status::Failed => ("⚠ stopped with an error", p.red),
        Status::Stopped => ("stopped", p.faint),
        Status::Merged => ("✔ merged", p.purple),
    }
}

fn chat_line(ui: &mut egui::Ui, l: &Line, kind: AgentKind) {
    let p = pal(ui);
    match l.role {
        Role::You | Role::Agent => {
            let you = l.role == Role::You;
            ui.add_space(6.0);
            ui.label(
                RichText::new(if you { "You" } else { kind.name() })
                    .small()
                    .strong()
                    .color(if you { p.accent_hover } else { p.green }),
            );
            egui::Frame::new()
                .fill(if you {
                    theme::alpha(p.accent, 0.14)
                } else {
                    p.surface0
                })
                .corner_radius(10.0)
                .inner_margin(egui::Margin::symmetric(12, 8))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.add(
                        egui::Label::new(RichText::new(&l.text).color(p.text_strong))
                            .wrap()
                            .selectable(true),
                    );
                });
        }
        Role::Tool => {
            ui.horizontal(|ui| {
                ui.add_space(8.0);
                ui.label(RichText::new("⚙").small().color(p.faint));
                ui.add(
                    egui::Label::new(RichText::new(&l.text).monospace().small().color(p.subtext))
                        .truncate(),
                );
            });
        }
        Role::Info => {
            ui.add(egui::Label::new(RichText::new(&l.text).small().color(p.faint)).wrap());
        }
        Role::Error => {
            ui.add(egui::Label::new(RichText::new(format!("⚠ {}", l.text)).color(p.red)).wrap());
        }
    }
}

impl Task {
    fn push(&mut self, role: Role, text: String) {
        self.log.push(Line { role, text });
        if self.log.len() > MAX_LOG {
            self.log.drain(..self.log.len() - MAX_LOG);
        }
    }

    fn apply(&mut self, ev: Ev) {
        match ev {
            Ev::Session(id) => self.session = Some(id),
            Ev::Say(t) => self.push(Role::Agent, t),
            Ev::SayDelta(d) => match self.log.last_mut() {
                Some(l) if l.role == Role::Agent => l.text.push_str(&d),
                _ => self.push(Role::Agent, d),
            },
            Ev::Act(a) => self.push(Role::Tool, a),
            Ev::Note(n) => self.push(Role::Info, n),
            Ev::Result { ok, text, cost } => {
                if let Some(c) = cost {
                    self.cost += c;
                }
                // Claude repeats its last message as the result; only show new text.
                if let Some(t) = text {
                    let dup = self
                        .log
                        .iter()
                        .rev()
                        .find(|l| l.role == Role::Agent)
                        .is_some_and(|l| l.text.trim() == t.trim());
                    if !dup {
                        self.push(if ok { Role::Agent } else { Role::Error }, t);
                    }
                }
                if !ok {
                    self.status = Status::Failed;
                }
            }
            Ev::Fail(e) => {
                self.push(Role::Error, e);
                self.status = Status::Failed;
            }
        }
    }

    /// Run the agent with this message (creating the worktree first if needed).
    fn send(&mut self, prompt: String, exe: PathBuf, ctx: &egui::Context) {
        self.push(Role::You, prompt.clone());
        self.status = Status::Running;
        self.started = Some(Instant::now());
        let resume = self
            .kind
            .can_resume()
            .then(|| self.session.clone())
            .flatten();
        let spec = RunSpec {
            kind: self.kind,
            exe,
            repo: self.repo.clone(),
            worktree: self.worktree.clone(),
            branch: self.branch.clone(),
            base: self.base.clone(),
            create: !self.worktree.exists(),
            prompt,
            resume,
            run_commands: self.run_commands,
        };
        self.runner = Some(spawn_run(spec, ctx.clone()));
    }

    fn stop(&mut self) {
        if let Some(r) = &self.runner {
            if let Ok(mut c) = r.child.lock() {
                if let Some(child) = c.as_mut() {
                    let _ = child.kill();
                }
            }
            self.status = Status::Stopped;
            self.push(Role::Info, "Stopped.".into());
        }
    }

    /// Push & PR, if the main repository has a remote.
    fn push_action(&self) -> Option<TaskAction> {
        let repo = git2::Repository::open(&self.repo).ok()?;
        let names = repo.remotes().ok()?;
        let mut names: Vec<String> = names.iter().flatten().flatten().map(String::from).collect();
        names.sort_by_key(|n| n != "origin");
        let remote = names.into_iter().next()?;
        let url = repo
            .find_remote(&remote)
            .ok()?
            .url()
            .ok()
            .map(String::from)
            .unwrap_or_default();
        let compare_url = crate::github::repo_slug(&url).map(|slug| {
            format!(
                "https://github.com/{slug}/compare/{}...{}?expand=1",
                self.base, self.branch
            )
        });
        Some(TaskAction::PushPr {
            repo: self.repo.clone(),
            worktree: self.worktree.clone(),
            branch: self.branch.clone(),
            remote,
            message: format!(
                "{}\n\nDone with {} in FoxGit.\n",
                self.title,
                self.kind.name()
            ),
            commit_first: self.uncommitted() > 0,
            compare_url,
            base: self.base.clone(),
            title: self.title.clone(),
        })
    }

    /// The Terminal pane: a shell in the agent's copy, plus "run the agent here".
    fn terminal_ui(&mut self, ui: &mut egui::Ui, running: bool, ctx: &egui::Context) {
        let p = pal(ui);
        if !self.worktree_ready {
            ui.centered_and_justified(|ui| {
                ui.label(
                    RichText::new(
                        "The agent's copy of the repository is created with the first message.",
                    )
                    .weak(),
                );
            });
            return;
        }
        if self.terminal.is_none() && self.term_error.is_none() {
            match crate::terminal::Terminal::spawn(&self.worktree, ctx) {
                Ok(t) => self.terminal = Some(t),
                Err(e) => self.term_error = Some(e),
            }
        }
        ui.horizontal(|ui| {
            let exe = self.kind.find();
            let resume = self.kind.can_resume().then(|| self.session.clone()).flatten();
            let label = match &resume {
                Some(_) => format!("▶  Continue in {}", self.kind.name()),
                None => format!("▶  Start {}", self.kind.name()),
            };
            let can = exe.is_some() && !running && self.terminal.as_ref().is_some_and(|t| !t.exited());
            if ui
                .add_enabled(can, egui::Button::new(label))
                .on_hover_text(if running {
                    "Wait until the chat run finishes (or stop it) — both would work on the same session"
                } else {
                    "Run the agent's own full-screen interface here, in its copy of the repository"
                })
                .clicked()
            {
                if let (Some(exe), Some(term)) = (exe, self.terminal.as_mut()) {
                    term.run_line(&self.kind.interactive_line(&exe, resume.as_deref()));
                }
            }
            if ui.button("⟳  New shell").on_hover_text("Close this shell and start a new one").clicked() {
                self.terminal = None;
                self.term_error = None;
            }
            if ui.button("Copy screen").clicked() {
                if let Some(t) = &self.terminal {
                    ui.ctx().copy_text(t.contents());
                }
            }
            ui.label(
                RichText::new(format!("in {}", self.worktree.display()))
                    .small()
                    .color(p.faint),
            );
        });
        ui.add_space(6.0);
        match (&mut self.terminal, &self.term_error) {
            (Some(t), _) => t.ui(ui),
            (None, Some(e)) => {
                ui.label(RichText::new(format!("Could not open a terminal: {e}")).color(p.red));
            }
            _ => {}
        }
    }

    fn merge_action(&self, squash: bool) -> TaskAction {
        TaskAction::Merge {
            task: self.id,
            repo: self.repo.clone(),
            worktree: self.worktree.clone(),
            branch: self.branch.clone(),
            message: format!(
                "{}\n\nDone with {} in FoxGit.\n",
                self.title,
                self.kind.name()
            ),
            commit_first: self.uncommitted() > 0,
            squash,
        }
    }

    fn uncommitted(&self) -> usize {
        git::Repo::open(&self.worktree)
            .and_then(|r| r.status())
            .map(|s| s.staged.len() + s.unstaged.len())
            .unwrap_or(0)
    }

    /// Count changed files in the worktree and commits on the branch.
    fn refresh_changes(&mut self) {
        let files = self.uncommitted();
        let commits = git2::Repository::open(&self.worktree)
            .ok()
            .and_then(|r| {
                let head = r.head().ok()?.target()?;
                let base = r
                    .revparse_single(&self.base)
                    .ok()?
                    .peel_to_commit()
                    .ok()?
                    .id();
                r.graph_ahead_behind(head, base).ok().map(|(a, _)| a)
            })
            .unwrap_or(0);
        self.changes = (files, commits);
    }
}

struct RunSpec {
    kind: AgentKind,
    exe: PathBuf,
    repo: PathBuf,
    worktree: PathBuf,
    branch: String,
    base: String,
    create: bool,
    prompt: String,
    resume: Option<String>,
    run_commands: bool,
}

fn spawn_run(spec: RunSpec, ctx: egui::Context) -> Runner {
    let (tx, rx) = channel::<Msg>();
    let child: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));
    let slot = child.clone();
    std::thread::spawn(move || {
        let send = |tx: &Sender<Msg>, m: Msg| {
            let _ = tx.send(m);
            ctx.request_repaint();
        };
        if spec.create {
            if let Some(parent) = spec.worktree.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let out = git::run_git(
                &spec.repo,
                &[
                    "worktree".into(),
                    "add".into(),
                    "-b".into(),
                    spec.branch.clone(),
                    spec.worktree.display().to_string(),
                    spec.base.clone(),
                ],
                None,
            );
            if !out.ok {
                send(
                    &tx,
                    Msg::Failed(format!(
                        "Could not create the agent's worktree:\n{}",
                        out.text
                    )),
                );
                return;
            }
        }
        let (mut cmd, stdin_text) = spec.kind.command(
            &spec.exe,
            &spec.prompt,
            &spec.worktree,
            spec.resume.as_deref(),
            spec.run_commands,
        );
        cmd.current_dir(&spec.worktree)
            .stdin(if stdin_text.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(p) = shellenv::path() {
            cmd.env("PATH", p);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let mut c = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                send(
                    &tx,
                    Msg::Failed(format!("Could not start {}: {e}", spec.kind.name())),
                );
                return;
            }
        };
        if let (Some(text), Some(mut stdin)) = (stdin_text, c.stdin.take()) {
            let _ = stdin.write_all(text.as_bytes());
        }
        let stdout = c.stdout.take();
        let stderr = c.stderr.take();
        if let Ok(mut s) = slot.lock() {
            *s = Some(c);
        }
        if let Some(err) = stderr {
            let tx = tx.clone();
            let ctx = ctx.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(err).lines().map_while(Result::ok) {
                    let line = line.trim().to_string();
                    if !line.is_empty() {
                        let _ = tx.send(Msg::Stderr(line));
                        ctx.request_repaint();
                    }
                }
            });
        }
        if let Some(out) = stdout {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                for ev in parse_line(spec.kind, &line, &spec.worktree) {
                    send(&tx, Msg::Ev(ev));
                }
            }
        }
        let code = slot
            .lock()
            .ok()
            .and_then(|mut s| s.take())
            .and_then(|mut c| c.wait().ok())
            .and_then(|st| st.code());
        send(&tx, Msg::Exited(code));
    });
    Runner {
        rx,
        child,
        got_result: false,
    }
}

/// `fix-the-login-timeout-k3f2` from the first words of the prompt.
pub fn slug(prompt: &str, id: u64) -> String {
    let words: Vec<String> = prompt
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .take(5)
        .map(|w| w.to_lowercase())
        .collect();
    let mut base = words.join("-");
    base.truncate(40);
    let base = base.trim_matches('-');
    let tag = format!("{:x}", (id * 2654435761) % 0xffff);
    if base.is_empty() {
        format!("task-{tag}")
    } else {
        format!("{base}-{tag}")
    }
}

fn sanitize(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let s = s.trim_matches('-');
    if s.is_empty() {
        "repo".into()
    } else {
        s.to_string()
    }
}

fn path_hash(p: &Path) -> u64 {
    p.to_string_lossy()
        .bytes()
        .fold(0xcbf29ce484222325u64, |h, b| {
            (h ^ b as u64).wrapping_mul(0x100000001b3)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::Op;

    #[test]
    fn slugs() {
        assert!(
            slug("Fix the login timeout bug, please!", 1).starts_with("fix-the-login-timeout-bug-")
        );
        assert!(slug("แก้บั๊ก login", 2).starts_with("login-"));
        assert!(slug("ภาษาไทยล้วน", 3).starts_with("task-"));
    }

    #[test]
    fn parses_claude_stream() {
        let cwd = Path::new("/w");
        let k = AgentKind::Claude;
        assert_eq!(
            parse_line(
                k,
                r#"{"type":"system","subtype":"init","session_id":"abc","model":"x"}"#,
                cwd
            ),
            vec![Ev::Session("abc".into())]
        );
        assert_eq!(
            parse_line(
                k,
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Looking."},{"type":"tool_use","name":"Edit","input":{"file_path":"/w/src/a.rs"}},{"type":"tool_use","name":"Bash","input":{"command":"cargo test\nmore"}}]}}"#,
                cwd
            ),
            vec![
                Ev::Say("Looking.".into()),
                Ev::Act("Edit src/a.rs".into()),
                Ev::Act("$ cargo test".into())
            ]
        );
        assert_eq!(
            parse_line(
                k,
                r#"{"type":"result","subtype":"success","is_error":false,"result":"Done.","total_cost_usd":0.25,"session_id":"abc"}"#,
                cwd
            ),
            vec![
                Ev::Session("abc".into()),
                Ev::Result {
                    ok: true,
                    text: Some("Done.".into()),
                    cost: Some(0.25)
                }
            ]
        );
        assert_eq!(
            parse_line(k, "plain text", cwd),
            vec![Ev::Note("plain text".into())]
        );
    }

    #[test]
    fn parses_codex_and_generic_events() {
        let cwd = Path::new("/w");
        assert_eq!(
            parse_line(
                AgentKind::Codex,
                r#"{"type":"thread.started","thread_id":"t1"}"#,
                cwd
            ),
            vec![Ev::Session("t1".into())]
        );
        assert_eq!(
            parse_line(
                AgentKind::Codex,
                r#"{"type":"item.started","item":{"type":"command_execution","command":"ls"}}"#,
                cwd
            ),
            vec![Ev::Act("$ ls".into())]
        );
        assert_eq!(
            parse_line(
                AgentKind::Codex,
                r#"{"type":"item.completed","item":{"type":"agent_message","text":"Hi"}}"#,
                cwd
            ),
            vec![Ev::Say("Hi".into())]
        );
        assert_eq!(
            parse_line(
                AgentKind::Hermes,
                r#"{"type":"text_delta","delta":"He"}"#,
                cwd
            ),
            vec![Ev::SayDelta("He".into())]
        );
        assert_eq!(
            parse_line(
                AgentKind::Hermes,
                r#"{"type":"tool_call","name":"terminal","args":{"command":"ls -la"}}"#,
                cwd
            ),
            vec![Ev::Act("terminal: ls -la".into())]
        );
        assert_eq!(
            parse_line(
                AgentKind::Hermes,
                r#"{"type":"final","response":"All set","session_id":"s9"}"#,
                cwd
            ),
            vec![
                Ev::Session("s9".into()),
                Ev::Result {
                    ok: true,
                    text: Some("All set".into()),
                    cost: None
                }
            ]
        );

        // real Hermes v0.23 stream-json lines
        let h = AgentKind::Hermes;
        assert!(parse_line(
            h,
            r#"{"type": "system", "subtype": "init", "model": "", "session_id": "", "timestamp": 1}"#,
            cwd
        )
        .is_empty());
        assert_eq!(
            parse_line(
                h,
                r#"{"type": "tool_use", "name": "terminal", "tool_call_id": "t1", "input": {"command": "ls"}}"#,
                cwd
            ),
            vec![Ev::Act("terminal: ls".into())]
        );
        assert!(parse_line(
            h,
            r#"{"type": "tool_result", "name": "terminal", "tool_call_id": "t1", "output": "{}", "is_error": false}"#,
            cwd
        )
        .is_empty());
        assert_eq!(
            parse_line(
                h,
                r#"{"type": "text", "text": "\n\n1", "timestamp": 2}"#,
                cwd
            ),
            vec![Ev::Say("1".into())]
        );
        assert_eq!(
            parse_line(
                h,
                r#"{"type": "result", "session_id": "local-abc", "exit_code": 0, "text": "1", "tokens": {"input": 5}}"#,
                cwd
            ),
            vec![
                Ev::Session("local-abc".into()),
                Ev::Result {
                    ok: true,
                    text: Some("1".into()),
                    cost: None
                }
            ]
        );
        assert_eq!(
            parse_line(
                h,
                r#"{"type": "result", "session_id": "local-abc", "exit_code": 1, "text": "boom"}"#,
                cwd
            )[1],
            Ev::Result {
                ok: false,
                text: Some("boom".into()),
                cost: None
            }
        );
    }

    /// The whole pipeline with a stand-in agent: worktree on a new branch, the CLI run there
    /// with the prompt on stdin, its JSON lines turned into chat lines, the follow-up resumed.
    #[test]
    #[cfg(unix)]
    fn fake_agent_runs_in_its_own_worktree() {
        use std::os::unix::fs::PermissionsExt;
        let repo = tempfile::tempdir().unwrap();
        let r = repo.path();
        let git = |args: &[&str]| {
            let out = git::run_git(
                r,
                &args.iter().map(|a| a.to_string()).collect::<Vec<_>>(),
                None,
            );
            assert!(out.ok, "{}", out.text);
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.name", "T"]);
        git(&["config", "user.email", "t@example.com"]);
        std::fs::write(r.join("a.txt"), "a\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "first"]);

        // a fake `claude`: echoes its arguments and stdin, edits a file, prints stream-json
        let bin = tempfile::tempdir().unwrap();
        let exe = bin.path().join("claude");
        std::fs::write(
            &exe,
            r#"#!/bin/sh
prompt=$(cat)
echo "$*" > args.txt
echo "agent was here" > agent.txt
echo '{"type":"system","subtype":"init","session_id":"sess-1"}'
printf '{"type":"assistant","message":{"content":[{"type":"text","text":"Got: %s"},{"type":"tool_use","name":"Write","input":{"file_path":"%s/agent.txt"}}]}}
' "$prompt" "$PWD"
echo '{"type":"result","subtype":"success","is_error":false,"result":"done","total_cost_usd":0.5,"session_id":"sess-1"}'
"#,
        )
        .unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();

        let ctx = egui::Context::default();
        let mut agents = Agents::new(&ctx, Vec::new(), None);
        let wt_root = tempfile::tempdir().unwrap();
        agents.worktree_root = wt_root.path().to_path_buf();
        let id = agents.create(
            NewTask {
                repo: r.to_path_buf(),
                repo_name: "demo".into(),
                base: "main".into(),
                prompt: "make a file".into(),
                kind: AgentKind::Claude,
                run_commands: false,
                installed: Vec::new(),
                focus: false,
            },
            exe.clone(),
        );
        assert!(agents
            .tasks
            .iter()
            .any(|t| t.id == id && t.runner.is_some()));
        assert!(agents.tasks[0].worktree.starts_with(wt_root.path()));
        let wait = |agents: &mut Agents| {
            for _ in 0..200 {
                agents.poll();
                if !agents.any_running() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            panic!("the fake agent did not finish");
        };
        wait(&mut agents);
        let t = agents.tasks.iter().find(|t| t.id == id).unwrap();
        assert_eq!(
            t.status,
            Status::Idle,
            "{:?}",
            t.log.iter().map(|l| &l.text).collect::<Vec<_>>()
        );
        assert_eq!(t.session.as_deref(), Some("sess-1"));
        assert!(t
            .log
            .iter()
            .any(|l| l.role == Role::Agent && l.text == "Got: make a file"));
        assert!(t
            .log
            .iter()
            .any(|l| l.role == Role::Tool && l.text == "Write agent.txt"));
        assert!((t.cost - 0.5).abs() < 1e-9);
        assert_eq!(
            std::fs::read_to_string(t.worktree.join("agent.txt")).unwrap(),
            "agent was here\n"
        );
        assert!(
            !r.join("agent.txt").exists(),
            "the user's own folder is untouched"
        );
        assert_eq!(
            t.changes.0, 2,
            "agent.txt and args.txt are new in the worktree"
        );
        let args = std::fs::read_to_string(t.worktree.join("args.txt")).unwrap();
        assert!(
            args.contains("--permission-mode acceptEdits") && !args.contains("--resume"),
            "{args}"
        );

        // a reply resumes the same session in the same worktree
        let worktree = t.worktree.clone();
        let t = agents.tasks.iter_mut().find(|t| t.id == id).unwrap();
        t.send("and another".into(), exe.clone(), &ctx);
        wait(&mut agents);
        let args = std::fs::read_to_string(worktree.join("args.txt")).unwrap();
        assert!(args.contains("--resume sess-1"), "{args}");

        // Discard removes the worktree and its branch
        let out = Op::AgentCleanup {
            worktree: worktree.display().to_string(),
            branch: format!("agent/{}", worktree.file_name().unwrap().to_string_lossy()),
            delete_branch: true,
        }
        .run(r);
        assert!(out.ok, "{}", out.text);
    }

    #[test]
    fn deltas_join_into_one_message() {
        let mut t = Task {
            id: 1,
            kind: AgentKind::Hermes,
            title: "t".into(),
            repo: PathBuf::new(),
            worktree: PathBuf::new(),
            branch: String::new(),
            base: String::new(),
            run_commands: false,
            status: Status::Running,
            session: None,
            log: Vec::new(),
            cost: 0.0,
            changes: (0, 0),
            worktree_ready: false,
            queue: VecDeque::new(),
            input: String::new(),
            started: None,
            runner: None,
            pane: Pane::Chat,
            terminal: None,
            term_error: None,
        };
        t.apply(Ev::SayDelta("Hel".into()));
        t.apply(Ev::SayDelta("lo".into()));
        t.apply(Ev::Result {
            ok: true,
            text: Some("Hello".into()),
            cost: Some(0.1),
        });
        assert_eq!(
            t.log.len(),
            1,
            "the result repeats the message and is not shown twice"
        );
        assert_eq!(t.log[0].text, "Hello");
        assert!((t.cost - 0.1).abs() < 1e-9);
    }
}
