#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod fs;
mod places;
mod tab;
mod views;

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
