//! Files in a Proton Drive account: paths like
//! `proton://drive/Documents/notes.txt`, the top being the account's "My
//! files".
//!
//! Removing something puts it in the Drive's trash, from which it can be
//! restored. A file is always written whole, as a new revision, which only
//! appears once all of it is in: [`File`]s being written are held in memory
//! until flushed, and anything big goes through [`upload`] instead.
//!
//! The account itself is the app's: it signs in and hands over a [`Backend`]
//! ([`set_backend`]), which keeps this crate free of the SDK and its async
//! runtime, and lets the tests here use a stand-in.
//!
//! Proton asks clients not to poll, so a listing is kept until the Drive
//! says it changed ([`folder_changed`]) or the user refreshes ([`forget`]),
//! rather than going stale after a while as a server's do. Changes made
//! through here update what's known at once.

use std::{
    cell::Cell,
    collections::{BTreeSet, HashMap},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError},
};

use crate::sftp::{Attrs, S_IFDIR, S_IFREG};

pub const SCHEME: &str = "proton://";
const TOP: &str = "proton://drive";

/// How much is fetched at a time: the Drive's own block size, as a read of
/// any part of a block downloads all of it anyway.
const CHUNK: u64 = 4 << 20;

/// The most a [`File`] being written may hold in memory.
const MAX_BUFFERED: u64 = 64 << 20;

/// A file or folder, as the Drive says it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    /// What the [`Backend`] knows it by.
    pub id: String,
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    /// Seconds since 1970.
    pub modified: i64,
}

/// The account, signed in. Its calls wait on the network.
pub trait Backend: Send + Sync {
    /// The top of "My files".
    fn root(&self) -> io::Result<Node>;
    /// What's in a folder, trashed items aside.
    fn list(&self, folder_id: &str) -> io::Result<Vec<Node>>;
    fn open(&self, file_id: &str) -> io::Result<Box<dyn Reader>>;
    /// The small picture the Drive keeps of a file, if it has one.
    fn thumbnail(&self, file_id: &str) -> io::Result<Option<Vec<u8>>>;
    fn create_folder(&self, parent_id: &str, name: &str) -> io::Result<Node>;
    /// A new file in a folder, with all of what `reader` gives. `size` must
    /// be how much that is.
    fn upload(
        &self,
        parent_id: &str,
        name: &str,
        reader: &mut (dyn Read + Send),
        size: u64,
        modified: Option<i64>,
    ) -> io::Result<Node>;
    /// A new revision of a file.
    fn upload_revision(
        &self,
        file_id: &str,
        reader: &mut (dyn Read + Send),
        size: u64,
        modified: Option<i64>,
    ) -> io::Result<Node>;
    fn rename(&self, id: &str, name: &str, is_dir: bool) -> io::Result<()>;
    fn move_to(&self, id: &str, parent_id: &str) -> io::Result<()>;
    /// A copy made on the Drive, nothing coming here.
    fn copy(&self, id: &str, parent_id: &str, name: &str) -> io::Result<Node>;
    /// Puts it in the Drive's trash.
    fn trash(&self, id: &str) -> io::Result<()>;
}

/// A file's latest contents.
pub trait Reader: Send {
    fn size(&self) -> u64;
    /// Up to `length` bytes at `offset`; fewer only at the end of the file.
    fn read_at(&self, offset: u64, length: u64) -> io::Result<Vec<u8>>;
}

/// The error for what the Drive has no such thing as: links, for one.
pub fn unsupported() -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, "Proton Drive can't do that")
}

/// A folder's contents, as last heard.
struct Listing {
    folder_id: String,
    nodes: Arc<Vec<Node>>,
}

#[derive(Default)]
struct State {
    backend: Option<Arc<dyn Backend>>,
    root: Option<Node>,
    /// By folder, as its parts joined with `/`.
    listings: HashMap<String, Listing>,
    /// Folders changed since [`take_changes`] was last called.
    changes: BTreeSet<PathBuf>,
    /// Bumped on signing in or out, so that a listing fetched from the
    /// account before isn't kept for the one after.
    generation: u64,
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(Mutex::default);

fn state() -> MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(PoisonError::into_inner)
}

thread_local! {
    static NO_WAITING: Cell<bool> = const { Cell::new(false) };
}

/// Keeps this thread from waiting on the Drive: anything not known already
/// fails instead. For the UI's thread.
pub fn never_wait_on_this_thread() {
    NO_WAITING.set(true);
}

fn may_wait() -> io::Result<()> {
    if NO_WAITING.get() {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "that isn't fetched from Proton Drive yet",
        ));
    }
    Ok(())
}

/// Signs in (`Some`) or out (`None`), forgetting what the last account had.
pub fn set_backend(backend: Option<Arc<dyn Backend>>) {
    let mut state = state();
    let generation = state.generation + 1;
    *state = State {
        backend,
        generation,
        ..State::default()
    };
}

pub fn is_signed_in() -> bool {
    state().backend.is_some()
}

fn backend() -> io::Result<(Arc<dyn Backend>, u64)> {
    let state = state();
    match &state.backend {
        Some(backend) => Ok((backend.clone(), state.generation)),
        None => Err(io::Error::new(
            io::ErrorKind::NotConnected,
            "not signed in to Proton Drive",
        )),
    }
}

/// The top of the Drive, as a path.
pub fn top() -> PathBuf {
    PathBuf::from(TOP)
}

pub fn is_proton(path: &Path) -> bool {
    path.to_str().is_some_and(|text| text.starts_with(SCHEME))
}

