//! std's filesystem, and for `sftp://` paths, servers' ([`crate::sftp`]).
//!
//! The types are std's, wrapped, with std's methods: a local file's are
//! std's very own, and `as_std` gives what's wrapped for the rest. On Unix
//! they have std's extension traits too (`MetadataExt` and the like).

use std::{
    ffi::OsString,
    hash::{BuildHasher, BuildHasherDefault, DefaultHasher},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{Mutex, PoisonError},
    time::SystemTime,
};

use crate::sftp::{self, Attrs, Location};

/// Where `path` is: `None` if on this computer.
fn locate(path: &Path) -> io::Result<Option<Location>> {
    if !sftp::is_remote(path) {
        return Ok(None);
    }
    sftp::parse(path).map(Some).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("not a valid server path: {}", path.display()),
        )
    })
}

/// A stand-in device number for a server's files: the same for one
/// server's, different from other servers' and (almost surely) from local
/// devices'.
fn device_of(location: &Location) -> u64 {
    BuildHasherDefault::<DefaultHasher>::default().hash_one(&location.authority) | 1 << 63
}

fn different_computers() -> io::Error {
    io::Error::new(
        io::ErrorKind::CrossesDevices,
        "they're on different computers",
    )
}

// ---------------------------------------------------------------------------
// Types

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FileType(Kind);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Kind {
    Local(std::fs::FileType),
    /// The type bits of its mode.
    Remote(u32),
}

impl FileType {
    pub fn is_dir(&self) -> bool {
        match self.0 {
            Kind::Local(t) => t.is_dir(),
            Kind::Remote(kind) => kind == crate::sftp::S_IFDIR,
        }
    }

    pub fn is_file(&self) -> bool {
        match self.0 {
            Kind::Local(t) => t.is_file(),
            Kind::Remote(kind) => kind == crate::sftp::S_IFREG || kind == 0,
        }
    }

    pub fn is_symlink(&self) -> bool {
        match self.0 {
            Kind::Local(t) => t.is_symlink(),
            Kind::Remote(kind) => kind == crate::sftp::S_IFLNK,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Permissions(Perms);

#[derive(Clone, Debug, PartialEq, Eq)]
enum Perms {
    Local(std::fs::Permissions),
    /// The permission bits of its mode.
    Remote(u32),
}

impl Permissions {
    pub fn readonly(&self) -> bool {
        match &self.0 {
            Perms::Local(p) => p.readonly(),
            Perms::Remote(mode) => mode & 0o222 == 0,
        }
    }

    pub fn set_readonly(&mut self, readonly: bool) {
        match &mut self.0 {
            Perms::Local(p) => p.set_readonly(readonly),
            // Only the owner gets to write, rather than all as std does.
            Perms::Remote(mode) if readonly => *mode &= !0o222,
            Perms::Remote(mode) => *mode |= 0o200,
        }
    }

    pub fn as_std(&self) -> Option<&std::fs::Permissions> {
        match &self.0 {
            Perms::Local(p) => Some(p),
            Perms::Remote(_) => None,
        }
    }

    /// The Unix mode bits, given what's there now (as Windows has only
    /// read-only to say).
    #[cfg_attr(unix, allow(unused_variables))]
    fn mode(&self, current: impl FnOnce() -> u32) -> u32 {
        match &self.0 {
            Perms::Remote(mode) => *mode,
            #[cfg(unix)]
            Perms::Local(p) => std::os::unix::fs::PermissionsExt::mode(p),
            #[cfg(not(unix))]
            Perms::Local(p) => {
                let current = current();
                if p.readonly() {
                    current & !0o222
                } else {
                    current | 0o200
                }
            }
        }
    }

    /// As std's, to set on a local file.
    #[cfg_attr(unix, allow(unused_variables))]
    fn into_std(self, path: &Path) -> io::Result<std::fs::Permissions> {
        match self.0 {
            Perms::Local(p) => Ok(p),
            #[cfg(unix)]
            Perms::Remote(mode) => Ok(std::os::unix::fs::PermissionsExt::from_mode(mode)),
            #[cfg(not(unix))]
            Perms::Remote(mode) => {
                let mut p = std::fs::metadata(path)?.permissions();
                p.set_readonly(mode & 0o222 == 0);
                Ok(p)
            }
        }
    }
}

impl From<std::fs::Permissions> for Permissions {
    fn from(p: std::fs::Permissions) -> Self {
        Self(Perms::Local(p))
    }
}

#[cfg(unix)]
impl std::os::unix::fs::PermissionsExt for Permissions {
    fn mode(&self) -> u32 {
        self.mode(|| 0)
    }

    fn set_mode(&mut self, mode: u32) {
        match &mut self.0 {
            Perms::Local(p) => p.set_mode(mode),
            Perms::Remote(m) => *m = mode,
        }
    }

    fn from_mode(mode: u32) -> Self {
        Self(Perms::Local(std::fs::Permissions::from_mode(mode)))
    }
}

#[derive(Clone, Debug)]
pub struct Metadata(Meta);

#[derive(Clone, Debug)]
enum Meta {
    Local(std::fs::Metadata),
    #[cfg_attr(not(unix), allow(dead_code))]
    Remote { attrs: Attrs, device: u64 },
}

fn no_time() -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, "the server didn't say when")
}

// As std's: a file's length isn't a collection's.
#[allow(clippy::len_without_is_empty)]
impl Metadata {
    fn remote(attrs: Attrs, location: &Location) -> Self {
        Self(Meta::Remote {
            attrs,
            device: device_of(location),
        })
    }

