//! Reading a file for editing and writing it back the way it was: same line
//! endings, byte order mark, encoding, permissions and owner, replaced
//! atomically.

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

/// How a file's text is stored. Only tables are read in anything but UTF-8,
/// as spreadsheets are what write those files; see [`decode`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoding {
    Utf8,
    /// Always with its byte order mark, which is how it's recognized.
    Utf16Le,
    Utf16Be,
    Windows1252,
}

impl Encoding {
    pub fn label(self, bom: bool) -> &'static str {
        match self {
            Self::Utf8 if bom => "UTF-8 with BOM",
            Self::Utf8 => "UTF-8",
            Self::Utf16Le => "UTF-16 LE",
            Self::Utf16Be => "UTF-16 BE",
            Self::Windows1252 => "Windows-1252",
        }
    }
}

/// Text read from a file, with its byte order mark, if UTF-8, still on it.
pub struct Decoded {
    pub text: String,
    pub encoding: Encoding,
    /// Some bytes are lost to `\u{fffd}`, so saving would change the file.
    pub lossy: bool,
}

/// Works out a table's encoding: a byte order mark says, else it's UTF-8 if
/// it's valid as that, else Windows-1252, which is what Excel on Windows
/// writes (every byte means something there, so it round-trips). With
/// `partial`, `bytes` are cut off from the rest of the file, maybe mid-character.
pub fn decode(bytes: &[u8], partial: bool) -> Decoded {
    let utf16 = |bytes: &[u8], encoding, decoder: &'static encoding_rs::Encoding| {
        let (text, lossy) = decoder.decode_without_bom_handling(&bytes[2..]);
        Decoded {
            text: text.into_owned(),
            encoding,
            lossy: lossy && !partial,
        }
    };
    if bytes.starts_with(&[0xff, 0xfe]) {
        return utf16(bytes, Encoding::Utf16Le, encoding_rs::UTF_16LE);
    }
    if bytes.starts_with(&[0xfe, 0xff]) {
        return utf16(bytes, Encoding::Utf16Be, encoding_rs::UTF_16BE);
    }
    match std::str::from_utf8(bytes) {
        Ok(text) => Decoded {
            text: text.to_owned(),
            encoding: Encoding::Utf8,
            lossy: false,
        },
        // Valid but for a character cut in half at the end.
        Err(err) if partial && err.error_len().is_none() => Decoded {
            text: String::from_utf8_lossy(&bytes[..err.valid_up_to()]).into_owned(),
            encoding: Encoding::Utf8,
            lossy: false,
        },
        Err(_) => Decoded {
            text: encoding_rs::WINDOWS_1252
                .decode_without_bom_handling(bytes)
                .0
                .into_owned(),
            encoding: Encoding::Windows1252,
            lossy: false,
        },
    }
}

