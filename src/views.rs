//! Main views: sidebar, history (commit graph) and file status (staging + commit).

use crate::app::{push_dialog_for, Action, Dialog, RepoTab, View};
use crate::branchtree;
use crate::diffview;
use crate::git::{
    ChangeKind, CommitInfo, DiffTarget, FileChange, LocalBranch, Op, RefKind, RefLabel,
    RemoteBranch,
};
use crate::graph::{GraphRow, Pos};
use crate::theme::{self, pal};
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

pub fn kind_color(p: &theme::Palette, k: ChangeKind) -> Color32 {
    match k {
        ChangeKind::Added | ChangeKind::Untracked => p.green,
        ChangeKind::Modified | ChangeKind::TypeChange => p.yellow,
        ChangeKind::Deleted => p.red,
        ChangeKind::Renamed => p.blue,
        ChangeKind::Conflicted => p.pink,
    }
}

pub fn format_time(c: &CommitInfo) -> String {
    let tz = chrono::FixedOffset::east_opt(c.offset_min * 60)
        .unwrap_or_else(|| chrono::FixedOffset::east_opt(0).unwrap());
    chrono::DateTime::from_timestamp(c.time, 0)
        .map(|d| d.with_timezone(&tz).format("%d %b %Y %H:%M").to_string())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Sidebar
// ---------------------------------------------------------------------------

const SIDE_ROW_H: f32 = 24.0;
const TREE_INDENT: f32 = 14.0;

/// One row of the sidebar (nav item, folder, branch, tag, stash).
struct SideRow<'a> {
    depth: usize,
    /// Reserve the disclosure column (tree rows); `caret` = Some(open) for folders.
    tree: bool,
    caret: Option<bool>,
    icon: &'a str,
    icon_color: Color32,
    label: &'a str,
    label_color: Color32,
    selected: bool,
    /// Small monospace chips on the right (e.g. "↑2").
    chips: Vec<(String, Color32)>,
    /// Count pill on the right.
    count: Option<usize>,
}

impl<'a> SideRow<'a> {
    fn new(ui: &egui::Ui, icon: &'a str, label: &'a str) -> Self {
        let p = pal(ui);
        Self {
            depth: 0,
            tree: false,
            caret: None,
            icon,
            icon_color: p.subtext,
            label,
            label_color: p.text,
            selected: false,
            chips: Vec::new(),
            count: None,
        }
    }
}

