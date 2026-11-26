//! Main views: sidebar, history (commit graph) and file status (staging + commit).

use crate::app::{push_dialog_for, Action, Dialog, RepoTab, View};
use crate::diffview;
use crate::git::{ChangeKind, CommitInfo, DiffTarget, FileChange, Op, RefKind, RefLabel};
use crate::graph::{GraphRow, Pos};
use eframe::egui;
use egui::{pos2, vec2, Color32, FontId, Rect, RichText, Sense, Stroke};

pub const LANE_COLORS: [Color32; 10] = [
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
];

pub fn lane_color(i: usize) -> Color32 {
    LANE_COLORS[i % LANE_COLORS.len()]
}

pub fn kind_color(k: ChangeKind) -> Color32 {
    match k {
        ChangeKind::Added | ChangeKind::Untracked => Color32::from_rgb(72, 187, 120),
        ChangeKind::Modified | ChangeKind::TypeChange => Color32::from_rgb(237, 170, 54),
        ChangeKind::Deleted => Color32::from_rgb(235, 90, 90),
        ChangeKind::Renamed => Color32::from_rgb(110, 160, 240),
        ChangeKind::Conflicted => Color32::from_rgb(240, 60, 200),
    }
}

pub fn format_time(c: &CommitInfo) -> String {
    let tz = chrono::FixedOffset::east_opt(c.offset_min * 60)
        .unwrap_or_else(|| chrono::FixedOffset::east_opt(0).unwrap());
    chrono::DateTime::from_timestamp(c.time, 0)
        .map(|d| d.with_timezone(&tz).format("%d %b %Y %H:%M").to_string())
        .unwrap_or_default()
}

fn section_header(ui: &mut egui::Ui, text: &str) {
    ui.add_space(8.0);
    ui.label(RichText::new(text).small().strong().weak());
    ui.add_space(2.0);
}

// ---------------------------------------------------------------------------
// Sidebar
// ---------------------------------------------------------------------------