/// The names leading to `path` from the top of the Drive; `None` if it isn't
/// a Drive path, or is one with `.` or `..` in it.
pub fn parts(path: &Path) -> Option<Vec<String>> {
    let rest = path.to_str()?.strip_prefix(TOP)?;
    if !rest.is_empty() && !rest.starts_with(['/', '\\']) {
        return None;
    }
    let parts: Vec<String> = rest
        .split(['/', '\\'])
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect();
    parts
        .iter()
        .all(|part| part != "." && part != "..")
        .then_some(parts)
}

fn parts_of(path: &Path) -> io::Result<Vec<String>> {
    parts(path).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("not a valid Proton Drive path: {}", path.display()),
        )
    })
}

fn path_of(parts: &[String]) -> PathBuf {
    let mut path = TOP.to_owned();
    for part in parts {
        path.push('/');
        path.push_str(part);
    }
    path.into()
}

/// A name fit to be part of a path. Others may have made the Drive's files
/// with anything in their names, so those that would climb out of a folder
/// (`None`) or be split into several parts are dealt with here.
fn safe_name(name: &str) -> Option<String> {
    if name.is_empty() || name == "." || name == ".." {
        return None;
    }
    Some(name.replace('/', "\u{2215}").replace('\\', "\u{29F5}"))
}

fn root_node() -> io::Result<Node> {
    if let Some(root) = state().root.clone() {
        return Ok(root);
    }
    may_wait()?;
    let (backend, generation) = backend()?;
    let root = backend.root()?;
    let mut state = state();
    if state.generation == generation {
        state.root = Some(root.clone());
    }
    Ok(root)
}

fn children(parts: &[String], folder: &Node) -> io::Result<Arc<Vec<Node>>> {
    let key = parts.join("/");
    if let Some(known) = state().listings.get(&key) {
        return Ok(known.nodes.clone());
    }
    may_wait()?;
    let (backend, generation) = backend()?;
    let nodes = Arc::new(fetch(backend.as_ref(), &folder.id)?);
    let mut state = state();
    if state.generation == generation {
        state.listings.insert(
            key,
            Listing {
                folder_id: folder.id.clone(),
                nodes: nodes.clone(),
            },
        );
    }
    Ok(nodes)
}

fn fetch(backend: &dyn Backend, folder_id: &str) -> io::Result<Vec<Node>> {
    let mut nodes = backend.list(folder_id)?;
    nodes.retain_mut(|node| match safe_name(&node.name) {
        Some(name) => {
            node.name = name;
            true
        }
        None => false,
    });
    Ok(nodes)
}

fn resolve(parts: &[String]) -> io::Result<Node> {
    let mut node = root_node()?;
    for (depth, name) in parts.iter().enumerate() {
        if !node.is_dir {
            return Err(io::ErrorKind::NotADirectory.into());
        }
        node = children(&parts[..depth], &node)?
            .iter()
            .find(|child| child.name == *name)
            .cloned()
            .ok_or(io::ErrorKind::NotFound)?;
    }
    Ok(node)
}

fn attrs_of(node: &Node) -> Attrs {
    let modified = u32::try_from(node.modified.max(0)).unwrap_or(u32::MAX);
    Attrs {
        size: Some(node.size),
        uid_gid: None,
        // As a file of the user's would be: what can't be done yet is
        // refused when tried, and copies out of the Drive shouldn't be
        // read-only because this is.
        mode: Some(if node.is_dir {
            S_IFDIR | 0o755
        } else {
            S_IFREG | 0o644
        }),
        atime_mtime: Some((modified, modified)),
    }
}

pub fn stat(path: &Path) -> io::Result<Attrs> {
    Ok(attrs_of(&resolve(&parts_of(path)?)?))
}

/// The path as it's written when listed.
pub fn canonicalize(path: &Path) -> io::Result<PathBuf> {
    let parts = parts_of(path)?;
    resolve(&parts)?;
    Ok(path_of(&parts))
}

/// An item of a folder.
pub struct Listed {
    pub path: PathBuf,
    pub name: String,
    pub attrs: Attrs,
}

pub fn read_dir(path: &Path) -> io::Result<Vec<Listed>> {
    let parts = parts_of(path)?;
    let folder = resolve(&parts)?;
    if !folder.is_dir {
        return Err(io::ErrorKind::NotADirectory.into());
    }
    Ok(children(&parts, &folder)?
        .iter()
        .map(|node| {
            let mut child = parts.clone();
            child.push(node.name.clone());
            Listed {
                path: path_of(&child),
                name: node.name.clone(),
                attrs: attrs_of(node),
            }
        })
        .collect())
}

/// Whether the folder at `path` can be read without waiting.
pub fn is_listed(path: &Path) -> bool {
    parts(path).is_some_and(|parts| state().listings.contains_key(&parts.join("/")))
}

/// Forgets what's known of the folder at `path` and those in it, so they're
/// fetched again.
pub fn forget(path: &Path) {
    let Some(parts) = parts(path) else { return };
    let key = parts.join("/");
    let within = format!("{key}/");
    state()
        .listings
        .retain(|folder, _| !key.is_empty() && *folder != key && !folder.starts_with(&within));
}

/// The small picture the Drive keeps of the file at `path`, if it has one.
pub fn thumbnail(path: &Path) -> io::Result<Option<Vec<u8>>> {
    let node = resolve(&parts_of(path)?)?;
    if node.is_dir {
        return Ok(None);
    }
    may_wait()?;
    backend()?.0.thumbnail(&node.id)
}

/// Folders changed since the last call, through here or by others.
pub fn take_changes() -> BTreeSet<PathBuf> {
    std::mem::take(&mut state().changes)
}

