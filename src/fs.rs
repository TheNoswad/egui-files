//! Directory listing, sorting, formatting and file operations.

use std::{
    cmp::Ordering,
    ffi::OsStr,
    fs, io,
    path::{Path, PathBuf},
    time::SystemTime,
};

use egui_phosphor::regular as icon;

use crate::theme::AppColors;
pub use crate::theme::IconKind;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug)]
pub struct Entry {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub hidden: bool,
    pub executable: bool,
    pub size: u64,
    /// A folder's size with everything in it, once a scan has measured it.
    pub folder_size: Option<u64>,
    pub modified: Option<SystemTime>,
    /// Not every filesystem records it.
    pub created: Option<SystemTime>,
}

impl Entry {
    /// Folders count what's in them when that's known, else nothing.
    fn sort_size(&self) -> u64 {
        if self.is_dir {
            self.folder_size.unwrap_or(0)
        } else {
            self.size
        }
    }

    fn extension(&self) -> String {
        self.path
            .extension()
            .and_then(OsStr::to_str)
            .map(str::to_ascii_lowercase)
            .unwrap_or_default()
    }

    pub fn icon(&self) -> &'static str {
        icon_for(&self.path, self.is_dir, self.is_symlink)
    }

    pub fn icon_kind(&self) -> IconKind {
        icon_kind(&self.path, self.is_dir, self.is_symlink)
    }

    /// The theme's color for this entry's icon, if it has one.
    pub fn icon_color(&self, colors: &AppColors) -> Option<egui::Color32> {
        colors.icon(self.icon_kind())
    }

    /// The theme's color for this entry's name, if it isn't the usual one.
    pub fn name_color(&self, colors: &AppColors) -> Option<egui::Color32> {
        colors.name(self.hidden, self.is_symlink, self.executable)
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

/// What kind of file a path is, for its icon, based on its extension.
pub fn icon_kind(path: &Path, is_dir: bool, is_symlink: bool) -> IconKind {
    if is_dir {
        return IconKind::Folder;
    }
    let extension = path
        .extension()
        .and_then(OsStr::to_str)
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    match extension.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "svg" | "ico" | "tif" | "tiff" => {
            IconKind::Image
        }
        "mp3" | "flac" | "ogg" | "wav" | "opus" | "m4a" | "aac" => IconKind::Audio,
        "mp4" | "mkv" | "webm" | "avi" | "mov" | "wmv" => IconKind::Video,
        "pdf" => IconKind::Pdf,
        "zip" | "tar" | "gz" | "xz" | "zst" | "7z" | "rar" | "bz2" => IconKind::Archive,
        "rs" | "py" | "js" | "ts" | "c" | "h" | "cpp" | "hpp" | "go" | "java" | "sh" | "toml"
        | "json" | "yaml" | "yml" | "html" | "css" | "lua" | "ron" => IconKind::Code,
        "txt" | "md" | "log" | "ini" | "cfg" | "conf" => IconKind::Text,
        "doc" | "docx" | "odt" | "rtf" => IconKind::Document,
        _ if is_symlink => IconKind::Link,
        _ => IconKind::File,
    }
}

/// Icon glyph for a path, based on its extension.
pub fn icon_for(path: &Path, is_dir: bool, is_symlink: bool) -> &'static str {
    match icon_kind(path, is_dir, is_symlink) {
        IconKind::Folder if is_symlink => icon::FOLDER_SIMPLE,
        IconKind::Folder => icon::FOLDER,
        IconKind::Image => icon::FILE_IMAGE,
        IconKind::Audio => icon::FILE_AUDIO,
        IconKind::Video => icon::FILE_VIDEO,
        IconKind::Pdf => icon::FILE_PDF,
        IconKind::Archive => icon::FILE_ZIP,
        IconKind::Code => icon::FILE_CODE,
        IconKind::Text => icon::FILE_TEXT,
        IconKind::Document => icon::FILE_DOC,
        IconKind::Link => icon::LINK,
        IconKind::File => icon::FILE,
    }
}

/// Whether a file can be run: any execute bit set, on Unix.
fn is_executable(meta: Option<&fs::Metadata>) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.is_some_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        false
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
        let link_meta = dir_entry.metadata().ok();
        let meta = fs::metadata(&path).ok().or_else(|| link_meta.clone());
        let is_dir = meta.as_ref().is_some_and(fs::Metadata::is_dir);
        entries.push(Entry {
            // Whether the link itself is hidden, not its target: Windows
            // hides junctions like Documents\My Music (to Music).
            hidden: is_hidden(&name, link_meta.as_ref()),
            executable: is_executable(meta.as_ref()),
            size: meta
                .as_ref()
                .filter(|m| m.is_file())
                .map_or(0, fs::Metadata::len),
            folder_size: None,
            modified: meta.as_ref().and_then(|m| m.modified().ok()),
            created: meta.as_ref().and_then(|m| m.created().ok()),
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
    Created,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sort {
    pub key: SortKey,
    pub ascending: bool,
    #[serde(default = "folders_first")]
    pub folders_first: bool,
}

fn folders_first() -> bool {
    true
}

impl Default for Sort {
    fn default() -> Self {
        Self {
            key: SortKey::Name,
            ascending: true,
            folders_first: true,
        }
    }
}

impl SortKey {
    pub const ALL: [(Self, &'static str); 5] = [
        (Self::Name, "Name"),
        (Self::Size, "Size"),
        (Self::Modified, "Modified"),
        (Self::Created, "Created"),
        (Self::Kind, "Kind"),
    ];

    /// The direction sorting by this key starts in: dates newest first.
    pub fn starts_ascending(self) -> bool {
        !matches!(self, Self::Modified | Self::Created)
    }

    /// What ascending and descending mean for this key.
    pub fn directions(self) -> [&'static str; 2] {
        match self {
            Self::Name | Self::Kind => ["A to Z", "Z to A"],
            Self::Size => ["Smallest first", "Largest first"],
            Self::Modified | Self::Created => ["Oldest first", "Newest first"],
        }
    }
}

/// Sorts entries, folders first unless the sort says otherwise.
pub fn sort_entries(entries: &mut [Entry], sort: Sort) {
    fn by_name(a: &Entry, b: &Entry) -> Ordering {
        a.name.to_lowercase().cmp(&b.name.to_lowercase())
    }
    entries.sort_by(|a, b| {
        let folders = if sort.folders_first {
            b.is_dir.cmp(&a.is_dir)
        } else {
            Ordering::Equal
        };
        folders.then_with(|| {
            let ord = match sort.key {
                SortKey::Name => by_name(a, b),
                SortKey::Size => a
                    .sort_size()
                    .cmp(&b.sort_size())
                    .then_with(|| by_name(a, b)),
                SortKey::Modified => a.modified.cmp(&b.modified).then_with(|| by_name(a, b)),
                SortKey::Created => a.created.cmp(&b.created).then_with(|| by_name(a, b)),
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
    time.map(|t| crate::dates::format_exact(t.into(), crate::dates::clock_format()))
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

/// Creates an empty file, or a copy of `template`, with a unique name in `parent`.
pub fn create_file(parent: &Path, template: Option<&Path>) -> io::Result<PathBuf> {
    let name = template
        .and_then(Path::file_name)
        .unwrap_or(OsStr::new("New File"));
    let path = unique_path(parent, name);
    match template {
        Some(template) => fs::copy(template, &path).map(drop)?,
        None => fs::File::create_new(&path).map(drop)?,
    }
    Ok(path)
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
