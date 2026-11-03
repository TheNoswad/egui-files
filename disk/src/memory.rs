//! A filesystem kept in memory, for the browser: `std::fs`'s functions and
//! types, with Linux's behavior where they differ by platform.
//!
//! Everything is one tree behind a lock, starting as an empty `/`. Relative
//! paths start at `/`. Folders, files and symlinks are nodes that folders
//! name; a file can be in several (hard links). Its data is dropped once no
//! folder names it and no [`File`] has it open.
//!
//! Every change is noted by path, for [`take_changes`]: the browser can't
//! watch folders, so the app asks instead.
//!
//! A file can also start out somewhere else ([`add_remote`]): it's listed with
//! its size, and its contents are fetched the first time something opens it,
//! through the function given to [`set_remote_loader`], unless they were given
//! to [`fill_remote`] by then.

// Natively it's built only for its tests.
#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{OsStr, OsString},
    io::{self, ErrorKind, Read, Seek, SeekFrom, Write},
    path::{Component, Path, PathBuf},
    sync::{Mutex, MutexGuard, OnceLock, PoisonError},
    time::SystemTime,
};

/// Links followed in one path at most, as Linux allows.
const MAX_SYMLINKS: usize = 40;

type Id = usize;
const ROOT: Id = 0;

/// Fetches a remote file's contents; see [`set_remote_loader`].
pub type RemoteLoader = fn(&str) -> io::Result<Vec<u8>>;

static REMOTE_LOADER: OnceLock<RemoteLoader> = OnceLock::new();

enum Data {
    File(Vec<u8>),
    /// A file whose contents are still at this URL.
    Remote {
        url: String,
        len: u64,
    },
    Dir(BTreeMap<OsString, Id>),
    Symlink(PathBuf),
}

struct Node {
    data: Data,
    readonly: bool,
    /// Folders naming it.
    links: u32,
    /// `File`s open on it.
    open: u32,
    modified: SystemTime,
    accessed: SystemTime,
    created: SystemTime,
}

impl Node {
    fn new(data: Data) -> Self {
        let now = now();
        Self {
            data,
            readonly: false,
            links: 1,
            open: 0,
            modified: now,
            accessed: now,
            created: now,
        }
    }

    fn file_type(&self) -> FileType {
        FileType(match self.data {
            Data::File(_) | Data::Remote { .. } => Kind::File,
            Data::Dir(_) => Kind::Dir,
            Data::Symlink(_) => Kind::Symlink,
        })
    }

    fn metadata(&self) -> Metadata {
        Metadata {
            file_type: self.file_type(),
            len: match &self.data {
                Data::File(bytes) => bytes.len() as u64,
                Data::Remote { len, .. } => *len,
                Data::Dir(_) => 0,
                Data::Symlink(target) => target.as_os_str().len() as u64,
            },
            readonly: self.readonly,
            modified: self.modified,
            accessed: self.accessed,
            created: self.created,
        }
    }
}

struct Tree {
    nodes: Vec<Node>,
    changed: BTreeSet<PathBuf>,
}

static TREE: Mutex<Tree> = Mutex::new(Tree {
    nodes: Vec::new(),
    changed: BTreeSet::new(),
});

fn tree() -> MutexGuard<'static, Tree> {
    let mut tree = TREE.lock().unwrap_or_else(PoisonError::into_inner);
    if tree.nodes.is_empty() {
        tree.nodes.push(Node::new(Data::Dir(BTreeMap::new())));
    }
    tree
}

use crate::now;

/// An error as Linux words it.
fn error(kind: ErrorKind) -> io::Error {
    let message = match kind {
        ErrorKind::NotFound => "No such file or directory",
        ErrorKind::AlreadyExists => "File exists",
        ErrorKind::NotADirectory => "Not a directory",
        ErrorKind::IsADirectory => "Is a directory",
        ErrorKind::DirectoryNotEmpty => "Directory not empty",
        ErrorKind::PermissionDenied => "Permission denied",
        _ => "Invalid argument",
    };
    io::Error::new(kind, message)
}

/// A node found by path, with the path that leads to it without links.
struct Found {
    id: Id,
    path: PathBuf,
}

impl Tree {
    fn node(&self, id: Id) -> &Node {
        &self.nodes[id]
    }

    fn node_mut(&mut self, id: Id) -> &mut Node {
        &mut self.nodes[id]
    }

    fn dir(&self, id: Id) -> io::Result<&BTreeMap<OsString, Id>> {
        match &self.node(id).data {
            Data::Dir(entries) => Ok(entries),
            _ => Err(error(ErrorKind::NotADirectory)),
        }
    }

    fn dir_mut(&mut self, id: Id) -> io::Result<&mut BTreeMap<OsString, Id>> {
        match &mut self.node_mut(id).data {
            Data::Dir(entries) => Ok(entries),
            _ => Err(error(ErrorKind::NotADirectory)),
        }
    }