/// The Drive says something in a folder changed (`None`: it can't say
/// where): those known are fetched again, and any that came out different are
/// noted for [`take_changes`]. The old listing serves until then, so nothing
/// on screen blinks. Waits on the network. Says whether any were different.
pub fn folder_changed(folder_id: Option<&str>) -> bool {
    let Ok((backend, generation)) = backend() else {
        return false;
    };
    let wanted: Vec<(String, String)> = state()
        .listings
        .iter()
        .filter(|(_, listing)| folder_id.is_none_or(|id| id == listing.folder_id))
        .map(|(key, listing)| (key.clone(), listing.folder_id.clone()))
        .collect();
    let mut any = false;
    for (key, id) in wanted {
        let fetched = fetch(backend.as_ref(), &id);
        let mut state = state();
        if state.generation != generation {
            return false;
        }
        match fetched {
            Ok(nodes) => {
                let Some(listing) = state.listings.get_mut(&key) else {
                    continue;
                };
                if *listing.nodes != nodes {
                    listing.nodes = Arc::new(nodes);
                    let parts: Vec<String> = key
                        .split('/')
                        .filter(|part| !part.is_empty())
                        .map(str::to_owned)
                        .collect();
                    state.changes.insert(path_of(&parts));
                    any = true;
                }
            }
            // That folder is gone; its parent's listing tells of it.
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                state.listings.remove(&key);
                any = true;
            }
            Err(_) => {}
        }
    }
    any
}

/// The folder `path` is in and its name there. The top isn't in anything.
fn split(path: &Path) -> io::Result<(Vec<String>, String)> {
    let mut parts = parts_of(path)?;
    let name = parts.pop().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            "the top of the Drive can't be changed",
        )
    })?;
    Ok((parts, name))
}

fn folder_at(parts: &[String]) -> io::Result<Node> {
    let folder = resolve(parts)?;
    if !folder.is_dir {
        return Err(io::ErrorKind::NotADirectory.into());
    }
    Ok(folder)
}

fn child(parts: &[String], folder: &Node, name: &str) -> io::Result<Option<Node>> {
    Ok(children(parts, folder)?
        .iter()
        .find(|node| node.name == name)
        .cloned())
}

/// Changes what's known of a folder, and notes that it changed.
fn edit_listing(parts: &[String], edit: impl FnOnce(&mut Vec<Node>)) {
    let mut state = state();
    if let Some(listing) = state.listings.get_mut(&parts.join("/")) {
        let mut nodes = (*listing.nodes).clone();
        edit(&mut nodes);
        listing.nodes = Arc::new(nodes);
    }
    state.changes.insert(path_of(parts));
}

/// Puts `node` in what's known of a folder, in place of any of its name.
fn know(parts: &[String], node: Node) {
    edit_listing(parts, |nodes| {
        nodes.retain(|other| other.name != node.name);
        nodes.push(node);
    });
}

pub fn create_dir(path: &Path) -> io::Result<()> {
    let (parts, name) = split(path)?;
    let parent = folder_at(&parts)?;
    if child(&parts, &parent, &name)?.is_some() {
        return Err(io::ErrorKind::AlreadyExists.into());
    }
    let mut node = backend()?.0.create_folder(&parent.id, &name)?;
    node.name = name;
    know(&parts, node);
    Ok(())
}

pub fn create_dir_all(path: &Path) -> io::Result<()> {
    let parts = parts_of(path)?;
    for depth in 1..=parts.len() {
        match create_dir(&path_of(&parts[..depth])) {
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                folder_at(&parts[..depth])?;
            }
            other => other?,
        }
    }
    Ok(())
}

fn trash(parts: &[String], name: &str, node: &Node) -> io::Result<()> {
    backend()?.0.trash(&node.id)?;
    edit_listing(parts, |nodes| nodes.retain(|other| other.name != name));
    Ok(())
}

/// Puts a file in the Drive's trash.
pub fn remove_file(path: &Path) -> io::Result<()> {
    let (parts, name) = split(path)?;
    let node = resolve(&parts_of(path)?)?;
    if node.is_dir {
        return Err(io::ErrorKind::IsADirectory.into());
    }
    trash(&parts, &name, &node)
}

/// Puts an empty folder in the Drive's trash.
pub fn remove_dir(path: &Path) -> io::Result<()> {
    let (parts, name) = split(path)?;
    let all = parts_of(path)?;
    let node = folder_at(&all)?;
    if !children(&all, &node)?.is_empty() {
        return Err(io::ErrorKind::DirectoryNotEmpty.into());
    }
    trash(&parts, &name, &node)?;
    forget(path);
    Ok(())
}

/// Puts a folder and what's in it in the Drive's trash.
pub fn remove_dir_all(path: &Path) -> io::Result<()> {
    let (parts, name) = split(path)?;
    let node = folder_at(&parts_of(path)?)?;
    trash(&parts, &name, &node)?;
    forget(path);
    Ok(())
}

/// Renames, moves, or both. Never replaces what's there already.
pub fn rename(from: &Path, to: &Path) -> io::Result<()> {
    let (from_parts, from_name) = split(from)?;
    let (to_parts, to_name) = split(to)?;
    if from_parts == to_parts && from_name == to_name {
        return Ok(());
    }
    let mut node = resolve(&parts_of(from)?)?;
    let inside = parts_of(to)?.starts_with(&parts_of(from)?);
    if inside {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a folder can't go inside itself",
        ));
    }
    let parent = folder_at(&to_parts)?;
    if child(&to_parts, &parent, &to_name)?.is_some() {
        return Err(io::ErrorKind::AlreadyExists.into());
    }
    let backend = backend()?.0;
    if from_parts != to_parts {
        backend.move_to(&node.id, &parent.id)?;
    }
    if from_name != to_name {
        backend.rename(&node.id, &to_name, node.is_dir)?;
    }
    edit_listing(&from_parts, |nodes| {
        nodes.retain(|other| other.name != from_name);
    });
    node.name = to_name;
    know(&to_parts, node);
    forget(from);
    Ok(())
}

