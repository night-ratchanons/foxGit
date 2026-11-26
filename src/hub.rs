//! GitHub inside FoxGit: pull requests (list, conversation, files, checks, review, merge, close,
//! checkout, create) and issues (list, conversation, comment, close, hand to an AI agent), plus
//! the CI checks of the commit selected in History.

use crate::github::{self, Check, CheckState, Comment, Issue, Pull, PullFile, Review};
use crate::theme::{self, pal};
use eframe::egui;
use egui::{pos2, vec2, Color32, FontId, Rect, RichText, Sense, Stroke};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

const LIST_TTL: Duration = Duration::from_secs(90);
const MAX_PATCH_LINES: usize = 1500;

/// What the GitHub view needs to know about the repository of the active tab.
pub struct RepoInfo {
    pub path: PathBuf,
    /// `owner/name` of the GitHub remote, if any.
    pub slug: Option<String>,
    pub remote: Option<String>,
    pub token: Option<String>,
    pub branch: Option<String>,
    pub has_upstream: bool,
    pub ahead: usize,
    /// Remote branches (for a new pull request's base).
    pub remote_branches: Vec<String>,
}

#[derive(Clone)]
pub struct NewPr {
    pub repo: PathBuf,
    pub slug: String,
    pub token: String,
    pub title: String,
    pub body: String,
    pub head: String,
    pub base: String,
    pub draft: bool,
}

pub enum HubAction {
    CheckoutPr {
        number: u64,
        remote: String,
    },
    StartAgent {
        prompt: String,
    },
    /// Push the branch first (committing an agent's worktree if `commit` is set), then create the PR.
    PushThenCreate {
        remote: String,
        branch: String,
        commit: Option<(PathBuf, String)>,
        pr: Box<NewPr>,
    },
    Notify(String, bool),
    SignIn,
    AddRemote,
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum ListKind {
    #[default]
    Pulls,
    Issues,
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum DetailTab {
    #[default]
    Conversation,
    Files,
    Checks,
}

struct Loadable<T> {
    value: Option<T>,
    loading: bool,
    error: Option<String>,
    at: Option<Instant>,
}

impl<T> Default for Loadable<T> {
    fn default() -> Self {
        Self {
            value: None,
            loading: false,
            error: None,
            at: None,
        }
    }
}

impl<T> Loadable<T> {
    fn stale(&self) -> bool {
        !self.loading && self.error.is_none() && self.at.is_none_or(|t| t.elapsed() > LIST_TTL)
    }
}

#[derive(Default)]
struct Detail {
    pull: Option<Pull>,
    issue: Option<Issue>,
    files: Option<Vec<PullFile>>,
    comments: Vec<Comment>,
    reviews: Vec<Review>,
    checks: Option<Vec<Check>>,
    loading: bool,
    error: Option<String>,
    loaded: bool,
}

#[derive(Default)]
struct RepoHub {
    kind: ListKind,
    closed: bool,
    pulls: Loadable<Vec<Pull>>,
    issues: Loadable<Vec<Issue>>,
    selected: Option<u64>,
    details: HashMap<u64, Detail>,
    tab: DetailTab,
    comment: String,
    busy: Option<String>,
    filter: String,
    open_files: HashMap<String, bool>,
    /// GitHub's default branch (asked once).
    default_branch: Option<String>,
    asked_default: bool,
}

struct DetailData {
    pull: Option<Pull>,
    issue: Option<Issue>,
    files: Option<Vec<PullFile>>,
    comments: Vec<Comment>,
    reviews: Vec<Review>,
    checks: Option<Vec<Check>>,
}

enum Msg {
    Pulls {
        repo: PathBuf,
        closed: bool,
        result: Result<Vec<Pull>, String>,
    },
    Issues {
        repo: PathBuf,
        closed: bool,
        result: Result<Vec<Issue>, String>,
    },
    Detail {
        repo: PathBuf,
        n: u64,
        result: Result<Box<DetailData>, String>,
    },
    Done {
        repo: PathBuf,
        n: Option<u64>,
        result: Result<String, String>,
    },
    Checks {
        sha: String,
        result: Result<Vec<Check>, String>,
    },
    Created {
        repo: PathBuf,
        result: Result<Box<Pull>, String>,
    },
    DefaultBranch {
        repo: PathBuf,
        name: String,
    },
}

struct CreateForm {
    repo: PathBuf,
    slug: String,
    token: String,
    remote: String,
    head: String,
    needs_push: bool,
    commit: Option<(PathBuf, String)>,
    title: String,
    body: String,
    base: String,
    bases: Vec<String>,
    draft: bool,
    focus: bool,
    /// The base was guessed (not picked); GitHub's default branch replaces it when known.
    base_auto: bool,
}

pub struct Hub {
    repos: HashMap<PathBuf, RepoHub>,
    commit_checks: HashMap<String, Loadable<Vec<Check>>>,
    create: Option<CreateForm>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    ctx: egui::Context,
    notes: Vec<(String, bool)>,
    created: Option<PathBuf>,
}

fn spawn(tx: &Sender<Msg>, ctx: &egui::Context, job: impl FnOnce() -> Msg + Send + 'static) {
    let (tx, ctx) = (tx.clone(), ctx.clone());
    std::thread::spawn(move || {
        let _ = tx.send(job());
        ctx.request_repaint();
    });
}

fn err(e: github::ApiError) -> String {
    e.to_string()
}

impl Hub {
    pub fn new(ctx: &egui::Context) -> Self {
        let (tx, rx) = channel();
        Self {
            repos: HashMap::new(),
            commit_checks: HashMap::new(),
            create: None,
            tx,
            rx,
            ctx: ctx.clone(),
            notes: Vec::new(),
            created: None,
        }
    }

    // ------------------------------------------------------------------ background results

    pub fn poll(&mut self) -> Vec<(String, bool)> {
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::Pulls {
                    repo,
                    closed,
                    result,
                } => {
                    let rh = self.repos.entry(repo).or_default();
                    if rh.closed == closed {
                        set(&mut rh.pulls, result);
                    }
                }
                Msg::Issues {
                    repo,
                    closed,
                    result,
                } => {
                    let rh = self.repos.entry(repo).or_default();
                    if rh.closed == closed {
                        set(&mut rh.issues, result);
                    }
                }
                Msg::Detail { repo, n, result } => {
                    let d = self
                        .repos
                        .entry(repo)
                        .or_default()
                        .details
                        .entry(n)
                        .or_default();
                    d.loading = false;
                    d.loaded = true;
                    match result {
                        Ok(x) => {
                            d.pull = x.pull;
                            d.issue = x.issue;
                            d.files = x.files;
                            d.comments = x.comments;
                            d.reviews = x.reviews;
                            d.checks = x.checks;
                            d.error = None;
                        }
                        Err(e) => d.error = Some(e),
                    }
                }
                Msg::Done { repo, n, result } => {
                    let rh = self.repos.entry(repo).or_default();
                    rh.busy = None;
                    match result {
                        Ok(text) => {
                            rh.comment.clear();
                            rh.pulls.at = None;
                            rh.issues.at = None;
                            if let Some(n) = n {
                                rh.details.remove(&n);
                            }
                            self.notes.push((text, true));
                        }
                        Err(e) => self.notes.push((e, false)),
                    }
                }
                Msg::DefaultBranch { repo, name } => {
                    if let Some(f) = self
                        .create
                        .as_mut()
                        .filter(|f| f.base_auto && f.repo == repo)
                    {
                        if f.head != name {
                            f.base = name.clone();
                        }
                    }
                    self.repos.entry(repo).or_default().default_branch = Some(name);
                }
                Msg::Checks { sha, result } => {
                    set(self.commit_checks.entry(sha).or_default(), result);
                }
                Msg::Created { repo, result } => match result {
                    Ok(pr) => {
                        let rh = self.repos.entry(repo.clone()).or_default();
                        rh.kind = ListKind::Pulls;
                        rh.closed = false;
                        rh.pulls.at = None;
                        rh.selected = Some(pr.number);
                        rh.tab = DetailTab::Conversation;
                        self.created = Some(repo);
                        self.notes.push((
                            format!("Created pull request #{} — {}", pr.number, pr.title),
                            true,
                        ));
                    }
                    Err(e) => {
                        if let Some(f) = &mut self.create {
                            f.focus = false;
                        }
                        self.notes
                            .push((format!("Could not create the pull request: {e}"), false));
                    }
                },
            }
        }
        std::mem::take(&mut self.notes)
    }

