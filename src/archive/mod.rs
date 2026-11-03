//! Archives: listing, extracting and creating zip, tar (gz/xz/zst/bz2), 7z,
//! and extracting rar. Everything runs on job or listing threads; the UI
//! parts are the archive tab and the compress / extract dialogs.

mod dialogs;
mod extract;
#[cfg(test)]
mod format_tests;
mod listings;
mod read;
mod tab;
mod write;

use std::{
    path::{Component, Path, PathBuf},
    time::SystemTime,
};

use serde::{Deserialize, Serialize};

pub use dialogs::{CompressDialog, DialogResult, ExtractDialog, subfolder_label};
pub use extract::extract;
pub use listings::{ListState, Listings};
pub use tab::{ArchiveTab, COLUMNS};
pub use write::compress;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Format {
    Zip,
    SevenZ,
    Tar,
    TarGz,
    TarXz,
    TarZst,
    TarBz2,
    Rar,
    /// A single compressed file, e.g. `notes.txt.gz`.
    Gz,
    Xz,
    Zst,
    Bz2,
}

/// Name suffixes, longest first so `.tar.gz` wins over `.gz`.
const SUFFIXES: &[(&str, Format)] = &[
    (".tar.gz", Format::TarGz),
    (".tar.xz", Format::TarXz),
    (".tar.zst", Format::TarZst),
    (".tar.bz2", Format::TarBz2),
    (".tgz", Format::TarGz),
    (".txz", Format::TarXz),
    (".tzst", Format::TarZst),
    (".tbz2", Format::TarBz2),
    (".tbz", Format::TarBz2),
    (".tar", Format::Tar),
    (".zip", Format::Zip),
    (".7z", Format::SevenZ),
    (".rar", Format::Rar),
    (".gz", Format::Gz),
    (".xz", Format::Xz),
    (".zst", Format::Zst),
    (".bz2", Format::Bz2),
];

impl Format {
    /// What Compress can create.
    #[cfg(not(target_arch = "wasm32"))]
    pub const CREATE: &[Self] = &[
        Self::Zip,
        Self::TarGz,
        Self::TarXz,
        Self::TarZst,
        Self::SevenZ,
    ];
    /// What Compress can create: zstd is C, so the browser has none.
    #[cfg(target_arch = "wasm32")]
    pub const CREATE: &[Self] = &[Self::Zip, Self::TarGz, Self::TarXz, Self::SevenZ];

    pub fn detect(path: &Path) -> Option<Self> {
        let name = path.file_name()?.to_string_lossy().to_lowercase();
        SUFFIXES
            .iter()
            .find(|(suffix, _)| name.len() > suffix.len() && name.ends_with(suffix))
            .map(|&(_, format)| format)
    }

    pub fn extension(self) -> &'static str {
        match self {
            Self::Zip => ".zip",
            Self::SevenZ => ".7z",
            Self::Tar => ".tar",
            Self::TarGz => ".tar.gz",
            Self::TarXz => ".tar.xz",
            Self::TarZst => ".tar.zst",
            Self::TarBz2 => ".tar.bz2",
            Self::Rar => ".rar",
            Self::Gz => ".gz",
            Self::Xz => ".xz",
            Self::Zst => ".zst",
            Self::Bz2 => ".bz2",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Zip => "ZIP",
            Self::SevenZ => "7-Zip",
            Self::Tar => "Tar",
            Self::TarGz => "Tar + gzip",
            Self::TarXz => "Tar + xz",
            Self::TarZst => "Tar + zstd",
            Self::TarBz2 => "Tar + bzip2",
            Self::Rar => "RAR",
            Self::Gz => "gzip",
            Self::Xz => "xz",
            Self::Zst => "zstd",
            Self::Bz2 => "bzip2",
        }
    }

    pub fn supports_password(self) -> bool {
        matches!(self, Self::Zip | Self::SevenZ)
    }
}

pub fn is_archive(path: &Path) -> bool {
    Format::detect(path).is_some()
}

/// The archive's name without its archive extension: `photos.tar.gz` → `photos`.
pub fn stem(path: &Path) -> String {
    let name = crate::fs::display_name(path);
    let lower = name.to_lowercase();
    let stem = SUFFIXES
        .iter()
        .find(|(suffix, _)| lower.len() > suffix.len() && lower.ends_with(suffix))
        .map_or(name.as_str(), |(suffix, _)| {
            &name[..name.len() - suffix.len()]
        });
    // Multi-volume rar: `movie.part1.rar` → `movie`.
    match stem.rsplit_once(".part") {
        Some((base, n)) if !base.is_empty() && n.chars().all(|c| c.is_ascii_digit()) => {
            base.to_owned()
        }
        _ => stem.to_owned(),
    }
}

