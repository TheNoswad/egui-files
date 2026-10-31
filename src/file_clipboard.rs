//! Copied and cut files on the Windows clipboard, the way Explorer puts them
//! there: `CF_HDROP` (what Explorer and chat apps paste files from) plus
//! "Preferred DropEffect" (copy or move). The paths also go on as text, since
//! egui only reports Ctrl+V when the clipboard holds non-empty text.

use std::{mem::size_of, os::windows::ffi::OsStrExt, path::PathBuf};

use windows::{
    Win32::{
        Foundation::{GlobalFree, HANDLE},
        System::{
            DataExchange::{
                CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW,
                SetClipboardData,
            },
            Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock},
            Ole::{CF_HDROP, CF_UNICODETEXT, DROPEFFECT_COPY, DROPEFFECT_MOVE},
        },
        UI::Shell::{CFSTR_PREFERREDDROPEFFECT, DROPFILES},
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
    let text: Vec<u16> = crate::tab::join_paths(paths)
        .replace('\n', "\r\n")
        .encode_utf16()
        .chain([0])
        .collect();

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
        )?;
        put(
            CF_UNICODETEXT.0.into(),
            &text
                .iter()
                .flat_map(|c| c.to_le_bytes())
                .collect::<Vec<_>>(),
        )
    })();
    let _ = unsafe { CloseClipboard() };
    result
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