    /// CI checks of a commit: `Some(None)` while loading, `None` when GitHub doesn't know the
    /// commit (not pushed) or the call failed. Running checks are re-polled every 20 s.
    pub fn commit_checks(
        &mut self,
        slug: &str,
        token: &str,
        sha: &str,
    ) -> Option<Option<Vec<Check>>> {
        let l = self.commit_checks.entry(sha.to_string()).or_default();
        if l.error.is_some() {
            return None;
        }
        let pending = l
            .value
            .as_ref()
            .is_some_and(|v| v.iter().any(|c| c.state == CheckState::Pending));
        let due = l.value.is_none()
            || (pending && l.at.is_some_and(|t| t.elapsed() > Duration::from_secs(20)));
        if due && !l.loading {
            l.loading = true;
            let (slug, token, sha2) = (slug.to_string(), token.to_string(), sha.to_string());
            spawn(&self.tx, &self.ctx, move || Msg::Checks {
                result: github::checks(&token, &slug, &sha2).map_err(err),
                sha: sha2,
            });
        }
        if pending {
            self.ctx.request_repaint_after(Duration::from_secs(5));
        }
        Some(l.value.clone())
    }

    /// The repository whose pull request was just created (to show it).
    pub fn take_created(&mut self) -> Option<PathBuf> {
        self.created.take()
    }

    fn load_list(&mut self, info: &RepoInfo) {
        let (Some(slug), Some(token)) = (&info.slug, &info.token) else {
            return;
        };
        let rh = self.repos.entry(info.path.clone()).or_default();
        if !rh.asked_default {
            rh.asked_default = true;
            let (slug, token, repo) = (slug.clone(), token.clone(), info.path.clone());
            let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
            std::thread::spawn(move || {
                if let Ok(name) = github::default_branch(&token, &slug) {
                    let _ = tx.send(Msg::DefaultBranch { repo, name });
                    ctx.request_repaint();
                }
            });
        }
        let closed = rh.closed;
        let state = if closed { "closed" } else { "open" };
        let repo = info.path.clone();
        match rh.kind {
            ListKind::Pulls if rh.pulls.stale() => {
                rh.pulls.loading = true;
                let (slug, token) = (slug.clone(), token.clone());
                spawn(&self.tx, &self.ctx, move || Msg::Pulls {
                    result: github::list_pulls(&token, &slug, state).map_err(err),
                    repo,
                    closed,
                });
            }
            ListKind::Issues if rh.issues.stale() => {
                rh.issues.loading = true;
                let (slug, token) = (slug.clone(), token.clone());
                spawn(&self.tx, &self.ctx, move || Msg::Issues {
                    result: github::list_issues(&token, &slug, state).map_err(err),
                    repo,
                    closed,
                });
            }
            _ => {}
        }
    }

    fn load_detail(&mut self, info: &RepoInfo, n: u64) {
        let (Some(slug), Some(token)) = (&info.slug, &info.token) else {
            return;
        };
        let rh = self.repos.entry(info.path.clone()).or_default();
        let is_pr = rh.kind == ListKind::Pulls;
        let d = rh.details.entry(n).or_default();
        if d.loading || d.loaded {
            return;
        }
        d.loading = true;
        let (slug, token, repo) = (slug.clone(), token.clone(), info.path.clone());
        spawn(&self.tx, &self.ctx, move || {
            let result = (|| -> Result<DetailData, github::ApiError> {
                if is_pr {
                    let pull = github::get_pull(&token, &slug, n)?;
                    let files = github::pull_files(&token, &slug, n).ok();
                    let comments = github::issue_comments(&token, &slug, n).unwrap_or_default();
                    let reviews = github::pull_reviews(&token, &slug, n).unwrap_or_default();
                    let checks = github::checks(&token, &slug, &pull.head.sha).ok();
                    Ok(DetailData {
                        pull: Some(pull),
                        issue: None,
                        files,
                        comments,
                        reviews,
                        checks,
                    })
                } else {
                    let issue = github::get_issue(&token, &slug, n)?;
                    let comments = github::issue_comments(&token, &slug, n).unwrap_or_default();
                    Ok(DetailData {
                        pull: None,
                        issue: Some(issue),
                        files: None,
                        comments,
                        reviews: Vec::new(),
                        checks: None,
                    })
                }
            })();
            Msg::Detail {
                repo,
                n,
                result: result.map(Box::new).map_err(err),
            }
        });
    }

    /// Run a write call (merge, review, comment, close …) in the background.
    fn act(
        &mut self,
        info: &RepoInfo,
        n: u64,
        label: &str,
        done: String,
        f: impl FnOnce(&str, &str) -> Result<(), github::ApiError> + Send + 'static,
    ) {
        let (Some(slug), Some(token)) = (info.slug.clone(), info.token.clone()) else {
            return;
        };
        let rh = self.repos.entry(info.path.clone()).or_default();
        rh.busy = Some(label.to_string());
        let repo = info.path.clone();
        spawn(&self.tx, &self.ctx, move || Msg::Done {
            repo,
            n: Some(n),
            result: f(&token, &slug).map(|_| done).map_err(err),
        });
    }

    /// Create the pull request right away (the branch is already on the remote).
    pub fn create_now(&mut self, pr: NewPr) {
        self.create = None;
        let repo = pr.repo.clone();
        spawn(&self.tx, &self.ctx, move || Msg::Created {
            repo,
            result: github::create_pull(
                &pr.token, &pr.slug, &pr.title, &pr.body, &pr.head, &pr.base, pr.draft,
            )
            .map(Box::new)
            .map_err(err),
        });
    }

    // ------------------------------------------------------------------ new pull request

