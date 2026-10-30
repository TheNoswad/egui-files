//! Apps from the shell's file associations (`SHAssocEnumHandlers`), started
//! through the shell so packaged apps work too.

use std::{
    io,
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
};

use windows::{
    Win32::{
        Graphics::Gdi::{
            BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, DeleteObject, GetDC,
            GetDIBits, GetObjectW, HGDIOBJ, ReleaseDC,
        },
        System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoTaskMemFree, IDataObject},
        UI::{
            Shell::{
                ASSOC_FILTER, ASSOC_FILTER_NONE, ASSOC_FILTER_RECOMMENDED,
                ASSOCF_INIT_IGNOREUNKNOWN, ASSOCSTR_EXECUTABLE, AssocQueryStringW, BHID_DataObject,
                Common::ITEMIDLIST, ExtractIconExW, IAssocHandler, ILFree, SHAssocEnumHandlers,
                SHCreateShellItemArrayFromIDLists, SHParseDisplayName,
            },
            WindowsAndMessaging::{DestroyIcon, GetIconInfo, HICON, ICONINFO},
        },
    },
    core::{HSTRING, PWSTR},
};

use super::{App, Handlers, Launch, Subject, Target};

/// The shell is asked every time; there's nothing to load.
pub struct Catalog;

/// The extension the shell keys associations by, e.g. `.txt`.
fn extension(subject: &Subject) -> Option<String> {
    if subject.on_disk && subject.path.is_dir() {
        return None;
    }
    let ext = subject.path.extension()?.to_str()?;
    Some(format!(".{}", ext.to_lowercase()))
}

fn take_string(s: PWSTR) -> String {
    if s.is_null() {
        return String::new();
    }
    // SAFETY: the shell returned a valid, NUL-terminated string that we own.
    unsafe {
        let text = s.to_string().unwrap_or_default();
        CoTaskMemFree(Some(s.0 as _));
        text
    }
}

fn init_com() {
    // Already initialized on the UI thread (by winit); harmless if so.
    // SAFETY: plain COM initialization.
    let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
}

/// The shell's handlers for an extension.
fn enum_handlers(ext: &str, filter: ASSOC_FILTER) -> Vec<IAssocHandler> {
    init_com();
    let mut out = Vec::new();
    // SAFETY: COM calls on initialized interfaces.
    unsafe {
        let Ok(handlers) = SHAssocEnumHandlers(&HSTRING::from(ext), filter) else {
            return out;
        };
        loop {
            let mut slot = [None];
            let mut fetched = 0;
            if handlers.Next(&mut slot, Some(&mut fetched)).is_err() || fetched == 0 {
                break;
            }
            out.extend(slot[0].take());
        }
    }
    out
}

fn to_app(handler: &IAssocHandler) -> Option<App> {
    // SAFETY: COM calls on a valid handler; returned strings are freed.
    unsafe {
        let id = take_string(handler.GetName().ok()?);
        let name = take_string(handler.GetUIName().ok()?);
        let mut path = PWSTR::null();
        let mut index = 0;
        let icon = handler
            .GetIconLocation(&mut path, &mut index)
            .ok()
            .map(|()| take_string(path))
            .filter(|p| !p.is_empty())
            .map(|p| format!("{p},{index}"));
        Some(App {
            name: if name.is_empty() { id.clone() } else { name },
            id,
            icon,
            ..App::default()
        })
    }
}

/// The program the shell opens an extension with by default.
fn default_executable(ext: &str) -> Option<String> {
    let mut buf = vec![0u16; 1024];
    let mut len = buf.len() as u32;
    // SAFETY: `buf` holds `len` characters.
    unsafe {
        AssocQueryStringW(
            ASSOCF_INIT_IGNOREUNKNOWN,
            ASSOCSTR_EXECUTABLE,
            &HSTRING::from(ext),
            None,
            Some(PWSTR(buf.as_mut_ptr())),
            &mut len,
        )
        .ok()
        .ok()?;
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..end]))
}

/// Apps for every extension in `subjects`, in the first one's order.
fn common_apps(subjects: &[Subject], filter: ASSOC_FILTER) -> (Vec<String>, Vec<App>) {
    let mut exts: Vec<String> = Vec::new();
    for subject in subjects {
        match extension(subject) {
            Some(ext) if !exts.contains(&ext) => exts.push(ext),
            Some(_) => {}
            None => return (exts, Vec::new()),
        }
    }
    let mut common: Option<Vec<App>> = None;
    for ext in &exts {
        let apps: Vec<App> = enum_handlers(ext, filter)
            .iter()
            .filter_map(to_app)
            .collect();
        common = Some(match common {
            None => apps,
            Some(mut common) => {
                common.retain(|a| apps.iter().any(|b| b.id == a.id));
                common
            }
        });
    }
    (exts, common.unwrap_or_default())
}

impl Catalog {
    pub fn load() -> Self {
        Self
    }

    pub fn is_stale(&self) -> bool {
        false
    }

    pub fn handlers(&self, subjects: &[Subject]) -> Handlers {
        let (exts, mut apps) = common_apps(subjects, ASSOC_FILTER_RECOMMENDED);
        let default_exe: Vec<Option<String>> = exts.iter().map(|e| default_executable(e)).collect();
        let same_default = default_exe.windows(2).all(|w| w[0] == w[1]);
        let default = default_exe
            .first()
            .cloned()
            .flatten()
            .filter(|_| same_default)
            .and_then(|exe| apps.iter().position(|a| a.id.eq_ignore_ascii_case(&exe)))
            // The shell lists the default first.
            .or((exts.len() == 1 && !apps.is_empty()).then_some(0))
            .map(|i| apps.remove(i));
        Handlers {
            default,
            others: apps,
            types: exts,
        }
    }

