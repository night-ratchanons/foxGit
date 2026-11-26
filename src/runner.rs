//! Run panel: the project's scripts (package.json with the right package manager, Cargo,
//! Makefile targets, Go) run in terminals docked at the bottom of a repository tab, with an
//! "Open in browser" button once a dev server prints its localhost URL.

use crate::terminal::Terminal;
use crate::theme::pal;
use eframe::egui;
use egui::{vec2, RichText, Stroke};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq)]
pub struct Task {
    pub label: String,
    pub command: String,
    pub group: &'static str,
}

/// Package manager of a JavaScript project, from its lockfile.
pub fn package_manager(dir: &Path) -> &'static str {
    if dir.join("pnpm-lock.yaml").exists() {
        "pnpm"
    } else if dir.join("bun.lockb").exists() || dir.join("bun.lock").exists() {
        "bun"
    } else if dir.join("yarn.lock").exists() {
        "yarn"
    } else {
        "npm"
    }
}

/// Everything runnable we can find in `dir`.
pub fn detect(dir: &Path) -> Vec<Task> {
    let mut out = Vec::new();
    if let Ok(text) = std::fs::read_to_string(dir.join("package.json")) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
            if let Some(scripts) = v.get("scripts").and_then(|s| s.as_object()) {
                let pm = package_manager(dir);
                let mut names: Vec<&String> = scripts.keys().collect();
                let rank = |n: &str| {
                    ["dev", "start", "serve", "build", "preview", "test", "lint"]
                        .iter()
                        .position(|x| *x == n)
                        .unwrap_or(99)
                };
                names.sort_by_key(|n| (rank(n), n.to_string()));
                for n in names {
                    let command = match pm {
                        "yarn" => format!("yarn {n}"),
                        pm => format!("{pm} run {n}"),
                    };
                    out.push(Task {
                        label: n.clone(),
                        command,
                        group: "package.json",
                    });
                }
                if !dir.join("node_modules").exists() {
                    out.insert(
                        0,
                        Task {
                            label: "install".into(),
                            command: format!("{pm} install"),
                            group: "package.json",
                        },
                    );
                }
            }
        }
    }
    if dir.join("Cargo.toml").exists() {
        for (l, c) in [
            ("run", "cargo run"),
            ("run --release", "cargo run --release"),
            ("test", "cargo test"),
            ("build", "cargo build"),
            ("clippy", "cargo clippy"),
        ] {
            out.push(Task {
                label: l.into(),
                command: c.into(),
                group: "Cargo",
            });
        }
    }
    for name in ["Makefile", "makefile", "GNUmakefile"] {
        if let Ok(text) = std::fs::read_to_string(dir.join(name)) {
            for t in make_targets(&text) {
                out.push(Task {
                    command: format!("make {t}"),
                    label: t,
                    group: "Makefile",
                });
            }
            break;
        }
    }
    if dir.join("go.mod").exists() {
        for (l, c) in [
            ("run .", "go run ."),
            ("test ./...", "go test ./..."),
            ("build", "go build ./..."),
        ] {
            out.push(Task {
                label: l.into(),
                command: c.into(),
                group: "Go",
            });
        }
    }
    out
}

/// Explicit targets of a Makefile (no pattern rules, special targets or variable assignments).
pub fn make_targets(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        if line.starts_with(['\t', ' ', '#', '.']) {
            continue;
        }
        let Some((head, rest)) = line.split_once(':') else {
            continue;
        };
        if rest.starts_with('=') || head.contains('=') || head.contains('%') || head.contains('$') {
            continue;
        }
        for t in head.split_whitespace() {
            if t.chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_./".contains(c))
                && !out.iter().any(|x| x == t)
            {
                out.push(t.to_string());
            }
        }
    }
    out
}

/// The first `http://localhost:…` style URL in a terminal's text.
pub fn find_url(text: &str) -> Option<String> {
    for start in [
        "http://localhost:",
        "https://localhost:",
        "http://127.0.0.1:",
        "http://0.0.0.0:",
        "http://[::1]:",
    ] {
        if let Some(i) = text.find(start) {
            let url: String = text[i..]
                .chars()
                .take_while(|c| !c.is_whitespace() && !"\"'<>)]│".contains(*c))
                .collect();
            let url = url
                .trim_end_matches(['.', ',', ';'])
                .replace("0.0.0.0", "localhost");
            if url.len() > start.len() {
                return Some(url);
            }
        }
    }
    None
}

pub struct Session {
    pub title: String,
    pub command: Option<String>,
    term: Terminal,
    url: Option<String>,
    scanned: Instant,
}

