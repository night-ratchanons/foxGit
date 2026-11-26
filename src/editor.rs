//! Quick edit: fix a line or two of a working-copy file without leaving FoxGit (a monospace
//! text editor window; ⌘S saves). Bigger edits go to the real editor.

use crate::app::RepoTab;
use crate::theme::{self, pal};
use eframe::egui;
use egui::{vec2, RichText};
use std::path::PathBuf;

const MAX_SIZE: u64 = 2 * 1024 * 1024;

pub struct EditState {
    pub path: String,
    full: PathBuf,
    text: String,
    original: String,
    focus: bool,
    /// The ✕ was clicked once with unsaved edits.
    warned: bool,
}

impl EditState {
    pub fn open(workdir: &std::path::Path, path: &str) -> Result<EditState, String> {
        let full = workdir.join(path);
        let meta = std::fs::metadata(&full).map_err(|e| format!("Cannot open {path}: {e}"))?;
        if meta.len() > MAX_SIZE {
            return Err(format!(
                "{path} is too big to edit here — open it in your editor"
            ));
        }
        let bytes = std::fs::read(&full).map_err(|e| format!("Cannot read {path}: {e}"))?;
        let text = String::from_utf8(bytes).map_err(|_| format!("{path} is not UTF-8 text"))?;
        Ok(EditState {
            path: path.into(),
            full,
            original: text.clone(),
            text,
            focus: true,
            warned: false,
        })
    }

    fn dirty(&self) -> bool {
        self.text != self.original
    }
}

/// The edit window of the tab, if one is open. Returns a note for the toast.
pub fn window(ctx: &egui::Context, tab: &mut RepoTab) -> Option<(String, bool)> {
    let st = tab.edit.as_mut()?;
    let mut note = None;
    let mut open = true;
    let mut close = false;
    let mut saved = false;
    let title = format!("{}{}", if st.dirty() { "● " } else { "" }, st.path);
    egui::Window::new(title)
        .id(egui::Id::unique("quick_edit"))
        .open(&mut open)
        .default_size(vec2(760.0, 560.0))
        .pivot(egui::Align2::CENTER_CENTER)
        .default_pos(ctx.content_rect().center())
        .min_size(vec2(360.0, 200.0))
        .resizable(true)
        .collapsible(false)
        .show(ctx, |ui| {
            let p = pal(ui);
            let save_key = ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::S));
            ui.horizontal(|ui| {
                if theme::primary_button(
                    ui,
                    &format!("Save  ({})", theme::shortcut("S")),
                    st.dirty(),
                )
                .clicked()
                    || (save_key && st.dirty())
                {
                    // keep the file's line endings: the editor only ever sees what was on disk
                    match std::fs::write(&st.full, &st.text) {
                        Ok(()) => {
                            st.original = st.text.clone();
                            saved = true;
                            note = Some((format!("Saved {}", st.path), true));
                        }
                        Err(e) => note = Some((format!("Could not save {}: {e}", st.path), false)),
                    }
                }
                if ui
                    .add_enabled(st.dirty(), egui::Button::new("Revert"))
                    .clicked()
                {
                    st.text = st.original.clone();
                }
                if ui.button("Open in editor").clicked() {
                    if let Some(dir) = st.full.parent() {
                        if let Err(e) = crate::desktop::open_in_editor(dir, Some(&st.full), None) {
                            note = Some((e, false));
                        }
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let lines = st.text.lines().count();
                    ui.label(
                        RichText::new(format!("{lines} lines"))
                            .small()
                            .color(p.faint),
                    );
                });
            });
            ui.add_space(4.0);
            egui::ScrollArea::both().auto_shrink(false).show(ui, |ui| {
                let r = ui.add(
                    egui::TextEdit::multiline(&mut st.text)
                        .code_editor()
                        .desired_width(f32::INFINITY)
                        .desired_rows(30)
                        .lock_focus(true),
                );
                if st.focus {
                    r.request_focus();
                    st.focus = false;
                }
            });
            if ui.input(|i| i.key_pressed(egui::Key::Escape)) && !st.dirty() {
                close = true;
            }
        });
    if !open || close {
        match tab.edit.as_mut() {
            Some(st) if st.dirty() && !st.warned => {
                // first ✕ with unsaved edits: keep the window, warn once
                st.warned = true;
                note = Some(("Unsaved edits — close again to discard them".into(), false));
            }
            _ => tab.edit = None,
        }
    }
    if saved {
        tab.reload_fast();
    }
    note
}