pub fn sidebar(ui: &mut egui::Ui, tab: &mut RepoTab, actions: &mut Vec<Action>) {
    egui::Panel::left("sidebar")
        .resizable(true)
        .default_size(240.0)
        .size_range(160.0..=480.0)
        .show(ui, |ui| {
            egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                section_header(ui, "WORKSPACE");
                let changes = tab.status.staged.len() + tab.status.unstaged.len();
                let fs_label = if changes > 0 {
                    format!("📝 File Status  ({changes})")
                } else {
                    "📝 File Status".to_string()
                };
                if ui
                    .selectable_label(tab.view == View::FileStatus, fs_label)
                    .clicked()
                {
                    tab.view = View::FileStatus;
                }
                if ui
                    .selectable_label(tab.view == View::History, "🕘 History")
                    .clicked()
                {
                    tab.view = View::History;
                }

                section_header(ui, "BRANCHES");
                let locals = tab.refs.locals.clone();
                if locals.is_empty() {
                    ui.label(RichText::new("  (no branches yet)").weak());
                }
                for b in &locals {
                    let mut text = if b.is_head {
                        RichText::new(format!("● {}", b.name)).strong()
                    } else {
                        RichText::new(format!("   {}", b.name))
                    };
                    if b.is_head {
                        text = text.color(Color32::from_rgb(90, 200, 120));
                    }
                    let resp = ui.horizontal(|ui| {
                        let r = ui.selectable_label(false, text);
                        if b.ahead > 0 {
                            ui.label(RichText::new(format!("↑{}", b.ahead)).small().weak());
                        }
                        if b.behind > 0 {
                            ui.label(RichText::new(format!("↓{}", b.behind)).small().weak());
                        }
                        r
                    });
                    let r = resp.inner;
                    let r = match &b.upstream {
                        Some(up) => r.on_hover_text(format!("Tracking {up}\nDouble-click to checkout")),
                        None => r.on_hover_text("Double-click to checkout"),
                    };
                    if r.clicked() {
                        if let Some(oid) = b.target {
                            jump_to(tab, oid);
                        }
                    }
                    if r.double_clicked() && !b.is_head {
                        actions.push(Action::Run(Op::Checkout(b.name.clone())));
                    }
                    let current = tab.refs.head_name.clone().unwrap_or_default();
                    r.context_menu(|ui| {
                        if ui.add_enabled(!b.is_head, egui::Button::new("Checkout")).clicked() {
                            actions.push(Action::Run(Op::Checkout(b.name.clone())));
                            ui.close();
                        }
                        if ui
                            .add_enabled(
                                !b.is_head,
                                egui::Button::new(format!("Merge {} into {current}", b.name)),
                            )
                            .clicked()
                        {
                            actions.push(Action::Dialog(Dialog::Confirm {
                                title: "Merge".into(),
                                text: format!("Merge '{}' into '{current}'?", b.name),
                                op: Op::Merge(b.name.clone()),
                                danger: false,
                            }));
                            ui.close();
                        }
                        if ui
                            .add_enabled(
                                !b.is_head,
                                egui::Button::new(format!("Rebase {current} onto {}", b.name)),
                            )
                            .clicked()
                        {
                            actions.push(Action::Dialog(Dialog::Confirm {
                                title: "Rebase".into(),
                                text: format!("Rebase '{current}' onto '{}'?", b.name),
                                op: Op::Rebase(b.name.clone()),
                                danger: true,
                            }));
                            ui.close();
                        }
                        if b.is_head && ui.button("Push…").clicked() {
                            actions.push(Action::Dialog(push_dialog_for(tab)));
                            ui.close();
                        }
                        if ui.button("New branch from here…").clicked() {
                            actions.push(Action::Dialog(Dialog::NewBranch {
                                name: String::new(),
                                start: Some(b.name.clone()),
                                start_label: b.name.clone(),
                                checkout: true,
                            }));
                            ui.close();
                        }
                        if ui.button("Copy name").clicked() {
                            ui.ctx().copy_text(b.name.clone());
                            ui.close();
                        }
                        ui.separator();
                        if ui
                            .add_enabled(!b.is_head, egui::Button::new("Delete…"))
                            .clicked()
                        {
                            actions.push(Action::Dialog(Dialog::Confirm {
                                title: "Delete branch".into(),
                                text: format!(
                                    "Delete local branch '{}'?\n(Force-deletes even if not merged.)",
                                    b.name
                                ),
                                op: Op::DeleteBranch {
                                    name: b.name.clone(),
                                    force: true,
                                },
                                danger: true,
                            }));
                            ui.close();
                        }
                    });
                }

                section_header(ui, "REMOTES");
                let remotes = tab.refs.remotes.clone();
                let names = tab.refs.remote_names.clone();
                if names.is_empty() {
                    ui.label(RichText::new("  (no remotes)").weak());
                }
                for remote in &names {
                    egui::CollapsingHeader::new(format!("☁ {remote}"))
                        .id_salt(("remote", remote))
                        .default_open(true)
                        .show(ui, |ui| {
                            for rb in remotes.iter().filter(|r| &r.remote == remote) {
                                let r = ui
                                    .selectable_label(false, &rb.branch)
                                    .on_hover_text("Double-click to checkout");
                                if r.clicked() {
                                    if let Some(oid) = rb.target {
                                        jump_to(tab, oid);
                                    }
                                }
                                let exists = locals.iter().any(|l| l.name == rb.branch);
                                let checkout_op = if exists {
                                    Op::Checkout(rb.branch.clone())
                                } else {
                                    Op::CheckoutRemote {
                                        remote_branch: rb.name.clone(),
                                        local: rb.branch.clone(),
                                    }
                                };
                                if r.double_clicked() {
                                    actions.push(Action::Run(checkout_op.clone()));
                                }
                                r.context_menu(|ui| {
                                    if ui.button("Checkout (track)").clicked() {
                                        actions.push(Action::Run(checkout_op.clone()));
                                        ui.close();
                                    }
                                    if ui.button("Merge into current").clicked() {
                                        actions.push(Action::Dialog(Dialog::Confirm {
                                            title: "Merge".into(),
                                            text: format!("Merge '{}' into the current branch?", rb.name),
                                            op: Op::Merge(rb.name.clone()),
                                            danger: false,
                                        }));
                                        ui.close();
                                    }
                                    if ui.button("Copy name").clicked() {
                                        ui.ctx().copy_text(rb.name.clone());
                                        ui.close();
                                    }
                                });
                            }
                        });
                }

                section_header(ui, "TAGS");
                let tags = tab.refs.tags.clone();
                egui::CollapsingHeader::new(format!("🏷 Tags ({})", tags.len()))
                    .id_salt("tags")
                    .default_open(false)
                    .show(ui, |ui| {
                        for (name, oid) in &tags {
                            let r = ui.selectable_label(false, name);
                            if r.clicked() {
                                jump_to(tab, *oid);
                            }
                            r.context_menu(|ui| {
                                if ui.button("Checkout (detached)").clicked() {
                                    actions.push(Action::Run(Op::CheckoutCommit(name.clone())));
                                    ui.close();
                                }
                                if ui.button("Delete tag…").clicked() {
                                    actions.push(Action::Dialog(Dialog::Confirm {
                                        title: "Delete tag".into(),
                                        text: format!("Delete local tag '{name}'?"),
                                        op: Op::DeleteTag(name.clone()),
                                        danger: true,
                                    }));
                                    ui.close();
                                }
                            });
                        }
                    });

                section_header(ui, "STASHES");
                let stashes = tab.refs.stashes.clone();
                if stashes.is_empty() {
                    ui.label(RichText::new("  (none)").weak());
                }
                for s in &stashes {
                    let r = ui
                        .selectable_label(false, format!("▤ {}", s.message))
                        .on_hover_text("Right-click to apply / pop / drop");
                    if r.clicked() {
                        tab.view = View::History;
                        tab.selected_commit = Some(s.oid);
                        tab.commit_files = tab.repo.commit_files(s.oid).unwrap_or_default();
                        let first = tab.commit_files.first().map(|f| f.path.clone());
                        tab.diff_target = first.map(|path| DiffTarget::Commit { oid: s.oid, path });
                        tab.reload_diff();
                    }
                    let i = s.index;
                    r.context_menu(|ui| {
                        if ui.button("Apply").clicked() {
                            actions.push(Action::Run(Op::StashApply(i)));
                            ui.close();
                        }
                        if ui.button("Pop (apply & delete)").clicked() {
                            actions.push(Action::Run(Op::StashPop(i)));
                            ui.close();
                        }
                        if ui.button("Drop…").clicked() {
                            actions.push(Action::Dialog(Dialog::Confirm {
                                title: "Drop stash".into(),
                                text: format!("Delete stash@{{{i}}} permanently?"),
                                op: Op::StashDrop(i),
                                danger: true,
                            }));
                            ui.close();
                        }
                    });
                }
                ui.add_space(12.0);
            });
        });
}

