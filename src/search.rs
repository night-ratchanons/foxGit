//! Search mode: find in files with `git grep` (respects .gitignore, skips binaries), results
//! grouped by file; a click opens the editor at the line.

use crate::theme::{self, pal};
use eframe::egui;
use egui::{pos2, vec2, FontId, RichText, Sense};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

const MAX_HITS: usize = 5000;

#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    pub path: String,
    pub line: usize,
    /// 1-based byte column of the first match.
    pub column: usize,
    pub text: String,
}

/// Parse `git grep -n -z --column` output: `path\0line\0column\0text\n`.
pub fn parse(out: &str) -> Vec<Hit> {
    out.lines()
        .filter_map(|l| {
            let mut it = l.splitn(4, '\0');
            let path = it.next()?.to_string();
            let line = it.next()?.parse().ok()?;
            let column = it.next()?.parse().ok()?;
            let text = it.next()?.to_string();
            Some(Hit {
                path,
                line,
                column,
                text,
            })
        })
        .take(MAX_HITS)
        .collect()
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub struct Opts {
    pub case: bool,
    pub regex: bool,
    pub word: bool,
    pub untracked: bool,
}

pub fn grep(dir: &Path, query: &str, o: Opts) -> Result<Vec<Hit>, String> {
    let mut c = Command::new("git");
    c.current_dir(dir)
        .args([
            "grep",
            "-n",
            "-z",
            "-I",
            "--column",
            "--no-color",
            "--full-name",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if !o.case {
        c.arg("-i");
    }
    c.arg(if o.regex { "-E" } else { "-F" });
    if o.word {
        c.arg("-w");
    }
    if o.untracked {
        c.arg("--untracked");
    }
    c.arg("-e").arg(query);
    if let Some(p) = crate::shellenv::path() {
        c.env("PATH", p);
    }
    let out = c.output().map_err(|e| format!("git grep: {e}"))?;
    match out.status.code() {
        Some(0) => Ok(parse(&String::from_utf8_lossy(&out.stdout))),
        Some(1) => Ok(Vec::new()), // nothing found
        _ => Err(String::from_utf8_lossy(&out.stderr).trim().to_string()),
    }
}

#[derive(Default)]
pub struct Search {
    pub query: String,
    pub opts: Opts,
    searched: Option<(String, Opts)>,
    rx: Option<Receiver<Result<Vec<Hit>, String>>>,
    results: Option<Result<Vec<Hit>, String>>,
    edited: Option<Instant>,
    focus: bool,
    collapsed: std::collections::HashSet<String>,
}

impl Search {
    pub fn focus(&mut self) {
        self.focus = true;
    }

    /// Search for `q` right away (as if typed).
    #[cfg(debug_assertions)]
    pub fn search_now(&mut self, q: &str) {
        self.query = q.to_string();
        self.edited = Instant::now().checked_sub(Duration::from_secs(1));
    }

    fn start(&mut self, dir: &Path, ctx: &egui::Context) {
        let q = self.query.clone();
        self.searched = Some((q.clone(), self.opts));
        if q.trim().is_empty() {
            self.results = None;
            self.rx = None;
            return;
        }
        let (tx, rx) = channel();
        let (dir, ctx, o) = (dir.to_path_buf(), ctx.clone(), self.opts);
        std::thread::spawn(move || {
            let _ = tx.send(grep(&dir, &q, o));
            ctx.request_repaint();
        });
        self.rx = Some(rx);
    }

    fn poll(&mut self) {
        if let Some(rx) = &self.rx {
            if let Ok(r) = rx.try_recv() {
                self.results = Some(r);
                self.rx = None;
            }
        }
    }

    pub fn sidebar(&mut self, ui: &mut egui::Ui, dir: &Path) {
        self.poll();
        let p = pal(ui);
        let ctx = ui.ctx().clone();
        egui::Panel::left("sidebar")
            .resizable(true)
            .default_size(280.0)
            .size_range(200.0..=520.0)
            .frame(theme::panel(p.mantle, 8, 6))
            .show(ui, |ui| {
                ui.add_space(6.0);
                ui.label(RichText::new("SEARCH IN FILES").small().color(p.faint));
                ui.add_space(4.0);
                let r = ui.add(
                    egui::TextEdit::singleline(&mut self.query)
                        .hint_text("Text to find")
                        .desired_width(f32::INFINITY)
                        .margin(vec2(8.0, 6.0)),
                );
                if self.focus {
                    r.request_focus();
                    self.focus = false;
                }
                if r.changed() {
                    self.edited = Some(Instant::now());
                }
                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                ui.horizontal_wrapped(|ui| {
                    let toggle = |ui: &mut egui::Ui, v: &mut bool, label: &str, tip: &str| {
                        if ui.selectable_label(*v, label).on_hover_text(tip).clicked() {
                            *v = !*v;
                        }
                    };
                    toggle(ui, &mut self.opts.case, "Aa", "Match case");
                    toggle(ui, &mut self.opts.word, "ab", "Whole words");
                    toggle(ui, &mut self.opts.regex, ".*", "Regular expression (extended)");
                    toggle(ui, &mut self.opts.untracked, "+untracked", "Also search new files that aren't committed yet");
                });
                // search as you type (debounced), on Enter, or when an option changes
                let typed = self.edited.is_some_and(|t| t.elapsed() > Duration::from_millis(350));
                let changed = self.searched.as_ref().is_some_and(|(_, o)| *o != self.opts);
                if enter || typed || changed {
                    self.edited = None;
                    if enter || self.searched.as_ref() != Some(&(self.query.clone(), self.opts)) {
                        self.start(dir, &ctx);
                    }
                } else if self.edited.is_some() {
                    ctx.request_repaint_after(Duration::from_millis(120));
                }
                ui.add_space(8.0);
                if self.rx.is_some() {
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new().size(14.0));
                        ui.label(RichText::new("Searching…").color(p.subtext));
                    });
                }
                match &self.results {
                    Some(Ok(hits)) => {
                        let files = count_files(hits);
                        ui.label(
                            RichText::new(if hits.len() >= MAX_HITS {
                                format!("First {MAX_HITS} matches in {files} file(s)")
                            } else {
                                format!("{} match(es) in {files} file(s)", hits.len())
                            })
                            .color(p.subtext),
                        );
                    }
                    Some(Err(e)) => {
                        ui.label(RichText::new(e).color(p.red));
                    }
                    None => {
                        ui.label(RichText::new("Searches tracked files with git grep — .gitignore'd and binary files are skipped.").small().color(p.faint));
                    }
                }
            });
    }

    pub fn main(&mut self, ui: &mut egui::Ui, dir: &Path) -> Option<String> {
        let p = pal(ui);
        let mut note = None;
        egui::CentralPanel::default()
            .frame(theme::panel(p.base, 12, 8))
            .show(ui, |ui| {
                let Some(Ok(hits)) = &self.results else {
                    ui.centered_and_justified(|ui| {
                        ui.label(
                            RichText::new(format!(
                                "🔍  Type in the box on the left  ({})",
                                theme::shortcut("Shift+F")
                            ))
                            .color(p.faint),
                        );
                    });
                    return;
                };
                if hits.is_empty() {
                    ui.centered_and_justified(|ui| {
                        ui.label(RichText::new("No matches").color(p.faint));
                    });
                    return;
                }
                // flatten into uniform rows: file headers + hit lines (virtualized)
                let mut rows: Vec<(bool, usize)> = Vec::new(); // (is_header, hit index)
                let mut last: Option<&str> = None;
                for (i, h) in hits.iter().enumerate() {
                    if last != Some(h.path.as_str()) {
                        rows.push((true, i));
                        last = Some(&h.path);
                    }
                    if !self.collapsed.contains(&h.path) {
                        rows.push((false, i));
                    }
                }
                let font = FontId::monospace(12.5);
                let row_h = 22.0;
                let mut toggle = None;
                egui::ScrollArea::both().auto_shrink(false).show_rows(
                    ui,
                    row_h,
                    rows.len(),
                    |ui, range| {
                        for &(header, i) in &rows[range] {
                            let h = &hits[i];
                            let w = ui.available_width().max(600.0);
                            let (rect, resp) =
                                ui.allocate_exact_size(vec2(w, row_h), Sense::click());
                            let painter = ui.painter();
                            if resp.hovered() {
                                painter.rect_filled(rect, 4.0, p.hover);
                            }
                            if header {
                                let open = !self.collapsed.contains(&h.path);
                                theme::paint_caret(
                                    painter,
                                    pos2(rect.left() + 8.0, rect.center().y),
                                    open,
                                    p.subtext,
                                );
                                let n = hits.iter().filter(|x| x.path == h.path).count();
                                painter.text(
                                    pos2(rect.left() + 20.0, rect.center().y),
                                    egui::Align2::LEFT_CENTER,
                                    &h.path,
                                    FontId::proportional(13.0),
                                    p.text_strong,
                                );
                                painter.text(
                                    pos2(rect.right() - 8.0, rect.center().y),
                                    egui::Align2::RIGHT_CENTER,
                                    n.to_string(),
                                    FontId::proportional(11.5),
                                    p.faint,
                                );
                                if resp.clicked() {
                                    toggle = Some(h.path.clone());
                                }
                            } else {
                                painter.text(
                                    pos2(rect.left() + 56.0, rect.center().y),
                                    egui::Align2::RIGHT_CENTER,
                                    h.line.to_string(),
                                    font.clone(),
                                    p.faint,
                                );
                                // the line with its first match highlighted (leading indentation dropped)
                                let raw = h.text.as_str();
                                let s0 = floor(raw, h.column.saturating_sub(1).min(raw.len()));
                                let s1 = floor(
                                    raw,
                                    (s0 + match_len(&self.searched, &raw[s0..])).min(raw.len()),
                                );
                                let fmt = |c| egui::TextFormat::simple(font.clone(), c);
                                let mut job = egui::text::LayoutJob::default();
                                job.append(
                                    &raw[..s0].trim_start().replace('\t', "    "),
                                    0.0,
                                    fmt(p.text),
                                );
                                let mut hl = fmt(p.text_strong);
                                hl.background = theme::alpha(p.yellow, 0.35);
                                job.append(&raw[s0..s1].replace('\t', "    "), 0.0, hl);
                                job.append(&raw[s1..].replace('\t', "    "), 0.0, fmt(p.text));
                                let g = ui.painter().layout_job(job);
                                ui.painter().galley(
                                    pos2(rect.left() + 66.0, rect.center().y - g.size().y / 2.0),
                                    g,
                                    p.text,
                                );
                                if resp.clicked() {
                                    let file = dir.join(&h.path);
                                    if let Err(e) = crate::desktop::open_in_editor(
                                        dir,
                                        Some(&file),
                                        Some(h.line),
                                    ) {
                                        note = Some(e);
                                    }
                                }
                                resp.on_hover_text("Open in the editor at this line")
                                    .context_menu(|ui| {
                                        if ui.button("Copy line").clicked() {
                                            ui.ctx().copy_text(h.text.clone());
                                            ui.close();
                                        }
                                        if ui.button("Copy path").clicked() {
                                            ui.ctx().copy_text(format!("{}:{}", h.path, h.line));
                                            ui.close();
                                        }
                                    });
                            }
                        }
                    },
                );
                if let Some(path) = toggle {
                    if !self.collapsed.remove(&path) {
                        self.collapsed.insert(path);
                    }
                }
            });
        note
    }
}

