//! GitHub account UI: personal-access-token sign-in, the toolbar account menu and the "Remote"
//! page that lists every repository the account can access (Sourcetree-style) with Clone.

use crate::github::{self, ApiError, GhRepo, User};
use crate::tabs::Project;
use crate::theme::{self, pal};
use eframe::egui;
use egui::{pos2, vec2, FontId, Rect, RichText, Sense, Stroke};
use std::collections::HashMap;
use std::sync::mpsc::{channel, Receiver, Sender};

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
    Repos(Result<Vec<GhRepo>, ApiError>),
    Orgs(Result<Vec<String>, ApiError>),
    Created {
        tag: u64,
        result: Result<GhRepo, ApiError>,
    },
}

/// State of the "Connect GitHub" dialog.
struct SignIn {
    token: String,
    error: Option<String>,
    busy: bool,
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
    repos: Vec<GhRepo>,
    repos_loaded: bool,
    repos_loading: bool,
    repos_error: Option<String>,
    dialog: Option<SignIn>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    ctx: egui::Context,
    /// `owner/name` → local project path, so known repositories show "Open" instead of "Clone".
    local: Option<HashMap<String, String>>,
    notes: Vec<(String, bool)>,
    orgs: Option<Vec<String>>,
    orgs_loading: bool,
    created: Vec<(u64, Result<GhRepo, String>)>,
}

