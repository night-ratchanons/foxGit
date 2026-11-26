//! The Changes tab of an agent chat: everything the agent changed in its copy compared with the
//! branch it started from (commits + uncommitted + new files), file by file. Select lines, write
//! a comment, collect a few, and send them to the agent as one review message.

use crate::theme::{self, pal};
use eframe::egui;
use egui::{pos2, vec2, Color32, FontId, RichText, Sense};
use std::path::Path;
use std::time::Instant;

#[derive(Clone, Debug, PartialEq)]
pub struct FileStat {
    pub path: String,
    /// A, M, D, R, T or ? (new, untracked).
    pub status: char,
    pub added: usize,
    pub removed: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Hunk,
    Context,
    Added,
    Removed,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub kind: Kind,
    pub old: Option<u32>,
    pub new: Option<u32>,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Comment {
    pub path: String,
    /// New-side (or old-side for removed lines) line numbers.
    pub lines: (u32, u32),
    pub quote: String,
    pub text: String,
}

/// Where the agent's branch left the base: merge-base of HEAD and `base`.
fn base_tree<'r>(repo: &'r git2::Repository, base: &str) -> Result<git2::Tree<'r>, String> {
    let head = repo
        .head()
        .ok()
        .and_then(|h| h.target())
        .ok_or("the agent's copy has no commits")?;
    let b = repo
        .revparse_single(base)
        .and_then(|o| o.peel_to_commit())
        .map_err(|e| format!("can't find {base}: {}", e.message()))?
        .id();
    let mb = repo.merge_base(head, b).unwrap_or(b);
    repo.find_commit(mb)
        .and_then(|c| c.tree())
        .map_err(|e| e.message().to_string())
}

fn diff<'r>(
    repo: &'r git2::Repository,
    base: &str,
    path: Option<&str>,
) -> Result<git2::Diff<'r>, String> {
    let tree = base_tree(repo, base)?;
    let mut o = git2::DiffOptions::new();
    o.include_untracked(true)
        .recurse_untracked_dirs(true)
        .show_untracked_content(true)
        .include_typechange(true)
        .context_lines(3);
    if let Some(p) = path {
        o.pathspec(p).disable_pathspec_match(true);
    }
    let mut d = repo
        .diff_tree_to_workdir_with_index(Some(&tree), Some(&mut o))
        .map_err(|e| e.message().to_string())?;
    let mut f = git2::DiffFindOptions::new();
    f.renames(true);
    let _ = d.find_similar(Some(&mut f));
    Ok(d)
}

/// Every file the agent touched.
pub fn list(worktree: &Path, base: &str) -> Result<Vec<FileStat>, String> {
    let repo = git2::Repository::open(worktree).map_err(|e| e.message().to_string())?;
    let d = diff(&repo, base, None)?;
    let mut out = Vec::new();
    for (i, delta) in d.deltas().enumerate() {
        let path = delta
            .new_file()
            .path()
            .or(delta.old_file().path())
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        let status = match delta.status() {
            git2::Delta::Added => 'A',
            git2::Delta::Deleted => 'D',
            git2::Delta::Renamed => 'R',
            git2::Delta::Typechange => 'T',
            git2::Delta::Untracked => '?',
            _ => 'M',
        };
        let (added, removed) = git2::Patch::from_diff(&d, i)
            .ok()
            .flatten()
            .and_then(|p| p.line_stats().ok())
            .map(|(_, a, r)| (a, r))
            .unwrap_or((0, 0));
        out.push(FileStat {
            path,
            status,
            added,
            removed,
        });
    }
    Ok(out)
}

