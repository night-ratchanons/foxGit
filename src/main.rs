#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")] // no console window on Windows release builds

mod account;
mod agents;
mod aicommit;
mod app;
mod askpass;
mod branchtree;
mod conflict;
mod desktop;
mod diffview;
mod editor;
mod git;
mod github;
mod graph;
mod hub;
mod instance;
mod palette;
mod runner;
mod search;
mod shellenv;
mod tabs;
mod terminal;
mod theme;
mod update;
mod views;

use eframe::egui;

pub const APP_NAME: &str = "FoxGit";

fn main() -> eframe::Result {
    // Started by git/ssh to ask for a password? Answer it and exit (no window).
    if let Some(code) = askpass::client_main() {
        std::process::exit(code);
    }
    // Finder may pass a "-psn_…" process serial number to apps it launches.
    let initial_repos: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| !a.starts_with("-psn_"))
        .collect();
    migrate_storage();
    // Already running? Hand the folders to that window instead of opening a second one.
    let lock = instance::lock_path();
    if std::env::var_os("FOXGIT_NEW_INSTANCE").is_none() {
        if let Some(l) = &lock {
            if instance::forward(l, &initial_repos) {
                return Ok(());
            }
        }
    }
    let incoming = lock.as_deref().and_then(instance::listen);
    install_crash_log();
    shellenv::init();

    let mut viewport = egui::ViewportBuilder::default()
        .with_title(APP_NAME)
        .with_app_id(APP_NAME)
        .with_inner_size([1320.0, 820.0])
        .with_min_inner_size([800.0, 500.0]);
    // Window / Dock / taskbar icon (eframe also sets the Dock icon on macOS).
    if let Ok(icon) =
        eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon/foxgit-512.png"))
    {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions {
        viewport,
        // Thai / CJK / other scripts in commit messages and file names are rendered with
        // system fonts loaded on demand (eframe `system_fonts` feature).
        system_font_fallback: true,
        ..Default::default()
    };

    eframe::run_native(
        APP_NAME,
        options,
        Box::new(move |cc| {
            let _ = instance::CTX.set(cc.egui_ctx.clone());
            Ok(Box::new(app::FoxGitApp::new(cc, initial_repos, incoming)))
        }),
    )
}

/// Where a panic report is written (shown to the user on the next start).
pub fn crash_log_path() -> Option<std::path::PathBuf> {
    eframe::storage_dir(APP_NAME).map(|d| d.join("crash.log"))
}

fn install_crash_log() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if let Some(path) = crash_log_path() {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let report = format!(
                "FoxGit {} ({} {}) — {}\n{info}\n\n{}\n",
                update::VERSION,
                std::env::consts::OS,
                std::env::consts::ARCH,
                chrono::Local::now().to_rfc3339(),
                std::backtrace::Backtrace::force_capture(),
            );
            let _ = std::fs::write(&path, report);
        }
        default(info);
    }));
}

/// The app used to be called RustTree: carry its saved state (open tabs, projects, settings)
/// over to the new storage folder once.
fn migrate_storage() {
    let (Some(old), Some(new)) = (
        eframe::storage_dir("RustTree"),
        eframe::storage_dir(APP_NAME),
    ) else {
        return;
    };
    let (from, to) = (old.join("app.ron"), new.join("app.ron"));
    if from.exists() && !to.exists() && std::fs::create_dir_all(&new).is_ok() {
        let _ = std::fs::copy(&from, &to);
    }
}
