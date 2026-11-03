#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(not(target_arch = "wasm32"))]
fn main() -> eframe::Result {
    egui_files::run()
}

// Trunk builds this binary for the web demo (see web/).
#[cfg(target_arch = "wasm32")]
fn main() {
    egui_files::run_web();
}
