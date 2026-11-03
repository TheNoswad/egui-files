//! The Android entry point: Android loads this library and calls
//! `android_main` on a thread of its own.

#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
fn android_main(app: egui_winit::winit::platform::android::activity::AndroidApp) {
    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Info)
            .with_filter(
                android_logger::FilterBuilder::new()
                    .filter_level(log::LevelFilter::Info)
                    // With OpenGL, Mali's driver warns of flushed
                    // submissions on nearly every frame.
                    .filter_module("wgpu_hal::gles", log::LevelFilter::Error)
                    .build(),
            )
            .with_tag("egui-files"),
    );
    // Panics would otherwise vanish: Android drops stderr.
    std::panic::set_hook(Box::new(|info| log::error!("{info}")));
    egui_files::run_android(app);
}
