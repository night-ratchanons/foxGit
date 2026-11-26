#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")] // no console window on Windows release builds

mod account;
mod app;
mod askpass;
mod branchtree;
mod diffview;
mod git;
mod github;
mod graph;
mod tabs;
mod theme;
mod views;

use eframe::egui;

fn main() -> eframe::Result {
    // Started by git/ssh to ask for a password? Answer it and exit (no window).
    if let Some(code) = askpass::client_main() {
        std::process::exit(code);
    }
    let initial_repos: Vec<String> = std::env::args().skip(1).collect();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("RustTree")
            .with_inner_size([1320.0, 820.0])
            .with_min_inner_size([800.0, 500.0]),
        // Thai / CJK / other scripts in commit messages and file names are rendered with
        // system fonts loaded on demand (eframe `system_fonts` feature).
        system_font_fallback: true,
        ..Default::default()
    };

    eframe::run_native(
        "RustTree",
        options,
        Box::new(move |cc| Ok(Box::new(app::RustTreeApp::new(cc, initial_repos)))),
    )
}
