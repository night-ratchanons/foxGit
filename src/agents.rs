//! AI coding agents (Claude Code, Codex, Hermes) driven from FoxGit.
//!
//! A task is a chat with one agent. Its first message creates a git worktree on an
//! `agent/<slug>` branch (the user's own working copy is never touched) and runs the agent's
//! CLI there headless; every later message runs it again, resuming the same agent session, so
//! the conversation carries on. Messages typed while the agent is busy wait in a queue. Output
//! (JSON lines) is parsed into chat lines: the agent's words, its tool calls, errors.
//! When done, the work is reviewed (worktree opened as a tab), merged, or discarded.

use crate::git;
use crate::prompts::{Profile, Snippet};
use crate::shellenv;
use crate::theme::{self, pal};
use eframe::egui;
use egui::{vec2, RichText};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
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

/// How one turn runs: model, plan mode, whether it may run commands.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RunOpts {
    pub run_commands: bool,
    /// Empty = the agent's default.
    pub model: String,
    /// Think and propose first, don't change files.
    pub plan: bool,
}

const PLAN_PREFIX: &str =
    "Plan only for now: explain step by step what you would change and why. Don't edit files or run commands that change anything until I say go.\n\n";

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
        o: &RunOpts,
    ) -> (Command, Option<String>) {
        let run_commands = o.run_commands;
        let mut c = Command::new(exe);
        // Claude Code has a real plan mode; the others get told
        let plan_text;
        let prompt = if o.plan && self != AgentKind::Claude {
            plan_text = format!("{PLAN_PREFIX}{prompt}");
            plan_text.as_str()
        } else {
            prompt
        };
        match self {
            AgentKind::Claude => {
                c.args(["-p", "--output-format", "stream-json", "--verbose"]);
                c.args([
                    "--permission-mode",
                    if o.plan {
                        "plan"
                    } else if run_commands {
                        "auto"
                    } else {
                        "acceptEdits"
                    },
                ]);
                if !o.model.is_empty() {
                    c.args(["--model", &o.model]);
                }
                if let Some(s) = resume {
                    c.args(["--resume", s]);
                }
                // the prompt goes in on stdin: no length limit, nothing interpreted
                (c, Some(prompt.to_string()))
            }
            AgentKind::Codex => {
                // Codex sandboxes its commands to the working directory (--full-auto).
                c.args(["exec", "--json", "--full-auto", "--skip-git-repo-check"]);
                if !o.model.is_empty() {
                    c.args(["-m", &o.model]);
                }
                c.arg("-C").arg(cwd).arg(prompt);
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
                if !o.model.is_empty() {
                    c.args(["-m", &o.model]);
                }
                if let Some(s) = resume {
                    c.args(["--resume", s]);
                }
                if run_commands && !o.plan {
                    c.arg("--yolo");
                }
                (c, Some(prompt.to_string()))
            }
        }
    }

    /// Command line for the agent's own full-screen interface, resuming a session and/or
    /// starting with a first message.
    fn interactive_line(
        self,
        exe: &Path,
        resume: Option<&str>,
        prompt: Option<&str>,
        o: &RunOpts,
    ) -> String {
        let q = shell_quote(&exe.to_string_lossy());
        let first = prompt.map(str::trim).filter(|p| !p.is_empty()).map(|p| {
            if o.plan && self != AgentKind::Claude {
                shell_quote(&format!("{PLAN_PREFIX}{p}"))
            } else {
                shell_quote(p)
            }
        });
        let mut c = match self {
            AgentKind::Hermes => format!("{q} chat"),
            _ => q,
        };
        if !o.model.is_empty() {
            let flag = if self == AgentKind::Claude {
                "--model"
            } else {
                "-m"
            };
            c.push_str(&format!(" {flag} {}", shell_quote(&o.model)));
        }
        if o.plan && self == AgentKind::Claude {
            c.push_str(" --permission-mode plan");
        }
        if let (Some(s), true) = (resume, self != AgentKind::Codex) {
            c.push_str(&format!(" --resume {}", shell_quote(s)));
        }
        if let Some(f) = first {
            // Hermes: on a TTY, -q seeds the interactive session with this first turn
            if self == AgentKind::Hermes {
                c.push_str(&format!(" -q {f}"));
            } else {
                c.push_str(&format!(" {f}"));
            }
        }
        c
    }

    /// Whether a later message can continue the agent's own session.
    fn can_resume(self) -> bool {
        !matches!(self, AgentKind::Codex)
    }
}

