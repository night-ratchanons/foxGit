//! Browser-style project tabs and the "Projects" launcher page.

use crate::theme::{self, pal};
use eframe::egui;
use egui::{pos2, vec2, Color32, FontId, Rect, RichText, Sense, Stroke};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Project list (persisted)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Project {
    pub path: String,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub last_opened: i64,
}

impl Project {
    pub fn name(&self) -> String {
        project_name(Path::new(&self.path))
    }
}

pub fn project_name(path: &Path) -> String {
    path.file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| path.display().to_string())
}

/// A stable, distinct color per project so tabs are easy to tell apart.
pub fn project_color(path: &str) -> Color32 {
    const PALETTE: [Color32; 12] = [
        Color32::from_rgb(66, 153, 225),
        Color32::from_rgb(72, 187, 120),
        Color32::from_rgb(237, 137, 54),
        Color32::from_rgb(159, 122, 234),
        Color32::from_rgb(236, 72, 153),
        Color32::from_rgb(56, 178, 172),
        Color32::from_rgb(236, 201, 75),
        Color32::from_rgb(245, 101, 101),
        Color32::from_rgb(129, 140, 248),
        Color32::from_rgb(163, 190, 140),
        Color32::from_rgb(244, 162, 97),
        Color32::from_rgb(94, 196, 229),
    ];
    let mut h: u32 = 2166136261;
    for b in path.trim_end_matches(['/', '\\']).bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(16777619);
    }
    PALETTE[(h % PALETTE.len() as u32) as usize]
}

/// Colors for the open tabs: the project's own color, unless an earlier tab already uses it.
pub fn assign_colors(paths: &[String]) -> Vec<Color32> {
    let mut used: Vec<Color32> = Vec::new();
    let all: Vec<Color32> = (0..12).map(|i| project_color(&format!("{i}"))).collect();
    let mut palette: Vec<Color32> = Vec::new();
    for c in all {
        if !palette.contains(&c) {
            palette.push(c);
        }
    }
    for p in paths {
        let mut c = project_color(p);
        if used.contains(&c) {
            if let Some(free) = palette.iter().find(|x| !used.contains(x)) {
                c = *free;
            }
        }
        used.push(c);
    }
    used
}

/// Read the current branch straight from `.git/HEAD` (cheap; used for the launcher list).
pub fn quick_branch(repo: &Path) -> Option<String> {
    let git = repo.join(".git");
    let head_file = if git.is_dir() {
        git.join("HEAD")
    } else {
        // worktree / submodule: ".git" is a file "gitdir: <path>"
        let txt = std::fs::read_to_string(&git).ok()?;
        let dir = txt.strip_prefix("gitdir:")?.trim();
        let dir = if Path::new(dir).is_absolute() {
            PathBuf::from(dir)
        } else {
            repo.join(dir)
        };
        dir.join("HEAD")
    };
    let head = std::fs::read_to_string(head_file).ok()?;
    let head = head.trim();
    Some(match head.strip_prefix("ref: refs/heads/") {
        Some(b) => b.to_string(),
        None => format!("detached {}", &head[..7.min(head.len())]),
    })
}

/// Find Git repositories under `root` (up to `max_depth` levels), skipping heavy folders.
pub fn scan_for_repos(root: &Path, max_depth: usize, limit: usize) -> Vec<PathBuf> {
    const SKIP: &[&str] = &[
        "node_modules",
        "target",
        "build",
        "dist",
        "vendor",
        "Library",
        "AppData",
        "venv",
        ".venv",
        "__pycache__",
        "Pods",
        "DerivedData",
    ];
    let mut found = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        if found.len() >= limit {
            break;
        }
        if dir.join(".git").exists() {
            found.push(dir);
            continue; // don't descend into repositories
        }
        if depth >= max_depth {
            continue;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut children: Vec<PathBuf> = rd
            .flatten()
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .map(|e| e.path())
            .filter(|p| {
                let n = p
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default();
                !n.starts_with('.') && !SKIP.contains(&n.as_str())
            })
            .collect();
        children.sort();
        for c in children.into_iter().rev() {
            stack.push((c, depth + 1));
        }
    }
    found.sort();
    found
}