fn side_row(ui: &mut egui::Ui, r: SideRow<'_>) -> egui::Response {
    let (rect, resp) =
        ui.allocate_exact_size(vec2(ui.available_width(), SIDE_ROW_H), Sense::click());
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    let p = pal(ui);
    let painter = ui.painter().clone();
    if r.selected {
        painter.rect_filled(rect, 6.0, p.selection);
        painter.rect_filled(
            Rect::from_min_size(
                rect.left_top() + vec2(0.0, 5.0),
                vec2(3.0, SIDE_ROW_H - 10.0),
            ),
            1.5,
            p.accent,
        );
    } else if resp.hovered() {
        painter.rect_filled(rect, 6.0, p.hover);
    }
    let cy = rect.center().y;
    let mut x = rect.left() + 8.0 + r.depth as f32 * TREE_INDENT;
    if r.tree {
        if let Some(open) = r.caret {
            let c = if resp.hovered() { p.text } else { p.faint };
            theme::paint_caret(&painter, pos2(x + 4.0, cy), open, c);
        }
        x += 13.0;
    }
    if !r.icon.is_empty() {
        painter.text(
            pos2(x + 7.0, cy),
            egui::Align2::CENTER_CENTER,
            r.icon,
            FontId::proportional(12.5),
            if r.selected {
                p.accent_hover
            } else {
                r.icon_color
            },
        );
        x += 21.0;
    }

    // right side: count pill, then chips
    let mut right = rect.right() - 6.0;
    if let Some(n) = r.count {
        let t = n.to_string();
        let w = theme::pill_width(&painter, &t);
        theme::paint_pill(&painter, right - w, cy, &t, p.surface1, p.subtext);
        right -= w + 6.0;
    }
    for (t, c) in r.chips.iter().rev() {
        let g = painter.layout_no_wrap(t.clone(), FontId::monospace(10.5), *c);
        right -= g.size().x;
        let gh = g.size().y;
        painter.galley(pos2(right, cy - gh / 2.0), g, *c);
        right -= 5.0;
    }

    let color = if r.selected {
        p.sel_text
    } else {
        r.label_color
    };
    let g = painter.layout_no_wrap(r.label.to_string(), FontId::proportional(13.0), color);
    let clip = Rect::from_min_max(pos2(x, rect.top()), pos2(right - 2.0, rect.bottom()))
        .intersect(ui.clip_rect());
    let clipped = g.size().x > clip.width();
    let gh = g.size().y;
    painter
        .with_clip_rect(clip)
        .galley(pos2(x, cy - gh / 2.0), g, color);
    if clipped && clip.width() > 12.0 {
        // fade out instead of a hard cut
        let bg = if r.selected {
            p.selection
        } else if resp.hovered() {
            p.hover
        } else {
            p.mantle
        };
        let fade = Rect::from_min_max(
            pos2(clip.right() - 14.0, rect.top()),
            pos2(clip.right(), rect.bottom()),
        );
        let mut mesh = egui::Mesh::default();
        mesh.colored_vertex(fade.left_top(), theme::alpha(bg, 0.0));
        mesh.colored_vertex(fade.right_top(), bg);
        mesh.colored_vertex(fade.left_bottom(), theme::alpha(bg, 0.0));
        mesh.colored_vertex(fade.right_bottom(), bg);
        mesh.add_triangle(0, 1, 2);
        mesh.add_triangle(1, 2, 3);
        painter.add(mesh);
    }
    resp
}

/// Collapsible section header ("BRANCHES 12"). Returns whether it is open.
fn section(ui: &mut egui::Ui, title: &str, count: usize) -> bool {
    let id = egui::Id::unique(("sidebar_section", title));
    let mut open = ui.ctx().data_mut(|d| *d.get_persisted_mut_or(id, true));
    ui.add_space(10.0);
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 20.0), Sense::click());
    let p = pal(ui);
    let painter = ui.painter();
    let color = if resp.hovered() { p.text } else { p.faint };
    let cy = rect.center().y;
    painter.text(
        pos2(rect.left() + 8.0, cy),
        egui::Align2::LEFT_CENTER,
        title,
        FontId::proportional(10.5),
        color,
    );
    if resp.hovered() || !open {
        theme::paint_caret(painter, pos2(rect.right() - 10.0, cy), open, color);
    }
    if count > 0 {
        painter.text(
            pos2(rect.right() - 22.0, cy),
            egui::Align2::RIGHT_CENTER,
            count.to_string(),
            FontId::proportional(10.5),
            p.faint,
        );
    }
    if resp.clicked() {
        open = !open;
        ui.ctx().data_mut(|d| d.insert_persisted(id, open));
    }
    ui.add_space(2.0);
    open
}

/// Open state of a tree folder, remembered per repository.
fn folder_open(ui: &egui::Ui, key: (&str, &str, &str), default: bool) -> (egui::Id, bool) {
    let id = egui::Id::unique(("branch_folder", key.0, key.1, key.2));
    let open = ui.ctx().data_mut(|d| *d.get_persisted_mut_or(id, default));
    (id, open)
}

fn folder_row(
    ui: &mut egui::Ui,
    depth: usize,
    name: &str,
    open: bool,
    count: usize,
) -> egui::Response {
    let p = pal(ui);
    let mut row = SideRow::new(ui, if open { "🗁" } else { "🗀" }, name);
    row.depth = depth;
    row.tree = true;
    row.caret = Some(open);
    row.icon_color = p.yellow;
    row.label_color = p.text;
    row.count = (!open).then_some(count);
    side_row(ui, row)
}

