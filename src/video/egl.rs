//! A headless OpenGL context for mpv to render into, on its own GPU device
//! rather than a window, so it's independent of the app's wgpu renderer.

use std::{
    ffi::{CStr, CString, c_char, c_void},
    fs::File,
    os::fd::AsRawFd,
};

use libloading::Library;

type EglDisplay = *mut c_void;
type EglContext = *mut c_void;
type EglDevice = *mut c_void;

const NONE: i32 = 0x3038;
const EXTENSIONS: i32 = 0x3055;
const OPENGL_API: u32 = 0x30A2;
const PLATFORM_DEVICE: u32 = 0x313F;
const PLATFORM_SURFACELESS: u32 = 0x31DD;
const DRM_RENDER_NODE_FILE: i32 = 0x3377;
const CONTEXT_MAJOR_VERSION: i32 = 0x3098;
const CONTEXT_MINOR_VERSION: i32 = 0x30FB;
const CONTEXT_OPENGL_PROFILE_MASK: i32 = 0x30FD;
const CONTEXT_OPENGL_CORE_PROFILE_BIT: i32 = 1;

struct Egl {
    _library: Library,
    get_proc_address: unsafe extern "C" fn(*const c_char) -> *mut c_void,
    get_platform_display: unsafe extern "C" fn(u32, *mut c_void, *const isize) -> EglDisplay,
    initialize: unsafe extern "C" fn(EglDisplay, *mut i32, *mut i32) -> u32,
    terminate: unsafe extern "C" fn(EglDisplay) -> u32,
    query_string: unsafe extern "C" fn(EglDisplay, i32) -> *const c_char,
    bind_api: unsafe extern "C" fn(u32) -> u32,
    create_context:
        unsafe extern "C" fn(EglDisplay, *mut c_void, EglContext, *const i32) -> EglContext,
    destroy_context: unsafe extern "C" fn(EglDisplay, EglContext) -> u32,
    make_current: unsafe extern "C" fn(EglDisplay, *mut c_void, *mut c_void, EglContext) -> u32,
}

impl Egl {
    fn load() -> Result<Self, String> {
        // SAFETY: libEGL has no constructors with preconditions; the
        // signatures below match egl.h.
        unsafe {
            let library = Library::new("libEGL.so.1").map_err(|e| format!("libEGL: {e}"))?;
            macro_rules! get {
                ($name:literal) => {
                    *library
                        .get(concat!($name, "\0").as_bytes())
                        .map_err(|e| e.to_string())?
                };
            }
            Ok(Self {
                get_proc_address: get!("eglGetProcAddress"),
                get_platform_display: get!("eglGetPlatformDisplay"),
                initialize: get!("eglInitialize"),
                terminate: get!("eglTerminate"),
                query_string: get!("eglQueryString"),
                bind_api: get!("eglBindAPI"),
                create_context: get!("eglCreateContext"),
                destroy_context: get!("eglDestroyContext"),
                make_current: get!("eglMakeCurrent"),
                _library: library,
            })
        }
    }

    fn proc_address(&self, name: &str) -> *mut c_void {
        let name = CString::new(name).expect("no nul in proc names");
        // SAFETY: valid C string.
        unsafe { (self.get_proc_address)(name.as_ptr()) }
    }

    fn has_extension(&self, display: EglDisplay, name: &str) -> bool {
        // SAFETY: EGL_EXTENSIONS on a valid or no display returns a static string or null.
        let extensions = unsafe { (self.query_string)(display, EXTENSIONS) };
        !extensions.is_null()
            && unsafe { CStr::from_ptr(extensions) }
                .to_string_lossy()
                .split(' ')
                .any(|e| e == name)
    }

    /// Picks the first GPU with a DRM render node, returning its display and
    /// the opened render node (which mpv needs for VA-API hardware decoding).
    fn device_display(&self) -> Option<(EglDisplay, RenderNode)> {
        type QueryDevices = unsafe extern "C" fn(i32, *mut EglDevice, *mut i32) -> u32;
        type QueryDeviceString = unsafe extern "C" fn(EglDevice, i32) -> *const c_char;
        let no_display = std::ptr::null_mut();
        if !self.has_extension(no_display, "EGL_EXT_platform_device") {
            return None;
        }
        let query_devices = self.proc_address("eglQueryDevicesEXT");
        let query_string = self.proc_address("eglQueryDeviceStringEXT");
        if query_devices.is_null() || query_string.is_null() {
            return None;
        }
        // SAFETY: non-null pointers to the EXT functions with these signatures.
        let (query_devices, query_string): (QueryDevices, QueryDeviceString) = unsafe {
            (
                std::mem::transmute::<*mut c_void, QueryDevices>(query_devices),
                std::mem::transmute::<*mut c_void, QueryDeviceString>(query_string),
            )
        };

        let mut devices = [std::ptr::null_mut(); 16];
        let mut count = 0;
        // SAFETY: buffer of the stated length.
        if unsafe { query_devices(devices.len() as i32, devices.as_mut_ptr(), &mut count) } == 0 {
            return None;
        }
        devices[..count.max(0) as usize].iter().find_map(|&device| {
            // SAFETY: device from eglQueryDevicesEXT.
            let node = unsafe { query_string(device, DRM_RENDER_NODE_FILE) };
            if node.is_null() {
                return None;
            }
            let node = unsafe { CStr::from_ptr(node) }.to_str().ok()?;
            let file = File::options().read(true).write(true).open(node).ok()?;
            let attribs = [NONE as isize];
            // SAFETY: valid device and attribute list.
            let display =
                unsafe { (self.get_platform_display)(PLATFORM_DEVICE, device, attribs.as_ptr()) };
            let node = RenderNode {
                file,
                path: node.into(),
            };
            (!display.is_null()).then_some((display, node))
        })
    }
}