fn jump_to(tab: &mut RepoTab, oid: git2::Oid) {
    tab.view = View::History;
    if !tab.search.is_empty() {
        tab.search.clear();
        tab.apply_filter();
    }
    tab.select_commit(oid);
    tab.scroll_to_selected = true;
}

// ---------------------------------------------------------------------------
// History view
// ---------------------------------------------------------------------------

const ROW_H: f32 = 24.0;
const LANE_W: f32 = 14.0;
const COL_HASH: f32 = 72.0;
const COL_AUTHOR: f32 = 150.0;
const COL_DATE: f32 = 140.0;

pub fn history_view(ui: &mut egui::Ui, tab: &mut RepoTab, actions: &mut Vec<Action>) {
    // Commit details + diff (bottom)
    egui::Panel::bottom("commit_details")
        .resizable(true)
        .default_size(330.0)
        .size_range(120.0..=900.0)
        .show(ui, |ui| {
            commit_details(ui, tab, actions);
        });

    egui::CentralPanel::default().show(ui, |ui| {
        merge_banner(ui, tab, actions);
        ui.horizontal(|ui| {
            ui.label("🔍");
            let r = ui.add(
                egui::TextEdit::singleline(&mut tab.search)
                    .hint_text("Search message, author or SHA…")
                    .desired_width(320.0),
            );
            if r.changed() {
                tab.apply_filter();
            }
            if !tab.search.is_empty() && ui.small_button("×").clicked() {
                tab.search.clear();
                tab.apply_filter();
            }
            if let Some(f) = &tab.filtered {
                ui.label(RichText::new(format!("{} match(es)", f.len())).weak());
            }
            if tab.is_loading() {
                ui.add(egui::Spinner::new());
                ui.label(RichText::new("Loading history…").weak());
            }
        });
        ui.add_space(4.0);
        commit_list(ui, tab, actions);
    });
}

fn merge_banner(ui: &mut egui::Ui, tab: &mut RepoTab, actions: &mut Vec<Action>) {
    let state = tab.repo.repo.state();
    if state == git2::RepositoryState::Clean {
        return;
    }
    egui::Frame::new()
        .fill(Color32::from_rgb(110, 70, 20))
        .inner_margin(6.0)
        .corner_radius(4.0)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!(
                        "⚠ Repository state: {state:?} — resolve conflicts, then commit."
                    ))
                    .color(Color32::WHITE),
                );
                if state == git2::RepositoryState::Merge && ui.button("Abort merge").clicked() {
                    actions.push(Action::Dialog(Dialog::Confirm {
                        title: "Abort merge".into(),
                        text: "Abort the merge and discard merge changes?".into(),
                        op: Op::AbortMerge,
                        danger: true,
                    }));
                }
            });
        });
    ui.add_space(4.0);
}

