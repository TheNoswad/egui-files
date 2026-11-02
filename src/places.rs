//! Standard user folders and mounted drives for the sidebar.

use std::path::{Path, PathBuf};

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
    /// How big the drive is and how much is free, for drives.
    pub space: Option<Space>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Space {
    pub total: u64,
    /// Free for the user (not counting space reserved for root).
    pub available: u64,
}

impl Space {
    pub fn used_share(self) -> f32 {
        self.total.saturating_sub(self.available) as f32 / self.total.max(1) as f32
    }
}

/// The size and free space of the filesystem holding `path`.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn space(path: &Path) -> Option<Space> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `path` is NUL-terminated and `stat` is the struct it fills in.
    if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    let block = stat.f_frsize as u64;
    Some(Space {
        total: stat.f_blocks as u64 * block,
        available: stat.f_bavail as u64 * block,
    })
    .filter(|s| s.total > 0)
}

#[cfg(windows)]
pub fn space(path: &Path) -> Option<Space> {
    use windows::{Win32::Storage::FileSystem::GetDiskFreeSpaceExW, core::PCWSTR};
    let wide: Vec<u16> = path
        .as_os_str()
        .to_string_lossy()
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let (mut available, mut total) = (0u64, 0u64);
    // SAFETY: `wide` is NUL-terminated; the outputs are u64s.
    unsafe {
        GetDiskFreeSpaceExW(
            PCWSTR(wide.as_ptr()),
            Some(&mut available),
            Some(&mut total),
            None,
        )
    }
    .ok()?;
    Some(Space { total, available }).filter(|s| s.total > 0)
}

#[cfg(not(any(target_os = "linux", target_os = "android", windows)))]
pub fn space(_path: &Path) -> Option<Space> {
    None
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
        space: None,
    }];
    #[cfg(not(target_os = "android"))]
    let optional = [
        (
            "Desktop",
            icon::DESKTOP,
            dirs.desktop_dir().map(Path::to_path_buf),
        ),
        (
            "Documents",
            icon::FILE_TEXT,
            dirs.document_dir().map(Path::to_path_buf),
        ),
        (
            "Downloads",
            icon::DOWNLOAD_SIMPLE,
            dirs.download_dir().map(Path::to_path_buf),
        ),
        (
            "Pictures",
            icon::IMAGE,
            dirs.picture_dir().map(Path::to_path_buf),
        ),
        (
            "Music",
            icon::MUSIC_NOTES,
            dirs.audio_dir().map(Path::to_path_buf),
        ),
        (
            "Videos",
            icon::FILM_STRIP,
            dirs.video_dir().map(Path::to_path_buf),
        ),
    ];
    // Android's own folders in its shared storage, which XDG doesn't name.
    #[cfg(target_os = "android")]
    let optional = {
        let home = dirs.home_dir();
        [
            ("Camera", icon::CAMERA, Some(home.join("DCIM"))),
            (
                "Downloads",
                icon::DOWNLOAD_SIMPLE,
                Some(home.join("Download")),
            ),
            ("Documents", icon::FILE_TEXT, Some(home.join("Documents"))),
            ("Pictures", icon::IMAGE, Some(home.join("Pictures"))),
            ("Music", icon::MUSIC_NOTES, Some(home.join("Music"))),
            ("Movies", icon::FILM_STRIP, Some(home.join("Movies"))),
        ]
    };
    places.extend(optional.into_iter().filter_map(|(label, icon, path)| {
        path.filter(|p| p.is_dir()).map(|p| Place {
            label: label.to_owned(),
            icon,
            path: p,
            ejectable: false,
            media: Some(0),
            space: None,
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
            // Network drives can be slow to answer.
            let space = (media.is_some() && kind != DRIVE_REMOTE)
                .then(|| space(Path::new(&root)))
                .flatten();
            Place {
                label: format!("{name} ({letter}:)"),
                icon: glyph,
                path: PathBuf::from(root),
                ejectable,
                media,
                space,
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

/// Android's storage volumes: the internal shared storage, then SD cards
/// and USB drives, each mounted under /storage by its id.
#[cfg(target_os = "android")]
pub fn drives() -> Vec<Place> {
    let internal = PathBuf::from("/storage/emulated/0");
    let mut drives = vec![Place {
        label: "Internal storage".to_owned(),
        icon: icon::DEVICE_MOBILE,
        space: space(&internal),
        path: internal,
        ejectable: false,
        media: Some(0),
    }];
    let Ok(read) = std::fs::read_dir("/storage") else {
        return drives;
    };
    let mut found: Vec<_> = read
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or_default();
            name != "emulated" && name != "self" && p.is_dir()
        })
        .map(|path| Place {
            label: crate::fs::display_name(&path),
            icon: icon::HARD_DRIVES,
            space: space(&path),
            path,
            ejectable: false,
            media: Some(0),
        })
        .collect();
    found.sort_by(|a, b| a.label.cmp(&b.label));
    drives.extend(found);
    drives
}

#[cfg(not(any(windows, target_os = "android")))]
pub fn drives() -> Vec<Place> {
    let mut drives = vec![Place {
        label: "File System".to_owned(),
        icon: icon::HARD_DRIVE,
        path: PathBuf::from("/"),
        ejectable: false,
        media: Some(0),
        space: space(Path::new("/")),
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
                space: space(&path),
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
