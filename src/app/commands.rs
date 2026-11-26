//! What the command palette lists, and running a picked command.

use super::{push_dialog_for, Action, Dialog, FoxGitApp, View};
use crate::git::Op;
use crate::palette::{Cmd, Item};
use crate::tabs;
use eframe::egui;
use std::path::Path;

impl FoxGitApp {
    pub(super) fn palette_items(&mut self) -> Vec<Item> {
        let mut v = Vec::new();
        let in_repo = !self.tabs.is_empty() && !self.show_launcher;
        if in_repo {
            for (view, name) in [
                (View::FileStatus, "Changes"),
                (View::History, "History"),
                (View::GitHub, "GitHub — pull requests & issues"),
                (View::Search, "Search in files"),
                (View::Agents, "Agents"),
            ] {
                v.push(Item::new("Go to", name, Cmd::View(view)));
            }
            v.push(Item::new("Git", "Fetch", Cmd::Fetch));
            v.push(Item::new("Git", "Pull…", Cmd::Pull));
            v.push(Item::new("Git", "Push…", Cmd::Push));
            v.push(Item::new("Git", "New branch…", Cmd::NewBranch));
            v.push(Item::new("Git", "Stash changes…", Cmd::Stash));
            v.push(Item::new(
                "Git",
                "✨ Write commit message with AI",
                Cmd::AiMessage,
            ));
            v.push(Item::new("GitHub", "New pull request…", Cmd::NewPr));
            v.push(Item::new("Agents", "New agent chat…", Cmd::NewAgent));
            v.push(Item::new("Run", "New terminal", Cmd::NewTerminal));
            v.push(Item::new("Open", "Open in code editor", Cmd::OpenEditor));
            v.push(Item::new(
                "Open",
                "Open in Terminal app",
                Cmd::OpenTerminalApp,
            ));
            v.push(Item::new("Open", "Show in file manager", Cmd::Reveal));
            let tab = &mut self.tabs[self.active];
            for b in tab.refs.locals.iter().filter(|b| !b.is_head) {
                v.push(Item::new(
                    "Checkout",
                    b.name.clone(),
                    Cmd::Checkout(b.name.clone()),
                ));
            }
            let dir = tab.path.clone();
            for t in tab.run.tasks(&dir) {
                v.push(
                    Item::new(
                        "Run",
                        format!("▶ {}", t.label),
                        Cmd::Run(t.label.clone(), t.command.clone()),
                    )
                    .hint(t.command),
                );
            }
        }
        for (i, t) in self.tabs.iter().enumerate() {
            v.push(
                Item::new("Project", t.name.clone(), Cmd::SelectTab(i))
                    .hint(t.path.display().to_string()),
            );
        }
        for pr in tabs::sorted_projects(&self.projects).iter().take(40) {
            if !self
                .tabs
                .iter()
                .any(|t| t.path.display().to_string() == pr.path)
            {
                v.push(
                    Item::new("Open project", pr.name(), Cmd::OpenProject(pr.path.clone()))
                        .hint(pr.path.clone()),
                );
            }
        }
        v.push(Item::new(
            "FoxGit",
            "Dashboard — all projects and agents",
            Cmd::Dashboard,
        ));
        v.push(Item::new("FoxGit", "Projects…", Cmd::Projects));
        v.push(Item::new("FoxGit", "Settings…", Cmd::Settings));
        v.push(Item::new(
            "FoxGit",
            if self.dark {
                "Light theme"
            } else {
                "Dark theme"
            },
            Cmd::ToggleTheme,
        ));
        v
    }