    pub fn file_type(&self) -> FileType {
        FileType(match &self.0 {
            Meta::Local(m) => Kind::Local(m.file_type()),
            Meta::Remote { attrs, .. } => Kind::Remote(attrs.kind()),
        })
    }

    pub fn is_dir(&self) -> bool {
        self.file_type().is_dir()
    }

    pub fn is_file(&self) -> bool {
        self.file_type().is_file()
    }

    pub fn is_symlink(&self) -> bool {
        self.file_type().is_symlink()
    }

    pub fn len(&self) -> u64 {
        match &self.0 {
            Meta::Local(m) => m.len(),
            Meta::Remote { attrs, .. } => attrs.size.unwrap_or(0),
        }
    }

    pub fn permissions(&self) -> Permissions {
        Permissions(match &self.0 {
            Meta::Local(m) => Perms::Local(m.permissions()),
            Meta::Remote { attrs, .. } => Perms::Remote(attrs.mode.unwrap_or(0o644) & 0o7777),
        })
    }

    pub fn modified(&self) -> io::Result<SystemTime> {
        match &self.0 {
            Meta::Local(m) => m.modified(),
            Meta::Remote { attrs, .. } => attrs.modified().ok_or_else(no_time),
        }
    }

    pub fn accessed(&self) -> io::Result<SystemTime> {
        match &self.0 {
            Meta::Local(m) => m.accessed(),
            Meta::Remote { attrs, .. } => attrs.accessed().ok_or_else(no_time),
        }
    }

    /// SFTP doesn't tell.
    pub fn created(&self) -> io::Result<SystemTime> {
        match &self.0 {
            Meta::Local(m) => m.created(),
            Meta::Remote { .. } => Err(no_time()),
        }
    }

    pub fn as_std(&self) -> Option<&std::fs::Metadata> {
        match &self.0 {
            Meta::Local(m) => Some(m),
            Meta::Remote { .. } => None,
        }
    }

    /// Whether it's on a server.
    pub fn is_remote(&self) -> bool {
        matches!(self.0, Meta::Remote { .. })
    }

