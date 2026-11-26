//! Conflict resolution: parse a file with `<<<<<<<` / `|||||||` / `=======` / `>>>>>>>` markers
//! into blocks, let the user pick a side per block, write the result back and mark it resolved.
//! Line endings are kept byte-for-byte (lines are split with their terminators).

use crate::app::{Action, Dialog, RepoTab};
use crate::git::{Op, Sequence};
use crate::theme::{self, pal};
use eframe::egui;
use egui::{vec2, RichText, Stroke};

#[derive(Clone, Debug, PartialEq)]
pub struct Block {
    pub ours: String,
    pub base: Option<String>,
    pub theirs: String,
    pub ours_label: String,
    pub base_label: String,
    pub theirs_label: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Chunk {
    Same(String),
    Conflict(Block),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pick {
    Ours,
    Theirs,
    /// Ours, then theirs.
    Both,
    /// Theirs, then ours.
    BothReversed,
    /// Drop both sides.
    Neither,
}

fn marker<'a>(line: &'a str, m: &str) -> Option<&'a str> {
    let body = line.trim_end_matches(['\n', '\r']);
    let rest = body.strip_prefix(m)?;
    if rest.is_empty() {
        Some("")
    } else {
        rest.strip_prefix(' ')
    }
}

/// Split into chunks. `None` when there are no (well-formed) conflict markers.
pub fn parse(text: &str) -> Option<Vec<Chunk>> {
    enum St {
        Out,
        Ours,
        Base,
        Theirs,
    }
    let mut out = Vec::new();
    let mut same = String::new();
    let mut cur = Block {
        ours: String::new(),
        base: None,
        theirs: String::new(),
        ours_label: String::new(),
        base_label: String::new(),
        theirs_label: String::new(),
    };
    let mut st = St::Out;
    let mut any = false;
    for line in text.split_inclusive('\n') {
        match st {
            St::Out => {
                if let Some(label) = marker(line, "<<<<<<<") {
                    if !same.is_empty() {
                        out.push(Chunk::Same(std::mem::take(&mut same)));
                    }
                    cur.ours_label = label.to_string();
                    st = St::Ours;
                } else {
                    same.push_str(line);
                }
            }
            St::Ours | St::Base => {
                if let Some(label) = marker(line, "|||||||").filter(|_| matches!(st, St::Ours)) {
                    cur.base = Some(String::new());
                    cur.base_label = label.to_string();
                    st = St::Base;
                } else if marker(line, "=======") == Some("") {
                    st = St::Theirs;
                } else if marker(line, "<<<<<<<").is_some() {
                    return None; // nested: not something we can resolve here
                } else if matches!(st, St::Ours) {
                    cur.ours.push_str(line);
                } else if let Some(b) = &mut cur.base {
                    b.push_str(line);
                }
            }
            St::Theirs => {
                if let Some(label) = marker(line, ">>>>>>>") {
                    cur.theirs_label = label.to_string();
                    out.push(Chunk::Conflict(std::mem::replace(
                        &mut cur,
                        Block {
                            ours: String::new(),
                            base: None,
                            theirs: String::new(),
                            ours_label: String::new(),
                            base_label: String::new(),
                            theirs_label: String::new(),
                        },
                    )));
                    any = true;
                    st = St::Out;
                } else if marker(line, "<<<<<<<").is_some() || marker(line, "=======") == Some("") {
                    return None;
                } else {
                    cur.theirs.push_str(line);
                }
            }
        }
    }
    if !matches!(st, St::Out) || !any {
        return None;
    }
    if !same.is_empty() {
        out.push(Chunk::Same(same));
    }
    Some(out)
}

