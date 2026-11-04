//! Reading a file for editing and writing it back the way it was: same line
//! endings, byte order mark, permissions and owner, replaced atomically.

use std::{
    io::{self, Write},
    path::{Path, PathBuf},
    time::SystemTime,
};

use crate::fs::human_size;

/// Bigger files are refused: egui lays out the whole text on every change.
pub const MAX_EDIT_BYTES: u64 = 10 * 1024 * 1024;

const BOM: &str = "\u{feff}";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineEnding {
    Lf,
    /// Shown as `\n` and written back as `\r\n`.
    Crlf,
    /// Both kinds: left exactly as they are.
    Mixed,
}

impl LineEnding {
    pub fn label(self) -> &'static str {
        match self {
            Self::Lf => "LF",
            Self::Crlf => "CRLF",
            Self::Mixed => "Mixed line endings",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadOnly {
    /// We can't write to it.
    Permissions,
    /// Saving the lossily decoded text would change the file.
    NotUtf8,
}

/// Identifies a version of the file on disk, to notice other programs changing it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp {
    modified: Option<SystemTime>,
    len: u64,
}

/// The file's current stamp, or `None` if it's gone.
pub fn stamp(path: &Path) -> Option<Stamp> {
    let meta = disk::metadata(path).ok()?;
    Some(Stamp {
        modified: meta.modified().ok(),
        len: meta.len(),
    })
}

pub struct Loaded {
    pub text: String,
    pub line_ending: LineEnding,
    pub bom: bool,
    pub read_only: Option<ReadOnly>,
    pub stamp: Option<Stamp>,
}

#[derive(Debug)]
pub enum LoadError {
    TooLarge(u64),
    Binary,
    Io(String),
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge(size) => write!(
                f,
                "This file is too large to edit here ({}; the limit is {}).",
                human_size(*size),
                human_size(MAX_EDIT_BYTES)
            ),
            Self::Binary => write!(f, "This looks like a binary file."),
            Self::Io(err) => write!(f, "Could not read the file: {err}"),
        }
    }
}

pub fn load(path: &Path) -> Result<Loaded, LoadError> {
    let io = |err: io::Error| LoadError::Io(err.to_string());
    let stamp = stamp(path);
    let meta = disk::metadata(path).map_err(io)?;
    if meta.is_dir() {
        return Err(LoadError::Io("it is a folder".into()));
    }
    if meta.len() > MAX_EDIT_BYTES {
        return Err(LoadError::TooLarge(meta.len()));
    }
    let bytes = disk::read(path).map_err(io)?;
    if bytes.contains(&0) {
        return Err(LoadError::Binary);
    }
    let (text, utf8) = match String::from_utf8(bytes) {
        Ok(text) => (text, true),
        Err(err) => (String::from_utf8_lossy(err.as_bytes()).into_owned(), false),
    };
    let (text, bom) = match text.strip_prefix(BOM) {
        Some(rest) => (rest.to_owned(), true),
        None => (text, false),
    };
    let crlf = text.matches("\r\n").count();
    let lf = text.matches('\n').count();
    let (text, line_ending) = if crlf == 0 {
        (text, LineEnding::Lf)
    } else if crlf == lf {
        (text.replace("\r\n", "\n"), LineEnding::Crlf)
    } else {
        (text, LineEnding::Mixed)
    };
    let read_only = if !utf8 {
        Some(ReadOnly::NotUtf8)
    } else if disk::OpenOptions::new().write(true).open(path).is_err() {
        Some(ReadOnly::Permissions)
    } else {
        None
    };
    Ok(Loaded {
        text,
        line_ending,
        bom,
        read_only,
        stamp,
    })
}

/// The file's bytes for `text`.
pub fn encode(text: &str, line_ending: LineEnding, bom: bool) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(text.len() + text.len() / 32 + BOM.len());
    if bom {
        bytes.extend_from_slice(BOM.as_bytes());
    }
    match line_ending {
        LineEnding::Crlf => {
            for (index, line) in text.split('\n').enumerate() {
                if index > 0 {
                    bytes.extend_from_slice(b"\r\n");
                }
                bytes.extend_from_slice(line.as_bytes());
            }
        }
        LineEnding::Lf | LineEnding::Mixed => bytes.extend_from_slice(text.as_bytes()),
    }
    bytes
}

/// Writes `text` to `path`. The new contents go to a temporary file that
/// then replaces the old one, so a crash never leaves a half-written file;
/// a symlink is followed so the link itself stays. Files with other hard
/// links, and folders we can't create files in, are overwritten in place.
pub fn save(path: &Path, text: &str, line_ending: LineEnding, bom: bool) -> io::Result<Stamp> {
    write_atomic(path, &encode(text, line_ending, bom))
}

