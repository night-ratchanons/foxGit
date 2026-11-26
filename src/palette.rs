//! Command palette (⌘K / ⇧⌘P): fuzzy search over everything you can do — switch project or mode,
//! git actions, check out a branch, run a script, start an agent, open settings …

use crate::app::View;
use crate::theme::pal;
use eframe::egui;
use egui::{pos2, vec2, FontId, RichText, Sense};

#[derive(Clone, Debug, PartialEq)]
pub enum Cmd {
    SelectTab(usize),
    OpenProject(String),
    Projects,
    View(View),
    Fetch,
    Pull,
    Push,
    NewBranch,
    Stash,
    Checkout(String),
    Run(String, String),
    NewTerminal,
    NewAgent,
    NewPr,
    AiMessage,
    Settings,
    ToggleTheme,
    OpenEditor,
    OpenTerminalApp,
    Reveal,
    Dashboard,
}

#[derive(Clone)]
pub struct Item {
    pub group: &'static str,
    pub label: String,
    pub hint: String,
    pub cmd: Cmd,
}

impl Item {
    pub fn new(group: &'static str, label: impl Into<String>, cmd: Cmd) -> Self {
        Item {
            group,
            label: label.into(),
            hint: String::new(),
            cmd,
        }
    }

    pub fn hint(mut self, h: impl Into<String>) -> Self {
        self.hint = h.into();
        self
    }
}

/// Subsequence fuzzy score (higher is better); `None` when `q` doesn't match.
pub fn score(q: &str, text: &str) -> Option<i32> {
    if q.is_empty() {
        return Some(0);
    }
    let t: Vec<char> = text.to_lowercase().chars().collect();
    let mut s = 0;
    let mut ti = 0;
    let mut prev: Option<usize> = None;
    for qc in q.to_lowercase().chars().filter(|c| !c.is_whitespace()) {
        let found = (ti..t.len()).find(|&i| t[i] == qc)?;
        // consecutive letters and word starts count more
        if prev.is_some_and(|p| p + 1 == found) {
            s += 8;
        }
        if found == 0 || !t[found - 1].is_alphanumeric() {
            s += 6;
        }
        s -= (found - ti).min(10) as i32;
        prev = Some(found);
        ti = found + 1;
    }
    // shorter labels first when equally good
    Some(s * 4 - t.len() as i32 / 4)
}

#[derive(Default)]
pub struct Palette {
    open: bool,
    query: String,
    selected: usize,
    items: Vec<Item>,
    focus: bool,
}

impl Palette {
    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn open(&mut self, items: Vec<Item>) {
        self.open = true;
        self.query.clear();
        self.selected = 0;
        self.items = items;
        self.focus = true;
    }