    /// Open the "New pull request" form for `head` (e.g. the current branch or an agent's branch).
    #[allow(clippy::too_many_arguments)]
    pub fn open_create(
        &mut self,
        info: &RepoInfo,
        head: &str,
        needs_push: bool,
        commit: Option<(PathBuf, String)>,
        title: &str,
        body: &str,
        base: Option<&str>,
    ) -> Result<(), String> {
        let slug = info
            .slug
            .clone()
            .ok_or("This repository has no GitHub remote")?;
        let token = info
            .token
            .clone()
            .ok_or("Connect a GitHub account first (Sign in, top right)")?;
        let remote = info.remote.clone().ok_or("This repository has no remote")?;
        let mut bases: Vec<String> = info
            .remote_branches
            .iter()
            .filter_map(|b| b.strip_prefix(&format!("{remote}/")).map(String::from))
            .filter(|b| b != "HEAD" && b != head)
            .collect();
        bases.sort();
        bases.dedup();
        let known = self
            .repos
            .get(&info.path)
            .and_then(|r| r.default_branch.clone())
            .filter(|b| b != head);
        let base_auto = base.is_none() && known.is_none();
        let base = base
            .map(String::from)
            .or(known)
            .or_else(|| {
                ["main", "master", "develop"]
                    .iter()
                    .find(|b| bases.iter().any(|x| x == *b))
                    .map(|b| b.to_string())
            })
            .or_else(|| bases.first().cloned())
            .unwrap_or_else(|| "main".into());
        self.create = Some(CreateForm {
            repo: info.path.clone(),
            slug,
            token,
            remote,
            head: head.to_string(),
            needs_push,
            commit,
            title: title.to_string(),
            body: body.to_string(),
            base,
            bases,
            draft: false,
            focus: true,
            base_auto,
        });
        if base_auto {
            // ask for the real default branch; `poll` swaps it in
            let rh = self.repos.entry(info.path.clone()).or_default();
            rh.asked_default = false;
            self.load_list(info);
        }
        Ok(())
    }

    /// "New pull request" for the current branch (pushing it first when needed).
    pub fn open_create_for_branch(&mut self, info: &RepoInfo) -> Result<(), String> {
        let b = info.branch.clone().ok_or("Check out a branch first")?;
        let needs_push = !info.has_upstream || info.ahead > 0;
        self.open_create(info, &b, needs_push, None, &title_from_branch(&b), "", None)
    }

