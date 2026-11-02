#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod about;
mod app;
mod archive;
mod dates;
mod devices;
mod dnd;
mod dock_tab;
#[cfg(target_os = "linux")]
mod drag_icon;
mod editor;
#[cfg(windows)]
mod file_clipboard;
mod fs;
mod images;
mod jobs;
mod keymap;
mod list_columns;
mod open_with;
mod pane_sizes;
mod places;
mod preview;
mod properties;
mod settings;
mod sidebar;
mod system_theme;
mod tab;
mod tab_windows;
mod terminal;
mod terminal_panel;
mod theme;
mod thumbnails;
mod titlebar;
mod trash;
mod tree;
mod usage;
mod video;
mod views;
mod watcher;
#[cfg(target_os = "linux")]
mod wayland_dnd;
mod widgets;
mod window_buttons;
#[cfg(windows)]
mod windows_drag;
#[cfg(windows)]
mod windows_frame;
#[cfg(target_os = "linux")]
mod x11_dnd;

fn main() -> eframe::Result {
    // Started elevated to apply changes from the Properties window.
    if let Some(code) = properties::run_helper() {
        std::process::exit(code);
    }
    // Started elevated to read a drive's master file table for disk usage.
    if let Some(code) = usage::run_helper() {
        std::process::exit(code);
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("egui-files")
            .with_app_id("egui-files")
            .with_inner_size([1100.0, 700.0])
            .with_min_inner_size([480.0, 320.0])
            .with_decorations(!titlebar::saved_enabled("egui-files")),
        wgpu_options: wgpu_options(),
        ..Default::default()
    };
    eframe::run_native(
        "egui-files",
        options,
        Box::new(|cc| Ok(Box::new(app::FilesApp::new(cc)))),
    )
}

/// egui-wgpu enables the GL backend alongside Vulkan/DX12/Metal by default,
/// which loads the whole OpenGL driver (~20 MB on Mesa) even though it is never
/// used. Only fall back to GL when no primary backend has an adapter.
/// `WGPU_BACKEND` still overrides this.
fn wgpu_options() -> eframe::egui_wgpu::WgpuConfiguration {
    use eframe::egui_wgpu::{WgpuConfiguration, WgpuSetup, wgpu};

    let mut config = WgpuConfiguration::default();
    if let WgpuSetup::CreateNew(setup) = &mut config.wgpu_setup
        && wgpu::Backends::from_env().is_none()
    {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::PRIMARY,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        // Native adapter enumeration completes immediately.
        let adapters = std::pin::pin!(instance.enumerate_adapters(wgpu::Backends::PRIMARY))
            .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()));
        let has_primary = matches!(adapters, std::task::Poll::Ready(a) if !a.is_empty());
        setup.instance_descriptor.backends = if has_primary {
            wgpu::Backends::PRIMARY
        } else {
            wgpu::Backends::GL
        };
    }
    config
}