    /// The node at `path`, following links on the way, and the last one too
    /// if `follow`.
    fn find(&self, path: &Path, follow: bool) -> io::Result<Found> {
        // The folders down to where the walk is, for `..`.
        let mut trail: Vec<(Id, OsString)> = Vec::new();
        // Components still to walk, last first.
        let mut todo: Vec<OsString> = Vec::new();
        push_components(&mut todo, path);
        let mut hops = 0;
        while let Some(name) = todo.pop() {
            if name == ".." {
                trail.pop();
                continue;
            }
            let here = trail.last().map_or(ROOT, |(id, _)| *id);
            let child = *self
                .dir(here)?
                .get(&name)
                .ok_or_else(|| error(ErrorKind::NotFound))?;
            if let Data::Symlink(target) = &self.node(child).data
                && (follow || !todo.is_empty())
            {
                hops += 1;
                if hops > MAX_SYMLINKS {
                    return Err(io::Error::new(
                        ErrorKind::InvalidInput,
                        "Too many levels of symbolic links",
                    ));
                }
                if target.has_root() {
                    trail.clear();
                }
                push_components(&mut todo, target);
                continue;
            }
            trail.push((child, name));
        }
        let mut found = PathBuf::from("/");
        found.extend(trail.iter().map(|(_, name)| name));
        Ok(Found {
            id: trail.last().map_or(ROOT, |(id, _)| *id),
            path: found,
        })
    }

    /// The folder `path` would be in, and its name there.
    fn parent(&self, path: &Path) -> io::Result<(Found, OsString)> {
        let name = path
            .file_name()
            .ok_or_else(|| error(ErrorKind::InvalidInput))?;
        let parent = self.find(path.parent().unwrap_or(Path::new("/")), true)?;
        self.dir(parent.id)?;
        Ok((parent, name.to_owned()))
    }

    /// What `name` in folder `dir` is, without following it.
    fn child(&self, dir: Id, name: &OsStr) -> io::Result<Id> {
        self.dir(dir)?
            .get(name)
            .copied()
            .ok_or_else(|| error(ErrorKind::NotFound))
    }

    fn changed(&mut self, path: &Path) {
        self.changed.insert(path.to_owned());
    }

    /// Puts a new node in folder `dir` as `name`.
    fn add(&mut self, dir: Id, name: OsString, data: Data) -> io::Result<Id> {
        if self.dir(dir)?.contains_key(&name) {
            return Err(error(ErrorKind::AlreadyExists));
        }
        let id = self.nodes.len();
        self.nodes.push(Node::new(data));
        self.dir_mut(dir)?.insert(name, id);
        self.node_mut(dir).modified = now();
        Ok(id)
    }

    /// Takes `name` out of folder `dir`, and drops what no longer has a name.
    fn unlink(&mut self, dir: Id, name: &OsStr) -> io::Result<()> {
        let id = self
            .dir_mut(dir)?
            .remove(name)
            .ok_or_else(|| error(ErrorKind::NotFound))?;
        self.node_mut(dir).modified = now();
        self.release(id);
        Ok(())
    }

    fn release(&mut self, id: Id) {
        let node = self.node_mut(id);
        node.links -= 1;
        if node.links > 0 {
            return;
        }
        if let Data::Dir(entries) = &mut node.data {
            for child in std::mem::take(entries).into_values() {
                self.release(child);
            }
        }
        self.free_if_unused(id);
    }

    fn free_if_unused(&mut self, id: Id) {
        let node = self.node_mut(id);
        if node.links == 0 && node.open == 0 {
            node.data = Data::File(Vec::new());
        }
    }

    /// Fetches the contents of a file still at its URL. Failures leave it
    /// there, to try again on the next open.
    fn fetch_remote(&mut self, id: Id) -> io::Result<()> {
        if let Data::Remote { url, .. } = &self.node(id).data {
            let loader = REMOTE_LOADER
                .get()
                .ok_or_else(|| io::Error::other("nothing to fetch remote files with"))?;
            let bytes = loader(url)?;
            self.node_mut(id).data = Data::File(bytes);
        }
        Ok(())
    }

    fn open(&mut self, path: &Path, options: &OpenOptions) -> io::Result<File> {
        let writes = options.write || options.append;
        if !options.read && !writes
            || (options.truncate || options.create || options.create_new) && !writes
        {
            return Err(error(ErrorKind::InvalidInput));
        }
        let (id, path) = match self.find(path, true) {
            Ok(_) if options.create_new => return Err(error(ErrorKind::AlreadyExists)),
            Ok(found) => {
                if writes && options.truncate {
                    // Going over it: no need for what's there.
                    if matches!(self.node(found.id).data, Data::Remote { .. })
                        && !self.node(found.id).readonly
                    {
                        self.node_mut(found.id).data = Data::File(Vec::new());
                    }
                } else {
                    self.fetch_remote(found.id)?;
                }
                let node = self.node_mut(found.id);
                match &mut node.data {
                    Data::Remote { .. } => return Err(error(ErrorKind::PermissionDenied)),
                    Data::Dir(_) if writes => return Err(error(ErrorKind::IsADirectory)),
                    Data::File(_) if writes && node.readonly => {
                        return Err(error(ErrorKind::PermissionDenied));
                    }
                    Data::File(bytes) if options.truncate => {
                        bytes.clear();
                        node.modified = now();
                        self.changed(&found.path);
                    }
                    _ => {}
                }
                (found.id, found.path)
            }
            Err(err)
                if err.kind() == ErrorKind::NotFound && (options.create || options.create_new) =>
            {
                let (parent, name) = self.parent(path)?;
                let path = parent.path.join(&name);
                let id = self.add(parent.id, name, Data::File(Vec::new()))?;
                self.changed(&path);
                (id, path)
            }
            Err(err) => return Err(err),
        };
        self.node_mut(id).open += 1;
        Ok(File {
            id,
            path,
            pos: 0,
            read: options.read,
            write: writes,
            append: options.append,
        })
    }

