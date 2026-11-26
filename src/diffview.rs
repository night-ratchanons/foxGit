//! Diff viewer: virtualized, colored unified diff with line numbers,
//! plus hunk / line staging for working-copy diffs.

use crate::app::{Action, Dialog, RepoTab};
use crate::git::{ChangeKind, DiffTarget, FileDiff, LineKind, Op};
use crate::views::{discard_dialog, split_tracked};
use eframe::egui;
use egui::{pos2, vec2, Color32, FontId, Rect, RichText, Sense, Stroke};

const LINE_H: f32 = 18.0;

/// Which partial operations the current diff supports.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Partial {
    /// Unstaged diff: stage or discard parts.
    Unstaged,
    /// Staged diff: unstage parts.
    Staged,
}

fn partial_mode(tab: &RepoTab) -> Option<Partial> {
    let Some(Ok(diff)) = &tab.diff else {
        return None;
    };
    if diff.binary || diff.truncated || diff.hunks.is_empty() {
        return None;
    }
    // New / deleted / untracked / conflicted files are handled per file.
    match tab.diff_target.as_ref()? {
        DiffTarget::Unstaged { path } => tab
            .status
            .unstaged
            .iter()
            .find(|f| &f.path == path)
            .filter(|f| f.kind == ChangeKind::Modified)
            .map(|_| Partial::Unstaged),
        DiffTarget::Staged { path } => tab
            .status
            .staged
            .iter()
            .find(|f| &f.path == path)
            .filter(|f| f.kind == ChangeKind::Modified)
            .map(|_| Partial::Staged),
        DiffTarget::Commit { .. } => None,
    }
}

#[derive(Clone, Copy)]
enum PartialOp {
    Stage,
    Unstage,
    Discard,
}

fn partial_op(
    tab: &RepoTab,
    diff: &FileDiff,
    hunks: &[usize],
    lines: Option<&std::collections::BTreeSet<usize>>,
    op: PartialOp,
) -> Option<Action> {
    let (cached, reverse) = match op {
        PartialOp::Stage => (true, false),
        PartialOp::Unstage => (true, true),
        PartialOp::Discard => (false, true),
    };
    let patch = match lines {
        Some(sel) => diff.build_patch(hunks, &|i| sel.contains(&i), reverse)?,
        None => diff.build_patch(hunks, &|_| true, reverse)?,
    };
    let what = match (lines, hunks.len()) {
        (Some(sel), _) => format!("{} line(s)", sel.len()),
        (None, 1) => "hunk".to_string(),
        (None, n) => format!("{n} hunks"),
    };
    let label = match op {
        PartialOp::Stage => format!("Stage {what}"),
        PartialOp::Unstage => format!("Unstage {what}"),
        PartialOp::Discard => format!("Discard {what}"),
    };
    let apply = Op::ApplyPatch {
        patch,
        cached,
        reverse,
        zero_context: tab.context_lines == 0,
        label: label.clone(),
    };
    Some(match op {
        PartialOp::Discard => Action::Dialog(Dialog::Confirm {
            title: "Discard changes".into(),
            text: format!("{label} from the working copy?\nThis cannot be undone."),
            op: apply,
            danger: true,
        }),
        _ => Action::Run(apply),
    })
}