fn commit_list(ui: &mut egui::Ui, tab: &mut RepoTab, actions: &mut Vec<Action>) {
    let show_graph = tab.filtered.is_none();
    let graph_w = if show_graph {
        LANE_W * tab.graph_lanes as f32 + 10.0
    } else {
        16.0
    };
    let show_wip = show_graph && !tab.status.is_clean();
    let wip_rows = usize::from(show_wip);
    let n_commits = tab
        .filtered
        .as_ref()
        .map(|f| f.len())
        .unwrap_or(tab.commits.len());
    let more_row = usize::from(tab.filtered.is_none() && tab.commits.len() >= tab.commit_limit);
    let total = wip_rows + n_commits + more_row;

    // Header
    let header_h = 20.0;
    let (hrect, _) = ui.allocate_exact_size(vec2(ui.available_width(), header_h), Sense::hover());
    let cols = Columns::new(hrect, graph_w);
    let painter = ui.painter();
    let weak = ui.visuals().weak_text_color();
    let font = FontId::proportional(12.0);
    painter.text(
        pos2(cols.graph.left() + 4.0, hrect.center().y),
        egui::Align2::LEFT_CENTER,
        "Graph",
        font.clone(),
        weak,
    );
    painter.text(
        pos2(cols.desc.left(), hrect.center().y),
        egui::Align2::LEFT_CENTER,
        "Description",
        font.clone(),
        weak,
    );
    painter.text(
        pos2(cols.date.left(), hrect.center().y),
        egui::Align2::LEFT_CENTER,
        "Date",
        font.clone(),
        weak,
    );
    painter.text(
        pos2(cols.author.left(), hrect.center().y),
        egui::Align2::LEFT_CENTER,
        "Author",
        font.clone(),
        weak,
    );
    painter.text(
        pos2(cols.hash.left(), hrect.center().y),
        egui::Align2::LEFT_CENTER,
        "Commit",
        font,
        weak,
    );
    ui.separator();

    // Keyboard navigation
    let nav = ui.ctx().memory(|m| m.focused().is_none());
    let (up, down) = if nav {
        ui.input(|i| {
            (
                i.key_pressed(egui::Key::ArrowUp),
                i.key_pressed(egui::Key::ArrowDown),
            )
        })
    } else {
        (false, false)
    };
    if (up || down) && n_commits > 0 {
        let cur = tab
            .selected_commit
            .and_then(|sel| visible_index_of(tab, sel));
        let next = match cur {
            None => 0,
            Some(i) if up => i.saturating_sub(1),
            Some(i) => (i + 1).min(n_commits - 1),
        };
        let idx = commit_index(tab, next);
        let oid = tab.commits[idx].oid;
        tab.select_commit(oid);
        let row = next + wip_rows;
        if row <= tab.history_range.0 || row + 1 >= tab.history_range.1 {
            tab.scroll_to_selected = true;
        }
    }

    let mut scroll = egui::ScrollArea::vertical()
        .id_salt("commit_list")
        .auto_shrink(false);
    if tab.scroll_to_selected {
        tab.scroll_to_selected = false;
        if let Some(i) = tab
            .selected_commit
            .and_then(|sel| visible_index_of(tab, sel))
        {
            let row = (i + wip_rows) as f32;
            let viewport = ui.available_height();
            scroll = scroll.vertical_scroll_offset((row * ROW_H - viewport / 2.0).max(0.0));
        }
    }

    ui.spacing_mut().item_spacing.y = 0.0;
    let mut select: Option<git2::Oid> = None;
    let mut goto_status = false;
    let mut load_more = false;

    scroll.show_rows(ui, ROW_H, total, |ui, range| {
        tab.history_range = (range.start, range.end);
        for row in range {
            let (rect, resp) =
                ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click());
            let cols = Columns::new(rect, graph_w);

            if show_wip && row == 0 {
                paint_wip_row(ui, rect, &cols, tab, &resp);
                if resp.clicked() {
                    goto_status = true;
                }
                continue;
            }
            let vis = row - wip_rows;
            if vis >= n_commits {
                // "Load more" row
                let r = ui.put(
                    rect,
                    egui::Button::new(format!("Load more commits (showing {})", tab.commits.len())),
                );
                if r.clicked() {
                    load_more = true;
                }
                continue;
            }
            let idx = commit_index(tab, vis);
            let commit = &tab.commits[idx];
            let selected = tab.selected_commit == Some(commit.oid);

            // Background
            let visuals = ui.visuals();
            if selected {
                ui.painter()
                    .rect_filled(rect, 0.0, visuals.selection.bg_fill);
            } else if resp.hovered() {
                ui.painter()
                    .rect_filled(rect, 0.0, visuals.widgets.hovered.weak_bg_fill);
            } else if vis % 2 == 1 {
                ui.painter().rect_filled(rect, 0.0, visuals.faint_bg_color);
            }

            if show_graph {
                let row_g = &tab.graph[idx];
                let is_head = tab.refs.head_oid == Some(commit.oid);
                paint_graph(
                    ui,
                    cols.graph,
                    row_g,
                    is_head,
                    commit.parents.len() > 1,
                    show_wip && vis == 0 && is_head,
                );
            } else {
                ui.painter()
                    .circle_filled(cols.graph.center(), 3.5, lane_color(0));
            }

            let text_color = if selected {
                Color32::WHITE
            } else {
                ui.visuals().text_color()
            };
            let weak = if selected {
                Color32::from_gray(225)
            } else {
                ui.visuals().weak_text_color()
            };

            // Description: ref badges + summary
            let mut x = cols.desc.left();
            if let Some(labels) = tab.refs.labels.get(&commit.oid) {
                for l in labels {
                    x = paint_badge(ui, l, x, rect.center().y, cols.desc.right());
                }
            }
            let desc_rect = Rect::from_min_max(
                pos2(x, rect.top()),
                pos2(cols.desc.right() - 6.0, rect.bottom()),
            );
            clipped_text(
                ui,
                desc_rect,
                &commit.summary,
                FontId::proportional(13.5),
                text_color,
            );
            clipped_text(
                ui,
                cols.date,
                &format_time(commit),
                FontId::proportional(12.5),
                weak,
            );
            clipped_text(
                ui,
                cols.author,
                &commit.author,
                FontId::proportional(12.5),
                weak,
            );
            clipped_text(ui, cols.hash, &commit.short, FontId::monospace(12.5), weak);

            let resp = resp.on_hover_text_at_pointer(format!(
                "{}\n\n{} <{}>\n{}",
                commit.message.trim_end(),
                commit.author,
                commit.email,
                commit.oid
            ));
            if resp.clicked() {
                select = Some(commit.oid);
            }
            let oid_str = commit.oid.to_string();
            let short = commit.short.clone();
            let summary = commit.summary.clone();
            let current = tab.refs.head_name.clone().unwrap_or_else(|| "HEAD".into());
            resp.context_menu(|ui| {
                commit_context_menu(ui, &oid_str, &short, &summary, &current, actions);
            });
        }
    });

    if let Some(oid) = select {
        tab.select_commit(oid);
    }
    if goto_status {
        tab.view = View::FileStatus;
    }
    if load_more {
        actions.push(Action::LoadMore);
    }
}