    fn create_dir(&mut self, path: &Path) -> io::Result<()> {
        let (parent, name) = self.parent(path)?;
        let changed = parent.path.join(&name);
        self.add(parent.id, name, Data::Dir(BTreeMap::new()))?;
        self.changed(&changed);
        Ok(())
    }

    fn remove(&mut self, path: &Path, dir: bool, recursive: bool) -> io::Result<()> {
        let (parent, name) = self.parent(path)?;
        let id = self.child(parent.id, &name)?;
        match &self.node(id).data {
            Data::Dir(_) if !dir => return Err(error(ErrorKind::IsADirectory)),
            Data::Dir(entries) if !recursive && !entries.is_empty() => {
                return Err(error(ErrorKind::DirectoryNotEmpty));
            }
            Data::Dir(_) => {}
            Data::Symlink(_) if recursive => {}
            _ if dir => return Err(error(ErrorKind::NotADirectory)),
            _ => {}
        }
        self.unlink(parent.id, &name)?;
        let path = parent.path.join(name);
        self.changed(&path);
        Ok(())
    }

    fn rename(&mut self, from: &Path, to: &Path) -> io::Result<()> {
        let (source_dir, source_name) = self.parent(from)?;
        let id = self.child(source_dir.id, &source_name)?;
        let (dest_dir, dest_name) = self.parent(to)?;
        let source = source_dir.path.join(&source_name);
        let dest = dest_dir.path.join(&dest_name);
        if source == dest {
            return Ok(());
        }
        let is_dir = matches!(self.node(id).data, Data::Dir(_));
        if is_dir && dest.starts_with(&source) {
            return Err(error(ErrorKind::InvalidInput));
        }
        if let Ok(existing) = self.child(dest_dir.id, &dest_name) {
            match (&self.node(existing).data, is_dir) {
                (Data::Dir(entries), true) if !entries.is_empty() => {
                    return Err(error(ErrorKind::DirectoryNotEmpty));
                }
                (Data::Dir(_), true) => {}
                (_, true) => return Err(error(ErrorKind::NotADirectory)),
                (Data::Dir(_), false) => return Err(error(ErrorKind::IsADirectory)),
                (_, false) => {}
            }
            self.unlink(dest_dir.id, &dest_name)?;
        }
        self.dir_mut(source_dir.id)?.remove(&source_name);
        self.dir_mut(dest_dir.id)?.insert(dest_name, id);
        let now = now();
        self.node_mut(source_dir.id).modified = now;
        self.node_mut(dest_dir.id).modified = now;
        self.changed(&source);
        self.changed(&dest);
        Ok(())
    }

    fn write_at(&mut self, file: &mut File, buf: &[u8]) -> io::Result<usize> {
        let node = self.node_mut(file.id);
        let Data::File(bytes) = &mut node.data else {
            return Err(error(ErrorKind::IsADirectory));
        };
        if file.append {
            file.pos = bytes.len() as u64;
        }
        let start = usize::try_from(file.pos).map_err(|_| error(ErrorKind::InvalidInput))?;
        let end = start + buf.len();
        if bytes.len() < end {
            bytes.resize(end, 0);
        }
        bytes[start..end].copy_from_slice(buf);
        node.modified = now();
        file.pos = end as u64;
        self.changed(&file.path);
        Ok(buf.len())
    }
}

/// The components of `path` that move the walk, pushed so the first pops first.
fn push_components(todo: &mut Vec<OsString>, path: &Path) {
    let start = todo.len();
    for component in path.components() {
        match component {
            Component::Normal(name) => todo.push(name.to_owned()),
            Component::ParentDir => todo.push("..".into()),
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
        }
    }
    todo[start..].reverse();
}