    pub fn create_dialog(&mut self, ctx: &egui::Context) -> Vec<HubAction> {
        let mut out = Vec::new();
        let Some(f) = &mut self.create else {
            return out;
        };
        let mut close = false;
        let mut submit = false;
        egui::Modal::new(egui::Id::unique("hub_new_pr")).show(ctx, |ui| {
            ui.set_width(560.0);
            let p = pal(ui);
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(egui::special_emojis::GITHUB.to_string())
                        .size(22.0)
                        .color(p.text_strong),
                );
                ui.label(
                    RichText::new("New pull request")
                        .size(18.0)
                        .strong()
                        .color(p.text_strong),
                );
            });
            ui.label(
                RichText::new(format!("{}  ·  {}  →  {}", f.slug, f.head, f.base)).color(p.subtext),
            );
            ui.add_space(10.0);
            egui::Grid::new("new_pr_grid")
                .num_columns(2)
                .spacing([10.0, 8.0])
                .show(ui, |ui| {
                    ui.label("Into:");
                    egui::ComboBox::from_id_salt("new_pr_base")
                        .selected_text(f.base.as_str())
                        .width(260.0)
                        .show_ui(ui, |ui| {
                            for b in &f.bases {
                                if ui.selectable_value(&mut f.base, b.clone(), b).clicked() {
                                    f.base_auto = false;
                                }
                            }
                        });
                    ui.end_row();
                    ui.label("Title:");
                    let r = ui.add(egui::TextEdit::singleline(&mut f.title).desired_width(420.0));
                    if f.focus {
                        r.request_focus();
                        f.focus = false;
                    }
                    ui.end_row();
                });
            ui.add_space(6.0);
            ui.label("Description:");
            ui.add(
                egui::TextEdit::multiline(&mut f.body)
                    .hint_text("What changed and why (Markdown)")
                    .desired_rows(8)
                    .desired_width(f32::INFINITY)
                    .margin(vec2(10.0, 8.0)),
            );
            ui.checkbox(&mut f.draft, "Create as draft");
            if f.needs_push {
                ui.label(
                    RichText::new(format!(
                        "{} will be {}pushed to {} first.",
                        f.head,
                        if f.commit.is_some() {
                            "committed and "
                        } else {
                            ""
                        },
                        f.remote
                    ))
                    .small()
                    .color(p.faint),
                );
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                let ok = !f.title.trim().is_empty() && !f.base.is_empty();
                let label = if f.needs_push {
                    "Push & create pull request"
                } else {
                    "Create pull request"
                };
                if theme::primary_button(ui, label, ok).clicked() {
                    submit = true;
                }
                if ui
                    .add(egui::Button::new("Cancel").min_size(vec2(0.0, 28.0)))
                    .clicked()
                    || ui.input(|i| i.key_pressed(egui::Key::Escape))
                {
                    close = true;
                }
            });
        });
        if submit {
            let f = self.create.take().unwrap();
            let pr = NewPr {
                repo: f.repo,
                slug: f.slug,
                token: f.token,
                title: f.title.trim().to_string(),
                body: f.body,
                head: f.head.clone(),
                base: f.base,
                draft: f.draft,
            };
            if f.needs_push {
                out.push(HubAction::PushThenCreate {
                    remote: f.remote,
                    branch: f.head,
                    commit: f.commit,
                    pr: Box::new(pr),
                });
            } else {
                self.create_now(pr);
            }
        } else if close {
            self.create = None;
        }
        out
    }

    // ------------------------------------------------------------------ sidebar

    pub fn sidebar(&mut self, ui: &mut egui::Ui, info: &RepoInfo) -> Vec<HubAction> {
        let mut out = Vec::new();
        let p = pal(ui);
        if info.slug.is_some() && info.token.is_some() {
            self.load_list(info);
        }
        egui::Panel::left("sidebar")
            .resizable(true)
            .default_size(300.0)
            .size_range(220.0..=520.0)
            .frame(theme::panel(p.mantle, 8, 6))
            .show(ui, |ui| {
                ui.add_space(6.0);
                ui.label(RichText::new("GITHUB").small().color(p.faint));
                ui.add_space(4.0);
                let Some(slug) = &info.slug else {
                    ui.label(
                        RichText::new("This repository has no GitHub remote.").color(p.subtext),
                    );
                    if ui.button("Add remote…").clicked() {
                        out.push(HubAction::AddRemote);
                    }
                    return;
                };
                ui.label(RichText::new(slug).strong().color(p.text_strong));
                if info.token.is_none() {
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new("Connect a GitHub account to see pull requests and issues.")
                            .color(p.subtext),
                    );
                    if theme::primary_button(ui, "Connect GitHub", true).clicked() {
                        out.push(HubAction::SignIn);
                    }
                    return;
                }
                let rh = self.repos.entry(info.path.clone()).or_default();
                ui.add_space(6.0);
                let mut idx = usize::from(rh.kind == ListKind::Issues);
                if theme::segmented(ui, &["Pull requests", "Issues"], &mut idx) {
                    rh.kind = if idx == 1 {
                        ListKind::Issues
                    } else {
                        ListKind::Pulls
                    };
                    rh.selected = None;
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.selectable_label(!rh.closed, "Open").clicked() && rh.closed {
                        rh.closed = false;
                        rh.pulls = Loadable::default();
                        rh.issues = Loadable::default();
                    }
                    if ui.selectable_label(rh.closed, "Closed").clicked() && !rh.closed {
                        rh.closed = true;
                        rh.pulls = Loadable::default();
                        rh.issues = Loadable::default();
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let loading = match rh.kind {
                            ListKind::Pulls => rh.pulls.loading,
                            ListKind::Issues => rh.issues.loading,
                        };
                        if loading {
                            ui.add(egui::Spinner::new().size(14.0));
                        } else if ui
                            .small_button("⟳")
                            .on_hover_text("Reload from GitHub")
                            .clicked()
                        {
                            rh.pulls.at = None;
                            rh.pulls.error = None;
                            rh.issues.at = None;
                            rh.issues.error = None;
                            rh.details.clear();
                        }
                    });
                });
                ui.add(
                    egui::TextEdit::singleline(&mut rh.filter)
                        .hint_text("🔍  Filter")
                        .desired_width(f32::INFINITY),
                );
                ui.add_space(6.0);
                let existing = info.branch.as_deref().and_then(|b| pr_for_branch(rh, b));
                if let (ListKind::Pulls, Some(n)) = (rh.kind, existing) {
                    let w = ui.available_width();
                    let r = ui.add(
                        egui::Button::new(
                            RichText::new(format!("View pull request #{n}")).strong(),
                        )
                        .min_size(vec2(w, 30.0)),
                    );
                    if r.on_hover_text("The current branch already has an open pull request")
                        .clicked()
                    {
                        rh.selected = Some(n);
                        rh.tab = DetailTab::Conversation;
                    }
                    ui.add_space(6.0);
                } else if rh.kind == ListKind::Pulls {
                    let w = ui.available_width();
                    let has_branch = info.branch.is_some();
                    let r = ui.add_enabled(
                        has_branch,
                        egui::Button::new(
                            RichText::new("✚  New pull request")
                                .strong()
                                .color(p.on_accent),
                        )
                        .fill(p.accent)
                        .stroke(Stroke::NONE)
                        .min_size(vec2(w, 30.0)),
                    );
                    if r.on_hover_text("Open a pull request for the current branch")
                        .clicked()
                    {
                        if let Err(e) = self.open_create_for_branch(info) {
                            out.push(HubAction::Notify(e, false));
                        }
                    }
                    ui.add_space(6.0);
                }
                let rh = self.repos.entry(info.path.clone()).or_default();
                let q = rh.filter.trim().to_lowercase();
                egui::ScrollArea::vertical()
                    .auto_shrink(false)
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;
                        match rh.kind {
                            ListKind::Pulls => {
                                if let Some(e) = &rh.pulls.error {
                                    ui.label(RichText::new(e).color(p.red));
                                }
                                let list = rh.pulls.value.clone().unwrap_or_default();
                                if rh.pulls.value.is_some() && list.is_empty() {
                                    ui.label(
                                        RichText::new(if rh.closed {
                                            "No closed pull requests."
                                        } else {
                                            "No open pull requests."
                                        })
                                        .color(p.faint),
                                    );
                                }
                                for pr in list.iter().filter(|x| {
                                    q.is_empty()
                                        || x.title.to_lowercase().contains(&q)
                                        || x.number.to_string() == q
                                }) {
                                    let (icon, color) = pr_icon(pr, p);
                                    let sub = format!(
                                        "{} · {} → {} · {}",
                                        pr.user.login,
                                        pr.head.ref_name,
                                        pr.base.ref_name,
                                        ago(&pr.updated_at)
                                    );
                                    if list_row(
                                        ui,
                                        icon,
                                        color,
                                        &format!("#{}  {}", pr.number, pr.title),
                                        &sub,
                                        rh.selected == Some(pr.number),
                                    )
                                    .clicked()
                                    {
                                        rh.selected = Some(pr.number);
                                        rh.tab = DetailTab::Conversation;
                                    }
                                }
                            }
                            ListKind::Issues => {
                                if let Some(e) = &rh.issues.error {
                                    ui.label(RichText::new(e).color(p.red));
                                }
                                let list = rh.issues.value.clone().unwrap_or_default();
                                if rh.issues.value.is_some() && list.is_empty() {
                                    ui.label(
                                        RichText::new(if rh.closed {
                                            "No closed issues."
                                        } else {
                                            "No open issues."
                                        })
                                        .color(p.faint),
                                    );
                                }
                                for is in list.iter().filter(|x| {
                                    q.is_empty()
                                        || x.title.to_lowercase().contains(&q)
                                        || x.number.to_string() == q
                                }) {
                                    let (icon, color) = if is.state == "open" {
                                        ("●", p.green)
                                    } else {
                                        ("✔", p.purple)
                                    };
                                    let mut sub =
                                        format!("{} · {}", is.user.login, ago(&is.updated_at));
                                    if is.comments > 0 {
                                        sub.push_str(&format!(" · 💬 {}", is.comments));
                                    }
                                    if list_row(
                                        ui,
                                        icon,
                                        color,
                                        &format!("#{}  {}", is.number, is.title),
                                        &sub,
                                        rh.selected == Some(is.number),
                                    )
                                    .clicked()
                                    {
                                        rh.selected = Some(is.number);
                                    }
                                }
                            }
                        }
                    });
            });
        out
    }

    // ------------------------------------------------------------------ main area

    pub fn main(&mut self, ui: &mut egui::Ui, info: &RepoInfo) -> Vec<HubAction> {
        let mut out = Vec::new();
        let selected = self.repos.get(&info.path).and_then(|r| r.selected);
        if let Some(n) = selected {
            self.load_detail(info, n);
        }
        let p = pal(ui);
        let Some(n) = selected else {
            egui::CentralPanel::default().show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space((ui.available_height() * 0.2).max(20.0));
                    ui.label(RichText::new(egui::special_emojis::GITHUB.to_string()).size(40.0).color(p.text_strong));
                    ui.label(RichText::new("Pull requests & issues").size(20.0).strong().color(p.text_strong));
                    ui.label(RichText::new("Pick one on the left — review, comment, check CI and merge without leaving FoxGit.").color(p.subtext));
                });
            });
            return out;
        };
        let rh = self.repos.entry(info.path.clone()).or_default();
        let is_pr = rh.kind == ListKind::Pulls;
        let busy = rh.busy.clone();
        let d = rh.details.entry(n).or_default();
        if d.loading && !d.loaded {
            egui::CentralPanel::default().show(ui, |ui| {
                ui.centered_and_justified(|ui| {
                    ui.add(egui::Spinner::new());
                });
            });
            return out;
        }
        if let Some(e) = &d.error {
            let e = e.clone();
            egui::CentralPanel::default().show(ui, |ui| {
                ui.label(RichText::new(format!("Could not load #{n}: {e}")).color(p.red));
                if ui.button("Try again").clicked() {
                    rh.details.remove(&n);
                }
            });
            return out;
        }

        // collect what the actions need before borrowing the UI
        let pull = d.pull.clone();
        let issue = d.issue.clone();
        let (title, state_txt, state_color, html, author, body) = match (&pull, &issue) {
            (Some(pr), _) => {
                let (txt, c) = pr_state(pr, p);
                (
                    pr.title.clone(),
                    txt,
                    c,
                    pr.html_url.clone(),
                    pr.user.login.clone(),
                    pr.body.clone().unwrap_or_default(),
                )
            }
            (_, Some(is)) => {
                let (txt, c) = if is.state == "open" {
                    ("Open", p.green)
                } else {
                    ("Closed", p.purple)
                };
                (
                    is.title.clone(),
                    txt,
                    c,
                    is.html_url.clone(),
                    is.user.login.clone(),
                    is.body.clone().unwrap_or_default(),
                )
            }
            _ => return out,
        };
        let mut todo: Vec<Todo> = Vec::new();

        egui::Panel::top("hub_header")
            .frame(theme::panel(p.base, 16, 12))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("#{n}")).size(18.0).color(p.faint));
                    ui.add(egui::Label::new(RichText::new(&title).size(18.0).strong().color(p.text_strong)).truncate());
                });
                ui.horizontal(|ui| {
                    let pill = egui::Frame::new()
                        .fill(theme::alpha(state_color, 0.18))
                        .stroke(Stroke::new(1.0, theme::alpha(state_color, 0.5)))
                        .corner_radius(10.0)
                        .inner_margin(egui::Margin::symmetric(8, 2));
                    pill.show(ui, |ui| ui.label(RichText::new(state_txt).small().strong().color(state_color)));
                    if let Some(pr) = &pull {
                        ui.label(RichText::new(format!("{author} wants to merge")).color(p.subtext));
                        ui.label(RichText::new(&pr.head.ref_name).monospace().color(p.accent_hover))
                            .on_hover_text(&pr.head.label);
                        ui.label(RichText::new("into").color(p.subtext));
                        ui.label(RichText::new(&pr.base.ref_name).monospace().color(p.accent_hover));
                        if let (Some(a), Some(dl), Some(f)) = (pr.additions, pr.deletions, pr.changed_files) {
                            ui.label(RichText::new(format!("+{a}")).monospace().color(p.green));
                            ui.label(RichText::new(format!("−{dl}")).monospace().color(p.red));
                            ui.label(RichText::new(format!("· {f} file(s)")).color(p.faint));
                        }
                    } else {
                        ui.label(RichText::new(format!("opened by {author}")).color(p.subtext));
                    }
                });
                let labels = pull
                    .as_ref()
                    .map(|x| &x.labels)
                    .or(issue.as_ref().map(|x| &x.labels));
                if let Some(labels) = labels.filter(|l| !l.is_empty()) {
                    ui.horizontal_wrapped(|ui| {
                        for l in labels {
                            label_chip(ui, l);
                        }
                    });
                }
                if let Some(checks) = &d.checks {
                    ui.horizontal(|ui| checks_summary(ui, checks));
                }
                ui.add_space(6.0);
                // ---- actions
                ui.horizontal(|ui| {
                    if let Some(b) = &busy {
                        ui.add(egui::Spinner::new().size(14.0));
                        ui.label(RichText::new(format!("{b}…")).color(p.accent_hover));
                        return;
                    }
                    if let Some(pr) = &pull {
                        let open = pr.state == "open";
                        if open {
                            let conflict = pr.mergeable == Some(false) || pr.mergeable_state.as_deref() == Some("dirty");
                            let merge = ui.add_enabled(
                                !conflict && !pr.draft,
                                egui::Button::new(RichText::new("Merge ▾").strong().color(p.on_accent))
                                    .fill(p.green)
                                    .stroke(Stroke::NONE)
                                    .min_size(vec2(0.0, 28.0)),
                            );
                            let merge = if conflict {
                                merge.on_disabled_hover_text("This branch has conflicts with the base — check it out and resolve them")
                            } else if pr.draft {
                                merge.on_disabled_hover_text("Draft — mark it ready on GitHub first")
                            } else {
                                merge
                            };
                            egui::Popup::menu(&merge).show(|ui| {
                                for (m, label) in [("merge", "Create a merge commit"), ("squash", "Squash and merge"), ("rebase", "Rebase and merge")] {
                                    if ui.button(label).clicked() {
                                        todo.push(Todo::Merge(m));
                                        ui.close();
                                    }
                                }
                            });
                            if ui.button("✔ Approve").clicked() {
                                todo.push(Todo::Review("APPROVE"));
                            }
                            if ui.button("Request changes").on_hover_text("Uses the text in the comment box below as the reason").clicked() {
                                todo.push(Todo::Review("REQUEST_CHANGES"));
                            }
                        }
                        if ui.button(format!("{} Checkout", crate::theme::BRANCH)).on_hover_text(format!("Fetch #{n} into the local branch pr/{n} and switch to it")).clicked() {
                            todo.push(Todo::Checkout);
                        }
                        if open && ui.button("Close").clicked() {
                            todo.push(Todo::State("closed"));
                        }
                        if !open && !pr.is_merged() && ui.button("Reopen").clicked() {
                            todo.push(Todo::State("open"));
                        }
                    }
                    if let Some(is) = &issue {
                        if ui
                            .add(
                                egui::Button::new(RichText::new("✨ Ask an agent").strong().color(p.on_accent))
                                    .fill(p.accent)
                                    .stroke(Stroke::NONE)
                                    .min_size(vec2(0.0, 28.0)),
                            )
                            .on_hover_text("Start an agent chat with this issue as its task")
                            .clicked()
                        {
                            todo.push(Todo::Agent);
                        }
                        if is.state == "open" {
                            if ui.button("Close issue").clicked() {
                                todo.push(Todo::State("closed"));
                            }
                        } else if ui.button("Reopen").clicked() {
                            todo.push(Todo::State("open"));
                        }
                    }
                    if ui.button("Open on GitHub").clicked() {
                        ui.ctx().open_url(egui::OpenUrl::new_tab(&html));
                    }
                });
            });

        // ---- comment box (bottom)
        let rh = self.repos.entry(info.path.clone()).or_default();
        egui::Panel::bottom("hub_comment")
            .frame(theme::panel(p.mantle, 16, 10))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let w = ui.available_width() - 110.0;
                    ui.add(
                        egui::TextEdit::multiline(&mut rh.comment)
                            .hint_text("Leave a comment (Markdown)")
                            .desired_rows(2)
                            .desired_width(w)
                            .margin(vec2(10.0, 8.0)),
                    );
                    if theme::primary_button(
                        ui,
                        "Comment",
                        !rh.comment.trim().is_empty() && rh.busy.is_none(),
                    )
                    .clicked()
                    {
                        todo.push(Todo::Comment);
                    }
                });
            });

        // ---- tabs + content
        let d = rh.details.entry(n).or_default();
        egui::CentralPanel::default()
            .frame(theme::panel(p.base, 16, 8))
            .show(ui, |ui| {
                if is_pr {
                    let files = d.files.as_ref().map(|f| f.len()).unwrap_or(0);
                    let checks = d.checks.as_ref().map(|c| c.len()).unwrap_or(0);
                    let labels = [
                        "Conversation".to_string(),
                        format!("Files ({files})"),
                        format!("Checks ({checks})"),
                    ];
                    let refs: Vec<&str> = labels.iter().map(|s| s.as_str()).collect();
                    let mut idx = match rh.tab {
                        DetailTab::Conversation => 0,
                        DetailTab::Files => 1,
                        DetailTab::Checks => 2,
                    };
                    if theme::segmented(ui, &refs, &mut idx) {
                        rh.tab =
                            [DetailTab::Conversation, DetailTab::Files, DetailTab::Checks][idx];
                    }
                    ui.add_space(8.0);
                }
                let tab = if is_pr {
                    rh.tab
                } else {
                    DetailTab::Conversation
                };
                match tab {
                    DetailTab::Conversation => {
                        conversation(ui, &author, &body, &d.comments, &d.reviews)
                    }
                    DetailTab::Files => {
                        files_view(ui, d.files.as_deref().unwrap_or(&[]), &mut rh.open_files)
                    }
                    DetailTab::Checks => checks_list(ui, d.checks.as_deref().unwrap_or(&[])),
                }
            });

        // ---- run what was clicked
        for t in todo {
            let comment = self
                .repos
                .get(&info.path)
                .map(|r| r.comment.trim().to_string())
                .unwrap_or_default();
            match t {
                Todo::Merge(method) => self.act(
                    info,
                    n,
                    "Merging",
                    format!("Merged #{n}"),
                    move |tok, slug| github::merge_pull(tok, slug, n, method),
                ),
                Todo::Review(event) => {
                    let done = if event == "APPROVE" {
                        format!("Approved #{n}")
                    } else {
                        format!("Requested changes on #{n}")
                    };
                    if event == "REQUEST_CHANGES" && comment.is_empty() {
                        out.push(HubAction::Notify(
                            "Write what should change in the comment box first".into(),
                            false,
                        ));
                    } else {
                        self.act(info, n, "Sending review", done, move |tok, slug| {
                            github::submit_review(tok, slug, n, event, &comment)
                        });
                    }
                }
                Todo::Comment => self.act(
                    info,
                    n,
                    "Posting comment",
                    format!("Commented on #{n}"),
                    move |tok, slug| github::add_comment(tok, slug, n, &comment),
                ),
                Todo::State(s) => {
                    let done =
                        format!("{} #{n}", if s == "closed" { "Closed" } else { "Reopened" });
                    self.act(info, n, "Updating", done, move |tok, slug| {
                        github::set_state(tok, slug, n, s)
                    });
                }
                Todo::Checkout => {
                    if let Some(remote) = &info.remote {
                        out.push(HubAction::CheckoutPr {
                            number: n,
                            remote: remote.clone(),
                        });
                    }
                }
                Todo::Agent => {
                    out.push(HubAction::StartAgent {
                        prompt: format!(
                            "Resolve GitHub issue #{n}: {title}\n\n{body}\n\nWhen you are done, summarise the change. The pull request should say \"Fixes #{n}\"."
                        ),
                    });
                }
            }
        }
        out
    }
}

