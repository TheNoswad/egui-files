//! Minimal bindings to libmpv's client and OpenGL render APIs, loaded at
//! runtime so the app still starts when mpv isn't installed.

// Only Linux has a video thread to use the render API and events yet (see
// "Video backend for Windows/macOS" in TODO.md); elsewhere only probing is.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::{
    ffi::{CStr, CString, c_char, c_int, c_ulong, c_void},
    path::Path,
    sync::OnceLock,
};

#[cfg(not(target_arch = "wasm32"))]
use libloading::Library;
#[cfg(target_arch = "wasm32")]
use web::Library;

#[cfg(target_os = "linux")]
const LIBRARY_NAMES: &[&str] = &["libmpv.so.2", "libmpv.so"];
#[cfg(target_os = "windows")]
const LIBRARY_NAMES: &[&str] = &["libmpv-2.dll", "mpv-2.dll"];
#[cfg(target_os = "macos")]
const LIBRARY_NAMES: &[&str] = &["libmpv.2.dylib", "libmpv.dylib"];
// Only found if bundled into the APK, which it isn't yet.
#[cfg(target_os = "android")]
const LIBRARY_NAMES: &[&str] = &["libmpv.so"];
#[cfg(target_arch = "wasm32")]
const LIBRARY_NAMES: &[&str] = &[];

#[cfg(not(target_arch = "wasm32"))]
const NOT_FOUND: &str = "libmpv not found (install mpv)";
#[cfg(target_arch = "wasm32")]
const NOT_FOUND: &str = "video playback needs the desktop app";

/// The browser can't load libraries, so there's never a libmpv.
#[cfg(target_arch = "wasm32")]
mod web {
    pub enum Library {}

    impl Library {
        pub unsafe fn new(_name: &str) -> Result<Self, ()> {
            Err(())
        }

        pub unsafe fn get<T>(&self, _symbol: &[u8]) -> Result<Box<T>, String> {
            match *self {}
        }
    }
}

pub const FORMAT_STRING: c_int = 1;
pub const FORMAT_FLAG: c_int = 3;
pub const FORMAT_INT64: c_int = 4;
pub const FORMAT_DOUBLE: c_int = 5;

const EVENT_NONE: c_int = 0;
const EVENT_SHUTDOWN: c_int = 1;
const EVENT_END_FILE: c_int = 7;
const EVENT_FILE_LOADED: c_int = 8;
const EVENT_VIDEO_RECONFIG: c_int = 17;
const EVENT_PROPERTY_CHANGE: c_int = 22;
const END_FILE_REASON_ERROR: c_int = 4;

pub const RENDER_PARAM_API_TYPE: c_int = 1;
pub const RENDER_PARAM_OPENGL_INIT_PARAMS: c_int = 2;
pub const RENDER_PARAM_OPENGL_FBO: c_int = 3;
pub const RENDER_PARAM_FLIP_Y: c_int = 4;
pub const RENDER_PARAM_BLOCK_FOR_TARGET_TIME: c_int = 12;
pub const RENDER_PARAM_DRM_DISPLAY_V2: c_int = 16;
pub const RENDER_UPDATE_FRAME: u64 = 1;

#[repr(C)]
struct RawEvent {
    event_id: c_int,
    error: c_int,
    reply_userdata: u64,
    data: *mut c_void,
}

#[repr(C)]
struct RawEventProperty {
    name: *const c_char,
    format: c_int,
    data: *mut c_void,
}

#[repr(C)]
struct RawEventEndFile {
    reason: c_int,
    error: c_int,
}

#[repr(C)]
pub struct RenderParam {
    pub kind: c_int,
    pub data: *mut c_void,
}

#[repr(C)]
pub struct OpenGlInitParams {
    pub get_proc_address: unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void,
    pub get_proc_address_ctx: *mut c_void,
}

#[repr(C)]
pub struct OpenGlFbo {
    pub fbo: c_int,
    pub w: c_int,
    pub h: c_int,
    pub internal_format: c_int,
}