// ---------------------------------------------------------------------------
// std::fs's types

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Kind {
    File,
    Dir,
    Symlink,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FileType(Kind);

impl FileType {
    pub fn is_dir(&self) -> bool {
        self.0 == Kind::Dir
    }

    pub fn is_file(&self) -> bool {
        self.0 == Kind::File
    }

    pub fn is_symlink(&self) -> bool {
        self.0 == Kind::Symlink
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Permissions {
    readonly: bool,
}

impl Permissions {
    pub fn readonly(&self) -> bool {
        self.readonly
    }

    pub fn set_readonly(&mut self, readonly: bool) {
        self.readonly = readonly;
    }
}

#[derive(Clone, Debug)]
pub struct Metadata {
    file_type: FileType,
    len: u64,
    readonly: bool,
    modified: SystemTime,
    accessed: SystemTime,
    created: SystemTime,
}

// As std's, which has no `is_empty`.
#[allow(clippy::len_without_is_empty)]
impl Metadata {
    pub fn file_type(&self) -> FileType {
        self.file_type
    }

    pub fn is_dir(&self) -> bool {
        self.file_type.is_dir()
    }

    pub fn is_file(&self) -> bool {
        self.file_type.is_file()
    }

    pub fn is_symlink(&self) -> bool {
        self.file_type.is_symlink()
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn permissions(&self) -> Permissions {
        Permissions {
            readonly: self.readonly,
        }
    }

    pub fn modified(&self) -> io::Result<SystemTime> {
        Ok(self.modified)
    }

    pub fn accessed(&self) -> io::Result<SystemTime> {
        Ok(self.accessed)
    }

    pub fn created(&self) -> io::Result<SystemTime> {
        Ok(self.created)
    }
}

#[derive(Debug)]
pub struct DirEntry {
    path: PathBuf,
    name: OsString,
    id: Id,
}

impl DirEntry {
    pub fn path(&self) -> PathBuf {
        self.path.clone()
    }

    pub fn file_name(&self) -> OsString {
        self.name.clone()
    }

    /// Of the entry itself, not what a link points to (as std's on Linux).
    pub fn metadata(&self) -> io::Result<Metadata> {
        Ok(tree().node(self.id).metadata())
    }

    pub fn file_type(&self) -> io::Result<FileType> {
        Ok(tree().node(self.id).file_type())
    }
}

/// A folder's entries as they were when it was read.
#[derive(Debug)]
pub struct ReadDir(std::vec::IntoIter<DirEntry>);

impl Iterator for ReadDir {
    type Item = io::Result<DirEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(Ok)
    }
}

#[derive(Clone, Debug, Default)]
pub struct OpenOptions {
    read: bool,
    write: bool,
    append: bool,
    truncate: bool,
    create: bool,
    create_new: bool,
}

impl OpenOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn read(&mut self, read: bool) -> &mut Self {
        self.read = read;
        self
    }

    pub fn write(&mut self, write: bool) -> &mut Self {
        self.write = write;
        self
    }

    pub fn append(&mut self, append: bool) -> &mut Self {
        self.append = append;
        self
    }

    pub fn truncate(&mut self, truncate: bool) -> &mut Self {
        self.truncate = truncate;
        self
    }

    pub fn create(&mut self, create: bool) -> &mut Self {
        self.create = create;
        self
    }

    pub fn create_new(&mut self, create_new: bool) -> &mut Self {
        self.create_new = create_new;
        self
    }

    pub fn open(&self, path: impl AsRef<Path>) -> io::Result<File> {
        tree().open(path.as_ref(), self)
    }
}

#[derive(Debug)]
pub struct File {
    id: Id,
    /// Where it was opened, for noting changes.
    path: PathBuf,
    pos: u64,
    read: bool,
    write: bool,
    append: bool,
}

impl File {
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        OpenOptions::new().read(true).open(path)
    }

    pub fn create(path: impl AsRef<Path>) -> io::Result<Self> {
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
    }

    pub fn create_new(path: impl AsRef<Path>) -> io::Result<Self> {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)
    }

    pub fn options() -> OpenOptions {
        OpenOptions::new()
    }

    pub fn metadata(&self) -> io::Result<Metadata> {
        Ok(tree().node(self.id).metadata())
    }

    pub fn set_len(&self, len: u64) -> io::Result<()> {
        if !self.write {
            return Err(error(ErrorKind::InvalidInput));
        }
        let mut tree = tree();
        let node = tree.node_mut(self.id);
        let Data::File(bytes) = &mut node.data else {
            return Err(error(ErrorKind::IsADirectory));
        };
        bytes.resize(
            usize::try_from(len).map_err(|_| error(ErrorKind::InvalidInput))?,
            0,
        );
        node.modified = now();
        tree.changed(&self.path);
        Ok(())
    }

    pub fn set_modified(&self, modified: SystemTime) -> io::Result<()> {
        tree().node_mut(self.id).modified = modified;
        Ok(())
    }

    pub fn set_permissions(&self, permissions: Permissions) -> io::Result<()> {
        tree().node_mut(self.id).readonly = permissions.readonly;
        Ok(())
    }

    pub fn sync_all(&self) -> io::Result<()> {
        Ok(())
    }

    pub fn sync_data(&self) -> io::Result<()> {
        Ok(())
    }
}