/// The first character of `text` that `encoding` can't store.
pub fn unrepresentable(text: &str, encoding: Encoding) -> Option<char> {
    if encoding != Encoding::Windows1252 {
        return None;
    }
    let mut buffer = [0; 4];
    text.chars().filter(|c| !c.is_ascii()).find(|c| {
        encoding_rs::WINDOWS_1252
            .encode(c.encode_utf8(&mut buffer))
            .2
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadOnly {
    /// We can't write to it.
    Permissions,
    /// Saving the lossily decoded text would change the file.
    Lossy,
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
    pub encoding: Encoding,
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
    let (text, encoding, lossy) = if super::table::is_table_file(path) {
        let decoded = decode(&bytes, false);
        if decoded.text.contains('\0') {
            return Err(LoadError::Binary);
        }
        (decoded.text, decoded.encoding, decoded.lossy)
    } else {
        if bytes.contains(&0) {
            return Err(LoadError::Binary);
        }
        match String::from_utf8(bytes) {
            Ok(text) => (text, Encoding::Utf8, false),
            Err(err) => (
                String::from_utf8_lossy(err.as_bytes()).into_owned(),
                Encoding::Utf8,
                true,
            ),
        }
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
    let read_only = if lossy {
        Some(ReadOnly::Lossy)
    } else if disk::OpenOptions::new().write(true).open(path).is_err() {
        Some(ReadOnly::Permissions)
    } else {
        None
    };
    Ok(Loaded {
        text,
        line_ending,
        bom,
        encoding,
        read_only,
        stamp,
    })
}

/// The file's bytes for `text`; an error if `encoding` can't store all of it.
pub fn encode(
    text: &str,
    line_ending: LineEnding,
    bom: bool,
    encoding: Encoding,
) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::with_capacity(text.len() + text.len() / 32 + BOM.len());
    if bom && encoding == Encoding::Utf8 {
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
    // Only valid UTF-8 went in, so nothing's lost reading it as text.
    Ok(match encoding {
        Encoding::Utf8 => bytes,
        Encoding::Utf16Le => [0xff, 0xfe]
            .into_iter()
            .chain(
                String::from_utf8_lossy(&bytes)
                    .encode_utf16()
                    .flat_map(u16::to_le_bytes),
            )
            .collect(),
        Encoding::Utf16Be => [0xfe, 0xff]
            .into_iter()
            .chain(
                String::from_utf8_lossy(&bytes)
                    .encode_utf16()
                    .flat_map(u16::to_be_bytes),
            )
            .collect(),
        Encoding::Windows1252 => {
            let utf8 = String::from_utf8_lossy(&bytes);
            let (encoded, _, unmappable) = encoding_rs::WINDOWS_1252.encode(&utf8);
            if unmappable {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "the text has characters Windows-1252 can't store",
                ));
            }
            encoded.into_owned()
        }
    })
}

/// Writes `text` to `path`. The new contents go to a temporary file that
/// then replaces the old one, so a crash never leaves a half-written file;
/// a symlink is followed so the link itself stays. Files with other hard
/// links, and folders we can't create files in, are overwritten in place.
pub fn save(
    path: &Path,
    text: &str,
    line_ending: LineEnding,
    bom: bool,
    encoding: Encoding,
) -> io::Result<Stamp> {
    let bytes = encode(text, line_ending, bom, encoding)?;
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
        Some(meta) if !hard_linked => replace(&target, &bytes, meta),
        _ => Err(io::ErrorKind::Unsupported.into()),
    };
    if replaced.is_err() {
        let mut file = disk::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&target)?;
        file.write_all(&bytes)?;
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
            save(
                &path,
                &loaded.text,
                loaded.line_ending,
                loaded.bom,
                loaded.encoding,
            )
            .unwrap();
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
        assert_eq!(loaded.read_only, Some(ReadOnly::Lossy));
        assert_eq!(loaded.text, "caf\u{fffd}\n");
        disk::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn tables_round_trip_in_their_encoding() {
        let dir = dir("encodings");
        let utf16 = |text: &str, be: bool| -> Vec<u8> {
            let bom: [u8; 2] = if be { [0xfe, 0xff] } else { [0xff, 0xfe] };
            bom.into_iter()
                .chain(text.encode_utf16().flat_map(|unit| {
                    if be {
                        unit.to_be_bytes()
                    } else {
                        unit.to_le_bytes()
                    }
                }))
                .collect()
        };
        for (name, bytes, encoding, text) in [
            (
                "1252.csv",
                b"caf\xe9,\x80\r\nb,\x81\r\n".to_vec(),
                Encoding::Windows1252,
                "café,€\nb,\u{81}\n",
            ),
            (
                "le.csv",
                utf16("é,日本\r\nb,😀\r\n", false),
                Encoding::Utf16Le,
                "é,日本\nb,😀\n",
            ),
            (
                "be.csv",
                utf16("é,日本\n", true),
                Encoding::Utf16Be,
                "é,日本\n",
            ),
            (
                "utf8.csv",
                "\u{feff}é,b\n".as_bytes().to_vec(),
                Encoding::Utf8,
                "é,b\n",
            ),
        ] {
            let path = dir.join(name);
            disk::write(&path, &bytes).unwrap();
            let loaded = load(&path).unwrap();
            assert_eq!(loaded.encoding, encoding, "{name}");
            assert_eq!(loaded.text, text, "{name}");
            assert_eq!(loaded.read_only, None, "{name}");
            save(
                &path,
                &loaded.text,
                loaded.line_ending,
                loaded.bom,
                loaded.encoding,
            )
            .unwrap();
            assert_eq!(disk::read(&path).unwrap(), bytes, "{name}");
        }
        disk::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn only_tables_are_read_in_other_encodings() {
        let dir = dir("only-tables");
        disk::write(dir.join("a.txt"), b"\xff\xfea\0").unwrap();
        assert!(matches!(load(&dir.join("a.txt")), Err(LoadError::Binary)));
        // Not text in any encoding.
        disk::write(dir.join("a.csv"), b"ab\0cd").unwrap();
        assert!(matches!(load(&dir.join("a.csv")), Err(LoadError::Binary)));
        // Half a UTF-16 character can't be written back.
        disk::write(dir.join("b.csv"), b"\xff\xfea\0b").unwrap();
        assert_eq!(
            load(&dir.join("b.csv")).unwrap().read_only,
            Some(ReadOnly::Lossy)
        );
        disk::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn windows_1252_refuses_what_it_cant_store() {
        assert_eq!(unrepresentable("café €", Encoding::Windows1252), None);
        assert_eq!(unrepresentable("a日b本", Encoding::Windows1252), Some('日'));
        assert_eq!(unrepresentable("a日", Encoding::Utf8), None);
        assert_eq!(unrepresentable("a日", Encoding::Utf16Le), None);

        let dir = dir("refuses");
        let path = dir.join("a.csv");
        disk::write(&path, b"old").unwrap();
        let err = save(&path, "日", LineEnding::Lf, false, Encoding::Windows1252).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert_eq!(disk::read(&path).unwrap(), b"old");
        disk::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_partial_read_keeps_its_encoding() {
        // Cut in the middle of "é".
        let decoded = decode(b"a,b\xc3", true);
        assert_eq!(decoded.encoding, Encoding::Utf8);
        assert_eq!(decoded.text, "a,b");
        // Whole, the same bytes aren't UTF-8.
        assert_eq!(decode(b"a,b\xc3", false).encoding, Encoding::Windows1252);
        // Half a UTF-16 character at the end doesn't count against it.
        assert!(!decode(b"\xff\xfea\0b", true).lossy);
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

        save(&link, "new", LineEnding::Lf, false, Encoding::Utf8).unwrap();
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
        save(&hard, "newer", LineEnding::Lf, false, Encoding::Utf8).unwrap();
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
        let saved = save(&path, "abc", LineEnding::Lf, false, Encoding::Utf8).unwrap();
        assert_ne!(first, Some(saved));
        assert_eq!(stamp(&path), Some(saved));
        disk::remove_file(&path).unwrap();
        assert_eq!(stamp(&path), None);
        disk::remove_dir_all(dir).unwrap();
    }
}