pub fn diff_view(ui: &mut egui::Ui, tab: &mut RepoTab, actions: &mut Vec<Action>) {
    let Some(target) = tab.diff_target.clone() else {
        ui.centered_and_justified(|ui| {
            ui.label(RichText::new("Select a file to see its diff").weak());
        });
        return;
    };

    let path = match &target {
        DiffTarget::Commit { path, .. }
        | DiffTarget::Staged { path }
        | DiffTarget::Unstaged { path } => path.clone(),
    };
    let partial = partial_mode(tab);

    // ---------------------------------------------------------------- header
    ui.horizontal(|ui| {
        ui.label(RichText::new(&path).strong().monospace());
        if let Some(Ok(d)) = &tab.diff {
            ui.label(RichText::new(format!("+{}", d.added)).color(Color32::from_rgb(72, 187, 120)));
            ui.label(
                RichText::new(format!("−{}", d.removed)).color(Color32::from_rgb(235, 90, 90)),
            );
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            match &target {
                DiffTarget::Staged { path } => {
                    if ui.button("Unstage file").clicked() {
                        let unborn = tab.repo.is_head_unborn();
                        actions.push(Action::Run(Op::Unstage {
                            paths: vec![path.clone()],
                            unborn,
                        }));
                    }
                }
                DiffTarget::Unstaged { path } => {
                    if ui.button("Stage file").clicked() {
                        actions.push(Action::Run(Op::Stage(vec![path.clone()])));
                    }
                    if let Some(f) = tab.status.unstaged.iter().find(|f| &f.path == path) {
                        if f.kind != ChangeKind::Conflicted && ui.button("Discard file…").clicked()
                        {
                            let (t, u) = split_tracked(std::slice::from_ref(f));
                            actions.push(Action::Dialog(discard_dialog(f, t, u)));
                        }
                    }
                }
                DiffTarget::Commit { .. } => {}
            }
            ui.separator();
            if ui.small_button("+").on_hover_text("More context").clicked() {
                tab.context_lines = (tab.context_lines + 3).min(100);
                tab.reload_diff();
            }
            ui.label(
                RichText::new(format!("context {}", tab.context_lines))
                    .weak()
                    .small(),
            );
            if ui.small_button("−").on_hover_text("Less context").clicked() {
                tab.context_lines = tab.context_lines.saturating_sub(3);
                tab.reload_diff();
            }
        });
    });

    // Selection bar (line staging). Fixed height so the diff below never jumps
    // when the bar appears — the next click must land on the line under the pointer.
    if partial.is_some() {
        let bar_h = 30.0;
        ui.allocate_ui_with_layout(
            vec2(ui.available_width(), bar_h),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                ui.set_min_height(bar_h);
            if let (Some(mode), Some(Ok(diff))) = (partial, &tab.diff) {
                let accent = ui.visuals().selection.bg_fill;
                if tab.line_sel.is_empty() {
                    ui.add_space(7.0);
                    ui.label(
                        RichText::new("Tip: click or drag over + / − lines to pick exact lines, or use the buttons on each hunk.")
                            .small()
                            .weak(),
                    );
                } else {
                    let mut clear = false;
                    egui::Frame::new()
                        .fill(accent.gamma_multiply(0.25))
                        .corner_radius(6.0)
                        .inner_margin(egui::Margin::symmetric(8, 4))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                let n = tab.line_sel.len();
                                ui.label(RichText::new(format!("{n} line(s) selected")).strong());
                                let mut hunks: Vec<usize> = tab
                                    .line_sel
                                    .iter()
                                    .filter_map(|&i| diff.hunk_of(i))
                                    .collect();
                                hunks.dedup();
                                let sel = tab.line_sel.clone();
                                match mode {
                                    Partial::Unstaged => {
                                        if ui.button(RichText::new("Stage lines").strong()).clicked() {
                                            actions.extend(partial_op(
                                                tab,
                                                diff,
                                                &hunks,
                                                Some(&sel),
                                                PartialOp::Stage,
                                            ));
                                        }
                                        if ui.button("Discard lines…").clicked() {
                                            actions.extend(partial_op(
                                                tab,
                                                diff,
                                                &hunks,
                                                Some(&sel),
                                                PartialOp::Discard,
                                            ));
                                        }
                                    }
                                    Partial::Staged => {
                                        if ui.button(RichText::new("Unstage lines").strong()).clicked() {
                                            actions.extend(partial_op(
                                                tab,
                                                diff,
                                                &hunks,
                                                Some(&sel),
                                                PartialOp::Unstage,
                                            ));
                                        }
                                    }
                                }
                                if ui.button("Clear").clicked() {
                                    clear = true;
                                }
                                ui.label(
                                    RichText::new("Shift+click extends · Esc clears")
                                        .small()
                                        .weak(),
                                );
                            });
                        });
                    if clear || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                        tab.line_sel.clear();
                        tab.sel_anchor = None;
                    }
                }
            }
            },
        );
    }
    ui.separator();

    let diff = match &tab.diff {
        Some(Ok(d)) => d.clone(),
        Some(Err(e)) => {
            ui.colored_label(Color32::from_rgb(230, 90, 80), e);
            return;
        }
        None => return,
    };

    if diff.binary {
        ui.centered_and_justified(|ui| {
            ui.label(RichText::new("Binary file — no text preview").weak())
        });
        return;
    }
    if diff.lines.is_empty() {
        ui.centered_and_justified(|ui| {
            ui.label(RichText::new("No textual changes (empty file or mode change)").weak())
        });
        return;
    }

    // ---------------------------------------------------------------- body
    let font = FontId::monospace(12.5);
    let char_w = ui
        .painter()
        .layout_no_wrap("M".into(), font.clone(), Color32::WHITE)
        .size()
        .x;
    let gutter = char_w * 12.0 + 12.0;
    let max_chars = diff
        .lines
        .iter()
        .map(|l| l.text.chars().count() + l.text.matches('\t').count() * 3)
        .max()
        .unwrap_or(0);
    let content_w = gutter + char_w * (max_chars as f32 + 4.0);

    let dark = ui.visuals().dark_mode;
    let (add_bg, del_bg, hunk_bg) = if dark {
        (
            Color32::from_rgba_unmultiplied(46, 160, 67, 50),
            Color32::from_rgba_unmultiplied(248, 81, 73, 50),
            Color32::from_rgba_unmultiplied(56, 139, 253, 35),
        )
    } else {
        (
            Color32::from_rgb(220, 255, 228),
            Color32::from_rgb(255, 225, 225),
            Color32::from_rgb(221, 236, 255),
        )
    };
    let accent = ui.visuals().selection.bg_fill;
    let sel_bg = accent.gamma_multiply(if dark { 0.45 } else { 0.30 });
    let text_color = ui.visuals().text_color();
    let weak = ui.visuals().weak_text_color();
    let gutter_bg = ui.visuals().faint_bg_color;

    let pointer_down = ui.input(|i| i.pointer.primary_down());
    let shift = ui.input(|i| i.modifiers.shift);
    if !pointer_down {
        tab.sel_dragging = false;
    }

    ui.spacing_mut().item_spacing.y = 0.0;
    let n = diff.lines.len() + usize::from(diff.truncated);
    let mut pending: Option<(Vec<usize>, PartialOp)> = None;

    egui::ScrollArea::both()
        .id_salt(("diff", &path))
        .auto_shrink(false)
        .show_rows(ui, LINE_H, n, |ui, range| {
            let width = content_w.max(ui.available_width());
            for i in range {
                if i >= diff.lines.len() {
                    let (rect, _) = ui.allocate_exact_size(vec2(width, LINE_H), Sense::hover());
                    ui.painter().text(
                        pos2(rect.left() + gutter, rect.center().y),
                        egui::Align2::LEFT_CENTER,
                        "… diff truncated (too large) — stage the whole file",
                        font.clone(),
                        weak,
                    );
                    continue;
                }
                let line = &diff.lines[i];
                let changed = matches!(line.kind, LineKind::Added | LineKind::Removed);
                let selectable = partial.is_some() && changed;
                let sense = if selectable {
                    Sense::click_and_drag()
                } else {
                    Sense::hover()
                };
                let (rect, resp) = ui.allocate_exact_size(vec2(width, LINE_H), sense);

                // Selection handling
                if selectable {
                    if resp.drag_started() {
                        if !shift {
                            tab.line_sel.clear();
                        }
                        tab.sel_anchor = Some(i);
                        tab.sel_dragging = true;
                        tab.line_sel.insert(i);
                    } else if resp.clicked() {
                        if shift {
                            if let Some(a) = tab.sel_anchor {
                                select_range(tab, &diff, a, i);
                            }
                        } else if !tab.line_sel.remove(&i) {
                            tab.line_sel.insert(i);
                        }
                        tab.sel_anchor = Some(i);
                    }
                    if tab.sel_dragging && pointer_down && ui.rect_contains_pointer(rect) {
                        if let Some(a) = tab.sel_anchor {
                            select_range(tab, &diff, a, i);
                        }
                    }
                }
                let is_sel = tab.line_sel.contains(&i);

                let painter = ui.painter();
                let bg = match line.kind {
                    LineKind::Added => Some(add_bg),
                    LineKind::Removed => Some(del_bg),
                    LineKind::Hunk => Some(hunk_bg),
                    _ => None,
                };
                if let Some(bg) = bg {
                    painter.rect_filled(rect, 0.0, bg);
                }
                if is_sel {
                    painter.rect_filled(rect, 0.0, sel_bg);
                } else if selectable && resp.hovered() {
                    painter.rect_filled(rect, 0.0, accent.gamma_multiply(0.12));
                }
                let g = Rect::from_min_size(rect.min, vec2(gutter - 6.0, LINE_H));
                painter.rect_filled(g, 0.0, gutter_bg);
                if is_sel {
                    painter.rect_filled(
                        Rect::from_min_size(rect.min, vec2(3.0, LINE_H)),
                        0.0,
                        accent,
                    );
                } else if selectable && resp.hovered() {
                    painter.line_segment(
                        [
                            rect.left_top() + vec2(1.5, 2.0),
                            rect.left_bottom() + vec2(1.5, -2.0),
                        ],
                        Stroke::new(2.0, accent.gamma_multiply(0.6)),
                    );
                }
                let num = |n: Option<u32>| {
                    n.map(|n| format!("{n:>5}"))
                        .unwrap_or_else(|| "     ".into())
                };
                if line.kind != LineKind::Hunk {
                    painter.text(
                        pos2(rect.left() + 5.0, rect.center().y),
                        egui::Align2::LEFT_CENTER,
                        format!("{} {}", num(line.old_no), num(line.new_no)),
                        font.clone(),
                        weak,
                    );
                }
                let (prefix, color) = match line.kind {
                    LineKind::Added => ("+", text_color),
                    LineKind::Removed => ("-", text_color),
                    LineKind::Context => (" ", text_color),
                    LineKind::Hunk => ("", Color32::from_rgb(110, 160, 240)),
                    LineKind::Meta => ("", weak),
                };
                let text = format!("{prefix}{}", line.text.replace('\t', "    "));
                painter.text(
                    pos2(rect.left() + gutter, rect.center().y),
                    egui::Align2::LEFT_CENTER,
                    text,
                    font.clone(),
                    color,
                );

                // Hunk buttons, pinned to the right edge of the visible area
                if line.kind == LineKind::Hunk {
                    if let (Some(mode), Some(h)) =
                        (partial, diff.hunks.iter().position(|h| h.header == i))
                    {
                        let mut right = rect.right().min(ui.clip_rect().right()) - 6.0;
                        let buttons: &[(&str, PartialOp)] = match mode {
                            Partial::Unstaged => &[
                                ("Discard hunk", PartialOp::Discard),
                                ("Stage hunk", PartialOp::Stage),
                            ],
                            Partial::Staged => &[("Unstage hunk", PartialOp::Unstage)],
                        };
                        for (label, op) in buttons {
                            let galley = ui.painter().layout_no_wrap(
                                label.to_string(),
                                FontId::proportional(11.5),
                                text_color,
                            );
                            let w = galley.size().x + 14.0;
                            let r = Rect::from_min_max(
                                pos2(right - w, rect.top() + 1.5),
                                pos2(right, rect.bottom() - 1.5),
                            );
                            let b = ui.interact(
                                r,
                                ui.scope_id().with(("hunk_btn", i, *label)),
                                Sense::click(),
                            );
                            let v = if b.hovered() {
                                &ui.visuals().widgets.hovered
                            } else {
                                &ui.visuals().widgets.inactive
                            };
                            let fill = match op {
                                PartialOp::Discard if b.hovered() => Color32::from_rgb(150, 50, 50),
                                PartialOp::Stage | PartialOp::Unstage => {
                                    if b.hovered() {
                                        accent
                                    } else {
                                        accent.gamma_multiply(0.7)
                                    }
                                }
                                _ => v.bg_fill,
                            };
                            ui.painter().rect_filled(r, 4.0, fill);
                            let tc = if matches!(op, PartialOp::Discard) && !b.hovered() {
                                v.fg_stroke.color
                            } else {
                                Color32::WHITE
                            };
                            ui.painter().galley(
                                pos2(
                                    r.center().x - galley.size().x / 2.0,
                                    r.center().y - galley.size().y / 2.0,
                                ),
                                galley,
                                tc,
                            );
                            if b.clicked() {
                                pending = Some((vec![h], *op));
                            }
                            right -= w + 6.0;
                        }
                    }
                }
            }
        });

    if let Some((hunks, op)) = pending {
        actions.extend(partial_op(tab, &diff, &hunks, None, op));
    }
}

/// Select every changed line between `a` and `b` (inclusive).
fn select_range(tab: &mut RepoTab, diff: &FileDiff, a: usize, b: usize) {
    let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
    for j in lo..=hi.min(diff.lines.len().saturating_sub(1)) {
        if matches!(diff.lines[j].kind, LineKind::Added | LineKind::Removed) {
            tab.line_sel.insert(j);
        }
    }
}