    /// The owner's and group's ids, if it's on a server that said. They're
    /// the server's: names for them here would be this computer's.
    pub fn remote_owner(&self) -> Option<(u32, u32)> {
        match &self.0 {
            Meta::Local(_) => None,
            Meta::Remote { attrs, .. } => attrs.uid_gid,
        }
    }
}

impl From<std::fs::Metadata> for Metadata {
    fn from(m: std::fs::Metadata) -> Self {
        Self(Meta::Local(m))
    }
}

#[cfg(unix)]
impl std::os::unix::fs::MetadataExt for Metadata {
    fn dev(&self) -> u64 {
        match &self.0 {
            Meta::Local(m) => m.dev(),
            Meta::Remote { device, .. } => *device,
        }
    }

    /// SFTP 3 doesn't tell, so 0, with one link each, so none look alike.
    fn ino(&self) -> u64 {
        self.as_std().map_or(0, std::os::unix::fs::MetadataExt::ino)
    }

    fn mode(&self) -> u32 {
        match &self.0 {
            Meta::Local(m) => m.mode(),
            Meta::Remote { attrs, .. } => attrs.mode.unwrap_or(0),
        }
    }

    fn nlink(&self) -> u64 {
        self.as_std().map_or(1, std::os::unix::fs::MetadataExt::nlink)
    }

    fn uid(&self) -> u32 {
        match &self.0 {
            Meta::Local(m) => m.uid(),
            Meta::Remote { attrs, .. } => attrs.uid_gid.map_or(0, |(uid, _)| uid),
        }
    }

    fn gid(&self) -> u32 {
        match &self.0 {
            Meta::Local(m) => m.gid(),
            Meta::Remote { attrs, .. } => attrs.uid_gid.map_or(0, |(_, gid)| gid),
        }
    }

    fn rdev(&self) -> u64 {
        self.as_std().map_or(0, std::os::unix::fs::MetadataExt::rdev)
    }

    fn size(&self) -> u64 {
        self.len()
    }

    fn atime(&self) -> i64 {
        match &self.0 {
            Meta::Local(m) => m.atime(),
            Meta::Remote { attrs, .. } => attrs.atime_mtime.map_or(0, |(a, _)| a.into()),
        }
    }

    fn atime_nsec(&self) -> i64 {
        self.as_std().map_or(0, std::os::unix::fs::MetadataExt::atime_nsec)
    }

    fn mtime(&self) -> i64 {
        match &self.0 {
            Meta::Local(m) => m.mtime(),
            Meta::Remote { attrs, .. } => attrs.atime_mtime.map_or(0, |(_, m)| m.into()),
        }
    }

    fn mtime_nsec(&self) -> i64 {
        self.as_std().map_or(0, std::os::unix::fs::MetadataExt::mtime_nsec)
    }

    /// SFTP doesn't tell; when it was changed is the nearest.
    fn ctime(&self) -> i64 {
        match &self.0 {
            Meta::Local(m) => m.ctime(),
            Meta::Remote { .. } => self.mtime(),
        }
    }

    fn ctime_nsec(&self) -> i64 {
        self.as_std().map_or(0, std::os::unix::fs::MetadataExt::ctime_nsec)
    }

    fn blksize(&self) -> u64 {
        self.as_std()
            .map_or(4096, std::os::unix::fs::MetadataExt::blksize)
    }

    /// For a server's file, its size, as if it took no more.
    fn blocks(&self) -> u64 {
        match &self.0 {
            Meta::Local(m) => m.blocks(),
            Meta::Remote { attrs, .. } => attrs.size.unwrap_or(0).div_ceil(512),
        }
    }
}

#[cfg(windows)]
impl Metadata {
    /// Windows' attributes; a server's files have none.
    pub fn file_attributes(&self) -> u32 {
        use std::os::windows::fs::MetadataExt;
        self.as_std().map_or(0, MetadataExt::file_attributes)
    }
}

pub struct DirEntry(Entry);

enum Entry {
    Local(std::fs::DirEntry),
    Remote(sftp::Listed),
}

impl DirEntry {
    pub fn path(&self) -> PathBuf {
        match &self.0 {
            Entry::Local(e) => e.path(),
            Entry::Remote(e) => e.location.to_path(),
        }
    }

    pub fn file_name(&self) -> OsString {
        match &self.0 {
            Entry::Local(e) => e.file_name(),
            Entry::Remote(e) => e.name.clone().into(),
        }
    }

