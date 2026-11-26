//! GitHub account UI: sign-in dialog, the toolbar account menu and the "Remote" page that
//! lists every repository the account can access (Sourcetree-style) with a Clone button.

use crate::github::{self, ApiError, DeviceCode, GhRepo, Poll, User};
use crate::tabs::Project;
use crate::theme::{self, pal};
use eframe::egui;
use egui::{pos2, vec2, FontId, Rect, RichText, Sense, Stroke};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

const GITHUB_GLYPH: char = egui::special_emojis::GITHUB;

pub struct Account {
    pub login: String,
    pub name: Option<String>,
    pub html_url: String,
    token: String,
    /// The last check against the API failed (offline, or the token was revoked).
    pub problem: Option<String>,
}

enum Msg {
    /// Startup: token read back from the keychain.
    Restored {
        login: String,
        token: Result<String, String>,
    },
    /// Startup: result of checking the restored token.
    Checked(Result<User, ApiError>),
    SignedIn {
        token: String,
        user: Result<User, ApiError>,
        keychain: Result<(), String>,
    },
    Device(Result<DeviceCode, String>),
    DeviceDone(Result<String, String>),
    Repos(Result<Vec<GhRepo>, ApiError>),
}

enum SignIn {
    Start {
        pat: String,
        error: Option<String>,
        busy: bool,
    },
    Device {
        code: DeviceCode,
        started: Instant,
        copied: bool,
        finishing: bool,
    },
}

pub enum MenuAction {
    OpenRemote,
    SignIn,
    SignOut,
}

pub enum RemoteEvent {
    Clone { url: String, name: String },
    Open(String),
    SignIn,
}

pub struct GitHub {
    pub account: Option<Account>,
    /// Login being restored from the keychain at startup.
    restoring: Option<String>,
    pub client_id: String,
    repos: Vec<GhRepo>,
    repos_loaded: bool,
    repos_loading: bool,
    repos_error: Option<String>,
    dialog: Option<SignIn>,
    cancel: Arc<AtomicBool>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    ctx: egui::Context,
    /// `owner/name` → local project path, so known repositories show "Open" instead of "Clone".
    local: Option<HashMap<String, String>>,
    notes: Vec<(String, bool)>,
}

impl GitHub {
    pub fn new(ctx: &egui::Context, login: Option<String>, client_id: Option<String>) -> Self {
        let (tx, rx) = channel();
        let client_id = client_id
            .filter(|s| !s.trim().is_empty())
            .or_else(|| github::BUILTIN_CLIENT_ID.map(String::from))
            .unwrap_or_default();
        let gh = Self {
            account: None,
            restoring: login.clone(),
            client_id,
            repos: Vec::new(),
            repos_loaded: false,
            repos_loading: false,
            repos_error: None,
            dialog: None,
            cancel: Arc::new(AtomicBool::new(false)),
            tx,
            rx,
            ctx: ctx.clone(),
            local: None,
            notes: Vec::new(),
        };
        if let Some(login) = login {
            // The keychain may show a system prompt: never do this on the UI thread.
            gh.spawn(move |tx| {
                let token = github::load_token(&login);
                let ok = token.clone().ok();
                let _ = tx.send(Msg::Restored { login, token });
                ok.map(|t| Msg::Checked(github::fetch_user(&t)))
            });
        }
        gh
    }

    fn spawn(&self, job: impl FnOnce(&Sender<Msg>) -> Option<Msg> + Send + 'static) {
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        std::thread::spawn(move || {
            if let Some(m) = job(&tx) {
                let _ = tx.send(m);
            }
            ctx.request_repaint();
        });
    }

    /// Login to persist (also while it is still being restored).
    pub fn saved_login(&self) -> Option<String> {
        self.account
            .as_ref()
            .map(|a| a.login.clone())
            .or_else(|| self.restoring.clone())
    }

    /// Client ID to persist (only when it differs from the built-in one).
    pub fn saved_client_id(&self) -> Option<String> {
        let id = self.client_id.trim();
        (!id.is_empty() && Some(id) != github::BUILTIN_CLIENT_ID).then(|| id.to_string())
    }