/// Copies a file within the Drive, on the Drive.
pub fn copy_within(from: &Path, to: &Path) -> io::Result<()> {
    let (parts, name) = split(to)?;
    let source = resolve(&parts_of(from)?)?;
    let parent = folder_at(&parts)?;
    if child(&parts, &parent, &name)?.is_some() {
        return Err(io::ErrorKind::AlreadyExists.into());
    }
    let mut node = backend()?.0.copy(&source.id, &parent.id, &name)?;
    node.name = name;
    know(&parts, node);
    Ok(())
}

/// Writes all of `reader` (`size` bytes) to the file at `path`: a new file,
/// or a new revision of the one there.
pub fn upload(
    path: &Path,
    reader: &mut (dyn Read + Send),
    size: u64,
    modified: Option<i64>,
) -> io::Result<()> {
    let (parts, name) = split(path)?;
    let parent = folder_at(&parts)?;
    let existing = child(&parts, &parent, &name)?;
    let backend = backend()?.0;
    let mut node = match existing {
        Some(node) if node.is_dir => return Err(io::ErrorKind::IsADirectory.into()),
        Some(node) => backend.upload_revision(&node.id, reader, size, modified)?,
        None => backend.upload(&parent.id, &name, reader, size, modified)?,
    };
    node.name = name;
    know(&parts, node);
    Ok(())
}

/// As [`upload`], but fails if there's something at `path` already.
pub fn upload_new(
    path: &Path,
    reader: &mut (dyn Read + Send),
    size: u64,
    modified: Option<i64>,
) -> io::Result<()> {
    let (parts, name) = split(path)?;
    if child(&parts, &folder_at(&parts)?, &name)?.is_some() {
        return Err(io::ErrorKind::AlreadyExists.into());
    }
    upload(path, reader, size, modified)
}

/// A file opened to read or write.
pub struct File {
    path: PathBuf,
    attrs: Attrs,
    position: u64,
    mode: Mode,
}

enum Mode {
    Reading {
        reader: Box<dyn Reader>,
        /// The last chunk fetched, and where in the file it starts.
        chunk: Vec<u8>,
        chunk_at: u64,
    },
    Writing {
        buffer: Vec<u8>,
        /// Whether the Drive doesn't have this yet.
        dirty: bool,
        modified: Option<i64>,
    },
}

pub fn open(path: &Path) -> io::Result<File> {
    let node = resolve(&parts_of(path)?)?;
    if node.is_dir {
        return Err(io::ErrorKind::IsADirectory.into());
    }
    may_wait()?;
    let reader = backend()?.0.open(&node.id)?;
    let mut attrs = attrs_of(&node);
    // The listing's size can be only what's stored, encryption included.
    attrs.size = Some(reader.size());
    Ok(File {
        path: path.to_path_buf(),
        attrs,
        position: 0,
        mode: Mode::Reading {
            reader,
            chunk: Vec::new(),
            chunk_at: 0,
        },
    })
}

/// How a file is opened for writing, as `OpenOptions` says.
#[derive(Clone, Copy, Debug, Default)]
pub struct Writing {
    pub create: bool,
    pub create_new: bool,
    pub truncate: bool,
    pub append: bool,
}

fn too_big() -> io::Error {
    io::Error::new(
        io::ErrorKind::FileTooLarge,
        "too big to write to Proton Drive this way",
    )
}

/// Opens a file for writing, which reaches the Drive when it's flushed.
pub fn open_for_writing(path: &Path, how: Writing) -> io::Result<File> {
    let (parts, name) = split(path)?;
    let existing = child(&parts, &folder_at(&parts)?, &name)?;
    let exists = existing.is_some();
    if existing.as_ref().is_some_and(|node| node.is_dir) {
        return Err(io::ErrorKind::IsADirectory.into());
    }
    if exists && how.create_new {
        return Err(io::ErrorKind::AlreadyExists.into());
    }
    if !exists && !how.create && !how.create_new {
        return Err(io::ErrorKind::NotFound.into());
    }
    let mut buffer = Vec::new();
    if let Some(node) = existing.as_ref().filter(|_| !how.truncate) {
        if node.size > MAX_BUFFERED {
            return Err(too_big());
        }
        open(path)?.read_to_end(&mut buffer)?;
    }
    let position = if how.append { buffer.len() as u64 } else { 0 };
    let attrs = attrs_of(&Node {
        id: String::new(),
        name,
        is_dir: false,
        size: buffer.len() as u64,
        modified: 0,
    });
    Ok(File {
        path: path.to_path_buf(),
        attrs,
        position,
        mode: Mode::Writing {
            buffer,
            // Something new, or emptied, is a change already.
            dirty: !exists || how.truncate,
            modified: None,
        },
    })
}

impl File {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn attrs(&self) -> Attrs {
        self.attrs.clone()
    }

    fn len(&self) -> u64 {
        match &self.mode {
            Mode::Reading { reader, .. } => reader.size(),
            Mode::Writing { buffer, .. } => buffer.len() as u64,
        }
    }

    pub fn set_len(&mut self, len: u64) -> io::Result<()> {
        let Mode::Writing { buffer, dirty, .. } = &mut self.mode else {
            return Err(not_for_writing());
        };
        if len > MAX_BUFFERED {
            return Err(too_big());
        }
        buffer.resize(len as usize, 0);
        *dirty = true;
        self.attrs.size = Some(len);
        Ok(())
    }

    /// When the file will say it was changed, once written.
    pub fn set_modified(&mut self, seconds: i64) {
        if let Mode::Writing {
            modified, dirty, ..
        } = &mut self.mode
        {
            *modified = Some(seconds);
            *dirty = true;
        }
    }

