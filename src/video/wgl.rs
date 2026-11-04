//! A hidden window's OpenGL context for mpv to render into: Windows has no way
//! to make one without a window. Independent of the app's own renderer.

use std::ffi::{CString, c_char, c_void};

use windows::{
    Win32::{
        Foundation::{HMODULE, HWND},
        Graphics::{
            Gdi::{GetDC, HDC, ReleaseDC},
            OpenGL::{
                ChoosePixelFormat, HGLRC, PFD_DOUBLEBUFFER, PFD_DRAW_TO_WINDOW, PFD_MAIN_PLANE,
                PFD_SUPPORT_OPENGL, PFD_TYPE_RGBA, PIXELFORMATDESCRIPTOR, SetPixelFormat,
                wglCreateContext, wglDeleteContext, wglGetProcAddress, wglMakeCurrent,
            },
        },
        System::LibraryLoader::{GetProcAddress, LoadLibraryW},
        UI::WindowsAndMessaging::{CreateWindowExW, DestroyWindow, WINDOW_EX_STYLE, WS_POPUP},
    },
    core::{PCSTR, w},
};

const WGL_CONTEXT_MAJOR_VERSION: i32 = 0x2091;
const WGL_CONTEXT_MINOR_VERSION: i32 = 0x2092;
const WGL_CONTEXT_PROFILE_MASK: i32 = 0x9126;
const WGL_CONTEXT_CORE_PROFILE_BIT: i32 = 1;

type CreateContextAttribs = unsafe extern "system" fn(HDC, HGLRC, *const i32) -> HGLRC;

/// A current OpenGL context. Must stay on the thread that created it.
pub struct GlContext {
    window: HWND,
    dc: HDC,
    context: HGLRC,
    /// Where OpenGL's oldest functions are: `wglGetProcAddress` only has the
    /// newer ones.
    opengl32: HMODULE,
    pub gl: glow::Context,
}

/// A function of the current context, or null.
fn proc_address(opengl32: HMODULE, name: *const c_char) -> *mut c_void {
    let name = PCSTR(name.cast());
    // SAFETY: `name` is NUL-terminated, by whoever's asking.
    unsafe {
        match wglGetProcAddress(name) {
            // Some drivers answer "no" with these as well as null.
            Some(function) if !matches!(function as usize, 1 | 2 | 3 | usize::MAX) => {
                function as *mut c_void
            }
            _ => GetProcAddress(opengl32, name).map_or(std::ptr::null_mut(), |f| f as *mut c_void),
        }
    }
}

impl GlContext {
    pub fn new() -> Result<Self, String> {
        let error = |what: &str, err: windows::core::Error| format!("{what}: {err}");
        // SAFETY: a window of a built-in class, never shown, used only from
        // this thread, and destroyed along with everything else on any error.
        unsafe {
            let opengl32 =
                LoadLibraryW(w!("opengl32.dll")).map_err(|err| error("opengl32.dll", err))?;
            let window = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                w!("egui-files video"),
                WS_POPUP,
                0,
                0,
                16,
                16,
                None,
                None,
                None,
                None,
            )
            .map_err(|err| error("no window for OpenGL", err))?;
            let dc = GetDC(Some(window));
            match Self::create_context(dc) {
                Ok(context) => {
                    let gl = glow::Context::from_loader_function_cstr(|name| {
                        proc_address(opengl32, name.as_ptr()).cast_const()
                    });
                    Ok(Self {
                        window,
                        dc,
                        context,
                        opengl32,
                        gl,
                    })
                }
                Err(err) => {
                    ReleaseDC(Some(window), dc);
                    let _ = DestroyWindow(window);
                    Err(err)
                }
            }
        }
    }

    /// Makes a current context on `dc`: the newest kind if the driver has it.
    ///
    /// # Safety
    /// `dc` is a window's device context, with no pixel format set yet.
    unsafe fn create_context(dc: HDC) -> Result<HGLRC, String> {
        let format = PIXELFORMATDESCRIPTOR {
            nSize: size_of::<PIXELFORMATDESCRIPTOR>() as u16,
            nVersion: 1,
            dwFlags: PFD_DRAW_TO_WINDOW | PFD_SUPPORT_OPENGL | PFD_DOUBLEBUFFER,
            iPixelType: PFD_TYPE_RGBA,
            cColorBits: 32,
            iLayerType: PFD_MAIN_PLANE.0 as u8,
            ..Default::default()
        };
        // SAFETY: forwarded to the caller; a context made current here is
        // deleted if it's replaced.
        unsafe {
            let chosen = ChoosePixelFormat(dc, &format);
            if chosen == 0 {
                return Err("no OpenGL pixel format".into());
            }
            SetPixelFormat(dc, chosen, &format).map_err(|err| format!("pixel format: {err}"))?;
            // Functions for making a better context need a context first.
            let basic =
                wglCreateContext(dc).map_err(|err| format!("couldn't start OpenGL: {err}"))?;
            wglMakeCurrent(dc, basic).map_err(|err| format!("couldn't start OpenGL: {err}"))?;
            let name = CString::new("wglCreateContextAttribsARB").expect("no NUL in it");
            let Some(create) = wglGetProcAddress(PCSTR(name.as_ptr().cast())) else {
                return Ok(basic);
            };
            let create: CreateContextAttribs = std::mem::transmute(create);
            let attributes = [
                WGL_CONTEXT_MAJOR_VERSION,
                3,
                WGL_CONTEXT_MINOR_VERSION,
                3,
                WGL_CONTEXT_PROFILE_MASK,
                WGL_CONTEXT_CORE_PROFILE_BIT,
                0,
            ];
            let core = create(dc, HGLRC::default(), attributes.as_ptr());
            if core.is_invalid() {
                return Ok(basic);
            }
            wglMakeCurrent(dc, core).map_err(|err| format!("couldn't start OpenGL: {err}"))?;
            let _ = wglDeleteContext(basic);
            Ok(core)
        }
    }

    /// For mpv's `get_proc_address`; `ctx` must point to this context.
    pub unsafe extern "C" fn get_proc_address(
        ctx: *mut c_void,
        name: *const c_char,
    ) -> *mut c_void {
        // SAFETY: forwarded to the caller.
        proc_address(unsafe { (*(ctx as *const Self)).opengl32 }, name)
    }

    /// Only Linux has render nodes.
    pub fn render_node_fd(&self) -> Option<i32> {
        None
    }

    /// mpv works it out itself here.
    pub fn hwdec_interop(&self) -> Option<&'static str> {
        None
    }
}

impl Drop for GlContext {
    fn drop(&mut self) {
        // SAFETY: valid handles, not used afterwards.
        unsafe {
            let _ = wglMakeCurrent(HDC::default(), HGLRC::default());
            let _ = wglDeleteContext(self.context);
            ReleaseDC(Some(self.window), self.dc);
            let _ = DestroyWindow(self.window);
        }
    }
}
