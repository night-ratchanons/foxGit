//! A real terminal inside FoxGit: a PTY running a shell (or an agent's full-screen TUI),
//! parsed by `vt100` and painted with egui. Used by the agent chats ("Terminal" tab).

use crate::shellenv;
use crate::theme::{self, pal};
use eframe::egui;
use egui::{pos2, vec2, Color32, FontId, Rect, Sense, Stroke};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use std::io::{Read, Write};
use std::num::NonZeroU16;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

const SCROLLBACK: usize = 5000;
/// Font size in points, as f32 bits (Settings can change it).
static FONT_SIZE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0x4150_0000); // 13.0

pub fn font_size() -> f32 {
    f32::from_bits(FONT_SIZE.load(Ordering::Relaxed))
}

pub fn set_font_size(size: f32) {
    FONT_SIZE.store(size.clamp(9.0, 24.0).to_bits(), Ordering::Relaxed);
}

pub struct Terminal {
    parser: Arc<Mutex<vt100::Parser>>,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    exited: Arc<AtomicBool>,
    size: (u16, u16),
    /// Lines scrolled back into history (0 = live screen).
    scroll: usize,
    scroll_acc: f32,
}

fn nz(v: u16) -> NonZeroU16 {
    NonZeroU16::new(v.max(1)).unwrap_or(NonZeroU16::MIN)
}

fn default_shell() -> CommandBuilder {
    if cfg!(windows) {
        CommandBuilder::new(std::env::var("COMSPEC").unwrap_or_else(|_| "powershell.exe".into()))
    } else {
        let shell = std::env::var("SHELL")
            .ok()
            .filter(|s| s.starts_with('/'))
            .unwrap_or_else(|| {
                if cfg!(target_os = "macos") {
                    "/bin/zsh".into()
                } else {
                    "/bin/sh".into()
                }
            });
        let mut c = CommandBuilder::new(shell);
        c.arg("-l");
        c
    }
}

impl Terminal {
    /// Start the user's shell in `cwd`.
    #[cfg(test)]
    pub fn spawn(cwd: &Path, ctx: &egui::Context) -> Result<Self, String> {
        Self::spawn_with_env(cwd, ctx, &[])
    }

