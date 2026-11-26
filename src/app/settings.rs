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