/// Quote for POSIX shells (and good enough for PowerShell when there are no quotes inside).
pub fn shell_quote(s: &str) -> String {
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

/// Quitting FoxGit stops agent runs: nobody would read their output any more, and the next
/// message resumes the same session.
impl Drop for Agents {
    fn drop(&mut self) {
        for t in &mut self.tasks {
            if let Some(r) = &t.runner {
                if let Ok(mut c) = r.child.lock() {
                    if let Some(child) = c.as_mut() {
                        let _ = child.kill();
                        let _ = child.wait();
                    }
                }
            }
        }
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
    /// Chat, Changes or Terminal.
    pub pane: Pane,
    /// Terminals in the agent's copy: its own interface, shells, scripts.
    run: crate::runner::RunPanel,
    /// Model for this chat ("" = the agent's default).
    pub model: String,
    /// Plan mode: propose before changing anything.
    pub plan: bool,
    /// Profile (separate account / config folder of the CLI).
    pub profile: Option<String>,
    envs: Vec<(String, String)>,
    /// Chats started together on the same task, to compare (id of the first one).
    pub group: Option<u64>,
    review: crate::agent_review::Review,
    /// Filter of the "attach file" picker.
    attach: String,
    renaming: Option<String>,
    /// Started in Chat or Terminal mode.
    mode: StartMode,
    /// Command to type into the terminal once it's open (the agent's interface).
    launch: Option<String>,
    /// The worktree being created for a Terminal-mode chat.
    creating: Option<Receiver<Result<(), String>>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Pane {
    #[default]
    Chat,
    Changes,
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
    #[serde(default)]
    mode: StartMode,
    #[serde(default)]
    model: String,
    #[serde(default)]
    plan: bool,
    #[serde(default)]
    profile: Option<String>,
    #[serde(default)]
    group: Option<u64>,
}

/// One row of the sidebar's AGENTS section.
pub struct RowInfo {
    pub id: u64,
    pub title: String,
    pub status: Status,
    pub kind: AgentKind,
    pub queued: usize,
    /// Started in Terminal mode.
    pub terminal: bool,
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
    /// Merge the base branch's new commits into the agent's copy.
    Sync {
        repo: PathBuf,
        worktree: PathBuf,
        base: String,
    },
    /// Show the board of this repository's chats (to compare runs).
    Board,
}

/// What the board asks the app to do.
pub enum BoardAction {
    /// Open the New chat dialog (mode already set in `last_mode`).
    New,
    Open(u64),
    Act(TaskAction),
}

/// How a new chat starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum StartMode {
    /// FoxGit's chat view (the agent runs headless per message).
    #[default]
    Chat,
    /// The agent's own full-screen interface in the built-in terminal.
    Terminal,
}

struct NewTask {
    repo: PathBuf,
    repo_name: String,
    base: String,
    prompt: String,
    /// The first picked agent.
    kind: AgentKind,
    /// More agents to run the same task on, side by side (to compare).
    also: Vec<AgentKind>,
    /// Runs per agent.
    copies: usize,
    mode: StartMode,
    run_commands: bool,
    model: String,
    plan: bool,
    profile: Option<String>,
    installed: Vec<(AgentKind, Option<PathBuf>)>,
    focus: bool,
}

pub struct Agents {
    tasks: Vec<Task>,
    next_id: u64,
    pub last_kind: AgentKind,
    /// Chat or Terminal, as picked last time in New chat.
    pub last_mode: StartMode,
    /// Agent profiles (separate accounts / config folders), from Settings.
    pub profiles: Vec<Profile>,
    /// Your own prompt snippets (Settings), shown with the built-in ones.
    pub snippets: Vec<Snippet>,
    /// Files of each worktree for the attach picker (refreshed every few seconds).
    files: HashMap<PathBuf, (Instant, Vec<String>)>,
    /// Several runs were just started: the app shows the board instead of one chat.
    pub show_board: bool,
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
    pub fn new(
        ctx: &egui::Context,
        saved: Vec<SavedTask>,
        last_kind: Option<AgentKind>,
        profiles: Vec<Profile>,
        snippets: Vec<Snippet>,
    ) -> Self {
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
                let envs = profile_env(&profiles, t.profile.as_deref(), t.kind);
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
                    pane: if t.mode == StartMode::Terminal { Pane::Terminal } else { Pane::Chat },
                    run: crate::runner::RunPanel::embedded(envs.clone()),
                    model: t.model,
                    plan: t.plan,
                    profile: t.profile,
                    envs,
                    group: t.group,
                    review: Default::default(),
                    attach: String::new(),
                    renaming: None,
                    mode: t.mode,
                    launch: None,
                    creating: None,
                };
                task.refresh_changes();
                task
            })
            .collect();
        Self {
            tasks,
            next_id,
            last_kind: last_kind.unwrap_or(AgentKind::Claude),
            last_mode: StartMode::Chat,
            profiles,
            snippets,
            files: HashMap::new(),
            show_board: false,
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
                mode: t.mode,
                model: t.model.clone(),
                plan: t.plan,
                profile: t.profile.clone(),
                group: t.group,
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
                terminal: t.mode == StartMode::Terminal,
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
                        terminal: t.mode == StartMode::Terminal,
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
                ui.horizontal(|ui| {
                    let w = 330.0;
                    ui.add_space(((ui.available_width() - w) / 2.0).max(0.0));
                    if theme::primary_button(ui, "💬  New chat", any).clicked() {
                        self.last_mode = StartMode::Chat;
                        clicked = true;
                    }
                    if ui
                        .add_enabled(any, egui::Button::new("⌨  New terminal session").min_size(vec2(0.0, 28.0)))
                        .on_hover_text("The agent's own full-screen interface, in its own copy of the repository")
                        .clicked()
                    {
                        self.last_mode = StartMode::Terminal;
                        clicked = true;
                    }
                });
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

    /// The page shown when no chat is open: every chat of `repo` as a card; runs of the same
    /// task side by side. Falls back to the intro when there are none.
    pub fn board(&mut self, ui: &mut egui::Ui, repo: &Path) -> Vec<BoardAction> {
        let mut out = Vec::new();
        if !self.tasks.iter().any(|t| t.repo == repo) {
            if self.empty_view(ui) {
                out.push(BoardAction::New);
            }
            return out;
        }
        let installed = self.installed().iter().any(|(_, ok)| *ok);
        egui::CentralPanel::default()
            .frame(theme::panel(pal(ui).base, 18, 14))
            .show(ui, |ui| {
                let p = pal(ui);
                let mine: Vec<&Task> = self.tasks.iter().rev().filter(|t| t.repo == repo).collect();
                let running = mine.iter().filter(|t| t.runner.is_some()).count();
                let cost: f64 = mine.iter().map(|t| t.cost).sum();
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("Agents")
                            .size(20.0)
                            .strong()
                            .color(p.text_strong),
                    );
                    let mut info = format!("{} chat(s)", mine.len());
                    if running > 0 {
                        info.push_str(&format!(" · {running} working"));
                    }
                    if cost > 0.0 {
                        info.push_str(&format!(" · ≈ ${cost:.2} at API prices"));
                    }
                    ui.label(RichText::new(info).color(p.subtext));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add_enabled(installed, egui::Button::new("⌨  New terminal session"))
                            .clicked()
                        {
                            self.last_mode = StartMode::Terminal;
                            out.push(BoardAction::New);
                        }
                        if theme::primary_button(ui, "💬  New chat", installed).clicked() {
                            self.last_mode = StartMode::Chat;
                            out.push(BoardAction::New);
                        }
                    });
                });
                ui.add_space(10.0);
                egui::ScrollArea::vertical()
                    .auto_shrink(false)
                    .show(ui, |ui| {
                        let mut seen = std::collections::HashSet::new();
                        let mut singles = Vec::new();
                        for t in &mine {
                            match t.group {
                                Some(g) if seen.insert(g) => {
                                    let runs: Vec<&&Task> =
                                        mine.iter().filter(|x| x.group == Some(g)).collect();
                                    let title = runs
                                        .iter()
                                        .min_by_key(|x| x.id)
                                        .map(|x| {
                                            x.title
                                                .split(" · ")
                                                .next()
                                                .unwrap_or(&x.title)
                                                .to_string()
                                        })
                                        .unwrap_or_default();
                                    ui.label(
                                        RichText::new(format!("COMPARE · {title}"))
                                            .small()
                                            .strong()
                                            .color(p.faint),
                                    );
                                    ui.add_space(4.0);
                                    ui.horizontal_wrapped(|ui| {
                                        ui.spacing_mut().item_spacing = vec2(10.0, 10.0);
                                        let mut runs = runs.clone();
                                        runs.sort_by_key(|x| x.id);
                                        for t in runs {
                                            board_card(ui, t, true, &mut out);
                                        }
                                    });
                                    ui.add_space(14.0);
                                }
                                Some(_) => {}
                                None => singles.push(*t),
                            }
                        }
                        if !singles.is_empty() {
                            if !seen.is_empty() {
                                ui.label(RichText::new("CHATS").small().strong().color(p.faint));
                                ui.add_space(4.0);
                            }
                            ui.horizontal_wrapped(|ui| {
                                ui.spacing_mut().item_spacing = vec2(10.0, 10.0);
                                for t in singles {
                                    board_card(ui, t, false, &mut out);
                                }
                            });
                        }
                    });
            });
        out
    }

    /// New profile list from Settings; chats re-read their profile's environment.
    pub fn set_profiles(&mut self, profiles: Vec<Profile>) {
        self.profiles = profiles;
        for t in &mut self.tasks {
            t.envs = profile_env(&self.profiles, t.profile.as_deref(), t.kind);
            t.run.envs = t.envs.clone();
        }
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
            also: Vec::new(),
            copies: 1,
            mode: self.last_mode,
            run_commands: true,
            model: String::new(),
            plan: false,
            profile: None,
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
            // how to work with it: FoxGit's chat, or the agent's own interface in a terminal
            let mut idx = usize::from(f.mode == StartMode::Terminal);
            if theme::segmented(ui, &["💬  Chat", "⌨  Terminal"], &mut idx) {
                f.mode = if idx == 1 { StartMode::Terminal } else { StartMode::Chat };
                f.focus = true;
            }
            ui.label(
                RichText::new(match f.mode {
                    StartMode::Chat => "Chat here: FoxGit shows what it says, the files it edits and the commands it runs.",
                    StartMode::Terminal => "Open the agent's own full-screen interface in a terminal (slash commands, approvals, everything it can do).",
                })
                .small()
                .color(p.subtext),
            );
            ui.add_space(8.0);
            let profiles = &self.profiles;
            ui.horizontal(|ui| {
                for (k, path) in &f.installed {
                    let on = f.kind == *k || f.also.contains(k);
                    let label = if path.is_some() {
                        RichText::new(k.name())
                    } else {
                        RichText::new(format!("{} (not installed)", k.name())).color(p.faint)
                    };
                    let r = ui
                        .add_enabled(path.is_some(), egui::Button::selectable(on, label).min_size(vec2(0.0, 28.0)))
                        .on_hover_text(format!("{}-click to run several agents on the same task and compare", if cfg!(target_os = "macos") { "⌘" } else { "Ctrl" }));
                    if r.clicked() {
                        let multi = ui.input(|i| i.modifiers.command);
                        if !multi {
                            f.kind = *k;
                            f.also.clear();
                        } else if f.kind == *k {
                            if let Some(next) = f.also.first().copied() {
                                f.kind = next;
                                f.also.remove(0);
                            }
                        } else if let Some(i) = f.also.iter().position(|x| x == k) {
                            f.also.remove(i);
                        } else {
                            f.also.push(*k);
                        }
                        if f.also.contains(&f.kind) {
                            f.also.retain(|x| *x != f.kind);
                        }
                        f.model.clear();
                        f.profile = None;
                    }
                }
                ui.separator();
                ui.label(RichText::new("Runs").color(p.subtext));
                ui.add(egui::DragValue::new(&mut f.copies).range(1..=4).suffix(" ×"))
                    .on_hover_text("Run the same task several times (each in its own copy) and keep the best");
            });
            let total = (1 + f.also.len()) * f.copies;
            if total == 1 {
                ui.label(
                    RichText::new(format!(
                        "Tip: {}-click more agents (or raise Runs) to try the same task several ways and compare.",
                        if cfg!(target_os = "macos") { "⌘" } else { "Ctrl" }
                    ))
                    .small()
                    .color(p.faint),
                );
            }
            if total > 1 {
                ui.label(
                    RichText::new(format!(
                        "{total} chats will start side by side, each in its own copy — compare them on the board, merge the best, discard the rest."
                    ))
                    .small()
                    .color(p.accent_hover),
                );
            }
            if f.also.is_empty() {
                ui.horizontal(|ui| {
                    let models = crate::prompts::models(f.kind);
                    egui::ComboBox::from_id_salt("new_chat_model")
                        .selected_text(
                            models
                                .iter()
                                .find(|m| m.0 == f.model)
                                .map(|m| format!("Model: {}", m.1))
                                .unwrap_or_else(|| format!("Model: {}", if f.model.is_empty() { "Default" } else { &f.model })),
                        )
                        .width(190.0)
                        .show_ui(ui, |ui| {
                            for (v, label) in models {
                                ui.selectable_value(&mut f.model, v.to_string(), *label);
                            }
                            ui.separator();
                            ui.label(RichText::new("Other model id:").small().weak());
                            ui.text_edit_singleline(&mut f.model);
                        });
                    let mine: Vec<&Profile> = profiles.iter().filter(|x| x.kind == f.kind).collect();
                    if !mine.is_empty() {
                        egui::ComboBox::from_id_salt("new_chat_profile")
                            .selected_text(f.profile.clone().map(|n| format!("Profile: {n}")).unwrap_or_else(|| "Default profile".into()))
                            .width(170.0)
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut f.profile, None, "Default profile");
                                for pr in mine {
                                    ui.selectable_value(&mut f.profile, Some(pr.name.clone()), &pr.name);
                                }
                            });
                    }
                    ui.checkbox(&mut f.plan, "Plan first")
                        .on_hover_text("It proposes a plan and waits — untick Plan mode in the chat to let it work");
                });
            }
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
                    .hint_text(match f.mode {
                        StartMode::Chat => "What should the agent do?  e.g. Fix the login timeout bug and add a test",
                        StartMode::Terminal => "First message (optional) — or leave empty and type in the terminal",
                    })
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
            if f.mode == StartMode::Chat {
                ui.checkbox(&mut f.run_commands, "Let it run commands (tests, builds, installs)");
            }
            if f.mode == StartMode::Chat && f.run_commands {
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
                let ok = exe.is_some()
                    && (f.mode == StartMode::Terminal || !f.prompt.trim().is_empty())
                    && (f.mode == StartMode::Chat || total == 1 || !f.prompt.trim().is_empty());
                let label = match f.mode {
                    StartMode::Chat => format!("▶  Start  ({})", theme::shortcut("Enter")),
                    StartMode::Terminal => format!("▶  Open in terminal  ({})", theme::shortcut("Enter")),
                };
                if theme::primary_button(ui, &label, ok).clicked()
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
            self.start_new();
        } else if close {
            self.new_task = None;
        }
    }

    /// Start the chat set up in the New chat dialog.
    pub fn start_new(&mut self) {
        let Some(f) = self.new_task.take() else {
            return;
        };
        self.last_kind = f.kind;
        self.last_mode = f.mode;
        let kinds: Vec<AgentKind> = std::iter::once(f.kind)
            .chain(f.also.iter().copied())
            .collect();
        let total = kinds.len() * f.copies;
        let mut group = None;
        let mut first = None;
        for (ki, kind) in kinds.iter().enumerate() {
            let Some(exe) = f
                .installed
                .iter()
                .find(|(k, _)| k == kind)
                .and_then(|(_, p)| p.clone())
            else {
                continue;
            };
            for c in 0..f.copies {
                let one = NewTask {
                    repo: f.repo.clone(),
                    repo_name: f.repo_name.clone(),
                    base: f.base.clone(),
                    prompt: f.prompt.clone(),
                    kind: *kind,
                    also: Vec::new(),
                    copies: 1,
                    mode: f.mode,
                    run_commands: f.run_commands,
                    model: if ki == 0 {
                        f.model.clone()
                    } else {
                        String::new()
                    },
                    plan: f.plan,
                    profile: if ki == 0 { f.profile.clone() } else { None },
                    installed: Vec::new(),
                    focus: false,
                };
                let id = self.create(one, exe.clone());
                if total > 1 {
                    let g = *group.get_or_insert(id);
                    if let Some(t) = self.tasks.iter_mut().find(|t| t.id == id) {
                        t.group = Some(g);
                        let n = if f.copies > 1 {
                            format!(" #{}", c + 1)
                        } else {
                            String::new()
                        };
                        t.title = format!("{} · {}{n}", t.title, kind.name());
                    }
                }
                first.get_or_insert(id);
            }
        }
        if let Some(id) = first {
            self.opened = self
                .tasks
                .iter()
                .find(|t| t.id == id)
                .map(|t| (t.repo.clone(), id));
            if total > 1 {
                // several runs: show them side by side on the board
                self.show_board = true;
            }
        }
    }

    /// Dev only (`FOXGIT_DEMO=agentsfake[:chat|changes|terminal]`): made-up chats with real
    /// worktrees and edits, for layout checks without running an AI.
    #[cfg(debug_assertions)]
    pub fn demo_fill(&mut self, repo: &Path, name: &str, base: &str, pane: &str) -> Option<u64> {
        let mk = |agents: &mut Agents,
                  kind: AgentKind,
                  title: &str,
                  edits: &[(&str, &str)],
                  log: &[(Role, &str)],
                  status: Status,
                  cost: f64| {
            let id = agents.next_id;
            agents.next_id += 1;
            let slug = slug(title, id);
            let wt = agents
                .worktree_root
                .join(format!("{}-demo", sanitize(name)))
                .join(&slug);
            let branch = format!("agent/{slug}");
            let _ = create_worktree(repo, &wt, &branch, base);
            for (f, text) in edits {
                let path = wt.join(f);
                if let Some(d) = path.parent() {
                    let _ = std::fs::create_dir_all(d);
                }
                let _ = std::fs::write(path, text);
            }
            let mut t = Task {
                id,
                kind,
                title: title.into(),
                repo: repo.to_path_buf(),
                worktree: wt,
                branch,
                base: base.into(),
                run_commands: true,
                status,
                session: None,
                log: log
                    .iter()
                    .map(|(r, x)| Line {
                        role: *r,
                        text: x.to_string(),
                    })
                    .collect(),
                cost,
                changes: (0, 0),
                worktree_ready: true,
                queue: VecDeque::new(),
                input: String::new(),
                started: None,
                runner: None,
                pane: Pane::Chat,
                run: crate::runner::RunPanel::embedded(Vec::new()),
                model: if kind == AgentKind::Claude {
                    "sonnet".into()
                } else {
                    String::new()
                },
                plan: false,
                profile: None,
                envs: Vec::new(),
                group: None,
                review: Default::default(),
                attach: String::new(),
                renaming: None,
                mode: StartMode::Chat,
                launch: None,
                creating: None,
            };
            t.refresh_changes();
            agents.tasks.push(t);
            id
        };
        let main_rs = "fn main() {\n    let limit = std::env::var(\"RATE\").unwrap_or(\"100\".into());\n    println!(\"shop, rate limit {limit}/min\");\n}\n// health check\n// rate limiting\n";
        let log = [
            (
                Role::You,
                "Add a configurable rate limit to the public endpoints",
            ),
            (Role::Tool, "Read src/main.rs"),
            (Role::Tool, "Edit src/main.rs"),
            (Role::Tool, "Run cargo test"),
            (
                Role::Agent,
                "Done — the limit comes from RATE (default 100/min). Tests pass.",
            ),
        ];
        let mut ids = Vec::new();
        for (k, c) in [
            (AgentKind::Claude, 0.42),
            (AgentKind::Codex, 0.0),
            (AgentKind::Hermes, 0.18),
        ] {
            let id = mk(
                self,
                k,
                &format!("Add a configurable rate limit · {}", k.name()),
                &[
                    ("src/main.rs", main_rs),
                    ("src/limit.rs", "pub const DEFAULT: u32 = 100;\n"),
                ],
                &log,
                if k == AgentKind::Codex {
                    Status::Failed
                } else {
                    Status::Idle
                },
                c,
            );
            ids.push(id);
        }
        let g = ids[0];
        for t in self.tasks.iter_mut().filter(|t| ids.contains(&t.id)) {
            t.group = Some(g);
        }
        let single = mk(
            self,
            AgentKind::Claude,
            "Write tests for the orders API",
            &[("src/orders.rs", "pub fn list() {}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn lists() {\n        super::list();\n    }\n}\n")],
            &[
                (Role::You, "Write tests for the orders API"),
                (Role::Tool, "Read src/orders.rs"),
                (Role::Agent, "I added a test module to src/orders.rs. Want me to cover pagination too?"),
            ],
            Status::Idle,
            0.07,
        );
        let t = self.tasks.iter_mut().find(|t| t.id == single)?;
        match pane {
            "changes" => t.pane = Pane::Changes,
            "terminal" => t.pane = Pane::Terminal,
            "chat" => t.input = "Also cover pagination @src/orders.rs".into(),
            _ => return None,
        }
        Some(single)
    }

    /// Dev only (`FOXGIT_DEMO=newchat:terminal`): the New chat dialog in a given mode.
    #[cfg(debug_assertions)]
    pub fn demo_mode(&mut self, terminal: bool) {
        self.last_mode = if terminal {
            StartMode::Terminal
        } else {
            StartMode::Chat
        };
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
        let terminal_mode = f.mode == StartMode::Terminal;
        let title = match first_line(&f.prompt, 60) {
            t if t.is_empty() => format!("{} in the terminal", f.kind.name()),
            t => t,
        };
        let envs = profile_env(&self.profiles, f.profile.as_deref(), f.kind);
        let mut task = Task {
            id,
            kind: f.kind,
            title,
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
            run: crate::runner::RunPanel::embedded(envs.clone()),
            model: f.model.clone(),
            plan: f.plan,
            profile: f.profile.clone(),
            envs,
            group: None,
            review: Default::default(),
            attach: String::new(),
            renaming: None,
            mode: f.mode,
            launch: None,
            creating: None,
        };
        if terminal_mode {
            // make its copy of the repository now, then run the agent's own interface in it
            task.status = Status::Idle;
            task.pane = Pane::Terminal;
            task.launch = Some(f.prompt.clone());
            task.push(
                Role::Info,
                format!(
                    "Started {} in the Terminal tab. Chat messages here run it headless in the same copy of the repository.",
                    f.kind.name()
                ),
            );
            let (tx, rx) = channel();
            let (repo, wt, branch, base, ctx) = (
                task.repo.clone(),
                task.worktree.clone(),
                task.branch.clone(),
                task.base.clone(),
                self.ctx.clone(),
            );
            std::thread::spawn(move || {
                let _ = tx.send(create_worktree(&repo, &wt, &branch, &base));
                ctx.request_repaint();
            });
            task.creating = Some(rx);
        } else {
            task.send(f.prompt.trim().to_string(), exe, &self.ctx);
        }
        self.tasks.push(task);
        id
    }

    // ------------------------------------------------------------------ background results

    /// Read what the agents printed. Returns toasts (text, ok).
    pub fn poll(&mut self) -> Vec<(String, bool)> {
        for t in &mut self.tasks {
            if let Some(rx) = &t.creating {
                if let Ok(r) = rx.try_recv() {
                    t.creating = None;
                    match r {
                        Ok(()) => {
                            t.worktree_ready = true;
                            t.refresh_changes();
                        }
                        Err(e) => {
                            t.launch = None;
                            t.status = Status::Failed;
                            t.push(Role::Error, e);
                        }
                    }
                }
            }
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
                t.review.invalidate();
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
        // what the message bar offers (read before borrowing the task)
        let mut snippets = crate::prompts::builtin();
        snippets.extend(self.snippets.iter().cloned());
        let files =
            self.tasks.iter().find(|t| t.id == id).and_then(|t| {
                (t.worktree_ready && !t.attach.is_empty()).then(|| t.worktree.clone())
            });
        let files = files.map(|wt| self.files_of(&wt)).unwrap_or_default();
        let group_size = self
            .tasks
            .iter()
            .find(|t| t.id == id)
            .and_then(|t| t.group)
            .map(|g| self.tasks.iter().filter(|x| x.group == Some(g)).count())
            .unwrap_or(0);
        let mut retry: Option<(PathBuf, String, String, AgentKind)> = None;
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
        if running {
            ctx.request_repaint_after(Duration::from_millis(500));
        }

        // ---- header
        egui::Panel::top("agent_header")
            .frame(theme::panel(p.base, 14, 10))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(if t.mode == StartMode::Terminal {
                            "⌨"
                        } else {
                            "✨"
                        })
                        .size(18.0)
                        .color(p.accent_hover),
                    );
                    match &mut t.renaming {
                        Some(name) => {
                            let r = ui.add(egui::TextEdit::singleline(name).desired_width(360.0));
                            r.request_focus();
                            if r.lost_focus() {
                                let n = name.trim().to_string();
                                if !n.is_empty() && !ui.input(|i| i.key_pressed(egui::Key::Escape))
                                {
                                    t.title = n;
                                }
                                t.renaming = None;
                            }
                        }
                        None => {
                            let r = ui.add(
                                egui::Label::new(
                                    RichText::new(&t.title)
                                        .size(16.0)
                                        .strong()
                                        .color(p.text_strong),
                                )
                                .sense(egui::Sense::click())
                                .truncate(),
                            );
                            if r.on_hover_text("Double-click to rename").double_clicked() {
                                t.renaming = Some(t.title.clone());
                            }
                        }
                    }
                    if group_size > 1
                        && ui
                            .link(format!("1 of {group_size} runs · compare"))
                            .clicked()
                    {
                        out.push(TaskAction::Board);
                    }
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
                        } else {
                            let (txt, c) = status_label(t.status, p);
                            ui.label(RichText::new(txt).color(c));
                        }
                    });
                });
                ui.horizontal(|ui| {
                    let mut info = format!("{} · {} · from {}", t.kind.name(), t.branch, t.base);
                    if !t.model.is_empty() {
                        info.push_str(&format!(" · {}", t.model));
                    }
                    if let Some(pr) = &t.profile {
                        info.push_str(&format!(" · profile {pr}"));
                    }
                    if t.cost > 0.0 {
                        info.push_str(&format!(" · ≈ ${:.2} at API prices", t.cost));
                    }
                    ui.label(RichText::new(info).small().color(p.faint));
                });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    let (files_n, commits) = t.changes;
                    let changes = if files_n + commits > 0 {
                        format!("Changes ({files_n})")
                    } else {
                        "Changes".to_string()
                    };
                    let labels = ["💬 Chat".to_string(), changes, "⌨ Terminal".to_string()];
                    let refs: Vec<&str> = labels.iter().map(|x| x.as_str()).collect();
                    let mut idx = match t.pane {
                        Pane::Chat => 0,
                        Pane::Changes => 1,
                        Pane::Terminal => 2,
                    };
                    if theme::segmented(ui, &refs, &mut idx) {
                        t.pane = [Pane::Chat, Pane::Changes, Pane::Terminal][idx];
                        if t.pane == Pane::Changes {
                            t.review.invalidate();
                        }
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let ready = t.worktree_ready;
                        let more = ui.button("⋯").on_hover_text("More");
                        egui::Popup::menu(&more).show(|ui| {
                            ui.set_min_width(230.0);
                            if ui.button("Rename").clicked() {
                                t.renaming = Some(t.title.clone());
                                ui.close();
                            }
                            ui.menu_button("Retry with another agent", |ui| {
                                for k in AgentKind::ALL {
                                    if ui.button(k.name()).clicked() {
                                        let first = t
                                            .log
                                            .iter()
                                            .find(|l| l.role == Role::You)
                                            .map(|l| l.text.clone())
                                            .unwrap_or_default();
                                        retry = Some((t.repo.clone(), t.base.clone(), first, k));
                                        ui.close();
                                    }
                                }
                            });
                            ui.separator();
                            if ui
                                .add_enabled(
                                    ready && !running,
                                    egui::Button::new(format!("Update from {}", t.base)),
                                )
                                .on_hover_text(format!(
                                    "Merge {}'s new commits into the agent's copy",
                                    t.base
                                ))
                                .clicked()
                            {
                                out.push(TaskAction::Sync {
                                    repo: t.repo.clone(),
                                    worktree: t.worktree.clone(),
                                    base: t.base.clone(),
                                });
                                ui.close();
                            }
                            if ui
                                .add_enabled(ready, egui::Button::new("Run tests"))
                                .clicked()
                            {
                                if let Err(e) = t.run_tests(&ctx) {
                                    out.push(TaskAction::Notify(e, false));
                                }
                                ui.close();
                            }
                            ui.separator();
                            if ui
                                .add_enabled(ready, egui::Button::new("Open in Terminal app"))
                                .clicked()
                            {
                                if let Err(e) = crate::desktop::open_terminal_app(&t.worktree) {
                                    out.push(TaskAction::Notify(e, false));
                                }
                                ui.close();
                            }
                            if ui
                                .add_enabled(ready, egui::Button::new("Show in file manager"))
                                .clicked()
                            {
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
                            .on_hover_text(
                                "Open the agent's copy of the repository in your code editor",
                            )
                            .clicked()
                        {
                            if let Err(e) = crate::desktop::open_in_editor(&t.worktree, None, None)
                            {
                                out.push(TaskAction::Notify(e, false));
                            }
                        }
                    });
                });
            });

        // ---- actions (+ message box in the Chat tab)
        egui::Panel::bottom("agent_input")
            .frame(theme::panel(p.mantle, 14, 10))
            .show(ui, |ui| {
                let (files_n, commits) = t.changes;
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(if files_n + commits == 0 {
                            "No changes yet".to_string()
                        } else {
                            format!("{files_n} file(s) changed · {commits} commit(s) on {}", t.branch)
                        })
                        .color(p.subtext),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let has_work = t.worktree_ready && files_n + commits > 0;
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
                                "Commit the agent's work, push its branch and open a pull request"
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
                            if ui.button("Open as tab").on_hover_text("Open the agent's copy as a repository tab — stage, discard or edit like any repository").clicked() {
                                out.push(TaskAction::Review(t.worktree.clone()));
                            }
                        });
                    });
                });
                if t.pane != Pane::Chat {
                    return;
                }
                ui.add_space(6.0);
                // snippets
                egui::ScrollArea::horizontal().id_salt("agent_snippets").show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        for sn in &snippets {
                            let r = ui
                                .add(egui::Button::new(RichText::new(&sn.name).small()).corner_radius(12.0))
                                .on_hover_text(format!("{}\n\nClick: put in the box · ⇧ click: send now", sn.text));
                            if r.clicked() {
                                if ui.input(|i| i.modifiers.shift) {
                                    t.say(sn.text.clone(), &ctx);
                                } else {
                                    if !t.input.trim().is_empty() {
                                        t.input.push_str("\n\n");
                                    }
                                    t.input.push_str(&sn.text);
                                }
                            }
                        }
                    });
                });
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    let w = ui.available_width() - 90.0;
                    let r = ui.add(
                        egui::TextEdit::multiline(&mut t.input)
                            .hint_text(if running {
                                "Type a message — it's sent when the agent finishes this step"
                            } else {
                                "Reply to the agent…  (@ to attach a file)"
                            })
                            .desired_rows(2)
                            .desired_width(w)
                            .margin(vec2(10.0, 8.0)),
                    );
                    if r.changed() && t.input.ends_with('@') && t.attach.is_empty() {
                        t.attach = " ".into(); // open the picker
                        t.input.pop();
                    }
                    let send_key = r.has_focus()
                        && ui.input(|i| i.key_pressed(egui::Key::Enter) && (i.modifiers.command || !i.modifiers.shift));
                    let can = !t.input.trim().is_empty();
                    if (theme::primary_button(ui, "▶  Send", can).clicked() || send_key) && can {
                        let msg = t.input.trim().to_string();
                        t.input.clear();
                        t.say(msg, &ctx);
                    }
                });
                ui.horizontal(|ui| {
                    // attach files
                    let r = ui.small_button("📎 Attach file").on_hover_text("Mention files so the agent reads them (@path)");
                    if r.clicked() {
                        t.attach = if t.attach.is_empty() { " ".into() } else { String::new() };
                    }
                    // model
                    let models = crate::prompts::models(t.kind);
                    let current = models
                        .iter()
                        .find(|m| m.0 == t.model)
                        .map(|m| m.1.to_string())
                        .unwrap_or_else(|| if t.model.is_empty() { "Default model".into() } else { t.model.clone() });
                    egui::ComboBox::from_id_salt(("agent_model", t.id))
                        .selected_text(RichText::new(format!("Model: {current}")).small())
                        .width(150.0)
                        .show_ui(ui, |ui| {
                            for (v, label) in models {
                                ui.selectable_value(&mut t.model, v.to_string(), *label);
                            }
                            ui.separator();
                            ui.label(RichText::new("Other model id:").small().weak());
                            ui.text_edit_singleline(&mut t.model);
                        });
                    ui.checkbox(&mut t.plan, RichText::new("Plan mode").small())
                        .on_hover_text("Ask for a plan first — it doesn't change files until you say go (untick to let it work)");
                    ui.label(RichText::new("Enter sends · Shift+Enter new line").small().color(p.faint));
                });
                if !t.attach.is_empty() {
                    egui::Frame::new()
                        .fill(p.raised)
                        .stroke(egui::Stroke::new(1.0, p.border))
                        .corner_radius(8.0)
                        .inner_margin(egui::Margin::same(8))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                let mut q = t.attach.trim_start().to_string();
                                let r = ui.add(egui::TextEdit::singleline(&mut q).hint_text("Find a file…").desired_width(300.0));
                                r.request_focus();
                                t.attach = format!(" {q}");
                                if ui.small_button("×").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                                    t.attach.clear();
                                }
                            });
                            let q = t.attach.trim().to_lowercase();
                            let hits: Vec<&String> = files.iter().filter(|f| q.is_empty() || f.to_lowercase().contains(&q)).take(12).collect();
                            if files.is_empty() {
                                ui.label(RichText::new("No files yet — the agent's copy is made with the first message.").small().weak());
                            }
                            let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
                            for (k, f) in hits.iter().enumerate() {
                                if ui.selectable_label(k == 0, f.as_str()).clicked() || (k == 0 && enter) {
                                    if !t.input.is_empty() && !t.input.ends_with([' ', '\n']) {
                                        t.input.push(' ');
                                    }
                                    t.input.push_str(&crate::prompts::mention(f));
                                    t.input.push(' ');
                                    t.attach.clear();
                                    break;
                                }
                            }
                        });
                }
            });

        // ---- the selected tab
        match t.pane {
            Pane::Terminal => {
                egui::CentralPanel::default()
                    .frame(theme::panel(p.base, 14, 8))
                    .show(ui, |ui| {
                        if let Some(e) = t.terminal_ui(ui, running, &ctx) {
                            out.push(TaskAction::Notify(e, false));
                        }
                    });
            }
            Pane::Changes => {
                egui::CentralPanel::default()
                    .frame(theme::panel(p.base, 14, 8))
                    .show(ui, |ui| {
                        let (wt, base, ready) =
                            (t.worktree.clone(), t.base.clone(), t.worktree_ready);
                        for a in t.review.ui(ui, &wt, &base, ready, running) {
                            match a {
                                crate::agent_review::ReviewAction::Send(msg) => {
                                    t.say(msg, &ctx);
                                    t.pane = Pane::Chat;
                                }
                                crate::agent_review::ReviewAction::Open(path, line) => {
                                    if let Err(e) = crate::desktop::open_in_editor(
                                        &wt,
                                        Some(&wt.join(path)),
                                        line.map(|l| l as usize),
                                    ) {
                                        out.push(TaskAction::Notify(e, false));
                                    }
                                }
                            }
                        }
                    });
            }
            Pane::Chat => {
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
            }
        }
        if let Some((repo, base, prompt, kind)) = retry {
            let name = repo
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            self.last_kind = kind;
            self.last_mode = StartMode::Chat;
            self.open_new(&repo, &name, &base, &prompt);
        }
        out
    }

    /// Files of a worktree for the attach picker (cached a few seconds).
    fn files_of(&mut self, wt: &Path) -> Vec<String> {
        let fresh = self
            .files
            .get(wt)
            .is_some_and(|(t, _)| t.elapsed() < Duration::from_secs(5));
        if !fresh {
            let list = crate::prompts::tracked_files(wt);
            self.files.insert(wt.to_path_buf(), (Instant::now(), list));
        }
        self.files.get(wt).map(|x| x.1.clone()).unwrap_or_default()
    }
}

