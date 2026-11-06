//! What Windows' own frame did for a window that the in-app title bar has to
//! ask for: rounded corners and the shadow (from DWM), the snap layouts
//! flyout over maximize, and the window menu.
//!
//! Snap layouts show only for a window telling Windows the pointer is over
//! its maximize button (`HTMAXBUTTON` from `WM_NCHITTEST`), which winit
//! doesn't let us answer; each window is subclassed for that. The pointer
//! there is then outside the client area, so egui sees no hover or clicks
//! on maximize: the subclass tracks them, the button is drawn from
//! [`maximize_state`], and a click maximizes or restores the window itself.
//!
//! Where maximize is comes from [`update`], once a frame: the same in every
//! window, at a fixed distance from one side, as the bar lays them out.
//!
//! [`update`] also gives the windows the theme's background as their own,
//! with or without the in-app title bar. Windows fills what a window grows
//! by with that until the next frame shows there, and winit leaves it
//! unset: white strips along the edges as the window is resized.

use std::{
    collections::HashSet,
    sync::{Mutex, OnceLock},
};

use windows::Win32::{
    Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
    Graphics::{
        Dwm::{
            DWM_WINDOW_CORNER_PREFERENCE, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DEFAULT,
            DWMWCP_ROUND, DwmExtendFrameIntoClientArea, DwmSetWindowAttribute,
        },
        Gdi::{ClientToScreen, CreateSolidBrush, DeleteObject, HGDIOBJ, ScreenToClient},
    },
    System::Threading::GetCurrentThreadId,
    UI::{
        Controls::MARGINS,
        HiDpi::GetDpiForWindow,
        Input::KeyboardAndMouse::{TME_LEAVE, TME_NONCLIENT, TRACKMOUSEEVENT, TrackMouseEvent},
        Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
        WindowsAndMessaging::{
            EnableMenuItem, EnumThreadWindows, GCLP_HBRBACKGROUND, GetClassNameW, GetClientRect,
            GetCursorPos, GetForegroundWindow, GetSystemMenu, HTMAXBUTTON, IsZoomed, MF_BYCOMMAND,
            MF_ENABLED, MF_GRAYED, PostMessageW, SC_CLOSE, SC_MAXIMIZE, SC_MINIMIZE, SC_MOVE,
            SC_RESTORE, SC_SIZE, SetClassLongPtrW, TPM_RETURNCMD, TrackPopupMenu, WM_NCDESTROY,
            WM_NCHITTEST, WM_NCLBUTTONDBLCLK, WM_NCLBUTTONDOWN, WM_NCLBUTTONUP, WM_NCMOUSELEAVE,
            WM_NCMOUSEMOVE, WM_SYSCOMMAND,
        },
    },
};

use crate::window_buttons::Side;

/// The subclass's id among any others on the window.
const SUBCLASS_ID: usize = 0xe9f1;

/// Where maximize is, in points: from the window's `side`, `near` to `far`
/// across and `height` down.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Maximize {
    pub side: Side,
    pub near: f32,
    pub far: f32,
    pub height: f32,
}

#[derive(Default)]
struct State {
    /// The windows styled and subclassed, as `HWND` values.
    styled: HashSet<isize>,
    maximize: Option<Maximize>,
    /// egui's zoom, on top of the window's DPI scale.
    zoom: f32,
    /// The window the pointer is over maximize in, and whether pressed.
    hovered: Option<isize>,
    pressed: bool,
    /// The windows' background brush, as an `HBRUSH`, and its colour.
    background: Option<(isize, egui::Color32)>,
}

fn state() -> &'static Mutex<State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE.get_or_init(Default::default)
}

fn context() -> &'static OnceLock<egui::Context> {
    static CONTEXT: OnceLock<egui::Context> = OnceLock::new();
    &CONTEXT
}

/// Styles and subclasses the app's windows while `enabled` (the in-app
/// title bar is on), and puts them back when it's not. Call it once a
/// frame with where maximize is now, `None` when it isn't shown.
pub fn update(ctx: &egui::Context, enabled: bool, maximize: Option<Maximize>) {
    let _ = context().set(ctx.clone());
    let windows = app_windows();
    background(&windows, ctx.global_style().visuals.panel_fill);
    let changes: Vec<HWND> = {
        let mut state = state().lock().unwrap();
        state.maximize = maximize.filter(|_| enabled);
        state.zoom = ctx.zoom_factor();
        let changes = windows
            .into_iter()
            .filter(|hwnd| {
                let key = hwnd.0 as isize;
                if enabled {
                    state.styled.insert(key)
                } else {
                    state.styled.remove(&key)
                }
            })
            .collect();
        if !enabled {
            // Windows closed since aren't in the list; forget them too.
            state.styled.clear();
            state.hovered = None;
        }
        changes
    };
    // Outside the lock: these can send the window messages, which the
    // subclass handles with it.
    for hwnd in changes {
        style(hwnd, enabled);
    }
}