    pub fn invalidate_local(&mut self) {
        self.local = None;
    }

    fn set_account(&mut self, a: Account) {
        github::set_git_credential(Some((a.login.clone(), a.token.clone())));
        self.account = Some(a);
        self.repos.clear();
        self.repos_loaded = false;
        self.repos_error = None;
    }

    pub fn open_signin(&mut self) {
        self.dialog = Some(SignIn::Start {
            pat: String::new(),
            error: None,
            busy: false,
        });
    }

    pub fn sign_out(&mut self) {
        if let Some(a) = self.account.take() {
            self.spawn(move |_| {
                github::delete_token(&a.login);
                None
            });
        }
        github::set_git_credential(None);
        self.repos.clear();
        self.repos_loaded = false;
        self.repos_error = None;
        self.notes.push(("Signed out of GitHub".into(), true));
    }

    pub fn reload_repos(&mut self) {
        let Some(a) = &self.account else { return };
        if self.repos_loading {
            return;
        }
        self.repos_loading = true;
        self.repos_error = None;
        let token = a.token.clone();
        self.spawn(move |_| Some(Msg::Repos(github::list_repos(&token))));
    }

    fn verify_and_store(&mut self, token: String) {
        self.spawn(move |_| {
            let user = github::fetch_user(&token);
            let keychain = match &user {
                Ok(u) => github::save_token(&u.login, &token),
                Err(_) => Ok(()),
            };
            Some(Msg::SignedIn {
                token,
                user,
                keychain,
            })
        });
    }