    /// Start the shell with extra environment variables (e.g. an agent profile's config dir).
    pub fn spawn_with_env(
        cwd: &Path,
        ctx: &egui::Context,
        envs: &[(String, String)],
    ) -> Result<Self, String> {
        let pty = native_pty_system();
        let size = PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        };
        let pair = pty.openpty(size).map_err(|e| e.to_string())?;
        let mut cmd = default_shell();
        cmd.cwd(cwd);
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env("TERM_PROGRAM", "FoxGit");
        if let Some(p) = shellenv::path() {
            cmd.env("PATH", p);
        }
        for (k, v) in envs {
            cmd.env(k, v);
        }
        let child = pair.slave.spawn_command(cmd).map_err(|e| e.to_string())?;
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().map_err(|e| e.to_string())?;
        let writer = pair.master.take_writer().map_err(|e| e.to_string())?;
        let parser = Arc::new(Mutex::new(vt100::Parser::new(nz(24), nz(80), SCROLLBACK)));
        let exited = Arc::new(AtomicBool::new(false));
        let (p2, ex2, ctx) = (parser.clone(), exited.clone(), ctx.clone());
        std::thread::spawn(move || {
            let mut buf = [0u8; 16 * 1024];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if let Ok(mut p) = p2.lock() {
                            p.process(&buf[..n]);
                        }
                        ctx.request_repaint();
                    }
                }
            }
            ex2.store(true, Ordering::Relaxed);
            ctx.request_repaint();
        });
        Ok(Self {
            parser,
            master: pair.master,
            writer,
            child,
            exited,
            size: (24, 80),
            scroll: 0,
            scroll_acc: 0.0,
        })
    }

    pub fn write(&mut self, bytes: &[u8]) {
        let _ = self.writer.write_all(bytes);
        let _ = self.writer.flush();
    }

    /// Type a command line into the shell and run it.
    pub fn run_line(&mut self, line: &str) {
        self.scroll = 0;
        self.write(format!("{line}\r").as_bytes());
    }

    pub fn exited(&self) -> bool {
        self.exited.load(Ordering::Relaxed)
    }

    /// Everything on the screen (for "Copy all").
    pub fn contents(&self) -> String {
        self.parser
            .lock()
            .map(|p| p.screen().contents())
            .unwrap_or_default()
    }

    /// Draw the terminal into the remaining space and feed it the keyboard while focused.
    pub fn ui(&mut self, ui: &mut egui::Ui) {
        let p = pal(ui);
        let font = FontId::monospace(font_size());
        let m = ui
            .painter()
            .layout_no_wrap("M".into(), font.clone(), Color32::WHITE);
        let (cw, ch) = (m.size().x, m.size().y.ceil());
        let avail = ui.available_size();
        let pad = 8.0;
        let cols = (((avail.x - 2.0 * pad) / cw).floor() as u16).max(10);
        let rows = (((avail.y - 2.0 * pad) / ch).floor() as u16).max(4);
        if (rows, cols) != self.size {
            self.size = (rows, cols);
            if let Ok(mut parser) = self.parser.lock() {
                parser.set_size(nz(rows), nz(cols));
            }
            let _ = self.master.resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            });
        }

        let (rect, resp) = ui.allocate_exact_size(avail, Sense::click());
        let id = resp.id;
        if resp.clicked() {
            resp.request_focus();
        }
        let focused = resp.has_focus();
        if focused {
            // keep Tab, arrows and Esc for the terminal instead of egui's focus navigation
            ui.memory_mut(|m| {
                m.set_focus_lock_filter(
                    id,
                    egui::EventFilter {
                        tab: true,
                        horizontal_arrows: true,
                        vertical_arrows: true,
                        escape: true,
                    },
                )
            });
            self.handle_input(ui);
        }
        if resp.hovered() {
            self.scroll_acc += ui.input(|i| i.smooth_scroll_delta().y) / ch;
            let lines = self.scroll_acc.trunc();
            if lines != 0.0 {
                self.scroll_acc -= lines;
                self.scroll = (self.scroll as isize + lines as isize).max(0) as usize;
            }
        }
        if resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Text);
        }

        let painter = ui.painter_at(rect);
        let bg = term_bg(p);
        painter.rect_filled(rect, 8.0, bg);
        if focused {
            painter.rect_stroke(
                rect,
                8.0,
                Stroke::new(1.0, theme::alpha(p.accent, 0.6)),
                egui::StrokeKind::Inside,
            );
        }

        let Ok(mut parser) = self.parser.lock() else {
            return;
        };
        parser.screen_mut().set_scrollback(self.scroll);
        self.scroll = parser.screen().scrollback();
        let screen = parser.screen();
        let origin = rect.min + vec2(pad, pad);
        for row in 0..rows {
            let y = origin.y + row as f32 * ch;
            let mut col = 0u16;
            while col < cols {
                let Some(cell) = screen.cell(row, col) else {
                    break;
                };
                let (fg, cbg) = cell_colors(cell, p, bg);
                // a run of narrow cells with the same colours is painted as one piece
                let start = col;
                let mut text = String::new();
                let wide = cell.is_wide();
                while let Some(c) = screen.cell(row, col) {
                    if c.is_wide_continuation() {
                        col += 1;
                        continue;
                    }
                    if col != start && (c.is_wide() || wide || cell_colors(c, p, bg) != (fg, cbg)) {
                        break;
                    }
                    let s = c.contents();
                    text.push_str(if s.is_empty() { " " } else { s });
                    col += if c.is_wide() { 2 } else { 1 };
                    if wide || col >= cols {
                        break;
                    }
                }
                let x = origin.x + start as f32 * cw;
                let width = (col - start) as f32 * cw;
                if cbg != bg {
                    painter.rect_filled(Rect::from_min_size(pos2(x, y), vec2(width, ch)), 0.0, cbg);
                }
                if !text.trim().is_empty() {
                    painter.text(pos2(x, y), egui::Align2::LEFT_TOP, text, font.clone(), fg);
                }
            }
        }
        // cursor
        if self.scroll == 0 && !screen.hide_cursor() {
            let (cr, cc) = screen.cursor_position();
            let r =
                Rect::from_min_size(origin + vec2(cc as f32 * cw, cr as f32 * ch), vec2(cw, ch));
            if focused {
                painter.rect_filled(r, 1.0, theme::alpha(p.accent_hover, 0.7));
            } else {
                painter.rect_stroke(
                    r,
                    1.0,
                    Stroke::new(1.0, p.subtext),
                    egui::StrokeKind::Inside,
                );
            }
        }
        if self.scroll > 0 {
            let tag = format!(
                "↑ {} lines back — scroll down or type to return",
                self.scroll
            );
            let g = painter.layout_no_wrap(tag, FontId::proportional(11.0), p.text_strong);
            let r = Rect::from_min_size(
                pos2(rect.right() - g.size().x - 22.0, rect.top() + 8.0),
                g.size() + vec2(12.0, 6.0),
            );
            painter.rect_filled(r, 6.0, p.surface2);
            painter.galley(r.min + vec2(6.0, 3.0), g, p.text_strong);
        }
        if self.exited() {
            let g = painter.layout_no_wrap(
                "[process exited — Restart to open a new shell]".into(),
                FontId::proportional(12.0),
                p.yellow,
            );
            painter.galley(
                pos2(origin.x, rect.bottom() - pad - g.size().y),
                g,
                p.yellow,
            );
        }
    }

    fn handle_input(&mut self, ui: &egui::Ui) {
        let (app_cursor, bracketed) = self
            .parser
            .lock()
            .map(|p| {
                (
                    p.screen().application_cursor(),
                    p.screen().bracketed_paste(),
                )
            })
            .unwrap_or((false, false));
        let events = ui.input(|i| i.events.clone());
        let mac = cfg!(target_os = "macos");
        for e in events {
            let bytes: Option<Vec<u8>> = match &e {
                egui::Event::Text(t) => Some(t.as_bytes().to_vec()),
                egui::Event::Paste(t) => Some(if bracketed {
                    format!("\x1b[200~{t}\x1b[201~").into_bytes()
                } else {
                    t.clone().into_bytes()
                }),
                egui::Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } => {
                    if modifiers.mac_cmd
                        || (mac && modifiers.command)
                        || (modifiers.ctrl && !mac && *key == egui::Key::V)
                    {
                        // ⌘-shortcuts belong to the app; Ctrl+V arrives as a Paste event too
                        None
                    } else if modifiers.ctrl {
                        ctrl_byte(*key).map(|b| vec![b])
                    } else {
                        key_bytes(*key, modifiers.shift, app_cursor).map(|s| s.as_bytes().to_vec())
                    }
                }
                _ => None,
            };
            if let Some(b) = bytes {
                self.scroll = 0;
                self.write(&b);
            }
        }
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