// ---------------------------------------------------------------------------
// Tab strip
// ---------------------------------------------------------------------------

pub struct TabInfo {
    pub name: String,
    pub color: Color32,
    pub path: String,
    pub branch: Option<String>,
    pub changes: usize,
    pub ahead: usize,
    pub behind: usize,
    pub busy: bool,
    pub conflict: bool,
    pub fetched: Option<String>,
}

pub enum TabEvent {
    Select(usize),
    Close(usize),
    CloseOthers(usize),
    CloseRight(usize),
    Move { from: usize, to: usize },
    Refresh(usize),
    Reveal(usize),
    TogglePin(usize),
    OpenLauncher,
    CloseLauncher,
}

const TAB_H: f32 = 34.0;
const MAX_NAME_CHARS: usize = 26;

pub struct StripState<'a> {
    pub active: Option<usize>,
    pub launcher_active: bool,
    pub drag: &'a mut Option<usize>,
    pub scroll_to_active: &'a mut bool,
    pub pinned: &'a dyn Fn(&str) -> bool,
}

pub fn tab_strip(ui: &mut egui::Ui, tabs: &[TabInfo], st: StripState<'_>) -> Vec<TabEvent> {
    let mut events = Vec::new();
    let StripState {
        active,
        launcher_active,
        drag,
        scroll_to_active,
        pinned,
    } = st;

    egui::ScrollArea::horizontal()
        .id_salt("tab_strip")
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                let pointer = ui.ctx().pointer_latest_pos();
                let released = ui.input(|i| i.pointer.any_released());

                for (i, t) in tabs.iter().enumerate() {
                    let is_active = active == Some(i) && !launcher_active;
                    let r = tab(ui, t, is_active, *drag == Some(i), i);

                    // Drag to reorder
                    if r.resp.drag_started() {
                        *drag = Some(i);
                    }
                    if let (Some(from), Some(p)) = (*drag, pointer) {
                        if from != i && r.rect.contains(p) {
                            events.push(TabEvent::Move { from, to: i });
                            *drag = Some(i);
                        }
                    }

                    if r.close_clicked || r.resp.middle_clicked() {
                        events.push(TabEvent::Close(i));
                    } else if r.resp.clicked() || r.resp.drag_started() {
                        events.push(TabEvent::Select(i));
                    }
                    if is_active && *scroll_to_active {
                        ui.scroll_to_rect(r.rect, None);
                        *scroll_to_active = false;
                    }

                    let is_pinned = pinned(&t.path);
                    r.resp.context_menu(|ui| {
                        if ui.button("Close tab        Ctrl+W").clicked() {
                            events.push(TabEvent::Close(i));
                            ui.close();
                        }
                        if ui
                            .add_enabled(tabs.len() > 1, egui::Button::new("Close other tabs"))
                            .clicked()
                        {
                            events.push(TabEvent::CloseOthers(i));
                            ui.close();
                        }
                        if ui
                            .add_enabled(
                                i + 1 < tabs.len(),
                                egui::Button::new("Close tabs to the right"),
                            )
                            .clicked()
                        {
                            events.push(TabEvent::CloseRight(i));
                            ui.close();
                        }
                        ui.separator();
                        if ui
                            .button(if is_pinned {
                                "★ Unpin from Projects"
                            } else {
                                "☆ Pin to Projects"
                            })
                            .clicked()
                        {
                            events.push(TabEvent::TogglePin(i));
                            ui.close();
                        }
                        if ui.button("Refresh").clicked() {
                            events.push(TabEvent::Refresh(i));
                            ui.close();
                        }
                        if ui.button("Show in file manager").clicked() {
                            events.push(TabEvent::Reveal(i));
                            ui.close();
                        }
                        if ui.button("Copy path").clicked() {
                            ui.ctx().copy_text(t.path.clone());
                            ui.close();
                        }
                    });
                    if drag.is_none() {
                        let mut tip = t.path.clone();
                        if let Some(b) = &t.branch {
                            tip.push_str(&format!("\nBranch: {b}"));
                        }
                        if t.changes > 0 {
                            tip.push_str(&format!("\n{} changed file(s)", t.changes));
                        }
                        if t.ahead + t.behind > 0 {
                            tip.push_str(&format!(
                                "\n↑{} to push · ↓{} to pull",
                                t.ahead, t.behind
                            ));
                        }
                        if let Some(f) = &t.fetched {
                            tip.push_str(&format!("\nFetched {f}"));
                        }
                        if i < 9 {
                            tip.push_str(&format!("\nCtrl+{}", i + 1));
                        }
                        r.resp.on_hover_text(tip);
                    }
                }

                if released {
                    *drag = None;
                }

                if launcher_active {
                    let r = launcher_tab(ui, tabs.is_empty());
                    if r.close_clicked {
                        events.push(TabEvent::CloseLauncher);
                    }
                    if *scroll_to_active {
                        ui.scroll_to_rect(r.rect, None);
                        *scroll_to_active = false;
                    }
                }

                // "+" new tab
                let (rect, resp) = ui.allocate_exact_size(vec2(30.0, TAB_H - 4.0), Sense::click());
                let hovered = resp.hovered();
                if hovered {
                    ui.painter().rect_filled(
                        rect.shrink(2.0),
                        6.0,
                        ui.visuals().widgets.hovered.weak_bg_fill,
                    );
                }
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    "+",
                    FontId::proportional(20.0),
                    if hovered {
                        ui.visuals().strong_text_color()
                    } else {
                        ui.visuals().weak_text_color()
                    },
                );
                if resp.on_hover_text("Open project  (Ctrl+T)").clicked() {
                    events.push(TabEvent::OpenLauncher);
                }
            });
        });
    events
}