    fn start_device(&mut self) {
        let client_id = self.client_id.trim().to_string();
        if let Some(SignIn::Start { busy, error, .. }) = &mut self.dialog {
            *busy = true;
            *error = None;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel = cancel.clone();
        self.spawn(move |tx| {
            let code = match github::device_start(&client_id) {
                Ok(c) => c,
                Err(e) => return Some(Msg::Device(Err(e))),
            };
            let _ = tx.send(Msg::Device(Ok(code.clone())));
            let mut interval = code.interval.max(5);
            let deadline = Instant::now() + Duration::from_secs(code.expires_in);
            loop {
                // sleep in small steps so Cancel takes effect quickly
                for _ in 0..interval * 4 {
                    if cancel.load(Ordering::Relaxed) {
                        return None;
                    }
                    std::thread::sleep(Duration::from_millis(250));
                }
                if Instant::now() > deadline {
                    return Some(Msg::DeviceDone(Err(
                        "The code expired — start the sign-in again".into(),
                    )));
                }
                match github::device_poll(&client_id, &code.device_code) {
                    Poll::Pending => {}
                    Poll::SlowDown => interval += 5,
                    Poll::Token(t) => return Some(Msg::DeviceDone(Ok(t))),
                    Poll::Failed(e) => return Some(Msg::DeviceDone(Err(e))),
                }
            }
        });
    }

    fn show_error(&mut self, e: String) {
        self.dialog = Some(SignIn::Start {
            pat: String::new(),
            error: Some(e),
            busy: false,
        });
    }

    /// Handle background results. Returns toasts to show (text, ok).
    pub fn poll(&mut self) -> Vec<(String, bool)> {
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::Restored { login, token } => {
                    self.restoring = None;
                    match token {
                        Ok(token) => self.set_account(Account {
                            html_url: format!("https://github.com/{login}"),
                            login,
                            name: None,
                            token,
                            problem: None,
                        }),
                        Err(e) => self.notes.push((
                            format!("GitHub: the saved sign-in could not be read from the keychain ({e}). Sign in again."),
                            false,
                        )),
                    }
                }
                Msg::Checked(res) => {
                    if let Some(a) = &mut self.account {
                        match res {
                            Ok(u) => {
                                a.name = u.name;
                                if !u.html_url.is_empty() {
                                    a.html_url = u.html_url;
                                }
                                a.problem = None;
                            }
                            Err(ApiError::Auth(m)) => {
                                self.notes
                                    .push((format!("GitHub: {m}. Sign in again."), false));
                                a.problem = Some(m);
                            }
                            Err(ApiError::Other(m)) => a.problem = Some(m),
                        }
                    }
                }
                Msg::SignedIn {
                    token,
                    user,
                    keychain,
                } => match user {
                    Ok(u) => {
                        self.dialog = None;
                        if let Some(old) = &self.account {
                            if old.login != u.login {
                                let old = old.login.clone();
                                self.spawn(move |_| {
                                    github::delete_token(&old);
                                    None
                                });
                            }
                        }
                        let login = u.login.clone();
                        self.set_account(Account {
                            login: u.login,
                            name: u.name,
                            html_url: u.html_url,
                            token,
                            problem: None,
                        });
                        self.notes
                            .push((format!("Signed in to GitHub as {login}"), true));
                        if let Err(e) = keychain {
                            self.notes.push((
                                format!("The token could not be saved to the keychain ({e}) — you will need to sign in again next time"),
                                false,
                            ));
                        }
                        self.reload_repos();
                    }
                    Err(e) => self.show_error(e.to_string()),
                },
                Msg::Device(res) => {
                    if self.cancel.load(Ordering::Relaxed) || self.dialog.is_none() {
                        continue;
                    }
                    match res {
                        Ok(code) => {
                            self.dialog = Some(SignIn::Device {
                                code,
                                started: Instant::now(),
                                copied: false,
                                finishing: false,
                            })
                        }
                        Err(e) => self.show_error(e),
                    }
                }
                Msg::DeviceDone(res) => {
                    if self.cancel.load(Ordering::Relaxed) || self.dialog.is_none() {
                        continue;
                    }
                    match res {
                        Ok(token) => {
                            if let Some(SignIn::Device { finishing, .. }) = &mut self.dialog {
                                *finishing = true;
                            }
                            self.verify_and_store(token);
                        }
                        Err(e) => self.show_error(e),
                    }
                }
                Msg::Repos(res) => {
                    self.repos_loading = false;
                    if self.account.is_none() {
                        continue;
                    }
                    match res {
                        Ok(r) => {
                            self.repos = r;
                            self.repos_loaded = true;
                        }
                        Err(e) => self.repos_error = Some(e.to_string()),
                    }
                }
            }
        }
        std::mem::take(&mut self.notes)
    }

    // ------------------------------------------------------------------ sign-in dialog

    pub fn dialog_ui(&mut self, ctx: &egui::Context) {
        let Some(state) = &mut self.dialog else {
            return;
        };
        let mut close = false;
        let mut start_device = false;
        let mut submit_pat: Option<String> = None;
        let client_id = &mut self.client_id;

        egui::Modal::new(egui::Id::unique("github_signin")).show(ctx, |ui| {
            ui.set_width(480.0);
            let p = pal(ui);
            ui.horizontal(|ui| {
                ui.label(RichText::new(GITHUB_GLYPH.to_string()).size(30.0).color(p.text_strong));
                ui.vertical(|ui| {
                    ui.label(RichText::new("Connect GitHub").size(18.0).strong().color(p.text_strong));
                    ui.label(
                        RichText::new("Used for push · pull · fetch over HTTPS and to list the repositories you can clone.")
                            .weak(),
                    );
                });
            });
            ui.add_space(12.0);

            match state {
                SignIn::Start { pat, error, busy } => {
                    card(ui, |ui| {
                        ui.label(RichText::new("Sign in with your browser").strong().color(p.text_strong));
                        ui.label(
                            RichText::new("You get a one-time code to enter on github.com — no password is typed here.")
                                .small()
                                .weak(),
                        );
                        ui.add_space(8.0);
                        let has_id = !client_id.trim().is_empty();
                        ui.horizontal(|ui| {
                            if theme::primary_button(ui, &format!("{GITHUB_GLYPH}  Sign in with GitHub"), has_id && !*busy)
                                .clicked()
                            {
                                start_device = true;
                            }
                            if *busy {
                                ui.add(egui::Spinner::new());
                            }
                        });
                        ui.add_space(4.0);
                        egui::CollapsingHeader::new(RichText::new("OAuth App client ID").small())
                            .id_salt("gh_client_id")
                            .default_open(!has_id)
                            .show(ui, |ui| {
                                ui.add(
                                    egui::TextEdit::singleline(client_id)
                                        .hint_text("e.g. Ov23li…")
                                        .desired_width(f32::INFINITY),
                                );
                                ui.label(
                                    RichText::new(
                                        "One-time setup: register an OAuth App on GitHub (any homepage / callback URL, \
                                         e.g. http://localhost), tick \u{201C}Enable Device Flow\u{201D}, then paste its Client ID here.",
                                    )
                                    .small()
                                    .weak(),
                                );
                                if ui.link("Register a new OAuth App on GitHub").clicked() {
                                    ui.ctx().open_url(egui::OpenUrl::new_tab(github::NEW_OAUTH_APP_URL));
                                }
                            });
                    });
                    ui.add_space(8.0);
                    card(ui, |ui| {
                        ui.label(RichText::new("Or use a personal access token").strong().color(p.text_strong));
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            let r = ui.add(
                                egui::TextEdit::singleline(pat)
                                    .password(true)
                                    .hint_text("ghp_…  or  github_pat_…")
                                    .desired_width(ui.available_width() - 96.0),
                            );
                            let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                            let ok = !pat.trim().is_empty() && !*busy;
                            if (ui.add_enabled(ok, egui::Button::new("Connect").min_size(vec2(84.0, 0.0))).clicked()
                                || enter)
                                && ok
                            {
                                submit_pat = Some(pat.trim().to_string());
                                *busy = true;
                                *error = None;
                            }
                        });
                        ui.label(
                            RichText::new("A classic token with the repo, read:org and workflow scopes, or a fine-grained token.")
                                .small()
                                .weak(),
                        );
                        if ui.link("Create a token on GitHub").clicked() {
                            ui.ctx().open_url(egui::OpenUrl::new_tab(github::NEW_TOKEN_URL));
                        }
                    });
                    if let Some(e) = error {
                        ui.add_space(8.0);
                        ui.label(RichText::new(e.as_str()).color(p.red));
                    }
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                        ui.label(RichText::new("The token is stored in the system keychain.").small().weak());
                    });
                }
                SignIn::Device {
                    code,
                    started,
                    copied,
                    finishing,
                } => {
                    ui.label("Enter this code on GitHub:");
                    ui.add_space(6.0);
                    egui::Frame::new()
                        .fill(p.input)
                        .stroke(Stroke::new(1.0, p.border_strong))
                        .corner_radius(10.0)
                        .inner_margin(egui::Margin::symmetric(16, 12))
                        .show(ui, |ui| {
                            ui.vertical_centered(|ui| {
                                ui.label(
                                    RichText::new(&code.user_code)
                                        .monospace()
                                        .size(30.0)
                                        .strong()
                                        .color(p.text_strong),
                                );
                            });
                        });
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        if theme::primary_button(ui, "Copy code & open GitHub", !*finishing).clicked() {
                            ui.ctx().copy_text(code.user_code.clone());
                            ui.ctx().open_url(egui::OpenUrl::new_tab(&code.verification_uri));
                            *copied = true;
                        }
                        if ui.button("Copy code").clicked() {
                            ui.ctx().copy_text(code.user_code.clone());
                            *copied = true;
                        }
                    });
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new());
                        if *finishing {
                            ui.label(RichText::new("Approved — finishing sign-in…").weak());
                        } else {
                            let left = code.expires_in.saturating_sub(started.elapsed().as_secs());
                            ui.label(
                                RichText::new(format!(
                                    "Waiting for you to approve on GitHub…  ({}:{:02} left)",
                                    left / 60,
                                    left % 60
                                ))
                                .weak(),
                            );
                        }
                    });
                    if *copied {
                        ui.label(RichText::new("Code copied to the clipboard.").small().weak());
                    }
                    ui.add_space(12.0);
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                    ui.ctx().request_repaint_after(Duration::from_secs(1));
                }
            }
            if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                close = true;
            }
        });

        if close {
            self.cancel.store(true, Ordering::Relaxed);
            self.dialog = None;
        } else if start_device {
            self.start_device();
        } else if let Some(t) = submit_pat {
            self.verify_and_store(t);
        }
    }

    // ------------------------------------------------------------------ toolbar

    pub fn toolbar_button(&mut self, ui: &mut egui::Ui) -> Option<MenuAction> {
        let mut act = None;
        if self.restoring.is_some() {
            ui.add(egui::Spinner::new().size(14.0));
            return None;
        }
        let Some(a) = &self.account else {
            if theme::ghost_button(ui, &GITHUB_GLYPH.to_string(), "Sign in", 0, false)
                .on_hover_text("Connect a GitHub account")
                .clicked()
            {
                act = Some(MenuAction::SignIn);
            }
            return act;
        };
        let resp = account_chip(ui, a);
        let resp = resp.on_hover_text(match &a.problem {
            Some(pb) => format!("GitHub: {}\n{pb}", a.login),
            None => format!("Signed in to GitHub as {}", a.login),
        });
        egui::Popup::menu(&resp).show(|ui| {
            ui.set_min_width(230.0);
            let p = pal(ui);
            ui.add_space(2.0);
            ui.label(
                RichText::new(a.name.as_deref().unwrap_or(&a.login))
                    .strong()
                    .color(p.text_strong),
            );
            ui.label(
                RichText::new(format!("{GITHUB_GLYPH} {}", a.login))
                    .weak()
                    .small(),
            );
            if let Some(pb) = &a.problem {
                ui.label(RichText::new(pb).small().color(p.red));
            }
            ui.separator();
            if ui.button("Remote repositories").clicked() {
                act = Some(MenuAction::OpenRemote);
                ui.close();
            }
            if ui.button("Open profile on GitHub").clicked() {
                ui.ctx().open_url(egui::OpenUrl::new_tab(&a.html_url));
                ui.close();
            }
            if ui.button("Switch account…").clicked() {
                act = Some(MenuAction::SignIn);
                ui.close();
            }
            ui.separator();
            if ui.button("Sign out").clicked() {
                act = Some(MenuAction::SignOut);
                ui.close();
            }
        });
        act
    }

    // ------------------------------------------------------------------ remote page

    /// The "Remote" half of the Projects page.
    pub fn remote_page(
        &mut self,
        ui: &mut egui::Ui,
        query: &str,
        projects: &[Project],
    ) -> Vec<RemoteEvent> {
        let mut ev = Vec::new();
        let p = pal(ui);
        let Some(a) = &self.account else {
            if self.restoring.is_some() {
                ui.add_space(40.0);
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new());
                    ui.label(RichText::new("Restoring GitHub sign-in…").weak());
                });
                return ev;
            }
            ui.add_space(30.0);
            card(ui, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(10.0);
                    ui.label(RichText::new(GITHUB_GLYPH.to_string()).size(44.0).color(p.text_strong));
                    ui.add_space(6.0);
                    ui.label(RichText::new("Connect your GitHub account").size(17.0).strong().color(p.text_strong));
                    ui.add_space(2.0);
                    ui.label(
                        RichText::new("See every repository you can access — your own, your organizations' and ones you collaborate on — and clone with one click.")
                            .weak(),
                    );
                    ui.add_space(12.0);
                    if theme::primary_button(ui, &format!("{GITHUB_GLYPH}  Connect GitHub"), true).clicked() {
                        ev.push(RemoteEvent::SignIn);
                    }
                    ui.add_space(10.0);
                });
            });
            return ev;
        };

        if !self.repos_loaded && !self.repos_loading && self.repos_error.is_none() {
            let token = a.token.clone();
            self.repos_loading = true;
            self.spawn(move |_| Some(Msg::Repos(github::list_repos(&token))));
        }
        let local = self.local.get_or_insert_with(|| local_repo_map(projects));
        let login = a.login.clone();

        // Account header, like Sourcetree's "GitHub (user)" group
        let open_id = egui::Id::unique(("gh_remote_open", &login));
        let mut open = ui
            .ctx()
            .data_mut(|d| *d.get_persisted_mut_or(open_id, true));
        let q = query.trim().to_lowercase();
        let list: Vec<&GhRepo> = self
            .repos
            .iter()
            .filter(|r| {
                q.is_empty()
                    || r.full_name.to_lowercase().contains(&q)
                    || r.description
                        .as_deref()
                        .is_some_and(|d| d.to_lowercase().contains(&q))
            })
            .collect();
        let mut reload = false;
        {
            let (rect, resp) =
                ui.allocate_exact_size(vec2(ui.available_width(), 38.0), Sense::click());
            let painter = ui.painter();
            painter.rect_filled(
                rect,
                8.0,
                if resp.hovered() {
                    p.surface1
                } else {
                    p.surface0
                },
            );
            let cy = rect.center().y;
            theme::paint_caret(painter, pos2(rect.left() + 14.0, cy), open, p.subtext);
            let mut x = rect.left() + 28.0;
            let g = painter.layout_no_wrap(
                format!("{GITHUB_GLYPH}  GitHub"),
                FontId::proportional(14.5),
                p.text_strong,
            );
            let gw = g.size().x;
            painter.galley(pos2(x, cy - g.size().y / 2.0), g, p.text_strong);
            x += gw + 8.0;
            painter.text(
                pos2(x, cy),
                egui::Align2::LEFT_CENTER,
                format!("({login})"),
                FontId::proportional(13.0),
                p.subtext,
            );
            let count = if q.is_empty() {
                format!("{} repositories", self.repos.len())
            } else {
                format!("{} of {}", list.len(), self.repos.len())
            };
            painter.text(
                pos2(rect.right() - 44.0, cy),
                egui::Align2::RIGHT_CENTER,
                if self.repos_loading {
                    "Loading…".to_string()
                } else {
                    count
                },
                FontId::proportional(12.0),
                p.faint,
            );
            let btn = Rect::from_center_size(pos2(rect.right() - 20.0, cy), vec2(26.0, 26.0));
            let br = ui.interact(btn, ui.scope_id().with("gh_reload"), Sense::click());
            if self.repos_loading {
                egui::Spinner::new()
                    .size(14.0)
                    .paint_at(ui, btn.shrink(6.0));
            } else {
                if br.hovered() {
                    ui.painter().rect_filled(btn, 6.0, p.surface2);
                }
                ui.painter().text(
                    btn.center(),
                    egui::Align2::CENTER_CENTER,
                    "⟳",
                    FontId::proportional(15.0),
                    p.accent_hover,
                );
            }
            if br.on_hover_text("Reload the list from GitHub").clicked() {
                reload = true;
            } else if resp.clicked() {
                open = !open;
                ui.ctx().data_mut(|d| d.insert_persisted(open_id, open));
            }
        }
        if reload {
            self.repos_loaded = false;
            self.repos_error = None;
            self.local = None;
            return ev;
        }
        if let Some(e) = &self.repos_error {
            ui.add_space(8.0);
            ui.label(RichText::new(format!("Could not load repositories: {e}")).color(p.red));
            if ui.button("Try again").clicked() {
                self.repos_error = None;
            }
        }
        if !open {
            return ev;
        }
        ui.add_space(4.0);
        if self.repos_loaded && list.is_empty() {
            ui.add_space(20.0);
            ui.label(
                RichText::new(if q.is_empty() {
                    "No repositories found for this account."
                } else {
                    "No repository matches the filter."
                })
                .weak(),
            );
            if q.is_empty() {
                ui.label(
                    RichText::new("Organization repositories are hidden until the organization allows this OAuth App (Settings → Third-party access), or use a token with access to them.")
                        .small()
                        .weak(),
                );
            }
            return ev;
        }

        const ROW_H: f32 = 48.0;
        egui::ScrollArea::vertical()
            .id_salt("gh_repos")
            .auto_shrink(false)
            .show_rows(ui, ROW_H, list.len(), |ui, range| {
                ui.spacing_mut().item_spacing.y = 0.0;
                for i in range {
                    let r = list[i];
                    let local_path = local.get(&r.full_name.to_lowercase()).cloned();
                    repo_row(ui, r, local_path, ROW_H, &mut ev);
                }
            });
        ev
    }
}