/// Puts these bytes in the file, as [`save`] does.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<Stamp> {
    let target = disk::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let meta = disk::metadata(&target).ok();
    #[cfg(unix)]
    let hard_linked = meta.as_ref().is_some_and(|m| {
        use std::os::unix::fs::MetadataExt;
        m.nlink() > 1
    });
    #[cfg(not(unix))]
    let hard_linked = false;

    let replaced = match &meta {
        Some(meta) if !hard_linked => replace(&target, bytes, meta),
        _ => Err(io::ErrorKind::Unsupported.into()),
    };
    if replaced.is_err() {
        let mut file = disk::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&target)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    stamp(&target).ok_or_else(|| io::ErrorKind::NotFound.into())
}

fn temp_path(target: &Path) -> Option<PathBuf> {
    let name = target.file_name()?.to_string_lossy();
    let nanos = disk::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    Some(target.with_file_name(format!(".{name}.{}-{nanos}.tmp", disk::process_id())))
}

fn replace(target: &Path, bytes: &[u8], meta: &disk::Metadata) -> io::Result<()> {
    let temp = temp_path(target).ok_or(io::ErrorKind::InvalidInput)?;
    let result = (|| {
        let mut file = disk::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.set_permissions(meta.permissions())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let mine = disk::metadata(&temp)?;
            if (mine.uid(), mine.gid()) != (meta.uid(), meta.gid()) {
                // Only works as root (or for our own groups); otherwise the
                // file would change owner, so don't replace it.
                std::os::unix::fs::fchown(&file, Some(meta.uid()), Some(meta.gid()))?;
            }
        }
        file.sync_all()?;
        drop(file);
        disk::rename(&temp, target)
    })();
    if result.is_err() {
        let _ = disk::remove_file(&temp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("egui-files-doc-{name}-{}", std::process::id()));
        let _ = disk::remove_dir_all(&dir);
        disk::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn round_trips_line_endings_and_bom() {
        let dir = dir("endings");
        for (name, bytes) in [
            ("lf.txt", &b"a\nb\n"[..]),
            ("crlf.txt", b"a\r\nb\r\n"),
            ("mixed.txt", b"a\r\nb\n"),
            ("bom.txt", b"\xef\xbb\xbfa\r\nb"),
            ("empty.txt", b""),
        ] {
            let path = dir.join(name);
            disk::write(&path, bytes).unwrap();
            let loaded = load(&path).unwrap();
            assert!(!loaded.text.contains("\r\n") || loaded.line_ending == LineEnding::Mixed);
            save(&path, &loaded.text, loaded.line_ending, loaded.bom).unwrap();
            assert_eq!(disk::read(&path).unwrap(), bytes, "{name}");
        }
        let loaded = load(&dir.join("crlf.txt")).unwrap();
        assert_eq!(
            (loaded.text.as_str(), loaded.line_ending),
            ("a\nb\n", LineEnding::Crlf)
        );
        disk::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn refuses_binary_and_flags_non_utf8() {
        let dir = dir("kinds");
        disk::write(dir.join("bin"), b"ab\0cd").unwrap();
        assert!(matches!(load(&dir.join("bin")), Err(LoadError::Binary)));
        disk::write(dir.join("latin1"), b"caf\xe9\n").unwrap();
        let loaded = load(&dir.join("latin1")).unwrap();
        assert_eq!(loaded.read_only, Some(ReadOnly::NotUtf8));
        assert_eq!(loaded.text, "caf\u{fffd}\n");
        disk::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn saving_keeps_permissions_symlinks_and_hard_links() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let dir = dir("links");
        let real = dir.join("real.sh");
        disk::write(&real, "old").unwrap();
        disk::set_permissions(&real, disk::Permissions::from_mode(0o750)).unwrap();
        let link = dir.join("link.sh");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        save(&link, "new", LineEnding::Lf, false).unwrap();
        assert!(
            disk::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(disk::read_to_string(&real).unwrap(), "new");
        assert_eq!(
            disk::metadata(&real).unwrap().permissions().mode() & 0o777,
            0o750
        );
        // No temporary files left behind.
        assert_eq!(disk::read_dir(&dir).unwrap().count(), 2);

        let hard = dir.join("hard.sh");
        disk::hard_link(&real, &hard).unwrap();
        save(&hard, "newer", LineEnding::Lf, false).unwrap();
        assert_eq!(disk::read_to_string(&real).unwrap(), "newer");
        assert_eq!(
            disk::metadata(&real).unwrap().ino(),
            disk::metadata(&hard).unwrap().ino()
        );
        disk::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn stamps_notice_changes() {
        let dir = dir("stamps");
        let path = dir.join("f");
        disk::write(&path, "a").unwrap();
        let first = stamp(&path);
        let saved = save(&path, "abc", LineEnding::Lf, false).unwrap();
        assert_ne!(first, Some(saved));
        assert_eq!(stamp(&path), Some(saved));
        disk::remove_file(&path).unwrap();
        assert_eq!(stamp(&path), None);
        disk::remove_dir_all(dir).unwrap();
    }
}