#[repr(C)]
pub struct DrmParamsV2 {
    pub fd: c_int,
    pub crtc_id: c_int,
    pub connector_id: c_int,
    pub atomic_request_ptr: *mut *mut c_void,
    pub render_fd: c_int,
}

type Callback = unsafe extern "C" fn(*mut c_void);

macro_rules! api {
    ($($field:ident: fn($($arg:ty),*) $(-> $ret:ty)?;)*) => {
        struct Api {
            _library: Library,
            /// The file name it was loaded by.
            name: &'static str,
            $($field: unsafe extern "C" fn($($arg),*) $(-> $ret)?,)*
        }

        impl Api {
            fn load() -> Result<Self, String> {
                let (name, library) = LIBRARY_NAMES
                    .iter()
                    // SAFETY: libmpv has no library constructors with preconditions.
                    .find_map(|name| Some((*name, unsafe { Library::new(name) }.ok()?)))
                    .ok_or(NOT_FOUND)?;
                // SAFETY: the signatures match libmpv's client.h and render.h.
                unsafe {
                    Ok(Self {
                        $($field: *library
                            .get(concat!("mpv_", stringify!($field), "\0").as_bytes())
                            .map_err(|e| e.to_string())?,)*
                        _library: library,
                        name,
                    })
                }
            }
        }
    };
}

api! {
    client_api_version: fn() -> c_ulong;
    free: fn(*mut c_void);
    create: fn() -> *mut c_void;
    initialize: fn(*mut c_void) -> c_int;
    terminate_destroy: fn(*mut c_void);
    error_string: fn(c_int) -> *const c_char;
    set_option_string: fn(*mut c_void, *const c_char, *const c_char) -> c_int;
    set_property_string: fn(*mut c_void, *const c_char, *const c_char) -> c_int;
    command: fn(*mut c_void, *const *const c_char) -> c_int;
    observe_property: fn(*mut c_void, u64, *const c_char, c_int) -> c_int;
    get_property: fn(*mut c_void, *const c_char, c_int, *mut c_void) -> c_int;
    wait_event: fn(*mut c_void, f64) -> *mut RawEvent;
    set_wakeup_callback: fn(*mut c_void, Option<Callback>, *mut c_void);
    render_context_create: fn(*mut *mut c_void, *mut c_void, *mut RenderParam) -> c_int;
    render_context_set_update_callback: fn(*mut c_void, Option<Callback>, *mut c_void);
    render_context_update: fn(*mut c_void) -> u64;
    render_context_render: fn(*mut c_void, *mut RenderParam) -> c_int;
    render_context_report_swap: fn(*mut c_void);
    render_context_free: fn(*mut c_void);
}

fn api() -> Result<&'static Api, String> {
    static API: OnceLock<Result<Api, String>> = OnceLock::new();
    API.get_or_init(Api::load).as_ref().map_err(Clone::clone)
}

/// Whether libmpv can be loaded.
pub fn available() -> Result<(), String> {
    api().map(|_| ())
}

/// The library file loaded and its client API version, as "major.minor".
pub fn library() -> Result<(&'static str, String), String> {
    let api = api()?;
    // SAFETY: no preconditions.
    let version = unsafe { (api.client_api_version)() };
    Ok((api.name, format!("{}.{}", version >> 16, version & 0xffff)))
}

fn error_string(api: &Api, code: c_int) -> String {
    // SAFETY: mpv_error_string returns a static string for any code.
    unsafe { CStr::from_ptr((api.error_string)(code)) }
        .to_string_lossy()
        .into_owned()
}

fn check(api: &Api, code: c_int) -> Result<(), String> {
    if code < 0 {
        Err(error_string(api, code))
    } else {
        Ok(())
    }
}

fn cstring(s: &str) -> CString {
    CString::new(s.replace('\0', "")).expect("nul bytes removed")
}

pub enum Event {
    Property {
        id: u64,
        value: PropertyValue,
    },
    FileLoaded,
    /// The video output was (re)configured, e.g. for a new file.
    VideoReconfig,
    EndFile {
        error: Option<String>,
    },
    Shutdown,
}

pub enum PropertyValue {
    None,
    Flag(bool),
    Double(f64),
}