    pub(super) fn run_cmd(&mut self, cmd: Cmd, ctx: &egui::Context, actions: &mut Vec<Action>) {
        // commands that don't need a repository
        match &cmd {
            Cmd::SelectTab(i) => return self.select_tab(*i),
            Cmd::OpenProject(p) => return self.open_repo(Path::new(p)),
            Cmd::Projects => return self.open_launcher(),
            Cmd::Dashboard => {
                self.open_launcher();
                self.launcher.mode = tabs::LauncherMode::Overview;
                return;
            }
            Cmd::Settings => {
                self.settings.open = true;
                return;
            }
            Cmd::ToggleTheme => {
                self.dark = !self.dark;
                self.apply_theme(ctx);
                return;
            }
            _ => {}
        }
        if self.show_launcher || self.tabs.is_empty() {
            return;
        }
        let i = self.active;
        match cmd {
            Cmd::View(v) => {
                self.tabs[i].view = v;
                if v == View::Search {
                    self.tabs[i].find.focus();
                }
            }
            Cmd::Fetch => actions.push(Action::Run(Op::Fetch)),
            Cmd::Pull => actions.push(Action::Dialog(Dialog::Pull { rebase: false })),
            Cmd::Push => actions.push(Action::Dialog(push_dialog_for(&self.tabs[i]))),
            Cmd::NewBranch => actions.push(Action::Dialog(Dialog::NewBranch {
                name: String::new(),
                start: None,
                start_label: "current HEAD".into(),
                checkout: true,
            })),
            Cmd::Stash => actions.push(Action::Dialog(Dialog::Stash {
                message: String::new(),
            })),
            Cmd::Checkout(b) => actions.push(Action::Run(Op::Checkout(b))),
            Cmd::Run(label, command) => {
                let dir = self.tabs[i].path.clone();
                if let Err(e) = self.tabs[i].run.start(&dir, ctx, &label, Some(&command)) {
                    self.notify(format!("Could not start a terminal: {e}"), false);
                }
            }
            Cmd::NewTerminal => {
                let dir = self.tabs[i].path.clone();
                if let Err(e) = self.tabs[i].run.start(&dir, ctx, "Terminal", None) {
                    self.notify(format!("Could not start a terminal: {e}"), false);
                }
            }
            Cmd::NewAgent => {
                self.tabs[i].view = View::Agents;
                actions.push(Action::NewAgentTask(String::new()));
            }
            Cmd::NewPr => {
                let info = self.hub_info(i);
                self.tabs[i].view = View::GitHub;
                if let Err(e) = self.hub.open_create_for_branch(&info) {
                    self.notify(e, false);
                }
            }
            Cmd::AiMessage => {
                self.tabs[i].view = View::FileStatus;
                actions.push(Action::AiCommitMessage);
            }
            Cmd::OpenEditor => {
                if let Err(e) = crate::desktop::open_in_editor(&self.tabs[i].path, None, None) {
                    self.notify(e, false);
                }
            }
            Cmd::OpenTerminalApp => {
                if let Err(e) = crate::desktop::open_terminal_app(&self.tabs[i].path) {
                    self.notify(e, false);
                }
            }
            Cmd::Reveal => tabs::reveal_in_file_manager(&self.tabs[i].path),
            _ => {}
        }
    }
}

#[cfg(debug_assertions)]
impl FoxGitApp {
    /// Dev only: `FOXGIT_DEMO=search:hello | run:dev | changes | palette | settings | dashboard`
    /// puts the window in that state once (screenshots without driving the UI).
    pub(super) fn demo_hook(&mut self, ctx: &egui::Context) {
        if self.demo_done || self.tabs.is_empty() {
            return;
        }
        self.demo_done = true;
        let Ok(v) = std::env::var("FOXGIT_DEMO") else {
            return;
        };
        for part in v.split(',') {
            let (k, arg) = part.split_once(':').unwrap_or((part, ""));
            let i = self.active;
            match k {
                "changes" => self.tabs[i].view = View::FileStatus,
                "history" => self.tabs[i].view = View::History,
                "github" => self.tabs[i].view = View::GitHub,
                "hubfake" => {
                    self.tabs[i].view = View::GitHub;
                    self.hub.demo_fill(&self.tabs[i].path.clone());
                    self.hub.demo_tab(&self.tabs[i].path.clone(), arg);
                    self.demo_token = true;
                }
                "agents" => self.tabs[i].view = View::Agents,
                "search" => {
                    self.tabs[i].view = View::Search;
                    self.tabs[i].find.search_now(arg);
                }
                "run" => {
                    let dir = self.tabs[i].path.clone();
                    let task = self.tabs[i]
                        .run
                        .tasks(&dir)
                        .into_iter()
                        .find(|t| t.label == arg);
                    let cmd = task.map(|t| t.command);
                    let _ = self.tabs[i].run.start(
                        &dir,
                        ctx,
                        if arg.is_empty() { "Terminal" } else { arg },
                        cmd.as_deref(),
                    );
                }
                "palette" => {
                    let items = self.palette_items();
                    self.palette.open(items);
                }
                "settings" => self.settings.open = true,
                "edit" => {
                    self.tabs[i].edit =
                        crate::editor::EditState::open(&self.tabs[i].path, arg).ok();
                }
                "newpr" => {
                    let mut acts = Vec::new();
                    self.run_cmd(Cmd::NewPr, ctx, &mut acts);
                }
                "dashboard" => {
                    self.open_launcher();
                    self.launcher.mode = tabs::LauncherMode::Overview;
                }
                "tab" => {
                    if let Ok(n) = arg.parse::<usize>() {
                        self.select_tab(n);
                    }
                }
                _ => {}
            }
        }
    }
}
