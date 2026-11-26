//! Settings window: appearance, editor, terminal, agents, auto-fetch, and the git identity of the
//! current repository.

use super::{Action, FoxGitApp};
use crate::agents::AgentKind;
use crate::git::Op;
use crate::theme::{self, pal};
use eframe::egui;
use egui::RichText;
use std::path::PathBuf;

#[derive(Default)]
pub struct SettingsState {
    pub open: bool,
    /// Repository the identity fields were read from.
    ident_for: Option<PathBuf>,
    name: String,
    email: String,
    /// What applies when the repository sets nothing (global config).
    inherited: (String, String),
    new_profile: String,
    new_profile_kind: Option<AgentKind>,
    snip_name: String,
    snip_text: String,
}

fn read_identity(repo: &git2::Repository) -> (String, String, (String, String)) {
    let get = |c: &git2::Config, k: &str| c.get_string(k).unwrap_or_default();
    let local = repo
        .config()
        .and_then(|c| c.open_level(git2::ConfigLevel::Local))
        .ok();
    let global = git2::Config::open_default().ok();
    let (n, e) = local
        .as_ref()
        .map(|c| (get(c, "user.name"), get(c, "user.email")))
        .unwrap_or_default();
    let inh = global
        .as_ref()
        .map(|c| (get(c, "user.name"), get(c, "user.email")))
        .unwrap_or_default();
    (n, e, inh)
}

fn global_hint(v: &str) -> String {
    if v.is_empty() {
        "not set".into()
    } else {
        format!("{v} (global)")
    }
}