fn commit_context_menu(
    ui: &mut egui::Ui,
    oid: &str,
    short: &str,
    summary: &str,
    current: &str,
    actions: &mut Vec<Action>,
) {
    if ui.button("Checkout (detached HEAD)").clicked() {
        actions.push(Action::Dialog(Dialog::Confirm {
            title: "Checkout commit".into(),
            text: format!("Checkout {short} in detached HEAD state?"),
            op: Op::CheckoutCommit(oid.into()),
            danger: false,
        }));
        ui.close();
    }
    if ui.button("New branch here…").clicked() {
        actions.push(Action::Dialog(Dialog::NewBranch {
            name: String::new(),
            start: Some(oid.into()),
            start_label: format!("{short} {summary}"),
            checkout: true,
        }));
        ui.close();
    }
    if ui.button("New tag here…").clicked() {
        actions.push(Action::Dialog(Dialog::NewTag {
            name: String::new(),
            commit: oid.into(),
        }));
        ui.close();
    }
    ui.separator();
    if ui.button(format!("Merge into {current}…")).clicked() {
        actions.push(Action::Dialog(Dialog::Confirm {
            title: "Merge".into(),
            text: format!("Merge {short} into '{current}'?"),
            op: Op::Merge(oid.into()),
            danger: false,
        }));
        ui.close();
    }
    if ui.button("Cherry-pick…").clicked() {
        actions.push(Action::Dialog(Dialog::Confirm {
            title: "Cherry-pick".into(),
            text: format!("Apply {short} \"{summary}\" onto '{current}'?"),
            op: Op::CherryPick(oid.into()),
            danger: false,
        }));
        ui.close();
    }
    if ui.button("Revert commit…").clicked() {
        actions.push(Action::Dialog(Dialog::Confirm {
            title: "Revert".into(),
            text: format!("Create a new commit that undoes {short}?"),
            op: Op::Revert(oid.into()),
            danger: false,
        }));
        ui.close();
    }
    ui.menu_button(format!("Reset {current} to this commit"), |ui| {
        for (mode, desc, danger) in [
            ("soft", "Soft — keep all changes staged", false),
            ("mixed", "Mixed — keep changes, unstaged", false),
            ("hard", "Hard — DISCARD all local changes", true),
        ] {
            if ui.button(desc).clicked() {
                actions.push(Action::Dialog(Dialog::Confirm {
                    title: format!("Reset ({mode})"),
                    text: format!(
                        "Reset '{current}' to {short}?{}",
                        if danger {
                            "\n\n⚠ Uncommitted changes will be lost permanently."
                        } else {
                            ""
                        }
                    ),
                    op: Op::ResetTo {
                        commit: oid.into(),
                        mode: mode.into(),
                    },
                    danger,
                }));
                ui.close();
            }
        }
    });
    ui.separator();
    if ui.button("Copy SHA").clicked() {
        ui.ctx().copy_text(oid.into());
        ui.close();
    }
    if ui.button("Copy message").clicked() {
        ui.ctx().copy_text(summary.into());
        ui.close();
    }
}

fn visible_index_of(tab: &RepoTab, oid: git2::Oid) -> Option<usize> {
    match &tab.filtered {
        Some(f) => f.iter().position(|&i| tab.commits[i].oid == oid),
        None => tab.commits.iter().position(|c| c.oid == oid),
    }
}

fn commit_index(tab: &RepoTab, visible: usize) -> usize {
    match &tab.filtered {
        Some(f) => f[visible],
        None => visible,
    }
}

struct Columns {
    graph: Rect,
    desc: Rect,
    date: Rect,
    author: Rect,
    hash: Rect,
}

impl Columns {
    fn new(rect: Rect, graph_w: f32) -> Self {
        let narrow = rect.width() < 900.0;
        let author_w = if narrow { 110.0 } else { COL_AUTHOR };
        let date_w = if narrow { 0.0 } else { COL_DATE };
        let r = rect.right();
        let hash = Rect::from_min_max(pos2(r - COL_HASH, rect.top()), pos2(r, rect.bottom()));
        let author = Rect::from_min_max(
            pos2(hash.left() - author_w, rect.top()),
            pos2(hash.left() - 8.0, rect.bottom()),
        );
        let date = Rect::from_min_max(
            pos2(author.left() - date_w, rect.top()),
            pos2(author.left() - 8.0, rect.bottom()),
        );
        let graph = Rect::from_min_max(rect.min, pos2(rect.left() + graph_w, rect.bottom()));
        let desc = Rect::from_min_max(
            pos2(graph.right() + 4.0, rect.top()),
            pos2(date.left(), rect.bottom()),
        );
        Self {
            graph,
            desc,
            date,
            author,
            hash,
        }
    }
}

fn clipped_text(ui: &egui::Ui, rect: Rect, text: &str, font: FontId, color: Color32) {
    if rect.width() <= 4.0 {
        return;
    }
    let painter = ui.painter().with_clip_rect(rect.intersect(ui.clip_rect()));
    painter.text(
        pos2(rect.left(), rect.center().y),
        egui::Align2::LEFT_CENTER,
        text,
        font,
        color,
    );
}

fn paint_badge(ui: &egui::Ui, label: &RefLabel, x: f32, cy: f32, max_x: f32) -> f32 {
    let (bg, fg, prefix) = match label.kind {
        RefKind::Head => (Color32::from_rgb(56, 161, 105), Color32::WHITE, "✔ "),
        RefKind::Local => (Color32::from_rgb(47, 133, 90), Color32::WHITE, ""),
        RefKind::Remote => (Color32::from_rgb(90, 103, 216), Color32::WHITE, "☁ "),
        RefKind::Tag => (Color32::from_rgb(214, 158, 46), Color32::BLACK, "🏷 "),
    };
    if x + 30.0 > max_x {
        return x;
    }
    let text = format!("{prefix}{}", label.name);
    let galley = ui
        .painter()
        .layout_no_wrap(text, FontId::proportional(11.5), fg);
    let w = (galley.size().x + 10.0).min(max_x - x - 4.0);
    let rect = Rect::from_min_size(pos2(x, cy - 9.0), vec2(w, 18.0));
    let painter = ui.painter().with_clip_rect(rect.intersect(ui.clip_rect()));
    painter.rect_filled(rect, 4.0, bg);
    painter.galley(
        pos2(rect.left() + 5.0, cy - galley.size().y / 2.0),
        galley,
        fg,
    );
    x + w + 4.0
}