impl Read for File {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if !self.read {
            return Err(io::Error::other("Bad file descriptor"));
        }
        let tree = tree();
        let Data::File(bytes) = &tree.node(self.id).data else {
            return Err(error(ErrorKind::IsADirectory));
        };
        let start = usize::try_from(self.pos)
            .unwrap_or(usize::MAX)
            .min(bytes.len());
        let n = buf.len().min(bytes.len() - start);
        buf[..n].copy_from_slice(&bytes[start..start + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl Write for File {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if !self.write {
            return Err(io::Error::other("Bad file descriptor"));
        }
        tree().write_at(self, buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Seek for File {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let len = tree().node(self.id).metadata().len;
        let pos = match pos {
            SeekFrom::Start(n) => Some(n),
            SeekFrom::End(n) => len.checked_add_signed(n),
            SeekFrom::Current(n) => self.pos.checked_add_signed(n),
        };
        self.pos = pos.ok_or_else(|| error(ErrorKind::InvalidInput))?;
        Ok(self.pos)
    }
}

impl Drop for File {
    fn drop(&mut self) {
        let mut tree = tree();
        tree.node_mut(self.id).open -= 1;
        tree.free_if_unused(self.id);
    }
}

// ---------------------------------------------------------------------------
// std::fs's functions

pub fn metadata(path: impl AsRef<Path>) -> io::Result<Metadata> {
    let tree = tree();
    let found = tree.find(path.as_ref(), true)?;
    Ok(tree.node(found.id).metadata())
}

pub fn symlink_metadata(path: impl AsRef<Path>) -> io::Result<Metadata> {
    let tree = tree();
    let found = tree.find(path.as_ref(), false)?;
    Ok(tree.node(found.id).metadata())
}

pub fn exists(path: impl AsRef<Path>) -> io::Result<bool> {
    match metadata(path) {
        Ok(_) => Ok(true),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err),
    }
}

pub fn read_dir(path: impl AsRef<Path>) -> io::Result<ReadDir> {
    let path = path.as_ref();
    let tree = tree();
    let found = tree.find(path, true)?;
    let entries = tree
        .dir(found.id)?
        .iter()
        .map(|(name, id)| DirEntry {
            path: path.join(name),
            name: name.clone(),
            id: *id,
        })
        .collect::<Vec<_>>();
    Ok(ReadDir(entries.into_iter()))
}

/// Sets how remote files are fetched. Only the first call counts.
pub fn set_remote_loader(loader: RemoteLoader) {
    let _ = REMOTE_LOADER.set(loader);
}

/// Adds a file whose contents are at `url`, to fetch when it's first opened.
pub fn add_remote(
    path: impl AsRef<Path>,
    url: impl Into<String>,
    len: u64,
    modified: SystemTime,
) -> io::Result<()> {
    let mut tree = tree();
    let (parent, name) = tree.parent(path.as_ref())?;
    let path = parent.path.join(&name);
    let data = Data::Remote {
        url: url.into(),
        len,
    };
    let id = tree.add(parent.id, name, data)?;
    tree.node_mut(id).modified = modified;
    tree.changed(&path);
    Ok(())
}

/// The URL of the file at `path`, if its contents are still there.
pub fn remote_url(path: impl AsRef<Path>) -> Option<String> {
    let tree = tree();
    let found = tree.find(path.as_ref(), true).ok()?;
    match &tree.node(found.id).data {
        Data::Remote { url, .. } => Some(url.clone()),
        _ => None,
    }
}

/// Gives the file still at `url` its contents, fetched ahead of its being
/// opened. By URL, as it may have moved since; if it has since been fetched,
/// written over or deleted, there's nothing to do.
pub fn fill_remote(url: &str, bytes: Vec<u8>) {
    let mut tree = tree();
    let waiting = tree
        .nodes
        .iter_mut()
        .find(|node| matches!(&node.data, Data::Remote { url: at, .. } if at == url));
    if let Some(node) = waiting {
        node.data = Data::File(bytes);
    }
}

pub fn read(path: impl AsRef<Path>) -> io::Result<Vec<u8>> {
    let mut tree = tree();
    let found = tree.find(path.as_ref(), true)?;
    tree.fetch_remote(found.id)?;
    match &tree.node(found.id).data {
        Data::File(bytes) => Ok(bytes.clone()),
        _ => Err(error(ErrorKind::IsADirectory)),
    }
}

pub fn read_to_string(path: impl AsRef<Path>) -> io::Result<String> {
    String::from_utf8(read(path)?)
        .map_err(|_| io::Error::new(ErrorKind::InvalidData, "stream did not contain valid UTF-8"))
}

pub fn write(path: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> io::Result<()> {
    File::create(path)?.write_all(contents.as_ref())
}

pub fn create_dir(path: impl AsRef<Path>) -> io::Result<()> {
    tree().create_dir(path.as_ref())
}

pub fn create_dir_all(path: impl AsRef<Path>) -> io::Result<()> {
    let path = path.as_ref();
    let mut tree = tree();
    let mut ancestors: Vec<&Path> = path.ancestors().collect();
    ancestors.reverse();
    for dir in ancestors {
        if dir.file_name().is_none() {
            continue;
        }
        match tree.find(dir, true) {
            Ok(found) => {
                tree.dir(found.id)?;
            }
            Err(err) if err.kind() == ErrorKind::NotFound => tree.create_dir(dir)?,
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

pub fn remove_file(path: impl AsRef<Path>) -> io::Result<()> {
    tree().remove(path.as_ref(), false, false)
}

pub fn remove_dir(path: impl AsRef<Path>) -> io::Result<()> {
    tree().remove(path.as_ref(), true, false)
}

/// Removes a folder and everything in it; a link is removed, not followed.
pub fn remove_dir_all(path: impl AsRef<Path>) -> io::Result<()> {
    tree().remove(path.as_ref(), true, true)
}

pub fn rename(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<()> {
    tree().rename(from.as_ref(), to.as_ref())
}

/// Copies a file's contents and permissions; returns the bytes copied.
pub fn copy(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<u64> {
    let mut tree = tree();
    let found = tree.find(from.as_ref(), true)?;
    tree.fetch_remote(found.id)?;
    let source = tree.node(found.id);
    let Data::File(bytes) = &source.data else {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "the source path is neither a regular file nor a symlink to a regular file",
        ));
    };
    let (bytes, readonly) = (bytes.clone(), source.readonly);
    let options = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .clone();
    let mut file = tree.open(to.as_ref(), &options)?;
    let written = tree.write_at(&mut file, &bytes);
    tree.node_mut(file.id).readonly = readonly;
    // Closing takes the lock.
    drop(tree);
    drop(file);
    written.map(|n| n as u64)
}

/// Names `original` (not a folder) at `link` too.
pub fn hard_link(original: impl AsRef<Path>, link: impl AsRef<Path>) -> io::Result<()> {
    let mut tree = tree();
    let id = tree.find(original.as_ref(), false)?.id;
    if matches!(tree.node(id).data, Data::Dir(_)) {
        return Err(error(ErrorKind::PermissionDenied));
    }
    let (parent, name) = tree.parent(link.as_ref())?;
    if tree.dir(parent.id)?.contains_key(&name) {
        return Err(error(ErrorKind::AlreadyExists));
    }
    let changed = parent.path.join(&name);
    tree.dir_mut(parent.id)?.insert(name, id);
    tree.node_mut(id).links += 1;
    tree.node_mut(parent.id).modified = now();
    tree.changed(&changed);
    Ok(())
}

/// Makes `link` a symlink to `target`.
pub fn symlink(target: impl AsRef<Path>, link: impl AsRef<Path>) -> io::Result<()> {
    let mut tree = tree();
    let (parent, name) = tree.parent(link.as_ref())?;
    let changed = parent.path.join(&name);
    tree.add(parent.id, name, Data::Symlink(target.as_ref().to_owned()))?;
    tree.changed(&changed);
    Ok(())
}

pub fn read_link(path: impl AsRef<Path>) -> io::Result<PathBuf> {
    let tree = tree();
    let found = tree.find(path.as_ref(), false)?;
    match &tree.node(found.id).data {
        Data::Symlink(target) => Ok(target.clone()),
        _ => Err(error(ErrorKind::InvalidInput)),
    }
}

/// The absolute path to what `path` names, with no links or `..` in it.
pub fn canonicalize(path: impl AsRef<Path>) -> io::Result<PathBuf> {
    Ok(tree().find(path.as_ref(), true)?.path)
}

pub fn set_permissions(path: impl AsRef<Path>, permissions: Permissions) -> io::Result<()> {
    let mut tree = tree();
    let found = tree.find(path.as_ref(), true)?;
    tree.node_mut(found.id).readonly = permissions.readonly;
    tree.changed(&found.path);
    Ok(())
}

/// Sets when `path` was last read and changed, following links.
pub fn set_file_times(
    path: impl AsRef<Path>,
    accessed: SystemTime,
    modified: SystemTime,
) -> io::Result<()> {
    let mut tree = tree();
    let found = tree.find(path.as_ref(), true)?;
    let node = tree.node_mut(found.id);
    node.accessed = accessed;
    node.modified = modified;
    tree.changed(&found.path);
    Ok(())
}

/// Sets when `path` was last changed, following links.
pub fn set_file_mtime(path: impl AsRef<Path>, modified: SystemTime) -> io::Result<()> {
    let mut tree = tree();
    let found = tree.find(path.as_ref(), true)?;
    tree.node_mut(found.id).modified = modified;
    tree.changed(&found.path);
    Ok(())
}

/// The paths created, changed or removed since the last call, as a folder
/// watcher would report them.
pub fn take_changes() -> BTreeSet<PathBuf> {
    std::mem::take(&mut tree().changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A folder of its own for each test: they share the one tree.
    fn root(name: &str) -> PathBuf {
        let root = Path::new("/tests").join(name);
        create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn files_and_folders() {
        let r = root("files_and_folders");
        write(r.join("a.txt"), "hello").unwrap();
        assert_eq!(read_to_string(r.join("a.txt")).unwrap(), "hello");
        assert_eq!(metadata(r.join("a.txt")).unwrap().len(), 5);
        assert!(metadata(&r).unwrap().is_dir());

        create_dir(r.join("sub")).unwrap();
        assert_eq!(
            create_dir(r.join("sub")).unwrap_err().kind(),
            ErrorKind::AlreadyExists
        );
        create_dir_all(r.join("sub/deep/er")).unwrap();
        let mut names: Vec<_> = read_dir(&r)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        names.sort();
        assert_eq!(names, ["a.txt", "sub"]);

        assert_eq!(
            remove_dir(r.join("sub")).unwrap_err().kind(),
            ErrorKind::DirectoryNotEmpty
        );
        assert_eq!(
            remove_file(r.join("sub")).unwrap_err().kind(),
            ErrorKind::IsADirectory
        );
        remove_dir_all(r.join("sub")).unwrap();
        assert_eq!(
            metadata(r.join("sub")).unwrap_err().kind(),
            ErrorKind::NotFound
        );
        assert_eq!(
            read(r.join("a.txt/x")).unwrap_err().kind(),
            ErrorKind::NotADirectory
        );
    }

    #[test]
    fn file_handles() {
        let r = root("file_handles");
        let path = r.join("f");
        {
            let mut f = File::create(&path).unwrap();
            f.write_all(b"0123456789").unwrap();
            f.seek(SeekFrom::Start(2)).unwrap();
            f.write_all(b"ab").unwrap();
        }
        assert_eq!(read(&path).unwrap(), b"01ab456789");
        let mut f = File::open(&path).unwrap();
        f.seek(SeekFrom::End(-3)).unwrap();
        let mut rest = String::new();
        f.read_to_string(&mut rest).unwrap();
        assert_eq!(rest, "789");
        assert!(f.write(b"x").is_err());

        let mut log = OpenOptions::new().append(true).open(&path).unwrap();
        log.write_all(b"!").unwrap();
        assert_eq!(read(&path).unwrap(), b"01ab456789!");
        assert_eq!(
            File::create_new(&path).unwrap_err().kind(),
            ErrorKind::AlreadyExists
        );

        // Removed while open: still readable until closed.
        remove_file(&path).unwrap();
        f.seek(SeekFrom::Start(0)).unwrap();
        let mut all = Vec::new();
        f.read_to_end(&mut all).unwrap();
        assert_eq!(all, b"01ab456789!");
    }

    #[test]
    fn rename_and_copy() {
        let r = root("rename_and_copy");
        write(r.join("a"), "a").unwrap();
        write(r.join("b"), "b").unwrap();
        rename(r.join("a"), r.join("b")).unwrap();
        assert_eq!(read_to_string(r.join("b")).unwrap(), "a");
        assert!(!path_exists(r.join("a")));

        create_dir_all(r.join("d/e")).unwrap();
        assert_eq!(
            rename(r.join("d"), r.join("d/e/f")).unwrap_err().kind(),
            ErrorKind::InvalidInput
        );
        assert_eq!(
            rename(r.join("b"), r.join("d")).unwrap_err().kind(),
            ErrorKind::IsADirectory
        );
        rename(r.join("d"), r.join("moved")).unwrap();
        assert!(metadata(r.join("moved/e")).unwrap().is_dir());

        let mut readonly = metadata(r.join("b")).unwrap().permissions();
        readonly.set_readonly(true);
        set_permissions(r.join("b"), readonly).unwrap();
        assert_eq!(copy(r.join("b"), r.join("c")).unwrap(), 1);
        assert!(metadata(r.join("c")).unwrap().permissions().readonly());
        assert_eq!(
            write(r.join("c"), "x").unwrap_err().kind(),
            ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn links() {
        let r = root("links");
        create_dir(r.join("real")).unwrap();
        write(r.join("real/f"), "f").unwrap();
        symlink(r.join("real"), r.join("abs")).unwrap();
        symlink("real", r.join("rel")).unwrap();
        symlink("../links/real/f", r.join("up")).unwrap();
        assert_eq!(read_to_string(r.join("abs/f")).unwrap(), "f");
        assert_eq!(read_to_string(r.join("rel/f")).unwrap(), "f");
        assert_eq!(read_to_string(r.join("up")).unwrap(), "f");
        assert!(symlink_metadata(r.join("rel")).unwrap().is_symlink());
        assert!(metadata(r.join("rel")).unwrap().is_dir());
        assert_eq!(read_link(r.join("rel")).unwrap(), Path::new("real"));
        assert_eq!(
            canonicalize(r.join("rel/../rel/f")).unwrap(),
            r.join("real/f")
        );

        symlink("loop", r.join("loop")).unwrap();
        assert!(metadata(r.join("loop")).is_err());
        // Removing a link to a folder leaves the folder.
        remove_dir_all(r.join("abs")).unwrap();
        assert!(path_exists(r.join("real/f")));

        hard_link(r.join("real/f"), r.join("hard")).unwrap();
        remove_file(r.join("real/f")).unwrap();
        assert_eq!(read_to_string(r.join("hard")).unwrap(), "f");
    }

    #[test]
    fn reports_changes() {
        let r = root("reports_changes");
        write(r.join("x"), "x").unwrap();
        // Other tests change the tree too.
        assert!(take_changes().contains(&r.join("x")));
        let _ = read(r.join("x")).unwrap();
        rename(r.join("x"), r.join("y")).unwrap();
        let changes = take_changes();
        assert!(changes.contains(&r.join("x")) && changes.contains(&r.join("y")));
    }

    fn path_exists(path: impl AsRef<Path>) -> bool {
        metadata(path).is_ok()
    }

    fn fake_loader(url: &str) -> io::Result<Vec<u8>> {
        match url {
            "good" | "good-2" | "good-3" | "good-4" => Ok(b"fetched".to_vec()),
            _ => Err(io::Error::new(ErrorKind::NotFound, "404")),
        }
    }

    #[test]
    fn remote_files_are_listed_without_fetching_and_read_when_opened() {
        set_remote_loader(fake_loader);
        let dir = PathBuf::from("/remote-read");
        create_dir_all(&dir).unwrap();
        let file = dir.join("photo.jpg");
        let when = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000);
        add_remote(&file, "good", 12_345, when).unwrap();

        // The size it's said to have, until the contents are here.
        let meta = metadata(&file).unwrap();
        assert!(meta.is_file());
        assert_eq!(meta.len(), 12_345);
        assert_eq!(meta.modified().unwrap(), when);

        assert_eq!(read(&file).unwrap(), b"fetched");
        assert_eq!(metadata(&file).unwrap().len(), 7);
        assert_eq!(metadata(&file).unwrap().modified().unwrap(), when);
    }

    #[test]
    fn a_failed_fetch_is_an_error_and_can_be_tried_again() {
        set_remote_loader(fake_loader);
        let dir = PathBuf::from("/remote-missing");
        create_dir_all(&dir).unwrap();
        let file = dir.join("gone");
        add_remote(&file, "missing", 5, now()).unwrap();
        assert_eq!(read(&file).unwrap_err().kind(), ErrorKind::NotFound);
        assert_eq!(File::open(&file).err().unwrap().kind(), ErrorKind::NotFound);
        // Still listed as it was.
        assert_eq!(metadata(&file).unwrap().len(), 5);
    }

    #[test]
    fn remote_files_can_be_filled_ahead_of_being_opened() {
        set_remote_loader(fake_loader);
        let dir = PathBuf::from("/remote-fill");
        create_dir_all(&dir).unwrap();
        let file = dir.join("photo.jpg");
        let when = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000);
        // The loader would fail on this URL, so reading it shows it wasn't used.
        add_remote(&file, "fill-1", 5, when).unwrap();
        assert_eq!(remote_url(&file).as_deref(), Some("fill-1"));
        fill_remote("fill-1", b"ahead".to_vec());
        assert_eq!(remote_url(&file), None);
        assert_eq!(read(&file).unwrap(), b"ahead");
        assert_eq!(metadata(&file).unwrap().modified().unwrap(), when);
        // Too late: what's there stays.
        fill_remote("fill-1", b"late".to_vec());
        assert_eq!(read(&file).unwrap(), b"ahead");

        // It finds the file where it went, and a link leads to it.
        let moved = dir.join("moved");
        add_remote(dir.join("before"), "fill-2", 5, when).unwrap();
        rename(dir.join("before"), &moved).unwrap();
        let link = dir.join("link");
        symlink("moved", &link).unwrap();
        assert_eq!(remote_url(&link).as_deref(), Some("fill-2"));
        fill_remote("fill-2", b"ahead".to_vec());
        assert_eq!(read(&moved).unwrap(), b"ahead");

        // Written over or deleted before it came: nothing to fill.
        let over = dir.join("over");
        add_remote(&over, "fill-3", 5, when).unwrap();
        write(&over, b"mine").unwrap();
        fill_remote("fill-3", b"ahead".to_vec());
        assert_eq!(read(&over).unwrap(), b"mine");
        let gone = dir.join("gone");
        add_remote(&gone, "fill-4", 5, when).unwrap();
        remove_file(&gone).unwrap();
        fill_remote("fill-4", b"ahead".to_vec());
        assert_eq!(remote_url(&gone), None);
        assert!(!path_exists(&gone));

        // Not remote, or not there.
        assert_eq!(remote_url(&dir), None);
        assert_eq!(remote_url(dir.join("missing")), None);
    }

    #[test]
    fn remote_files_can_be_copied_moved_and_replaced() {
        set_remote_loader(fake_loader);
        let dir = PathBuf::from("/remote-ops");
        create_dir_all(&dir).unwrap();
        let (a, b, c) = (dir.join("a"), dir.join("b"), dir.join("c"));
        add_remote(&a, "good-2", 7, now()).unwrap();
        copy(&a, &b).unwrap();
        assert_eq!(read(&b).unwrap(), b"fetched");
        add_remote(&c, "good-3", 7, now()).unwrap();
        // Moved before it's fetched: the URL goes along.
        let moved = dir.join("moved");
        rename(&c, &moved).unwrap();
        assert_eq!(read(&moved).unwrap(), b"fetched");
        // Written over without fetching: the URL here doesn't even exist.
        let over = dir.join("over");
        add_remote(&over, "missing", 7, now()).unwrap();
        write(&over, b"mine").unwrap();
        assert_eq!(read(&over).unwrap(), b"mine");
        // Appending needs what's there.
        let log = dir.join("log");
        add_remote(&log, "good-4", 7, now()).unwrap();
        let mut file = OpenOptions::new().append(true).open(&log).unwrap();
        file.write_all(b"!").unwrap();
        drop(file);
        assert_eq!(read(&log).unwrap(), b"fetched!");
    }
}