/// The file with every block resolved by `picks` (unpicked blocks keep their markers).
pub fn render(chunks: &[Chunk], picks: &[Option<Pick>]) -> String {
    let mut s = String::new();
    let mut i = 0;
    for c in chunks {
        match c {
            Chunk::Same(t) => s.push_str(t),
            Chunk::Conflict(b) => {
                match picks.get(i).copied().flatten() {
                    Some(Pick::Ours) => s.push_str(&b.ours),
                    Some(Pick::Theirs) => s.push_str(&b.theirs),
                    Some(Pick::Both) => {
                        s.push_str(&b.ours);
                        push_joined(&mut s, &b.theirs);
                    }
                    Some(Pick::BothReversed) => {
                        s.push_str(&b.theirs);
                        push_joined(&mut s, &b.ours);
                    }
                    Some(Pick::Neither) => {}
                    None => {
                        let nl = if b.ours.contains("\r\n") || b.theirs.contains("\r\n") {
                            "\r\n"
                        } else {
                            "\n"
                        };
                        let mark = |m: &str, label: &str| {
                            if label.is_empty() {
                                format!("{m}{nl}")
                            } else {
                                format!("{m} {label}{nl}")
                            }
                        };
                        s.push_str(&mark("<<<<<<<", &b.ours_label));
                        s.push_str(&b.ours);
                        if let Some(base) = &b.base {
                            s.push_str(&mark("|||||||", &b.base_label));
                            s.push_str(base);
                        }
                        s.push_str(&mark("=======", ""));
                        s.push_str(&b.theirs);
                        s.push_str(&mark(">>>>>>>", &b.theirs_label));
                    }
                }
                i += 1;
            }
        }
    }
    s
}

/// Append `t`, making sure what's already there ends with a line break first.
fn push_joined(s: &mut String, t: &str) {
    if !s.is_empty() && !s.ends_with('\n') && !t.is_empty() {
        s.push('\n');
    }
    s.push_str(t);
}

pub fn count(chunks: &[Chunk]) -> usize {
    chunks
        .iter()
        .filter(|c| matches!(c, Chunk::Conflict(_)))
        .count()
}

// ---------------------------------------------------------------------------
// UI
// ---------------------------------------------------------------------------

/// The open conflicted file of a tab.
pub struct State {
    pub path: String,
    pub chunks: Option<Vec<Chunk>>,
    pub picks: Vec<Option<Pick>>,
    pub error: Option<String>,
}

impl State {
    pub fn load(workdir: &std::path::Path, path: &str) -> State {
        let full = workdir.join(path);
        match std::fs::read(&full) {
            Err(e) => State {
                path: path.into(),
                chunks: None,
                picks: Vec::new(),
                error: Some(format!("Cannot read the file: {e}")),
            },
            Ok(bytes) => match String::from_utf8(bytes) {
                Err(_) => State {
                    path: path.into(),
                    chunks: None,
                    picks: Vec::new(),
                    error: Some("Binary or non-UTF-8 file — pick a whole side below.".into()),
                },
                Ok(text) => {
                    let chunks = parse(&text);
                    let n = chunks.as_deref().map(count).unwrap_or(0);
                    State {
                        path: path.into(),
                        error: chunks.is_none().then(|| {
                            "No conflict markers left (or they are nested). Edit the file, or pick a whole side, then mark it resolved.".into()
                        }),
                        chunks,
                        picks: vec![None; n],
                    }
                }
            },
        }
    }
}

/// Merge / rebase / cherry-pick banner with Continue and Abort.
pub fn banner(ui: &mut egui::Ui, tab: &mut RepoTab, actions: &mut Vec<Action>) {
    let state = tab.repo.repo.state();
    if state == git2::RepositoryState::Clean {
        return;
    }
    let p = pal(ui);
    let seq = Sequence::of(state);
    let conflicts = tab.conflicted().len();
    let tint = if conflicts > 0 { p.yellow } else { p.green };
    egui::Frame::new()
        .fill(theme::alpha(tint, 0.12))
        .stroke(Stroke::new(1.0, theme::alpha(tint, 0.35)))
        .inner_margin(egui::Margin::symmetric(10, 8))
        .corner_radius(8.0)
        .show(ui, |ui| {
            let what = seq.map(|s| s.noun()).unwrap_or("operation");
            let text = if conflicts > 0 {
                format!("⚠  {} in progress — {conflicts} conflicted file(s). Pick a side for each, then Continue.", capital(what))
            } else {
                format!("✔  {} in progress — no conflicts left. Continue when ready.", capital(what))
            };
            ui.add(egui::Label::new(RichText::new(text).color(tint)).wrap());
            if let Some(seq) = seq {
                ui.horizontal(|ui| {
                    let cont = ui.add_enabled(
                        conflicts == 0,
                        egui::Button::new(RichText::new(format!("Continue {what}")).strong().color(p.on_accent))
                            .fill(p.green)
                            .stroke(Stroke::NONE),
                    );
                    if cont.on_disabled_hover_text("Resolve every conflicted file first").clicked() {
                        actions.push(Action::Run(Op::Continue(seq)));
                    }
                    if ui.button(format!("Abort {what}")).clicked() {
                        actions.push(Action::Dialog(Dialog::Confirm {
                            title: format!("Abort {what}"),
                            text: format!("Abort the {what} and go back to where you were?\nResolutions made so far are lost."),
                            op: Op::Abort(seq),
                            danger: true,
                        }));
                    }
                });
            }
        });
    ui.add_space(6.0);
}

