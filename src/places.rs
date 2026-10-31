//! Standard user folders and mounted drives for the sidebar.

use std::path::PathBuf;

use egui_phosphor::regular as icon;

#[derive(Clone, Debug, PartialEq)]
pub struct Place {
    pub label: String,
    pub icon: &'static str,
    pub path: PathBuf,
    /// A removable drive the sidebar offers to eject.
    pub ejectable: bool,
    /// Tells the media in a drive apart (on Windows, the volume serial
    /// number), so swapping a memory card shows; `None` when the drive is
    /// empty, like a card reader without a card.
    pub media: Option<u32>,
}

pub fn home_dir() -> PathBuf {
    directories::UserDirs::new()
        .map(|d| d.home_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from(if cfg!(windows) { "C:\\" } else { "/" }))
}

pub fn standard_places() -> Vec<Place> {
    let Some(dirs) = directories::UserDirs::new() else {
        return Vec::new();
    };
    let mut places = vec![Place {
        label: "Home".to_owned(),
        icon: icon::HOUSE,
        path: dirs.home_dir().to_path_buf(),
        ejectable: false,
        media: Some(0),
    }];
    let optional = [
        ("Desktop", icon::DESKTOP, dirs.desktop_dir()),
        ("Documents", icon::FILE_TEXT, dirs.document_dir()),
        ("Downloads", icon::DOWNLOAD_SIMPLE, dirs.download_dir()),
        ("Pictures", icon::IMAGE, dirs.picture_dir()),
        ("Music", icon::MUSIC_NOTES, dirs.audio_dir()),
        ("Videos", icon::FILM_STRIP, dirs.video_dir()),
    ];
    places.extend(optional.into_iter().filter_map(|(label, icon, path)| {
        path.filter(|p| p.is_dir()).map(|p| Place {
            label: label.to_owned(),
            icon,
            path: p.to_path_buf(),
            ejectable: false,
            media: Some(0),
        })
    }));
    places
}

/// Files in the user's templates folder, for "New from template".
pub fn templates() -> Vec<PathBuf> {
    let Some(dir) = directories::UserDirs::new().and_then(|d| d.template_dir().map(PathBuf::from))
    else {
        return Vec::new();
    };
    let mut templates: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    templates.sort();
    templates
}

/// Drive letters from the bitmask (probing A: to Z: can hang on network
/// drives and card readers), named like Explorer does.
#[cfg(windows)]
pub fn drives() -> Vec<Place> {
    use windows::{
        Win32::{
            Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW},
            System::WindowsProgramming::{DRIVE_CDROM, DRIVE_FIXED, DRIVE_REMOTE, DRIVE_REMOVABLE},
        },
        core::PCWSTR,
    };
    // SAFETY: no arguments.
    let mask = unsafe { GetLogicalDrives() };
    (0..26)
        .filter(|bit| mask & (1 << bit) != 0)
        .map(|bit| {
            let letter = char::from(b'A' + bit as u8);
            let root = format!("{letter}:\\");
            let wide: Vec<u16> = root.encode_utf16().chain(Some(0)).collect();
            // SAFETY: `wide` is NUL-terminated.
            let kind = unsafe { GetDriveTypeW(PCWSTR(wide.as_ptr())) };
            let mut name = [0u16; 261];
            let mut serial = 0;
            // Network drives can take long to answer, or never; they're
            // taken to be there.
            let named = kind != DRIVE_REMOTE
                // SAFETY: `wide` is NUL-terminated; the buffer's length is passed.
                && unsafe {
                    GetVolumeInformationW(
                        PCWSTR(wide.as_ptr()),
                        Some(&mut name),
                        Some(&mut serial),
                        None,
                        None,
                        None,
                    )
                }
                .is_ok();
            let media = if kind == DRIVE_REMOTE {
                Some(0)
            } else {
                named.then_some(serial)
            };
            let len = name.iter().position(|&c| c == 0).unwrap_or(0);
            let name = String::from_utf16_lossy(&name[..len]);
            let ejectable = matches!(kind, DRIVE_REMOVABLE | DRIVE_CDROM)
                || kind == DRIVE_FIXED && windows_drive_is_usb(letter);
            let (fallback, glyph) = match kind {
                DRIVE_CDROM => ("CD Drive", icon::DISC),
                DRIVE_REMOTE => ("Network Drive", icon::NETWORK),
                _ if ejectable => ("USB Drive", icon::USB),
                _ => ("Local Disk", icon::HARD_DRIVE),
            };
            let name = if named && !name.is_empty() {
                name.as_str()
            } else {
                fallback
            };
            Place {
                label: format!("{name} ({letter}:)"),
                icon: glyph,
                path: PathBuf::from(root),
                ejectable,
                media,
            }
        })
        .collect()
}