#[cfg(debug_assertions)]
impl Hub {
    /// Dev only (`FOXGIT_DEMO=hubfake`): made-up pull requests for layout checks, no network.
    pub fn demo_fill(&mut self, repo: &std::path::Path) {
        let now = chrono::Utc::now();
        let t = |h: i64| (now - chrono::Duration::hours(h)).to_rfc3339();
        let pr = |n: u64, title: &str, user: &str, head: &str, draft: bool, h: i64| {
            serde_json::json!({
                "number": n, "title": title, "body": "Adds **currency** support to prices.\n\n- formats with the locale\n- keeps two decimals\n\nFixes #12",
                "state": "open", "draft": draft, "user": {"login": user}, "html_url": "https://github.com/example/web-app/pull/1",
                "head": {"ref": head, "sha": "abc123", "label": format!("{user}:{head}")},
                "base": {"ref": "main", "sha": "def456", "label": "example:main"},
                "updated_at": t(h), "mergeable": true, "mergeable_state": "clean",
                "additions": 42, "deletions": 7, "changed_files": 3,
                "labels": [{"name": "enhancement", "color": "a2eeef"}, {"name": "ui", "color": "d876e3"}]
            })
        };
        let pulls: Vec<Pull> = serde_json::from_value(serde_json::json!([
            pr(
                14,
                "Show the currency next to prices",
                "nok",
                "feature/currency",
                false,
                2
            ),
            pr(
                13,
                "Dark mode for the checkout page",
                "mali",
                "feature/dark-checkout",
                true,
                26
            ),
            pr(
                11,
                "Bump vite from 5.1 to 5.4",
                "dependabot",
                "deps/vite",
                false,
                80
            ),
        ]))
        .unwrap();
        let detail = Detail {
            pull: Some(pulls[0].clone()),
            issue: None,
            files: Some(
                serde_json::from_value(serde_json::json!([
                    {"filename": "src.js", "status": "modified", "additions": 3, "deletions": 1,
                     "patch": "@@ -1,5 +1,5 @@\n-export const price = (p) => {\n-  return p.toFixed(2)\n+export const price = (p, cur = \"THB\") => {\n+  return `${p.toFixed(2)} ${cur}`\n }\n \n export const total = (items) => items.length"},
                    {"filename": "README.md", "status": "modified", "additions": 2, "deletions": 0, "patch": "@@ -3,3 +3,5 @@\n Run `pnpm dev`.\n+\n+Prices show their currency."},
                    {"filename": "assets/logo.png", "status": "added", "additions": 0, "deletions": 0}
                ]))
                .unwrap(),
            ),
            comments: serde_json::from_value(serde_json::json!([
                {"user": {"login": "mali"}, "body": "Looks good — can we default to the user's locale?", "created_at": t(1)},
            ]))
            .unwrap(),
            reviews: serde_json::from_value(serde_json::json!([
                {"user": {"login": "krit"}, "body": "", "state": "APPROVED", "submitted_at": t(0)},
            ]))
            .unwrap(),
            checks: Some(vec![
                Check { name: "build (ubuntu)".into(), state: CheckState::Success, url: None },
                Check { name: "build (macos)".into(), state: CheckState::Success, url: None },
                Check { name: "test".into(), state: CheckState::Failure, url: None },
                Check { name: "deploy preview".into(), state: CheckState::Pending, url: None },
            ]),
            loading: false,
            error: None,
            loaded: true,
        };
        let rh = self.repos.entry(repo.to_path_buf()).or_default();
        rh.pulls = Loadable {
            value: Some(pulls),
            loading: false,
            error: None,
            at: Some(Instant::now()),
        };
        rh.issues.at = Some(Instant::now());
        rh.issues.value = Some(Vec::new());
        rh.asked_default = true;
        rh.selected = Some(14);
        rh.details.insert(14, detail);
    }