fn key_bytes(key: egui::Key, shift: bool, app_cursor: bool) -> Option<&'static str> {
    use egui::Key as K;
    Some(match key {
        K::Enter => "\r",
        K::Backspace => "\x7f",
        K::Tab => {
            if shift {
                "\x1b[Z"
            } else {
                "\t"
            }
        }
        K::Escape => "\x1b",
        K::ArrowUp => {
            if app_cursor {
                "\x1bOA"
            } else {
                "\x1b[A"
            }
        }
        K::ArrowDown => {
            if app_cursor {
                "\x1bOB"
            } else {
                "\x1b[B"
            }
        }
        K::ArrowRight => {
            if app_cursor {
                "\x1bOC"
            } else {
                "\x1b[C"
            }
        }
        K::ArrowLeft => {
            if app_cursor {
                "\x1bOD"
            } else {
                "\x1b[D"
            }
        }
        K::Home => "\x1b[H",
        K::End => "\x1b[F",
        K::PageUp => "\x1b[5~",
        K::PageDown => "\x1b[6~",
        K::Delete => "\x1b[3~",
        K::Insert => "\x1b[2~",
        K::F1 => "\x1bOP",
        K::F2 => "\x1bOQ",
        K::F3 => "\x1bOR",
        K::F4 => "\x1bOS",
        _ => return None,
    })
}

