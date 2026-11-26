//! Dashboard (Projects → Dashboard): every open project and agent chat at a glance — branch,
//! ahead/behind, changes, conflicts, running scripts — with one click to the right place.

use super::{FoxGitApp, View};
use crate::agents::{RowInfo, Status};
use crate::theme::{self, pal};
use eframe::egui;
use egui::{vec2, Color32, RichText, Stroke};
use std::path::PathBuf;

pub struct Card {
    tab: usize,
    name: String,
    color: Color32,
    branch: String,
    ahead: usize,
    behind: usize,
    changes: usize,
    conflicts: usize,
    state: Option<&'static str>,
    scripts: usize,
    agents: Vec<RowInfo>,
}

pub enum DashEvent {
    Open(usize, View),
    Chat(PathBuf, u64),
    FetchAll,
}

impl FoxGitApp {
    pub(super) fn dashboard_cards(&self) -> (Vec<Card>, Vec<(PathBuf, RowInfo)>) {
        let colors = self.tab_colors();
        let mut all = self.agents.all_rows();
        let cards = self
            .tabs
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let head = t.head_branch();
                let (agents, rest): (Vec<_>, Vec<_>) =
                    all.drain(..).partition(|(r, _)| *r == t.path);
                all = rest;
                Card {
                    tab: i,
                    name: t.name.clone(),
                    color: colors.get(i).copied().unwrap_or(Color32::GRAY),
                    branch: if t.refs.head_detached {
                        "detached HEAD".into()
                    } else {
                        t.refs.head_name.clone().unwrap_or_default()
                    },
                    ahead: head.map_or(0, |b| b.ahead),
                    behind: head.map_or(0, |b| b.behind),
                    changes: t.status.staged.len() + t.status.unstaged.len(),
                    conflicts: t.conflicted().len(),
                    state: crate::git::Sequence::of(t.repo.repo.state()).map(|s| s.noun()),
                    scripts: t.run.alive(),
                    agents: agents.into_iter().map(|a| a.1).collect(),
                }
            })
            .collect();
        (cards, all)
    }
}

pub fn ui(
    ui: &mut egui::Ui,
    cards: &[Card],
    others: &[(PathBuf, RowInfo)],
    out: &mut Vec<DashEvent>,
) {
    let p = pal(ui);
    ui.horizontal(|ui| {
        ui.label(RichText::new(format!("{} open project(s)", cards.len())).color(p.subtext));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("⟳ Fetch all").clicked() {
                out.push(DashEvent::FetchAll);
            }
        });
    });
    ui.add_space(8.0);
    if cards.is_empty() {
        ui.label(RichText::new("No projects open — pick one under Local.").color(p.faint));
    }
    let card_w = 268.0;
    egui::ScrollArea::vertical()
        .auto_shrink(false)
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = vec2(10.0, 10.0);
                for c in cards {
                    card(ui, c, card_w, out);
                }
            });
            if !others.is_empty() {
                ui.add_space(14.0);
                ui.label(
                    RichText::new("AGENT CHATS IN OTHER PROJECTS")
                        .small()
                        .color(p.faint),
                );
                for (repo, a) in others {
                    let name = repo
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default();
                    let (dot, c) = status_dot(a.status, p);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(dot).color(c));
                        if ui.link(format!("{name} · {}", a.title)).clicked() {
                            out.push(DashEvent::Chat(repo.clone(), a.id));
                        }
                        ui.label(RichText::new(a.kind.name()).small().color(p.faint));
                    });
                }
            }
        });
}

fn status_dot(s: Status, p: &theme::Palette) -> (&'static str, Color32) {
    match s {
        Status::Running => ("●", p.accent_hover),
        Status::Idle => ("●", p.green),
        Status::Failed => ("●", p.red),
        Status::Stopped => ("■", p.faint),
        Status::Merged => ("✔", p.purple),
    }
}

fn card(ui: &mut egui::Ui, c: &Card, w: f32, out: &mut Vec<DashEvent>) {
    let p = pal(ui);
    let attention = c.conflicts > 0 || c.agents.iter().any(|a| a.status == Status::Failed);
    egui::Frame::new()
        .fill(p.raised)
        .stroke(Stroke::new(
            1.0,
            if attention {
                theme::alpha(p.yellow, 0.6)
            } else {
                p.border
            },
        ))
        .corner_radius(12.0)
        .inner_margin(egui::Margin::same(12))
        .show(ui, |ui| {
            ui.set_width(w);
            ui.set_min_height(120.0);
            ui.vertical(|ui| card_body(ui, c, out));
        });
}

fn card_body(ui: &mut egui::Ui, c: &Card, out: &mut Vec<DashEvent>) {
    let p = pal(ui);
    {
        {
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(vec2(10.0, 10.0), egui::Sense::hover());
                ui.painter().circle_filled(r.center(), 5.0, c.color);
                if ui
                    .add(
                        egui::Label::new(
                            RichText::new(&c.name)
                                .size(15.0)
                                .strong()
                                .color(p.text_strong),
                        )
                        .sense(egui::Sense::click())
                        .truncate(),
                    )
                    .clicked()
                {
                    out.push(DashEvent::Open(c.tab, View::History));
                }
            });
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!("{} {}", theme::BRANCH, c.branch))
                        .monospace()
                        .color(p.accent_hover),
                );
                if c.ahead > 0 {
                    ui.label(RichText::new(format!("↑{}", c.ahead)).color(p.green));
                }
                if c.behind > 0 {
                    ui.label(RichText::new(format!("↓{}", c.behind)).color(p.yellow));
                }
            });
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                if let Some(s) = c.state {
                    if ui
                        .link(RichText::new(format!("⚠ {s} in progress")).color(p.yellow))
                        .clicked()
                    {
                        out.push(DashEvent::Open(c.tab, View::FileStatus));
                    }
                }
                if c.conflicts > 0 {
                    if ui
                        .link(RichText::new(format!("{} conflict(s)", c.conflicts)).color(p.red))
                        .clicked()
                    {
                        out.push(DashEvent::Open(c.tab, View::FileStatus));
                    }
                } else if c.changes > 0 {
                    if ui
                        .link(RichText::new(format!("{} change(s)", c.changes)).color(p.text))
                        .clicked()
                    {
                        out.push(DashEvent::Open(c.tab, View::FileStatus));
                    }
                } else {
                    ui.label(RichText::new("clean ✔").color(p.faint));
                }
                if c.scripts > 0 {
                    ui.label(RichText::new(format!("▶ {} running", c.scripts)).color(p.teal));
                }
            });
            if !c.agents.is_empty() {
                ui.add_space(4.0);
                for a in c
                    .agents
                    .iter()
                    .filter(|a| a.status != Status::Merged)
                    .take(4)
                {
                    let (dot, col) = status_dot(a.status, p);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(dot).color(col));
                        if ui
                            .add(egui::Link::new(RichText::new(&a.title).color(p.text)))
                            .clicked()
                        {
                            out.push(DashEvent::Open(c.tab, View::Agents));
                            out.push(DashEvent::Chat(PathBuf::new(), a.id));
                        }
                    });
                }
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                for (label, v) in [
                    ("Changes", View::FileStatus),
                    ("History", View::History),
                    ("GitHub", View::GitHub),
                ] {
                    if ui.small_button(label).clicked() {
                        out.push(DashEvent::Open(c.tab, v));
                    }
                }
            });
        }
    }
}