/// An mpv core. All methods are thread-safe on mpv's side.
pub struct Mpv {
    api: &'static Api,
    handle: *mut c_void,
}

// SAFETY: the mpv client API may be used from any thread.
unsafe impl Send for Mpv {}

impl Mpv {
    /// Creates an uninitialized core; set options, then call [`Self::initialize`].
    pub fn new() -> Result<Self, String> {
        let api = api()?;
        // SAFETY: no preconditions.
        let handle = unsafe { (api.create)() };
        if handle.is_null() {
            return Err("mpv_create failed".into());
        }
        Ok(Self { api, handle })
    }

    pub fn set_option(&self, name: &str, value: &str) -> Result<(), String> {
        let (name, value) = (cstring(name), cstring(value));
        // SAFETY: valid handle and C strings.
        check(self.api, unsafe {
            (self.api.set_option_string)(self.handle, name.as_ptr(), value.as_ptr())
        })
    }

    pub fn initialize(&self) -> Result<(), String> {
        // SAFETY: valid handle.
        check(self.api, unsafe { (self.api.initialize)(self.handle) })
    }

    pub fn set_property(&self, name: &str, value: &str) -> Result<(), String> {
        let (name, value) = (cstring(name), cstring(value));
        // SAFETY: valid handle and C strings.
        check(self.api, unsafe {
            (self.api.set_property_string)(self.handle, name.as_ptr(), value.as_ptr())
        })
    }

    pub fn command(&self, args: &[&str]) -> Result<(), String> {
        let args: Vec<CString> = args.iter().map(|a| cstring(a)).collect();
        let mut pointers: Vec<*const c_char> = args.iter().map(|a| a.as_ptr()).collect();
        pointers.push(std::ptr::null());
        // SAFETY: null-terminated array of valid C strings.
        check(self.api, unsafe {
            (self.api.command)(self.handle, pointers.as_ptr())
        })
    }

    pub fn load_file(&self, path: &Path) -> Result<(), String> {
        let on_phone = crate::phones::mount_path(path);
        let path = on_phone.as_deref().unwrap_or(path);
        // mpv takes paths as UTF-8; it would misread "-" or "foo://" as non-files.
        let path = path.to_string_lossy();
        self.command(&["loadfile", &path, "replace"])
    }

    /// An integer property's current value, if available.
    pub fn get_int(&self, name: &str) -> Option<i64> {
        let name = cstring(name);
        let mut value: i64 = 0;
        // SAFETY: valid handle and C string; INT64 writes an i64.
        let code = unsafe {
            (self.api.get_property)(
                self.handle,
                name.as_ptr(),
                FORMAT_INT64,
                (&raw mut value).cast(),
            )
        };
        (code >= 0).then_some(value)
    }

    /// A property's current value as a string, if available.
    pub fn get_string(&self, name: &str) -> Option<String> {
        let name = cstring(name);
        let mut value: *mut c_char = std::ptr::null_mut();
        // SAFETY: valid handle and C string; STRING writes a char* that we
        // own and free with mpv_free.
        let code = unsafe {
            (self.api.get_property)(
                self.handle,
                name.as_ptr(),
                FORMAT_STRING,
                (&raw mut value).cast(),
            )
        };
        if code < 0 || value.is_null() {
            return None;
        }
        // SAFETY: mpv returned a valid C string, freed once copied.
        unsafe {
            let string = CStr::from_ptr(value).to_string_lossy().into_owned();
            (self.api.free)(value.cast());
            Some(string)
        }
    }

    pub fn observe(&self, id: u64, name: &str, format: c_int) -> Result<(), String> {
        let name = cstring(name);
        // SAFETY: valid handle and C string.
        check(self.api, unsafe {
            (self.api.observe_property)(self.handle, id, name.as_ptr(), format)
        })
    }

    /// Calls `callback(data)` from an mpv thread whenever events are pending.
    ///
    /// # Safety
    /// `data` must stay valid until this core is dropped.
    pub unsafe fn set_wakeup_callback(&self, callback: Callback, data: *mut c_void) {
        // SAFETY: forwarded to the caller.
        unsafe { (self.api.set_wakeup_callback)(self.handle, Some(callback), data) }
    }