    /// Sends the Drive what's been written, if it hasn't got it.
    pub fn commit(&mut self) -> io::Result<()> {
        let Mode::Writing {
            buffer,
            dirty,
            modified,
        } = &mut self.mode
        else {
            return Ok(());
        };
        if *dirty {
            let mut contents = buffer.as_slice();
            upload(&self.path, &mut contents, buffer.len() as u64, *modified)?;
            *dirty = false;
        }
        Ok(())
    }
}

fn not_for_writing() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "this file was opened only to read it",
    )
}

/// A last try for what wasn't flushed, with nowhere to say if it fails:
/// whoever cares flushes first.
impl Drop for File {
    fn drop(&mut self) {
        let _ = self.commit();
    }
}

impl Read for File {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() || self.position >= self.len() {
            return Ok(0);
        }
        let available = match &mut self.mode {
            Mode::Reading {
                reader,
                chunk,
                chunk_at,
            } => {
                let chunk_end = *chunk_at + chunk.len() as u64;
                if self.position < *chunk_at || self.position >= chunk_end {
                    let at = self.position / CHUNK * CHUNK;
                    *chunk = reader.read_at(at, CHUNK)?;
                    *chunk_at = at;
                }
                let skip = usize::try_from(self.position - *chunk_at).ok();
                skip.and_then(|skip| chunk.get(skip..))
            }
            Mode::Writing { buffer, .. } => buffer.get(self.position as usize..),
        };
        // Nothing where the file should go on: it was shorter than it said.
        let Some(available) = available else {
            return Ok(0);
        };
        let count = available.len().min(buf.len());
        buf[..count].copy_from_slice(&available[..count]);
        self.position += count as u64;
        Ok(count)
    }
}

impl Write for File {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let end = self.position.saturating_add(buf.len() as u64);
        if end > MAX_BUFFERED {
            return Err(too_big());
        }
        let Mode::Writing { buffer, dirty, .. } = &mut self.mode else {
            return Err(not_for_writing());
        };
        let start = self.position as usize;
        if buffer.len() < end as usize {
            buffer.resize(end as usize, 0);
        }
        buffer[start..end as usize].copy_from_slice(buf);
        *dirty = true;
        self.position = end;
        self.attrs.size = Some(buffer.len() as u64);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.commit()
    }
}