    pub fn demo_tab(&mut self, repo: &std::path::Path, tab: &str) {
        let rh = self.repos.entry(repo.to_path_buf()).or_default();
        rh.tab = match tab {
            "files" => DetailTab::Files,
            "checks" => DetailTab::Checks,
            _ => DetailTab::Conversation,
        };
    }
}

enum Todo {
    Merge(&'static str),
    Review(&'static str),
    Comment,
    State(&'static str),
    Checkout,
    Agent,
}

/// The open pull request whose head is `branch`, if the list is loaded.
fn pr_for_branch(rh: &RepoHub, branch: &str) -> Option<u64> {
    rh.pulls
        .value
        .as_ref()?
        .iter()
        .find(|p| p.head.ref_name == branch && p.state == "open")
        .map(|p| p.number)
}

fn label_chip(ui: &mut egui::Ui, l: &github::Label) {
    let c = u32::from_str_radix(l.color.trim_start_matches('#'), 16).unwrap_or(0x8b949e);
    let color = Color32::from_rgb((c >> 16) as u8, (c >> 8) as u8, c as u8);
    egui::Frame::new()
        .fill(theme::alpha(color, 0.22))
        .stroke(Stroke::new(1.0, theme::alpha(color, 0.6)))
        .corner_radius(9.0)
        .inner_margin(egui::Margin::symmetric(7, 1))
        .show(ui, |ui| {
            ui.label(RichText::new(&l.name).small().color(pal(ui).text_strong));
        });
}

fn set<T>(l: &mut Loadable<T>, r: Result<T, String>) {
    l.loading = false;
    l.at = Some(Instant::now());
    match r {
        Ok(v) => {
            l.value = Some(v);
            l.error = None;
        }
        Err(e) => l.error = Some(e),
    }
}

fn title_from_branch(b: &str) -> String {
    let last = b.rsplit('/').next().unwrap_or(b);
    let t = last.replace(['-', '_'], " ");
    let mut c = t.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// "3h ago" from an ISO-8601 timestamp.
pub fn ago(ts: &str) -> String {
    let Ok(t) = chrono::DateTime::parse_from_rfc3339(ts) else {
        return String::new();
    };
    let s = (chrono::Utc::now() - t.with_timezone(&chrono::Utc))
        .num_seconds()
        .max(0);
    match s {
        0..=59 => "just now".into(),
        60..=3599 => format!("{}m ago", s / 60),
        3600..=86399 => format!("{}h ago", s / 3600),
        86400..=2_591_999 => format!("{}d ago", s / 86400),
        _ => t.format("%d %b %Y").to_string(),
    }
}

fn pr_icon(pr: &Pull, p: &theme::Palette) -> (&'static str, Color32) {
    if pr.is_merged() {
        ("✔", p.purple)
    } else if pr.state != "open" {
        ("×", p.red)
    } else if pr.draft {
        ("●", p.faint)
    } else {
        ("●", p.green)
    }
}

fn pr_state(pr: &Pull, p: &theme::Palette) -> (&'static str, Color32) {
    if pr.is_merged() {
        ("Merged", p.purple)
    } else if pr.state != "open" {
        ("Closed", p.red)
    } else if pr.draft {
        ("Draft", p.faint)
    } else {
        ("Open", p.green)
    }
}

fn list_row(
    ui: &mut egui::Ui,
    icon: &str,
    color: Color32,
    title: &str,
    sub: &str,
    selected: bool,
) -> egui::Response {
    let p = pal(ui);
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 46.0), Sense::click());
    let painter = ui.painter();
    if selected {
        painter.rect_filled(rect, 8.0, p.selection);
        painter.rect_filled(
            Rect::from_min_size(rect.left_top() + vec2(0.0, 9.0), vec2(3.0, 28.0)),
            1.5,
            p.accent,
        );
    } else if resp.hovered() {
        painter.rect_filled(rect, 8.0, p.hover);
    }
    painter.text(
        pos2(rect.left() + 15.0, rect.top() + 15.0),
        egui::Align2::CENTER_CENTER,
        icon,
        FontId::proportional(12.0),
        color,
    );
    let clip = Rect::from_min_max(
        pos2(rect.left() + 28.0, rect.top()),
        pos2(rect.right() - 6.0, rect.bottom()),
    )
    .intersect(ui.clip_rect());
    let tp = painter.with_clip_rect(clip);
    tp.text(
        pos2(rect.left() + 28.0, rect.top() + 15.0),
        egui::Align2::LEFT_CENTER,
        title,
        FontId::proportional(13.0),
        if selected { p.sel_text } else { p.text_strong },
    );
    tp.text(
        pos2(rect.left() + 28.0, rect.top() + 33.0),
        egui::Align2::LEFT_CENTER,
        sub,
        FontId::proportional(11.0),
        p.faint,
    );
    resp.on_hover_text(title)
}

pub fn check_icon(s: CheckState, p: &theme::Palette) -> (&'static str, Color32) {
    match s {
        CheckState::Success => ("✔", p.green),
        CheckState::Failure => ("×", p.red),
        CheckState::Pending => ("●", p.yellow),
        CheckState::Neutral => ("●", p.faint),
    }
}

/// "✔ 5  ✕ 1  ● 2" with a popup listing every check.
pub fn checks_summary(ui: &mut egui::Ui, checks: &[Check]) {
    let p = pal(ui);
    if checks.is_empty() {
        ui.label(RichText::new("No CI checks").small().color(p.faint));
        return;
    }
    let count = |s| checks.iter().filter(|c| c.state == s).count();
    let (ok, bad, wait) = (
        count(CheckState::Success),
        count(CheckState::Failure),
        count(CheckState::Pending),
    );
    let mut job = egui::text::LayoutJob::default();
    let f = FontId::proportional(12.5);
    let mut add =
        |t: String, c: Color32| job.append(&t, 0.0, egui::TextFormat::simple(f.clone(), c));
    add("CI  ".into(), p.subtext);
    if ok > 0 {
        add(format!("✔ {ok}  "), p.green);
    }
    if bad > 0 {
        add(format!("× {bad}  "), p.red);
    }
    if wait > 0 {
        add(format!("● {wait} running"), p.yellow);
    }
    let r = ui
        .add(egui::Button::new(job).frame(false))
        .on_hover_text("Show every check");
    egui::Popup::menu(&r).show(|ui| {
        ui.set_min_width(260.0);
        checks_list(ui, checks);
    });
}

fn checks_list(ui: &mut egui::Ui, checks: &[Check]) {
    let p = pal(ui);
    if checks.is_empty() {
        ui.label(RichText::new("No CI checks reported for this commit.").color(p.faint));
        return;
    }
    egui::ScrollArea::vertical()
        .id_salt("checks_list")
        .show(ui, |ui| {
            for c in checks {
                ui.horizontal(|ui| {
                    let (icon, color) = check_icon(c.state, p);
                    ui.label(RichText::new(icon).color(color));
                    ui.label(RichText::new(&c.name).color(p.text));
                    if let Some(u) = &c.url {
                        if ui.link(RichText::new("details").small()).clicked() {
                            ui.ctx().open_url(egui::OpenUrl::new_tab(u));
                        }
                    }
                });
            }
        });
}

/// (time, author, text, review verdict) of one conversation entry.
type Entry = (String, String, String, Option<(&'static str, Color32)>);

fn conversation(
    ui: &mut egui::Ui,
    author: &str,
    body: &str,
    comments: &[Comment],
    reviews: &[Review],
) {
    let p = pal(ui);
    // the description, then comments and reviews in time order
    let mut items: Vec<Entry> = Vec::new();
    for c in comments {
        items.push((
            c.created_at.clone(),
            c.user.login.clone(),
            c.body.clone().unwrap_or_default(),
            None,
        ));
    }
    for r in reviews {
        let tag = match r.state.as_str() {
            "APPROVED" => Some(("approved", p.green)),
            "CHANGES_REQUESTED" => Some(("requested changes", p.red)),
            "COMMENTED" => Some(("reviewed", p.subtext)),
            _ => None,
        };
        let body = r.body.clone().unwrap_or_default();
        if tag.is_some() || !body.trim().is_empty() {
            items.push((
                r.submitted_at.clone().unwrap_or_default(),
                r.user.login.clone(),
                body,
                tag,
            ));
        }
    }
    items.sort_by(|a, b| a.0.cmp(&b.0));
    egui::ScrollArea::vertical()
        .auto_shrink(false)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            bubble(
                ui,
                author,
                "",
                if body.trim().is_empty() {
                    "No description provided."
                } else {
                    body
                },
                None,
            );
            for (when, who, text, tag) in &items {
                bubble(ui, who, when, text, *tag);
            }
            if items.is_empty() {
                ui.add_space(6.0);
                ui.label(RichText::new("No comments yet.").color(p.faint));
            }
        });
}

fn bubble(ui: &mut egui::Ui, who: &str, when: &str, text: &str, tag: Option<(&str, Color32)>) {
    let p = pal(ui);
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        let (r, _) = ui.allocate_exact_size(vec2(20.0, 20.0), Sense::hover());
        theme::paint_avatar(ui.painter(), r.center(), 10.0, who);
        ui.label(RichText::new(who).strong().color(p.text_strong));
        if let Some((t, c)) = tag {
            ui.label(RichText::new(t).color(c));
        }
        if !when.is_empty() {
            ui.label(RichText::new(ago(when)).small().color(p.faint));
        }
    });
    if !text.trim().is_empty() {
        egui::Frame::new()
            .fill(p.surface0)
            .stroke(Stroke::new(1.0, p.border))
            .corner_radius(10.0)
            .inner_margin(egui::Margin::symmetric(12, 10))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.add(
                    egui::Label::new(RichText::new(text).color(p.text))
                        .wrap()
                        .selectable(true),
                );
            });
    }
}