/// Gives the windows a background of `color`, made when it changes. winit's
/// windows share one class, so setting it on any sets it for all of them,
/// and for the ones opened later.
fn background(windows: &[HWND], color: egui::Color32) {
    let Some(&hwnd) = windows.first() else {
        return;
    };
    let mut state = state().lock().unwrap();
    if state.background.is_some_and(|(_, was)| was == color) {
        return;
    }
    let [r, g, b, _] = color.to_array();
    // SAFETY: a brush of our own, set on our own windows' class; the one
    // it replaces is ours too, and no longer the class's once replaced.
    unsafe {
        let brush = CreateSolidBrush(COLORREF(u32::from_le_bytes([r, g, b, 0])));
        if brush.is_invalid() {
            return;
        }
        SetClassLongPtrW(hwnd, GCLP_HBRBACKGROUND, brush.0 as isize);
        if let Some((old, _)) = state.background.replace((brush.0 as isize, color)) {
            let _ = DeleteObject(HGDIOBJ(old as *mut _));
        }
    }
}

/// Whether the pointer is over maximize in the window `ctx` is drawing,
/// and whether it's pressed there.
pub fn maximize_state(ctx: &egui::Context) -> (bool, bool) {
    let (hovered, pressed) = {
        let state = state().lock().unwrap();
        (state.hovered, state.pressed)
    };
    let Some(hwnd) = hovered else {
        return (false, false);
    };
    let Some(inner) = ctx.input(|i| i.viewport().inner_rect) else {
        return (false, false);
    };
    // The window drawn is the one whose client area is where egui says.
    let hwnd = HWND(hwnd as *mut _);
    let mut origin = POINT::default();
    // SAFETY: a plain query on a window handle; a stale one just fails.
    let scale = unsafe {
        let _ = ClientToScreen(hwnd, &mut origin);
        scale(hwnd)
    };
    let at = egui::pos2(origin.x as f32 / scale, origin.y as f32 / scale);
    let this = (at - inner.min).length() < 2.0;
    (this, this && pressed)
}

/// Shows Windows' own window menu at the pointer, for the focused window
/// (the one just right-clicked), and does what's chosen. Returns whether
/// there was one to show.
pub fn show_system_menu() -> bool {
    // SAFETY: Win32 calls on our own foreground window; each failure is
    // checked or harmless.
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_invalid() {
            return false;
        }
        let menu = GetSystemMenu(hwnd, false);
        if menu.is_invalid() {
            return false;
        }
        let zoomed = IsZoomed(hwnd).as_bool();
        let enable = |command: u32, on: bool| {
            let flag = if on { MF_ENABLED } else { MF_GRAYED };
            let _ = EnableMenuItem(menu, command, MF_BYCOMMAND | flag);
        };
        enable(SC_RESTORE, zoomed);
        enable(SC_MAXIMIZE, !zoomed);
        enable(SC_MOVE, !zoomed);
        enable(SC_SIZE, !zoomed);
        enable(SC_MINIMIZE, true);
        enable(SC_CLOSE, true);
        let mut at = POINT::default();
        if GetCursorPos(&mut at).is_err() {
            return false;
        }
        let command = TrackPopupMenu(menu, TPM_RETURNCMD, at.x, at.y, None, hwnd, None).0;
        if command != 0 {
            let _ = PostMessageW(
                Some(hwnd),
                WM_SYSCOMMAND,
                WPARAM(command as usize),
                LPARAM(0),
            );
        }
        true
    }
}

/// The app's top-level windows: winit's, on this (the UI) thread.
fn app_windows() -> Vec<HWND> {
    unsafe extern "system" fn collect(hwnd: HWND, list: LPARAM) -> windows::core::BOOL {
        let mut name = [0u16; 32];
        // SAFETY: `list` is the `Vec` passed below, alive for the call.
        unsafe {
            let len = GetClassNameW(hwnd, &mut name) as usize;
            if String::from_utf16_lossy(&name[..len]) == "Window Class" {
                (*(list.0 as *mut Vec<HWND>)).push(hwnd);
            }
        }
        true.into()
    }
    let mut list: Vec<HWND> = Vec::new();
    // SAFETY: `collect` only runs during the call, while `list` is alive.
    unsafe {
        let _ = EnumThreadWindows(
            GetCurrentThreadId(),
            Some(collect),
            LPARAM(&mut list as *mut Vec<HWND> as isize),
        );
    }
    list
}

