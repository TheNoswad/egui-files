//! What the app needs from Android itself, through JNI: how much of the
//! screen the status and navigation bars cover. Apps targeting Android 15
//! and up draw under them, and egui-winit only reports these insets on iOS.

use std::{
    sync::OnceLock,
    time::{Duration, Instant},
};

use egui::{SafeAreaInsets, epaint::MarginF32};
use egui_winit::winit::platform::android::activity::AndroidApp;
use jni::{Env, JValue, jni_sig, jni_str, objects::JObject};

static APP: OnceLock<AndroidApp> = OnceLock::new();

/// Keeps the app's handle for later JNI calls; call once at startup.
pub fn init(app: &AndroidApp) {
    let _ = APP.set(app.clone());
}

/// How often the insets are read again; they change on rotation, or when
/// the keyboard or a bar shows or hides.
const POLL: Duration = Duration::from_millis(200);

/// Follows the system bars' insets for egui.
#[derive(Default)]
pub struct Insets {
    last: Option<(Instant, [i32; 4])>,
}

impl Insets {
    /// Tells egui where the system bars are, in `raw`; call from
    /// `raw_input_hook`.
    pub fn hook(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput) {
        let stale = self.last.is_none_or(|(at, _)| at.elapsed() >= POLL);
        if stale && let Some(px) = read() {
            if self.last.is_none_or(|(_, last)| last != px) {
                log::info!("system bar insets (left, top, right, bottom): {px:?}");
            }
            self.last = Some((Instant::now(), px));
        }
        let Some((_, [left, top, right, bottom])) = self.last else {
            return;
        };
        let scale = ctx.pixels_per_point();
        raw.safe_area_insets = Some(SafeAreaInsets(MarginF32 {
            left: left as f32 / scale,
            right: right as f32 / scale,
            top: top as f32 / scale,
            bottom: bottom as f32 / scale,
        }));
    }
}

/// Sends the app to the background, as Android's back does with nowhere
/// left to go back to: it keeps running, and comes back as it was.
pub fn move_to_back() {
    let Some(app) = APP.get() else {
        return;
    };
    // SAFETY: as in `read`.
    let vm = unsafe { jni::JavaVM::from_raw(app.vm_as_ptr().cast()) };
    let result = vm.attach_current_thread(|env| -> jni::errors::Result<()> {
        // SAFETY: as in `read`.
        let activity = unsafe { JObject::from_raw(env, app.activity_as_ptr().cast()) };
        env.call_method(
            &activity,
            jni_str!("moveTaskToBack"),
            jni_sig!("(Z)Z"),
            &[JValue::Bool(true)],
        )?;
        Ok(())
    });
    if let Err(err) = result {
        log::warn!("couldn't move the app to the background: {err}");
    }
}

/// The system bars', display cutout's and keyboard's insets in pixels:
/// left, top, right, bottom.
fn read() -> Option<[i32; 4]> {
    let app = APP.get()?;
    // SAFETY: the pointer is the process's Java VM, valid for its lifetime.
    let vm = unsafe { jni::JavaVM::from_raw(app.vm_as_ptr().cast()) };
    let result = vm.attach_current_thread(|env| {
        env.with_local_frame(16, |env| -> jni::errors::Result<Option<[i32; 4]>> {
            // SAFETY: the activity is a global reference NativeActivity
            // keeps for as long as the app runs.
            let activity = unsafe { JObject::from_raw(env, app.activity_as_ptr().cast()) };
            insets(env, &activity)
        })
    });
    match result {
        Ok(insets) => insets,
        Err(err) => {
            log::warn!("couldn't read the system bars' insets: {err}");
            None
        }
    }
}

fn insets(env: &mut Env, activity: &JObject) -> jni::errors::Result<Option<[i32; 4]>> {
    let window = env
        .call_method(
            activity,
            jni_str!("getWindow"),
            jni_sig!("()Landroid/view/Window;"),
            &[],
        )?
        .l()?;
    let decor = env
        .call_method(
            &window,
            jni_str!("getDecorView"),
            jni_sig!("()Landroid/view/View;"),
            &[],
        )?
        .l()?;
    let root = env
        .call_method(
            &decor,
            jni_str!("getRootWindowInsets"),
            jni_sig!("()Landroid/view/WindowInsets;"),
            &[],
        )?
        .l()?;
    // Before the window is attached.
    if root.is_null() {
        return Ok(None);
    }
    let class = jni_str!("android/view/WindowInsets$Type");
    let mut types = 0;
    // The on-screen keyboard too, which pushes the app up rather than
    // covering what's being typed into.
    for name in [
        jni_str!("systemBars"),
        jni_str!("displayCutout"),
        jni_str!("ime"),
    ] {
        types |= env
            .call_static_method(class, name, jni_sig!("()I"), &[])?
            .i()?;
    }
    let insets = env
        .call_method(
            &root,
            jni_str!("getInsets"),
            jni_sig!("(I)Landroid/graphics/Insets;"),
            &[JValue::Int(types)],
        )?
        .l()?;
    let mut side =
        |name| -> jni::errors::Result<i32> { env.get_field(&insets, name, jni_sig!("I"))?.i() };
    Ok(Some([
        side(jni_str!("left"))?,
        side(jni_str!("top"))?,
        side(jni_str!("right"))?,
        side(jni_str!("bottom"))?,
    ]))
}
