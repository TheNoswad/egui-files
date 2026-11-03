//! A tabbed file browser. The desktop binary and the Android library both
//! start it from here.

// Files are dropped to close them before a rename or removal; on
// wasm32-unknown-unknown `File` has no `Drop`, so clippy calls that pointless.
#![cfg_attr(target_arch = "wasm32", allow(clippy::drop_non_drop))]
// clippy.toml's disallowed items fail only in the browser, and natively the
// replacements are the very same items.
#![cfg_attr(
    not(target_arch = "wasm32"),
    allow(clippy::disallowed_methods, clippy::disallowed_types)
)]

mod about;
#[cfg(target_os = "android")]
mod android;
mod app;
mod archive;
mod background;
mod dates;
#[cfg(target_arch = "wasm32")]
mod demo;
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
mod remote;
#[cfg(target_os = "android")]
mod renderer;
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
#[cfg(target_arch = "wasm32")]
mod web_files;
mod widgets;
mod window_buttons;
#[cfg(windows)]
mod windows_drag;
#[cfg(windows)]
mod windows_frame;
#[cfg(target_os = "linux")]
mod x11_dnd;

/// Runs the desktop app.
#[cfg(not(target_arch = "wasm32"))]
pub fn run() -> eframe::Result {
    // Started by ssh to ask the running app for a password or the like.
    if let Some(code) = disk::sftp::askpass_main() {
        std::process::exit(code);
    }
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
        wgpu_options: wgpu_options(false),
        ..Default::default()
    };
    eframe::run_native(
        "egui-files",
        options,
        Box::new(|cc| Ok(Box::new(app::FilesApp::new(cc)))),
    )
}

/// Starts the app in the page's `<canvas id="app">` (see web/index.html).
#[cfg(target_arch = "wasm32")]
pub fn run_web() {
    use wasm_bindgen::JsCast;

    eframe::WebLogger::init(log::LevelFilter::Info).ok();
    demo::create();
    wasm_bindgen_futures::spawn_local(async {
        let document = web_sys::window().and_then(|w| w.document());
        let Some(canvas) = document
            .and_then(|d| d.get_element_by_id("app"))
            .and_then(|e| e.dyn_into::<web_sys::HtmlCanvasElement>().ok())
        else {
            log::error!("no <canvas id=\"app\"> to draw in");
            return;
        };
        let started = eframe::WebRunner::new()
            .start(
                canvas,
                eframe::WebOptions::default(),
                Box::new(|cc| {
                    web_files::install(&cc.egui_ctx);
                    demo::set_context(&cc.egui_ctx);
                    Ok(Box::new(app::FilesApp::new(cc)))
                }),
            )
            .await;
        // For web/demo/loading.js, which waits to show the app.
        let event = match started {
            Ok(()) => "egui-files-started",
            Err(err) => {
                log::error!("egui-files didn't start: {err:?}");
                "egui-files-failed"
            }
        };
        if let (Some(window), Ok(event)) = (web_sys::window(), web_sys::Event::new(event)) {
            let _ = window.dispatch_event(&event);
        }
    });
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
        wgpu_options: wgpu_options(renderer::Renderer::start() == renderer::Renderer::OpenGl),
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

#[cfg(not(target_arch = "wasm32"))]
/// egui-wgpu enables the GL backend alongside Vulkan/DX12/Metal by default,
/// which loads the whole OpenGL driver (~20 MB on Mesa) even though it is never
/// used. Only fall back to GL when no primary backend has an adapter.
/// `WGPU_BACKEND` still overrides this, as `opengl` (asking for GL) does.
fn wgpu_options(opengl: bool) -> eframe::egui_wgpu::WgpuConfiguration {
    use eframe::egui_wgpu::{WgpuConfiguration, WgpuSetup, wgpu};

    let mut config = WgpuConfiguration::default();
    if let WgpuSetup::CreateNew(setup) = &mut config.wgpu_setup
        && wgpu::Backends::from_env().is_none()
    {
        if opengl {
            setup.instance_descriptor.backends = wgpu::Backends::GL;
            return config;
        }
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