/// One entry of an archive's listing.
#[derive(Clone, Debug)]
pub struct Entry {
    /// `/`-separated path inside the archive, without a trailing slash.
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    /// Compressed size, where the format records one per entry.
    pub packed: Option<u64>,
    pub modified: Option<SystemTime>,
    pub encrypted: bool,
}

pub struct Listing {
    pub format: Format,
    pub entries: Vec<Entry>,
    pub archive_size: u64,
}

impl Listing {
    pub fn total_size(&self) -> u64 {
        self.entries.iter().map(|e| e.size).sum()
    }

    pub fn files(&self) -> usize {
        self.entries.iter().filter(|e| !e.is_dir).count()
    }

    pub fn encrypted(&self) -> bool {
        self.entries.iter().any(|e| e.encrypted)
    }
}

/// Why RAR and zstd archives fail in the browser: both are C libraries.
#[cfg(target_arch = "wasm32")]
const NOT_IN_BROWSER: &str = "this kind of archive needs the desktop app";

#[derive(Debug)]
pub enum Error {
    NeedsPassword,
    WrongPassword,
    Cancelled,
    Other(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NeedsPassword => f.write_str("a password is required"),
            Self::WrongPassword => f.write_str("wrong password"),
            Self::Cancelled => f.write_str("cancelled"),
            Self::Other(message) => f.write_str(message),
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        if err.kind() == std::io::ErrorKind::Interrupted {
            Self::Cancelled
        } else {
            Self::Other(err.to_string())
        }
    }
}

/// `name` as a relative path that stays inside the folder it is extracted to,
/// or `None` if it would escape (`..`, absolute paths, drive letters).
pub fn safe_relative(name: &str) -> Option<PathBuf> {
    let mut path = PathBuf::new();
    for part in name.split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." => return None,
            part => {
                let mut components = Path::new(part).components();
                if !matches!(
                    (components.next(), components.next()),
                    (Some(Component::Normal(_)), None)
                ) || cfg!(windows) && part.contains(':')
                {
                    return None;
                }
                path.push(part);
            }
        }
    }
    (!path.as_os_str().is_empty()).then_some(path)
}

/// `name` normalized to the form listings use: `a/b`, no leading `./` or trailing `/`.
fn normalize(name: &str) -> String {
    name.split(['/', '\\'])
        .filter(|part| !part.is_empty() && *part != ".")
        .collect::<Vec<_>>()
        .join("/")
}

/// Entries of an archive to extract, and the folder inside the archive they
/// are taken out of (so extracting `docs/a.txt` gives `a.txt`).
#[derive(Clone, Debug, Default)]
pub struct Selection {
    /// Selected paths, as in [`Entry::path`]. Folders include their contents.
    pub paths: Vec<String>,
    /// Removed from the front of every path; empty or ends in `/`.
    pub base: String,
}

impl Selection {
    /// Selects `paths`, extracting them relative to their common parent.
    pub fn new(paths: Vec<String>) -> Self {
        let parent = |p: &str| p.rsplit_once('/').map_or("", |(dir, _)| dir).to_owned();
        let mut base = paths.first().map(|p| parent(p)).unwrap_or_default();
        for path in &paths {
            while !(base.is_empty() || path.starts_with(&format!("{base}/"))) {
                base = parent(&base);
            }
        }
        if !base.is_empty() {
            base.push('/');
        }
        Self { paths, base }
    }

    /// Where an entry goes relative to the destination, if it is selected.
    fn map(&self, path: &str) -> Option<String> {
        let selected = self.paths.iter().any(|p| {
            path == p
                || path.len() > p.len()
                    && path.starts_with(p.as_str())
                    && path.as_bytes()[p.len()] == b'/'
        });
        if !selected {
            return None;
        }
        path.strip_prefix(&self.base).map(str::to_owned)
    }
}

/// Settings remembered between uses of the dialogs.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    pub format: Format,
    /// 0 (store) to 9 (smallest).
    pub level: u32,
    pub encrypt_names: bool,
    pub trash_after_extract: bool,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            format: Format::Zip,
            level: 5,
            encrypt_names: true,
            trash_after_extract: false,
        }
    }
}