    /// Links not followed, as std.
    pub fn metadata(&self) -> io::Result<Metadata> {
        match &self.0 {
            Entry::Local(e) => e.metadata().map(Metadata::from),
            Entry::Remote(e) => Ok(Metadata::remote(e.link.clone(), &e.location)),
        }
    }

    pub fn file_type(&self) -> io::Result<FileType> {
        match &self.0 {
            Entry::Local(e) => e.file_type().map(|t| FileType(Kind::Local(t))),
            Entry::Remote(e) => Ok(FileType(Kind::Remote(e.link.kind()))),
        }
    }
}

pub struct ReadDir(Listing);

enum Listing {
    Local(std::fs::ReadDir),
    Remote(std::vec::IntoIter<sftp::Listed>),
}

impl Iterator for ReadDir {
    type Item = io::Result<DirEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        match &mut self.0 {
            Listing::Local(entries) => Some(entries.next()?.map(|e| DirEntry(Entry::Local(e)))),
            Listing::Remote(entries) => Some(Ok(DirEntry(Entry::Remote(entries.next()?)))),
        }
    }
}

#[derive(Clone, Debug)]
pub struct OpenOptions {
    std: std::fs::OpenOptions,
    remote: sftp::Open,
}

impl Default for OpenOptions {
    fn default() -> Self {
        Self::new()
    }
}

impl OpenOptions {
    pub fn new() -> Self {
        Self {
            std: std::fs::OpenOptions::new(),
            remote: sftp::Open::default(),
        }
    }

    pub fn read(&mut self, read: bool) -> &mut Self {
        self.std.read(read);
        self.remote.read = read;
        self
    }

    pub fn write(&mut self, write: bool) -> &mut Self {
        self.std.write(write);
        self.remote.write = write;
        self
    }

    pub fn append(&mut self, append: bool) -> &mut Self {
        self.std.append(append);
        self.remote.append = append;
        self
    }

    pub fn truncate(&mut self, truncate: bool) -> &mut Self {
        self.std.truncate(truncate);
        self.remote.truncate = truncate;
        self
    }

    pub fn create(&mut self, create: bool) -> &mut Self {
        self.std.create(create);
        self.remote.create = create;
        self
    }

    pub fn create_new(&mut self, create_new: bool) -> &mut Self {
        self.std.create_new(create_new);
        self.remote.create_new = create_new;
        self
    }

    pub fn open(&self, path: impl AsRef<Path>) -> io::Result<File> {
        let path = path.as_ref();
        match locate(path)? {
            None => self.std.open(path).map(File::from),
            Some(location) => Ok(File(Handle::Remote(Box::new(Mutex::new(sftp::open(
                &location,
                self.remote,
            )?))))),
        }
    }
}

#[cfg(unix)]
impl std::os::unix::fs::OpenOptionsExt for OpenOptions {
    fn mode(&mut self, mode: u32) -> &mut Self {
        self.std.mode(mode);
        self.remote.mode = Some(mode);
        self
    }

    /// Only for local files.
    fn custom_flags(&mut self, flags: i32) -> &mut Self {
        self.std.custom_flags(flags);
        self
    }
}

#[derive(Debug)]
pub struct File(Handle);

enum Handle {
    Local(std::fs::File),
    Remote(Box<Mutex<sftp::File>>),
}