/// Rounded corners and the shadow, and the subclass for snap layouts; or
/// back to how winit left the window.
fn style(hwnd: HWND, on: bool) {
    let corners: DWM_WINDOW_CORNER_PREFERENCE = if on { DWMWCP_ROUND } else { DWMWCP_DEFAULT };
    // A frame of a pixel into the client area is what gives an undecorated
    // window its shadow back, as winit's own undecorated shadow does.
    let inset = if on { 1 } else { 0 };
    let margins = MARGINS {
        cxLeftWidth: inset,
        cxRightWidth: inset,
        cyTopHeight: inset,
        cyBottomHeight: inset,
    };
    // SAFETY: our own window; the attribute is the size it says. Before
    // Windows 11 the corner preference fails, which is fine.
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &corners as *const _ as *const _,
            size_of::<DWM_WINDOW_CORNER_PREFERENCE>() as u32,
        );
        let _ = DwmExtendFrameIntoClientArea(hwnd, &margins);
        if on {
            let _ = SetWindowSubclass(hwnd, Some(subclass), SUBCLASS_ID, 0);
        } else {
            let _ = RemoveWindowSubclass(hwnd, Some(subclass), SUBCLASS_ID);
        }
    }
}

/// Physical pixels per point in `hwnd`: its DPI scale times egui's zoom.
unsafe fn scale(hwnd: HWND) -> f32 {
    let zoom = state().lock().unwrap().zoom.max(0.1);
    // SAFETY: a query on a window handle.
    let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);
    dpi as f32 / 96.0 * zoom
}

/// Whether the screen point in `lparam` is over maximize in `hwnd`.
unsafe fn over_maximize(hwnd: HWND, lparam: LPARAM) -> bool {
    let Some(maximize) = state().lock().unwrap().maximize else {
        return false;
    };
    let mut point = POINT {
        x: (lparam.0 & 0xffff) as i16 as i32,
        y: ((lparam.0 >> 16) & 0xffff) as i16 as i32,
    };
    let mut client = RECT::default();
    // SAFETY: queries on the window the message is for.
    unsafe {
        if !ScreenToClient(hwnd, &mut point).as_bool() || GetClientRect(hwnd, &mut client).is_err()
        {
            return false;
        }
        let scale = scale(hwnd);
        let (x, y) = (point.x as f32 / scale, point.y as f32 / scale);
        let width = client.right as f32 / scale;
        let from_side = match maximize.side {
            Side::Left => x,
            Side::Right => width - x,
        };
        (0.0..maximize.height).contains(&y) && (maximize.near..maximize.far).contains(&from_side)
    }
}

fn repaint() {
    if let Some(ctx) = context().get() {
        ctx.request_repaint();
    }
}

unsafe extern "system" fn subclass(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    let key = hwnd.0 as isize;
    let on_maximize = wparam.0 as u32 == HTMAXBUTTON;
    // SAFETY: the subclass proc's contract: unhandled messages go on to
    // `DefSubclassProc` with what came in.
    unsafe {
        match message {
            WM_NCHITTEST if over_maximize(hwnd, lparam) => {
                return LRESULT(HTMAXBUTTON as isize);
            }
            WM_NCMOUSEMOVE if on_maximize => {
                let mut state = state().lock().unwrap();
                if state.hovered != Some(key) {
                    state.hovered = Some(key);
                    drop(state);
                    // For WM_NCMOUSELEAVE when the pointer moves off.
                    let mut track = TRACKMOUSEEVENT {
                        cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE | TME_NONCLIENT,
                        hwndTrack: hwnd,
                        dwHoverTime: 0,
                    };
                    let _ = TrackMouseEvent(&mut track);
                    repaint();
                }
                return LRESULT(0);
            }
            WM_NCMOUSEMOVE | WM_NCMOUSELEAVE => {
                let mut state = state().lock().unwrap();
                if state.hovered == Some(key) {
                    state.hovered = None;
                    state.pressed = false;
                    repaint();
                }
            }
            WM_NCLBUTTONDOWN | WM_NCLBUTTONDBLCLK if on_maximize => {
                state().lock().unwrap().pressed = true;
                repaint();
                return LRESULT(0);
            }
            WM_NCLBUTTONUP if on_maximize => {
                let was_pressed = std::mem::take(&mut state().lock().unwrap().pressed);
                if was_pressed {
                    let command = if IsZoomed(hwnd).as_bool() {
                        SC_RESTORE
                    } else {
                        SC_MAXIMIZE
                    };
                    let _ = PostMessageW(
                        Some(hwnd),
                        WM_SYSCOMMAND,
                        WPARAM(command as usize),
                        LPARAM(0),
                    );
                }
                repaint();
                return LRESULT(0);
            }
            WM_NCDESTROY => {
                state().lock().unwrap().styled.remove(&key);
                let _ = RemoveWindowSubclass(hwnd, Some(subclass), SUBCLASS_ID);
            }
            _ => {}
        }
        DefSubclassProc(hwnd, message, wparam, lparam)
    }
}