fn capital(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
        .unwrap_or_default()
}

/// The conflicted file in the main area: whole-file actions on top, then one card per block.
pub fn view(ui: &mut egui::Ui, tab: &mut RepoTab, path: &str, actions: &mut Vec<Action>) {
    let p = pal(ui);
    if tab.conflict.as_ref().is_none_or(|s| s.path != path) {
        tab.conflict = Some(State::load(&tab.path, path));
    }
    let workdir = tab.path.clone();
    let Some(st) = tab.conflict.as_mut() else {
        return;
    };
    let ours_name = tab.refs.head_name.clone().unwrap_or_else(|| "HEAD".into());

    ui.horizontal(|ui| {
        ui.label(RichText::new("!").strong().color(p.red));
        ui.add(
            egui::Label::new(
                RichText::new(path)
                    .monospace()
                    .strong()
                    .color(p.text_strong),
            )
            .truncate(),
        );
    });
    ui.horizontal_wrapped(|ui| {
        if ui
            .button("Use ours for the whole file")
            .on_hover_text(format!("Keep the version from {ours_name}"))
            .clicked()
        {
            actions.push(Action::Run(Op::ResolveWith {
                paths: vec![path.into()],
                theirs: false,
            }));
        }
        if ui
            .button("Use theirs for the whole file")
            .on_hover_text("Take the incoming version")
            .clicked()
        {
            actions.push(Action::Run(Op::ResolveWith {
                paths: vec![path.into()],
                theirs: true,
            }));
        }
        if ui.button("Open in editor").clicked() {
            let line = st.chunks.as_ref().and_then(|c| first_conflict_line(c));
            match crate::desktop::open_in_editor(&workdir, Some(&workdir.join(path)), line) {
                Ok(_) => {}
                Err(e) => actions.push(Action::Notify(e, false)),
            }
        }
        if ui
            .button("Merge tool…")
            .on_hover_text("git mergetool (FileMerge, Meld, KDiff3 … whatever git is set up with)")
            .clicked()
        {
            actions.push(Action::Run(Op::MergeTool(path.into())));
        }
        if ui
            .button("⟳")
            .on_hover_text("Reload the file from disk")
            .clicked()
        {
            *st = State::load(&workdir, path);
        }
        if ui
            .button("Mark resolved")
            .on_hover_text("git add — the file is fine as it is on disk")
            .clicked()
        {
            actions.push(Action::Run(Op::MarkResolved(vec![path.into()])));
        }
    });
    ui.add_space(4.0);
    if let Some(e) = &st.error {
        ui.label(RichText::new(e).color(p.subtext));
    }
    let Some(chunks) = st.chunks.clone() else {
        return;
    };
    let total = st.picks.len();
    let done = st.picks.iter().filter(|x| x.is_some()).count();
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(format!("{done} of {total} conflict(s) resolved"))
                .color(if done == total { p.green } else { p.yellow }),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let save = theme::primary_button(ui, "Save & mark resolved", done == total);
            if save
                .on_disabled_hover_text("Pick a side for every conflict first")
                .clicked()
            {
                let text = render(&chunks, &st.picks);
                match std::fs::write(workdir.join(path), text) {
                    Ok(()) => actions.push(Action::Run(Op::MarkResolved(vec![path.into()]))),
                    Err(e) => {
                        actions.push(Action::Notify(format!("Could not save {path}: {e}"), false))
                    }
                }
            }
            if done > 0 && ui.button("Reset picks").clicked() {
                st.picks.iter_mut().for_each(|x| *x = None);
            }
            ui.menu_button("All…", |ui| {
                for (label, pick) in [
                    ("All ours", Pick::Ours),
                    ("All theirs", Pick::Theirs),
                    ("All both (ours first)", Pick::Both),
                ] {
                    if ui.button(label).clicked() {
                        st.picks.iter_mut().for_each(|x| *x = Some(pick));
                        ui.close();
                    }
                }
            });
        });
    });
    ui.add_space(6.0);

    let font = egui::FontId::monospace(12.5);
    egui::ScrollArea::vertical()
        .id_salt("conflict_blocks")
        .auto_shrink(false)
        .show(ui, |ui| {
            let mut k = 0;
            for c in &chunks {
                match c {
                    Chunk::Same(t) => {
                        // a few lines of context around each block
                        let lines: Vec<&str> = t.lines().collect();
                        let n = lines.len();
                        let shown: Vec<&str> = if n <= 6 {
                            lines
                        } else {
                            lines[..2]
                                .iter()
                                .chain(["⋯"].iter())
                                .chain(lines[n - 3..].iter())
                                .copied()
                                .collect()
                        };
                        for l in shown {
                            ui.label(
                                RichText::new(l.replace('\t', "    "))
                                    .font(font.clone())
                                    .color(p.faint),
                            );
                        }
                    }
                    Chunk::Conflict(b) => {
                        let pick = st.picks[k];
                        egui::Frame::new()
                            .fill(p.surface0)
                            .stroke(Stroke::new(
                                1.0,
                                if pick.is_some() {
                                    theme::alpha(p.green, 0.5)
                                } else {
                                    theme::alpha(p.yellow, 0.6)
                                },
                            ))
                            .corner_radius(8.0)
                            .inner_margin(egui::Margin::same(8))
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                ui.horizontal(|ui| {
                                    ui.label(
                                        RichText::new(format!("Conflict {}", k + 1))
                                            .strong()
                                            .color(p.text_strong),
                                    );
                                    if let Some(x) = pick {
                                        let t = match x {
                                            Pick::Ours => "→ ours",
                                            Pick::Theirs => "→ theirs",
                                            Pick::Both => "→ both (ours first)",
                                            Pick::BothReversed => "→ both (theirs first)",
                                            Pick::Neither => "→ neither",
                                        };
                                        ui.label(RichText::new(t).color(p.green));
                                    }
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            ui.menu_button("More", |ui| {
                                                for (label, x) in [
                                                    ("Both — theirs first", Pick::BothReversed),
                                                    ("Neither", Pick::Neither),
                                                ] {
                                                    if ui.button(label).clicked() {
                                                        st.picks[k] = Some(x);
                                                        ui.close();
                                                    }
                                                }
                                                if pick.is_some()
                                                    && ui.button("Undo pick").clicked()
                                                {
                                                    st.picks[k] = None;
                                                    ui.close();
                                                }
                                            });
                                            if ui
                                                .selectable_label(pick == Some(Pick::Both), "Both")
                                                .clicked()
                                            {
                                                st.picks[k] = Some(Pick::Both);
                                            }
                                            if ui
                                                .selectable_label(
                                                    pick == Some(Pick::Theirs),
                                                    "Use theirs",
                                                )
                                                .clicked()
                                            {
                                                st.picks[k] = Some(Pick::Theirs);
                                            }
                                            if ui
                                                .selectable_label(
                                                    pick == Some(Pick::Ours),
                                                    "Use ours",
                                                )
                                                .clicked()
                                            {
                                                st.picks[k] = Some(Pick::Ours);
                                            }
                                        },
                                    );
                                });
                                ui.add_space(4.0);
                                let w = (ui.available_width() - 8.0) / 2.0;
                                ui.horizontal_top(|ui| {
                                    side(
                                        ui,
                                        w,
                                        &format!(
                                            "Ours · {}",
                                            if b.ours_label.is_empty() {
                                                &ours_name
                                            } else {
                                                &b.ours_label
                                            }
                                        ),
                                        &b.ours,
                                        p.blue,
                                        matches!(
                                            pick,
                                            Some(Pick::Ours | Pick::Both | Pick::BothReversed)
                                        ),
                                        &font,
                                    );
                                    side(
                                        ui,
                                        w,
                                        &format!("Theirs · {}", b.theirs_label),
                                        &b.theirs,
                                        p.purple,
                                        matches!(
                                            pick,
                                            Some(Pick::Theirs | Pick::Both | Pick::BothReversed)
                                        ),
                                        &font,
                                    );
                                });
                            });
                        ui.add_space(6.0);
                        k += 1;
                    }
                }
            }
        });
}

