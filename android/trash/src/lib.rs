//! The parts of the `trash` crate's API that egui-files uses, for Android,
//! where the crate doesn't build.
//!
//! Android has no trash for files in shared storage, so this keeps one per
//! storage volume, in the freedesktop.org layout the desktop uses: a hidden
//! `.Trash` folder at the volume's root holding the items in `files/` and a
//! `.trashinfo` file for each in `info/` saying where it came from and when.
//! Keeping it on the same volume makes trashing a rename, however big.

// clippy.toml's disallowed items fail only in the browser, and natively the
// replacements are the very same items.
#![cfg_attr(
    not(target_arch = "wasm32"),
    allow(clippy::disallowed_methods, clippy::disallowed_types)
)]

use std::{
    borrow::Borrow,
    ffi::OsString,
    fmt, io,
    path::{Path, PathBuf},
};

use chrono::{Local, NaiveDateTime, TimeZone};
use percent_encoding::{AsciiSet, CONTROLS, percent_decode_str, utf8_percent_encode};

#[derive(Debug)]
pub enum Error {
    Unknown {
        description: String,
    },
    RestoreCollision {
        path: PathBuf,
        remaining_items: Vec<TrashItem>,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown { description } => f.write_str(description),
            Self::RestoreCollision { path, .. } => {
                write!(f, "{} already exists", path.display())
            }
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Self {
        Self::Unknown {
            description: err.to_string(),
        }
    }
}

fn error(description: impl Into<String>) -> Error {
    Error::Unknown {
        description: description.into(),
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TrashItem {
    /// The item's `.trashinfo` file.
    pub id: OsString,
    pub name: OsString,
    pub original_parent: PathBuf,
    /// Seconds since the Unix epoch.
    pub time_deleted: i64,
}

impl TrashItem {
    pub fn original_path(&self) -> PathBuf {
        self.original_parent.join(&self.name)
    }

    /// Where its contents are kept: `<trash>/files/<name>` for
    /// `<trash>/info/<name>.trashinfo`.
    fn stored(&self) -> Option<PathBuf> {
        let info = Path::new(&self.id);
        Some(
            info.parent()?
                .parent()?
                .join("files")
                .join(info.file_stem()?),
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TrashItemSize {
    Bytes(u64),
    Entries(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TrashItemMetadata {
    pub size: TrashItemSize,
}

/// The internal shared storage.
#[cfg(not(target_arch = "wasm32"))]
const INTERNAL: &str = "/storage/emulated/0";
/// The browser's files, in memory, are one volume.
#[cfg(target_arch = "wasm32")]
const INTERNAL: &str = "/";

/// The trash folder at a volume's root.
const TRASH: &str = ".Trash";

/// The roots of the storage volumes: the internal shared storage, then SD
/// cards and USB drives, mounted under /storage by their ids.
fn volumes() -> Vec<PathBuf> {
    let mut roots = vec![PathBuf::from(INTERNAL)];
    if let Ok(read) = disk::read_dir("/storage") {
        roots.extend(read.flatten().map(|e| e.path()).filter(|p| {
            !matches!(
                p.file_name().and_then(|n| n.to_str()),
                Some("emulated" | "self")
            )
        }));
    }
    roots
}

/// The trash for the volume, of `roots`, that `path` is on.
fn trash_for(path: &Path, roots: &[PathBuf]) -> Option<PathBuf> {
    roots
        .iter()
        .filter(|root| path.starts_with(root) && path != root.as_path())
        .max_by_key(|root| root.components().count())
        .map(|root| root.join(TRASH))
}

/// The folder whose changes mean the internal storage's trash changed.
pub fn watched_dir() -> PathBuf {
    Path::new(INTERNAL).join(TRASH).join("info")
}

/// What `Path=` may hold unescaped: the spec escapes as URIs do.
const PATH_ESCAPE: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}');

const DATE_FORMAT: &str = "%Y-%m-%dT%H:%M:%S";

pub fn delete<T: AsRef<Path>>(path: T) -> Result<(), Error> {
    delete_in(path.as_ref(), &volumes())
}

fn delete_in(path: &Path, roots: &[PathBuf]) -> Result<(), Error> {
    // Through symlinks such as /sdcard, but not the item's own.
    let parent = disk::canonicalize(path.parent().ok_or_else(|| error("can't trash the root"))?)?;
    let name = path
        .file_name()
        .ok_or_else(|| error("can't trash the root"))?;
    let path = parent.join(name);
    disk::symlink_metadata(&path)?;
    let trash = trash_for(&path, roots)
        .ok_or_else(|| error("only files in shared storage or on SD cards can be trashed"))?;
    if path.starts_with(&trash) {
        return Err(error("it's already in the trash"));
    }
    let (files, info) = (trash.join("files"), trash.join("info"));
    disk::create_dir_all(&files)?;
    disk::create_dir_all(&info)?;

    let path_text = path
        .to_str()
        .ok_or_else(|| error("the name isn't valid UTF-8"))?;
    let contents = format!(
        "[Trash Info]\nPath={}\nDeletionDate={}\n",
        utf8_percent_encode(path_text, PATH_ESCAPE),
        Local::now().format(DATE_FORMAT),
    );
    // Claims a free name by creating its info file, as the spec has it.
    let name = name.to_string_lossy();
    for n in 1u32.. {
        let stored = if n == 1 {
            name.to_string()
        } else {
            format!("{name}.{n}")
        };
        let info_path = info.join(format!("{stored}.trashinfo"));
        match disk::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&info_path)
        {
            Ok(mut file) => {
                use io::Write;
                let moved = file
                    .write_all(contents.as_bytes())
                    .and_then(|()| disk::rename(&path, files.join(&stored)));
                if let Err(err) = moved {
                    let _ = disk::remove_file(&info_path);
                    return Err(err.into());
                }
                return Ok(());
            }
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err.into()),
        }
    }
    unreachable!("some name is free")
}

/// Reads one `.trashinfo` file.
fn read_info(info_path: &Path) -> Option<TrashItem> {
    let text = disk::read_to_string(info_path).ok()?;
    let mut original = None;
    let mut deleted = None;
    for line in text.lines() {
        if let Some(path) = line.strip_prefix("Path=") {
            original = Some(PathBuf::from(
                percent_decode_str(path).decode_utf8_lossy().into_owned(),
            ));
        } else if let Some(date) = line.strip_prefix("DeletionDate=") {
            deleted = NaiveDateTime::parse_from_str(date, DATE_FORMAT)
                .ok()
                .and_then(|date| Local.from_local_datetime(&date).earliest())
                .map(|date| date.timestamp());
        }
    }
    let original = original?;
    Some(TrashItem {
        id: info_path.as_os_str().to_owned(),
        name: original.file_name()?.to_owned(),
        original_parent: original.parent()?.to_path_buf(),
        time_deleted: deleted.unwrap_or(0),
    })
}

fn list_in(roots: &[PathBuf]) -> Vec<TrashItem> {
    let mut items = Vec::new();
    for root in roots {
        let Ok(read) = disk::read_dir(root.join(TRASH).join("info")) else {
            continue;
        };
        items.extend(
            read.flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "trashinfo"))
                .filter_map(|p| read_info(&p))
                // Info without the item it describes is left over.
                .filter(|item| {
                    item.stored()
                        .is_some_and(|s| disk::symlink_metadata(s).is_ok())
                }),
        );
    }
    items
}

/// Removes a file, or a folder and everything in it.
fn remove(path: &Path) -> io::Result<()> {
    if disk::symlink_metadata(path)?.is_dir() {
        disk::remove_dir_all(path)
    } else {
        disk::remove_file(path)
    }
}

pub mod os_limited {
    use super::*;

    pub fn list() -> Result<Vec<TrashItem>, Error> {
        Ok(list_in(&volumes()))
    }

    pub fn metadata(item: &TrashItem) -> Result<TrashItemMetadata, Error> {
        let stored = item.stored().ok_or_else(|| error("not a trashed item"))?;
        let size = if disk::symlink_metadata(&stored)?.is_dir() {
            TrashItemSize::Entries(disk::read_dir(&stored)?.count())
        } else {
            TrashItemSize::Bytes(disk::symlink_metadata(&stored)?.len())
        };
        Ok(TrashItemMetadata { size })
    }

    pub fn purge_all<I>(items: I) -> Result<(), Error>
    where
        I: IntoIterator,
        <I as IntoIterator>::Item: Borrow<TrashItem>,
    {
        for item in items {
            let item = item.borrow();
            if let Some(stored) = item.stored() {
                match remove(&stored) {
                    Err(err) if err.kind() != io::ErrorKind::NotFound => return Err(err.into()),
                    _ => {}
                }
            }
            disk::remove_file(&item.id)?;
        }
        Ok(())
    }

    pub fn restore_all<I>(items: I) -> Result<(), Error>
    where
        I: IntoIterator<Item = TrashItem>,
    {
        let mut items: Vec<TrashItem> = items.into_iter().collect();
        while !items.is_empty() {
            let item = items.remove(0);
            let original = item.original_path();
            if disk::symlink_metadata(&original).is_ok() {
                items.insert(0, item);
                return Err(Error::RestoreCollision {
                    path: original,
                    remaining_items: items,
                });
            }
            let stored = item.stored().ok_or_else(|| error("not a trashed item"))?;
            disk::create_dir_all(&item.original_parent)?;
            disk::rename(&stored, &original)?;
            disk::remove_file(&item.id)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("trash-android-{name}-{}", std::process::id()));
        let _ = disk::remove_dir_all(&dir);
        disk::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    #[test]
    fn trash_list_restore_and_purge() {
        let root = scratch("cycle");
        let roots = [root.clone()];
        let docs = root.join("Documents");
        disk::create_dir_all(&docs).unwrap();
        let file = docs.join("a b%.txt");
        disk::write(&file, "hi").unwrap();

        delete_in(&file, &roots).unwrap();
        assert!(!file.exists());
        // Another with the same name gets a name of its own.
        disk::write(&file, "again").unwrap();
        delete_in(&file, &roots).unwrap();

        let mut items = list_in(&roots);
        assert_eq!(items.len(), 2);
        // The first trashed kept its name; the second got ".2".
        items.sort_by_key(|i| i.id.len());
        assert_eq!(items[0].original_path(), file);
        assert_eq!(items[1].original_path(), file);
        assert!(items[0].time_deleted > 0);
        assert_eq!(
            os_limited::metadata(&items[0]).unwrap().size,
            TrashItemSize::Bytes(2)
        );

        os_limited::restore_all([items[0].clone()]).unwrap();
        assert_eq!(disk::read_to_string(&file).unwrap(), "hi");
        // The other can't go back over it.
        assert!(matches!(
            os_limited::restore_all([items[1].clone()]),
            Err(Error::RestoreCollision { .. })
        ));
        os_limited::purge_all([&items[1]]).unwrap();
        assert!(list_in(&roots).is_empty());
        disk::remove_dir_all(root).unwrap();
    }

    #[test]
    fn folders_are_trashed_whole() {
        let root = scratch("folder");
        let roots = [root.clone()];
        let folder = root.join("photos");
        disk::create_dir_all(folder.join("inner")).unwrap();
        disk::write(folder.join("x.jpg"), "x").unwrap();
        delete_in(&folder, &roots).unwrap();
        let items = list_in(&roots);
        assert_eq!(
            os_limited::metadata(&items[0]).unwrap().size,
            TrashItemSize::Entries(2)
        );
        os_limited::purge_all(&items).unwrap();
        assert!(!root.join(TRASH).join("files").join("photos").exists());
        disk::remove_dir_all(root).unwrap();
    }

    #[test]
    fn only_inside_a_volume() {
        let root = scratch("outside");
        let elsewhere = scratch("elsewhere");
        let file = elsewhere.join("f");
        disk::write(&file, "").unwrap();
        assert!(delete_in(&file, std::slice::from_ref(&root)).is_err());
        assert!(file.exists());
        // Nor the trash itself.
        let trash = root.join(TRASH).join("files");
        disk::create_dir_all(&trash).unwrap();
        disk::write(trash.join("g"), "").unwrap();
        assert!(delete_in(&trash.join("g"), std::slice::from_ref(&root)).is_err());
        disk::remove_dir_all(root).unwrap();
        disk::remove_dir_all(elsewhere).unwrap();
    }
}