    /// Returns the next pending event without blocking.
    pub fn poll_event(&self) -> Option<Event> {
        loop {
            // SAFETY: valid handle; the event lives until the next call.
            let event = unsafe { &*(self.api.wait_event)(self.handle, 0.0) };
            let converted = match event.event_id {
                EVENT_NONE => return None,
                EVENT_SHUTDOWN => Event::Shutdown,
                EVENT_FILE_LOADED => Event::FileLoaded,
                EVENT_VIDEO_RECONFIG => Event::VideoReconfig,
                EVENT_END_FILE => {
                    // SAFETY: END_FILE events carry an mpv_event_end_file.
                    let end = unsafe { &*(event.data as *const RawEventEndFile) };
                    Event::EndFile {
                        error: (end.reason == END_FILE_REASON_ERROR)
                            .then(|| error_string(self.api, end.error)),
                    }
                }
                EVENT_PROPERTY_CHANGE => {
                    // SAFETY: PROPERTY_CHANGE events carry an mpv_event_property
                    // whose data matches its format.
                    let value = unsafe {
                        let property = &*(event.data as *const RawEventProperty);
                        match property.format {
                            FORMAT_FLAG => {
                                PropertyValue::Flag(*(property.data as *const c_int) != 0)
                            }
                            FORMAT_DOUBLE => PropertyValue::Double(*(property.data as *const f64)),
                            _ => PropertyValue::None,
                        }
                    };
                    Event::Property {
                        id: event.reply_userdata,
                        value,
                    }
                }
                _ => continue,
            };
            return Some(converted);
        }
    }
}

impl Drop for Mpv {
    fn drop(&mut self) {
        // SAFETY: valid handle, not used afterwards.
        unsafe { (self.api.terminate_destroy)(self.handle) }
    }
}

/// An OpenGL render context. Must be dropped before its [`Mpv`], with the GL
/// context it was created with current.
pub struct RenderContext {
    api: &'static Api,
    handle: *mut c_void,
}

// SAFETY: only used from the video thread, which owns the GL context.
unsafe impl Send for RenderContext {}

impl RenderContext {
    /// # Safety
    /// The GL context `params` refers to must be current, and `params` must be
    /// terminated by a param with kind 0.
    pub unsafe fn new(mpv: &Mpv, params: &mut [RenderParam]) -> Result<Self, String> {
        let mut handle = std::ptr::null_mut();
        // SAFETY: forwarded to the caller.
        check(mpv.api, unsafe {
            (mpv.api.render_context_create)(&mut handle, mpv.handle, params.as_mut_ptr())
        })?;
        Ok(Self {
            api: mpv.api,
            handle,
        })
    }

    /// Calls `callback(data)` from an mpv thread when a new frame is due.
    ///
    /// # Safety
    /// `data` must stay valid until this context is dropped.
    pub unsafe fn set_update_callback(&self, callback: Callback, data: *mut c_void) {
        // SAFETY: forwarded to the caller.
        unsafe { (self.api.render_context_set_update_callback)(self.handle, Some(callback), data) }
    }

    /// Returns `RENDER_UPDATE_*` flags; call after each update callback.
    pub fn update(&self) -> u64 {
        // SAFETY: valid handle.
        unsafe { (self.api.render_context_update)(self.handle) }
    }

    /// # Safety
    /// The GL context must be current, and `params` terminated by a kind-0 param.
    pub unsafe fn render(&self, params: &mut [RenderParam]) -> Result<(), String> {
        // SAFETY: forwarded to the caller.
        check(self.api, unsafe {
            (self.api.render_context_render)(self.handle, params.as_mut_ptr())
        })?;
        // SAFETY: valid handle.
        unsafe { (self.api.render_context_report_swap)(self.handle) };
        Ok(())
    }
}

impl Drop for RenderContext {
    fn drop(&mut self) {
        // SAFETY: valid handle, not used afterwards.
        unsafe { (self.api.render_context_free)(self.handle) }
    }
}