fn lane_x(rect: Rect, lane: usize) -> f32 {
    rect.left() + 6.0 + lane as f32 * LANE_W + LANE_W / 2.0
}

fn paint_graph(
    ui: &egui::Ui,
    rect: Rect,
    row: &GraphRow,
    is_head: bool,
    is_merge: bool,
    wip_above: bool,
) {
    let painter = ui.painter().with_clip_rect(rect.intersect(ui.clip_rect()));
    let y = |p: Pos| match p {
        Pos::Top => rect.top(),
        Pos::Mid => rect.center().y,
        Pos::Bottom => rect.bottom(),
    };
    for s in &row.segments {
        let p0 = pos2(lane_x(rect, s.from_lane), y(s.from_pos));
        let p1 = pos2(lane_x(rect, s.to_lane), y(s.to_pos));
        let stroke = Stroke::new(2.0, lane_color(s.color));
        if s.from_lane == s.to_lane {
            painter.line_segment([p0, p1], stroke);
        } else {
            let my = (p0.y + p1.y) / 2.0;
            let shape = egui::epaint::CubicBezierShape::from_points_stroke(
                [p0, pos2(p0.x, my), pos2(p1.x, my), p1],
                false,
                Color32::TRANSPARENT,
                stroke,
            );
            painter.add(shape);
        }
    }
    let c = pos2(lane_x(rect, row.lane), rect.center().y);
    let color = row
        .segments
        .iter()
        .find(|s| s.from_lane == row.lane && s.from_pos == Pos::Mid)
        .or_else(|| {
            row.segments
                .iter()
                .find(|s| s.to_lane == row.lane && s.to_pos == Pos::Mid)
        })
        .map(|s| lane_color(s.color))
        .unwrap_or(lane_color(row.lane));
    if wip_above {
        // dashed connector up to the "Uncommitted changes" row
        painter.line_segment([pos2(c.x, rect.top()), c], Stroke::new(1.5, Color32::GRAY));
    }
    if is_head {
        painter.circle_filled(c, 5.5, Color32::WHITE);
        painter.circle_stroke(c, 5.5, Stroke::new(2.5, color));
    } else if is_merge {
        painter.circle_filled(c, 3.5, color);
    } else {
        painter.circle_filled(c, 4.5, color);
    }
}

fn paint_wip_row(ui: &egui::Ui, rect: Rect, cols: &Columns, tab: &RepoTab, resp: &egui::Response) {
    if resp.hovered() {
        ui.painter()
            .rect_filled(rect, 0.0, ui.visuals().widgets.hovered.weak_bg_fill);
    }
    let lane = tab.graph.first().map(|_| {
        // Put the WIP node above HEAD's lane if HEAD is the first commit, else lane 0.
        tab.commits
            .iter()
            .position(|c| Some(c.oid) == tab.refs.head_oid)
            .and_then(|i| tab.graph.get(i))
            .map(|g| g.lane)
            .unwrap_or(0)
    });
    let c = pos2(lane_x(cols.graph, lane.unwrap_or(0)), rect.center().y);
    let head_is_first = tab
        .commits
        .first()
        .map(|c| Some(c.oid) == tab.refs.head_oid)
        .unwrap_or(false);
    if head_is_first {
        ui.painter().line_segment(
            [c, pos2(c.x, rect.bottom())],
            Stroke::new(1.5, Color32::GRAY),
        );
    }
    ui.painter().circle_filled(c, 5.0, ui.visuals().panel_fill);
    ui.painter()
        .circle_stroke(c, 5.0, Stroke::new(2.0, Color32::GRAY));
    let n = tab.status.staged.len() + tab.status.unstaged.len();
    clipped_text(
        ui,
        cols.desc,
        &format!(
            "Uncommitted changes ({n} file{})",
            if n == 1 { "" } else { "s" }
        ),
        FontId::proportional(13.5),
        ui.visuals().strong_text_color(),
    );
    clipped_text(
        ui,
        cols.hash,
        "*",
        FontId::monospace(12.5),
        ui.visuals().weak_text_color(),
    );
}