/// Whether the disk behind a fixed drive letter is on USB (an external hard
/// drive, which Windows calls fixed).
#[cfg(windows)]
fn windows_drive_is_usb(letter: char) -> bool {
    use windows::{
        Win32::{
            Foundation::CloseHandle,
            Storage::FileSystem::{
                BusTypeUsb, CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ,
                FILE_SHARE_WRITE, OPEN_EXISTING,
            },
            System::{
                IO::DeviceIoControl,
                Ioctl::{
                    IOCTL_STORAGE_QUERY_PROPERTY, PropertyStandardQuery, STORAGE_DEVICE_DESCRIPTOR,
                    STORAGE_PROPERTY_QUERY, StorageDeviceProperty,
                },
            },
        },
        core::PCWSTR,
    };
    let volume: Vec<u16> = format!(r"\\.\{letter}:")
        .encode_utf16()
        .chain(Some(0))
        .collect();
    // SAFETY: `volume` is NUL-terminated; the query and descriptor are the
    // sizes passed; the handle is closed.
    unsafe {
        let Ok(handle) = CreateFileW(
            PCWSTR(volume.as_ptr()),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        ) else {
            return false;
        };
        let query = STORAGE_PROPERTY_QUERY {
            PropertyId: StorageDeviceProperty,
            QueryType: PropertyStandardQuery,
            ..Default::default()
        };
        let mut descriptor = STORAGE_DEVICE_DESCRIPTOR::default();
        let ok = DeviceIoControl(
            handle,
            IOCTL_STORAGE_QUERY_PROPERTY,
            Some((&raw const query).cast()),
            size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            Some((&raw mut descriptor).cast()),
            size_of::<STORAGE_DEVICE_DESCRIPTOR>() as u32,
            None,
            None,
        )
        .is_ok();
        let _ = CloseHandle(handle);
        ok && descriptor.BusType == BusTypeUsb
    }
}

#[cfg(not(windows))]
pub fn drives() -> Vec<Place> {
    let mut drives = vec![Place {
        label: "File System".to_owned(),
        icon: icon::HARD_DRIVE,
        path: PathBuf::from("/"),
        ejectable: false,
        media: Some(0),
    }];
    let user = std::env::var("USER").unwrap_or_default();
    let roots = [
        PathBuf::from("/run/media").join(&user),
        PathBuf::from("/media").join(&user),
        PathBuf::from("/mnt"),
    ];
    // UDisks mounts removable drives under /run/media/$USER (or /media).
    for (i, root) in roots.into_iter().enumerate() {
        let ejectable = i < 2;
        let Ok(read) = std::fs::read_dir(&root) else {
            continue;
        };
        let mut found: Vec<_> = read
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .map(|path| Place {
                label: crate::fs::display_name(&path),
                icon: if ejectable {
                    icon::USB
                } else {
                    icon::HARD_DRIVE
                },
                path,
                ejectable,
                media: Some(0),
            })
            .collect();
        found.sort_by(|a, b| a.label.cmp(&b.label));
        drives.extend(found);
    }
    drives
}