/// Diff rows of one file. `Ok(None)` for binary files.
pub fn rows(worktree: &Path, base: &str, path: &str) -> Result<Option<Vec<Row>>, String> {
    let repo = git2::Repository::open(worktree).map_err(|e| e.message().to_string())?;
    let d = diff(&repo, base, Some(path))?;
    let Some(patch) = git2::Patch::from_diff(&d, 0).map_err(|e| e.message().to_string())? else {
        return Ok(Some(Vec::new()));
    };
    if patch.delta().flags().is_binary() {
        return Ok(None);
    }
    let mut out = Vec::new();
    for h in 0..patch.num_hunks() {
        let (hunk, n) = patch.hunk(h).map_err(|e| e.message().to_string())?;
        out.push(Row {
            kind: Kind::Hunk,
            old: None,
            new: None,
            text: String::from_utf8_lossy(hunk.header())
                .trim_end()
                .to_string(),
        });
        for l in 0..n {
            let line = patch
                .line_in_hunk(h, l)
                .map_err(|e| e.message().to_string())?;
            let kind = match line.origin() {
                '+' => Kind::Added,
                '-' => Kind::Removed,
                ' ' => Kind::Context,
                _ => continue, // "\ No newline at end of file"
            };
            out.push(Row {
                kind,
                old: line.old_lineno(),
                new: line.new_lineno(),
                text: String::from_utf8_lossy(line.content())
                    .trim_end_matches(['\n', '\r'])
                    .to_string(),
            });
        }
    }
    Ok(Some(out))
}

/// One message with every comment, quoting the lines they're about.
pub fn review_message(comments: &[Comment]) -> String {
    let mut s = String::from("Review comments on your changes — please address each one:\n");
    for (i, c) in comments.iter().enumerate() {
        let where_ = if c.lines.0 == c.lines.1 {
            format!("line {}", c.lines.0)
        } else {
            format!("lines {}–{}", c.lines.0, c.lines.1)
        };
        s.push_str(&format!(
            "\n{}. `{}` {where_}:\n```\n{}\n```\n{}\n",
            i + 1,
            c.path,
            c.quote.trim_end(),
            c.text.trim()
        ));
    }
    s
}

#[derive(Default)]
pub struct Review {
    files: Option<Result<Vec<FileStat>, String>>,
    loaded: Option<Instant>,
    selected: Option<String>,
    rows: Option<Result<Option<Vec<Row>>, String>>,
    sel: Option<(usize, usize)>,
    anchor: Option<usize>,
    dragging: bool,
    comment: String,
    pub pending: Vec<Comment>,
}

pub enum ReviewAction {
    /// Send this message to the agent.
    Send(String),
    Open(String, Option<u32>),
}

impl Review {
    /// Re-read the file list (and the open file) on the next frame.
    pub fn invalidate(&mut self) {
        self.files = None;
        self.rows = None;
    }