struct TabResp {
    resp: egui::Response,
    rect: Rect,
    close_clicked: bool,
}

fn tab(ui: &mut egui::Ui, t: &TabInfo, active: bool, dragging: bool, index: usize) -> TabResp {
    let visuals = ui.visuals().clone();
    let name_color = if active {
        visuals.strong_text_color()
    } else {
        visuals.text_color()
    };
    let weak = visuals.weak_text_color();

    let name = truncate(&t.name, MAX_NAME_CHARS);
    let name_g = ui.painter().layout_no_wrap(
        name,
        if active {
            FontId::proportional(13.5)
        } else {
            FontId::proportional(13.0)
        },
        name_color,
    );
    let branch_g = t.branch.as_ref().map(|b| {
        ui.painter().layout_no_wrap(
            format!("{} {}", theme::BRANCH, truncate(b, 18)),
            FontId::proportional(11.0),
            weak,
        )
    });
    let badge_g = (t.changes > 0).then(|| {
        ui.painter().layout_no_wrap(
            if t.changes > 99 {
                "99+".to_string()
            } else {
                t.changes.to_string()
            },
            FontId::proportional(10.5),
            Color32::from_gray(20),
        )
    });
    let sync_text = match (t.ahead, t.behind) {
        (0, 0) => None,
        (a, 0) => Some(format!("↑{a}")),
        (0, b) => Some(format!("↓{b}")),
        (a, b) => Some(format!("↑{a}↓{b}")),
    };
    let pl = pal(ui);
    let sync_g = sync_text.map(|s| {
        ui.painter()
            .layout_no_wrap(s, FontId::monospace(10.5), pl.accent_hover)
    });

    let pad = 12.0;
    let mut w = pad + 9.0 + 8.0 + name_g.size().x;
    if let Some(g) = &branch_g {
        w += 8.0 + g.size().x;
    }
    if let Some(g) = &sync_g {
        w += 6.0 + g.size().x;
    }
    if let Some(g) = &badge_g {
        w += 6.0 + (g.size().x + 10.0).max(18.0);
    }
    w += 8.0 + 16.0 + 8.0;

    let (rect, resp) = ui.allocate_exact_size(vec2(w, TAB_H), Sense::click_and_drag());
    let hovered = resp.hovered() || dragging;
    let painter = ui.painter();
    let color = t.color;

    // Background: the active tab blends into the toolbar below it.
    let radius = egui::CornerRadius {
        nw: 8,
        ne: 8,
        sw: 0,
        se: 0,
    };
    if active {
        painter.rect_filled(rect, radius, pl.mantle);
        painter.rect_filled(
            Rect::from_min_size(rect.min + vec2(10.0, 0.0), vec2(rect.width() - 20.0, 2.0)),
            1.0,
            color,
        );
    } else if hovered {
        painter.rect_filled(
            rect.shrink2(vec2(0.0, 2.0)).translate(vec2(0.0, 2.0)),
            radius,
            pl.surface0,
        );
    }
    if dragging {
        painter.rect_stroke(
            rect.shrink(1.0),
            radius,
            Stroke::new(1.0, color),
            egui::StrokeKind::Inside,
        );
    }

    let cy = rect.center().y + 1.0;
    let mut x = rect.left() + pad;

    // Project color dot (spinner while busy)
    let dot = Rect::from_center_size(pos2(x + 4.5, cy), vec2(11.0, 11.0));
    if t.busy {
        egui::Spinner::new()
            .size(11.0)
            .color(color)
            .paint_at(ui, dot);
    } else if t.conflict {
        painter.circle_filled(dot.center(), 4.5, pl.pink);
    } else {
        painter.circle_filled(dot.center(), 4.5, color);
    }
    x += 9.0 + 8.0;

    let gy = |g: &egui::Galley| cy - g.size().y / 2.0;
    let nw = name_g.size().x;
    painter.galley(pos2(x, gy(&name_g)), name_g.clone(), name_color);
    x += nw;
    if let Some(g) = branch_g {
        x += 8.0;
        let gw = g.size().x;
        painter.galley(pos2(x, gy(&g)), g, weak);
        x += gw;
    }
    if let Some(g) = sync_g {
        x += 6.0;
        let gw = g.size().x;
        painter.galley(pos2(x, gy(&g)), g, pl.accent_hover);
        x += gw;
    }
    if let Some(g) = badge_g {
        x += 6.0;
        let bw = (g.size().x + 10.0).max(18.0);
        let pill = Rect::from_min_size(pos2(x, cy - 8.0), vec2(bw, 16.0));
        painter.rect_filled(pill, 8.0, pl.orange);
        painter.galley(
            pos2(pill.center().x - g.size().x / 2.0, gy(&g)),
            g,
            Color32::from_gray(20),
        );
    }

    // Close button
    let close = Rect::from_center_size(pos2(rect.right() - 8.0 - 8.0, cy), vec2(16.0, 16.0));
    let over_close = resp
        .hover_pos()
        .map(|p| close.expand(2.0).contains(p))
        .unwrap_or(false);
    if active || hovered {
        if over_close {
            painter.rect_filled(close.expand(1.0), 4.0, visuals.widgets.active.weak_bg_fill);
        }
        painter.text(
            close.center(),
            egui::Align2::CENTER_CENTER,
            "×",
            FontId::proportional(14.0),
            if over_close {
                visuals.strong_text_color()
            } else {
                weak
            },
        );
    } else if index < 9 {
        // subtle shortcut hint for inactive tabs
        painter.text(
            close.center(),
            egui::Align2::CENTER_CENTER,
            format!("{}", index + 1),
            FontId::proportional(9.5),
            weak.gamma_multiply(0.6),
        );
    }

    // Separator between inactive tabs
    if !active && !hovered {
        painter.line_segment(
            [
                pos2(rect.right() + 1.0, rect.top() + 11.0),
                pos2(rect.right() + 1.0, rect.bottom() - 9.0),
            ],
            Stroke::new(1.0, pl.border_strong),
        );
    }

    let close_clicked = resp.clicked() && over_close;
    TabResp {
        resp,
        rect,
        close_clicked,
    }
}

