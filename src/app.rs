//! Application state, background jobs, toolbar, tabs and dialogs.

use crate::account::{GitHub, MenuAction, RemoteEvent};
use crate::agents;
use crate::askpass::{self, AskRequest};
use crate::git::{self, CommitInfo, DiffTarget, FileChange, FileDiff, Op, Refs, Repo, WorkStatus};
use crate::github;
use crate::graph::{self, GraphRow};
use crate::tabs::{self, LauncherEvent, LauncherMode, LauncherState, Project, TabEvent, TabInfo};
use crate::theme;
use crate::update::{self, Release};
use crate::views;
use eframe::egui;
use egui::{Color32, RichText};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

pub const DEFAULT_COMMIT_LIMIT: usize = 3000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    FileStatus,
    History,
    /// AI agent chats (the open chat is `RepoTab::agent_chat`).
    Agents,
}

type HistoryResult = Result<(Vec<CommitInfo>, Vec<GraphRow>), String>;

/// One open repository (one tab).
pub struct RepoTab {
    pub id: u64,
    pub repo: Repo,
    pub path: PathBuf,
    pub name: String,
    pub view: View,
    /// The agent chat shown in the Agents view.
    pub agent_chat: Option<u64>,

    pub commits: Vec<CommitInfo>,
    pub graph: Vec<GraphRow>,
    pub graph_lanes: usize,
    pub commit_limit: usize,
    pub history_rx: Option<Receiver<HistoryResult>>,

    pub refs: Refs,
    pub status: WorkStatus,

    pub selected_commit: Option<git2::Oid>,
    pub commit_files: Vec<FileChange>,
    pub diff_target: Option<DiffTarget>,
    pub diff: Option<Result<FileDiff, String>>,
    pub context_lines: u32,
    /// Selected diff lines (indices into the current FileDiff) for line staging.
    pub line_sel: std::collections::BTreeSet<usize>,
    pub sel_anchor: Option<usize>,
    pub sel_dragging: bool,
    pub last_fetch: Option<Instant>,

    pub search: String,
    pub filtered: Option<Vec<usize>>,
    pub scroll_to_selected: bool,
    /// Rows of the history list visible last frame (for keyboard navigation scrolling).
    pub history_range: (usize, usize),

    pub commit_msg: String,
    pub amend: bool,
    pub push_after_commit: bool,

    pub load_error: Option<String>,
}

impl RepoTab {
    fn open(id: u64, path: &Path, limit: usize) -> Result<Self, String> {
        let repo = Repo::open(path)?;
        let path = repo.workdir.clone();
        let name = repo.name();
        let mut tab = RepoTab {
            id,
            repo,
            path,
            name,
            view: View::History,
            agent_chat: None,
            commits: Vec::new(),
            graph: Vec::new(),
            graph_lanes: 1,
            commit_limit: limit,
            history_rx: None,
            refs: Refs::default(),
            status: WorkStatus::default(),
            selected_commit: None,
            commit_files: Vec::new(),
            diff_target: None,
            diff: None,
            context_lines: 3,
            line_sel: Default::default(),
            sel_anchor: None,
            sel_dragging: false,
            last_fetch: None,
            search: String::new(),
            filtered: None,
            scroll_to_selected: false,
            history_range: (0, 0),
            commit_msg: String::new(),
            amend: false,
            push_after_commit: false,
            load_error: None,
        };
        tab.reload_fast();
        tab.start_history_load(None);
        if !tab.status.is_clean() {
            tab.view = View::FileStatus;
        }
        Ok(tab)
    }

    /// Refresh refs + working copy status + current diff (cheap, synchronous).
    pub fn reload_fast(&mut self) {
        self.load_error = None;
        match self.repo.refs() {
            Ok(r) => self.refs = r,
            Err(e) => self.load_error = Some(e),
        }
        match self.repo.status() {
            Ok(s) => self.status = s,
            Err(e) => self.load_error = Some(e),
        }
        // Drop a working-copy selection that no longer exists.
        let still_there = match &self.diff_target {
            Some(DiffTarget::Staged { path }) => self.status.staged.iter().any(|f| &f.path == path),
            Some(DiffTarget::Unstaged { path }) => {
                self.status.unstaged.iter().any(|f| &f.path == path)
            }
            _ => true,
        };
        if !still_there {
            // Same file may have moved between staged and unstaged.
            let path = match &self.diff_target {
                Some(DiffTarget::Staged { path }) | Some(DiffTarget::Unstaged { path }) => {
                    path.clone()
                }
                _ => String::new(),
            };
            self.diff_target = if self.status.unstaged.iter().any(|f| f.path == path) {
                Some(DiffTarget::Unstaged { path })
            } else if self.status.staged.iter().any(|f| f.path == path) {
                Some(DiffTarget::Staged { path })
            } else {
                None
            };
        }
        self.reload_diff();
    }

    /// Load history on a background thread (libgit2 topological walk can take a moment on big repos).
    pub fn start_history_load(&mut self, ctx: Option<&egui::Context>) {
        let (tx, rx) = channel();
        let path = self.path.clone();
        let limit = self.commit_limit;
        let ctx = ctx.cloned();
        std::thread::spawn(move || {
            let res = (|| {
                let repo = Repo::open(&path)?;
                let commits = repo.commits(limit)?;
                let input: Vec<_> = commits.iter().map(|c| (c.oid, c.parents.clone())).collect();
                let rows = graph::layout(&input);
                Ok((commits, rows))
            })();
            let _ = tx.send(res);
            if let Some(ctx) = ctx {
                ctx.request_repaint();
            }
        });
        self.history_rx = Some(rx);
    }

    fn poll_history(&mut self) {
        let Some(rx) = &self.history_rx else { return };
        match rx.try_recv() {
            Ok(Ok((commits, rows))) => {
                self.graph_lanes = rows.iter().map(|r| r.width).max().unwrap_or(1).clamp(1, 24);
                self.commits = commits;
                self.graph = rows;
                self.history_rx = None;
                self.apply_filter();
                if let Some(sel) = self.selected_commit {
                    if !self.commits.iter().any(|c| c.oid == sel) {
                        self.selected_commit = None;
                        self.commit_files.clear();
                        if matches!(self.diff_target, Some(DiffTarget::Commit { .. })) {
                            self.diff_target = None;
                            self.diff = None;
                        }
                    }
                }
            }
            Ok(Err(e)) => {
                self.load_error = Some(e);
                self.history_rx = None;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => self.history_rx = None,
        }
    }

    pub fn is_loading(&self) -> bool {
        self.history_rx.is_some()
    }

    pub fn apply_filter(&mut self) {
        let q = self.search.trim().to_lowercase();
        if q.is_empty() {
            self.filtered = None;
            return;
        }
        self.filtered = Some(
            self.commits
                .iter()
                .enumerate()
                .filter(|(_, c)| {
                    c.message.to_lowercase().contains(&q)
                        || c.author.to_lowercase().contains(&q)
                        || c.email.to_lowercase().contains(&q)
                        || c.oid.to_string().starts_with(&q)
                })
                .map(|(i, _)| i)
                .collect(),
        );
    }

    pub fn select_commit(&mut self, oid: git2::Oid) {
        self.selected_commit = Some(oid);
        self.commit_files = self.repo.commit_files(oid).unwrap_or_default();
        let first = self.commit_files.first().map(|f| f.path.clone());
        self.diff_target = first.map(|path| DiffTarget::Commit { oid, path });
        self.reload_diff();
    }

    pub fn select_diff(&mut self, target: DiffTarget) {
        if self.diff_target.as_ref() != Some(&target) {
            self.diff_target = Some(target);
            self.reload_diff();
        }
    }

    pub fn reload_diff(&mut self) {
        self.line_sel.clear();
        self.sel_anchor = None;
        self.sel_dragging = false;
        self.diff = self
            .diff_target
            .as_ref()
            .map(|t| self.repo.diff(t, self.context_lines));
    }

    pub fn head_branch(&self) -> Option<&git::LocalBranch> {
        self.refs.locals.iter().find(|b| b.is_head)
    }

    pub fn commit_by_oid(&self, oid: git2::Oid) -> Option<&CommitInfo> {
        self.commits.iter().find(|c| c.oid == oid)
    }
}

// ---------------------------------------------------------------------------
// Jobs
// ---------------------------------------------------------------------------

pub enum JobKind {
    Op(Op),
    Clone {
        url: String,
        parent: PathBuf,
        name: String,
    },
}

struct Job {
    tab_id: Option<u64>,
    dir: PathBuf,
    kind: JobKind,
    after: Option<AfterJob>,
}

/// Bookkeeping to do when a job succeeds.
#[derive(Clone)]
enum AfterJob {
    AgentMerged(u64),
    AgentDiscarded(u64),
    OpenUrl(String),
}

struct JobDone {
    tab_id: Option<u64>,
    title: String,
    ok: bool,
    text: String,
    open_after: Option<PathBuf>,
    after: Option<AfterJob>,
}

pub struct LogEntry {
    pub when: String,
    pub title: String,
    pub ok: bool,
    pub text: String,
}

// ---------------------------------------------------------------------------
// Dialogs
// ---------------------------------------------------------------------------

pub enum Dialog {
    NewBranch {
        name: String,
        start: Option<String>,
        start_label: String,
        checkout: bool,
    },
    NewTag {
        name: String,
        commit: String,
    },
    Push {
        remote: String,
        branch: String,
        set_upstream: bool,
        force: bool,
    },
    Pull {
        rebase: bool,
    },
    Stash {
        message: String,
    },
    Clone {
        url: String,
        parent: String,
        name: String,
    },
    Confirm {
        title: String,
        text: String,
        op: Op,
        danger: bool,
    },
    /// Delete an agent's worktree + branch (and forget the chat).
    DiscardAgent {
        task: u64,
        repo: PathBuf,
        worktree: PathBuf,
        branch: String,
    },
    AddRemote {
        name: String,
        url: String,
        fetch: bool,
        /// Create a new repository on GitHub instead of typing a URL.
        github: bool,
        form: PublishForm,
    },
    EditRemote {
        original: String,
        name: String,
        url: String,
    },
}

/// "Create on GitHub" part of the Add remote dialog.
pub struct PublishForm {
    pub owner: String,
    pub repo: String,
    pub description: String,
    pub private: bool,
    /// Push the current branch once the remote is added.
    pub push: bool,
    pub branch: Option<String>,
    /// Waiting for GitHub (request tag).
    pub busy: Option<u64>,
    pub error: Option<String>,
}

/// A repository being created on GitHub for a tab.
struct PendingPublish {
    tag: u64,
    tab_id: u64,
    dir: PathBuf,
    remote: String,
    push_branch: Option<String>,
}

/// Things the UI asks the app to do (collected during a frame, applied afterwards).
pub enum Action {
    Run(Op),
    Dialog(Dialog),
    Refresh,
    LoadMore,
    /// Open the Add remote dialog for the active tab.
    AddRemote,
    /// Open the Edit remote dialog.
    EditRemote(String),
    /// Start a chat with an AI agent in the active repository.
    NewAgentTask,
}

// ---------------------------------------------------------------------------
// Persistence
// ---------------------------------------------------------------------------

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct Saved {
    open_tabs: Vec<String>,
    active: usize,
    #[serde(default)]
    recent: Vec<String>,
    #[serde(default)]
    projects: Vec<Project>,
    dark: Option<bool>,
    #[serde(default)]
    auto_fetch_min: Option<u32>,
    /// Active GitHub login (older versions had only this one account).
    #[serde(default)]
    github_login: Option<String>,
    /// Every connected GitHub login; their tokens live in the keychain, never in this file.
    #[serde(default)]
    github_accounts: Vec<String>,
    /// Folder the last clone went into.
    #[serde(default)]
    clone_parent: Option<String>,
    /// Unix time of the last automatic update check.
    #[serde(default)]
    last_update_check: i64,
    #[serde(default)]
    agent_tasks: Vec<agents::SavedTask>,
    #[serde(default)]
    last_agent: Option<agents::AgentKind>,
}

/// Result of looking for `git` at startup.
enum GitCheck {
    Checking(Receiver<Result<String, String>>),
    Found,
    Missing(String),
}

enum FetchMsg {
    Tab { id: u64, ok: bool, text: String },
    Done,
}

// Kept from the RustTree days so saved state carries over after the rename.
const STORAGE_KEY: &str = "rusttree_state";

// ---------------------------------------------------------------------------
// App
// ---------------------------------------------------------------------------

pub struct FoxGitApp {
    tabs: Vec<RepoTab>,
    active: usize,
    next_id: u64,
    projects: Vec<Project>,
    dark: bool,