#[derive(Default)]
pub struct RunPanel {
    pub open: bool,
    sessions: Vec<Session>,
    active: usize,
    tasks: Option<(Instant, PathBuf, Vec<Task>)>,
}

impl RunPanel {
    /// Detected tasks (re-scanned at most every 10 s).
    pub fn tasks(&mut self, dir: &Path) -> Vec<Task> {
        let fresh = self
            .tasks
            .as_ref()
            .is_some_and(|(t, d, _)| d == dir && t.elapsed() < Duration::from_secs(10));
        if !fresh {
            self.tasks = Some((Instant::now(), dir.to_path_buf(), detect(dir)));
        }
        self.tasks.as_ref().map(|t| t.2.clone()).unwrap_or_default()
    }

    /// Sessions whose shell is still alive.
    pub fn alive(&self) -> usize {
        self.sessions.iter().filter(|s| !s.term.exited()).count()
    }

    /// Open a terminal in `dir`, optionally running `command` in it.
    pub fn start(
        &mut self,
        dir: &Path,
        ctx: &egui::Context,
        title: &str,
        command: Option<&str>,
    ) -> Result<(), String> {
        // re-run in the session that already ran this command, if it's finished
        if let Some(cmd) = command {
            if let Some(i) = self
                .sessions
                .iter()
                .position(|s| s.command.as_deref() == Some(cmd) && !s.term.exited())
            {
                let s = &mut self.sessions[i];
                s.term.write(&[3]);
                s.term.run_line(cmd);
                s.url = None;
                self.active = i;
                self.open = true;
                return Ok(());
            }
        }
        let mut term = Terminal::spawn(dir, ctx)?;
        if let Some(cmd) = command {
            term.run_line(cmd);
        }
        self.sessions.push(Session {
            title: title.to_string(),
            command: command.map(String::from),
            term,
            url: None,
            scanned: Instant::now(),
        });
        self.active = self.sessions.len() - 1;
        self.open = true;
        Ok(())
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, dir: &Path) {
        let p = pal(ui);
        let ctx = ui.ctx().clone();
        // strip: one pill per session + actions
        let mut close = None;
        let mut start: Option<(String, Option<String>)> = None;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            ui.label(RichText::new("▶ RUN").small().strong().color(p.faint));
            for (i, s) in self.sessions.iter().enumerate() {
                let dead = s.term.exited();
                let sel = i == self.active;
                let text = RichText::new(format!("{}{}", if dead { "■ " } else { "● " }, s.title))
                    .color(if sel { p.text_strong } else { p.subtext });
                let r = ui.add(
                    egui::Button::new(text)
                        .fill(if sel {
                            p.surface1
                        } else {
                            egui::Color32::TRANSPARENT
                        })
                        .stroke(if sel {
                            Stroke::new(1.0, p.border)
                        } else {
                            Stroke::NONE
                        })
                        .corner_radius(6.0),
                );
                if r.clicked() {
                    self.active = i;
                }
                if r.middle_clicked() {
                    close = Some(i);
                }
                r.context_menu(|ui| {
                    if ui.button("Close").clicked() {
                        close = Some(i);
                        ui.close();
                    }
                });
            }
            let add = ui.small_button("✚").on_hover_text("New terminal or task");
            egui::Popup::menu(&add).show(|ui| {
                ui.set_min_width(220.0);
                if ui.button("New terminal").clicked() {
                    start = Some(("Terminal".into(), None));
                    ui.close();
                }
                let tasks = self.tasks(dir);
                let mut group = "";
                for t in &tasks {
                    if t.group != group {
                        group = t.group;
                        ui.separator();
                        ui.label(RichText::new(group).small().color(p.faint));
                    }
                    if ui.button(&t.label).on_hover_text(&t.command).clicked() {
                        start = Some((t.label.clone(), Some(t.command.clone())));
                        ui.close();
                    }
                }
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .small_button("Hide")
                    .on_hover_text("Hide the panel (terminals keep running)")
                    .clicked()
                {
                    self.open = false;
                }
                if let Some(s) = self.sessions.get_mut(self.active) {
                    if s.scanned.elapsed() > Duration::from_millis(800) && s.url.is_none() {
                        s.scanned = Instant::now();
                        s.url = find_url(&s.term.contents());
                    }
                    if let Some(u) = s.url.clone() {
                        if ui
                            .add(
                                egui::Button::new(
                                    RichText::new(format!("🌐 {u}")).color(p.on_accent),
                                )
                                .fill(p.accent)
                                .stroke(Stroke::NONE),
                            )
                            .on_hover_text("Open in the browser")
                            .clicked()
                        {
                            ctx.open_url(egui::OpenUrl::new_tab(u));
                        }
                    }
                    if let Some(cmd) = s.command.clone() {
                        if ui
                            .small_button("↻")
                            .on_hover_text(format!("Restart: {cmd}"))
                            .clicked()
                        {
                            if s.term.exited() {
                                start = Some((s.title.clone(), Some(cmd)));
                            } else {
                                s.term.write(&[3]);
                                s.term.run_line(&cmd);
                                s.url = None;
                            }
                        }
                    }
                    if !s.term.exited()
                        && ui
                            .small_button("■")
                            .on_hover_text("Stop (Ctrl+C)")
                            .clicked()
                    {
                        s.term.write(&[3]);
                    }
                }
            });
        });
        if let Some(i) = close {
            self.sessions.remove(i);
            self.active = self.active.min(self.sessions.len().saturating_sub(1));
        }
        if let Some((title, cmd)) = start {
            if let Err(e) = self.start(dir, &ctx, &title, cmd.as_deref()) {
                ui.label(RichText::new(format!("Could not start a terminal: {e}")).color(p.red));
            }
        }
        ui.add_space(2.0);
        match self.sessions.get_mut(self.active) {
            Some(s) => s.term.ui(ui),
            None => {
                ui.add_space(8.0);
                ui.vertical_centered(|ui| {
                    ui.label(
                        RichText::new("Nothing running. Use ✚ to open a terminal or run a script.")
                            .color(p.faint),
                    );
                });
                ui.allocate_space(vec2(ui.available_width(), ui.available_height()));
            }
        }
    }
}