#[derive(Clone, Debug)]
pub struct CompressJob {
    pub sources: Vec<PathBuf>,
    /// The archive to create; a free name is picked if it is taken.
    pub dest: PathBuf,
    pub format: Format,
    pub level: u32,
    pub password: Option<String>,
    /// 7z only: also hide the names of the files.
    pub encrypt_names: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExtractMode {
    /// One top-level item goes straight in; several get a folder named after the archive.
    Smart,
    /// Into a folder named after the archive.
    Subfolder,
    /// Straight into the destination.
    Into,
    /// To a temporary folder, then open the single file extracted.
    Open,
}

#[derive(Clone, Debug)]
pub struct ExtractJob {
    pub archive: PathBuf,
    pub dest: PathBuf,
    pub mode: ExtractMode,
    /// Only these entries; everything if `None`.
    pub selection: Option<Selection>,
    /// Tried first, e.g. the one typed into the archive tab.
    pub password: Option<String>,
    /// Move the archive to the trash once it extracted without errors.
    pub trash_after: bool,
    /// With [`ExtractMode::Open`], the app to open the file with instead of
    /// its default one.
    pub open_with: Option<crate::open_with::Target>,
}

impl ExtractJob {
    pub fn new(archive: PathBuf, dest: PathBuf, mode: ExtractMode, trash_after: bool) -> Self {
        Self {
            archive,
            dest,
            mode,
            selection: None,
            password: None,
            trash_after,
            open_with: None,
        }
    }

    pub fn title(&self) -> String {
        let name = crate::fs::display_name(&self.archive);
        match (&self.selection, self.mode) {
            (_, ExtractMode::Open) => format!("Opening from {name}"),
            (Some(selection), _) => format!(
                "Extracting {} item{} from {name}",
                selection.paths.len(),
                if selection.paths.len() == 1 { "" } else { "s" }
            ),
            (None, _) => format!("Extracting {name}"),
        }
    }
}

/// Where files opened from archives are extracted to.
fn open_dir() -> PathBuf {
    // Other apps can only be handed files in shared storage.
    #[cfg(target_os = "android")]
    if let Some(dir) = crate::android::media_dir() {
        return dir.join("opened");
    }
    directories::BaseDirs::new()
        .map_or_else(std::env::temp_dir, |dirs| dirs.cache_dir().to_path_buf())
        .join("egui-files")
        .join("opened")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// unrar asks GCC which SIMD the CPU has, which links GCC's CPU detection
    /// from the static libgcc.a. On distros that build GCC for newer CPUs
    /// (CachyOS: x86-64-v3) that code itself uses BMI2 and crashed the app at
    /// startup on older CPUs; patches/unrar_sys/cpu_supports.hpp avoids it.
    /// This binary links unrar too, so it must not contain it either.
    #[test]
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    fn gcc_cpu_detection_is_not_linked() {
        let exe = disk::read(std::env::current_exe().unwrap()).unwrap();
        // `__cpu_indicator_init`, spelled backwards so this test doesn't put
        // the name in the binary itself.
        let needle: Vec<u8> = b"tini_rotacidni_upc__".iter().rev().copied().collect();
        assert!(
            memchr::memmem::find(&exe, &needle).is_none(),
            "GCC's CPU detection is linked in; it may need a newer CPU than the \
             binary targets (see patches/unrar_sys/cpu_supports.hpp)"
        );
    }

    #[test]
    fn detects_formats_and_stems() {
        let cases = [
            ("a.tar.gz", Some(Format::TarGz), "a"),
            ("A.TGZ", Some(Format::TarGz), "A"),
            ("pics.zip", Some(Format::Zip), "pics"),
            ("x.y.7z", Some(Format::SevenZ), "x.y"),
            ("notes.txt.gz", Some(Format::Gz), "notes.txt"),
            ("movie.part01.rar", Some(Format::Rar), "movie"),
            (".zip", None, ".zip"),
            ("readme.md", None, "readme.md"),
        ];
        for (name, format, expected_stem) in cases {
            assert_eq!(Format::detect(Path::new(name)), format, "{name}");
            assert_eq!(stem(Path::new(name)), expected_stem, "{name}");
        }
    }

    #[test]
    fn rejects_escaping_names() {
        assert_eq!(safe_relative("a/./b/"), Some(PathBuf::from("a/b")));
        assert_eq!(
            safe_relative("/etc/passwd"),
            Some(PathBuf::from("etc/passwd"))
        );
        assert_eq!(safe_relative("a/../../b"), None);
        assert_eq!(safe_relative(".."), None);
        assert_eq!(safe_relative("./"), None);
    }

    #[test]
    fn selections_strip_their_common_parent() {
        let s = Selection::new(vec!["docs/a/x.txt".into(), "docs/b".into()]);
        assert_eq!(s.base, "docs/");
        assert_eq!(s.map("docs/a/x.txt").as_deref(), Some("a/x.txt"));
        assert_eq!(s.map("docs/b/y").as_deref(), Some("b/y"));
        assert_eq!(s.map("docs/bb"), None);
        assert_eq!(s.map("docs/a/z.txt"), None);
        assert_eq!(Selection::new(vec!["top".into()]).base, "");
        assert_eq!(
            Selection::new(vec!["a/b/c".into()]).map("a/b/c").as_deref(),
            Some("c")
        );
    }
}