impl FoxGitApp {
    pub(super) fn settings_window(&mut self, ctx: &egui::Context, actions: &mut Vec<Action>) {
        if !self.settings.open {
            return;
        }
        let mut open = true;
        let tab_path = (!self.show_launcher)
            .then(|| self.tabs.get(self.active).map(|t| t.path.clone()))
            .flatten();
        if self.settings.ident_for != tab_path {
            self.settings.ident_for = tab_path.clone();
            if let Some(tab) = self.tabs.get(self.active) {
                let (n, e, inh) = read_identity(&tab.repo.repo);
                self.settings.name = n;
                self.settings.email = e;
                self.settings.inherited = inh;
            }
        }
        egui::Window::new("Settings")
            .open(&mut open)
            .pivot(egui::Align2::CENTER_TOP)
            .default_pos(ctx.content_rect().center_top() + egui::vec2(0.0, 90.0))
            .collapsible(false)
            .resizable(false)
            .vscroll(true)
            .max_height((ctx.content_rect().height() - 140.0).max(300.0))
            .default_width(460.0)
            .show(ctx, |ui| {
                let p = pal(ui);
                let heading = |ui: &mut egui::Ui, t: &str| {
                    ui.add_space(8.0);
                    ui.label(RichText::new(t).small().strong().color(p.faint));
                    ui.add_space(2.0);
                };
                heading(ui, "APPEARANCE");
                egui::Grid::new("settings_grid_a")
                    .num_columns(2)
                    .spacing([16.0, 8.0])
                    .show(ui, |ui| {
                        ui.label("Theme");
                        let mut idx = usize::from(!self.dark);
                        if theme::segmented(ui, &["Dark", "Light"], &mut idx) {
                            self.dark = idx == 0;
                            self.apply_theme(ui.ctx());
                        }
                        ui.end_row();
                        ui.label("Terminal font size");
                        let mut size = crate::terminal::font_size();
                        if ui
                            .add(
                                egui::Slider::new(&mut size, 10.0..=20.0)
                                    .step_by(0.5)
                                    .suffix(" pt"),
                            )
                            .changed()
                        {
                            crate::terminal::set_font_size(size);
                            self.term_font = size;
                        }
                        ui.end_row();
                    });

                heading(ui, "TOOLS");
                egui::Grid::new("settings_grid_t")
                    .num_columns(2)
                    .spacing([16.0, 8.0])
                    .show(ui, |ui| {
                        ui.label("Code editor");
                        let installed = crate::desktop::installed_editors();
                        let current = self.editor_pref.clone().unwrap_or_default();
                        egui::ComboBox::from_id_salt("settings_editor")
                            .selected_text(if current.is_empty() {
                                "Automatic".to_string()
                            } else {
                                current.clone()
                            })
                            .width(220.0)
                            .show_ui(ui, |ui| {
                                let mut pick =
                                    |ui: &mut egui::Ui,
                                     value: Option<String>,
                                     label: String,
                                     enabled: bool| {
                                        let sel = self.editor_pref == value;
                                        if ui
                                            .add_enabled(
                                                enabled,
                                                egui::Button::selectable(sel, label),
                                            )
                                            .clicked()
                                        {
                                            self.editor_pref = value.clone();
                                            crate::desktop::set_preferred_editor(value);
                                        }
                                    };
                                let first = installed.first().map(|e| e.0).unwrap_or("none found");
                                pick(ui, None, format!("Automatic ({first})"), true);
                                for (name, _) in crate::desktop::EDITORS {
                                    let ok = installed.iter().any(|e| e.0 == name);
                                    let label = if ok {
                                        name.to_string()
                                    } else {
                                        format!("{name} — not installed")
                                    };
                                    pick(ui, Some(name.to_string()), label, ok);
                                }
                            });
                        ui.end_row();
                        ui.label("Default agent");
                        egui::ComboBox::from_id_salt("settings_agent")
                            .selected_text(self.agents.last_kind.name())
                            .width(220.0)
                            .show_ui(ui, |ui| {
                                for k in AgentKind::ALL {
                                    let label = if k.find().is_some() {
                                        k.name().to_string()
                                    } else {
                                        format!("{} — not installed", k.name())
                                    };
                                    ui.selectable_value(&mut self.agents.last_kind, k, label);
                                }
                            });
                        ui.end_row();
                        ui.label("Auto-fetch");
                        egui::ComboBox::from_id_salt("settings_fetch")
                            .selected_text(match self.auto_fetch_min {
                                0 => "Off".to_string(),
                                m => format!("Every {m} minutes"),
                            })
                            .width(220.0)
                            .show_ui(ui, |ui| {
                                for (m, label) in [
                                    (0, "Off"),
                                    (5, "Every 5 minutes"),
                                    (10, "Every 10 minutes"),
                                    (15, "Every 15 minutes"),
                                    (30, "Every 30 minutes"),
                                ] {
                                    if ui
                                        .selectable_value(&mut self.auto_fetch_min, m, label)
                                        .clicked()
                                    {
                                        self.last_auto_fetch = std::time::Instant::now();
                                    }
                                }
                            });
                        ui.end_row();
                    });

                heading(ui, "AGENT PROFILES");
                ui.label(
                    RichText::new("Separate accounts / settings for an agent CLI (its own config folder). Pick one in New chat.")
                        .small()
                        .color(p.faint),
                );
                let mut remove = None;
                let mut login = None;
                for (i, pr) in self.agents.profiles.iter().enumerate() {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(&pr.name).strong().color(p.text_strong));
                        ui.label(RichText::new(pr.kind.name()).color(p.subtext));
                        ui.label(RichText::new(pr.dir.display().to_string()).small().color(p.faint))
                            .on_hover_text(format!("{}={}", crate::prompts::Profile::env_var(pr.kind), pr.dir.display()));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("Remove").clicked() {
                                remove = Some(i);
                            }
                            if ui
                                .small_button("Sign in…")
                                .on_hover_text("Open the agent with this profile in a terminal (Run panel) to log in")
                                .clicked()
                            {
                                login = Some(pr.clone());
                            }
                        });
                    });
                }
                if let Some(i) = remove {
                    let mut v = self.agents.profiles.clone();
                    v.remove(i);
                    self.agents.set_profiles(v);
                }
                if let Some(pr) = login {
                    match (self.tabs.get_mut(self.active), pr.kind.find()) {
                        (Some(tab), Some(exe)) => {
                            let dir = tab.path.clone();
                            let _ = std::fs::create_dir_all(&pr.dir);
                            tab.run.envs = pr.env();
                            let cmd = crate::agents::shell_quote(&exe.to_string_lossy());
                            if let Err(e) = tab.run.start(&dir, ui.ctx(), &format!("{} · {}", pr.kind.name(), pr.name), Some(&cmd)) {
                                actions.push(Action::Notify(e, false));
                            }
                            tab.run.envs.clear();
                        }
                        (None, _) => actions.push(Action::Notify("Open a repository first".into(), false)),
                        (_, None) => actions.push(Action::Notify(format!("{} isn't installed", pr.kind.name()), false)),
                    }
                }
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.settings.new_profile).hint_text("Profile name, e.g. work").desired_width(150.0));
                    let kind = self.settings.new_profile_kind.get_or_insert(AgentKind::Claude);
                    egui::ComboBox::from_id_salt("settings_profile_kind")
                        .selected_text(kind.name())
                        .width(120.0)
                        .show_ui(ui, |ui| {
                            for k in AgentKind::ALL {
                                ui.selectable_value(kind, k, k.name());
                            }
                        });
                    let kind = *kind;
                    let name = self.settings.new_profile.trim().to_string();
                    let taken = self.agents.profiles.iter().any(|x| x.name == name && x.kind == kind);
                    if ui.add_enabled(!name.is_empty() && !taken, egui::Button::new("Add profile")).clicked() {
                        let mut v = self.agents.profiles.clone();
                        v.push(crate::prompts::Profile {
                            dir: crate::prompts::Profile::default_dir(kind, &name),
                            name,
                            kind,
                        });
                        self.agents.set_profiles(v);
                        self.settings.new_profile.clear();
                    }
                });

                heading(ui, "PROMPT SNIPPETS");
                ui.label(
                    RichText::new("Your own chips above the agent's message box (next to the built-in ones).")
                        .small()
                        .color(p.faint),
                );
                let mut drop = None;
                for (i, sn) in self.agents.snippets.iter().enumerate() {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(&sn.name).strong().color(p.text_strong));
                        ui.add(egui::Label::new(RichText::new(&sn.text).small().color(p.faint)).truncate());
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("Remove").clicked() {
                                drop = Some(i);
                            }
                        });
                    });
                }
                if let Some(i) = drop {
                    self.agents.snippets.remove(i);
                }
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.settings.snip_name).hint_text("Name").desired_width(110.0));
                    ui.add(egui::TextEdit::singleline(&mut self.settings.snip_text).hint_text("Message for the agent").desired_width(230.0));
                    let ok = !self.settings.snip_name.trim().is_empty() && !self.settings.snip_text.trim().is_empty();
                    if ui.add_enabled(ok, egui::Button::new("Add")).clicked() {
                        self.agents.snippets.push(crate::prompts::Snippet {
                            name: self.settings.snip_name.trim().to_string(),
                            text: self.settings.snip_text.trim().to_string(),
                        });
                        self.settings.snip_name.clear();
                        self.settings.snip_text.clear();
                    }
                });

                heading(ui, "THIS REPOSITORY — AUTHOR");
                match self.tabs.get(self.active).filter(|_| tab_path.is_some()) {
                    None => {
                        ui.label(
                            RichText::new("Open a repository to set its author name and email.")
                                .color(p.faint),
                        );
                    }
                    Some(tab) => {
                        ui.label(
                            RichText::new(format!("Commits in {} are made as:", tab.name))
                                .color(p.subtext),
                        );
                        let inh = self.settings.inherited.clone();
                        egui::Grid::new("settings_grid_i")
                            .num_columns(2)
                            .spacing([16.0, 8.0])
                            .show(ui, |ui| {
                                ui.label("Name");
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.settings.name)
                                        .hint_text(global_hint(&inh.0))
                                        .desired_width(260.0),
                                );
                                ui.end_row();
                                ui.label("Email");
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.settings.email)
                                        .hint_text(global_hint(&inh.1))
                                        .desired_width(260.0),
                                );
                                ui.end_row();
                            });
                        ui.horizontal(|ui| {
                            let ok = !self.settings.name.trim().is_empty()
                                || !self.settings.email.trim().is_empty();
                            if theme::primary_button(ui, "Save for this repository", ok).clicked() {
                                actions.push(Action::Run(Op::SetIdentity {
                                    name: self.settings.name.clone(),
                                    email: self.settings.email.clone(),
                                }));
                            }
                            ui.label(
                                RichText::new("Empty = use the global value")
                                    .small()
                                    .color(p.faint),
                            );
                        });
                        if self.github.login().is_some() {
                            ui.add_space(4.0);
                            if ui.link("Use my GitHub noreply address").clicked() {
                                if let Some(login) = self.github.login() {
                                    self.settings.email =
                                        format!("{login}@users.noreply.github.com");
                                    if self.settings.name.trim().is_empty() {
                                        self.settings.name = login.to_string();
                                    }
                                }
                            }
                        }
                    }
                }
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui.button("GitHub accounts…").clicked() {
                        self.github.open_signin();
                    }
                });
            });
        if !open {
            self.settings.open = false;
            self.settings.ident_for = None;
        }
    }
}