pub fn sidebar(ui: &mut egui::Ui, tab: &mut RepoTab, actions: &mut Vec<Action>) {
    let p = pal(ui);
    egui::Panel::left("sidebar")
        .resizable(true)
        .default_size(250.0)
        .size_range(170.0..=480.0)
        .frame(theme::panel(p.mantle, 8, 6))
        .show(ui, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink(false)
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 1.0;
                    ui.add_space(2.0);
                    let changes = tab.status.staged.len() + tab.status.unstaged.len();
                    let mut row = SideRow::new(ui, "📝", "File Status");
                    row.selected = tab.view == View::FileStatus;
                    row.count = (changes > 0).then_some(changes);
                    if side_row(ui, row).clicked() {
                        tab.view = View::FileStatus;
                    }
                    let mut row = SideRow::new(ui, "🕘", "History");
                    row.selected = tab.view == View::History;
                    if side_row(ui, row).clicked() {
                        tab.view = View::History;
                    }

                    let locals = tab.refs.locals.clone();
                    if section(ui, "BRANCHES", locals.len()) {
                        if locals.is_empty() {
                            empty_row(ui, "No branches yet");
                        }
                        let names: Vec<&str> = locals.iter().map(|b| b.name.as_str()).collect();
                        let tree = branchtree::build(&names);
                        let head = locals.iter().position(|b| b.is_head);
                        local_folder(ui, tab, &tree, 0, &locals, head, actions);
                    }

                    let remotes = tab.refs.remotes.clone();
                    let names = tab.refs.remote_names.clone();
                    if section(ui, "REMOTES", names.len()) {
                        if names.is_empty() {
                            empty_row(ui, "No remotes");
                        }
                        let repo_key = tab.path.display().to_string();
                        for remote in &names {
                            let rbs: Vec<&RemoteBranch> =
                                remotes.iter().filter(|r| &r.remote == remote).collect();
                            let (id, mut open) =
                                folder_open(ui, (&repo_key, "remote", remote), true);
                            let mut row = SideRow::new(ui, "☁", remote);
                            row.tree = true;
                            row.caret = Some(open);
                            row.icon_color = p.blue;
                            row.count = (!open).then_some(rbs.len());
                            if side_row(ui, row).clicked() {
                                open = !open;
                                ui.ctx().data_mut(|d| d.insert_persisted(id, open));
                            }
                            if open {
                                let bnames: Vec<&str> =
                                    rbs.iter().map(|r| r.branch.as_str()).collect();
                                let tree = branchtree::build(&bnames);
                                remote_folder(ui, tab, &tree, 1, remote, &rbs, &locals, actions);
                            }
                        }
                    }

                    let tags = tab.refs.tags.clone();
                    if section(ui, "TAGS", tags.len()) {
                        if tags.is_empty() {
                            empty_row(ui, "No tags");
                        }
                        for (name, oid) in &tags {
                            let mut row = SideRow::new(ui, "🏷", name);
                            row.icon_color = p.yellow;
                            let r = side_row(ui, row);
                            if r.clicked() {
                                jump_to(tab, *oid);
                            }
                            r.context_menu(|ui| {
                                if ui.button("Checkout (detached)").clicked() {
                                    actions.push(Action::Run(Op::CheckoutCommit(name.clone())));
                                    ui.close();
                                }
                                if ui.button("Copy name").clicked() {
                                    ui.ctx().copy_text(name.clone());
                                    ui.close();
                                }
                                ui.separator();
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
                    }

                    let stashes = tab.refs.stashes.clone();
                    if section(ui, "STASHES", stashes.len()) {
                        if stashes.is_empty() {
                            empty_row(ui, "No stashes");
                        }
                        for s in &stashes {
                            let mut row = SideRow::new(ui, "📦", &s.message);
                            row.icon_color = p.purple;
                            let r = side_row(ui, row).on_hover_text(format!(
                                "stash@{{{}}}: {}\nRight-click to apply / pop / drop",
                                s.index, s.message
                            ));
                            if r.clicked() {
                                tab.view = View::History;
                                tab.selected_commit = Some(s.oid);
                                tab.commit_files = tab.repo.commit_files(s.oid).unwrap_or_default();
                                let first = tab.commit_files.first().map(|f| f.path.clone());
                                tab.diff_target =
                                    first.map(|path| DiffTarget::Commit { oid: s.oid, path });
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
                                ui.separator();
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
                    }
                    ui.add_space(12.0);
                });
        });
}

fn empty_row(ui: &mut egui::Ui, text: &str) {
    let p = pal(ui);
    let mut row = SideRow::new(ui, "", text);
    row.label_color = p.faint;
    row.tree = true;
    side_row(ui, row);
}

fn local_folder(
    ui: &mut egui::Ui,
    tab: &mut RepoTab,
    f: &branchtree::Folder,
    depth: usize,
    locals: &[LocalBranch],
    head: Option<usize>,
    actions: &mut Vec<Action>,
) {
    let repo_key = tab.path.display().to_string();
    for sub in &f.folders {
        let default = head.is_some_and(|h| sub.contains(h));
        let (id, mut open) = folder_open(ui, (&repo_key, "local", &sub.path), default);
        if folder_row(ui, depth, &sub.name, open, sub.count())
            .on_hover_text(format!("{}/ · {} branch(es)", sub.path, sub.count()))
            .clicked()
        {
            open = !open;
            ui.ctx().data_mut(|d| d.insert_persisted(id, open));
        }
        if open {
            local_folder(ui, tab, sub, depth + 1, locals, head, actions);
        }
    }
    for leaf in &f.leaves {
        local_branch_row(ui, tab, &locals[leaf.index], &leaf.label, depth, actions);
    }
}

fn local_branch_row(
    ui: &mut egui::Ui,
    tab: &mut RepoTab,
    b: &LocalBranch,
    label: &str,
    depth: usize,
    actions: &mut Vec<Action>,
) {
    let p = pal(ui);
    let mut row = SideRow::new(ui, if b.is_head { "●" } else { theme::BRANCH }, label);
    row.depth = depth;
    row.tree = true;
    if b.is_head {
        row.icon_color = p.green;
        row.label_color = p.text_strong;
    } else {
        row.icon_color = p.faint;
    }
    if b.ahead > 0 {
        row.chips.push((format!("↑{}", b.ahead), p.accent_hover));
    }
    if b.behind > 0 {
        row.chips.push((format!("↓{}", b.behind), p.orange));
    }
    let r = side_row(ui, row);
    let tip = match &b.upstream {
        Some(up) => format!("{}\nTracking {up}\nDouble-click to checkout", b.name),
        None => format!("{}\nDouble-click to checkout", b.name),
    };
    let r = r.on_hover_text(tip);
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
        if ui
            .add_enabled(!b.is_head, egui::Button::new("Checkout"))
            .clicked()
        {
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

#[allow(clippy::too_many_arguments)]
fn remote_folder(
    ui: &mut egui::Ui,
    tab: &mut RepoTab,
    f: &branchtree::Folder,
    depth: usize,
    remote: &str,
    rbs: &[&RemoteBranch],
    locals: &[LocalBranch],
    actions: &mut Vec<Action>,
) {
    let repo_key = tab.path.display().to_string();
    for sub in &f.folders {
        let key = format!("{remote}/{}", sub.path);
        let (id, mut open) = folder_open(ui, (&repo_key, "remote", &key), false);
        if folder_row(ui, depth, &sub.name, open, sub.count())
            .on_hover_text(format!(
                "{remote}/{}/ · {} branch(es)",
                sub.path,
                sub.count()
            ))
            .clicked()
        {
            open = !open;
            ui.ctx().data_mut(|d| d.insert_persisted(id, open));
        }
        if open {
            remote_folder(ui, tab, sub, depth + 1, remote, rbs, locals, actions);
        }
    }
    let p = pal(ui);
    for leaf in &f.leaves {
        let rb = rbs[leaf.index];
        let mut row = SideRow::new(ui, theme::BRANCH, &leaf.label);
        row.depth = depth;
        row.tree = true;
        row.icon_color = p.faint;
        let r = side_row(ui, row).on_hover_text(format!("{}\nDouble-click to checkout", rb.name));
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
const COL_GAP: f32 = 10.0;
/// Description never gets narrower than this; Date hides first, then Author shrinks.
const DESC_MIN: f32 = 140.0;

/// History column widths the user dragged to (remembered across restarts).
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
struct ColWidths {
    /// `None` = fit the graph's lanes.
    graph: Option<f32>,
    date: f32,
    author: f32,
    hash: f32,
}

impl Default for ColWidths {
    fn default() -> Self {
        Self {
            graph: None,
            date: COL_DATE,
            author: COL_AUTHOR,
            hash: COL_HASH,
        }
    }
}

fn col_widths_id() -> egui::Id {
    egui::Id::unique("history_column_widths")
}

pub fn history_view(ui: &mut egui::Ui, tab: &mut RepoTab, actions: &mut Vec<Action>) {
    // Show something useful right away: the newest commit, or the selected commit's own diff
    // again when coming back from File Status (both views share the diff pane).
    let reselect = match (&tab.selected_commit, &tab.diff_target) {
        (None, _) => tab.commits.first().map(|c| c.oid),
        (Some(oid), Some(DiffTarget::Staged { .. } | DiffTarget::Unstaged { .. })) => Some(*oid),
        _ => None,
    };
    if let Some(oid) = reselect {
        tab.select_commit(oid);
    }

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
            let r = ui.add(
                egui::TextEdit::singleline(&mut tab.search)
                    .hint_text("🔍  Search message, author or SHA…")
                    .margin(vec2(10.0, 6.0))
                    .desired_width(340.0),
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
        ui.add_space(6.0);
        commit_list(ui, tab, actions);
    });
}

fn merge_banner(ui: &mut egui::Ui, tab: &mut RepoTab, actions: &mut Vec<Action>) {
    let state = tab.repo.repo.state();
    if state == git2::RepositoryState::Clean {
        return;
    }
    let p = pal(ui);
    egui::Frame::new()
        .fill(theme::alpha(p.yellow, 0.12))
        .stroke(Stroke::new(1.0, theme::alpha(p.yellow, 0.35)))
        .inner_margin(egui::Margin::symmetric(10, 6))
        .corner_radius(8.0)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!(
                        "⚠  Repository state: {state:?} — resolve conflicts, then commit."
                    ))
                    .color(p.yellow),
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
    ui.add_space(6.0);
}

fn commit_list(ui: &mut egui::Ui, tab: &mut RepoTab, actions: &mut Vec<Action>) {
    let show_graph = tab.filtered.is_none();
    let mut widths: ColWidths = ui
        .ctx()
        .data_mut(|d| d.get_persisted(col_widths_id()))
        .unwrap_or_default();
    let auto_graph = LANE_W * tab.graph_lanes as f32 + 10.0;
    let mut graph_w = if show_graph {
        widths.graph.unwrap_or(auto_graph)
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

    // Header with draggable column dividers
    let header_h = 22.0;
    let (hrect, _) = ui.allocate_exact_size(vec2(ui.available_width(), header_h), Sense::hover());
    let pl = pal(ui);
    if column_dividers(ui, hrect, show_graph, auto_graph, &mut widths) {
        ui.ctx()
            .data_mut(|d| d.insert_persisted(col_widths_id(), widths));
        if show_graph {
            graph_w = widths.graph.unwrap_or(auto_graph);
        }
    }
    let cols = Columns::new(hrect, graph_w, &widths);
    let painter = ui.painter();
    let weak = pl.faint;
    let font = FontId::proportional(11.5);
    painter.line_segment(
        [hrect.left_bottom(), hrect.right_bottom()],
        Stroke::new(1.0, pl.border),
    );
    for (rect, label) in [
        (cols.graph.translate(vec2(4.0, 0.0)), "Graph"),
        (cols.desc, "Description"),
        (cols.date, "Date"),
        (cols.author, "Author"),
        (cols.hash, "Commit"),
    ] {
        clipped_text(ui, rect, label, font.clone(), weak);
    }
    ui.add_space(2.0);

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

    let pl = pal(ui);
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
            let cols = Columns::new(rect, graph_w, &widths);

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
            let row_bg = if selected {
                pl.selection
            } else if resp.hovered() {
                pl.hover
            } else if vis % 2 == 1 {
                pl.stripe
            } else {
                pl.base
            };
            if row_bg != pl.base {
                ui.painter().rect_filled(rect, 0.0, row_bg);
            }
            if selected {
                ui.painter().rect_filled(
                    Rect::from_min_size(rect.left_top(), vec2(3.0, rect.height())),
                    0.0,
                    pl.accent,
                );
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
                    row_bg,
                );
            } else {
                ui.painter()
                    .circle_filled(cols.graph.center(), 3.5, lane_color(0));
            }

            let text_color = if selected { pl.sel_text } else { pl.text };
            let weak = if selected { pl.text } else { pl.subtext };

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
            clipped_text(
                ui,
                cols.hash,
                &commit.short,
                FontId::monospace(12.0),
                if selected { pl.text } else { pl.faint },
            );

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
    fn new(rect: Rect, graph_w: f32, w: &ColWidths) -> Self {
        // Too narrow? Hide Date first, then shrink Author.
        let room = rect.width() - graph_w - 4.0 - DESC_MIN;
        let mut date_w = w.date;
        let mut author_w = w.author;
        let hash_w = w.hash;
        if date_w + author_w + hash_w + 3.0 * COL_GAP > room {
            date_w = 0.0;
        }
        if author_w + hash_w + 2.0 * COL_GAP > room {
            author_w = (room - hash_w - 2.0 * COL_GAP).max(60.0);
        }
        let (top, bottom) = (rect.top(), rect.bottom());
        let r = rect.right();
        let hash = Rect::from_min_max(pos2(r - hash_w, top), pos2(r, bottom));
        let author = Rect::from_min_max(
            pos2(hash.left() - COL_GAP - author_w, top),
            pos2(hash.left() - COL_GAP, bottom),
        );
        let date = if date_w > 0.0 {
            Rect::from_min_max(
                pos2(author.left() - COL_GAP - date_w, top),
                pos2(author.left() - COL_GAP, bottom),
            )
        } else {
            Rect::from_min_max(pos2(author.left(), top), pos2(author.left(), bottom))
        };
        let graph = Rect::from_min_max(rect.min, pos2(rect.left() + graph_w, bottom));
        let desc = Rect::from_min_max(
            pos2(graph.right() + 4.0, top),
            pos2(date.left() - COL_GAP, bottom),
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

/// Drag the dividers in the header to resize Graph / Date / Author / Commit (Description takes
/// the rest); double-click a divider to reset that column. Returns true if a width changed.
fn column_dividers(
    ui: &mut egui::Ui,
    hrect: Rect,
    show_graph: bool,
    auto_graph: f32,
    w: &mut ColWidths,
) -> bool {
    #[derive(Clone, Copy)]
    enum Col {
        Graph,
        Date,
        Author,
        Hash,
    }
    let graph_w = if show_graph {
        w.graph.unwrap_or(auto_graph)
    } else {
        16.0
    };
    let cols = Columns::new(hrect, graph_w, w);
    let mut dividers = Vec::new();
    if show_graph {
        dividers.push((Col::Graph, cols.graph.right() + 2.0));
    }
    if cols.date.width() > 0.0 {
        dividers.push((Col::Date, cols.date.left() - COL_GAP / 2.0));
    }
    dividers.push((Col::Author, cols.author.left() - COL_GAP / 2.0));
    dividers.push((Col::Hash, cols.hash.left() - COL_GAP / 2.0));

    let p = pal(ui);
    let mut changed = false;
    for (i, (col, x)) in dividers.into_iter().enumerate() {
        let hit = Rect::from_center_size(pos2(x, hrect.center().y), vec2(9.0, hrect.height()));
        let r = ui.interact(
            hit,
            ui.scope_id().with(("col_divider", i)),
            Sense::click_and_drag(),
        );
        let active = r.hovered() || r.dragged();
        if active {
            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
        }
        ui.painter().line_segment(
            [pos2(x, hrect.top() + 5.0), pos2(x, hrect.bottom() - 5.0)],
            Stroke::new(
                if active { 2.0 } else { 1.0 },
                if active { p.accent } else { p.border_strong },
            ),
        );
        if r.double_clicked() {
            let d = ColWidths::default();
            match col {
                Col::Graph => w.graph = None,
                Col::Date => w.date = d.date,
                Col::Author => w.author = d.author,
                Col::Hash => w.hash = d.hash,
            }
            changed = true;
            continue;
        }
        let dx = r.drag_delta().x;
        if dx == 0.0 {
            continue;
        }
        // Keep Description at least DESC_MIN wide.
        let used = graph_w + 4.0 + w.date + w.author + w.hash + 3.0 * COL_GAP;
        let spare = (hrect.width() - DESC_MIN - used).max(0.0);
        match col {
            Col::Graph => {
                w.graph = Some((graph_w + dx.min(spare)).clamp(16.0, 700.0));
            }
            Col::Date => w.date = (w.date - dx.max(-spare)).clamp(70.0, 320.0),
            Col::Author => w.author = (w.author - dx.max(-spare)).clamp(60.0, 400.0),
            Col::Hash => w.hash = (w.hash - dx.max(-spare)).clamp(50.0, 240.0),
        }
        changed = true;
    }
    changed
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
    let p = pal(ui);
    let (color, prefix) = match label.kind {
        RefKind::Head => (p.green, "✔ "),
        RefKind::Local => (p.green, ""),
        RefKind::Remote => (p.purple, "☁ "),
        RefKind::Tag => (p.yellow, "🏷 "),
    };
    if x + 30.0 > max_x {
        return x;
    }
    let text = format!("{prefix}{}", label.name);
    let galley = ui
        .painter()
        .layout_no_wrap(text, FontId::proportional(11.5), color);
    let w = (galley.size().x + 12.0).min(max_x - x - 4.0);
    let rect = Rect::from_min_size(pos2(x, cy - 9.0), vec2(w, 18.0));
    let painter = ui.painter().with_clip_rect(rect.intersect(ui.clip_rect()));
    let head = label.kind == RefKind::Head;
    painter.rect_filled(
        rect,
        5.0,
        theme::alpha(color, if head { 0.26 } else { 0.15 }),
    );
    painter.rect_stroke(
        rect,
        5.0,
        Stroke::new(1.0, theme::alpha(color, if head { 0.8 } else { 0.4 })),
        egui::StrokeKind::Inside,
    );
    painter.galley(
        pos2(rect.left() + 6.0, cy - galley.size().y / 2.0),
        galley,
        color,
    );
    x + w + 5.0
}

fn lane_x(rect: Rect, lane: usize) -> f32 {
    rect.left() + 6.0 + lane as f32 * LANE_W + LANE_W / 2.0
}

#[allow(clippy::too_many_arguments)]
fn paint_graph(
    ui: &egui::Ui,
    rect: Rect,
    row: &GraphRow,
    is_head: bool,
    is_merge: bool,
    wip_above: bool,
    row_bg: Color32,
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
    let pl = pal(ui);
    if wip_above {
        // connector up to the "Uncommitted changes" row
        painter.line_segment([pos2(c.x, rect.top()), c], Stroke::new(1.5, pl.faint));
    }
    // a ring in the row color separates the node from the lines running through it
    if is_head {
        painter.circle_filled(c, 7.5, row_bg);
        painter.circle_filled(c, 5.5, row_bg);
        painter.circle_stroke(c, 5.5, Stroke::new(2.5, color));
        painter.circle_filled(c, 2.2, color);
    } else if is_merge {
        painter.circle_filled(c, 5.0, row_bg);
        painter.circle_filled(c, 3.5, color);
    } else {
        painter.circle_filled(c, 6.0, row_bg);
        painter.circle_filled(c, 4.5, color);
    }
}

fn paint_wip_row(ui: &egui::Ui, rect: Rect, cols: &Columns, tab: &RepoTab, resp: &egui::Response) {
    let pl = pal(ui);
    if resp.hovered() {
        ui.painter().rect_filled(rect, 0.0, pl.hover);
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
        ui.painter()
            .line_segment([c, pos2(c.x, rect.bottom())], Stroke::new(1.5, pl.faint));
    }
    ui.painter().circle_filled(c, 5.0, pl.base);
    ui.painter()
        .circle_stroke(c, 5.0, Stroke::new(2.0, pl.orange));
    let n = tab.status.staged.len() + tab.status.unstaged.len();
    clipped_text(
        ui,
        cols.desc,
        &format!(
            "Uncommitted changes ({n} file{})",
            if n == 1 { "" } else { "s" }
        ),
        FontId::proportional(13.5),
        pl.orange,
    );
    clipped_text(ui, cols.hash, "*", FontId::monospace(12.0), pl.faint);
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
        let p = pal(ui);
        let kc = kind_color(p, f.kind);
        let (br, _) = ui.allocate_exact_size(vec2(18.0, 18.0), Sense::hover());
        ui.painter().rect_filled(br, 5.0, theme::alpha(kc, 0.16));
        ui.painter().text(
            br.center(),
            egui::Align2::CENTER_CENTER,
            f.kind.letter(),
            FontId::monospace(11.0),
            kc,
        );
        let (dir, name) = match f.path.rsplit_once('/') {
            Some((d, n)) => (format!("{d}/"), n.to_string()),
            None => (String::new(), f.path.clone()),
        };
        let mut job = egui::text::LayoutJob::default();
        let (base, weak) = if selected {
            (p.sel_text, p.text)
        } else {
            (p.text, p.faint)
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
    // Pick the first changed file so the diff pane isn't empty (or still showing a commit).
    if !matches!(
        tab.diff_target,
        Some(DiffTarget::Staged { .. } | DiffTarget::Unstaged { .. })
    ) {
        let first = tab
            .status
            .unstaged
            .first()
            .map(|f| DiffTarget::Unstaged {
                path: f.path.clone(),
            })
            .or_else(|| {
                tab.status.staged.first().map(|f| DiffTarget::Staged {
                    path: f.path.clone(),
                })
            });
        if let Some(t) = first {
            tab.select_diff(t);
        }
    }

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
                let p = pal(ui);
                ui.label(RichText::new("Staged files").strong().color(p.text_strong));
                ui.label(RichText::new(tab.status.staged.len().to_string()).color(p.faint));
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
                let p = pal(ui);
                ui.label(RichText::new("Unstaged files").strong().color(p.text_strong));
                ui.label(RichText::new(tab.status.unstaged.len().to_string()).color(p.faint));
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
        if matches!(tab.diff_target, Some(DiffTarget::Commit { .. })) {
            // working copy is clean: don't show the History selection here
            ui.centered_and_justified(|ui| {
                ui.label(RichText::new("No local changes").weak());
            });
        } else {
            diffview::diff_view(ui, tab, actions);
        }
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
        let p = pal(ui);
        ui.label(RichText::new("Commit to").color(p.subtext));
        ui.label(
            RichText::new(format!("{} {branch}", theme::BRANCH))
                .strong()
                .color(p.text_strong),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(
                RichText::new(format!("{} to commit", theme::shortcut("Enter")))
                    .weak()
                    .small(),
            );
        });
    });
    let editor = egui::TextEdit::multiline(&mut tab.commit_msg)
        .hint_text("Commit message (first line = summary)")
        .margin(vec2(10.0, 8.0))
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
            let label = if tab.push_after_commit {
                "  Commit & Push  "
            } else {
                "  Commit  "
            };
            if theme::primary_button(ui, label, can_commit).clicked() || (shortcut && can_commit) {
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