fn launcher_tab(ui: &mut egui::Ui, only: bool) -> TabResp {
    let visuals = ui.visuals().clone();
    let w = if only { 120.0 } else { 140.0 };
    let (rect, resp) = ui.allocate_exact_size(vec2(w, TAB_H), Sense::click());
    let painter = ui.painter();
    painter.rect_filled(
        rect,
        egui::CornerRadius {
            nw: 8,
            ne: 8,
            sw: 0,
            se: 0,
        },
        pal(ui).mantle,
    );
    painter.rect_filled(
        Rect::from_min_size(rect.min + vec2(10.0, 0.0), vec2(rect.width() - 20.0, 2.0)),
        1.0,
        pal(ui).accent,
    );
    let cy = rect.center().y + 1.0;
    painter.text(
        pos2(rect.left() + 14.0, cy),
        egui::Align2::LEFT_CENTER,
        "☰  Projects",
        FontId::proportional(13.5),
        visuals.strong_text_color(),
    );
    let mut close_clicked = false;
    if !only {
        let close = Rect::from_center_size(pos2(rect.right() - 16.0, cy), vec2(16.0, 16.0));
        let over = resp
            .hover_pos()
            .map(|p| close.expand(2.0).contains(p))
            .unwrap_or(false);
        if over {
            painter.rect_filled(close.expand(1.0), 4.0, visuals.widgets.active.weak_bg_fill);
        }
        painter.text(
            close.center(),
            egui::Align2::CENTER_CENTER,
            "×",
            FontId::proportional(14.0),
            visuals.weak_text_color(),
        );
        close_clicked = resp.clicked() && over;
    }
    TabResp {
        resp,
        rect,
        close_clicked,
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max - 1).collect();
        t.push('…');
        t
    }
}