/// Toolbar ▶ Run menu contents. Returns the task picked, or `Some(None)` for a plain terminal.
pub fn menu(
    ui: &mut egui::Ui,
    panel: &mut RunPanel,
    dir: &Path,
) -> Option<(String, Option<String>)> {
    let p = pal(ui);
    let mut pick = None;
    ui.set_min_width(240.0);
    let tasks = panel.tasks(dir);
    if tasks.is_empty() {
        ui.label(
            RichText::new("No package.json scripts, Cargo, Makefile or go.mod here.")
                .small()
                .color(p.faint),
        );
    }
    let mut group = "";
    for t in &tasks {
        if t.group != group {
            if !group.is_empty() {
                ui.separator();
            }
            group = t.group;
            ui.label(RichText::new(group).small().color(p.faint));
        }
        if ui
            .button(format!("▶  {}", t.label))
            .on_hover_text(&t.command)
            .clicked()
        {
            pick = Some((t.label.clone(), Some(t.command.clone())));
            ui.close();
        }
    }
    ui.separator();
    if ui.button("New terminal").clicked() {
        pick = Some(("Terminal".into(), None));
        ui.close();
    }
    let label = if panel.open {
        "Hide run panel"
    } else {
        "Show run panel"
    };
    if ui.button(label).clicked() {
        panel.open = !panel.open;
        ui.close();
    }
    pick
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn detects_scripts_with_the_right_package_manager() {
        let t = tempfile::tempdir().unwrap();
        let d = t.path();
        fs::write(
            d.join("package.json"),
            r#"{"scripts":{"lint":"eslint","build":"vite build","dev":"vite"}}"#,
        )
        .unwrap();
        fs::write(d.join("pnpm-lock.yaml"), "").unwrap();
        fs::create_dir(d.join("node_modules")).unwrap();
        let tasks = detect(d);
        let cmds: Vec<&str> = tasks.iter().map(|t| t.command.as_str()).collect();
        assert_eq!(cmds, ["pnpm run dev", "pnpm run build", "pnpm run lint"]);
        fs::remove_file(d.join("pnpm-lock.yaml")).unwrap();
        fs::write(d.join("yarn.lock"), "").unwrap();
        assert_eq!(detect(d)[0].command, "yarn dev");
        fs::remove_dir(d.join("node_modules")).unwrap();
        assert_eq!(detect(d)[0].command, "yarn install");
    }

    #[test]
    fn makefile_targets() {
        let mk = "CC = gcc\n.PHONY: all test\nall: build\nbuild test: deps\n\t$(CC) x\n%.o: %.c\nVAR := 1\n# c: x\ndeploy:\n";
        assert_eq!(make_targets(mk), ["all", "build", "test", "deploy"]);
    }

    #[test]
    fn finds_dev_server_urls() {
        assert_eq!(
            find_url("  VITE v5  ready\n  ➜  Local:   http://localhost:5173/\n").as_deref(),
            Some("http://localhost:5173/")
        );
        assert_eq!(
            find_url("Listening on http://0.0.0.0:3000.").as_deref(),
            Some("http://localhost:3000")
        );
        assert_eq!(find_url("no url here"), None);
        assert_eq!(find_url("http://localhost: oops"), None);
    }
}