/// Ctrl+letter → the control character (Ctrl+C = 0x03 …).
fn ctrl_byte(key: egui::Key) -> Option<u8> {
    let name = key.name();
    let c = name.chars().next()?;
    if name.len() == 1 && c.is_ascii_alphabetic() {
        return Some((c.to_ascii_uppercase() as u8) & 0x1f);
    }
    match key {
        egui::Key::OpenBracket => Some(0x1b),
        egui::Key::Backslash => Some(0x1c),
        egui::Key::CloseBracket => Some(0x1d),
        egui::Key::Space => Some(0x00),
        _ => None,
    }
}

fn term_bg(p: &theme::Palette) -> Color32 {
    if p.dark {
        Color32::from_rgb(10, 12, 17)
    } else {
        Color32::from_rgb(250, 250, 252)
    }
}

fn cell_colors(cell: &vt100::Cell, p: &theme::Palette, bg: Color32) -> (Color32, Color32) {
    let mut fg = color(cell.fgcolor(), p.text, p);
    let mut b = color(cell.bgcolor(), bg, p);
    if cell.bold() && cell.fgcolor() == vt100::Color::Default {
        fg = p.text_strong;
    }
    if cell.dim() {
        fg = fg.gamma_multiply(0.6);
    }
    if cell.inverse() {
        std::mem::swap(&mut fg, &mut b);
    }
    (fg, b)
}

fn color(c: vt100::Color, default: Color32, p: &theme::Palette) -> Color32 {
    match c {
        vt100::Color::Default => default,
        vt100::Color::Rgb(r, g, b) => Color32::from_rgb(r, g, b),
        vt100::Color::Idx(i) => ansi(i, p),
    }
}

fn ansi(i: u8, p: &theme::Palette) -> Color32 {
    let rgb = Color32::from_rgb;
    match i {
        0 => rgb(40, 44, 54),
        1 => p.red,
        2 => p.green,
        3 => p.yellow,
        4 => p.blue,
        5 => p.purple,
        6 => p.teal,
        7 => rgb(200, 206, 218),
        8 => rgb(98, 107, 125),
        9 => rgb(255, 128, 135),
        10 => rgb(110, 225, 160),
        11 => rgb(250, 215, 125),
        12 => rgb(135, 185, 255),
        13 => rgb(195, 175, 255),
        14 => rgb(110, 220, 215),
        15 => rgb(245, 247, 250),
        16..=231 => {
            let n = i - 16;
            let v = |x: u8| if x == 0 { 0 } else { 55 + x * 40 };
            rgb(v(n / 36), v((n / 6) % 6), v(n % 6))
        }
        232..=255 => {
            let g = 8 + (i - 232) * 10;
            rgb(g, g, g)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_and_control_bytes() {
        assert_eq!(ctrl_byte(egui::Key::C), Some(0x03));
        assert_eq!(ctrl_byte(egui::Key::D), Some(0x04));
        assert_eq!(key_bytes(egui::Key::ArrowUp, false, false), Some("\x1b[A"));
        assert_eq!(key_bytes(egui::Key::ArrowUp, false, true), Some("\x1bOA"));
        assert_eq!(key_bytes(egui::Key::Tab, true, false), Some("\x1b[Z"));
    }

    #[test]
    #[cfg(unix)]
    fn runs_a_shell_in_a_pty() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = egui::Context::default();
        let mut t = Terminal::spawn(dir.path(), &ctx).unwrap();
        t.run_line("echo foxgit-$((40+2)); exit");
        for _ in 0..200 {
            if t.contents().contains("foxgit-42") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        assert!(t.contents().contains("foxgit-42"), "{}", t.contents());
    }
}