// ---------------------------------------------------------------------------
// Projects launcher page
// ---------------------------------------------------------------------------

pub enum LauncherEvent {
    Open(String),
    OpenMany(Vec<String>),
    Remove(String),
    TogglePin(String),
    PickFolder,
    Clone,
    Init,
    Scan,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LauncherMode {
    /// Saved projects on this computer.
    Local,
    /// Repositories of the signed-in GitHub account.
    Remote,
}

pub struct LauncherState {
    pub mode: LauncherMode,
    pub query: String,
    pub remote_query: String,
    pub branches: HashMap<String, Option<String>>,
    pub focus_search: bool,
    pub scanning: bool,
    pub selected: usize,
}

impl Default for LauncherState {
    fn default() -> Self {
        Self {
            mode: LauncherMode::Local,
            query: String::new(),
            remote_query: String::new(),
            branches: HashMap::new(),
            focus_search: true,
            scanning: false,
            selected: 0,
        }
    }
}

pub fn sorted_projects(projects: &[Project]) -> Vec<Project> {
    let mut v = projects.to_vec();
    v.sort_by(|a, b| {
        b.pinned
            .cmp(&a.pinned)
            .then(b.last_opened.cmp(&a.last_opened))
            .then(a.name().to_lowercase().cmp(&b.name().to_lowercase()))
    });
    v
}

/// The Projects page. `remote` draws the body of the "Remote" mode (GitHub repositories)
/// and gets the filter text.
pub fn launcher(
    ui: &mut egui::Ui,
    st: &mut LauncherState,
    projects: &[Project],
    open_paths: &[String],
    open_colors: &[Color32],
    remote: &mut dyn FnMut(&mut egui::Ui, &str),
) -> Vec<LauncherEvent> {
    let mut ev = Vec::new();
    let q = st.query.trim().to_lowercase();
    let list: Vec<Project> = sorted_projects(projects)
        .into_iter()
        .filter(|p| {
            q.is_empty()
                || p.name().to_lowercase().contains(&q)
                || p.path.to_lowercase().contains(&q)
        })
        .collect();
    if st.selected >= list.len() {
        st.selected = list.len().saturating_sub(1);
    }

    let width = ui.available_width().min(860.0);
    ui.vertical_centered(|ui| {
        ui.set_max_width(width);
        ui.add_space(28.0);
        let p = pal(ui);
        ui.horizontal(|ui| {
            ui.label(RichText::new("Projects").size(26.0).strong().color(p.text_strong));
            ui.add_space(12.0);
            let mut idx = usize::from(st.mode == LauncherMode::Remote);
            if theme::segmented(ui, &["Local", "Remote"], &mut idx) {
                st.mode = if idx == 1 {
                    LauncherMode::Remote
                } else {
                    LauncherMode::Local
                };
                st.focus_search = true;
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if st.mode == LauncherMode::Local {
                    ui.label(
                        RichText::new(format!("{} saved · {} open", projects.len(), open_paths.len()))
                            .color(p.faint),
                    );
                }
            });
        });
        ui.add_space(14.0);

        // Search / path box
        let remote_mode = st.mode == LauncherMode::Remote;
        let search = egui::TextEdit::singleline(if remote_mode {
            &mut st.remote_query
        } else {
            &mut st.query
        })
        .hint_text(if remote_mode {
            "🔍  Filter repositories"
        } else {
            "🔍  Search projects, or paste a folder path and press Enter"
        })
        .desired_width(f32::INFINITY)
        .margin(vec2(12.0, 9.0))
        .font(FontId::proportional(15.0));
        let r = ui.add(search);
        if st.focus_search {
            r.request_focus();
            st.focus_search = false;
        }
        if remote_mode {
            ui.add_space(12.0);
            let q = st.remote_query.clone();
            remote(ui, &q);
            return;
        }
        if r.changed() {
            st.selected = 0;
        }
        let (enter, up, down) = ui.input(|i| {
            (
                i.key_pressed(egui::Key::Enter),
                i.key_pressed(egui::Key::ArrowUp),
                i.key_pressed(egui::Key::ArrowDown),
            )
        });
        if r.has_focus() || r.lost_focus() {
            if down && st.selected + 1 < list.len() {
                st.selected += 1;
            }
            if up {
                st.selected = st.selected.saturating_sub(1);
            }
            if enter {
                let typed = st.query.trim();
                let as_path = expand_home(typed);
                if !typed.is_empty() && as_path.is_dir() {
                    ev.push(LauncherEvent::Open(as_path.display().to_string()));
                } else if let Some(p) = list.get(st.selected) {
                    ev.push(LauncherEvent::Open(p.path.clone()));
                }
            }
        }

        ui.add_space(10.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().button_padding = vec2(12.0, 7.0);
            if ui.button("📂  Open folder…").clicked() {
                ev.push(LauncherEvent::PickFolder);
            }
            if ui.button("⬇  Clone…").clicked() {
                ev.push(LauncherEvent::Clone);
            }
            if ui.button("✚  New repository…").clicked() {
                ev.push(LauncherEvent::Init);
            }
            ui.add_enabled_ui(!st.scanning, |ui| {
                if ui
                    .button("🔎  Scan folder for repos…")
                    .on_hover_text("Pick a folder (e.g. ~/Projects) — every Git repository inside is added to this list")
                    .clicked()
                {
                    ev.push(LauncherEvent::Scan);
                }
            });
            if st.scanning {
                ui.add(egui::Spinner::new());
                ui.label(RichText::new("Scanning…").weak());
            }
        });
        ui.add_space(14.0);

        if list.is_empty() {
            ui.add_space(40.0);
            ui.label(
                RichText::new(if projects.is_empty() {
                    "No projects yet — open a folder, clone, or scan a folder that contains your projects."
                } else {
                    "No project matches your search."
                })
                .weak(),
            );
            return;
        }

        // Pinned / recent headers + cards
        let not_open: Vec<String> = list
            .iter()
            .filter(|p| !open_paths.contains(&p.path))
            .map(|p| p.path.clone())
            .collect();
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            let mut last_pinned: Option<bool> = None;
            for (i, p) in list.iter().enumerate() {
                if last_pinned != Some(p.pinned) {
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(if p.pinned { "★ PINNED" } else { "RECENT" })
                                .small()
                                .strong()
                                .weak(),
                        );
                        if p.pinned && last_pinned.is_none() && not_open.len() > 1 && q.is_empty() {
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui
                                    .small_button("Open all pinned")
                                    .on_hover_text("Open every pinned project as a tab")
                                    .clicked()
                                {
                                    let pinned: Vec<String> = list
                                        .iter()
                                        .filter(|p| p.pinned && !open_paths.contains(&p.path))
                                        .map(|p| p.path.clone())
                                        .collect();
                                    if !pinned.is_empty() {
                                        ev.push(LauncherEvent::OpenMany(pinned));
                                    }
                                }
                            });
                        }
                    });
                    ui.add_space(2.0);
                    last_pinned = Some(p.pinned);
                }
                let branch = st
                    .branches
                    .entry(p.path.clone())
                    .or_insert_with(|| quick_branch(Path::new(&p.path)))
                    .clone();
                let open_idx = open_paths.iter().position(|x| x == &p.path);
                let color = open_idx
                    .and_then(|i| open_colors.get(i).copied())
                    .unwrap_or_else(|| project_color(&p.path));
                project_card(ui, p, branch.as_deref(), open_idx.is_some(), i == st.selected, color, &mut ev);
            }
            ui.add_space(20.0);
        });
    });
    ev
}