fn commit_details(ui: &mut egui::Ui, tab: &mut RepoTab, actions: &mut Vec<Action>) {
    let Some(oid) = tab.selected_commit else {
        ui.centered_and_justified(|ui| {
            ui.label(RichText::new("Select a commit to see its changes").weak());
        });
        return;
    };

    egui::Panel::left("commit_files")
        .resizable(true)
        .default_size(380.0)
        .size_range(220.0..=700.0)
        .show(ui, |ui| {
            egui::ScrollArea::vertical()
                .id_salt("commit_meta_files")
                .auto_shrink(false)
                .show(ui, |ui| {
                    if let Some(c) = tab.commit_by_oid(oid).cloned() {
                        egui::Grid::new("meta")
                            .num_columns(2)
                            .spacing([8.0, 2.0])
                            .show(ui, |ui| {
                                ui.label(RichText::new("Commit").weak());
                                ui.horizontal(|ui| {
                                    ui.label(
                                        RichText::new(c.oid.to_string()).monospace().size(11.5),
                                    );
                                    if ui.small_button("⎘").on_hover_text("Copy SHA").clicked() {
                                        ui.ctx().copy_text(c.oid.to_string());
                                    }
                                });
                                ui.end_row();
                                if !c.parents.is_empty() {
                                    ui.label(RichText::new("Parents").weak());
                                    ui.horizontal(|ui| {
                                        for p in &c.parents {
                                            if ui
                                                .link(
                                                    RichText::new(&p.to_string()[..7]).monospace(),
                                                )
                                                .clicked()
                                            {
                                                tab.select_commit(*p);
                                                tab.scroll_to_selected = true;
                                            }
                                        }
                                    });
                                    ui.end_row();
                                }
                                ui.label(RichText::new("Author").weak());
                                ui.label(format!("{} <{}>", c.author, c.email));
                                ui.end_row();
                                if c.committer != c.author {
                                    ui.label(RichText::new("Committer").weak());
                                    ui.label(&c.committer);
                                    ui.end_row();
                                }
                                ui.label(RichText::new("Date").weak());
                                ui.label(format_time(&c));
                                ui.end_row();
                            });
                        ui.add_space(6.0);
                        ui.label(RichText::new(c.message.trim_end()).strong());
                        ui.separator();
                    } else {
                        // Stash or a commit outside the loaded range
                        ui.label(RichText::new(oid.to_string()).monospace());
                        ui.separator();
                    }
                    let files = tab.commit_files.clone();
                    ui.label(RichText::new(format!("{} file(s) changed", files.len())).weak());
                    for f in &files {
                        let target = DiffTarget::Commit {
                            oid,
                            path: f.path.clone(),
                        };
                        let selected = tab.diff_target.as_ref() == Some(&target);
                        if file_row(ui, f, selected, None).0.clicked() {
                            tab.select_diff(target);
                        }
                    }
                });
        });

    egui::CentralPanel::default().show(ui, |ui| {
        diffview::diff_view(ui, tab, actions);
    });
}

/// A file row: `[button] X path`. Returns (row response, button clicked).
pub fn file_row(
    ui: &mut egui::Ui,
    f: &FileChange,
    selected: bool,
    button: Option<(&str, &str)>,
) -> (egui::Response, bool) {
    let mut clicked_btn = false;
    let inner = ui.horizontal(|ui| {
        if let Some((icon, tip)) = button {
            if ui.small_button(icon).on_hover_text(tip).clicked() {
                clicked_btn = true;
            }
        }
        ui.label(
            RichText::new(f.kind.letter())
                .monospace()
                .strong()
                .color(kind_color(f.kind)),
        );
        let (dir, name) = match f.path.rsplit_once('/') {
            Some((d, n)) => (format!("{d}/"), n.to_string()),
            None => (String::new(), f.path.clone()),
        };
        let mut job = egui::text::LayoutJob::default();
        let (base, weak) = if selected {
            (Color32::WHITE, Color32::from_gray(215))
        } else {
            (ui.visuals().text_color(), ui.visuals().weak_text_color())
        };
        let font = FontId::proportional(13.0);
        job.append(&name, 0.0, egui::TextFormat::simple(font.clone(), base));
        if !dir.is_empty() {
            job.append(
                &format!("  {dir}"),
                0.0,
                egui::TextFormat::simple(font, weak),
            );
        }
        ui.add(egui::Button::selectable(selected, job).truncate())
            .on_hover_text(&f.path)
    });
    (inner.inner, clicked_btn)
}

// ---------------------------------------------------------------------------
// File status view
// ---------------------------------------------------------------------------