    show_launcher: bool,
    launcher: LauncherState,
    scan_rx: Option<Receiver<Vec<PathBuf>>>,
    drag_tab: Option<usize>,
    scroll_to_active: bool,

    job_tx: Sender<JobDone>,
    job_rx: Receiver<JobDone>,
    queue: VecDeque<Job>,
    running: Option<String>,
    running_tab: Option<u64>,

    log: Vec<LogEntry>,
    show_log: bool,
    toast: Option<(String, bool, Instant)>,
    dialog: Option<Dialog>,
    was_focused: bool,

    // credential prompts
    ask_rx: Option<Receiver<AskRequest>>,
    ask_queue: VecDeque<AskRequest>,
    ask_input: String,
    ask_remember: bool,
    ask_focus: bool,
    cred_cache: HashMap<String, String>,

    // auto-fetch
    auto_fetch_min: u32,
    last_auto_fetch: Instant,
    fetch_rx: Option<Receiver<FetchMsg>>,
    fetch_manual: bool,

    github: GitHub,
    clone_parent: String,

    git_check: GitCheck,
    show_about: bool,
    logo: Option<egui::TextureHandle>,
    update: Option<Release>,
    update_rx: Option<Receiver<update::CheckResult>>,
    last_update_check: i64,
    publish: Option<PendingPublish>,
    next_tag: u64,
    agents: agents::Agents,
}

impl FoxGitApp {
    pub fn new(cc: &eframe::CreationContext<'_>, initial: Vec<String>) -> Self {
        let saved: Saved = cc
            .storage
            .and_then(|s| eframe::get_value(s, STORAGE_KEY))
            .unwrap_or_default();
        let (job_tx, job_rx) = channel();
        let dark = saved.dark.unwrap_or(true);
        let mut app = Self {
            tabs: Vec::new(),
            active: 0,
            next_id: 1,
            projects: if saved.projects.is_empty() {
                // migrate the old "recent" list
                saved
                    .recent
                    .iter()
                    .enumerate()
                    .map(|(i, p)| Project {
                        path: p.clone(),
                        pinned: false,
                        last_opened: -(i as i64),
                    })
                    .collect()
            } else {
                saved.projects.clone()
            },
            dark,
            show_launcher: false,
            launcher: LauncherState::default(),
            scan_rx: None,
            drag_tab: None,
            scroll_to_active: true,
            job_tx,
            job_rx,
            queue: VecDeque::new(),
            running: None,
            running_tab: None,
            log: Vec::new(),
            show_log: false,
            toast: None,
            dialog: None,
            was_focused: true,
            ask_rx: {
                let ctx = cc.egui_ctx.clone();
                askpass::start(move || ctx.request_repaint())
            },
            ask_queue: VecDeque::new(),
            ask_input: String::new(),
            ask_remember: true,
            ask_focus: true,
            cred_cache: HashMap::new(),
            auto_fetch_min: saved.auto_fetch_min.unwrap_or(10),
            // first automatic fetch ~30 s after start
            last_auto_fetch: Instant::now()
                .checked_sub(Duration::from_secs(
                    saved.auto_fetch_min.unwrap_or(10) as u64 * 60,
                ))
                .and_then(|t| t.checked_add(Duration::from_secs(30)))
                .unwrap_or_else(Instant::now),
            fetch_rx: None,
            fetch_manual: false,
            github: GitHub::new(
                &cc.egui_ctx,
                if saved.github_accounts.is_empty() {
                    saved.github_login.iter().cloned().collect()
                } else {
                    saved.github_accounts.clone()
                },
                saved.github_login.clone(),
            ),
            clone_parent: saved.clone_parent.clone().unwrap_or_else(dirs_home),
            git_check: GitCheck::Found,
            show_about: false,
            logo: None,
            update: None,
            update_rx: None,
            last_update_check: saved.last_update_check,
            publish: None,
            next_tag: 1,
            agents: agents::Agents::new(&cc.egui_ctx, saved.agent_tasks.clone(), saved.last_agent),
        };
        app.apply_theme(&cc.egui_ctx);
        app.start_git_check(&cc.egui_ctx);
        if chrono::Utc::now().timestamp() - app.last_update_check > 24 * 3600 {
            app.start_update_check(&cc.egui_ctx, false);
        }
        if let Some(log) = crate::crash_log_path().filter(|p| p.exists()) {
            let prev = log.with_file_name("crash-previous.log");
            if std::fs::rename(&log, &prev).is_ok() {
                app.notify(
                    format!(
                        "FoxGit hit an internal error last time. Details: {} — please attach it when reporting the bug.",
                        prev.display()
                    ),
                    false,
                );
            }
        }

        for p in &saved.open_tabs {
            app.open_repo_quiet(Path::new(p));
        }
        app.active = saved.active.min(app.tabs.len().saturating_sub(1));
        for p in initial {
            app.open_repo(Path::new(&p));
        }
        app.show_launcher = app.tabs.is_empty();
        app
    }

    fn apply_theme(&self, ctx: &egui::Context) {
        theme::apply(ctx, self.dark);
    }