fn project_card(
    ui: &mut egui::Ui,
    p: &Project,
    branch: Option<&str>,
    is_open: bool,
    selected: bool,
    color: Color32,
    ev: &mut Vec<LauncherEvent>,
) {
    let visuals = ui.visuals().clone();
    let h = 54.0;
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), h), Sense::click());
    let painter = ui.painter();
    let hovered = resp.hovered();
    let pl = pal(ui);
    let fill = if hovered || selected {
        pl.surface1
    } else {
        pl.surface0
    };
    let card = rect.shrink2(vec2(0.0, 3.0));
    painter.rect_filled(card, 10.0, fill);
    painter.rect_stroke(
        card,
        10.0,
        Stroke::new(1.0, if selected { pl.accent } else { pl.border }),
        egui::StrokeKind::Inside,
    );

    // Avatar: colored rounded square with the first letter
    let av = Rect::from_min_size(
        pos2(card.left() + 10.0, card.center().y - 16.0),
        vec2(32.0, 32.0),
    );
    painter.rect_filled(av, 8.0, color);
    let name = p.name();
    let letter = name
        .chars()
        .next()
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_default();
    painter.text(
        av.center(),
        egui::Align2::CENTER_CENTER,
        letter,
        FontId::proportional(17.0),
        Color32::from_gray(20),
    );

    let x = av.right() + 12.0;
    let exists = Path::new(&p.path).exists();
    let name_color = if exists {
        visuals.strong_text_color()
    } else {
        visuals.weak_text_color()
    };
    let ng = painter.layout_no_wrap(name.clone(), FontId::proportional(14.5), name_color);
    let nw = ng.size().x;
    painter.galley(pos2(x, card.top() + 8.0), ng, name_color);
    let mut bx = x + nw + 10.0;
    if let Some(b) = branch {
        let g = painter.layout_no_wrap(
            format!("{} {b}", theme::BRANCH),
            FontId::proportional(11.5),
            visuals.weak_text_color(),
        );
        let gw = g.size().x;
        painter.galley(pos2(bx, card.top() + 11.0), g, visuals.weak_text_color());
        bx += gw + 10.0;
    }
    if is_open {
        let g = painter.layout_no_wrap("OPEN".into(), FontId::proportional(9.5), pl.green);
        let pill = Rect::from_min_size(pos2(bx, card.top() + 10.0), vec2(g.size().x + 12.0, 15.0));
        painter.rect_filled(pill, 7.5, theme::alpha(pl.green, 0.16));
        painter.galley(
            pos2(pill.left() + 6.0, pill.center().y - g.size().y / 2.0),
            g,
            pl.green,
        );
    }
    if !exists {
        let g = painter.layout_no_wrap(
            "folder not found".into(),
            FontId::proportional(10.5),
            pl.red,
        );
        painter.galley(pos2(bx, card.top() + 11.0), g, pl.red);
    }
    let path_rect = Rect::from_min_max(
        pos2(x, card.top() + 28.0),
        pos2(card.right() - 80.0, card.bottom()),
    );
    ui.painter().with_clip_rect(path_rect).text(
        pos2(x, card.top() + 36.0),
        egui::Align2::LEFT_CENTER,
        &p.path,
        FontId::proportional(11.5),
        visuals.weak_text_color(),
    );

    // Right-side actions: pin + remove
    let pin = Rect::from_center_size(pos2(card.right() - 52.0, card.center().y), vec2(24.0, 24.0));
    let del = Rect::from_center_size(pos2(card.right() - 22.0, card.center().y), vec2(24.0, 24.0));
    let hp = resp.hover_pos();
    let over_pin = hp.map(|q| pin.contains(q)).unwrap_or(false);
    let over_del = hp.map(|q| del.contains(q)).unwrap_or(false);
    let painter = ui.painter();
    if p.pinned || hovered {
        if over_pin {
            painter.rect_filled(pin, 6.0, visuals.widgets.active.weak_bg_fill);
        }
        painter.text(
            pin.center(),
            egui::Align2::CENTER_CENTER,
            if p.pinned { "★" } else { "☆" },
            FontId::proportional(16.0),
            if p.pinned {
                pl.yellow
            } else {
                visuals.weak_text_color()
            },
        );
    }
    if hovered {
        if over_del {
            painter.rect_filled(del, 6.0, visuals.widgets.active.weak_bg_fill);
        }
        painter.text(
            del.center(),
            egui::Align2::CENTER_CENTER,
            "×",
            FontId::proportional(16.0),
            visuals.weak_text_color(),
        );
    }

    let resp = resp.on_hover_text(if over_pin {
        if p.pinned {
            "Unpin"
        } else {
            "Pin to top"
        }
    } else if over_del {
        "Remove from list (files are not touched)"
    } else if is_open {
        "Switch to this tab"
    } else {
        "Open in a new tab"
    });
    if resp.clicked() {
        if over_pin {
            ev.push(LauncherEvent::TogglePin(p.path.clone()));
        } else if over_del {
            ev.push(LauncherEvent::Remove(p.path.clone()));
        } else {
            ev.push(LauncherEvent::Open(p.path.clone()));
        }
    }
    resp.context_menu(|ui| {
        if ui.button("Open").clicked() {
            ev.push(LauncherEvent::Open(p.path.clone()));
            ui.close();
        }
        if ui
            .button(if p.pinned { "Unpin" } else { "Pin to top" })
            .clicked()
        {
            ev.push(LauncherEvent::TogglePin(p.path.clone()));
            ui.close();
        }
        if ui.button("Copy path").clicked() {
            ui.ctx().copy_text(p.path.clone());
            ui.close();
        }
        if ui.button("Remove from list").clicked() {
            ev.push(LauncherEvent::Remove(p.path.clone()));
            ui.close();
        }
    });
}