fn board_card(ui: &mut egui::Ui, t: &Task, compare: bool, out: &mut Vec<BoardAction>) {
    let p = pal(ui);
    let running = t.runner.is_some();
    let (files, commits) = t.changes;
    let border = match t.status {
        Status::Failed => theme::alpha(p.red, 0.6),
        Status::Running => theme::alpha(p.accent, 0.6),
        _ => p.border,
    };
    egui::Frame::new()
        .fill(p.raised)
        .stroke(egui::Stroke::new(1.0, border))
        .corner_radius(12.0)
        .inner_margin(egui::Margin::same(12))
        .show(ui, |ui| {
            ui.set_width(if compare { 250.0 } else { 270.0 });
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(if t.mode == StartMode::Terminal {
                            "⌨"
                        } else {
                            "✨"
                        })
                        .color(p.accent_hover),
                    );
                    let title = if compare {
                        t.title.split(" · ").skip(1).collect::<Vec<_>>().join(" · ")
                    } else {
                        t.title.clone()
                    };
                    let r = ui.add(
                        egui::Label::new(RichText::new(title).strong().color(p.text_strong))
                            .sense(egui::Sense::click())
                            .truncate(),
                    );
                    if r.clicked() {
                        out.push(BoardAction::Open(t.id));
                    }
                });
                let (txt, c) = if running {
                    let secs = t.started.map(|s| s.elapsed().as_secs()).unwrap_or(0);
                    (
                        format!("working · {}:{:02}", secs / 60, secs % 60),
                        p.accent_hover,
                    )
                } else {
                    let (a, b) = status_label(t.status, p);
                    (a.to_string(), b)
                };
                ui.horizontal(|ui| {
                    if running {
                        ui.add(egui::Spinner::new().size(11.0));
                    }
                    ui.label(RichText::new(txt).small().color(c));
                    ui.label(
                        RichText::new(format!("· {}", t.kind.name()))
                            .small()
                            .color(p.faint),
                    );
                    if !t.model.is_empty() {
                        ui.label(RichText::new(&t.model).small().color(p.faint));
                    }
                });
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(if files + commits == 0 {
                            "no changes yet".to_string()
                        } else {
                            format!("{files} file(s) · {commits} commit(s)")
                        })
                        .small()
                        .color(p.subtext),
                    );
                    if t.cost > 0.0 {
                        ui.label(
                            RichText::new(format!("· ${:.2}", t.cost))
                                .small()
                                .color(p.faint),
                        );
                    }
                });
                if let Some(last) = t.log.iter().rev().find(|l| l.role == Role::Agent) {
                    ui.add(
                        egui::Label::new(
                            RichText::new(first_line(&last.text, 120))
                                .small()
                                .color(p.faint),
                        )
                        .truncate(),
                    );
                }
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if ui.small_button("Open").clicked() {
                        out.push(BoardAction::Open(t.id));
                    }
                    let can_merge = !running
                        && t.worktree_ready
                        && files + commits > 0
                        && t.status != Status::Merged;
                    if ui
                        .add_enabled(
                            can_merge,
                            egui::Button::new(
                                RichText::new(format!("Merge into {}", t.base)).small(),
                            ),
                        )
                        .clicked()
                    {
                        out.push(BoardAction::Act(t.merge_action(false)));
                    }
                    if ui
                        .add_enabled(
                            !running && t.worktree_ready,
                            egui::Button::new(RichText::new("Discard…").small()),
                        )
                        .clicked()
                    {
                        out.push(BoardAction::Act(TaskAction::Discard {
                            task: t.id,
                            repo: t.repo.clone(),
                            worktree: t.worktree.clone(),
                            branch: t.branch.clone(),
                        }));
                    }
                });
            });
        });
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
            opts: self.opts(),
            envs: self.envs.clone(),
        };
        self.runner = Some(spawn_run(spec, ctx.clone()));
    }

    fn opts(&self) -> RunOpts {
        RunOpts {
            run_commands: self.run_commands,
            model: self.model.clone(),
            plan: self.plan,
        }
    }

    /// Send a message now, or queue it while the agent is busy.
    fn say(&mut self, msg: String, ctx: &egui::Context) {
        if self.runner.is_some() {
            self.queue.push_back(msg.clone());
            self.push(
                Role::You,
                format!("{msg}\n(queued — sent when the agent finishes this step)"),
            );
        } else if let Some(exe) = self.kind.find() {
            self.send(msg, exe, ctx);
        } else {
            self.push(
                Role::Error,
                format!("{} isn't installed any more.", self.kind.name()),
            );
        }
    }

    /// The agent's own interface in the Terminal tab (resuming this chat's session).
    fn open_agent_terminal(
        &mut self,
        ctx: &egui::Context,
        first: Option<&str>,
    ) -> Result<(), String> {
        let exe = self
            .kind
            .find()
            .ok_or_else(|| format!("{} isn't installed", self.kind.name()))?;
        let resume = self
            .kind
            .can_resume()
            .then(|| self.session.clone())
            .flatten();
        let line = self
            .kind
            .interactive_line(&exe, resume.as_deref(), first, &self.opts());
        self.run.envs = self.envs.clone();
        self.pane = Pane::Terminal;
        self.run
            .focus_or_start(&self.worktree, ctx, self.kind.name(), &line)
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

    /// The Terminal pane: the agent's own interface, shells and the project's scripts, all in
    /// the agent's copy of the repository.
    fn terminal_ui(
        &mut self,
        ui: &mut egui::Ui,
        running: bool,
        ctx: &egui::Context,
    ) -> Option<String> {
        let p = pal(ui);
        let mut note = None;
        if !self.worktree_ready {
            ui.centered_and_justified(|ui| {
                if self.creating.is_some() {
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new());
                        ui.label(
                            RichText::new("Making the agent's copy of the repository…").weak(),
                        );
                    });
                } else {
                    ui.label(
                        RichText::new(
                            "The agent's copy of the repository is created with the first message.",
                        )
                        .weak(),
                    );
                }
            });
            return None;
        }
        // a Terminal-mode chat starts its agent as soon as the copy exists
        if let Some(first) = self.launch.take() {
            if let Err(e) = self.open_agent_terminal(ctx, Some(&first)) {
                note = Some(e);
            }
        }
        if self.run.is_empty() {
            self.run.envs = self.envs.clone();
            if let Err(e) = self.run.start(&self.worktree, ctx, "Shell", None) {
                ui.label(RichText::new(format!("Could not open a terminal: {e}")).color(p.red));
                return note;
            }
        }
        ui.horizontal(|ui| {
            let resume = self.kind.can_resume() && self.session.is_some();
            let label = if resume {
                format!("▶  Continue in {}", self.kind.name())
            } else {
                format!("▶  Start {}", self.kind.name())
            };
            let can = self.kind.find().is_some() && !running;
            if ui
                .add_enabled(can, egui::Button::new(label))
                .on_hover_text(if running {
                    "Wait until the chat run finishes (or stop it) — both would work on the same session"
                } else {
                    "The agent's own full-screen interface, in its copy of the repository"
                })
                .clicked()
            {
                if let Err(e) = self.open_agent_terminal(ctx, None) {
                    note = Some(e);
                }
            }
            if ui.button("Copy screen").clicked() {
                if let Some(t) = self.run.active_contents() {
                    ui.ctx().copy_text(t);
                }
            }
            ui.label(RichText::new(format!("in {}", self.worktree.display())).small().color(p.faint));
        });
        ui.add_space(4.0);
        let dir = self.worktree.clone();
        self.run.ui(ui, &dir);
        note
    }

    /// Start the project's test script in a terminal of the agent's copy.
    fn run_tests(&mut self, ctx: &egui::Context) -> Result<(), String> {
        let tasks = crate::runner::detect(&self.worktree);
        let t = tasks
            .iter()
            .find(|t| t.label == "test" || t.label.starts_with("test"))
            .or_else(|| tasks.iter().find(|t| t.label.contains("test")))
            .ok_or(
                "No test script found (package.json \"test\", cargo test, make test, go test)",
            )?;
        self.pane = Pane::Terminal;
        self.run.envs = self.envs.clone();
        self.run
            .start(&self.worktree, ctx, &t.label, Some(&t.command))
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
    opts: RunOpts,
    /// Profile environment (config folder of the CLI).
    envs: Vec<(String, String)>,
}