/// Rounded panel used for grouped content in dialogs and empty states.
fn card(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    let p = pal(ui);
    egui::Frame::new()
        .fill(p.surface0)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(10.0)
        .inner_margin(egui::Margin::same(14))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        });
}

fn account_chip(ui: &mut egui::Ui, a: &Account) -> egui::Response {
    let p = pal(ui);
    let painter = ui.painter().clone();
    let g = painter.layout_no_wrap(a.login.clone(), FontId::proportional(13.0), p.text);
    let w = 8.0 + 20.0 + 7.0 + g.size().x + 8.0 + 10.0 + 6.0;
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 30.0), Sense::click());
    let open = egui::Popup::is_id_open(ui.ctx(), egui::Popup::default_response_id(&resp));
    if resp.hovered() || open {
        painter.rect_filled(rect, 7.0, p.surface1);
    }
    let cy = rect.center().y;
    let c = pos2(rect.left() + 8.0 + 10.0, cy);
    theme::paint_avatar(&painter, c, 10.0, &a.login);
    if a.problem.is_some() {
        painter.circle_filled(c + vec2(7.0, 7.0), 4.0, p.red);
        painter.circle_stroke(c + vec2(7.0, 7.0), 4.0, Stroke::new(1.5, p.mantle));
    }
    let x = c.x + 10.0 + 7.0;
    let gw = g.size().x;
    painter.galley(pos2(x, cy - g.size().y / 2.0), g, p.text);
    theme::paint_caret(&painter, pos2(x + gw + 12.0, cy), true, p.subtext);
    resp
}