    fn start_git_check(&mut self, ctx: &egui::Context) {
        let (tx, rx) = channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(crate::shellenv::check_git());
            ctx.request_repaint();
        });
        self.git_check = GitCheck::Checking(rx);
    }

    /// `manual` = from the menu: always report the result.
    fn start_update_check(&mut self, ctx: &egui::Context, manual: bool) {
        if update::REPO.is_none() || self.update_rx.is_some() {
            return;
        }
        self.last_update_check = chrono::Utc::now().timestamp();
        let (tx, rx) = channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send((manual, update::check()));
            ctx.request_repaint();
        });
        self.update_rx = Some(rx);
    }

    fn poll_checks(&mut self) {
        if let GitCheck::Checking(rx) = &self.git_check {
            if let Ok(res) = rx.try_recv() {
                self.git_check = match res {
                    Ok(_) => GitCheck::Found,
                    Err(e) => GitCheck::Missing(e),
                };
            }
        }
        if let Some(rx) = &self.update_rx {
            if let Ok((manual, res)) = rx.try_recv() {
                self.update_rx = None;
                match res {
                    Ok(Some(rel)) => {
                        self.notify(
                            format!(
                                "FoxGit {} is available — click Update in the toolbar",
                                rel.version()
                            ),
                            true,
                        );
                        self.update = Some(rel);
                    }
                    Ok(None) if manual => self.notify(
                        format!("You're on the latest version ({})", update::VERSION),
                        true,
                    ),
                    Err(e) if manual => self.notify(format!("Update check failed: {e}"), false),
                    _ => {}
                }
            }
        }
    }

    fn git_missing_banner(&mut self, ui: &mut egui::Ui) {
        let GitCheck::Missing(err) = &self.git_check else {
            return;
        };
        let err = err.clone();
        let p = theme::palette(self.dark);
        let mut retry = false;
        egui::Panel::top("git_missing")
            .frame(
                egui::Frame::new()
                    .fill(theme::alpha(p.red, 0.14))
                    .inner_margin(egui::Margin::symmetric(14, 8)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("⚠").size(16.0).color(p.red));
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new("Git was not found on this computer")
                                .strong()
                                .color(p.text_strong),
                        );
                        let hint = if cfg!(target_os = "macos") {
                            "FoxGit needs Git to commit, pull and push. Install it with  xcode-select --install  (or from git-scm.com), then click Retry."
                        } else {
                            "FoxGit needs Git to commit, pull and push. Install it from git-scm.com, then click Retry."
                        };
                        ui.label(RichText::new(hint).color(p.subtext));
                        if !err.is_empty() {
                            ui.label(RichText::new(err.as_str()).small().color(p.faint));
                        }
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Retry").clicked() {
                            retry = true;
                        }
                        if theme::primary_button(ui, "Download Git", true).clicked() {
                            ui.ctx()
                                .open_url(egui::OpenUrl::new_tab("https://git-scm.com/downloads"));
                        }
                    });
                });
            });
        if retry {
            let ctx = ui.ctx().clone();
            self.start_git_check(&ctx);
        }
    }

    fn about_window(&mut self, ctx: &egui::Context) {
        if !self.show_about {
            return;
        }
        if self.logo.is_none() {
            if let Ok(icon) =
                eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon/foxgit-512.png"))
            {
                let img = egui::ColorImage::from_rgba_unmultiplied(
                    [icon.width as usize, icon.height as usize],
                    &icon.rgba,
                );
                self.logo = Some(ctx.load_texture("foxgit_logo", img, Default::default()));
            }
        }
        let mut open = true;
        let mut check = false;
        egui::Window::new("About FoxGit")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, -30.0])
            .show(ctx, |ui| {
                let p = theme::pal(ui);
                ui.set_width(380.0);
                ui.vertical_centered(|ui| {
                    if let Some(logo) = &self.logo {
                        ui.add(egui::Image::new((logo.id(), egui::vec2(112.0, 112.0))));
                    }
                    ui.label(
                        RichText::new("FoxGit")
                            .size(24.0)
                            .strong()
                            .color(p.text_strong),
                    );
                    ui.label(
                        RichText::new(format!("Version {}", update::VERSION)).color(p.subtext),
                    );
                    ui.add_space(6.0);
                    ui.label(RichText::new("A fast, friendly Git client.").color(p.text));
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        if let Some(url) = update::repo_url() {
                            if ui.link("Website").clicked() {
                                ui.ctx().open_url(egui::OpenUrl::new_tab(&url));
                            }
                            if ui.link("Report a problem").clicked() {
                                ui.ctx()
                                    .open_url(egui::OpenUrl::new_tab(format!("{url}/issues")));
                            }
                        }
                        if ui.link("Open-source licenses").clicked() {
                            open_licenses(ui.ctx());
                        }
                    });
                    ui.add_space(6.0);
                    if update::REPO.is_some() && ui.button("Check for updates").clicked() {
                        check = true;
                    }
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new("© 2026 FoxGit contributors · MIT License")
                            .small()
                            .color(p.faint),
                    );
                });
            });
        if check {
            self.start_update_check(ctx, true);
        }
        self.show_about = open;
    }

    fn open_repo_quiet(&mut self, path: &Path) -> bool {
        let id = self.next_id;
        self.next_id += 1;
        match RepoTab::open(id, path, DEFAULT_COMMIT_LIMIT) {
            Ok(tab) => {
                let p = tab.path.display().to_string();
                if let Some(i) = self.tabs.iter().position(|t| t.path == tab.path) {
                    self.active = i;
                } else {
                    self.tabs.push(tab);
                    self.active = self.tabs.len() - 1;
                }
                let now = chrono::Utc::now().timestamp();
                match self.projects.iter_mut().find(|x| x.path == p) {
                    Some(x) => x.last_opened = now,
                    None => self.projects.push(Project {
                        path: p,
                        pinned: false,
                        last_opened: now,
                    }),
                }
                self.show_launcher = false;
                self.scroll_to_active = true;
                true
            }
            Err(_) => false,
        }
    }

    pub fn open_repo(&mut self, path: &Path) {
        if !self.open_repo_quiet(path) {
            let msg = match Repo::open(path) {
                Err(e) => e,
                Ok(_) => "Unknown error".into(),
            };
            self.notify(format!("Cannot open {}: {msg}", path.display()), false);
        }
    }

    fn notify(&mut self, text: String, ok: bool) {
        self.toast = Some((text, ok, Instant::now()));
    }

    fn enqueue(&mut self, tab_id: Option<u64>, dir: PathBuf, kind: JobKind) {
        self.queue.push_back(Job {
            tab_id,
            dir,
            kind,
            after: None,
        });
    }

    fn enqueue_then(&mut self, tab_id: Option<u64>, dir: PathBuf, op: Op, after: AfterJob) {
        self.queue.push_back(Job {
            tab_id,
            dir,
            kind: JobKind::Op(op),
            after: Some(after),
        });
    }

    fn pump_jobs(&mut self, ctx: &egui::Context) {
        // Finished jobs
        while let Ok(done) = self.job_rx.try_recv() {
            self.running = None;
            self.running_tab = None;
            self.log.push(LogEntry {
                when: chrono::Local::now().format("%H:%M:%S").to_string(),
                title: done.title.clone(),
                ok: done.ok,
                text: done.text.clone(),
            });
            if !done.ok && looks_like_auth_failure(&done.text) {
                self.cred_cache.clear();
            }
            if done.ok {
                self.notify(format!("{} ✔", done.title), true);
            } else {
                let first = done
                    .text
                    .lines()
                    .filter(|l| !l.starts_with("$ "))
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("failed")
                    .to_string();
                self.notify(format!("{} failed: {first}", done.title), false);
            }
            if let Some(path) = done.open_after {
                if done.ok {
                    self.open_repo(&path);
                    self.github.invalidate_local();
                }
            }
            match done.after {
                Some(AfterJob::AgentMerged(id)) if done.ok => self.agents.mark_merged(id),
                Some(AfterJob::OpenUrl(url)) if done.ok => {
                    ctx.open_url(egui::OpenUrl::new_tab(url));
                }
                Some(AfterJob::AgentDiscarded(id)) if done.ok => {
                    self.agents.remove(id);
                    for t in &mut self.tabs {
                        if t.agent_chat == Some(id) {
                            t.agent_chat = None;
                        }
                    }
                }
                _ => {}
            }
            if let Some(id) = done.tab_id {
                if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == id) {
                    tab.reload_fast();
                    tab.start_history_load(Some(ctx));
                    if done.ok && (done.title == "Commit" || done.title == "Amend commit") {
                        tab.commit_msg.clear();
                        tab.amend = false;
                    }
                }
            }
        }

        // Start next job
        if self.running.is_none() {
            if let Some(job) = self.queue.pop_front() {
                let (title, open_after) = match &job.kind {
                    JobKind::Op(op) => (op.title(), None),
                    JobKind::Clone { url, parent, name } => {
                        (format!("Clone {url}"), Some(parent.join(name)))
                    }
                };
                self.running = Some(title.clone());
                self.running_tab = job.tab_id;
                let tx = self.job_tx.clone();
                let ctx = ctx.clone();
                std::thread::spawn(move || {
                    let out = match &job.kind {
                        JobKind::Op(op) => op.run(&job.dir),
                        JobKind::Clone { url, parent, name } => git::clone_repo(url, parent, name),
                    };
                    let _ = tx.send(JobDone {
                        tab_id: job.tab_id,
                        title,
                        ok: out.ok,
                        text: out.text,
                        open_after,
                        after: job.after,
                    });
                    ctx.request_repaint();
                });
            }
        }
    }

    fn apply_actions(&mut self, actions: Vec<Action>, ctx: &egui::Context) {
        for a in actions {
            match a {
                Action::Run(op) => {
                    if let Some(tab) = self.tabs.get(self.active) {
                        let (id, dir) = (tab.id, tab.path.clone());
                        self.enqueue(Some(id), dir, JobKind::Op(op));
                    }
                }
                Action::Dialog(d) => self.dialog = Some(d),
                Action::Refresh => {
                    if let Some(tab) = self.tabs.get_mut(self.active) {
                        tab.reload_fast();
                        tab.start_history_load(Some(ctx));
                    }
                }
                Action::AddRemote => self.open_add_remote(),
                Action::NewAgentTask => {
                    if let Some(tab) = self.tabs.get(self.active) {
                        if tab.repo.is_head_unborn() {
                            self.notify(
                                "Make a first commit before starting an agent — it works on a copy of your branch".into(),
                                false,
                            );
                        } else {
                            let base = if tab.refs.head_detached {
                                tab.refs
                                    .head_oid
                                    .map(|o| o.to_string())
                                    .unwrap_or_else(|| "HEAD".into())
                            } else {
                                tab.refs.head_name.clone().unwrap_or_else(|| "HEAD".into())
                            };
                            self.agents.open_new(&tab.path, &tab.name, &base);
                        }
                    }
                }
                Action::EditRemote(name) => {
                    if let Some(tab) = self.tabs.get(self.active) {
                        let url = tab.refs.remote_urls.get(&name).cloned().unwrap_or_default();
                        self.dialog = Some(Dialog::EditRemote {
                            original: name.clone(),
                            name,
                            url,
                        });
                    }
                }
                Action::LoadMore => {
                    if let Some(tab) = self.tabs.get_mut(self.active) {
                        tab.commit_limit *= 2;
                        tab.start_history_load(Some(ctx));
                    }
                }
            }
        }
    }

    fn apply_agent_actions(&mut self, acts: Vec<agents::TaskAction>) {
        for a in acts {
            match a {
                agents::TaskAction::Review(path) => self.open_repo(&path),
                agents::TaskAction::Merge {
                    task,
                    repo,
                    worktree,
                    branch,
                    message,
                    commit_first,
                    squash,
                } => {
                    let tab_id = self.tabs.iter().find(|t| t.path == repo).map(|t| t.id);
                    self.enqueue_then(
                        tab_id,
                        repo,
                        Op::AgentMerge {
                            worktree: worktree.display().to_string(),
                            branch,
                            message,
                            commit_first,
                            squash,
                        },
                        AfterJob::AgentMerged(task),
                    );
                    if squash {
                        self.notify(
                            "The agent's changes will be staged in your repository — review them in File Status and commit".into(),
                            true,
                        );
                    }
                }
                agents::TaskAction::Notify(text, ok) => self.notify(text, ok),
                agents::TaskAction::PushPr {
                    repo,
                    worktree,
                    branch,
                    remote,
                    message,
                    commit_first,
                    compare_url,
                } => {
                    let tab_id = self.tabs.iter().find(|t| t.path == repo).map(|t| t.id);
                    let op = Op::AgentPush {
                        worktree: worktree.display().to_string(),
                        branch: branch.clone(),
                        remote,
                        message,
                        commit_first,
                    };
                    match compare_url {
                        Some(url) => self.enqueue_then(tab_id, repo, op, AfterJob::OpenUrl(url)),
                        None => self.enqueue(tab_id, repo, JobKind::Op(op)),
                    }
                }
                agents::TaskAction::Discard {
                    task,
                    repo,
                    worktree,
                    branch,
                } => {
                    self.dialog = Some(Dialog::DiscardAgent {
                        task,
                        repo,
                        worktree,
                        branch,
                    })
                }
            }
        }
    }

    fn open_add_remote(&mut self) {
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        let taken = &tab.refs.remote_names;
        let name = ["origin", "upstream", "github"]
            .into_iter()
            .find(|n| !taken.iter().any(|t| t == n))
            .unwrap_or("remote")
            .to_string();
        let branch = (!tab.repo.is_head_unborn())
            .then(|| tab.refs.head_name.clone())
            .flatten()
            .filter(|_| !tab.refs.head_detached);
        self.dialog = Some(Dialog::AddRemote {
            name,
            url: String::new(),
            fetch: true,
            github: self.github.login().is_some(),
            form: PublishForm {
                owner: self.github.login().unwrap_or_default().to_string(),
                repo: github::sanitize_repo_name(&tab.name),
                description: String::new(),
                private: true,
                push: branch.is_some(),
                branch,
                busy: None,
                error: None,
            },
        });
    }

    /// A "Create on GitHub" request finished: add the remote (and push) or show the error.
    fn poll_publish(&mut self) {
        for (tag, res) in self.github.take_created() {
            let Some(p) = self.publish.take_if(|p| p.tag == tag) else {
                continue;
            };
            match res {
                Ok(repo) => {
                    self.enqueue(
                        Some(p.tab_id),
                        p.dir.clone(),
                        JobKind::Op(Op::RemoteAdd {
                            name: p.remote.clone(),
                            url: repo.clone_url.clone(),
                            fetch: false,
                        }),
                    );
                    if let Some(branch) = p.push_branch {
                        self.enqueue(
                            Some(p.tab_id),
                            p.dir.clone(),
                            JobKind::Op(Op::Push {
                                set_upstream: Some((p.remote.clone(), branch)),
                                force: false,
                            }),
                        );
                    }
                    if matches!(&self.dialog, Some(Dialog::AddRemote { form, .. }) if form.busy == Some(tag))
                    {
                        self.dialog = None;
                    }
                    self.notify(
                        format!(
                            "Created {} on GitHub and added it as '{}'",
                            repo.full_name, p.remote
                        ),
                        true,
                    );
                }
                Err(e) => {
                    if let Some(Dialog::AddRemote { form, .. }) = &mut self.dialog {
                        if form.busy == Some(tag) {
                            form.busy = None;
                            form.error = Some(e);
                            continue;
                        }
                    }
                    self.notify(
                        format!("Could not create the GitHub repository: {e}"),
                        false,
                    );
                }
            }
        }
    }

    fn pick_folder(&mut self) {
        if let Some(dir) = rfd::FileDialog::new()
            .set_title("Open Git repository")
            .pick_folder()
        {
            self.open_repo(&dir);
        }
    }

    fn init_folder(&mut self) {
        if let Some(dir) = rfd::FileDialog::new()
            .set_title("Create new repository in folder")
            .pick_folder()
        {
            match git::init_repo(&dir) {
                Ok(()) => self.open_repo(&dir),
                Err(e) => self.notify(format!("git init failed: {e}"), false),
            }
        }
    }

    // ------------------------------------------------------------------ UI

    fn toolbar(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        let has_tab = !self.tabs.is_empty() && !self.show_launcher;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;

            let repo_btn = theme::ghost_button(ui, "☰", "Repository", 0, false);
            egui::Popup::menu(&repo_btn).show(|ui| {
                ui.set_min_width(250.0);
                if ui
                    .button(format!("Open…   ({})", theme::shortcut("O")))
                    .clicked()
                {
                    ui.close();
                    self.pick_folder();
                }
                if ui.button("Clone…").clicked() {
                    ui.close();
                    self.dialog = Some(Dialog::Clone {
                        url: String::new(),
                        parent: self.clone_parent.clone(),
                        name: String::new(),
                    });
                }
                if ui.button("New repository (git init)…").clicked() {
                    ui.close();
                    self.init_folder();
                }
                if ui
                    .add_enabled(
                        !self.tabs.is_empty() && !self.show_launcher,
                        egui::Button::new("Add remote…"),
                    )
                    .clicked()
                {
                    ui.close();
                    self.open_add_remote();
                }
                if ui
                    .button(format!("Projects…   ({})", theme::shortcut("T")))
                    .clicked()
                {
                    ui.close();
                    self.open_launcher();
                }
                ui.separator();
                if ui
                    .add_enabled(
                        self.fetch_rx.is_none(),
                        egui::Button::new("Fetch all open projects now"),
                    )
                    .clicked()
                {
                    ui.close();
                    let ctx = ui.ctx().clone();
                    self.start_fetch_all(&ctx, true);
                }
                ui.menu_button("Auto-fetch", |ui| {
                    ui.label(RichText::new("Fetch every open project in the background").weak());
                    for (m, label) in [
                        (0, "Off"),
                        (5, "Every 5 minutes"),
                        (10, "Every 10 minutes"),
                        (15, "Every 15 minutes"),
                        (30, "Every 30 minutes"),
                    ] {
                        if ui.radio(self.auto_fetch_min == m, label).clicked() {
                            self.auto_fetch_min = m;
                            self.last_auto_fetch = Instant::now();
                            ui.close();
                        }
                    }
                });
                ui.separator();
                ui.label(RichText::new("Recent").weak());
                let mut open = None;
                for r in tabs::sorted_projects(&self.projects).iter().take(12) {
                    if ui.button(format!("{}   {}", r.name(), r.path)).clicked() {
                        open = Some(r.path.clone());
                        ui.close();
                    }
                }
                if let Some(r) = open {
                    self.open_repo(Path::new(&r));
                }
                ui.separator();
                if ui
                    .button(if self.dark {
                        "☀  Light theme"
                    } else {
                        "🌙  Dark theme"
                    })
                    .clicked()
                {
                    self.dark = !self.dark;
                    self.apply_theme(ui.ctx());
                    ui.close();
                }
                ui.separator();
                if ui
                    .add_enabled(
                        update::REPO.is_some(),
                        egui::Button::new("Check for updates…"),
                    )
                    .clicked()
                {
                    ui.close();
                    let ctx = ui.ctx().clone();
                    self.start_update_check(&ctx, true);
                }
                if ui.button("About FoxGit").clicked() {
                    ui.close();
                    self.show_about = true;
                }
            });

            toolbar_divider(ui);

            ui.add_enabled_ui(has_tab, |ui| {
                let tab = self.tabs.get(self.active);
                let (ahead, behind) = tab
                    .and_then(|t| t.head_branch())
                    .map(|b| (b.ahead, b.behind))
                    .unwrap_or((0, 0));
                let changes = tab
                    .map(|t| t.status.staged.len() + t.status.unstaged.len())
                    .unwrap_or(0);

                let mut go_status = false;
                if theme::ghost_button(ui, "✚", "Commit", changes, false)
                    .on_hover_text("Stage and commit changes")
                    .clicked()
                {
                    go_status = true;
                }
                if theme::ghost_button(ui, "⬇", "Pull", behind, false)
                    .on_hover_text("Pull from the tracked branch")
                    .clicked()
                {
                    actions.push(Action::Dialog(Dialog::Pull { rebase: false }));
                }
                if theme::ghost_button(ui, "⬆", "Push", ahead, false)
                    .on_hover_text("Push local commits")
                    .clicked()
                {
                    if let Some(t) = tab {
                        actions.push(Action::Dialog(push_dialog_for(t)));
                    }
                }
                if theme::ghost_button(ui, "⟳", "Fetch", 0, false)
                    .on_hover_text("Fetch from all remotes")
                    .clicked()
                {
                    actions.push(Action::Run(Op::Fetch));
                }
                toolbar_divider(ui);
                if theme::ghost_button(ui, theme::BRANCH, "Branch", 0, false)
                    .on_hover_text("Create a new branch")
                    .clicked()
                {
                    actions.push(Action::Dialog(Dialog::NewBranch {
                        name: String::new(),
                        start: None,
                        start_label: "current HEAD".into(),
                        checkout: true,
                    }));
                }
                if theme::ghost_button(ui, "📦", "Stash", 0, false)
                    .on_hover_text("Stash all local changes")
                    .clicked()
                {
                    actions.push(Action::Dialog(Dialog::Stash {
                        message: String::new(),
                    }));
                }
                toolbar_divider(ui);
                if theme::ghost_button(ui, "↻", "Refresh", 0, false)
                    .on_hover_text("Reload (F5)")
                    .clicked()
                {
                    actions.push(Action::Refresh);
                }
                if go_status {
                    if let Some(t) = self.tabs.get_mut(self.active) {
                        t.view = View::FileStatus;
                    }
                }
            });

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                if let Some(rel) = &self.update {
                    let p = theme::pal(ui);
                    let btn = egui::Button::new(
                        RichText::new(format!("⬆  Update to {}", rel.version()))
                            .strong()
                            .color(p.on_accent),
                    )
                    .fill(p.green)
                    .stroke(egui::Stroke::NONE)
                    .corner_radius(14.0)
                    .min_size(egui::vec2(0.0, 26.0));
                    if ui
                        .add(btn)
                        .on_hover_text("Open the download page for the new version")
                        .clicked()
                    {
                        ui.ctx().open_url(egui::OpenUrl::new_tab(&rel.html_url));
                    }
                }
                match self.github.toolbar_button(ui) {
                    Some(MenuAction::OpenRemote) => {
                        self.open_launcher();
                        self.launcher.mode = LauncherMode::Remote;
                    }
                    Some(MenuAction::SignIn) => self.github.open_signin(),
                    None => {}
                }
                toolbar_divider(ui);
                if theme::ghost_button(ui, "📋", "Log", 0, self.show_log)
                    .on_hover_text("Show git command output")
                    .clicked()
                {
                    self.show_log = !self.show_log;
                }
                if let Some(r) = &self.running {
                    ui.label(RichText::new(r).weak());
                    ui.add(egui::Spinner::new());
                }
            });
        });
    }

    // ------------------------------------------------------- credentials

    fn poll_ask(&mut self) {
        let Some(rx) = &self.ask_rx else { return };
        while let Ok(req) = rx.try_recv() {
            if let Some(v) = self.cred_cache.get(&req.prompt) {
                let v = v.clone();
                req.answer(Some(v));
            } else if req.background {
                // never interrupt the user for automatic fetches
                req.answer(None);
            } else {
                if self.ask_queue.is_empty() {
                    self.ask_input.clear();
                    self.ask_focus = true;
                }
                self.ask_queue.push_back(req);
            }
        }
    }

    fn credential_dialog(&mut self, ctx: &egui::Context) {
        let Some(req) = self.ask_queue.front() else {
            return;
        };
        let secret = req.is_secret();
        let prompt = req.prompt.clone();
        let mut submit = false;
        let mut cancel = false;
        let lower = prompt.to_lowercase();
        let is_github =
            lower.contains("github.com") || lower.contains("gitlab") || lower.contains("bitbucket");
        let is_yes_no = lower.contains("(yes/no");

        egui::Modal::new(egui::Id::unique("credential_prompt")).show(ctx, |ui| {
            ui.set_width(440.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("🔑").size(22.0));
                ui.heading(if is_yes_no { "Confirm connection" } else { "Sign in to remote" });
            });
            ui.add_space(6.0);
            ui.label(RichText::new(prompt.trim_end_matches(':').trim()).strong());
            ui.add_space(6.0);
            let edit = egui::TextEdit::singleline(&mut self.ask_input)
                .password(secret)
                .desired_width(f32::INFINITY)
                .hint_text(if is_yes_no { "yes / no" } else if secret { "password or token" } else { "username" });
            let r = ui.add(edit);
            if self.ask_focus {
                r.request_focus();
                self.ask_focus = false;
            }
            if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                submit = true;
            }
            if secret && is_github {
                ui.label(
                    RichText::new("GitHub / GitLab / Bitbucket need a personal access token here, not your account password.")
                        .small()
                        .weak(),
                );
            }
            if !is_yes_no {
                ui.checkbox(&mut self.ask_remember, "Remember until FoxGit is closed (memory only, never written to disk)");
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button(RichText::new("  OK  ").strong()).clicked() {
                    submit = true;
                }
                if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    cancel = true;
                }
                if self.ask_queue.len() > 1 {
                    ui.label(RichText::new(format!("{} more prompt(s) waiting", self.ask_queue.len() - 1)).weak());
                }
            });
        });

        if submit || cancel {
            let req = self.ask_queue.pop_front().unwrap();
            if submit {
                let value = std::mem::take(&mut self.ask_input);
                if self.ask_remember && !is_yes_no {
                    self.cred_cache.insert(req.prompt.clone(), value.clone());
                }
                req.answer(Some(value));
            } else {
                self.ask_input.clear();
                req.answer(None);
            }
            self.ask_focus = true;
        }
    }

    // -------------------------------------------------------- auto-fetch

    fn start_fetch_all(&mut self, ctx: &egui::Context, manual: bool) {
        if self.fetch_rx.is_some() {
            return;
        }
        let targets: Vec<(u64, PathBuf)> = self
            .tabs
            .iter()
            .filter(|t| !t.refs.remote_names.is_empty())
            .map(|t| (t.id, t.path.clone()))
            .collect();
        self.last_auto_fetch = Instant::now();
        if targets.is_empty() {
            if manual {
                self.notify("No open project has a remote to fetch from".into(), true);
            }
            return;
        }
        let (tx, rx) = channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let args: Vec<String> = ["fetch", "--all", "--prune", "--quiet"]
                .iter()
                .map(|s| s.to_string())
                .collect();
            for (id, path) in targets {
                let out = git::run_git_ext(&path, &args, None, !manual);
                let _ = tx.send(FetchMsg::Tab {
                    id,
                    ok: out.ok,
                    text: out.text,
                });
                ctx.request_repaint();
            }
            let _ = tx.send(FetchMsg::Done);
            ctx.request_repaint();
        });
        self.fetch_rx = Some(rx);
        self.fetch_manual = manual;
        if manual {
            self.running = Some("Fetching all projects".into());
        }
    }

    fn poll_fetch(&mut self, ctx: &egui::Context) {
        let Some(rx) = &self.fetch_rx else { return };
        let mut msgs = Vec::new();
        while let Ok(m) = rx.try_recv() {
            msgs.push(m);
        }
        for m in msgs {
            match m {
                FetchMsg::Tab { id, ok, text } => {
                    let Some(tab) = self.tabs.iter_mut().find(|t| t.id == id) else {
                        continue;
                    };
                    let before = tab.head_branch().map(|b| b.behind).unwrap_or(0);
                    tab.reload_fast();
                    tab.start_history_load(Some(ctx));
                    tab.last_fetch = Some(Instant::now());
                    let after = tab.head_branch().map(|b| b.behind).unwrap_or(0);
                    let name = tab.name.clone();
                    let upstream = tab
                        .head_branch()
                        .and_then(|b| b.upstream.clone())
                        .unwrap_or_default();
                    if !ok || self.fetch_manual {
                        self.log.push(LogEntry {
                            when: chrono::Local::now().format("%H:%M:%S").to_string(),
                            title: format!(
                                "{} fetch {name}",
                                if self.fetch_manual { "Manual" } else { "Auto" }
                            ),
                            ok,
                            text: if text.is_empty() {
                                "$ git fetch --all --prune".into()
                            } else {
                                text.clone()
                            },
                        });
                    }
                    if after > before {
                        self.notify(
                            format!(
                                "{name}: {} new commit(s) on {upstream} — Pull to update",
                                after - before
                            ),
                            true,
                        );
                    } else if !ok && self.fetch_manual {
                        let first = text
                            .lines()
                            .find(|l| !l.trim().is_empty())
                            .unwrap_or("failed")
                            .to_string();
                        self.notify(format!("Fetch {name} failed: {first}"), false);
                    }
                    if !ok && looks_like_auth_failure(&text) && self.fetch_manual {
                        self.cred_cache.clear();
                    }
                }
                FetchMsg::Done => {
                    self.fetch_rx = None;
                    if self.fetch_manual {
                        self.running = None;
                        self.fetch_manual = false;
                    }
                }
            }
        }
    }

    // ------------------------------------------------------------ tabs

    fn open_launcher(&mut self) {
        self.github.invalidate_local();
        self.show_launcher = true;
        self.launcher.focus_search = true;
        self.launcher.query.clear();
        self.launcher.selected = 0;
        self.launcher.branches.clear(); // re-read branches
        self.scroll_to_active = true;
    }

    fn active_id(&self) -> Option<u64> {
        self.tabs.get(self.active).map(|t| t.id)
    }

    fn set_active_id(&mut self, id: Option<u64>) {
        if let Some(i) = id.and_then(|id| self.tabs.iter().position(|t| t.id == id)) {
            self.active = i;
        }
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len().saturating_sub(1);
        }
        if self.tabs.is_empty() {
            self.show_launcher = true;
        }
    }

    fn select_tab(&mut self, i: usize) {
        if i < self.tabs.len() {
            self.active = i;
            self.show_launcher = false;
            self.scroll_to_active = true;
        }
    }

    fn close_tab(&mut self, i: usize) {
        if i >= self.tabs.len() {
            return;
        }
        let keep = if i == self.active {
            // like a browser: activate the tab to the right, else the one to the left
            self.tabs
                .get(i + 1)
                .or_else(|| i.checked_sub(1).and_then(|j| self.tabs.get(j)))
                .map(|t| t.id)
        } else {
            self.active_id()
        };
        self.tabs.remove(i);
        self.set_active_id(keep);
    }

    fn tab_colors(&self) -> Vec<Color32> {
        let paths: Vec<String> = self
            .tabs
            .iter()
            .map(|t| t.path.display().to_string())
            .collect();
        tabs::assign_colors(&paths)
    }

    fn tab_infos(&self) -> Vec<TabInfo> {
        let colors = self.tab_colors();
        self.tabs
            .iter()
            .zip(colors)
            .map(|(t, color)| {
                let head = t.head_branch();
                TabInfo {
                    color,
                    name: t.name.clone(),
                    path: t.path.display().to_string(),
                    branch: t.refs.head_name.clone().map(|b| {
                        if t.refs.head_detached {
                            "detached".to_string()
                        } else {
                            b
                        }
                    }),
                    changes: t.status.staged.len() + t.status.unstaged.len(),
                    ahead: head.map(|b| b.ahead).unwrap_or(0),
                    behind: head.map(|b| b.behind).unwrap_or(0),
                    busy: t.is_loading() || self.running_tab == Some(t.id),
                    conflict: t
                        .status
                        .unstaged
                        .iter()
                        .any(|f| f.kind == git::ChangeKind::Conflicted),
                    fetched: t.last_fetch.map(|at| {
                        let m = at.elapsed().as_secs() / 60;
                        if m == 0 {
                            "just now".to_string()
                        } else {
                            format!("{m} min ago")
                        }
                    }),
                }
            })
            .collect()
    }

    fn tab_strip_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let infos = self.tab_infos();
        let projects = self.projects.clone();
        let pinned = move |path: &str| projects.iter().any(|p| p.path == path && p.pinned);
        let events = tabs::tab_strip(
            ui,
            &infos,
            tabs::StripState {
                active: (!self.tabs.is_empty()).then_some(self.active),
                launcher_active: self.show_launcher,
                drag: &mut self.drag_tab,
                scroll_to_active: &mut self.scroll_to_active,
                pinned: &pinned,
            },
        );
        for e in events {
            match e {
                TabEvent::Select(i) => self.select_tab(i),
                TabEvent::Close(i) => self.close_tab(i),
                TabEvent::CloseOthers(i) => {
                    if let Some(t) = self.tabs.get(i) {
                        let id = t.id;
                        self.tabs.retain(|t| t.id == id);
                        self.set_active_id(Some(id));
                        self.show_launcher = false;
                    }
                }
                TabEvent::CloseRight(i) => {
                    let keep = self.active_id();
                    self.tabs.truncate(i + 1);
                    self.set_active_id(keep);
                    if self.active > i {
                        self.active = i;
                    }
                }
                TabEvent::Move { from, to } => {
                    if from < self.tabs.len() && to < self.tabs.len() {
                        let keep = self.active_id();
                        let t = self.tabs.remove(from);
                        self.tabs.insert(to, t);
                        self.set_active_id(keep);
                    }
                }
                TabEvent::Refresh(i) => {
                    if let Some(t) = self.tabs.get_mut(i) {
                        t.reload_fast();
                        t.start_history_load(Some(ctx));
                    }
                }
                TabEvent::Reveal(i) => {
                    if let Some(t) = self.tabs.get(i) {
                        tabs::reveal_in_file_manager(&t.path);
                    }
                }
                TabEvent::TogglePin(i) => {
                    if let Some(t) = self.tabs.get(i) {
                        let path = t.path.display().to_string();
                        self.toggle_pin(&path);
                    }
                }
                TabEvent::OpenLauncher => self.open_launcher(),
                TabEvent::CloseLauncher => {
                    self.show_launcher = false;
                    self.scroll_to_active = true;
                }
            }
        }
    }

    fn toggle_pin(&mut self, path: &str) {
        match self.projects.iter_mut().find(|p| p.path == path) {
            Some(p) => p.pinned = !p.pinned,
            None => self.projects.push(Project {
                path: path.to_string(),
                pinned: true,
                last_opened: chrono::Utc::now().timestamp(),
            }),
        }
    }

    fn launcher_ui(&mut self, ui: &mut egui::Ui) {
        let open: Vec<String> = self
            .tabs
            .iter()
            .map(|t| t.path.display().to_string())
            .collect();
        self.launcher.scanning = self.scan_rx.is_some();
        let colors = self.tab_colors();
        let mut remote_events = Vec::new();
        let github = &mut self.github;
        let projects = &self.projects;
        let events = tabs::launcher(
            ui,
            &mut self.launcher,
            projects,
            &open,
            &colors,
            &mut |ui, query| remote_events.extend(github.remote_page(ui, query, projects)),
        );
        for e in remote_events {
            match e {
                RemoteEvent::Clone { url, name } => {
                    self.dialog = Some(Dialog::Clone {
                        url,
                        parent: self.clone_parent.clone(),
                        name,
                    })
                }
                RemoteEvent::Open(path) => {
                    if let Some(i) = self
                        .tabs
                        .iter()
                        .position(|t| t.path.display().to_string() == path)
                    {
                        self.select_tab(i);
                    } else {
                        self.open_repo(Path::new(&path));
                    }
                }
                RemoteEvent::SignIn => self.github.open_signin(),
            }
        }
        for e in events {
            match e {
                LauncherEvent::Open(path) => {
                    if let Some(i) = self
                        .tabs
                        .iter()
                        .position(|t| t.path.display().to_string() == path)
                    {
                        self.select_tab(i);
                    } else {
                        self.open_repo(Path::new(&path));
                    }
                }
                LauncherEvent::OpenMany(paths) => {
                    for p in paths {
                        self.open_repo_quiet(Path::new(&p));
                    }
                }
                LauncherEvent::Remove(path) => self.projects.retain(|p| p.path != path),
                LauncherEvent::TogglePin(path) => self.toggle_pin(&path),
                LauncherEvent::PickFolder => self.pick_folder(),
                LauncherEvent::Clone => {
                    self.dialog = Some(Dialog::Clone {
                        url: String::new(),
                        parent: self.clone_parent.clone(),
                        name: String::new(),
                    })
                }
                LauncherEvent::Init => self.init_folder(),
                LauncherEvent::Scan => {
                    if let Some(dir) = rfd::FileDialog::new()
                        .set_title("Scan a folder for Git repositories")
                        .pick_folder()
                    {
                        let (tx, rx) = channel();
                        let ctx = ui.ctx().clone();
                        std::thread::spawn(move || {
                            let found = tabs::scan_for_repos(&dir, 4, 500);
                            let _ = tx.send(found);
                            ctx.request_repaint();
                        });
                        self.scan_rx = Some(rx);
                    }
                }
            }
        }
    }

    fn poll_scan(&mut self) {
        let Some(rx) = &self.scan_rx else { return };
        if let Ok(found) = rx.try_recv() {
            self.scan_rx = None;
            let mut added = 0;
            for p in found {
                // Store the same canonical form that Repo::open uses (workdir with trailing slash)
                let path = Repo::open(&p)
                    .map(|r| r.workdir.display().to_string())
                    .unwrap_or_else(|_| p.display().to_string());
                if !self.projects.iter().any(|x| x.path == path) {
                    self.projects.push(Project {
                        path,
                        pinned: false,
                        last_opened: 0,
                    });
                    added += 1;
                }
            }
            self.notify(format!("Scan finished: {added} new project(s) added"), true);
        }
    }

    fn status_bar(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if self.show_launcher {
                ui.label(
                    RichText::new(if cfg!(target_os = "macos") {
                        "⌘T projects · Ctrl+Tab next tab · ⌘1…9 jump to tab · ⌘W close tab"
                    } else {
                        "Ctrl+T projects · Ctrl+Tab next tab · Ctrl+1…9 jump to tab · Ctrl+W close tab"
                    })
                        .weak(),
                );
                return;
            }
            if let Some(tab) = self.tabs.get(self.active) {
                let branch = tab
                    .refs
                    .head_name
                    .clone()
                    .unwrap_or_else(|| "(none)".into());
                let detached = if tab.refs.head_detached {
                    " (detached)"
                } else {
                    ""
                };
                let p = theme::pal(ui);
                ui.label(
                    RichText::new(format!("{} {branch}{detached}", theme::BRANCH))
                        .color(p.text_strong),
                );
                if let Some(b) = tab.head_branch() {
                    if let Some(up) = &b.upstream {
                        ui.label(RichText::new(format!("→ {up}")).color(p.faint));
                        if b.ahead > 0 {
                            ui.label(RichText::new(format!("↑{}", b.ahead)).monospace().color(p.accent_hover));
                        }
                        if b.behind > 0 {
                            ui.label(RichText::new(format!("↓{}", b.behind)).monospace().color(p.orange));
                        }
                    }
                }
                ui.separator();
                ui.label(
                    RichText::new(format!(
                        "{} staged · {} unstaged · {} commits{}",
                        tab.status.staged.len(),
                        tab.status.unstaged.len(),
                        tab.commits.len(),
                        if tab.commits.len() >= tab.commit_limit {
                            "+"
                        } else {
                            ""
                        }
                    ))
                    .weak(),
                );
                if tab.is_loading() {
                    ui.add(egui::Spinner::new().size(12.0));
                }
                if let Some(e) = &tab.load_error {
                    ui.colored_label(p.red, e);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new(tab.path.display().to_string()).color(p.faint));
                });
            }
        });
    }

    fn log_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_log;
        egui::Window::new("Git output")
            .open(&mut open)
            .default_size([640.0, 360.0])
            .show(ctx, |ui| {
                if ui.button("Clear").clicked() {
                    self.log.clear();
                }
                egui::ScrollArea::vertical()
                    .stick_to_bottom(true)
                    .auto_shrink(false)
                    .show(ui, |ui| {
                        let p = theme::pal(ui);
                        for entry in &self.log {
                            let color = if entry.ok { p.green } else { p.red };
                            ui.label(
                                RichText::new(format!("[{}] {}", entry.when, entry.title))
                                    .color(color)
                                    .strong(),
                            );
                            ui.label(RichText::new(&entry.text).monospace().size(12.0));
                            ui.separator();
                        }
                    });
            });
        self.show_log = open;
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        let Some(mut dialog) = self.dialog.take() else {
            return;
        };
        let mut keep = true;
        let mut run: Option<Op> = None;
        let mut clone: Option<(String, PathBuf, String)> = None;
        let mut publish = false;
        let mut discard_agent: Option<(u64, PathBuf, PathBuf, String)> = None;
        let signed_in = self.github.login().is_some();
        let owners = if matches!(&dialog, Dialog::AddRemote { github: true, .. }) {
            self.github.owners()
        } else {
            Vec::new()
        };
        let remote_names: Vec<String> = self
            .tabs
            .get(self.active)
            .map(|t| t.refs.remote_names.clone())
            .unwrap_or_default();
        let valid_remote_name = |n: &str, except: Option<&str>| {
            let n = n.trim();
            !n.is_empty()
                && !n.contains(char::is_whitespace)
                && (Some(n) == except || !remote_names.iter().any(|r| r == n))
        };

        let title = match &dialog {
            Dialog::NewBranch { .. } => "New branch",
            Dialog::NewTag { .. } => "New tag",
            Dialog::Push { .. } => "Push",
            Dialog::Pull { .. } => "Pull",
            Dialog::Stash { .. } => "Stash changes",
            Dialog::Clone { .. } => "Clone repository",
            Dialog::Confirm { title, .. } => title.as_str(),
            Dialog::AddRemote { .. } => "Add remote",
            Dialog::DiscardAgent { .. } => "Discard agent work",
            Dialog::EditRemote { .. } => "Edit remote",
        }
        .to_string();

        let enter = ctx.input(|i| i.key_pressed(egui::Key::Enter) && !i.modifiers.shift);
        let esc = ctx.input(|i| i.key_pressed(egui::Key::Escape));

        egui::Window::new(title)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, -40.0])
            .default_width(420.0)
            .show(ctx, |ui| {
                ui.set_min_width(380.0);
                let ok_label;
                let mut ok_enabled = true;
                match &mut dialog {
                    Dialog::NewBranch {
                        name,
                        start,
                        start_label,
                        checkout,
                    } => {
                        ui.label("Branch name:");
                        ui.add(egui::TextEdit::singleline(name).desired_width(f32::INFINITY))
                            .request_focus();
                        ui.label(RichText::new(format!("Starting at: {start_label}")).weak());
                        ui.checkbox(checkout, "Checkout new branch");
                        ok_enabled = !name.trim().is_empty() && !name.contains(' ');
                        ok_label = "Create branch";
                        if ok_enabled && (enter || false) {
                            run = Some(Op::CreateBranch {
                                name: name.trim().into(),
                                start: start.clone(),
                                checkout: *checkout,
                            });
                        }
                    }
                    Dialog::NewTag { name, commit } => {
                        ui.label("Tag name:");
                        ui.add(egui::TextEdit::singleline(name).desired_width(f32::INFINITY))
                            .request_focus();
                        ui.label(RichText::new(format!("Commit: {}", &commit[..7])).weak());
                        ok_enabled = !name.trim().is_empty() && !name.contains(' ');
                        ok_label = "Create tag";
                        if ok_enabled && enter {
                            run = Some(Op::CreateTag {
                                name: name.trim().into(),
                                commit: commit.clone(),
                            });
                        }
                    }
                    Dialog::Push {
                        remote,
                        branch,
                        set_upstream,
                        force,
                    } => {
                        ui.horizontal(|ui| {
                            ui.label("Remote:");
                            ui.text_edit_singleline(remote);
                        });
                        ui.horizontal(|ui| {
                            ui.label("Branch:");
                            ui.text_edit_singleline(branch);
                        });
                        ui.checkbox(set_upstream, "Set upstream (track remote branch)");
                        ui.checkbox(force, "Force push (--force-with-lease)");
                        ok_label = "Push";
                        ok_enabled = !remote.trim().is_empty() && !branch.trim().is_empty();
                    }
                    Dialog::Pull { rebase } => {
                        ui.label("Pull from the tracked remote branch.");
                        ui.checkbox(rebase, "Rebase instead of merge");
                        ok_label = "Pull";
                    }
                    Dialog::Stash { message } => {
                        ui.label("Message (optional):");
                        ui.add(egui::TextEdit::singleline(message).desired_width(f32::INFINITY))
                            .request_focus();
                        ui.label(RichText::new("Includes untracked files.").weak());
                        ok_label = "Stash";
                        if enter {
                            run = Some(Op::StashSave(message.clone()));
                        }
                    }
                    Dialog::Clone { url, parent, name } => {
                        ui.label("Source URL:");
                        let r = ui.add(
                            egui::TextEdit::singleline(url)
                                .hint_text("https://github.com/user/repo.git")
                                .desired_width(f32::INFINITY),
                        );
                        if r.changed() {
                            *name = repo_name_from_url(url);
                        }
                        ui.label("Destination folder:");
                        ui.horizontal(|ui| {
                            ui.add(egui::TextEdit::singleline(parent).desired_width(300.0));
                            if ui.button("Browse…").clicked() {
                                if let Some(d) = rfd::FileDialog::new().pick_folder() {
                                    *parent = d.display().to_string();
                                }
                            }
                        });
                        ui.label("Name:");
                        ui.add(egui::TextEdit::singleline(name).desired_width(f32::INFINITY));
                        ok_label = "Clone";
                        ok_enabled = !url.trim().is_empty() && !name.trim().is_empty();
                    }
                    Dialog::Confirm { text, danger, .. } => {
                        ui.label(text.as_str());
                        ok_label = if *danger { "Yes, do it" } else { "OK" };
                    }
                    Dialog::AddRemote {
                        name,
                        url,
                        fetch,
                        github,
                        form,
                    } => {
                        if signed_in {
                            let mut idx = usize::from(*github);
                            if theme::segmented(ui, &["Enter a URL", "Create on GitHub"], &mut idx) {
                                *github = idx == 1;
                            }
                            ui.add_space(8.0);
                        }
                        if !*github {
                            ui.label("Remote name:");
                            ui.add(egui::TextEdit::singleline(name).desired_width(f32::INFINITY));
                            ui.label("URL:");
                            ui.add(
                                egui::TextEdit::singleline(url)
                                    .hint_text("https://github.com/user/repo.git  or  git@github.com:user/repo.git")
                                    .desired_width(f32::INFINITY),
                            )
                            .request_focus();
                            ui.checkbox(fetch, "Fetch from it after adding");
                            if !signed_in {
                                ui.label(
                                    RichText::new("Connect GitHub (Sign in, top right) to create the repository from here.")
                                        .small()
                                        .weak(),
                                );
                            }
                            ok_label = "Add remote";
                            ok_enabled = valid_remote_name(name, None) && !url.trim().is_empty();
                        } else {
                            let busy = form.busy.is_some();
                            ui.add_enabled_ui(!busy, |ui| {
                                egui::Grid::new("publish_grid")
                                    .num_columns(2)
                                    .spacing([10.0, 8.0])
                                    .show(ui, |ui| {
                                        ui.label("Owner:");
                                        egui::ComboBox::from_id_salt("publish_owner")
                                            .selected_text(form.owner.as_str())
                                            .width(260.0)
                                            .show_ui(ui, |ui| {
                                                for o in &owners {
                                                    ui.selectable_value(&mut form.owner, o.clone(), o);
                                                }
                                            });
                                        ui.end_row();
                                        ui.label("Repository:");
                                        let r = ui.add(
                                            egui::TextEdit::singleline(&mut form.repo).desired_width(260.0),
                                        );
                                        if r.lost_focus() {
                                            form.repo = github::sanitize_repo_name(&form.repo);
                                        }
                                        ui.end_row();
                                        ui.label("Description:");
                                        ui.add(
                                            egui::TextEdit::singleline(&mut form.description)
                                                .hint_text("optional")
                                                .desired_width(260.0),
                                        );
                                        ui.end_row();
                                        ui.label("Remote name:");
                                        ui.add(egui::TextEdit::singleline(name).desired_width(260.0));
                                        ui.end_row();
                                    });
                                ui.add_space(4.0);
                                ui.checkbox(&mut form.private, "Private repository");
                                match &form.branch {
                                    Some(b) => {
                                        ui.checkbox(&mut form.push, format!("Push {b} after creating"));
                                    }
                                    None => {
                                        ui.label(
                                            RichText::new("Nothing to push yet — make a first commit, then Push.")
                                                .small()
                                                .weak(),
                                        );
                                    }
                                }
                            });
                            if busy {
                                ui.horizontal(|ui| {
                                    ui.add(egui::Spinner::new().size(14.0));
                                    ui.label(RichText::new("Creating the repository on GitHub…").weak());
                                });
                            }
                            if let Some(e) = &form.error {
                                ui.label(RichText::new(e.as_str()).color(theme::pal(ui).red));
                                ui.label(
                                    RichText::new(
                                        "Creating repositories needs a classic token with the repo scope, \
                                         or a fine-grained token with Administration: Read and write.",
                                    )
                                    .small()
                                    .weak(),
                                );
                            }
                            ok_label = "Create & add remote";
                            ok_enabled = !busy
                                && valid_remote_name(name, None)
                                && !github::sanitize_repo_name(&form.repo).is_empty()
                                && !form.owner.is_empty();
                        }
                    }
                    Dialog::DiscardAgent { branch, .. } => {
                        ui.label(format!(
                            "Delete the agent's copy of the repository and its branch {branch}?\nAnything not merged is lost. Your own folder is not touched."
                        ));
                        ok_label = "Discard";
                    }
                    Dialog::EditRemote {
                        original,
                        name,
                        url,
                    } => {
                        ui.label("Remote name:");
                        ui.add(egui::TextEdit::singleline(name).desired_width(f32::INFINITY));
                        ui.label("URL:");
                        ui.add(egui::TextEdit::singleline(url).desired_width(f32::INFINITY))
                            .request_focus();
                        ok_label = "Save";
                        ok_enabled =
                            valid_remote_name(name, Some(original.as_str())) && !url.trim().is_empty();
                    }
                }

                ui.add_space(12.0);
                let danger = matches!(
                    &dialog,
                    Dialog::Confirm { danger: true, .. } | Dialog::DiscardAgent { .. }
                );
                ui.horizontal(|ui| {
                    let p = theme::pal(ui);
                    let ok_btn =
                        egui::Button::new(RichText::new(ok_label).strong().color(p.on_accent))
                            .fill(if danger { p.red } else { p.accent })
                            .stroke(egui::Stroke::NONE)
                            .min_size(egui::vec2(0.0, 28.0));
                    if ui.add_enabled(ok_enabled, ok_btn).clicked() {
                        match &dialog {
                            Dialog::NewBranch {
                                name,
                                start,
                                checkout,
                                ..
                            } => {
                                run = Some(Op::CreateBranch {
                                    name: name.trim().into(),
                                    start: start.clone(),
                                    checkout: *checkout,
                                })
                            }
                            Dialog::NewTag { name, commit } => {
                                run = Some(Op::CreateTag {
                                    name: name.trim().into(),
                                    commit: commit.clone(),
                                })
                            }
                            Dialog::Push {
                                remote,
                                branch,
                                set_upstream,
                                force,
                            } => {
                                run = Some(Op::Push {
                                    set_upstream: if *set_upstream {
                                        Some((remote.trim().into(), branch.trim().into()))
                                    } else {
                                        None
                                    },
                                    force: *force,
                                })
                            }
                            Dialog::Pull { rebase } => run = Some(Op::Pull { rebase: *rebase }),
                            Dialog::Stash { message } => run = Some(Op::StashSave(message.clone())),
                            Dialog::Clone { url, parent, name } => {
                                clone = Some((
                                    url.trim().into(),
                                    PathBuf::from(parent.trim()),
                                    name.trim().into(),
                                ))
                            }
                            Dialog::Confirm { op, .. } => run = Some(op.clone()),
                            Dialog::AddRemote {
                                name,
                                url,
                                fetch,
                                github,
                                ..
                            } => {
                                if *github {
                                    publish = true;
                                } else {
                                    run = Some(Op::RemoteAdd {
                                        name: name.trim().into(),
                                        url: url.trim().into(),
                                        fetch: *fetch,
                                    })
                                }
                            }
                            Dialog::EditRemote {
                                original,
                                name,
                                url,
                            } => {
                                run = Some(Op::RemoteEdit {
                                    name: original.clone(),
                                    new_name: Some(name.trim().to_string())
                                        .filter(|n| n != original),
                                    url: Some(url.trim().to_string()),
                                })
                            }
                            Dialog::DiscardAgent {
                                task,
                                repo,
                                worktree,
                                branch,
                            } => {
                                discard_agent = Some((*task, repo.clone(), worktree.clone(), branch.clone()))
                            }
                        }
                    }
                    if ui
                        .add(egui::Button::new("Cancel").min_size(egui::vec2(0.0, 28.0)))
                        .clicked()
                        || esc
                    {
                        keep = false;
                    }
                });
            });

        if let Some(op) = run {
            if let Some(tab) = self.tabs.get(self.active) {
                let (id, dir) = (tab.id, tab.path.clone());
                self.enqueue(Some(id), dir, JobKind::Op(op));
            }
            keep = false;
        }
        if let Some((task, repo, worktree, branch)) = discard_agent {
            // a tab showing the worktree would point at a deleted folder
            if let Some(i) = self.tabs.iter().position(|t| t.path == worktree) {
                self.close_tab(i);
            }
            if worktree.exists() {
                let tab_id = self.tabs.iter().find(|t| t.path == repo).map(|t| t.id);
                self.enqueue_then(
                    tab_id,
                    repo,
                    Op::AgentCleanup {
                        worktree: worktree.display().to_string(),
                        branch,
                        delete_branch: true,
                    },
                    AfterJob::AgentDiscarded(task),
                );
            } else {
                self.agents.remove(task);
            }
            keep = false;
        }
        if publish {
            if let (Dialog::AddRemote { name, form, .. }, Some(tab)) =
                (&mut dialog, self.tabs.get(self.active))
            {
                let tag = self.next_tag;
                self.next_tag += 1;
                form.busy = Some(tag);
                form.error = None;
                self.publish = Some(PendingPublish {
                    tag,
                    tab_id: tab.id,
                    dir: tab.path.clone(),
                    remote: name.trim().to_string(),
                    push_branch: form.branch.clone().filter(|_| form.push),
                });
                let repo = github::sanitize_repo_name(&form.repo);
                self.github.create_repo(
                    tag,
                    &form.owner.clone(),
                    repo,
                    form.description.trim().to_string(),
                    form.private,
                );
            }
        }
        if let Some((url, parent, name)) = clone {
            self.clone_parent = parent.display().to_string();
            self.enqueue(None, parent.clone(), JobKind::Clone { url, parent, name });
            keep = false;
        }
        if keep {
            self.dialog = Some(dialog);
        }
    }

    fn toast_ui(&mut self, ctx: &egui::Context) {
        let Some((text, ok, at)) = &self.toast else {
            return;
        };
        let ttl = if *ok { 3.0 } else { 8.0 };
        if at.elapsed().as_secs_f32() > ttl {
            self.toast = None;
            return;
        }
        let (text, ok) = (text.clone(), *ok);
        let mut dismiss = false;
        let p = theme::palette(self.dark);
        egui::Area::new(egui::Id::unique("toast"))
            .anchor(egui::Align2::RIGHT_TOP, [-16.0, 88.0])
            .show(ctx, |ui| {
                let color = if ok { p.green } else { p.red };
                let r = egui::Frame::new()
                    .fill(p.raised)
                    .stroke(egui::Stroke::new(1.0, theme::alpha(color, 0.45)))
                    .corner_radius(10.0)
                    .shadow(ui.visuals().popup_shadow)
                    .inner_margin(egui::Margin {
                        left: 14,
                        right: 16,
                        top: 10,
                        bottom: 10,
                    })
                    .show(ui, |ui| {
                        ui.set_max_width(440.0);
                        ui.horizontal_top(|ui| {
                            ui.label(
                                RichText::new(if ok { "✔" } else { "⚠" })
                                    .size(15.0)
                                    .color(color),
                            );
                            ui.vertical(|ui| {
                                ui.add(
                                    egui::Label::new(RichText::new(&text).color(p.text_strong))
                                        .wrap(),
                                );
                                if !ok {
                                    ui.label(
                                        RichText::new("Click to dismiss · details in Log")
                                            .small()
                                            .color(p.faint),
                                    );
                                }
                            });
                        });
                    });
                if r.response.interact(egui::Sense::click()).clicked() {
                    dismiss = true;
                }
            });
        if dismiss {
            self.toast = None;
        }
        ctx.request_repaint_after(Duration::from_millis(250));
    }

    fn shortcuts(&mut self, ctx: &egui::Context, actions: &mut Vec<Action>) {
        use egui::Key;
        // consume_key runs before any widget sees the keys, so text fields and
        // focus navigation (Tab) don't swallow the tab shortcuts.
        let (f5, open, new_tab, close, next, prev, num) = ctx.input_mut(|i| {
            use egui::Modifiers as M;
            let cmd = M::COMMAND;
            let ctrl_shift = M::CTRL | M::SHIFT;
            let num = [
                Key::Num1,
                Key::Num2,
                Key::Num3,
                Key::Num4,
                Key::Num5,
                Key::Num6,
                Key::Num7,
                Key::Num8,
                Key::Num9,
            ]
            .iter()
            .position(|k| i.consume_key(cmd, *k));
            let prev = i.consume_key(ctrl_shift, Key::Tab) || i.consume_key(cmd, Key::PageUp);
            let next = i.consume_key(M::CTRL, Key::Tab) || i.consume_key(cmd, Key::PageDown);
            (
                i.key_pressed(Key::F5),
                i.consume_key(cmd, Key::O),
                i.consume_key(cmd, Key::T),
                i.consume_key(cmd, Key::W),
                next,
                prev,
                num,
            )
        });
        if f5 {
            actions.push(Action::Refresh);
        }
        if open {
            self.pick_folder();
        }
        if new_tab {
            self.open_launcher();
        }
        if close {
            if self.show_launcher && !self.tabs.is_empty() {
                self.show_launcher = false;
            } else if !self.show_launcher {
                self.close_tab(self.active);
            }
        }
        let n = self.tabs.len();
        if n > 0 {
            if next {
                self.select_tab(if self.show_launcher {
                    0
                } else {
                    (self.active + 1) % n
                });
            }
            if prev {
                self.select_tab(if self.show_launcher {
                    n - 1
                } else {
                    (self.active + n - 1) % n
                });
            }
            if let Some(k) = num {
                // Ctrl+9 = last tab, like browsers
                self.select_tab(if k == 8 { n - 1 } else { k });
            }
        }
        // Auto-refresh every open project when the window regains focus (like Sourcetree).
        let focused = ctx.input(|i| i.viewport().focused.unwrap_or(true));
        if focused && !self.was_focused {
            for t in &mut self.tabs {
                t.reload_fast();
                t.start_history_load(Some(ctx));
            }
            self.launcher.branches.clear();
        }
        self.was_focused = focused;
    }
}