pub fn file_status_view(ui: &mut egui::Ui, tab: &mut RepoTab, actions: &mut Vec<Action>) {
    egui::Panel::bottom("commit_box")
        .resizable(true)
        .default_size(170.0)
        .size_range(120.0..=400.0)
        .show(ui, |ui| commit_box(ui, tab, actions));

    egui::Panel::left("status_files")
        .resizable(true)
        .default_size(420.0)
        .size_range(240.0..=800.0)
        .show(ui, |ui| {
            merge_banner(ui, tab, actions);
            let unborn = tab.repo.is_head_unborn();
            let half = (ui.available_height() / 2.0 - 30.0).max(80.0);

            // Staged
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("Staged files ({})", tab.status.staged.len())).strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add_enabled(!tab.status.staged.is_empty(), egui::Button::new("Unstage all"))
                        .clicked()
                    {
                        actions.push(Action::Run(Op::UnstageAll { unborn }));
                    }
                });
            });
            ui.separator();
            egui::ScrollArea::vertical()
                .id_salt("staged")
                .max_height(half)
                .min_scrolled_height(half)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    if tab.status.staged.is_empty() {
                        ui.label(RichText::new("Nothing staged yet").weak());
                    }
                    let files = tab.status.staged.clone();
                    for f in &files {
                        let target = DiffTarget::Staged { path: f.path.clone() };
                        let selected = tab.diff_target.as_ref() == Some(&target);
                        let (r, btn) = file_row(ui, f, selected, Some(("−", "Unstage")));
                        if btn || r.double_clicked() {
                            actions.push(Action::Run(Op::Unstage { paths: vec![f.path.clone()], unborn }));
                        } else if r.clicked() {
                            tab.select_diff(target);
                        }
                        r.context_menu(|ui| {
                            if ui.button("Unstage").clicked() {
                                actions.push(Action::Run(Op::Unstage { paths: vec![f.path.clone()], unborn }));
                                ui.close();
                            }
                            if ui.button("Copy path").clicked() {
                                ui.ctx().copy_text(f.path.clone());
                                ui.close();
                            }
                        });
                    }
                });

            ui.add_space(8.0);
            // Unstaged
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("Unstaged files ({})", tab.status.unstaged.len())).strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let any = !tab.status.unstaged.is_empty();
                    if ui.add_enabled(any, egui::Button::new("Stage all")).clicked() {
                        actions.push(Action::Run(Op::StageAll));
                    }
                    if ui.add_enabled(any, egui::Button::new("Discard all…")).clicked() {
                        let (tracked, untracked) = split_tracked(&tab.status.unstaged);
                        actions.push(Action::Dialog(Dialog::Confirm {
                            title: "Discard changes".into(),
                            text: format!(
                                "Discard ALL unstaged changes in {} file(s)?\nUntracked files will be deleted.\nThis cannot be undone.",
                                tab.status.unstaged.len()
                            ),
                            op: Op::Discard { tracked, untracked },
                            danger: true,
                        }));
                    }
                });
            });
            ui.separator();
            egui::ScrollArea::vertical()
                .id_salt("unstaged")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    if tab.status.unstaged.is_empty() {
                        ui.label(RichText::new("Working copy is clean ✔").weak());
                    }
                    let files = tab.status.unstaged.clone();
                    for f in &files {
                        let target = DiffTarget::Unstaged { path: f.path.clone() };
                        let selected = tab.diff_target.as_ref() == Some(&target);
                        let (r, btn) = file_row(ui, f, selected, Some(("+", "Stage")));
                        if btn || r.double_clicked() {
                            actions.push(Action::Run(Op::Stage(vec![f.path.clone()])));
                        } else if r.clicked() {
                            tab.select_diff(target);
                        }
                        r.context_menu(|ui| {
                            if ui.button("Stage").clicked() {
                                actions.push(Action::Run(Op::Stage(vec![f.path.clone()])));
                                ui.close();
                            }
                            if ui.button("Discard changes…").clicked() {
                                let (tracked, untracked) = split_tracked(std::slice::from_ref(f));
                                actions.push(Action::Dialog(discard_dialog(f, tracked, untracked)));
                                ui.close();
                            }
                            if ui.button("Copy path").clicked() {
                                ui.ctx().copy_text(f.path.clone());
                                ui.close();
                            }
                        });
                    }
                });
        });

    egui::CentralPanel::default().show(ui, |ui| {
        diffview::diff_view(ui, tab, actions);
    });
}

pub fn discard_dialog(f: &FileChange, tracked: Vec<String>, untracked: Vec<String>) -> Dialog {
    let what = if f.kind == ChangeKind::Untracked {
        "Delete untracked file"
    } else {
        "Discard changes to"
    };
    Dialog::Confirm {
        title: "Discard changes".into(),
        text: format!("{what} '{}'?\nThis cannot be undone.", f.path),
        op: Op::Discard { tracked, untracked },
        danger: true,
    }
}

pub fn split_tracked(files: &[FileChange]) -> (Vec<String>, Vec<String>) {
    let mut tracked = Vec::new();
    let mut untracked = Vec::new();
    for f in files {
        if f.kind == ChangeKind::Untracked {
            untracked.push(f.path.clone());
        } else {
            tracked.push(f.path.clone());
        }
    }
    (tracked, untracked)
}

fn commit_box(ui: &mut egui::Ui, tab: &mut RepoTab, actions: &mut Vec<Action>) {
    ui.add_space(4.0);
    let branch = tab.refs.head_name.clone().unwrap_or_else(|| "HEAD".into());
    ui.horizontal(|ui| {
        ui.label(RichText::new(format!("Commit to ⑂ {branch}")).strong());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(RichText::new("Ctrl+Enter to commit").weak().small());
        });
    });
    let editor = egui::TextEdit::multiline(&mut tab.commit_msg)
        .hint_text("Commit message (first line = summary)")
        .desired_width(f32::INFINITY)
        .desired_rows(3);
    let avail = (ui.available_height() - 36.0).max(40.0);
    let resp = ui.add_sized([ui.available_width(), avail], editor);

    let can_commit =
        !tab.commit_msg.trim().is_empty() && (!tab.status.staged.is_empty() || tab.amend);
    let shortcut =
        resp.has_focus() && ui.input(|i| i.modifiers.command && i.key_pressed(egui::Key::Enter));

    ui.horizontal(|ui| {
        let was = tab.amend;
        ui.add_enabled_ui(!tab.repo.is_head_unborn(), |ui| {
            ui.checkbox(&mut tab.amend, "Amend last commit");
        });
        if tab.amend && !was && tab.commit_msg.trim().is_empty() {
            if let Some(m) = tab.repo.head_message() {
                tab.commit_msg = m;
            }
        }
        ui.checkbox(&mut tab.push_after_commit, "Push changes immediately");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let btn = egui::Button::new(RichText::new("  Commit  ").strong()).fill(if can_commit {
                Color32::from_rgb(38, 110, 200)
            } else {
                ui.visuals().widgets.inactive.bg_fill
            });
            if ui.add_enabled(can_commit, btn).clicked() || (shortcut && can_commit) {
                actions.push(Action::Run(Op::Commit {
                    message: tab.commit_msg.trim_end().to_string() + "\n",
                    amend: tab.amend,
                    push: tab.push_after_commit,
                }));
            }
            if tab.status.staged.is_empty() && !tab.amend {
                ui.label(RichText::new("Stage files to commit").weak());
            }
        });
    });
}