impl Seek for File {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let target = match from {
            SeekFrom::Start(at) => Some(at),
            SeekFrom::End(by) => self.len().checked_add_signed(by),
            SeekFrom::Current(by) => self.position.checked_add_signed(by),
        };
        self.position = target.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek before the start of a file",
            )
        })?;
        Ok(self.position)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        MutexGuard as Guard,
        atomic::{AtomicUsize, Ordering},
    };

    use super::*;

    /// One test at a time: the signed-in account is global.
    static SERIAL: Mutex<()> = Mutex::new(());

    const FILE_SIZE: usize = 9 << 20;

    /// A Drive in memory.
    struct Fake {
        lists: AtomicUsize,
        reads: Arc<AtomicUsize>,
        folders: Mutex<HashMap<String, Vec<Node>>>,
        contents: Mutex<HashMap<String, Vec<u8>>>,
        trashed: Mutex<Vec<String>>,
        next_id: AtomicUsize,
    }

    impl Default for Fake {
        fn default() -> Self {
            Self {
                lists: AtomicUsize::new(0),
                reads: Arc::default(),
                folders: Mutex::new(HashMap::from([
                    (
                        "root".to_owned(),
                        vec![
                            node("docs", "Docs", true),
                            node("a", "a.bin", false),
                            node("up", "..", false),
                            node("slash", "x/y", false),
                        ],
                    ),
                    ("docs".to_owned(), vec![node("b", "b.txt", false)]),
                ])),
                contents: Mutex::default(),
                trashed: Mutex::default(),
                next_id: AtomicUsize::new(0),
            }
        }
    }

    fn node(id: &str, name: &str, is_dir: bool) -> Node {
        Node {
            id: id.to_owned(),
            name: name.to_owned(),
            is_dir,
            size: if is_dir { 0 } else { FILE_SIZE as u64 },
            modified: 1_700_000_000,
        }
    }

    struct FakeReader(Arc<AtomicUsize>);

    impl Reader for FakeReader {
        fn size(&self) -> u64 {
            FILE_SIZE as u64
        }

        fn read_at(&self, offset: u64, length: u64) -> io::Result<Vec<u8>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            let end = (offset + length).min(FILE_SIZE as u64);
            Ok((offset..end).map(|at| (at % 251) as u8).collect())
        }
    }

    struct Stored(Vec<u8>);

    impl Reader for Stored {
        fn size(&self) -> u64 {
            self.0.len() as u64
        }

        fn read_at(&self, offset: u64, length: u64) -> io::Result<Vec<u8>> {
            let start = (offset as usize).min(self.0.len());
            let end = (start + length as usize).min(self.0.len());
            Ok(self.0[start..end].to_vec())
        }
    }

    impl Fake {
        fn store(
            &self,
            id: &str,
            reader: &mut (dyn Read + Send),
            modified: Option<i64>,
            name: &str,
        ) -> io::Result<Node> {
            let mut bytes = Vec::new();
            reader.read_to_end(&mut bytes)?;
            let node = Node {
                id: id.to_owned(),
                name: name.to_owned(),
                is_dir: false,
                size: bytes.len() as u64,
                modified: modified.unwrap_or(1),
            };
            self.contents.lock().unwrap().insert(id.to_owned(), bytes);
            Ok(node)
        }

        fn take(&self, id: &str) -> Option<(String, Node)> {
            let mut folders = self.folders.lock().unwrap();
            for (folder, nodes) in folders.iter_mut() {
                if let Some(at) = nodes.iter().position(|node| node.id == id) {
                    return Some((folder.clone(), nodes.remove(at)));
                }
            }
            None
        }

        fn fresh_id(&self) -> String {
            format!("n{}", self.next_id.fetch_add(1, Ordering::SeqCst))
        }

        fn add(&self, parent: &str, node: Node) {
            let mut folders = self.folders.lock().unwrap();
            folders.entry(parent.to_owned()).or_default().push(node);
        }
    }

    impl Backend for Fake {
        fn root(&self) -> io::Result<Node> {
            Ok(node("root", "root", true))
        }

        fn list(&self, folder_id: &str) -> io::Result<Vec<Node>> {
            self.lists.fetch_add(1, Ordering::SeqCst);
            let folders = self.folders.lock().unwrap();
            Ok(folders.get(folder_id).cloned().unwrap_or_default())
        }

        fn open(&self, file_id: &str) -> io::Result<Box<dyn Reader>> {
            Ok(match self.contents.lock().unwrap().get(file_id) {
                Some(bytes) => Box::new(Stored(bytes.clone())),
                None => Box::new(FakeReader(self.reads.clone())),
            })
        }

        fn thumbnail(&self, file_id: &str) -> io::Result<Option<Vec<u8>>> {
            Ok((file_id == "a").then(|| vec![1, 2, 3]))
        }

        fn create_folder(&self, parent_id: &str, name: &str) -> io::Result<Node> {
            let folder = node(&self.fresh_id(), name, true);
            self.add(parent_id, folder.clone());
            Ok(folder)
        }

        fn upload(
            &self,
            parent_id: &str,
            name: &str,
            reader: &mut (dyn Read + Send),
            _size: u64,
            modified: Option<i64>,
        ) -> io::Result<Node> {
            let node = self.store(&self.fresh_id(), reader, modified, name)?;
            self.add(parent_id, node.clone());
            Ok(node)
        }

        fn upload_revision(
            &self,
            file_id: &str,
            reader: &mut (dyn Read + Send),
            _size: u64,
            modified: Option<i64>,
        ) -> io::Result<Node> {
            let (folder, old) = self.take(file_id).ok_or(io::ErrorKind::NotFound)?;
            let node = self.store(file_id, reader, modified, &old.name)?;
            self.add(&folder, node.clone());
            Ok(node)
        }

        fn rename(&self, id: &str, name: &str, _is_dir: bool) -> io::Result<()> {
            let (folder, mut node) = self.take(id).ok_or(io::ErrorKind::NotFound)?;
            node.name = name.to_owned();
            self.add(&folder, node);
            Ok(())
        }

        fn move_to(&self, id: &str, parent_id: &str) -> io::Result<()> {
            let (_, node) = self.take(id).ok_or(io::ErrorKind::NotFound)?;
            self.add(parent_id, node);
            Ok(())
        }

        fn copy(&self, id: &str, parent_id: &str, name: &str) -> io::Result<Node> {
            let contents = self.contents.lock().unwrap().get(id).cloned();
            let copy = Node {
                id: self.fresh_id(),
                name: name.to_owned(),
                is_dir: false,
                size: contents.as_ref().map_or(FILE_SIZE, Vec::len) as u64,
                modified: 2,
            };
            if let Some(contents) = contents {
                self.contents
                    .lock()
                    .unwrap()
                    .insert(copy.id.clone(), contents);
            }
            self.add(parent_id, copy.clone());
            Ok(copy)
        }

        fn trash(&self, id: &str) -> io::Result<()> {
            self.take(id).ok_or(io::ErrorKind::NotFound)?;
            self.trashed.lock().unwrap().push(id.to_owned());
            Ok(())
        }
    }

    fn signed_in() -> (Guard<'static, ()>, Arc<Fake>) {
        let guard = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
        let fake = Arc::new(Fake::default());
        set_backend(Some(fake.clone()));
        (guard, fake)
    }

    fn p(path: &str) -> PathBuf {
        PathBuf::from(format!("proton://drive{path}"))
    }

    fn read_all(path: &Path) -> Vec<u8> {
        let mut bytes = Vec::new();
        open(path).unwrap().read_to_end(&mut bytes).unwrap();
        bytes
    }

    fn names(path: &Path) -> Vec<String> {
        let mut names: Vec<String> = read_dir(path)
            .unwrap()
            .into_iter()
            .map(|listed| listed.name)
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_new_folder_is_there_at_once_and_noted() {
        let (_guard, fake) = signed_in();
        read_dir(&p("/Docs")).unwrap();
        take_changes();
        let lists = fake.lists.load(Ordering::SeqCst);
        create_dir(&p("/Docs/new")).unwrap();
        assert_eq!(names(&p("/Docs")), ["b.txt", "new"]);
        assert_eq!(fake.lists.load(Ordering::SeqCst), lists);
        assert!(take_changes().contains(&p("/Docs")));
        assert_eq!(
            create_dir(&p("/Docs/new")).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        create_dir_all(&p("/Docs/new/deep/er")).unwrap();
        assert!(stat(&p("/Docs/new/deep/er")).unwrap().is_dir());
    }

    #[test]
    fn a_new_file_reaches_the_drive_when_flushed() {
        let (_guard, _fake) = signed_in();
        let mut file = open_for_writing(
            &p("/Docs/new.txt"),
            Writing {
                create_new: true,
                ..Writing::default()
            },
        )
        .unwrap();
        file.write_all(b"hello").unwrap();
        assert_eq!(
            stat(&p("/Docs/new.txt")).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        file.flush().unwrap();
        assert_eq!(read_all(&p("/Docs/new.txt")), b"hello");
        assert_eq!(stat(&p("/Docs/new.txt")).unwrap().size, Some(5));
    }

    #[test]
    fn writing_over_a_file_makes_a_revision_of_it() {
        let (_guard, fake) = signed_in();
        let mut one: &[u8] = b"one";
        upload(&p("/Docs/f"), &mut one, 3, Some(5)).unwrap();
        let id = |fake: &Fake| {
            fake.folders.lock().unwrap()["docs"]
                .iter()
                .find(|node| node.name == "f")
                .unwrap()
                .id
                .clone()
        };
        let first = id(&fake);
        let mut two: &[u8] = b"two!";
        upload(&p("/Docs/f"), &mut two, 4, Some(6)).unwrap();
        assert_eq!(id(&fake), first);
        assert_eq!(read_all(&p("/Docs/f")), b"two!");
        let mut three: &[u8] = b"x";
        assert_eq!(
            upload_new(&p("/Docs/f"), &mut three, 1, None)
                .unwrap_err()
                .kind(),
            io::ErrorKind::AlreadyExists
        );
    }

    #[test]
    fn opening_without_truncating_keeps_whats_there() {
        let (_guard, _fake) = signed_in();
        let mut one: &[u8] = b"start";
        upload(&p("/Docs/f"), &mut one, 5, None).unwrap();
        let mut file = open_for_writing(
            &p("/Docs/f"),
            Writing {
                append: true,
                ..Writing::default()
            },
        )
        .unwrap();
        file.write_all(b"+more").unwrap();
        file.flush().unwrap();
        assert_eq!(read_all(&p("/Docs/f")), b"start+more");
    }

    #[test]
    fn dropping_an_unflushed_new_file_still_makes_it() {
        let (_guard, _fake) = signed_in();
        drop(
            open_for_writing(
                &p("/Docs/empty"),
                Writing {
                    create_new: true,
                    ..Writing::default()
                },
            )
            .unwrap(),
        );
        assert_eq!(stat(&p("/Docs/empty")).unwrap().size, Some(0));
    }

    #[test]
    fn reading_is_not_writing() {
        let (_guard, _fake) = signed_in();
        let mut file = open(&p("/a.bin")).unwrap();
        assert_eq!(
            file.write(b"x").unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn renames_moves_and_refuses_to_replace() {
        let (_guard, fake) = signed_in();
        rename(&p("/a.bin"), &p("/b.bin")).unwrap();
        assert!(stat(&p("/a.bin")).is_err());
        stat(&p("/b.bin")).unwrap();

        rename(&p("/b.bin"), &p("/Docs/c.bin")).unwrap();
        assert_eq!(names(&p("/Docs")), ["b.txt", "c.bin"]);
        assert!(
            fake.folders.lock().unwrap()["docs"]
                .iter()
                .any(|node| node.name == "c.bin")
        );

        assert_eq!(
            rename(&p("/Docs/c.bin"), &p("/Docs/b.txt"))
                .unwrap_err()
                .kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(
            rename(&p("/Docs"), &p("/Docs/inside")).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            rename(&top(), &p("/x")).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn a_moved_folders_contents_are_found_at_its_new_path() {
        let (_guard, _fake) = signed_in();
        read_dir(&p("/Docs")).unwrap();
        create_dir(&p("/Other")).unwrap();
        rename(&p("/Docs"), &p("/Other/Docs")).unwrap();
        assert!(!is_listed(&p("/Docs")));
        assert_eq!(names(&p("/Other/Docs")), ["b.txt"]);
    }

    #[test]
    fn removing_goes_to_the_trash() {
        let (_guard, fake) = signed_in();
        remove_file(&p("/a.bin")).unwrap();
        assert_eq!(*fake.trashed.lock().unwrap(), ["a"]);
        assert_eq!(
            remove_dir(&p("/Docs")).unwrap_err().kind(),
            io::ErrorKind::DirectoryNotEmpty
        );
        assert_eq!(
            remove_file(&p("/Docs")).unwrap_err().kind(),
            io::ErrorKind::IsADirectory
        );
        remove_dir_all(&p("/Docs")).unwrap();
        assert_eq!(*fake.trashed.lock().unwrap(), ["a", "docs"]);
        assert!(stat(&p("/Docs")).is_err());
        assert_eq!(
            remove_dir_all(&top()).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn copies_on_the_drive() {
        let (_guard, _fake) = signed_in();
        let mut one: &[u8] = b"data";
        upload(&p("/f"), &mut one, 4, None).unwrap();
        copy_within(&p("/f"), &p("/Docs/g")).unwrap();
        assert_eq!(read_all(&p("/Docs/g")), b"data");
        assert_eq!(
            copy_within(&p("/f"), &p("/Docs/g")).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
    }

    #[test]
    fn others_changes_are_noted_only_if_they_changed_something() {
        let (_guard, fake) = signed_in();
        read_dir(&p("/Docs")).unwrap();
        take_changes();
        assert!(!folder_changed(Some("docs")));
        assert!(take_changes().is_empty());

        fake.add("docs", node("o", "others.txt", false));
        assert!(!folder_changed(Some("root")));
        assert_eq!(names(&p("/Docs")), ["b.txt"]);
        assert!(folder_changed(Some("docs")));
        assert!(take_changes().contains(&p("/Docs")));
        assert_eq!(names(&p("/Docs")), ["b.txt", "others.txt"]);

        fake.add("root", node("o2", "top.txt", false));
        assert!(folder_changed(None));
        assert!(take_changes().contains(&top()));
    }

    #[test]
    fn the_ui_thread_can_write_where_it_can_see() {
        let (_guard, _fake) = signed_in();
        read_dir(&top()).unwrap();
        std::thread::spawn(|| {
            never_wait_on_this_thread();
            create_dir(&p("/made")).unwrap();
            rename(&p("/made"), &p("/renamed")).unwrap();
            stat(&p("/renamed")).unwrap();
        })
        .join()
        .unwrap();
    }

    #[test]
    fn paths_become_parts() {
        assert_eq!(parts(Path::new("proton://drive")), Some(vec![]));
        assert_eq!(parts(Path::new("proton://drive/")), Some(vec![]));
        assert_eq!(
            parts(Path::new("proton://drive/a b/c")),
            Some(vec!["a b".to_owned(), "c".to_owned()])
        );
        // As Windows joins them.
        assert_eq!(
            parts(Path::new(r"proton://drive\a")),
            Some(vec!["a".to_owned()])
        );
        assert_eq!(parts(Path::new("proton://drive/../x")), None);
        assert_eq!(parts(Path::new("proton://drives/x")), None);
        assert_eq!(parts(Path::new("/tmp/x")), None);
    }

    #[test]
    fn lists_and_finds_nested_files_with_one_fetch_each() {
        let (_guard, fake) = signed_in();
        let listed = read_dir(Path::new("proton://drive/Docs")).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].path, Path::new("proton://drive/Docs/b.txt"));
        assert_eq!(listed[0].name, "b.txt");
        assert_eq!(listed[0].attrs.kind(), S_IFREG);
        assert!(is_listed(Path::new("proton://drive/Docs")));
        assert_eq!(
            stat(Path::new("proton://drive/Docs")).unwrap().kind(),
            S_IFDIR
        );
        stat(Path::new("proton://drive/Docs/b.txt")).unwrap();
        assert_eq!(fake.lists.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn names_cannot_climb_or_split() {
        let (_guard, _fake) = signed_in();
        let names: Vec<String> = read_dir(&top())
            .unwrap()
            .into_iter()
            .map(|listed| listed.name)
            .collect();
        assert_eq!(names, ["Docs", "a.bin", "x\u{2215}y"]);
        stat(Path::new("proton://drive/x\u{2215}y")).unwrap();
    }

    #[test]
    fn missing_is_not_found() {
        let (_guard, _fake) = signed_in();
        let err = stat(Path::new("proton://drive/nope")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn forgetting_fetches_again() {
        let (_guard, fake) = signed_in();
        read_dir(Path::new("proton://drive/Docs")).unwrap();
        forget(Path::new("proton://drive/Docs"));
        assert!(!is_listed(Path::new("proton://drive/Docs")));
        assert!(is_listed(&top()));
        read_dir(Path::new("proton://drive/Docs")).unwrap();
        assert_eq!(fake.lists.load(Ordering::SeqCst), 3);
        forget(&top());
        assert!(!is_listed(&top()));
    }

    #[test]
    fn reads_in_chunks_and_seeks() {
        let (_guard, fake) = signed_in();
        let mut file = open(Path::new("proton://drive/a.bin")).unwrap();
        let mut head = [0; 100];
        file.read_exact(&mut head).unwrap();
        file.read_exact(&mut head).unwrap();
        assert_eq!(head[0], 100);
        assert_eq!(fake.reads.load(Ordering::SeqCst), 1);

        // Across the end of a chunk.
        file.seek(SeekFrom::Start((4 << 20) - 1)).unwrap();
        let mut across = [0; 2];
        file.read_exact(&mut across).unwrap();
        assert_eq!(across[0], (((4u64 << 20) - 1) % 251) as u8);
        assert_eq!(across[1], ((4u64 << 20) % 251) as u8);

        file.seek(SeekFrom::End(-1)).unwrap();
        let mut rest = Vec::new();
        file.read_to_end(&mut rest).unwrap();
        assert_eq!(rest.len(), 1);
        assert!(
            file.seek(SeekFrom::Current(-(FILE_SIZE as i64) - 1))
                .is_err()
        );
    }

    #[test]
    fn reads_a_whole_file() {
        let (_guard, _fake) = signed_in();
        let mut file = open(Path::new("proton://drive/a.bin")).unwrap();
        let mut all = Vec::new();
        file.read_to_end(&mut all).unwrap();
        assert_eq!(all.len(), FILE_SIZE);
        assert!(all.iter().enumerate().all(|(at, b)| *b == (at % 251) as u8));
    }

    #[test]
    fn folders_are_not_files() {
        let (_guard, _fake) = signed_in();
        assert_eq!(
            open(Path::new("proton://drive/Docs")).err().unwrap().kind(),
            io::ErrorKind::IsADirectory
        );
        assert_eq!(
            read_dir(Path::new("proton://drive/a.bin"))
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::NotADirectory
        );
        assert_eq!(thumbnail(Path::new("proton://drive/Docs")).unwrap(), None);
        assert_eq!(
            thumbnail(Path::new("proton://drive/a.bin")).unwrap(),
            Some(vec![1, 2, 3])
        );
    }

    #[test]
    fn signed_out_says_so_and_forgets() {
        let (_guard, _fake) = signed_in();
        read_dir(&top()).unwrap();
        set_backend(None);
        assert!(!is_listed(&top()));
        assert_eq!(
            read_dir(&top()).err().unwrap().kind(),
            io::ErrorKind::NotConnected
        );
    }

    #[test]
    fn the_ui_thread_only_gets_what_is_known() {
        let (_guard, fake) = signed_in();
        read_dir(&top()).unwrap();
        // A thread of its own, as the setting stays with the thread.
        let fake_lists = std::thread::spawn(|| {
            never_wait_on_this_thread();
            assert_eq!(read_dir(&top()).unwrap().len(), 3);
            assert_eq!(
                read_dir(Path::new("proton://drive/Docs"))
                    .err()
                    .unwrap()
                    .kind(),
                io::ErrorKind::WouldBlock
            );
        });
        fake_lists.join().unwrap();
        assert_eq!(fake.lists.load(Ordering::SeqCst), 1);
    }
}