fn floor(s: &str, mut i: usize) -> usize {
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Length of the match at the start of `rest` (the query for fixed strings; a guess for regexes).
fn match_len(searched: &Option<(String, Opts)>, rest: &str) -> usize {
    match searched {
        Some((q, o)) if !o.regex => {
            if rest.len() >= q.len() {
                q.len()
            } else {
                0
            }
        }
        // regex: highlight to the end of the word
        _ => rest
            .char_indices()
            .find(|(_, c)| !c.is_alphanumeric() && *c != '_')
            .map(|(i, _)| i.max(1))
            .unwrap_or(rest.len()),
    }
}

fn count_files(hits: &[Hit]) -> usize {
    let mut n = 0;
    let mut last: Option<&str> = None;
    for h in hits {
        if last != Some(h.path.as_str()) {
            n += 1;
            last = Some(&h.path);
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn greps_tracked_files_with_spaces_thai_and_colons() {
        let t = tempfile::tempdir().unwrap();
        let d = t.path();
        let git = |args: &[&str]| {
            assert!(Command::new("git")
                .args(args)
                .current_dir(d)
                .status()
                .unwrap()
                .success());
        };
        git(&["init", "-q"]);
        fs::write(d.join("a b.txt"), "hello world\nfoo Hello\n").unwrap();
        fs::write(d.join("ไทย.txt"), "x:hello:y\n").unwrap();
        fs::write(d.join("new.txt"), "hello untracked\n").unwrap();
        git(&["add", "a b.txt", "ไทย.txt"]);

        let hits = grep(d, "hello", Opts::default()).unwrap();
        assert_eq!(hits.len(), 3);
        assert_eq!(
            hits[0],
            Hit {
                path: "a b.txt".into(),
                line: 1,
                column: 1,
                text: "hello world".into()
            }
        );
        assert_eq!(hits[1].column, 5);
        assert_eq!(hits[2].text, "x:hello:y");
        assert_eq!(count_files(&hits), 2);

        let case = grep(
            d,
            "Hello",
            Opts {
                case: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(case.len(), 1);
        let all = grep(
            d,
            "hello",
            Opts {
                untracked: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(all.len(), 4);
        assert!(grep(d, "nothing-here", Opts::default()).unwrap().is_empty());
        assert!(grep(
            d,
            "(",
            Opts {
                regex: true,
                ..Default::default()
            }
        )
        .is_err());
    }
}
