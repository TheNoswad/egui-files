//! Dragging files out to other apps on Windows (Explorer, Discord, ...).
//!
//! The shell builds the data object, so targets get everything an Explorer
//! drag offers (`CF_HDROP`, shell ID lists, file contents), and
//! `SHDoDragDrop` supplies the drop source and drag image.

use std::{os::windows::ffi::OsStrExt, path::PathBuf};

use windows::{
    Win32::{
        System::{
            Com::IDataObject,
            Ole::{DROPEFFECT_COPY, DROPEFFECT_LINK, DROPEFFECT_MOVE, IDropSource},
        },
        UI::Shell::{
            BHID_DataObject, Common::ITEMIDLIST, ILFree, IShellItemArray,
            SHCreateShellItemArrayFromIDLists, SHDoDragDrop, SHParseDisplayName,
        },
    },
    core::{PCWSTR, Result},
};

/// Runs the drag until the user drops or cancels. This blocks: OLE runs its own
/// message loop meanwhile, and the target decides between copy, move and link
/// (Explorer moves within a drive and copies across, as usual).
pub fn run(paths: &[PathBuf]) -> Result<()> {
    let mut pidls = Pidls(Vec::with_capacity(paths.len()));
    for path in paths {
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
        let mut pidl = std::ptr::null_mut();
        unsafe { SHParseDisplayName(PCWSTR(wide.as_ptr()), None, &mut pidl, 0, None) }?;
        pidls.0.push(pidl.cast_const());
    }
    unsafe {
        let items: IShellItemArray = SHCreateShellItemArrayFromIDLists(&pidls.0)?;
        let data: IDataObject = items.BindToHandler(None, &BHID_DataObject)?;
        SHDoDragDrop(
            None,
            &data,
            None::<&IDropSource>,
            DROPEFFECT_COPY | DROPEFFECT_MOVE | DROPEFFECT_LINK,
        )?;
    }
    Ok(())
}

/// Absolute item ID lists, freed on drop.
struct Pidls(Vec<*const ITEMIDLIST>);

impl Drop for Pidls {
    fn drop(&mut self) {
        for &pidl in &self.0 {
            unsafe { ILFree(Some(pidl)) };
        }
    }
}