impl eframe::App for FoxGitApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.pump_jobs(ctx);
        self.poll_scan();
        self.poll_ask();
        self.poll_fetch(ctx);
        for (text, ok) in self.github.poll() {
            self.notify(text, ok);
        }
        self.poll_checks();
        self.poll_publish();
        for (text, ok) in self.agents.poll() {
            self.notify(text, ok);
        }
        if self.agents.any_running() {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
        if self.auto_fetch_min > 0 {
            let every = Duration::from_secs(self.auto_fetch_min as u64 * 60);
            let since = self.last_auto_fetch.elapsed();
            if since >= every {
                self.start_fetch_all(ctx, false);
            } else {
                ctx.request_repaint_after(every - since);
            }
        }
        for tab in &mut self.tabs {
            tab.poll_history();
        }
        if self.running.is_some() || !self.queue.is_empty() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        if self.tabs.iter().any(|t| t.is_loading()) {
            ctx.request_repaint_after(Duration::from_millis(50));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let mut actions = Vec::new();
        self.shortcuts(&ctx, &mut actions);

        // Browser-style tab strip on top; the active tab blends into the toolbar below.
        let p = theme::palette(self.dark);
        egui::Panel::top("tabstrip")
            .show_separator_line(false)
            .frame(egui::Frame::new().fill(p.crust).inner_margin(egui::Margin {
                left: 8,
                right: 8,
                top: 6,
                bottom: 0,
            }))
            .show(ui, |ui| self.tab_strip_ui(ui, &ctx));

        egui::Panel::top("toolbar")
            .frame(theme::panel(p.mantle, 8, 5))
            .show(ui, |ui| self.toolbar(ui, &mut actions));

        self.git_missing_banner(ui);

        egui::Panel::bottom("statusbar")
            .frame(theme::panel(p.crust, 12, 4))
            .show(ui, |ui| self.status_bar(ui));

        // connected GitHub logins, for the "GitHub account" menu on remotes in the sidebar
        let logins = self.github.logins();
        ctx.data_mut(|d| d.insert_temp(views::github_logins_id(), logins));
        if self.show_launcher || self.tabs.is_empty() {
            egui::CentralPanel::default().show(ui, |ui| self.launcher_ui(ui));
        } else {
            let active = self.active;
            let agent_rows = self.agents.rows_for(&self.tabs[active].path);
            let tab = &mut self.tabs[active];
            // three zones: mode rail · mode sidebar · work area
            views::activity_rail(ui, tab, &agent_rows);
            views::sidebar(ui, tab, &agent_rows, &mut actions);
            match tab.view {
                View::History => views::history_view(ui, tab, &mut actions),
                View::FileStatus => views::file_status_view(ui, tab, &mut actions),
                View::Agents => match tab.agent_chat.filter(|id| self.agents.has(*id)) {
                    Some(id) => {
                        let acts = self.agents.task_view(ui, id);
                        self.apply_agent_actions(acts);
                    }
                    None => {
                        if self.agents.empty_view(ui) {
                            actions.push(Action::NewAgentTask);
                        }
                    }
                },
            }
        }
        // a chat started from the New task dialog: show it in its repository's tab
        if let Some((repo, id)) = self.agents.opened.take() {
            if let Some(i) = self.tabs.iter().position(|t| t.path == repo) {
                self.tabs[i].view = View::Agents;
                self.tabs[i].agent_chat = Some(id);
                self.select_tab(i);
            }
        }

        self.apply_actions(actions, &ctx);
        self.dialogs(&ctx);
        if self.show_log {
            self.log_window(&ctx);
        }
        self.about_window(&ctx);
        self.agents.dialog_ui(&ctx);
        self.github.dialog_ui(&ctx);
        self.credential_dialog(&ctx);
        self.toast_ui(&ctx);
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        let saved = Saved {
            open_tabs: self
                .tabs
                .iter()
                .map(|t| t.path.display().to_string())
                .collect(),
            active: self.active,
            recent: Vec::new(),
            projects: self.projects.clone(),
            dark: Some(self.dark),
            auto_fetch_min: Some(self.auto_fetch_min),
            github_login: self.github.saved_active(),
            github_accounts: self.github.saved_logins(),
            clone_parent: Some(self.clone_parent.clone()),
            last_update_check: self.last_update_check,
            agent_tasks: self.agents.saved(),
            last_agent: Some(self.agents.last_kind),
        };
        eframe::set_value(storage, STORAGE_KEY, &saved);
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Open THIRD-PARTY-LICENSES.txt shipped next to the program (or the copy in the source tree).
fn open_licenses(ctx: &egui::Context) {
    const NAME: &str = "THIRD-PARTY-LICENSES.txt";
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(Path::to_path_buf));
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(d) = &exe_dir {
        candidates.push(d.join("../Resources").join(NAME)); // macOS .app
        candidates.push(d.join(NAME)); // Windows / Linux archives
    }
    candidates.push(PathBuf::from("/usr/share/doc/foxgit").join(NAME)); // .deb
    candidates.push(Path::new(env!("CARGO_MANIFEST_DIR")).join(NAME)); // dev build
    match candidates.into_iter().find(|p| p.exists()) {
        Some(p) => tabs::reveal_in_file_manager(&p),
        None => {
            if let Some(url) = update::repo_url() {
                ctx.open_url(egui::OpenUrl::new_tab(format!("{url}/blob/main/{NAME}")));
            }
        }
    }
}

/// Thin vertical divider between toolbar groups.
fn toolbar_divider(ui: &mut egui::Ui) {
    let p = theme::pal(ui);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(13.0, 30.0), egui::Sense::hover());
    ui.painter().line_segment(
        [
            egui::pos2(rect.center().x, rect.top() + 7.0),
            egui::pos2(rect.center().x, rect.bottom() - 7.0),
        ],
        egui::Stroke::new(1.0, p.border_strong),
    );
}