    /// Every app registered for the files' extensions; Windows has no list
    /// of apps that can open anything.
    pub fn all_apps(&self, subjects: &[Subject]) -> Vec<App> {
        let mut apps = common_apps(subjects, ASSOC_FILTER_NONE).1;
        apps.sort_by_cached_key(|a| a.name.to_lowercase());
        apps
    }

    pub fn launch(&self, target: &Target, paths: &[PathBuf]) -> io::Result<Vec<Launch>> {
        let app = match target {
            Target::App(app) => app,
            Target::Command { .. } => return super::launch_command(target, paths),
        };
        let ext = paths
            .first()
            .and_then(|p| {
                extension(&Subject {
                    path: p.clone(),
                    on_disk: true,
                })
            })
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no file extension"))?;
        let handler = enum_handlers(&ext, ASSOC_FILTER_NONE)
            .into_iter()
            .find(|h| to_app(h).is_some_and(|a| a.id == app.id))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("{} is no longer available", app.name),
                )
            })?;
        let data = data_object(paths).map_err(io::Error::other)?;
        // SAFETY: COM call with a valid data object.
        unsafe { handler.Invoke(&data) }.map_err(io::Error::other)?;
        Ok(Vec::new())
    }
}

/// The shell's data object for files, which handlers are invoked with.
fn data_object(paths: &[PathBuf]) -> windows::core::Result<IDataObject> {
    init_com();
    let mut pidls: Vec<*mut ITEMIDLIST> = Vec::new();
    // SAFETY: each PIDL is allocated by the shell and freed below.
    let result = unsafe {
        let mut result = Ok(());
        for path in paths {
            let mut pidl = std::ptr::null_mut();
            match SHParseDisplayName(&HSTRING::from(path.as_os_str()), None, &mut pidl, 0, None) {
                Ok(()) => pidls.push(pidl),
                Err(err) => {
                    result = Err(err);
                    break;
                }
            }
        }
        result.and_then(|()| {
            let ids: Vec<*const ITEMIDLIST> = pidls.iter().map(|&p| p as *const _).collect();
            let items = SHCreateShellItemArrayFromIDLists(&ids)?;
            items.BindToHandler::<_, IDataObject>(None, &BHID_DataObject)
        })
    };
    for pidl in pidls {
        // SAFETY: allocated by SHParseDisplayName.
        unsafe { ILFree(Some(pidl)) };
    }
    result
}

/// An app's icon from `path,index`, as the shell reports it.
pub fn icon_image(icon: &str, _size: u32) -> Option<egui::ColorImage> {
    let (path, index) = icon.rsplit_once(',')?;
    let index: i32 = index.trim().parse().ok()?;
    let wide: Vec<u16> = Path::new(path)
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let mut large = HICON::default();
    // SAFETY: `wide` is NUL-terminated; the icon is destroyed below.
    unsafe {
        if ExtractIconExW(
            windows::core::PCWSTR(wide.as_ptr()),
            index,
            Some(&mut large),
            None,
            1,
        ) == 0
            || large.is_invalid()
        {
            return None;
        }
        let image = icon_pixels(large);
        let _ = DestroyIcon(large);
        image
    }
}

/// The colors of an icon, with alpha.
unsafe fn icon_pixels(icon: HICON) -> Option<egui::ColorImage> {
    // SAFETY: the caller passes a valid icon; GDI objects are released.
    unsafe {
        let mut info = ICONINFO::default();
        GetIconInfo(icon, &mut info).ok()?;
        let color = info.hbmColor;
        let mask = info.hbmMask;
        let mut bitmap = BITMAP::default();
        let ok = !color.is_invalid()
            && GetObjectW(
                HGDIOBJ(color.0),
                size_of::<BITMAP>() as i32,
                Some(&mut bitmap as *mut _ as _),
            ) != 0;
        let mut image = None;
        if ok {
            let (w, h) = (bitmap.bmWidth, bitmap.bmHeight);
            let mut header = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    // Negative: rows top to bottom.
                    biHeight: -h,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut pixels = vec![0u8; (w * h * 4) as usize];
            let dc = GetDC(None);
            let lines = GetDIBits(
                dc,
                color,
                0,
                h as u32,
                Some(pixels.as_mut_ptr() as _),
                &mut header,
                DIB_RGB_COLORS,
            );
            ReleaseDC(None, dc);
            if lines == h {
                // BGRA to RGBA; icons without an alpha channel are opaque.
                let has_alpha = pixels.as_chunks::<4>().0.iter().any(|p| p[3] != 0);
                for p in pixels.as_chunks_mut::<4>().0 {
                    p.swap(0, 2);
                    if !has_alpha {
                        p[3] = 255;
                    }
                }
                image = Some(egui::ColorImage::from_rgba_unmultiplied(
                    [w as usize, h as usize],
                    &pixels,
                ));
            }
        }
        let _ = DeleteObject(HGDIOBJ(color.0));
        let _ = DeleteObject(HGDIOBJ(mask.0));
        image
    }
}
