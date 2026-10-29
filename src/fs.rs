//! Directory listing, sorting, formatting and file operations.

use std::{
    cmp::Ordering,
    ffi::OsStr,
    fs, io,
    path::{Path, PathBuf},
    time::SystemTime,
};

use egui_phosphor::regular as icon;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug)]
pub struct Entry {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub hidden: bool,
    pub size: u64,
    pub modified: Option<SystemTime>,
}

impl Entry {
    fn extension(&self) -> String {
        self.path
            .extension()
            .and_then(OsStr::to_str)
            .map(str::to_ascii_lowercase)
            .unwrap_or_default()
    }

    pub fn icon(&self) -> &'static str {
        if self.is_dir {
            return if self.is_symlink {
                icon::FOLDER_SIMPLE
            } else {
                icon::FOLDER
            };
        }
        match self.extension().as_str() {
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "svg" | "ico" | "tif" | "tiff" => {
                icon::FILE_IMAGE
            }
            "mp3" | "flac" | "ogg" | "wav" | "opus" | "m4a" | "aac" => icon::FILE_AUDIO,
            "mp4" | "mkv" | "webm" | "avi" | "mov" | "wmv" => icon::FILE_VIDEO,
            "pdf" => icon::FILE_PDF,
            "zip" | "tar" | "gz" | "xz" | "zst" | "7z" | "rar" | "bz2" => icon::FILE_ZIP,
            "rs" | "py" | "js" | "ts" | "c" | "h" | "cpp" | "hpp" | "go" | "java" | "sh"
            | "toml" | "json" | "yaml" | "yml" | "html" | "css" | "lua" | "ron" => icon::FILE_CODE,
            "txt" | "md" | "log" | "ini" | "cfg" | "conf" => icon::FILE_TEXT,
            "doc" | "docx" | "odt" | "rtf" => icon::FILE_DOC,
            _ if self.is_symlink => icon::LINK,
            _ => icon::FILE,
        }
    }

    pub fn kind(&self) -> String {
        if self.is_dir {
            return "Folder".to_owned();
        }
        match self.extension() {
            ext if ext.is_empty() => "File".to_owned(),
            ext => format!("{} file", ext.to_ascii_uppercase()),
        }
    }
}

pub fn read_dir(dir: &Path) -> io::Result<Vec<Entry>> {
    let mut entries = Vec::new();
    for dir_entry in fs::read_dir(dir)? {
        let Ok(dir_entry) = dir_entry else { continue };
        let path = dir_entry.path();
        let name = dir_entry.file_name().to_string_lossy().into_owned();
        let is_symlink = dir_entry.file_type().is_ok_and(|t| t.is_symlink());
        // Follow symlinks, but fall back to the link itself when it is broken.
        let meta = fs::metadata(&path).or_else(|_| dir_entry.metadata()).ok();
        let is_dir = meta.as_ref().is_some_and(fs::Metadata::is_dir);
        entries.push(Entry {
            hidden: is_hidden(&name, meta.as_ref()),
            size: meta
                .as_ref()
                .filter(|m| m.is_file())
                .map_or(0, fs::Metadata::len),
            modified: meta.as_ref().and_then(|m| m.modified().ok()),
            path,
            name,
            is_dir,
            is_symlink,
        });
    }
    Ok(entries)
}

#[cfg(windows)]
fn is_hidden(_name: &str, meta: Option<&fs::Metadata>) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
    meta.is_some_and(|m| m.file_attributes() & FILE_ATTRIBUTE_HIDDEN != 0)
}

#[cfg(not(windows))]
fn is_hidden(name: &str, _meta: Option<&fs::Metadata>) -> bool {
    name.starts_with('.')
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SortKey {
    #[default]
    Name,
    Size,
    Modified,
    Kind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sort {
    pub key: SortKey,
    pub ascending: bool,
}

impl Default for Sort {
    fn default() -> Self {
        Self {
            key: SortKey::Name,
            ascending: true,
        }
    }
}

/// Sorts entries with folders always first.
pub fn sort_entries(entries: &mut [Entry], sort: Sort) {
    fn by_name(a: &Entry, b: &Entry) -> Ordering {
        a.name.to_lowercase().cmp(&b.name.to_lowercase())
    }
    entries.sort_by(|a, b| {
        b.is_dir.cmp(&a.is_dir).then_with(|| {
            let ord = match sort.key {
                SortKey::Name => by_name(a, b),
                SortKey::Size => a.size.cmp(&b.size).then_with(|| by_name(a, b)),
                SortKey::Modified => a.modified.cmp(&b.modified).then_with(|| by_name(a, b)),
                SortKey::Kind => a.kind().cmp(&b.kind()).then_with(|| by_name(a, b)),
            };
            if sort.ascending { ord } else { ord.reverse() }
        })
    });
}

pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

pub fn format_time(time: Option<SystemTime>) -> String {
    time.map(|t| {
        chrono::DateTime::<chrono::Local>::from(t)
            .format("%Y-%m-%d %H:%M")
            .to_string()
    })
    .unwrap_or_default()
}

/// Returns the display name for a path, falling back to the full path for roots.
pub fn display_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

// ---------------------------------------------------------------------------
// Operations

pub fn open(path: &Path) -> io::Result<()> {
    open::that_detached(path)
}

pub fn rename(from: &Path, new_name: &str) -> io::Result<PathBuf> {
    let new_name = new_name.trim();
    if new_name.is_empty() || new_name.contains(['/', std::path::MAIN_SEPARATOR]) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid file name",
        ));
    }
    let to = from.with_file_name(new_name);
    if to != from && exists(&to) {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{new_name} already exists"),
        ));
    }
    fs::rename(from, &to)?;
    Ok(to)
}

pub fn create_dir(parent: &Path) -> io::Result<PathBuf> {
    let path = unique_path(parent, OsStr::new("New Folder"));
    fs::create_dir(&path)?;
    Ok(path)
}

fn exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

/// `dir/name`, or `dir/name (n).ext` if that is taken.
pub fn unique_path(dir: &Path, name: &OsStr) -> PathBuf {
    let candidate = dir.join(name);
    if !exists(&candidate) {
        return candidate;
    }
    let as_path = Path::new(name);
    let stem = as_path.file_stem().unwrap_or(name).to_string_lossy();
    let ext = as_path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    (1..)
        .map(|n| dir.join(format!("{stem} ({n}){ext}")))
        .find(|p| !exists(p))
        .expect("unbounded search")
}