impl std::fmt::Debug for Handle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Local(file) => file.fmt(f),
            Self::Remote(file) => {
                let file = file.lock().unwrap_or_else(PoisonError::into_inner);
                write!(f, "File({})", file.location().to_path().display())
            }
        }
    }
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

    /// The server's file, locked; `None` for a local one.
    fn remote(&self) -> Option<std::sync::MutexGuard<'_, sftp::File>> {
        match &self.0 {
            Handle::Local(_) => None,
            Handle::Remote(file) => Some(file.lock().unwrap_or_else(PoisonError::into_inner)),
        }
    }

    pub fn metadata(&self) -> io::Result<Metadata> {
        match &self.0 {
            Handle::Local(file) => file.metadata().map(Metadata::from),
            Handle::Remote(_) => {
                let mut file = self.remote().expect("remote");
                let attrs = file.metadata()?;
                Ok(Metadata::remote(attrs, file.location()))
            }
        }
    }

    pub fn set_len(&self, len: u64) -> io::Result<()> {
        match &self.0 {
            Handle::Local(file) => file.set_len(len),
            Handle::Remote(_) => self.remote().expect("remote").set_len(len),
        }
    }

    pub fn set_modified(&self, modified: SystemTime) -> io::Result<()> {
        match &self.0 {
            Handle::Local(file) => file.set_modified(modified),
            Handle::Remote(_) => self.remote().expect("remote").set_times(None, modified),
        }
    }

    pub fn set_permissions(&self, permissions: Permissions) -> io::Result<()> {
        match &self.0 {
            Handle::Local(file) => {
                let std = match permissions.0 {
                    Perms::Local(p) => p,
                    #[cfg(unix)]
                    Perms::Remote(mode) => std::os::unix::fs::PermissionsExt::from_mode(mode),
                    #[cfg(not(unix))]
                    Perms::Remote(mode) => {
                        let mut p = file.metadata()?.permissions();
                        p.set_readonly(mode & 0o222 == 0);
                        p
                    }
                };
                file.set_permissions(std)
            }
            Handle::Remote(_) => {
                let mut file = self.remote().expect("remote");
                let mode = permissions.mode(|| {
                    file.metadata()
                        .ok()
                        .and_then(|a| a.mode)
                        .unwrap_or(0o644)
                });
                file.set_mode(mode)
            }
        }
    }

    pub fn sync_all(&self) -> io::Result<()> {
        match &self.0 {
            Handle::Local(file) => file.sync_all(),
            Handle::Remote(_) => self.remote().expect("remote").sync(),
        }
    }

    pub fn sync_data(&self) -> io::Result<()> {
        match &self.0 {
            Handle::Local(file) => file.sync_data(),
            Handle::Remote(_) => self.remote().expect("remote").sync(),
        }
    }

    pub fn try_clone(&self) -> io::Result<Self> {
        match &self.0 {
            Handle::Local(file) => file.try_clone().map(Self::from),
            Handle::Remote(_) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "a server's files can't be cloned",
            )),
        }
    }

    pub fn as_std(&self) -> Option<&std::fs::File> {
        match &self.0 {
            Handle::Local(file) => Some(file),
            Handle::Remote(_) => None,
        }
    }
}

impl From<std::fs::File> for File {
    fn from(file: std::fs::File) -> Self {
        Self(Handle::Local(file))
    }
}

#[cfg(unix)]
impl From<std::os::fd::OwnedFd> for File {
    fn from(fd: std::os::fd::OwnedFd) -> Self {
        Self(Handle::Local(fd.into()))
    }
}

impl Read for File {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match &mut self.0 {
            Handle::Local(file) => file.read(buf),
            Handle::Remote(file) => file
                .get_mut()
                .unwrap_or_else(PoisonError::into_inner)
                .read(buf),
        }
    }
}

impl Write for File {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match &mut self.0 {
            Handle::Local(file) => file.write(buf),
            Handle::Remote(file) => file
                .get_mut()
                .unwrap_or_else(PoisonError::into_inner)
                .write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match &mut self.0 {
            Handle::Local(file) => file.flush(),
            Handle::Remote(file) => file.get_mut().unwrap_or_else(PoisonError::into_inner).flush(),
        }
    }
}

impl Seek for File {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        match &mut self.0 {
            Handle::Local(file) => file.seek(pos),
            Handle::Remote(file) => file
                .get_mut()
                .unwrap_or_else(PoisonError::into_inner)
                .seek(pos),
        }
    }
}

// ---------------------------------------------------------------------------
// Functions

