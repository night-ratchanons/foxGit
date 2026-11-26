//! GitHub accounts UI: personal-access-token sign-in (several accounts, one active), the toolbar
//! account menu and the "Remote" page that lists the repositories of every account
//! (Sourcetree-style) with Clone.

use crate::github::{self, ApiError, GhRepo, User};
use crate::tabs::Project;
use crate::theme::{self, pal};
use eframe::egui;
use egui::{pos2, vec2, FontId, Rect, RichText, Sense, Stroke};
use std::collections::{HashMap, HashSet};
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

#[derive(Default)]
struct RepoList {
    list: Vec<GhRepo>,
    loaded: bool,
    loading: bool,
    error: Option<String>,
}

enum Msg {
    /// Startup: token read back from the keychain.
    Restored {
        login: String,
        token: Result<String, String>,
    },
    /// Startup: result of checking a restored token.
    Checked {
        login: String,
        result: Result<User, ApiError>,
    },
    SignedIn {
        token: String,
        user: Result<User, ApiError>,
        keychain: Result<(), String>,
    },
    Repos {
        login: String,
        result: Result<Vec<GhRepo>, ApiError>,
    },
    Orgs {
        login: String,
        result: Result<Vec<String>, ApiError>,
    },
    Created {
        tag: u64,
        login: String,
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
}

pub enum RemoteEvent {
    Clone { url: String, name: String },
    Open(String),
    SignIn,
}

/// A row of the Remote page (one flat, uniformly tall list so it can be virtualized).
enum Row {
    Header(usize),
    Repo(usize, usize),
    Note(String),
    AddAccount,
}

pub struct GitHub {
    /// Connected accounts, in the user's order.
    accounts: Vec<Account>,
    /// Login of the active account (used for remotes without a user name in their URL).
    active: Option<String>,
    /// Saved order of logins (also those still being restored).
    order: Vec<String>,
    /// Logins still being read from the keychain at startup.
    restoring: HashSet<String>,
    repos: HashMap<String, RepoList>,
    orgs: HashMap<String, Vec<String>>,
    orgs_loading: HashSet<String>,
    dialog: Option<SignIn>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    ctx: egui::Context,
    /// `owner/name` → local project path, so known repositories show "Open" instead of "Clone".
    local: Option<HashMap<String, String>>,
    notes: Vec<(String, bool)>,
    created: Vec<(u64, Result<GhRepo, String>)>,
}

impl GitHub {
    pub fn new(ctx: &egui::Context, logins: Vec<String>, active: Option<String>) -> Self {
        let (tx, rx) = channel();
        let gh = Self {
            accounts: Vec::new(),
            active: active
                .filter(|a| logins.contains(a))
                .or_else(|| logins.first().cloned()),
            order: logins.clone(),
            restoring: logins.iter().cloned().collect(),
            repos: HashMap::new(),
            orgs: HashMap::new(),
            orgs_loading: HashSet::new(),
            dialog: None,
            tx,
            rx,
            ctx: ctx.clone(),
            local: None,
            notes: Vec::new(),
            created: Vec::new(),
        };
        for login in logins {
            // The keychain may show a system prompt: never do this on the UI thread.
            gh.spawn(move |tx| {
                let token = github::load_token(&login);
                let ok = token.clone().ok();
                let _ = tx.send(Msg::Restored {
                    login: login.clone(),
                    token,
                });
                ok.map(|t| Msg::Checked {
                    login,
                    result: github::fetch_user(&t),
                })
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

    /// Logins to persist, in order (also those still being restored).
    pub fn saved_logins(&self) -> Vec<String> {
        self.order.clone()
    }

    pub fn saved_active(&self) -> Option<String> {
        self.active.clone()
    }

    fn active_account(&self) -> Option<&Account> {
        self.active
            .as_ref()
            .and_then(|l| self.accounts.iter().find(|a| &a.login == l))
            .or_else(|| self.accounts.first())
    }

    /// Login of the active account.
    pub fn login(&self) -> Option<&str> {
        self.active_account().map(|a| a.login.as_str())
    }

    /// Every connected login, the active one first.
    pub fn logins(&self) -> Vec<String> {
        let active = self.login().map(String::from);
        let mut v: Vec<String> = active.iter().cloned().collect();
        v.extend(
            self.accounts
                .iter()
                .map(|a| a.login.clone())
                .filter(|l| Some(l) != active.as_ref()),
        );
        v
    }

    /// Tell git which tokens it may use (the active account answers URLs without a user name).
    fn sync_credentials(&self) {
        let active = self.login().map(String::from);
        let mut creds: Vec<(String, String)> = Vec::new();
        if let Some(a) = self.active_account() {
            creds.push((a.login.clone(), a.token.clone()));
        }
        creds.extend(
            self.accounts
                .iter()
                .filter(|a| Some(&a.login) != active.as_ref())
                .map(|a| (a.login.clone(), a.token.clone())),
        );
        github::set_git_credentials(creds);
    }

    pub fn invalidate_local(&mut self) {
        self.local = None;
    }

    /// Where a new repository can be created: the active account first, then its
    /// organizations (loaded in the background on first use).
    pub fn owners(&mut self) -> Vec<String> {
        let Some((login, token)) = self
            .active_account()
            .map(|a| (a.login.clone(), a.token.clone()))
        else {
            return Vec::new();
        };
        if !self.orgs.contains_key(&login) && !self.orgs_loading.contains(&login) {
            self.orgs_loading.insert(login.clone());
            let l = login.clone();
            self.spawn(move |_| {
                Some(Msg::Orgs {
                    login: l,
                    result: github::list_orgs(&token),
                })
            });
        }
        let mut v = vec![login.clone()];
        v.extend(self.orgs.get(&login).cloned().unwrap_or_default());
        v
    }

    /// Create an empty repository on GitHub with the active account; the result (with the
    /// account's user name in its clone URL) comes back from [`Self::take_created`].
    pub fn create_repo(
        &mut self,
        tag: u64,
        owner: &str,
        name: String,
        description: String,
        private: bool,
    ) {
        let Some(a) = self.active_account() else {
            self.created
                .push((tag, Err("Not connected to GitHub".into())));
            return;
        };
        let (token, login) = (a.token.clone(), a.login.clone());
        let org = (owner != login).then(|| owner.to_string());
        self.spawn(move |_| {
            let result = github::create_repo(&token, org.as_deref(), &name, &description, private);
            Some(Msg::Created { tag, login, result })
        });
    }

    pub fn take_created(&mut self) -> Vec<(u64, Result<GhRepo, String>)> {
        std::mem::take(&mut self.created)
    }

    pub fn open_signin(&mut self) {
        self.dialog = Some(SignIn {
            token: String::new(),
            error: None,
            busy: false,
        });
    }

    pub fn switch_to(&mut self, login: &str) {
        if self.accounts.iter().any(|a| a.login == login) {
            self.active = Some(login.to_string());
            self.sync_credentials();
            self.notes.push((format!("Using {login} for GitHub"), true));
        }
    }

    pub fn sign_out(&mut self, login: &str) {
        if let Some(i) = self.accounts.iter().position(|a| a.login == login) {
            let a = self.accounts.remove(i);
            self.spawn(move |_| {
                github::delete_token(&a.login);
                None
            });
        }
        self.order.retain(|l| l != login);
        self.repos.remove(login);
        self.orgs.remove(login);
        if self.active.as_deref() == Some(login) {
            self.active = self.accounts.first().map(|a| a.login.clone());
        }
        self.sync_credentials();
        self.local = None;
        self.notes
            .push((format!("Signed out of GitHub account {login}"), true));
    }

    fn reload_repos(&mut self, login: &str) {
        let Some(a) = self.accounts.iter().find(|a| a.login == login) else {
            return;
        };
        let entry = self.repos.entry(login.to_string()).or_default();
        if entry.loading {
            return;
        }
        entry.loading = true;
        entry.error = None;
        let (token, login) = (a.token.clone(), a.login.clone());
        self.spawn(move |_| {
            Some(Msg::Repos {
                result: github::list_repos(&token),
                login,
            })
        });
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

    /// Add or update an account and keep `accounts` in the saved order.
    fn upsert(&mut self, a: Account) {
        if !self.order.contains(&a.login) {
            self.order.push(a.login.clone());
        }
        match self.accounts.iter_mut().find(|x| x.login == a.login) {
            Some(x) => *x = a,
            None => self.accounts.push(a),
        }
        let order = &self.order;
        self.accounts.sort_by_key(|x| {
            order
                .iter()
                .position(|l| l == &x.login)
                .unwrap_or(usize::MAX)
        });
    }

    /// Handle background results. Returns toasts to show (text, ok).
    pub fn poll(&mut self) -> Vec<(String, bool)> {
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::Restored { login, token } => {
                    self.restoring.remove(&login);
                    match token {
                        Ok(token) => {
                            self.upsert(Account {
                                html_url: format!("https://github.com/{login}"),
                                login,
                                name: None,
                                token,
                                problem: None,
                            });
                            self.sync_credentials();
                        }
                        Err(e) => self.notes.push((
                            format!("GitHub: the saved token of {login} could not be read from the keychain ({e}). Connect it again."),
                            false,
                        )),
                    }
                }
                Msg::Checked { login, result } => {
                    if let Some(a) = self.accounts.iter_mut().find(|a| a.login == login) {
                        match result {
                            Ok(u) => {
                                a.name = u.name;
                                if !u.html_url.is_empty() {
                                    a.html_url = u.html_url;
                                }
                                a.problem = None;
                            }
                            Err(ApiError::Auth(m)) => {
                                self.notes.push((
                                    format!(
                                        "GitHub ({login}): {m}. Connect again with a new token."
                                    ),
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
                        let login = u.login.clone();
                        let existed = self.accounts.iter().any(|a| a.login == login);
                        self.upsert(Account {
                            login: u.login,
                            name: u.name,
                            html_url: u.html_url,
                            token,
                            problem: None,
                        });
                        self.active = Some(login.clone());
                        self.sync_credentials();
                        self.repos.remove(&login);
                        self.local = None;
                        self.notes.push((
                            if existed {
                                format!("Updated the token of {login} — now using it for GitHub")
                            } else {
                                format!("Connected {login} — now using it for GitHub")
                            },
                            true,
                        ));
                        if let Err(e) = keychain {
                            self.notes.push((
                                format!("The token could not be saved to the keychain ({e}) — you will need to paste it again next time"),
                                false,
                            ));
                        }
                    }
                    Err(e) => {
                        if let Some(d) = &mut self.dialog {
                            d.busy = false;
                            d.error = Some(e.to_string());
                        }
                    }
                },
                Msg::Orgs { login, result } => {
                    self.orgs_loading.remove(&login);
                    self.orgs.insert(login, result.unwrap_or_default());
                }
                Msg::Created { tag, login, result } => {
                    let result = result.map(|mut r| {
                        // remember which account this repository belongs to
                        r.clone_url = github::url_with_user(&r.clone_url, Some(&login));
                        r
                    });
                    if result.is_ok() {
                        // the new repository should show up on the Remote page
                        self.repos.remove(&login);
                        self.local = None;
                    }
                    self.created.push((tag, result.map_err(|e| e.to_string())));
                }
                Msg::Repos { login, result } => {
                    if !self.accounts.iter().any(|a| a.login == login) {
                        continue;
                    }
                    let entry = self.repos.entry(login).or_default();
                    entry.loading = false;
                    match result {
                        Ok(r) => {
                            entry.list = r;
                            entry.loaded = true;
                        }
                        Err(e) => entry.error = Some(e.to_string()),
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
        let has_accounts = !self.accounts.is_empty();

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
                        RichText::new(if has_accounts {
                            "Add a GitHub account"
                        } else {
                            "Connect GitHub"
                        })
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
        let Some(a) = self.active_account() else {
            if !self.restoring.is_empty() {
                ui.add(egui::Spinner::new().size(14.0));
                return None;
            }
            if theme::ghost_button(ui, &GITHUB_GLYPH.to_string(), "Sign in", 0, false)
                .on_hover_text("Connect a GitHub account")
                .clicked()
            {
                act = Some(MenuAction::SignIn);
            }
            return act;
        };
        let resp = account_chip(ui, a, self.accounts.len());
        let resp = resp.on_hover_text(match &a.problem {
            Some(pb) => format!("GitHub: {}\n{pb}", a.login),
            None => format!(
                "Using {} for GitHub{}",
                a.login,
                if self.accounts.len() > 1 {
                    "\nClick to switch account"
                } else {
                    ""
                }
            ),
        });
        let active = a.login.clone();
        let html_url = a.html_url.clone();
        let mut switch: Option<String> = None;
        let mut sign_out = false;
        egui::Popup::menu(&resp).show(|ui| {
            ui.set_min_width(260.0);
            let p = pal(ui);
            ui.label(RichText::new("GITHUB ACCOUNTS").small().color(p.faint));
            for acc in &self.accounts {
                let is_active = acc.login == active;
                let r = account_menu_row(ui, acc, is_active);
                if r.clicked() && !is_active {
                    switch = Some(acc.login.clone());
                    ui.close();
                }
            }
            if ui.button("✚  Add another account…").clicked() {
                act = Some(MenuAction::SignIn);
                ui.close();
            }
            ui.separator();
            if ui.button("Remote repositories").clicked() {
                act = Some(MenuAction::OpenRemote);
                ui.close();
            }
            if ui.button(format!("Open {active} on GitHub")).clicked() {
                ui.ctx().open_url(egui::OpenUrl::new_tab(&html_url));
                ui.close();
            }
            ui.separator();
            if ui.button(format!("Sign out of {active}")).clicked() {
                sign_out = true;
                ui.close();
            }
        });
        if let Some(l) = switch {
            self.switch_to(&l);
        }
        if sign_out {
            self.sign_out(&active);
        }
        act
    }

    // ------------------------------------------------------------------ remote page

    /// The "Remote" half of the Projects page: one group per connected account.
    pub fn remote_page(
        &mut self,
        ui: &mut egui::Ui,
        query: &str,
        projects: &[Project],
    ) -> Vec<RemoteEvent> {
        let mut ev = Vec::new();
        let p = pal(ui);
        if self.accounts.is_empty() {
            if !self.restoring.is_empty() {
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
        }

        let open_id = |login: &str| egui::Id::unique(("gh_remote_open", login));
        let is_open = |ui: &egui::Ui, login: &str| {
            ui.ctx()
                .data_mut(|d| *d.get_persisted_mut_or(open_id(login), true))
        };
        // load the repositories of every expanded account
        let logins: Vec<String> = self.accounts.iter().map(|a| a.login.clone()).collect();
        for l in &logins {
            let st = self.repos.get(l);
            if is_open(ui, l) && st.is_none_or(|s| !s.loaded && !s.loading && s.error.is_none()) {
                self.reload_repos(l);
            }
        }

        let q = query.trim().to_lowercase();
        let matches = |r: &GhRepo| {
            q.is_empty()
                || r.full_name.to_lowercase().contains(&q)
                || r.description
                    .as_deref()
                    .is_some_and(|d| d.to_lowercase().contains(&q))
        };
        let mut rows = Vec::new();
        for (ai, acc) in self.accounts.iter().enumerate() {
            rows.push(Row::Header(ai));
            if !is_open(ui, &acc.login) {
                continue;
            }
            let st = self.repos.get(&acc.login);
            match st {
                Some(s) if s.error.is_some() => rows.push(Row::Note(format!(
                    "Could not load repositories: {} — click ⟳ to retry",
                    s.error.as_deref().unwrap_or("")
                ))),
                Some(s) if s.loaded => {
                    let before = rows.len();
                    rows.extend(
                        s.list
                            .iter()
                            .enumerate()
                            .filter(|(_, r)| matches(r))
                            .map(|(i, _)| Row::Repo(ai, i)),
                    );
                    if rows.len() == before {
                        rows.push(Row::Note(if q.is_empty() {
                                "No repositories — organization repositories only show up if the token can access them.".into()
                            } else {
                                "No repository matches the filter.".into()
                            },
                        ));
                    }
                }
                _ => rows.push(Row::Note("Loading repositories…".into())),
            }
        }
        rows.push(Row::AddAccount);

        let local = self.local.get_or_insert_with(|| local_repo_map(projects));
        let accounts = &self.accounts;
        let repos = &self.repos;
        let active = self.active.clone();
        let mut reload: Option<String> = None;
        const ROW_H: f32 = 48.0;
        egui::ScrollArea::vertical()
            .id_salt("gh_repos")
            .auto_shrink(false)
            .show_rows(ui, ROW_H, rows.len(), |ui, range| {
                ui.spacing_mut().item_spacing.y = 0.0;
                for row in &rows[range] {
                    match row {
                        Row::Header(ai) => {
                            let acc = &accounts[*ai];
                            let st = repos.get(&acc.login);
                            let (count, loading) = match st {
                                Some(s) if s.loaded => {
                                    let n = s.list.iter().filter(|r| matches(r)).count();
                                    (
                                        if q.is_empty() {
                                            format!("{} repositories", s.list.len())
                                        } else {
                                            format!("{n} of {}", s.list.len())
                                        },
                                        s.loading,
                                    )
                                }
                                Some(s) => (String::new(), s.loading),
                                None => (String::new(), false),
                            };
                            let open = is_open(ui, &acc.login);
                            let is_active = active.as_deref() == Some(acc.login.as_str())
                                || (active.is_none() && *ai == 0);
                            let (toggle, again) =
                                group_header(ui, acc, open, is_active, &count, loading, ROW_H);
                            if toggle {
                                ui.ctx()
                                    .data_mut(|d| d.insert_persisted(open_id(&acc.login), !open));
                            }
                            if again {
                                reload = Some(acc.login.clone());
                            }
                        }
                        Row::Repo(ai, ri) => {
                            let acc = &accounts[*ai];
                            if let Some(r) = repos.get(&acc.login).and_then(|s| s.list.get(*ri)) {
                                let local_path = local.get(&r.full_name.to_lowercase()).cloned();
                                repo_row(ui, r, local_path, &acc.login, ROW_H, &mut ev);
                            }
                        }
                        Row::Note(text) => {
                            let (rect, _) = ui.allocate_exact_size(
                                vec2(ui.available_width(), ROW_H),
                                Sense::hover(),
                            );
                            ui.painter().with_clip_rect(rect).text(
                                pos2(rect.left() + 18.0, rect.center().y),
                                egui::Align2::LEFT_CENTER,
                                text,
                                FontId::proportional(12.5),
                                pal(ui).subtext,
                            );
                        }
                        Row::AddAccount => {
                            let (rect, resp) = ui.allocate_exact_size(
                                vec2(ui.available_width(), ROW_H),
                                Sense::click(),
                            );
                            let pl = pal(ui);
                            if resp.hovered() {
                                ui.painter().rect_filled(
                                    rect.shrink2(vec2(0.0, 6.0)),
                                    8.0,
                                    pl.hover,
                                );
                            }
                            ui.painter().text(
                                pos2(rect.left() + 18.0, rect.center().y),
                                egui::Align2::LEFT_CENTER,
                                "✚  Add another GitHub account",
                                FontId::proportional(13.0),
                                pl.accent_hover,
                            );
                            if resp.clicked() {
                                ev.push(RemoteEvent::SignIn);
                            }
                        }
                    }
                }
            });
        if let Some(l) = reload {
            self.repos.remove(&l);
            self.local = None;
        }
        ev
    }
}

/// "GitHub (login)" group header on the Remote page. Returns (toggle open, reload).
fn group_header(
    ui: &mut egui::Ui,
    acc: &Account,
    open: bool,
    is_active: bool,
    count: &str,
    loading: bool,
    h: f32,
) -> (bool, bool) {
    let p = pal(ui);
    let (outer, resp) = ui.allocate_exact_size(vec2(ui.available_width(), h), Sense::click());
    let rect = outer.shrink2(vec2(0.0, 4.0));
    let painter = ui.painter().clone();
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
    theme::paint_caret(&painter, pos2(rect.left() + 14.0, cy), open, p.subtext);
    theme::paint_avatar(&painter, pos2(rect.left() + 38.0, cy), 10.0, &acc.login);
    let mut x = rect.left() + 56.0;
    let g = painter.layout_no_wrap(acc.login.clone(), FontId::proportional(14.5), p.text_strong);
    let gw = g.size().x;
    painter.galley(pos2(x, cy - g.size().y / 2.0), g, p.text_strong);
    x += gw + 10.0;
    if is_active {
        let w = theme::paint_pill(
            &painter,
            x,
            cy,
            "active",
            theme::alpha(p.green, 0.18),
            p.green,
        );
        x += w + 8.0;
    }
    if let Some(name) = &acc.name {
        painter.text(
            pos2(x, cy),
            egui::Align2::LEFT_CENTER,
            name,
            FontId::proportional(12.5),
            p.faint,
        );
    }
    painter.text(
        pos2(rect.right() - 44.0, cy),
        egui::Align2::RIGHT_CENTER,
        if loading { "Loading…" } else { count },
        FontId::proportional(12.0),
        p.faint,
    );
    let btn = Rect::from_center_size(pos2(rect.right() - 20.0, cy), vec2(26.0, 26.0));
    let br = ui.interact(
        btn,
        ui.scope_id().with(("gh_reload", &acc.login)),
        Sense::click(),
    );
    if loading {
        egui::Spinner::new()
            .size(14.0)
            .paint_at(ui, btn.shrink(6.0));
    } else {
        if br.hovered() {
            painter.rect_filled(btn, 6.0, p.surface2);
        }
        painter.text(
            btn.center(),
            egui::Align2::CENTER_CENTER,
            "⟳",
            FontId::proportional(15.0),
            p.accent_hover,
        );
    }
    let again = br.on_hover_text("Reload the list from GitHub").clicked();
    (!again && resp.clicked(), again)
}

/// One account in the toolbar account menu (click to switch).
fn account_menu_row(ui: &mut egui::Ui, acc: &Account, active: bool) -> egui::Response {
    let p = pal(ui);
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 40.0), Sense::click());
    let painter = ui.painter();
    if active {
        painter.rect_filled(rect, 6.0, p.selection);
    } else if resp.hovered() {
        painter.rect_filled(rect, 6.0, p.hover);
    }
    let cy = rect.center().y;
    theme::paint_avatar(painter, pos2(rect.left() + 18.0, cy), 11.0, &acc.login);
    let x = rect.left() + 38.0;
    let (l1, l2) = match &acc.name {
        Some(n) => (cy - 8.0, Some(n.as_str())),
        None => (cy, None),
    };
    painter.text(
        pos2(x, l1),
        egui::Align2::LEFT_CENTER,
        &acc.login,
        FontId::proportional(13.5),
        p.text_strong,
    );
    if let Some(n) = l2 {
        painter.text(
            pos2(x, cy + 8.0),
            egui::Align2::LEFT_CENTER,
            n,
            FontId::proportional(11.5),
            p.faint,
        );
    }
    if active {
        painter.text(
            pos2(rect.right() - 12.0, cy),
            egui::Align2::RIGHT_CENTER,
            "✔",
            FontId::proportional(13.0),
            p.green,
        );
    } else if acc.problem.is_some() {
        painter.text(
            pos2(rect.right() - 12.0, cy),
            egui::Align2::RIGHT_CENTER,
            "⚠",
            FontId::proportional(13.0),
            p.red,
        );
    }
    resp.on_hover_text(match (&acc.problem, active) {
        (Some(pb), _) => pb.clone(),
        (None, true) => "Active account".into(),
        (None, false) => format!("Switch to {}", acc.login),
    })
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

fn account_chip(ui: &mut egui::Ui, a: &Account, n_accounts: usize) -> egui::Response {
    let p = pal(ui);
    let painter = ui.painter().clone();
    let g = painter.layout_no_wrap(a.login.clone(), FontId::proportional(13.0), p.text);
    let extra = if n_accounts > 1 {
        6.0 + theme::pill_width(&painter, &n_accounts.to_string())
    } else {
        0.0
    };
    let w = 8.0 + 20.0 + 7.0 + g.size().x + extra + 8.0 + 10.0 + 6.0;
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
    let mut x = c.x + 10.0 + 7.0;
    let gw = g.size().x;
    painter.galley(pos2(x, cy - g.size().y / 2.0), g, p.text);
    x += gw;
    if n_accounts > 1 {
        x += 6.0;
        x += theme::paint_pill(
            &painter,
            x,
            cy,
            &n_accounts.to_string(),
            p.surface2,
            p.subtext,
        );
    }
    theme::paint_caret(&painter, pos2(x + 12.0, cy), true, p.subtext);
    resp
}

fn repo_row(
    ui: &mut egui::Ui,
    r: &GhRepo,
    local: Option<String>,
    login: &str,
    h: f32,
    ev: &mut Vec<RemoteEvent>,
) {
    // HTTPS clones carry the account in the URL, so git later uses that account's token.
    let https = github::url_with_user(&r.clone_url, Some(login));
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
        url: https.clone(),
        name: r.name.clone(),
    };
    if b.on_hover_text(match &local {
        Some(path) => format!("Open {path}"),
        None => format!("Clone {} as {login}", r.clone_url),
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
        if ui.button(format!("Clone (HTTPS, as {login})…")).clicked() {
            ev.push(RemoteEvent::Clone {
                url: https.clone(),
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