    pub fn ui(&mut self, ctx: &egui::Context) -> Option<Cmd> {
        if !self.open {
            return None;
        }
        let mut matches: Vec<(i32, usize)> = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(i, it)| {
                score(&self.query, &format!("{} {}", it.label, it.group)).map(|s| (s, i))
            })
            .collect();
        if !self.query.is_empty() {
            matches.sort_by_key(|m| std::cmp::Reverse(m.0));
        }
        let (up, down, enter, esc) = ctx.input_mut(|i| {
            (
                i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp),
                i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown),
                i.consume_key(egui::Modifiers::NONE, egui::Key::Enter),
                i.consume_key(egui::Modifiers::NONE, egui::Key::Escape),
            )
        });
        if down {
            self.selected = (self.selected + 1).min(matches.len().saturating_sub(1));
        }
        if up {
            self.selected = self.selected.saturating_sub(1);
        }
        self.selected = self.selected.min(matches.len().saturating_sub(1));
        let mut picked = None;
        if enter {
            picked = matches
                .get(self.selected)
                .map(|m| self.items[m.1].cmd.clone());
        }

        let screen = ctx.content_rect();
        let width = (screen.width() - 40.0).clamp(320.0, 640.0);
        let mut close = esc;
        let area = egui::Area::new(egui::Id::unique("command_palette"))
            .order(egui::Order::Foreground)
            .fixed_pos(pos2(screen.center().x - width / 2.0, screen.top() + 70.0))
            .show(ctx, |ui| {
                let p = pal(ui);
                egui::Frame::new()
                    .fill(p.raised)
                    .stroke(egui::Stroke::new(1.0, p.border_strong))
                    .corner_radius(12.0)
                    .shadow(egui::epaint::Shadow {
                        offset: [0, 10],
                        blur: 30,
                        spread: 0,
                        color: egui::Color32::from_black_alpha(110),
                    })
                    .inner_margin(egui::Margin::same(10))
                    .show(ui, |ui| {
                        ui.set_width(width);
                        let r = ui.add(
                            egui::TextEdit::singleline(&mut self.query)
                                .hint_text("Type a command, branch, project or script…")
                                .desired_width(f32::INFINITY)
                                .font(FontId::proportional(16.0))
                                .margin(vec2(10.0, 8.0)),
                        );
                        if self.focus {
                            r.request_focus();
                            self.focus = false;
                        }
                        if r.changed() {
                            self.selected = 0;
                        }
                        ui.add_space(6.0);
                        if matches.is_empty() {
                            ui.label(RichText::new("No matches").color(p.faint));
                        }
                        let row_h = 30.0;
                        let rows = matches.len().min(400);
                        egui::ScrollArea::vertical()
                            .max_height(420.0)
                            .auto_shrink([false, true])
                            .show_rows(ui, row_h, rows, |ui, range| {
                                for k in range {
                                    let it = &self.items[matches[k].1];
                                    let (rect, resp) = ui.allocate_exact_size(
                                        vec2(ui.available_width(), row_h),
                                        Sense::click(),
                                    );
                                    let sel = k == self.selected;
                                    let painter = ui.painter();
                                    if sel {
                                        painter.rect_filled(rect, 7.0, p.selection);
                                    } else if resp.hovered() {
                                        painter.rect_filled(rect, 7.0, p.hover);
                                    }
                                    painter.text(
                                        pos2(rect.left() + 10.0, rect.center().y),
                                        egui::Align2::LEFT_CENTER,
                                        &it.label,
                                        FontId::proportional(14.0),
                                        if sel { p.sel_text } else { p.text_strong },
                                    );
                                    let right = if it.hint.is_empty() {
                                        it.group.to_string()
                                    } else {
                                        format!("{}  ·  {}", it.hint, it.group)
                                    };
                                    painter.text(
                                        pos2(rect.right() - 10.0, rect.center().y),
                                        egui::Align2::RIGHT_CENTER,
                                        right,
                                        FontId::proportional(12.0),
                                        p.faint,
                                    );
                                    if resp.clicked() {
                                        picked = Some(it.cmd.clone());
                                    }
                                    if sel && (up || down) {
                                        resp.scroll_to_me(None);
                                    }
                                }
                            });
                        ui.add_space(4.0);
                        ui.label(
                            RichText::new("↑↓ to move · Enter to run · Esc to close")
                                .small()
                                .color(p.faint),
                        );
                    });
            });
        if area.response.clicked_elsewhere() {
            close = true;
        }
        if picked.is_some() || close {
            self.open = false;
        }
        picked
    }
}

#[cfg(test)]
mod tests {
    use super::score;

    #[test]
    fn fuzzy_scores() {
        assert!(score("fet", "Fetch").is_some());
        assert!(score("xyz", "Fetch").is_none());
        assert!(score("nb", "New branch…").is_some());
        // a prefix beats letters scattered through a longer label
        assert!(
            score("push", "Push").unwrap() > score("push", "Pull request from a push").unwrap()
        );
        assert!(
            score("co main", "Checkout main").unwrap()
                > score("co main", "Commit to domain").unwrap_or(i32::MIN)
        );
        assert_eq!(score("", "anything"), Some(0));
    }
}