pub fn metadata(path: impl AsRef<Path>) -> io::Result<Metadata> {
    let path = path.as_ref();
    match locate(path)? {
        None => std::fs::metadata(path).map(Metadata::from),
        Some(location) => Ok(Metadata::remote(sftp::stat(&location, true)?, &location)),
    }
}

pub fn symlink_metadata(path: impl AsRef<Path>) -> io::Result<Metadata> {
    let path = path.as_ref();
    match locate(path)? {
        None => std::fs::symlink_metadata(path).map(Metadata::from),
        Some(location) => Ok(Metadata::remote(sftp::stat(&location, false)?, &location)),
    }
}

pub fn exists(path: impl AsRef<Path>) -> io::Result<bool> {
    let path = path.as_ref();
    match locate(path)? {
        None => std::fs::exists(path),
        Some(location) => match sftp::stat(&location, true) {
            Ok(_) => Ok(true),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(err),
        },
    }
}

pub fn read_dir(path: impl AsRef<Path>) -> io::Result<ReadDir> {
    let path = path.as_ref();
    Ok(ReadDir(match locate(path)? {
        None => Listing::Local(std::fs::read_dir(path)?),
        Some(location) => Listing::Remote(sftp::read_dir(&location)?.into_iter()),
    }))
}

pub fn read(path: impl AsRef<Path>) -> io::Result<Vec<u8>> {
    let path = path.as_ref();
    if !sftp::is_remote(path) {
        return std::fs::read(path);
    }
    let mut file = File::open(path)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

pub fn read_to_string(path: impl AsRef<Path>) -> io::Result<String> {
    String::from_utf8(read(path)?).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "stream did not contain valid UTF-8",
        )
    })
}