pub struct RenderNode {
    file: File,
    path: std::path::PathBuf,
}

/// A current, surfaceless OpenGL context. Must stay on the thread that created it.
pub struct GlContext {
    egl: Egl,
    display: EglDisplay,
    context: EglContext,
    pub gl: glow::Context,
    /// The GPU's render node, if known; mpv uses it for VA-API.
    render_node: Option<RenderNode>,
}

impl GlContext {
    pub fn new() -> Result<Self, String> {
        let egl = Egl::load()?;
        let (display, render_node) = match egl.device_display() {
            Some((display, node)) => (display, Some(node)),
            None => {
                let attribs = [NONE as isize];
                // SAFETY: EGL_DEFAULT_DISPLAY with an empty attribute list.
                let display = unsafe {
                    (egl.get_platform_display)(
                        PLATFORM_SURFACELESS,
                        std::ptr::null_mut(),
                        attribs.as_ptr(),
                    )
                };
                (display, None)
            }
        };
        if display.is_null() {
            return Err("no EGL display".into());
        }
        // SAFETY: valid display; null version pointers are allowed.
        if unsafe { (egl.initialize)(display, std::ptr::null_mut(), std::ptr::null_mut()) } == 0 {
            return Err("eglInitialize failed".into());
        }
        let context = Self::create_context(&egl, display);
        if context.is_null() {
            unsafe { (egl.terminate)(display) };
            return Err("could not create an OpenGL context".into());
        }
        let no_surface = std::ptr::null_mut();
        // SAFETY: valid display and context; surfaceless is checked by create_context.
        if unsafe { (egl.make_current)(display, no_surface, no_surface, context) } == 0 {
            unsafe {
                (egl.destroy_context)(display, context);
                (egl.terminate)(display);
            }
            return Err("eglMakeCurrent failed".into());
        }
        // SAFETY: the context is current on this thread.
        let gl = unsafe {
            glow::Context::from_loader_function_cstr(|name| {
                (egl.get_proc_address)(name.as_ptr()).cast_const()
            })
        };
        Ok(Self {
            egl,
            display,
            context,
            gl,
            render_node,
        })
    }

    fn create_context(egl: &Egl, display: EglDisplay) -> EglContext {
        if !egl.has_extension(display, "EGL_KHR_surfaceless_context")
            || !egl.has_extension(display, "EGL_KHR_no_config_context")
        {
            return std::ptr::null_mut();
        }
        // SAFETY: valid enum.
        if unsafe { (egl.bind_api)(OPENGL_API) } == 0 {
            return std::ptr::null_mut();
        }
        let core = [
            CONTEXT_MAJOR_VERSION,
            3,
            CONTEXT_MINOR_VERSION,
            3,
            CONTEXT_OPENGL_PROFILE_MASK,
            CONTEXT_OPENGL_CORE_PROFILE_BIT,
            NONE,
        ];
        let any = [NONE];
        [core.as_slice(), any.as_slice()]
            .into_iter()
            .map(|attribs| {
                // SAFETY: valid display, EGL_NO_CONFIG, no share context, terminated list.
                unsafe {
                    (egl.create_context)(
                        display,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        attribs.as_ptr(),
                    )
                }
            })
            .find(|context| !context.is_null())
            .unwrap_or(std::ptr::null_mut())
    }

    /// For mpv's `get_proc_address`; `ctx` must point to this context.
    pub unsafe extern "C" fn get_proc_address(
        ctx: *mut c_void,
        name: *const c_char,
    ) -> *mut c_void {
        // SAFETY: forwarded to the caller.
        unsafe { ((*(ctx as *const Self)).egl.get_proc_address)(name) }
    }

    pub fn render_node_fd(&self) -> Option<i32> {
        self.render_node.as_ref().map(|node| node.file.as_raw_fd())
    }

    /// The hardware decoding interop for this GPU's kernel driver, if known.
    pub fn hwdec_interop(&self) -> Option<&'static str> {
        let node = self.render_node.as_ref()?.path.file_name()?.to_owned();
        let driver = std::fs::read_link(
            std::path::Path::new("/sys/class/drm")
                .join(node)
                .join("device/driver"),
        )
        .ok()?;
        match driver.file_name()?.to_str()? {
            // nvidia-vaapi-driver also provides VA-API on NVIDIA.
            "nvidia" => Some("cuda,vaapi"),
            "amdgpu" | "radeon" | "i915" | "xe" | "nouveau" => Some("vaapi"),
            _ => None,
        }
    }
}

impl Drop for GlContext {
    fn drop(&mut self) {
        let no_surface = std::ptr::null_mut();
        // SAFETY: valid handles, not used afterwards.
        unsafe {
            (self.egl.make_current)(self.display, no_surface, no_surface, std::ptr::null_mut());
            (self.egl.destroy_context)(self.display, self.context);
            (self.egl.terminate)(self.display);
        }
    }
}
