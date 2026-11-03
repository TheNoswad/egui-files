//! A tabbed file browser. The desktop binary and the Android library both
//! start it from here.

mod about;
#[cfg(target_os = "android")]
mod android;
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
mod haptics;
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
mod touch;
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

/// Runs the desktop app.
pub fn run() -> eframe::Result {
    // Started elevated to apply changes from the Properties window.
    if let Some(code) = properties::run_helper() {
        std::process::exit(code);
    }
    // Started elevated to read a drive's master file table for disk usage.
    if let Some(code) = usage::run_helper() {
        std::process::exit(code);
    }
    // The mobile layout keeps the system's frame and goes narrower.
    let mobile = app::saved_mobile("egui-files");
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("egui-files")
            .with_app_id("egui-files")
            .with_inner_size([1100.0, 700.0])
            .with_min_inner_size(if mobile {
                app::MOBILE_MIN_SIZE
            } else {
                app::DESKTOP_MIN_SIZE
            })
            .with_decorations(mobile || !titlebar::saved_enabled("egui-files")),
        wgpu_options: wgpu_options(),
        ..Default::default()
    };
    eframe::run_native(
        "egui-files",
        options,
        Box::new(|cc| Ok(Box::new(app::FilesApp::new(cc)))),
    )
}

/// Runs the app on Android, from the `android` crate's `android_main`.
#[cfg(target_os = "android")]
pub fn run_android(android_app: egui_winit::winit::platform::android::activity::AndroidApp) {
    android::init(&android_app);
    let data = android_app.internal_data_path();
    // SAFETY: set as the app starts, before it starts threads of its own
    // that could read the environment.
    unsafe {
        // Home is the phone's shared storage, as file managers show it.
        std::env::set_var("HOME", "/storage/emulated/0");
        // Settings, themes and caches stay private to the app.
        if let Some(data) = &data {
            std::env::set_var("XDG_CONFIG_HOME", data.join("config"));
            std::env::set_var("XDG_DATA_HOME", data.join("data"));
            std::env::set_var("XDG_CACHE_HOME", data.join("cache"));
        }
    }
    let options = eframe::NativeOptions {
        android_app: Some(android_app),
        persistence_path: data.map(|d| d.join("app.ron")),
        wgpu_options: wgpu_options(),
        ..Default::default()
    };
    if let Err(err) = eframe::run_native(
        "egui-files",
        options,
        Box::new(|cc| Ok(Box::new(app::FilesApp::new(cc)))),
    ) {
        log::error!("egui-files stopped: {err}");
    }
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