pub fn write(path: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> io::Result<()> {
    let path = path.as_ref();
    if !sftp::is_remote(path) {
        return std::fs::write(path, contents);
    }
    let mut file = File::create(path)?;
    file.write_all(contents.as_ref())?;
    file.flush()
}

pub fn create_dir(path: impl AsRef<Path>) -> io::Result<()> {
    let path = path.as_ref();
    match locate(path)? {
        None => std::fs::create_dir(path),
        Some(location) => sftp::create_dir(&location),
    }
}

pub fn create_dir_all(path: impl AsRef<Path>) -> io::Result<()> {
    let path = path.as_ref();
    match locate(path)? {
        None => std::fs::create_dir_all(path),
        Some(location) => create_remote_dir_all(&location),
    }
}

fn create_remote_dir_all(location: &Location) -> io::Result<()> {
    if sftp::stat(location, true).is_ok_and(|attrs| attrs.is_dir()) {
        return Ok(());
    }
    if let Some(parent) = location.parent() {
        create_remote_dir_all(&parent)?;
    }
    match sftp::create_dir(location) {
        Err(err)
            if err.kind() == io::ErrorKind::AlreadyExists
                && sftp::stat(location, true).is_ok_and(|attrs| attrs.is_dir()) =>
        {
            Ok(())
        }
        result => result,
    }
}

pub fn remove_file(path: impl AsRef<Path>) -> io::Result<()> {
    let path = path.as_ref();
    match locate(path)? {
        None => std::fs::remove_file(path),
        Some(location) => sftp::remove_file(&location),
    }
}

pub fn remove_dir(path: impl AsRef<Path>) -> io::Result<()> {
    let path = path.as_ref();
    match locate(path)? {
        None => std::fs::remove_dir(path),
        Some(location) => sftp::remove_dir(&location),
    }
}

pub fn remove_dir_all(path: impl AsRef<Path>) -> io::Result<()> {
    let path = path.as_ref();
    match locate(path)? {
        None => std::fs::remove_dir_all(path),
        Some(location) => sftp::remove_dir_all(&location),
    }
}

/// Within one computer only: across them it fails with `CrossesDevices`,
/// as across local devices.
pub fn rename(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<()> {
    let (from, to) = (from.as_ref(), to.as_ref());
    match (locate(from)?, locate(to)?) {
        (None, None) => std::fs::rename(from, to),
        (Some(from), Some(to)) => sftp::rename(&from, &to),
        _ => Err(different_computers()),
    }
}

/// Copies the contents and permissions, as std does, between any two
/// places.
pub fn copy(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<u64> {
    let (from, to) = (from.as_ref(), to.as_ref());
    if !sftp::is_remote(from) && !sftp::is_remote(to) {
        return std::fs::copy(from, to);
    }
    let mut source = File::open(from)?;
    let permissions = source.metadata()?.permissions();
    let mut dest = File::create(to)?;
    let copied = io::copy(&mut source, &mut dest)?;
    dest.flush()?;
    dest.set_permissions(permissions)?;
    Ok(copied)
}

pub fn hard_link(original: impl AsRef<Path>, link: impl AsRef<Path>) -> io::Result<()> {
    let (original, link) = (original.as_ref(), link.as_ref());
    match (locate(original)?, locate(link)?) {
        (None, None) => std::fs::hard_link(original, link),
        (Some(original), Some(link)) => sftp::hard_link(&original, &link),
        _ => Err(different_computers()),
    }
}

/// Makes a link at `link` leading to `target`, kept as given. Where it's
/// made decides which computer's it is.
pub fn symlink(target: impl AsRef<Path>, link: impl AsRef<Path>) -> io::Result<()> {
    let (target, link) = (target.as_ref(), link.as_ref());
    match locate(link)? {
        Some(location) => sftp::symlink(target, &location),
        #[cfg(unix)]
        None => std::os::unix::fs::symlink(target, link),
        #[cfg(windows)]
        None => {
            let base = link.parent().unwrap_or(Path::new("."));
            if std::fs::metadata(base.join(target)).is_ok_and(|m| m.is_dir()) {
                std::os::windows::fs::symlink_dir(target, link)
            } else {
                std::os::windows::fs::symlink_file(target, link)
            }
        }
    }
}

pub fn read_link(path: impl AsRef<Path>) -> io::Result<PathBuf> {
    let path = path.as_ref();
    match locate(path)? {
        None => std::fs::read_link(path),
        Some(location) => sftp::read_link(&location),
    }
}

pub fn canonicalize(path: impl AsRef<Path>) -> io::Result<PathBuf> {
    let path = path.as_ref();
    match locate(path)? {
        None => std::fs::canonicalize(path),
        Some(location) => Ok(sftp::canonicalize(&location)?.to_path()),
    }
}

pub fn set_permissions(path: impl AsRef<Path>, permissions: Permissions) -> io::Result<()> {
    let path = path.as_ref();
    match locate(path)? {
        None => std::fs::set_permissions(path, permissions.into_std(path)?),
        Some(location) => {
            let mode = permissions.mode(|| {
                sftp::stat(&location, true)
                    .ok()
                    .and_then(|attrs| attrs.mode)
                    .unwrap_or(0o644)
            });
            sftp::set_mode(&location, mode)
        }
    }
}

/// Sets when `path` was last read and changed, following links.
pub fn set_file_times(
    path: impl AsRef<Path>,
    accessed: SystemTime,
    modified: SystemTime,
) -> io::Result<()> {
    let path = path.as_ref();
    match locate(path)? {
        None => filetime::set_file_times(
            path,
            filetime::FileTime::from_system_time(accessed),
            filetime::FileTime::from_system_time(modified),
        ),
        Some(location) => sftp::set_times(&location, Some(accessed), modified),
    }
}

/// Sets when `path` was last changed, following links.
pub fn set_file_mtime(path: impl AsRef<Path>, modified: SystemTime) -> io::Result<()> {
    let path = path.as_ref();
    match locate(path)? {
        None => filetime::set_file_mtime(path, filetime::FileTime::from_system_time(modified)),
        Some(location) => sftp::set_times(&location, None, modified),
    }
}

/// Paths changed on servers since the last call, as a folder watcher would
/// report them: nothing watches servers' folders.
pub fn take_changes() -> std::collections::BTreeSet<PathBuf> {
    sftp::take_changes()
}
