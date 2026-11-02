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
    last: Option<(Instant, Read)>,
}

/// What [`read`] finds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Read {
    /// Left, top, right, bottom, in pixels.
    px: [i32; 4],
    /// The on-screen keyboard is up.
    keyboard: bool,
}

impl Insets {
    /// Tells egui where the system bars are, in `raw`; call from
    /// `raw_input_hook`.
    pub fn hook(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput) {
        let stale = self.last.is_none_or(|(at, _)| at.elapsed() >= POLL);
        if stale && let Some(read) = read() {
            if self.last.is_none_or(|(_, last)| last != read) {
                log::info!("system bar insets (left, top, right, bottom): {read:?}");
            }
            self.last = Some((Instant::now(), read));
        }
        let Some((
            _,
            Read {
                px: [left, top, right, bottom],
                ..
            },
        )) = self.last
        else {
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

    /// Whether the on-screen keyboard is up, as last read.
    pub fn keyboard(&self) -> bool {
        self.last.is_some_and(|(_, read)| read.keyboard)
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

/// The system bars', display cutout's and keyboard's insets.
fn read() -> Option<Read> {
    let app = APP.get()?;
    // SAFETY: the pointer is the process's Java VM, valid for its lifetime.
    let vm = unsafe { jni::JavaVM::from_raw(app.vm_as_ptr().cast()) };
    let result = vm.attach_current_thread(|env| {
        env.with_local_frame(16, |env| -> jni::errors::Result<Option<Read>> {
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

fn insets(env: &mut Env, activity: &JObject) -> jni::errors::Result<Option<Read>> {
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
    for name in [jni_str!("systemBars"), jni_str!("displayCutout")] {
        types |= env
            .call_static_method(class, name, jni_sig!("()I"), &[])?
            .i()?;
    }
    let ime = env
        .call_static_method(class, jni_str!("ime"), jni_sig!("()I"), &[])?
        .i()?;
    types |= ime;
    let keyboard = env
        .call_method(
            &root,
            jni_str!("isVisible"),
            jni_sig!("(I)Z"),
            &[JValue::Int(ime)],
        )?
        .z()?;
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
    let px = [
        side(jni_str!("left"))?,
        side(jni_str!("top"))?,
        side(jni_str!("right"))?,
        side(jni_str!("bottom"))?,
    ];
    Ok(Some(Read { px, keyboard }))
}

/// Runs `f` with the activity on this thread, attached to the Java VM; a
/// Java exception it ends in comes back as an error with its message.
fn with_activity<T>(
    what: &str,
    f: impl FnOnce(&mut Env, &JObject) -> jni::errors::Result<T>,
) -> Result<T, String> {
    let app = APP.get().ok_or("Android isn't set up yet")?;
    // SAFETY: as in `read`.
    let vm = unsafe { jni::JavaVM::from_raw(app.vm_as_ptr().cast()) };
    let result = vm.attach_current_thread(|env| {
        env.with_local_frame(32, |env| -> jni::errors::Result<T> {
            // SAFETY: as in `read`.
            let activity = unsafe { JObject::from_raw(env, app.activity_as_ptr().cast()) };
            let result = f(env, &activity);
            // Clears the exception, which would otherwise stop every JNI
            // call after it.
            if env.exception_check() {
                env.exception_catch()?;
            }
            result
        })
    });
    result.map_err(|err| {
        log::warn!("couldn't {what}: {err}");
        match err {
            jni::errors::Error::CaughtJavaException { name, msg, .. } => {
                match name.rsplit('.').next() {
                    Some("ActivityNotFoundException") => "No app opens this file".to_owned(),
                    _ if !msg.is_empty() => msg,
                    _ => name,
                }
            }
            err => err.to_string(),
        }
    })
}

/// The app's folder in shared storage for media (`Android/media/<package>`),
/// which other apps can be given files from, unlike the app's own storage.
pub fn media_dir() -> Option<std::path::PathBuf> {
    with_activity("find the app's media folder", |env, activity| {
        let dirs = env
            .call_method(
                activity,
                jni_str!("getExternalMediaDirs"),
                jni_sig!("()[Ljava/io/File;"),
                &[],
            )?
            .l()?;
        let dirs = env.cast_local::<jni::objects::JObjectArray>(dirs)?;
        if dirs.is_null() || dirs.len(env)? == 0 {
            return Ok(None);
        }
        let dir = dirs.get_element(env, 0)?;
        if dir.is_null() {
            return Ok(None);
        }
        let path = env
            .call_method(
                &dir,
                jni_str!("getAbsolutePath"),
                jni_sig!("()Ljava/lang/String;"),
                &[],
            )?
            .l()?;
        let path = env.cast_local::<jni::objects::JString>(path)?;
        Ok(Some(path.try_to_string(env)?.into()))
    })
    .ok()
    .flatten()
}

/// Opens `path` in another app: the default one for its type, or with
/// `choose`, one picked from the system's list (Android's "Open with").
///
/// Apps can't read the app's files by their path, so they're given the
/// file's `content://` URI from the media store (which indexes everything
/// in shared storage) with leave to read it.
pub fn open(path: &std::path::Path, choose: bool) -> std::io::Result<()> {
    let path = path
        .to_str()
        .ok_or_else(|| std::io::Error::other("the file's name isn't valid Unicode"))?;
    with_activity("open a file", |env, activity| {
        let Some((uri, mime)) = media_uri(env, activity, path)? else {
            return Ok(Err(
                "Android's media store doesn't know this file yet; try again in a moment"
                    .to_owned(),
            ));
        };
        let mime = match mime {
            Some(mime) => mime,
            None => {
                let mime = mime_from_extension(env, path)?;
                env.new_string(mime)?
            }
        };
        let action = env.new_string("android.intent.action.VIEW")?;
        let mut intent = env.new_object(
            jni_str!("android/content/Intent"),
            jni_sig!("(Ljava/lang/String;)V"),
            &[JValue::Object(&action)],
        )?;
        env.call_method(
            &intent,
            jni_str!("setDataAndType"),
            jni_sig!("(Landroid/net/Uri;Ljava/lang/String;)Landroid/content/Intent;"),
            &[JValue::Object(&uri), JValue::Object(&mime)],
        )?;
        // FLAG_GRANT_READ_URI_PERMISSION
        env.call_method(
            &intent,
            jni_str!("addFlags"),
            jni_sig!("(I)Landroid/content/Intent;"),
            &[JValue::Int(1)],
        )?;
        if choose {
            let title = env.new_string("Open with")?;
            intent = env
                .call_static_method(
                    jni_str!("android/content/Intent"),
                    jni_str!("createChooser"),
                    jni_sig!(
                        "(Landroid/content/Intent;Ljava/lang/CharSequence;)Landroid/content/Intent;"
                    ),
                    &[JValue::Object(&intent), JValue::Object(&title)],
                )?
                .l()?;
        }
        env.call_method(
            activity,
            jni_str!("startActivity"),
            jni_sig!("(Landroid/content/Intent;)V"),
            &[JValue::Object(&intent)],
        )?;
        Ok(Ok(()))
    })
    .and_then(|opened| opened)
    .map_err(std::io::Error::other)
}

/// The media store's URI for the file at `path`, and its type if the store
/// knows it. A file it hasn't indexed yet is scanned, and waited for a moment.
fn media_uri<'local>(
    env: &mut Env<'local>,
    activity: &JObject,
    path: &str,
) -> jni::errors::Result<Option<(JObject<'local>, Option<jni::objects::JString<'local>>)>> {
    let mut scanned = false;
    for _ in 0..20 {
        if let Some(found) = query_media(env, activity, path)? {
            return Ok(Some(found));
        }
        if !scanned {
            scanned = true;
            let string = jni_str!("java/lang/String");
            let paths = env.new_object_array(1, string, JObject::null())?;
            let jpath = env.new_string(path)?;
            paths.set_element(env, 0, &jpath)?;
            env.call_static_method(
                jni_str!("android/media/MediaScannerConnection"),
                jni_str!("scanFile"),
                jni_sig!(
                    "(Landroid/content/Context;[Ljava/lang/String;[Ljava/lang/String;Landroid/media/MediaScannerConnection$OnScanCompletedListener;)V"
                ),
                &[
                    JValue::Object(activity),
                    JValue::Object(&paths),
                    JValue::Object(&JObject::null()),
                    JValue::Object(&JObject::null()),
                ],
            )?;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(None)
}

/// Looks `path` up in the media store: its URI, in the collection for its
/// kind of media (which some apps expect), and its type.
fn query_media<'local>(
    env: &mut Env<'local>,
    activity: &JObject,
    path: &str,
) -> jni::errors::Result<Option<(JObject<'local>, Option<jni::objects::JString<'local>>)>> {
    let external = env.new_string("external")?;
    let files = env
        .call_static_method(
            jni_str!("android/provider/MediaStore$Files"),
            jni_str!("getContentUri"),
            jni_sig!("(Ljava/lang/String;)Landroid/net/Uri;"),
            &[JValue::Object(&external)],
        )?
        .l()?;
    let string = jni_str!("java/lang/String");
    let projection = env.new_object_array(3, string, JObject::null())?;
    for (i, column) in ["_id", "media_type", "mime_type"].into_iter().enumerate() {
        let column = env.new_string(column)?;
        projection.set_element(env, i, &column)?;
    }
    let selection = env.new_string("_data=?")?;
    let args = env.new_object_array(1, string, JObject::null())?;
    let jpath = env.new_string(path)?;
    args.set_element(env, 0, &jpath)?;
    let resolver = env
        .call_method(
            activity,
            jni_str!("getContentResolver"),
            jni_sig!("()Landroid/content/ContentResolver;"),
            &[],
        )?
        .l()?;
    let cursor = env
        .call_method(
            &resolver,
            jni_str!("query"),
            jni_sig!(
                "(Landroid/net/Uri;[Ljava/lang/String;Ljava/lang/String;[Ljava/lang/String;Ljava/lang/String;)Landroid/database/Cursor;"
            ),
            &[
                JValue::Object(&files),
                JValue::Object(&projection),
                JValue::Object(&selection),
                JValue::Object(&args),
                JValue::Object(&JObject::null()),
            ],
        )?
        .l()?;
    if cursor.is_null() {
        return Ok(None);
    }
    let found = (|| -> jni::errors::Result<_> {
        let first = env
            .call_method(&cursor, jni_str!("moveToFirst"), jni_sig!("()Z"), &[])?
            .z()?;
        if !first {
            return Ok(None);
        }
        let id = env
            .call_method(
                &cursor,
                jni_str!("getLong"),
                jni_sig!("(I)J"),
                &[JValue::Int(0)],
            )?
            .j()?;
        let media_type = env
            .call_method(
                &cursor,
                jni_str!("getInt"),
                jni_sig!("(I)I"),
                &[JValue::Int(1)],
            )?
            .i()?;
        let mime = env
            .call_method(
                &cursor,
                jni_str!("getString"),
                jni_sig!("(I)Ljava/lang/String;"),
                &[JValue::Int(2)],
            )?
            .l()?;
        let mime = env.cast_local::<jni::objects::JString>(mime)?;
        // MediaStore.Files.FileColumns.MEDIA_TYPE_IMAGE, _AUDIO, _VIDEO.
        let collection = match media_type {
            1 => Some(jni_str!("android/provider/MediaStore$Images$Media")),
            2 => Some(jni_str!("android/provider/MediaStore$Audio$Media")),
            3 => Some(jni_str!("android/provider/MediaStore$Video$Media")),
            _ => None,
        };
        let base = match collection {
            Some(class) => env
                .call_static_method(
                    class,
                    jni_str!("getContentUri"),
                    jni_sig!("(Ljava/lang/String;)Landroid/net/Uri;"),
                    &[JValue::Object(&external)],
                )?
                .l()?,
            None => files,
        };
        let uri = env
            .call_static_method(
                jni_str!("android/content/ContentUris"),
                jni_str!("withAppendedId"),
                jni_sig!("(Landroid/net/Uri;J)Landroid/net/Uri;"),
                &[JValue::Object(&base), JValue::Long(id)],
            )?
            .l()?;
        Ok(Some((uri, (!mime.is_null()).then_some(mime))))
    })();
    // Closed even when reading it failed, though not with an exception
    // pending, which `with_activity` clears.
    if !env.exception_check() {
        env.call_method(&cursor, jni_str!("close"), jni_sig!("()V"), &[])?;
    }
    found
}

/// The MIME type Android gives `path`'s extension, else any type.
fn mime_from_extension(env: &mut Env, path: &str) -> jni::errors::Result<String> {
    let Some(extension) = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
    else {
        return Ok("*/*".to_owned());
    };
    let map = env
        .call_static_method(
            jni_str!("android/webkit/MimeTypeMap"),
            jni_str!("getSingleton"),
            jni_sig!("()Landroid/webkit/MimeTypeMap;"),
            &[],
        )?
        .l()?;
    let extension = env.new_string(extension.to_lowercase())?;
    let mime = env
        .call_method(
            &map,
            jni_str!("getMimeTypeFromExtension"),
            jni_sig!("(Ljava/lang/String;)Ljava/lang/String;"),
            &[JValue::Object(&extension)],
        )?
        .l()?;
    if mime.is_null() {
        return Ok("*/*".to_owned());
    }
    env.cast_local::<jni::objects::JString>(mime)?
        .try_to_string(env)
}
