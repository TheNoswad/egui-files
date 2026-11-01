//! What is known about one file or folder on disk.

use std::{
    fs,
    path::{Path, PathBuf},
    time::SystemTime,
};

use super::platform;

/// One item's metadata. Fields a platform doesn't have stay `None`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Item {
    pub path: PathBuf,
    pub name: String,
    pub exists: bool,
    pub is_dir: bool,
    pub is_symlink: bool,
    /// Where a symlink points, as written in the link.
    pub link_target: Option<PathBuf>,
    /// A symlink whose target doesn't exist.
    pub broken: bool,
    /// A file's length (a folder's contents are counted separately).
    pub size: u64,
    /// Space taken on disk (Unix).
    pub allocated: Option<u64>,
    pub modified: Option<SystemTime>,
    pub accessed: Option<SystemTime>,
    pub created: Option<SystemTime>,
    /// When the metadata last changed (Unix ctime).
    pub changed: Option<SystemTime>,
    pub readonly: bool,
    /// Permission bits, `0o7777` (Unix).
    pub mode: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub owner: Option<String>,
    pub group: Option<String>,
    /// File attributes (Windows).
    pub attributes: Option<u32>,
    pub inode: Option<u64>,
    pub links: Option<u64>,
    pub device: Option<u64>,
}

impl Item {
    pub fn load(path: &Path) -> Self {
        let name = crate::fs::display_name(path);
        let mut item = Item {
            path: path.to_path_buf(),
            name,
            ..Item::default()
        };
        let Ok(link_meta) = fs::symlink_metadata(path) else {
            return item;
        };
        item.exists = true;
        item.is_symlink = link_meta.file_type().is_symlink();
        let meta = if item.is_symlink {
            item.link_target = fs::read_link(path).ok();
            match fs::metadata(path) {
                Ok(meta) => meta,
                Err(_) => {
                    item.broken = true;
                    link_meta
                }
            }
        } else {
            link_meta
        };
        item.is_dir = meta.is_dir();
        item.size = if meta.is_file() { meta.len() } else { 0 };
        item.modified = meta.modified().ok();
        item.accessed = meta.accessed().ok();
        item.created = meta.created().ok();
        item.readonly = meta.permissions().readonly();
        platform::fill(&mut item, &meta);
        item
    }

    /// What the item is, in words.
    pub fn kind(&self) -> String {
        if !self.exists {
            return "Missing".to_owned();
        }
        if self.is_symlink {
            let to = if self.broken {
                "broken"
            } else if self.is_dir {
                "folder"
            } else {
                "file"
            };
            return format!("Link ({to})");
        }
        let entry = crate::fs::Entry {
            path: self.path.clone(),
            name: self.name.clone(),
            is_dir: self.is_dir,
            is_symlink: false,
            hidden: false,
            executable: false,
            size: self.size,
            folder_size: None,
            modified: None,
            created: None,
        };
        entry.kind()
    }
}