fn files_view(ui: &mut egui::Ui, files: &[PullFile], open: &mut HashMap<String, bool>) {
    let p = pal(ui);
    if files.is_empty() {
        ui.label(RichText::new("No changed files reported.").color(p.faint));
        return;
    }
    egui::ScrollArea::both()
        .id_salt("pr_files")
        .auto_shrink(false)
        .show(ui, |ui| {
            for f in files {
                let is_open = *open.entry(f.filename.clone()).or_insert(files.len() <= 8);
                let (rect, resp) = ui.allocate_exact_size(
                    vec2(ui.available_width().max(400.0), 30.0),
                    Sense::click(),
                );
                let painter = ui.painter();
                painter.rect_filled(
                    rect,
                    6.0,
                    if resp.hovered() {
                        p.surface1
                    } else {
                        p.surface0
                    },
                );
                theme::paint_caret(
                    painter,
                    pos2(rect.left() + 12.0, rect.center().y),
                    is_open,
                    p.subtext,
                );
                let (letter, c) = match f.status.as_str() {
                    "added" => ("A", p.green),
                    "removed" => ("D", p.red),
                    "renamed" => ("R", p.blue),
                    _ => ("M", p.yellow),
                };
                painter.text(
                    pos2(rect.left() + 26.0, rect.center().y),
                    egui::Align2::LEFT_CENTER,
                    letter,
                    FontId::monospace(12.0),
                    c,
                );
                painter.text(
                    pos2(rect.left() + 42.0, rect.center().y),
                    egui::Align2::LEFT_CENTER,
                    &f.filename,
                    FontId::monospace(12.5),
                    p.text_strong,
                );
                painter.text(
                    pos2(rect.right() - 10.0, rect.center().y),
                    egui::Align2::RIGHT_CENTER,
                    format!("+{}  −{}", f.additions, f.deletions),
                    FontId::monospace(12.0),
                    p.subtext,
                );
                if resp.clicked() {
                    open.insert(f.filename.clone(), !is_open);
                }
                if is_open {
                    match &f.patch {
                        Some(patch) => patch_lines(ui, patch),
                        None => {
                            ui.label(
                                RichText::new("  (no text diff — binary or too large)")
                                    .small()
                                    .color(p.faint),
                            );
                        }
                    }
                }
                ui.add_space(6.0);
            }
        });
}