    fn load(&mut self, worktree: &Path, base: &str) {
        if self.files.is_none() {
            let files = list(worktree, base);
            if let Ok(f) = &files {
                if self
                    .selected
                    .as_ref()
                    .is_none_or(|s| !f.iter().any(|x| &x.path == s))
                {
                    self.selected = f.first().map(|x| x.path.clone());
                    self.rows = None;
                    self.sel = None;
                }
            }
            self.files = Some(files);
            self.loaded = Some(Instant::now());
        }
        if self.rows.is_none() {
            if let Some(path) = &self.selected {
                self.rows = Some(rows(worktree, base, path));
            }
        }
    }

    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        worktree: &Path,
        base: &str,
        ready: bool,
        busy: bool,
    ) -> Vec<ReviewAction> {
        let mut out = Vec::new();
        let p = pal(ui);
        if !ready {
            ui.centered_and_justified(|ui| {
                ui.label(
                    RichText::new(
                        "No changes yet — the agent's copy is made with the first message.",
                    )
                    .weak(),
                );
            });
            return out;
        }
        // live while the agent works
        if busy
            && self
                .loaded
                .is_some_and(|t| t.elapsed() > std::time::Duration::from_secs(3))
        {
            self.invalidate();
        }
        if busy {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_secs(3));
        }
        self.load(worktree, base);
        // ---- top bar
        ui.horizontal(|ui| {
            let n = self
                .files
                .as_ref()
                .and_then(|f| f.as_ref().ok())
                .map_or(0, |f| f.len());
            let (a, r) = self
                .files
                .as_ref()
                .and_then(|f| f.as_ref().ok())
                .map(|f| {
                    f.iter()
                        .fold((0, 0), |(a, r), x| (a + x.added, r + x.removed))
                })
                .unwrap_or((0, 0));
            ui.label(RichText::new(format!("{n} file(s) changed since {base}")).color(p.subtext));
            ui.label(RichText::new(format!("+{a}")).monospace().color(p.green));
            ui.label(RichText::new(format!("−{r}")).monospace().color(p.red));
            if ui.small_button("⟳").on_hover_text("Reload").clicked() {
                self.invalidate();
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if !self.pending.is_empty() {
                    let label = format!("Send {} comment(s) to the agent", self.pending.len());
                    if theme::primary_button(ui, &label, true)
                        .on_hover_text(if busy {
                            "Queued until the agent finishes this step"
                        } else {
                            "One message with every comment"
                        })
                        .clicked()
                    {
                        out.push(ReviewAction::Send(review_message(&self.pending)));
                        self.pending.clear();
                    }
                    if ui.small_button("Clear").clicked() {
                        self.pending.clear();
                    }
                }
            });
        });
        ui.add_space(4.0);
        let files = match &self.files {
            Some(Ok(f)) => f.clone(),
            Some(Err(e)) => {
                ui.label(RichText::new(e).color(p.red));
                return out;
            }
            None => return out,
        };
        if files.is_empty() {
            ui.centered_and_justified(|ui| {
                ui.label(RichText::new("No changes yet").weak());
            });
            return out;
        }

        // ---- file list
        egui::Panel::left("agent_review_files")
            .resizable(true)
            .default_size(260.0)
            .size_range(160.0..=520.0)
            .frame(egui::Frame::new())
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("agent_review_list")
                    .auto_shrink(false)
                    .show(ui, |ui| {
                        for f in &files {
                            let sel = self.selected.as_deref() == Some(f.path.as_str());
                            let (rect, resp) = ui.allocate_exact_size(
                                vec2(ui.available_width(), 26.0),
                                Sense::click(),
                            );
                            let painter = ui.painter();
                            if sel {
                                painter.rect_filled(rect, 6.0, p.selection);
                            } else if resp.hovered() {
                                painter.rect_filled(rect, 6.0, p.hover);
                            }
                            let c = match f.status {
                                'A' | '?' => p.green,
                                'D' => p.red,
                                'R' => p.blue,
                                _ => p.yellow,
                            };
                            let letter = if f.status == '?' { 'A' } else { f.status };
                            painter.text(
                                pos2(rect.left() + 10.0, rect.center().y),
                                egui::Align2::CENTER_CENTER,
                                letter,
                                FontId::monospace(11.5),
                                c,
                            );
                            let stats = format!("+{} −{}", f.added, f.removed);
                            let sg =
                                painter.layout_no_wrap(stats, FontId::monospace(10.5), p.faint);
                            let sw = sg.size().x;
                            painter.galley(
                                pos2(rect.right() - sw - 6.0, rect.center().y - sg.size().y / 2.0),
                                sg,
                                p.faint,
                            );
                            let (dir, name) = match f.path.rsplit_once('/') {
                                Some((d, n)) => (format!("{d}/"), n.to_string()),
                                None => (String::new(), f.path.clone()),
                            };
                            let clip = egui::Rect::from_min_max(
                                pos2(rect.left() + 20.0, rect.top()),
                                pos2(rect.right() - sw - 12.0, rect.bottom()),
                            );
                            let tp = painter.with_clip_rect(clip.intersect(ui.clip_rect()));
                            let ng = tp.layout_no_wrap(
                                name,
                                FontId::proportional(12.5),
                                if sel { p.sel_text } else { p.text_strong },
                            );
                            let nw = ng.size().x;
                            tp.galley(
                                pos2(rect.left() + 22.0, rect.center().y - ng.size().y / 2.0),
                                ng,
                                p.text,
                            );
                            tp.text(
                                pos2(rect.left() + 28.0 + nw, rect.center().y),
                                egui::Align2::LEFT_CENTER,
                                dir,
                                FontId::proportional(11.0),
                                p.faint,
                            );
                            let n = self.pending.iter().filter(|c| c.path == f.path).count();
                            if n > 0 {
                                painter.circle_filled(
                                    pos2(rect.left() + 3.0, rect.center().y),
                                    3.0,
                                    p.accent,
                                );
                            }
                            if resp.on_hover_text(&f.path).clicked() && !sel {
                                self.selected = Some(f.path.clone());
                                self.rows = None;
                                self.sel = None;
                                self.anchor = None;
                            }
                        }
                    });
            });

        // ---- the diff, with a fixed comment bar on top
        let Some(path) = self.selected.clone() else {
            return out;
        };
        let rows = match &self.rows {
            Some(Ok(Some(r))) => r.clone(),
            Some(Ok(None)) => {
                ui.label(RichText::new("Binary file").weak());
                return out;
            }
            Some(Err(e)) => {
                ui.label(RichText::new(e).color(p.red));
                return out;
            }
            None => return out,
        };
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(&path)
                    .monospace()
                    .strong()
                    .color(p.text_strong),
            );
            if ui
                .small_button("↗")
                .on_hover_text("Open in your editor")
                .clicked()
            {
                let line = self
                    .sel
                    .and_then(|(a, _)| rows.get(a))
                    .and_then(|r| r.new.or(r.old));
                out.push(ReviewAction::Open(path.clone(), line));
            }
        });
        let bar_h = 34.0;
        ui.allocate_ui(vec2(ui.available_width(), bar_h), |ui| {
            ui.horizontal(|ui| match self.sel {
                None => {
                    ui.label(
                        RichText::new("Click or drag over lines to comment on them")
                            .small()
                            .color(p.faint),
                    );
                }
                Some((a, b)) => {
                    let nums: Vec<u32> =
                        rows[a..=b].iter().filter_map(|r| r.new.or(r.old)).collect();
                    let (lo, hi) = (
                        nums.iter().min().copied().unwrap_or(0),
                        nums.iter().max().copied().unwrap_or(0),
                    );
                    ui.label(
                        RichText::new(if lo == hi {
                            format!("Line {lo}")
                        } else {
                            format!("Lines {lo}–{hi}")
                        })
                        .color(p.accent_hover),
                    );
                    let w = (ui.available_width() - 190.0).max(120.0);
                    let r = ui.add(
                        egui::TextEdit::singleline(&mut self.comment)
                            .hint_text("Comment for the agent…")
                            .desired_width(w),
                    );
                    let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    let ok = !self.comment.trim().is_empty();
                    let make = |text: &str| Comment {
                        path: path.clone(),
                        lines: (lo, hi),
                        quote: rows[a..=b]
                            .iter()
                            .filter(|r| r.kind != Kind::Hunk)
                            .map(|r| {
                                let m = match r.kind {
                                    Kind::Added => "+",
                                    Kind::Removed => "-",
                                    _ => " ",
                                };
                                format!("{m}{}", r.text)
                            })
                            .collect::<Vec<_>>()
                            .join("\n"),
                        text: text.to_string(),
                    };
                    if ui
                        .add_enabled(ok, egui::Button::new("Add"))
                        .on_hover_text("Collect it; send all at once")
                        .clicked()
                        || (enter && ok)
                    {
                        self.pending.push(make(&self.comment));
                        self.comment.clear();
                        self.sel = None;
                    }
                    if ui
                        .add_enabled(ok, egui::Button::new("Ask now"))
                        .on_hover_text("Send just this comment")
                        .clicked()
                    {
                        out.push(ReviewAction::Send(review_message(&[make(&self.comment)])));
                        self.comment.clear();
                        self.sel = None;
                    }
                    if ui.small_button("×").clicked() {
                        self.sel = None;
                    }
                }
            });
        });
        let font = FontId::monospace(12.5);
        let row_h = ui.fonts_mut(|f| f.row_height(&font)) + 3.0;
        let commented: Vec<(u32, u32)> = self
            .pending
            .iter()
            .filter(|c| c.path == path)
            .map(|c| c.lines)
            .collect();
        let (mut clicked, mut hovered_row) = (None, None);
        egui::ScrollArea::both()
            .id_salt("agent_review_diff")
            .auto_shrink(false)
            .show_rows(ui, row_h, rows.len(), |ui, range| {
                for i in range {
                    let r = &rows[i];
                    let text = r.text.replace('\t', "    ");
                    let g = ui.painter().layout_no_wrap(text, font.clone(), p.text);
                    let w = (g.size().x + 110.0).max(ui.available_width());
                    let (rect, resp) =
                        ui.allocate_exact_size(vec2(w, row_h), Sense::click_and_drag());
                    let in_sel = self.sel.is_some_and(|(a, b)| i >= a && i <= b);
                    let painter = ui.painter();
                    let bg = match r.kind {
                        Kind::Added => theme::alpha(p.green, 0.13),
                        Kind::Removed => theme::alpha(p.red, 0.13),
                        Kind::Hunk => theme::alpha(p.blue, 0.10),
                        Kind::Context => Color32::TRANSPARENT,
                    };
                    painter.rect_filled(rect, 0.0, bg);
                    if in_sel {
                        painter.rect_filled(rect, 0.0, theme::alpha(p.accent, 0.25));
                        painter.rect_filled(
                            egui::Rect::from_min_size(rect.left_top(), vec2(3.0, row_h)),
                            0.0,
                            p.accent,
                        );
                    } else if resp.hovered() && r.kind != Kind::Hunk {
                        painter.rect_filled(rect, 0.0, theme::alpha(p.accent, 0.08));
                    }
                    let n = r.new.or(r.old);
                    if n.is_some_and(|n| commented.iter().any(|(a, b)| n >= *a && n <= *b)) {
                        painter.circle_filled(
                            pos2(rect.left() + 6.0, rect.center().y),
                            3.0,
                            p.accent,
                        );
                    }
                    if let Some(o) = r.old {
                        painter.text(
                            pos2(rect.left() + 44.0, rect.center().y),
                            egui::Align2::RIGHT_CENTER,
                            o.to_string(),
                            FontId::monospace(11.0),
                            p.faint,
                        );
                    }
                    if let Some(nn) = r.new {
                        painter.text(
                            pos2(rect.left() + 82.0, rect.center().y),
                            egui::Align2::RIGHT_CENTER,
                            nn.to_string(),
                            FontId::monospace(11.0),
                            p.faint,
                        );
                    }
                    let (mark, fg) = match r.kind {
                        Kind::Added => ("+", p.green),
                        Kind::Removed => ("−", p.red),
                        Kind::Hunk => ("", p.blue),
                        Kind::Context => ("", p.text),
                    };
                    painter.text(
                        pos2(rect.left() + 90.0, rect.center().y),
                        egui::Align2::LEFT_CENTER,
                        mark,
                        font.clone(),
                        fg,
                    );
                    let color = if r.kind == Kind::Hunk { p.blue } else { p.text };
                    painter.galley(pos2(rect.left() + 104.0, rect.top() + 1.5), g, color);
                    if r.kind != Kind::Hunk {
                        if resp.drag_started() || resp.clicked() {
                            clicked = Some((i, ui.input(|x| x.modifiers.shift)));
                        }
                        if resp.contains_pointer() {
                            hovered_row = Some(i);
                        }
                    }
                }
            });
        let down = ui.input(|i| i.pointer.primary_down());
        if let Some((i, shift)) = clicked {
            match (shift, self.anchor) {
                (true, Some(a)) => self.sel = Some((a.min(i), a.max(i))),
                _ => {
                    self.anchor = Some(i);
                    self.sel = Some((i, i));
                }
            }
            self.dragging = true;
        } else if self.dragging && down {
            if let (Some(a), Some(h)) = (self.anchor, hovered_row) {
                self.sel = Some((a.min(h), a.max(h)));
            }
        }
        if !down {
            self.dragging = false;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;

    fn git(d: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .args(args)
            .current_dir(d)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "T")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "T")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    }

    #[test]
    fn lists_commits_uncommitted_and_new_files_since_the_base() {
        let t = tempfile::tempdir().unwrap();
        let d = t.path();
        git(d, &["init", "-q", "-b", "main"]);
        fs::write(d.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        fs::write(d.join("gone.txt"), "bye\n").unwrap();
        git(d, &["add", "."]);
        git(d, &["commit", "-q", "-m", "base"]);
        git(d, &["checkout", "-q", "-b", "agent/x"]);
        fs::write(d.join("a.txt"), "one\nTWO\nthree\n").unwrap();
        git(d, &["commit", "-q", "-am", "agent commit"]);
        fs::remove_file(d.join("gone.txt")).unwrap();
        fs::write(d.join("ใหม่ file.txt"), "new\n").unwrap();
        // the base moving on doesn't show up as the agent's change
        git(d, &["checkout", "-q", "main"]);
        fs::write(d.join("base-only.txt"), "x\n").unwrap();
        git(d, &["add", "base-only.txt"]);
        git(d, &["commit", "-q", "-m", "base moves"]);
        git(d, &["checkout", "-q", "agent/x"]);

        let files = list(d, "main").unwrap();
        let mut got: Vec<(String, char, usize, usize)> = files
            .iter()
            .map(|f| (f.path.clone(), f.status, f.added, f.removed))
            .collect();
        got.sort();
        assert_eq!(
            got,
            [
                ("a.txt".to_string(), 'M', 1, 1),
                ("gone.txt".to_string(), 'D', 0, 1),
                ("ใหม่ file.txt".to_string(), '?', 1, 0),
            ]
        );
        let r = rows(d, "main", "a.txt").unwrap().unwrap();
        assert_eq!(r[0].kind, Kind::Hunk);
        let changed: Vec<(Kind, Option<u32>, Option<u32>, &str)> = r
            .iter()
            .filter(|x| x.kind != Kind::Context && x.kind != Kind::Hunk)
            .map(|x| (x.kind, x.old, x.new, x.text.as_str()))
            .collect();
        assert_eq!(
            changed,
            [
                (Kind::Removed, Some(2), None, "two"),
                (Kind::Added, None, Some(2), "TWO")
            ]
        );
        let r = rows(d, "main", "ใหม่ file.txt").unwrap().unwrap();
        assert_eq!(r.last().unwrap().text, "new");
    }

    #[test]
    fn review_message_quotes_each_comment() {
        let m = review_message(&[
            Comment {
                path: "src/a.rs".into(),
                lines: (3, 3),
                quote: "+let x = 1;".into(),
                text: "Use a constant".into(),
            },
            Comment {
                path: "b.rs".into(),
                lines: (1, 4),
                quote: " a\n-b".into(),
                text: " Why? ".into(),
            },
        ]);
        assert!(m.contains("1. `src/a.rs` line 3:\n```\n+let x = 1;\n```\nUse a constant"));
        assert!(m.contains("2. `b.rs` lines 1–4:"));
        assert!(m.contains("\nWhy?\n"));
    }
}