fn repo_row(
    ui: &mut egui::Ui,
    r: &GhRepo,
    local: Option<String>,
    h: f32,
    ev: &mut Vec<RemoteEvent>,
) {
    let p = pal(ui);
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), h), Sense::click());
    if !ui.is_rect_visible(rect) {
        return;
    }
    let painter = ui.painter().clone();
    if resp.hovered() {
        painter.rect_filled(rect.shrink2(vec2(0.0, 1.0)), 8.0, p.hover);
    } else {
        painter.line_segment(
            [
                pos2(rect.left() + 14.0, rect.bottom() - 0.5),
                pos2(rect.right() - 14.0, rect.bottom() - 0.5),
            ],
            Stroke::new(1.0, p.border),
        );
    }

    // Right: Clone / Open
    let action = if local.is_some() { "Open" } else { "Clone" };
    let ag = painter.layout_no_wrap(
        action.into(),
        FontId::proportional(13.0),
        egui::Color32::PLACEHOLDER,
    );
    let bw = ag.size().x + 24.0;
    let btn = Rect::from_center_size(
        pos2(rect.right() - 14.0 - bw / 2.0, rect.center().y),
        vec2(bw, 28.0),
    );
    let b = ui.interact(
        btn,
        ui.scope_id().with(("gh_clone", &r.full_name)),
        Sense::click(),
    );
    let (fill, fg) = if local.is_some() {
        (
            if b.hovered() { p.surface2 } else { p.surface1 },
            p.text_strong,
        )
    } else if b.hovered() {
        (p.accent_hover, p.on_accent)
    } else {
        (theme::alpha(p.accent, 0.16), p.accent_hover)
    };
    painter.rect_filled(btn, 7.0, fill);
    painter.galley(
        pos2(
            btn.center().x - ag.size().x / 2.0,
            btn.center().y - ag.size().y / 2.0,
        ),
        ag,
        fg,
    );

    // Left: owner/name, chips, description
    let x0 = rect.left() + 16.0;
    let text_right = btn.left() - 16.0;
    let clip = Rect::from_min_max(pos2(x0, rect.top()), pos2(text_right, rect.bottom()))
        .intersect(ui.clip_rect());
    let tp = painter.with_clip_rect(clip);
    let (owner, name) = r
        .full_name
        .split_once('/')
        .unwrap_or(("", r.full_name.as_str()));
    let has_desc = r
        .description
        .as_deref()
        .is_some_and(|d| !d.trim().is_empty());
    let line1 = if has_desc {
        rect.top() + 16.0
    } else {
        rect.center().y
    };
    let og = tp.layout_no_wrap(format!("{owner}/"), FontId::proportional(14.0), p.subtext);
    let ow = og.size().x;
    tp.galley(pos2(x0, line1 - og.size().y / 2.0), og, p.subtext);
    let ng = tp.layout_no_wrap(name.to_string(), FontId::proportional(14.0), p.text_strong);
    let nw = ng.size().x;
    tp.galley(pos2(x0 + ow, line1 - ng.size().y / 2.0), ng, p.text_strong);
    let mut cx = x0 + ow + nw + 10.0;
    let mut chip = |text: &str, color: egui::Color32| {
        let g = tp.layout_no_wrap(text.to_string(), FontId::proportional(10.5), color);
        let cr = Rect::from_min_size(pos2(cx, line1 - 8.0), vec2(g.size().x + 12.0, 16.0));
        tp.rect_filled(cr, 8.0, theme::alpha(color, 0.14));
        tp.rect_stroke(
            cr,
            8.0,
            Stroke::new(1.0, theme::alpha(color, 0.35)),
            egui::StrokeKind::Inside,
        );
        let gh = g.size().y;
        tp.galley(pos2(cr.left() + 6.0, line1 - gh / 2.0), g, color);
        cx = cr.right() + 6.0;
    };
    if r.private {
        chip("Private", p.yellow);
    }
    if r.fork {
        chip("Fork", p.subtext);
    }
    if r.archived {
        chip("Archived", p.orange);
    }
    if let Some(d) = r.description.as_deref().filter(|d| !d.trim().is_empty()) {
        tp.text(
            pos2(x0, rect.top() + 34.0),
            egui::Align2::LEFT_CENTER,
            d.lines().next().unwrap_or(""),
            FontId::proportional(12.0),
            p.faint,
        );
    }

    let clone_https = RemoteEvent::Clone {
        url: r.clone_url.clone(),
        name: r.name.clone(),
    };
    if b.on_hover_text(match &local {
        Some(path) => format!("Open {path}"),
        None => format!("Clone {}", r.clone_url),
    })
    .clicked()
        || resp.double_clicked()
    {
        ev.push(match &local {
            Some(path) => RemoteEvent::Open(path.clone()),
            None => clone_https,
        });
    }
    resp.context_menu(|ui| {
        if let Some(path) = &local {
            if ui.button("Open").clicked() {
                ev.push(RemoteEvent::Open(path.clone()));
                ui.close();
            }
        }
        if ui.button("Clone (HTTPS)…").clicked() {
            ev.push(RemoteEvent::Clone {
                url: r.clone_url.clone(),
                name: r.name.clone(),
            });
            ui.close();
        }
        if ui.button("Clone (SSH)…").clicked() {
            ev.push(RemoteEvent::Clone {
                url: r.ssh_url.clone(),
                name: r.name.clone(),
            });
            ui.close();
        }
        ui.separator();
        if ui.button("Open on GitHub").clicked() {
            ui.ctx().open_url(egui::OpenUrl::new_tab(&r.html_url));
            ui.close();
        }
        if ui.button("Copy HTTPS URL").clicked() {
            ui.ctx().copy_text(r.clone_url.clone());
            ui.close();
        }
        if ui.button("Copy SSH URL").clicked() {
            ui.ctx().copy_text(r.ssh_url.clone());
            ui.close();
        }
    });
}

/// `owner/name` → path for every saved project whose remotes point at github.com.
fn local_repo_map(projects: &[Project]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for pr in projects {
        let Ok(repo) = git2::Repository::open(&pr.path) else {
            continue;
        };
        let Ok(names) = repo.remotes() else { continue };
        let mut names: Vec<String> = names.iter().flatten().flatten().map(String::from).collect();
        names.sort_by_key(|n| n != "origin");
        for n in names {
            if let Some(slug) = repo
                .find_remote(&n)
                .ok()
                .and_then(|r| r.url().ok().map(String::from))
                .and_then(|u| github::repo_slug(&u))
            {
                map.entry(slug).or_insert_with(|| pr.path.clone());
            }
        }
    }
    map
}