impl GitHub {
    pub fn new(ctx: &egui::Context, login: Option<String>) -> Self {
        let (tx, rx) = channel();
        let gh = Self {
            account: None,
            restoring: login.clone(),
            repos: Vec::new(),
            repos_loaded: false,
            repos_loading: false,
            repos_error: None,
            dialog: None,
            tx,
            rx,
            ctx: ctx.clone(),
            local: None,
            notes: Vec::new(),
            orgs: None,
            orgs_loading: false,
            created: Vec::new(),
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

    pub fn invalidate_local(&mut self) {
        self.local = None;
    }

    pub fn login(&self) -> Option<&str> {
        self.account.as_ref().map(|a| a.login.as_str())
    }

    /// Where a new repository can be created: the account itself first, then its
    /// organizations (loaded in the background on first use).
    pub fn owners(&mut self) -> Vec<String> {
        let Some(a) = &self.account else {
            return Vec::new();
        };
        if self.orgs.is_none() && !self.orgs_loading {
            self.orgs_loading = true;
            let token = a.token.clone();
            self.spawn(move |_| Some(Msg::Orgs(github::list_orgs(&token))));
        }
        let mut v = vec![a.login.clone()];
        v.extend(self.orgs.clone().unwrap_or_default());
        v
    }

    /// Create an empty repository on GitHub; the result comes back from [`Self::take_created`].
    pub fn create_repo(
        &mut self,
        tag: u64,
        owner: &str,
        name: String,
        description: String,
        private: bool,
    ) {
        let Some(a) = &self.account else {
            self.created
                .push((tag, Err("Not connected to GitHub".into())));
            return;
        };
        let token = a.token.clone();
        let org = (owner != a.login).then(|| owner.to_string());
        self.spawn(move |_| {
            let result = github::create_repo(&token, org.as_deref(), &name, &description, private);
            Some(Msg::Created { tag, result })
        });
    }

    pub fn take_created(&mut self) -> Vec<(u64, Result<GhRepo, String>)> {
        std::mem::take(&mut self.created)
    }

    fn set_account(&mut self, a: Account) {
        github::set_git_credential(Some((a.login.clone(), a.token.clone())));
        self.account = Some(a);
        self.orgs = None;
        self.repos.clear();
        self.repos_loaded = false;
        self.repos_error = None;
    }

    pub fn open_signin(&mut self) {
        self.dialog = Some(SignIn {
            token: String::new(),
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

    /// Check the token with GitHub, then keep it in the keychain.
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
                            format!("GitHub: the saved token could not be read from the keychain ({e}). Connect again."),
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
                                self.notes.push((
                                    format!("GitHub: {m}. Connect again with a new token."),
                                    false,
                                ));
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
                            .push((format!("Connected to GitHub as {login}"), true));
                        if let Err(e) = keychain {
                            self.notes.push((
                                format!("The token could not be saved to the keychain ({e}) — you will need to paste it again next time"),
                                false,
                            ));
                        }
                        self.reload_repos();
                    }
                    Err(e) => {
                        if let Some(d) = &mut self.dialog {
                            d.busy = false;
                            d.error = Some(e.to_string());
                        }
                    }
                },
                Msg::Orgs(res) => {
                    self.orgs_loading = false;
                    self.orgs = Some(res.unwrap_or_default());
                }
                Msg::Created { tag, result } => {
                    if result.is_ok() {
                        // the new repository should show up on the Remote page
                        self.repos_loaded = false;
                        self.local = None;
                    }
                    self.created.push((tag, result.map_err(|e| e.to_string())));
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
        let Some(d) = &mut self.dialog else {
            return;
        };
        let mut close = false;
        let mut submit: Option<String> = None;

        egui::Modal::new(egui::Id::unique("github_signin")).show(ctx, |ui| {
            ui.set_width(480.0);
            let p = pal(ui);
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(GITHUB_GLYPH.to_string())
                        .size(30.0)
                        .color(p.text_strong),
                );
                ui.vertical(|ui| {
                    ui.label(
                        RichText::new("Connect GitHub")
                            .size(18.0)
                            .strong()
                            .color(p.text_strong),
                    );
                    ui.label(
                        RichText::new(
                            "Paste a personal access token. FoxGit uses it for push · pull · fetch \
                             over HTTPS and to list the repositories you can clone.",
                        )
                        .weak(),
                    );
                });
            });
            ui.add_space(12.0);

            card(ui, |ui| {
                step(ui, "1", "Create a token on GitHub");
                ui.horizontal(|ui| {
                    ui.add_space(28.0);
                    if ui.link("Classic token").clicked() {
                        ui.ctx()
                            .open_url(egui::OpenUrl::new_tab(github::NEW_TOKEN_URL));
                    }
                    ui.label(RichText::new("·").weak());
                    if ui.link("Fine-grained token").clicked() {
                        ui.ctx()
                            .open_url(egui::OpenUrl::new_tab(github::NEW_FINE_GRAINED_TOKEN_URL));
                    }
                });
                ui.horizontal(|ui| {
                    ui.add_space(28.0);
                    ui.add(
                        egui::Label::new(
                            RichText::new(
                                "Classic: the link pre-ticks repo, read:org and workflow. \
                                 Fine-grained: pick the repositories (or organization) and allow \
                                 Contents: Read and write.",
                            )
                            .small()
                            .weak(),
                        )
                        .wrap(),
                    );
                });
                ui.add_space(8.0);
                step(ui, "2", "Paste it here");
                ui.horizontal(|ui| {
                    ui.add_space(28.0);
                    let r = ui.add(
                        egui::TextEdit::singleline(&mut d.token)
                            .password(true)
                            .hint_text("ghp_…  or  github_pat_…")
                            .desired_width(ui.available_width() - 96.0),
                    );
                    if !d.busy && d.token.is_empty() && !r.has_focus() {
                        r.request_focus();
                    }
                    let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    let ok = !d.token.trim().is_empty() && !d.busy;
                    if (theme::primary_button(ui, "Connect", ok).clicked() || enter) && ok {
                        submit = Some(d.token.trim().to_string());
                        d.busy = true;
                        d.error = None;
                    }
                });
                if d.busy {
                    ui.horizontal(|ui| {
                        ui.add_space(28.0);
                        ui.add(egui::Spinner::new().size(14.0));
                        ui.label(RichText::new("Checking the token with GitHub…").weak());
                    });
                }
            });
            if let Some(e) = &d.error {
                ui.add_space(8.0);
                ui.label(RichText::new(e.as_str()).color(p.red));
            }
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                if ui.button("Cancel").clicked() {
                    close = true;
                }
                ui.label(
                    RichText::new("The token is stored in the system keychain, never in a file.")
                        .small()
                        .weak(),
                );
            });
            if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                close = true;
            }
        });

        if close {
            self.dialog = None;
        } else if let Some(t) = submit {
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
            if ui.button("Use a different token…").clicked() {
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
                    if theme::primary_button(ui, &format!("{GITHUB_GLYPH}  Connect with a token"), true)
                        .clicked()
                    {
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
                    RichText::new("Organization repositories only show up if the token can access them: for a fine-grained token pick the organization as resource owner; for a classic token on an SSO organization click Configure SSO → Authorize next to the token.")
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

/// A numbered step heading in the sign-in dialog.
fn step(ui: &mut egui::Ui, n: &str, title: &str) {
    let p = pal(ui);
    ui.horizontal(|ui| {
        let (r, _) = ui.allocate_exact_size(vec2(20.0, 20.0), Sense::hover());
        ui.painter()
            .circle_filled(r.center(), 10.0, theme::alpha(p.accent, 0.22));
        ui.painter().text(
            r.center(),
            egui::Align2::CENTER_CENTER,
            n,
            FontId::proportional(11.5),
            p.accent_hover,
        );
        ui.add_space(4.0);
        ui.label(RichText::new(title).strong().color(p.text_strong));
    });
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
