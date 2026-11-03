//! The filesystem, as the app sees it.
//!
//! Everywhere but the browser it's `std::fs`, re-exported. The browser has
//! no filesystem the app can reach (std's calls all fail there), so the web
//! demo keeps one in memory with the same functions and types (`memory`).
//! The app goes through here for every file; in the web build, clippy.toml
//! makes reaching `std::fs` or `Path`'s filesystem methods directly a warning.
//!
//! `Path::is_dir` and its kind go to std directly, so they're here as
//! functions: [`is_dir`], [`is_file`], [`is_symlink`] and [`path_exists`].

// clippy.toml's disallowed items fail only in the browser, and natively the
// replacements are the very same items.
#![cfg_attr(
    not(target_arch = "wasm32"),
    allow(clippy::disallowed_methods, clippy::disallowed_types)
)]

use std::path::Path;

#[cfg(not(target_arch = "wasm32"))]
pub use std::fs::{
    DirEntry, File, FileType, Metadata, OpenOptions, Permissions, ReadDir, canonicalize, copy,
    create_dir, create_dir_all, exists, hard_link, metadata, read, read_dir, read_link,
    read_to_string, remove_dir, remove_dir_all, remove_file, rename, set_permissions,
    symlink_metadata, write,
};

#[cfg(any(test, target_arch = "wasm32"))]
mod memory;
#[cfg(target_arch = "wasm32")]
pub use memory::{
    DirEntry, File, FileType, Metadata, OpenOptions, Permissions, ReadDir, RemoteLoader,
    add_remote, canonicalize, copy, create_dir, create_dir_all, exists, hard_link, metadata, read,
    read_dir, read_link, read_to_string, remove_dir, remove_dir_all, remove_file, rename,
    set_file_mtime, set_file_times, set_permissions, set_remote_loader, symlink, symlink_metadata,
    take_changes, write,
};

/// The time now, as file times are given. std's clock panics in the browser.
pub fn now() -> std::time::SystemTime {
    let since = web_time::SystemTime::now()
        .duration_since(web_time::UNIX_EPOCH)
        .unwrap_or_default();
    std::time::UNIX_EPOCH + since
}

/// This process's id, to tell its temporary files from others'. The browser
/// has no processes, and std panics there, but it has just the one app.
pub fn process_id() -> u32 {
    #[cfg(target_arch = "wasm32")]
    return 1;
    #[cfg(not(target_arch = "wasm32"))]
    std::process::id()
}

/// Whether `path` is a folder, or a link to one (`Path::is_dir`).
pub fn is_dir(path: impl AsRef<Path>) -> bool {
    metadata(path).is_ok_and(|m| m.is_dir())
}

/// Whether `path` is a file, or a link to one (`Path::is_file`).
pub fn is_file(path: impl AsRef<Path>) -> bool {
    metadata(path).is_ok_and(|m| m.is_file())
}

/// Whether `path` is a symlink, whatever it points to (`Path::is_symlink`).
pub fn is_symlink(path: impl AsRef<Path>) -> bool {
    symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
}

/// Whether there's something at `path`, following links (`Path::exists`).
pub fn path_exists(path: impl AsRef<Path>) -> bool {
    metadata(path).is_ok()
}

/// Sets when `path` was last read and changed, following links.
#[cfg(not(target_arch = "wasm32"))]
pub fn set_file_times(
    path: impl AsRef<Path>,
    accessed: std::time::SystemTime,
    modified: std::time::SystemTime,
) -> std::io::Result<()> {
    filetime::set_file_times(
        path,
        filetime::FileTime::from_system_time(accessed),
        filetime::FileTime::from_system_time(modified),
    )
}

/// Sets when `path` was last changed, following links.
#[cfg(not(target_arch = "wasm32"))]
pub fn set_file_mtime(
    path: impl AsRef<Path>,
    modified: std::time::SystemTime,
) -> std::io::Result<()> {
    filetime::set_file_mtime(path, filetime::FileTime::from_system_time(modified))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_helpers_match_std() {
        let dir = std::env::temp_dir().join(format!("disk-helpers-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("f");
        std::fs::write(&file, "x").unwrap();
        for path in [&dir, &file, &dir.join("missing")] {
            assert_eq!(is_dir(path), path.is_dir());
            assert_eq!(is_file(path), path.is_file());
            assert_eq!(is_symlink(path), path.is_symlink());
            assert_eq!(path_exists(path), path.exists());
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