fn profile_env(profiles: &[Profile], name: Option<&str>, kind: AgentKind) -> Vec<(String, String)> {
    name.and_then(|n| profiles.iter().find(|p| p.name == n && p.kind == kind))
        .map(|p| p.env())
        .unwrap_or_default()
}

/// `git worktree add -b <branch> <dir> <base>` for an agent's copy of the repository.
fn create_worktree(repo: &Path, dir: &Path, branch: &str, base: &str) -> Result<(), String> {
    if let Some(parent) = dir.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let out = git::run_git(
        repo,
        &[
            "worktree".into(),
            "add".into(),
            "-b".into(),
            branch.to_string(),
            dir.display().to_string(),
            base.to_string(),
        ],
        None,
    );
    if out.ok {
        Ok(())
    } else {
        Err(format!(
            "Could not create the agent's worktree:\n{}",
            out.text
        ))
    }
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
            if let Err(e) = create_worktree(&spec.repo, &spec.worktree, &spec.branch, &spec.base) {
                send(&tx, Msg::Failed(e));
                return;
            }
        }
        let (mut cmd, stdin_text) = spec.kind.command(
            &spec.exe,
            &spec.prompt,
            &spec.worktree,
            spec.resume.as_deref(),
            &spec.opts,
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
        for (k, v) in &spec.envs {
            cmd.env(k, v);
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
        let mut agents = Agents::new(&ctx, Vec::new(), None, Vec::new(), Vec::new());
        let wt_root = tempfile::tempdir().unwrap();
        agents.worktree_root = wt_root.path().to_path_buf();
        let id = agents.create(
            NewTask {
                repo: r.to_path_buf(),
                repo_name: "demo".into(),
                base: "main".into(),
                prompt: "make a file".into(),
                kind: AgentKind::Claude,
                also: Vec::new(),
                copies: 1,
                mode: StartMode::Chat,
                run_commands: false,
                model: String::new(),
                plan: false,
                profile: None,
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
    #[cfg(unix)]
    fn several_runs_start_as_one_group_with_their_own_copies() {
        use std::os::unix::fs::PermissionsExt;
        let repo = tempfile::tempdir().unwrap();
        let r = repo.path();
        for a in [
            &["init", "-q", "-b", "main"][..],
            &["config", "user.name", "T"],
            &["config", "user.email", "t@example.com"],
            &["commit", "-q", "--allow-empty", "-m", "first"],
        ] {
            let out = git::run_git(
                r,
                &a.iter().map(|x| x.to_string()).collect::<Vec<_>>(),
                None,
            );
            assert!(out.ok, "{}", out.text);
        }
        let bin = tempfile::tempdir().unwrap();
        let exe = bin.path().join("claude");
        std::fs::write(
            &exe,
            "#!/bin/sh\ncat >/dev/null\necho '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"ok\"}'\n",
        )
        .unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let ctx = egui::Context::default();
        let mut agents = Agents::new(&ctx, Vec::new(), None, Vec::new(), Vec::new());
        let wt_root = tempfile::tempdir().unwrap();
        agents.worktree_root = wt_root.path().to_path_buf();
        agents.new_task = Some(NewTask {
            repo: r.to_path_buf(),
            repo_name: "demo".into(),
            base: "main".into(),
            prompt: "try it".into(),
            kind: AgentKind::Claude,
            also: Vec::new(),
            copies: 2,
            mode: StartMode::Chat,
            run_commands: false,
            model: "haiku".into(),
            plan: false,
            profile: None,
            installed: vec![(AgentKind::Claude, Some(exe))],
            focus: false,
        });
        agents.start_new();
        assert_eq!(agents.tasks.len(), 2);
        let g = agents.tasks[0].group;
        assert!(g.is_some() && agents.tasks.iter().all(|t| t.group == g));
        assert_eq!(agents.tasks[1].title, "try it · Claude Code #2");
        assert_ne!(agents.tasks[0].worktree, agents.tasks[1].worktree);
        assert!(agents.tasks.iter().all(|t| t.model == "haiku"));
        assert!(agents.show_board);
        for _ in 0..200 {
            agents.poll();
            if !agents.any_running() {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(agents
            .tasks
            .iter()
            .all(|t| t.worktree.exists() && t.status == Status::Idle));
    }

    #[test]
    fn interactive_lines_with_a_first_message() {
        let d = RunOpts::default();
        let exe = Path::new("/usr/bin/claude");
        assert_eq!(
            AgentKind::Claude.interactive_line(exe, None, None, &d),
            "/usr/bin/claude"
        );
        assert_eq!(
            AgentKind::Claude.interactive_line(exe, Some("s1"), Some("fix it's bug"), &d),
            "/usr/bin/claude --resume s1 'fix it'\\''s bug'"
        );
        let o = RunOpts {
            model: "opus".into(),
            plan: true,
            run_commands: false,
        };
        assert_eq!(
            AgentKind::Claude.interactive_line(exe, None, Some("go"), &o),
            "/usr/bin/claude --model opus --permission-mode plan go"
        );
        let h = Path::new("/bin/hermes");
        assert_eq!(
            AgentKind::Hermes.interactive_line(h, None, Some("  "), &d),
            "/bin/hermes chat"
        );
        assert_eq!(
            AgentKind::Hermes.interactive_line(h, Some("s9"), Some("add tests"), &d),
            "/bin/hermes chat --resume s9 -q 'add tests'"
        );
        assert_eq!(
            AgentKind::Codex.interactive_line(Path::new("codex"), None, Some("go"), &d),
            "codex go"
        );
        // agents without a plan mode are told in the first message
        let line = AgentKind::Codex.interactive_line(Path::new("codex"), None, Some("go"), &o);
        assert!(line.starts_with("codex -m opus 'Plan only"), "{line}");
    }

    #[test]
    fn headless_commands_carry_model_plan_and_permissions() {
        let args = |k: AgentKind, o: &RunOpts| {
            let (c, _) = k.command(Path::new("x"), "hi", Path::new("/w"), Some("s"), o);
            c.get_args()
                .map(|a| a.to_string_lossy().to_string())
                .collect::<Vec<_>>()
                .join(" ")
        };
        let o = RunOpts {
            run_commands: true,
            model: "sonnet".into(),
            plan: false,
        };
        assert_eq!(
            args(AgentKind::Claude, &o),
            "-p --output-format stream-json --verbose --permission-mode auto --model sonnet --resume s"
        );
        let plan = RunOpts {
            plan: true,
            ..o.clone()
        };
        assert!(args(AgentKind::Claude, &plan).contains("--permission-mode plan"));
        // plan mode never runs Hermes with --yolo
        assert!(args(AgentKind::Hermes, &o).contains("--yolo"));
        assert!(!args(AgentKind::Hermes, &plan).contains("--yolo"));
        let (_, stdin) =
            AgentKind::Hermes.command(Path::new("x"), "hi", Path::new("/w"), None, &plan);
        assert!(stdin.unwrap().starts_with("Plan only"));
        assert!(args(AgentKind::Codex, &o).contains("-m sonnet -C /w hi"));
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
            run: crate::runner::RunPanel::embedded(Vec::new()),
            model: String::new(),
            plan: false,
            profile: None,
            envs: Vec::new(),
            group: None,
            review: Default::default(),
            attach: String::new(),
            renaming: None,
            mode: StartMode::Chat,
            launch: None,
            creating: None,
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