fn patch_lines(ui: &mut egui::Ui, patch: &str) {
    let p = pal(ui);
    let font = FontId::monospace(12.5);
    let h = ui.fonts_mut(|f| f.row_height(&font)) + 2.0;
    let lines: Vec<&str> = patch.lines().collect();
    for (i, l) in lines.iter().enumerate() {
        if i >= MAX_PATCH_LINES {
            ui.label(
                RichText::new(format!(
                    "  … {} more lines — open on GitHub",
                    lines.len() - i
                ))
                .small()
                .color(p.faint),
            );
            break;
        }
        let (bg, fg) = if l.starts_with("@@") {
            (theme::alpha(p.blue, 0.10), p.blue)
        } else if l.starts_with('+') {
            (theme::alpha(p.green, 0.13), p.text)
        } else if l.starts_with('-') {
            (theme::alpha(p.red, 0.13), p.text)
        } else {
            (Color32::TRANSPARENT, p.text)
        };
        let g = ui
            .painter()
            .layout_no_wrap(l.replace('\t', "    "), font.clone(), fg);
        let w = (g.size().x + 20.0).max(ui.available_width());
        let (rect, _) = ui.allocate_exact_size(vec2(w, h), Sense::hover());
        if bg != Color32::TRANSPARENT {
            ui.painter().rect_filled(rect, 0.0, bg);
        }
        ui.painter()
            .galley(pos2(rect.left() + 8.0, rect.top() + 1.0), g, fg);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_titles() {
        assert_eq!(title_from_branch("feature/login-timeout"), "Login timeout");
        assert_eq!(title_from_branch("agent/fix_crash-12ab"), "Fix crash 12ab");
    }

    #[test]
    fn relative_times() {
        let now = chrono::Utc::now();
        assert_eq!(
            ago(&(now - chrono::Duration::seconds(30)).to_rfc3339()),
            "just now"
        );
        assert_eq!(
            ago(&(now - chrono::Duration::minutes(5)).to_rfc3339()),
            "5m ago"
        );
        assert_eq!(
            ago(&(now - chrono::Duration::hours(3)).to_rfc3339()),
            "3h ago"
        );
        assert_eq!(ago("garbage"), "");
    }
}