pub fn expand_home(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/").or_else(|| s.strip_prefix("~\\")) {
        if let Ok(h) = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")) {
            return Path::new(&h).join(rest);
        }
    }
    PathBuf::from(s)
}

pub fn reveal_in_file_manager(path: &Path) {
    #[cfg(target_os = "windows")]
    let cmd = "explorer";
    #[cfg(target_os = "macos")]
    let cmd = "open";
    #[cfg(all(unix, not(target_os = "macos")))]
    let cmd = "xdg-open";
    let _ = std::process::Command::new(cmd).arg(path).spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_finds_nested_repos_and_skips_heavy_dirs() {
        let t = tempfile::tempdir().unwrap();
        let r = t.path();
        for p in [
            "a/.git",
            "group/b/.git",
            "group/b/inner/.git",
            "node_modules/x/.git",
            ".hidden/c/.git",
        ] {
            std::fs::create_dir_all(r.join(p)).unwrap();
        }
        let found = scan_for_repos(r, 4, 100);
        let names: Vec<String> = found.iter().map(|p| project_name(p)).collect();
        assert_eq!(names, vec!["a", "b"]);
    }

    #[test]
    fn quick_branch_reads_head() {
        let t = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(t.path().join(".git")).unwrap();
        std::fs::write(t.path().join(".git/HEAD"), "ref: refs/heads/dev\n").unwrap();
        assert_eq!(quick_branch(t.path()).as_deref(), Some("dev"));
    }

    #[test]
    fn sort_pinned_then_recent() {
        let p = |path: &str, pinned, t| Project {
            path: path.into(),
            pinned,
            last_opened: t,
        };
        let v = sorted_projects(&[
            p("/x/old", false, 1),
            p("/x/new", false, 5),
            p("/x/pin", true, 0),
        ]);
        let names: Vec<String> = v.iter().map(|p| p.name()).collect();
        assert_eq!(names, vec!["pin", "new", "old"]);
    }
}
