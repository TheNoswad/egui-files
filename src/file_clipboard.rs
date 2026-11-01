//! Copied and cut files on the Windows clipboard, the way Explorer puts them
//! there: `CF_HDROP` (what Explorer and chat apps paste files from) plus
//! "Preferred DropEffect" (copy or move). No text, like Explorer: chat apps such
//! as Discord paste text in preference to files whenever there is any.
//!
//! [`files_if_changed`] reads files copied or cut elsewhere (e.g. Explorer)
//! back, so they paste here too.
//!
//! egui-winit (0.36 and 0.37) drops Ctrl+V entirely when the clipboard has no
//! text (or image), so [`PasteKey`] notices the key press itself.

use std::{
    ffi::OsString,
    mem::size_of,
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::PathBuf,
};

use windows::{
    Win32::{
        Foundation::{GlobalFree, HANDLE, HGLOBAL},
        System::{
            DataExchange::{
                CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber,
                IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW,
                SetClipboardData,
            },
            Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock},
            Ole::{CF_HDROP, DROPEFFECT_COPY, DROPEFFECT_MOVE},
        },
        UI::{
            Input::KeyboardAndMouse::{GetKeyState, VK_CONTROL, VK_MENU, VK_V},
            Shell::{CFSTR_PREFERREDDROPEFFECT, DROPFILES, DragQueryFileW, HDROP},
        },
    },
    core::Result,
};

pub fn set(paths: &[PathBuf], cut: bool) -> Result<()> {
    // DROPFILES, then each path NUL-terminated, then an extra NUL.
    let mut files: Vec<u16> = Vec::new();
    for path in paths {
        files.extend(path.as_os_str().encode_wide());
        files.push(0);
    }
    files.push(0);
    let header = DROPFILES {
        pFiles: size_of::<DROPFILES>() as u32,
        fWide: true.into(),
        ..Default::default()
    };
    let mut hdrop = bytes_of(&header).to_vec();
    hdrop.extend(files.iter().flat_map(|c| c.to_le_bytes()));

    let effect = if cut {
        DROPEFFECT_MOVE
    } else {
        DROPEFFECT_COPY
    };

    // Another app may hold the clipboard open for a moment.
    let mut tries = 0;
    while unsafe { OpenClipboard(None) }.is_err() {
        tries += 1;
        if tries == 10 {
            return Err(windows::core::Error::from_thread());
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let result = (|| unsafe {
        EmptyClipboard()?;
        put(CF_HDROP.0.into(), &hdrop)?;
        put(
            RegisterClipboardFormatW(CFSTR_PREFERREDDROPEFFECT),
            &effect.0.to_le_bytes(),
        )
    })();
    let _ = unsafe { CloseClipboard() };
    result
}

/// The files on the clipboard and whether they were cut, when the clipboard
/// changed since `seq` (a `GetClipboardSequenceNumber`) and holds files.
/// Start `seq` at 0 to pick up what was copied before the app started.
pub fn files_if_changed(seq: &mut u32) -> Option<(Vec<PathBuf>, bool)> {
    let now = unsafe { GetClipboardSequenceNumber() };
    if now == *seq {
        return None;
    }
    if unsafe { IsClipboardFormatAvailable(CF_HDROP.0.into()) }.is_err() {
        *seq = now;
        return None;
    }
    // Busy (another app has it open): leave `seq` alone and try next frame.
    unsafe { OpenClipboard(None) }.ok()?;
    let files = unsafe { read_files() };
    let _ = unsafe { CloseClipboard() };
    *seq = now;
    files
}

/// Reads `CF_HDROP` and "Preferred DropEffect" from the open clipboard.
unsafe fn read_files() -> Option<(Vec<PathBuf>, bool)> {
    unsafe {
        let hdrop = HDROP(GetClipboardData(CF_HDROP.0.into()).ok()?.0);
        let count = DragQueryFileW(hdrop, u32::MAX, None);
        let mut paths = Vec::with_capacity(count as usize);
        for i in 0..count {
            let len = DragQueryFileW(hdrop, i, None) as usize;
            let mut buf = vec![0u16; len + 1];
            let len = DragQueryFileW(hdrop, i, Some(&mut buf)) as usize;
            paths.push(OsString::from_wide(&buf[..len]).into());
        }
        // Explorer: copy is COPY | LINK, cut is MOVE alone.
        let mut cut = false;
        let format = RegisterClipboardFormatW(CFSTR_PREFERREDDROPEFFECT);
        if let Ok(handle) = GetClipboardData(format) {
            let memory = HGLOBAL(handle.0);
            let ptr = GlobalLock(memory).cast::<u32>();
            if !ptr.is_null() {
                let effect = ptr.read_unaligned();
                let _ = GlobalUnlock(memory);
                cut = effect & DROPEFFECT_MOVE.0 != 0 && effect & DROPEFFECT_COPY.0 == 0;
            }
        }
        (!paths.is_empty()).then_some((paths, cut))
    }
}

/// Copies `data` into global memory and hands it to the open clipboard.
unsafe fn put(format: u32, data: &[u8]) -> Result<()> {
    unsafe {
        let memory = GlobalAlloc(GMEM_MOVEABLE, data.len())?;
        let ptr = GlobalLock(memory);
        if ptr.is_null() {
            let _ = GlobalFree(Some(memory));
            return Err(windows::core::Error::from_thread());
        }
        std::ptr::copy_nonoverlapping(data.as_ptr(), ptr.cast(), data.len());
        let _ = GlobalUnlock(memory);
        // On success the clipboard owns the memory.
        if let Err(err) = SetClipboardData(format, Some(HANDLE(memory.0))) {
            let _ = GlobalFree(Some(memory));
            return Err(err);
        }
        Ok(())
    }
}

fn bytes_of(header: &DROPFILES) -> &[u8] {
    // SAFETY: DROPFILES is plain old data with no padding.
    unsafe {
        std::slice::from_raw_parts((header as *const DROPFILES).cast(), size_of::<DROPFILES>())
    }
}

/// Reports Ctrl+V as [`egui::Event::Paste`] when egui-winit didn't, because the
/// clipboard had no text (our own copied files, or Explorer's).
///
/// Workaround for egui-winit swallowing the key press; see TODO.md.
#[derive(Default)]
pub struct PasteKey {
    v_down: bool,
}

impl PasteKey {
    pub fn poll(&mut self, ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        let down = |key: i32| unsafe { GetKeyState(key) } < 0;
        let v_down = down(VK_V.0.into());
        // A press and release can both land between two frames; egui still
        // gets the release (only the press is swallowed).
        let released = raw_input.events.iter().any(|e| {
            matches!(
                e,
                egui::Event::Key {
                    key: egui::Key::V,
                    pressed: false,
                    ..
                }
            )
        });
        let pressed = (v_down || released) && !self.v_down;
        self.v_down = v_down;
        if !pressed
            || !raw_input.focused
            || !down(VK_CONTROL.0.into())
            // Ctrl+Alt is AltGr on many layouts.
            || down(VK_MENU.0.into())
            || raw_input
                .events
                .iter()
                .any(|e| matches!(e, egui::Event::Paste(_)))
            // A text field or terminal would treat an empty paste as input
            // (replacing a selection, sending bracketed-paste markers).
            || ctx.memory(|m| m.focused()).is_some()
        {
            return;
        }
        raw_input.events.push(egui::Event::Paste(String::new()));
    }
}