fn side(
    ui: &mut egui::Ui,
    w: f32,
    title: &str,
    text: &str,
    color: egui::Color32,
    chosen: bool,
    font: &egui::FontId,
) {
    let p = pal(ui);
    ui.allocate_ui_with_layout(
        vec2(w, 0.0),
        egui::Layout::top_down(egui::Align::Min),
        |ui| {
            ui.set_width(w);
            egui::Frame::new()
                .fill(theme::alpha(color, if chosen { 0.16 } else { 0.06 }))
                .stroke(Stroke::new(
                    1.0,
                    theme::alpha(color, if chosen { 0.7 } else { 0.25 }),
                ))
                .corner_radius(6.0)
                .inner_margin(egui::Margin::same(6))
                .show(ui, |ui| {
                    ui.set_width(w - 14.0);
                    ui.label(RichText::new(title).small().strong().color(color));
                    if text.is_empty() {
                        ui.label(RichText::new("(nothing)").italics().color(p.faint));
                    }
                    egui::ScrollArea::horizontal()
                        .id_salt(title)
                        .show(ui, |ui| {
                            ui.vertical(|ui| {
                                ui.spacing_mut().item_spacing.y = 0.0;
                                for l in text.lines() {
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(l.replace('\t', "    "))
                                                .font(font.clone())
                                                .color(p.text),
                                        )
                                        .extend(),
                                    );
                                }
                            });
                        });
                });
        },
    );
}