pub fn push_dialog_for(tab: &RepoTab) -> Dialog {
    let head = tab.head_branch();
    let branch = head.map(|b| b.name.clone()).unwrap_or_default();
    let (remote, set_upstream) = match head.and_then(|b| b.upstream.clone()) {
        Some(up) => (up.split('/').next().unwrap_or("origin").to_string(), false),
        None => (
            tab.refs
                .remote_names
                .first()
                .cloned()
                .unwrap_or_else(|| "origin".into()),
            true,
        ),
    };
    Dialog::Push {
        remote,
        branch,
        set_upstream,
        force: false,
    }
}

fn looks_like_auth_failure(text: &str) -> bool {
    let t = text.to_lowercase();
    [
        "authentication failed",
        "could not read username",
        "could not read password",
        "permission denied",
        "invalid username or password",
        "403",
        "401",
    ]
    .iter()
    .any(|k| t.contains(k))
}

fn dirs_home() -> String {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_default()
}

fn repo_name_from_url(url: &str) -> String {
    url.trim()
        .trim_end_matches('/')
        .rsplit(['/', ':'])
        .next()
        .unwrap_or("")
        .trim_end_matches(".git")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::repo_name_from_url;

    #[test]
    fn repo_names() {
        assert_eq!(repo_name_from_url("https://github.com/a/b.git"), "b");
        assert_eq!(
            repo_name_from_url("git@github.com:a/my-repo.git"),
            "my-repo"
        );
        assert_eq!(repo_name_from_url("https://x.org/a/c/"), "c");
    }
}
