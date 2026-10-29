#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod dnd;
mod dock_tab;
mod fs;
mod images;
mod jobs;
mod places;
mod preview;
mod tab;
mod trash;
mod tree;
mod views;
mod watcher;
#[cfg(target_os = "linux")]
mod wayland_dnd;
mod widgets;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("egui-files")
            .with_app_id("egui-files")
            .with_inner_size([1100.0, 700.0])
            .with_min_inner_size([480.0, 320.0]),
        ..Default::default()
    };
    eframe::run_native(
        "egui-files",
        options,
        Box::new(|cc| Ok(Box::new(app::FilesApp::new(cc)))),
    )
}