fn first_conflict_line(chunks: &[Chunk]) -> Option<usize> {
    let mut line = 1;
    for c in chunks {
        match c {
            Chunk::Same(t) => line += t.matches('\n').count(),
            Chunk::Conflict(_) => return Some(line),
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEXT: &str = "a\n<<<<<<< HEAD\nours 1\nours 2\n=======\ntheirs\n>>>>>>> feature\nb\n";

    #[test]
    fn parses_blocks_and_context() {
        let c = parse(TEXT).unwrap();
        assert_eq!(c.len(), 3);
        assert_eq!(c[0], Chunk::Same("a\n".into()));
        let Chunk::Conflict(b) = &c[1] else { panic!() };
        assert_eq!(b.ours, "ours 1\nours 2\n");
        assert_eq!(b.theirs, "theirs\n");
        assert_eq!(b.ours_label, "HEAD");
        assert_eq!(b.theirs_label, "feature");
        assert_eq!(c[2], Chunk::Same("b\n".into()));
        assert_eq!(first_conflict_line(&c), Some(2));
    }

    #[test]
    fn renders_every_pick() {
        let c = parse(TEXT).unwrap();
        assert_eq!(render(&c, &[Some(Pick::Ours)]), "a\nours 1\nours 2\nb\n");
        assert_eq!(render(&c, &[Some(Pick::Theirs)]), "a\ntheirs\nb\n");
        assert_eq!(
            render(&c, &[Some(Pick::Both)]),
            "a\nours 1\nours 2\ntheirs\nb\n"
        );
        assert_eq!(
            render(&c, &[Some(Pick::BothReversed)]),
            "a\ntheirs\nours 1\nours 2\nb\n"
        );
        assert_eq!(render(&c, &[Some(Pick::Neither)]), "a\nb\n");
        // unresolved blocks round-trip
        assert_eq!(render(&c, &[None]), TEXT);
    }

    #[test]
    fn keeps_crlf_diff3_and_missing_final_newline() {
        let t =
            "x\r\n<<<<<<< ours\r\nA\r\n||||||| base\r\nO\r\n=======\r\nB\r\n>>>>>>> theirs\r\nz";
        let c = parse(t).unwrap();
        let Chunk::Conflict(b) = &c[1] else { panic!() };
        assert_eq!(b.base.as_deref(), Some("O\r\n"));
        assert_eq!(render(&c, &[Some(Pick::Theirs)]), "x\r\nB\r\nz");
        assert_eq!(render(&c, &[None]), t);
        // last block without a final newline in "ours" still separates the sides
        let t = "<<<<<<< a\nA\n=======\nB\n>>>>>>> b\n";
        let mut c = parse(t).unwrap();
        if let Chunk::Conflict(b) = &mut c[0] {
            b.ours = "A".into();
        }
        assert_eq!(render(&c, &[Some(Pick::Both)]), "A\nB\n");
    }

    #[test]
    fn rejects_text_without_or_with_broken_markers() {
        assert!(parse("plain\ntext\n").is_none());
        assert!(parse("<<<<<<< a\nA\n=======\nB\n").is_none());
        // a line of equals inside normal text isn't a marker
        assert!(parse("=======\n").is_none());
        // "<<<<<<<<" (8) is not a marker
        assert!(parse("<<<<<<<< x\n").is_none());
        // Thai content survives
        let c = parse("<<<<<<< a\nสวัสดี\n=======\nลาก่อน\n>>>>>>> b\n").unwrap();
        assert_eq!(render(&c, &[Some(Pick::Theirs)]), "ลาก่อน\n");
    }
}
