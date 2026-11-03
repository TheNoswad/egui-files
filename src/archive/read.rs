//! Listing and unpacking each archive format.

use disk::File;
use std::{
    io::{self, BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use super::{Entry, Error, Format, Listing, Selection, normalize, safe_relative};
use crate::jobs::Progress;

const BUFFER_SIZE: usize = 1 << 20;
/// 7z stores Unix permissions in the high half of the Windows attributes.
const UNIX_EXTENSION: u32 = 0x8000;
const S_IFMT: u32 = 0o170000;
const S_IFLNK: u32 = 0o120000;

/// Reads from the archive file, reporting the position as progress and
/// failing once the job is cancelled.
struct Tracked<'a, R> {
    inner: R,
    pos: u64,
    p: Option<&'a Progress>,
}

impl<'a, R> Tracked<'a, R> {
    fn new(inner: R, p: Option<&'a Progress>) -> Self {
        Self { inner, pos: 0, p }
    }
}

impl<R: Read> Read for Tracked<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.p.is_some_and(Progress::cancelled) {
            // Not `Interrupted`: readers retry those.
            return Err(io::Error::other("cancelled"));
        }
        let n = self.inner.read(buf)?;
        self.pos += n as u64;
        if let Some(p) = self.p {
            p.set_done_bytes(self.pos);
        }
        Ok(n)
    }
}

impl<R: Seek> Seek for Tracked<'_, R> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        // Not reported: zip and 7z seek to their index at the end first.
        self.pos = self.inner.seek(pos)?;
        Ok(self.pos)
    }
}

fn open<'a>(path: &Path, p: Option<&'a Progress>) -> Result<Tracked<'a, BufReader<File>>, Error> {
    let file = File::open(path).map_err(|e| Error::Other(format!("cannot open: {e}")))?;
    Ok(Tracked::new(BufReader::with_capacity(BUFFER_SIZE, file), p))
}

fn threads() -> u32 {
    std::thread::available_parallelism().map_or(1, |n| n.get() as u32)
}

// ---------------------------------------------------------------------------
// Listing

pub fn list(path: &Path, password: Option<&str>) -> Result<Listing, Error> {
    let format = Format::detect(path).ok_or_else(|| Error::Other("not an archive".into()))?;
    let archive_size = disk::metadata(path).map_or(0, |m| m.len());
    let entries = match format {
        Format::Zip => list_zip(path)?,
        Format::SevenZ => list_7z(path, password)?,
        Format::Rar => list_rar(path, password)?,
        Format::Gz | Format::Xz | Format::Zst | Format::Bz2 => vec![Entry {
            path: super::stem(path),
            is_dir: false,
            size: single_file_size(path, format).unwrap_or(0),
            packed: Some(archive_size),
            modified: disk::metadata(path).and_then(|m| m.modified()).ok(),
            encrypted: false,
        }],
        Format::Tar | Format::TarGz | Format::TarXz | Format::TarZst | Format::TarBz2 => {
            list_tar(path, format)?
        }
    };
    Ok(Listing {
        format,
        entries,
        archive_size,
    })
}

fn list_zip(path: &Path) -> Result<Vec<Entry>, Error> {
    let mut archive = zip::ZipArchive::new(open(path, None)?).map_err(zip_error)?;
    let mut entries = Vec::with_capacity(archive.len());
    for i in 0..archive.len() {
        let file = archive.by_index_raw(i).map_err(zip_error)?;
        let name = file.name().map_err(zip_error)?;
        entries.push(Entry {
            path: normalize(&name),
            is_dir: file.is_dir(),
            size: file.size(),
            packed: Some(file.compressed_size()),
            modified: file.last_modified().and_then(zip_time),
            encrypted: file.encrypted(),
        });
    }
    Ok(entries)
}

fn list_tar(path: &Path, format: Format) -> Result<Vec<Entry>, Error> {
    let mut archive = tar::Archive::new(decoder(format, open(path, None)?)?);
    let mut entries = Vec::new();
    for entry in archive.entries()? {
        let entry = entry?;
        let header = entry.header();
        entries.push(Entry {
            path: normalize(&entry.path()?.to_string_lossy()),
            is_dir: header.entry_type().is_dir(),
            size: entry.size(),
            packed: None,
            modified: header.mtime().ok().map(unix_time),
            encrypted: false,
        });
    }
    Ok(entries)
}

fn list_7z(path: &Path, password: Option<&str>) -> Result<Vec<Entry>, Error> {
    let reader = open_7z(path, password, None)?;
    let archive = reader.archive();
    let encrypted = archive.blocks.iter().any(|block| {
        block
            .coders
            .iter()
            .any(|c| c.encoder_method_id() == sevenz_rust2::EncoderMethod::ID_AES256_SHA256)
    });
    Ok(archive
        .files
        .iter()
        .map(|file| Entry {
            path: normalize(&file.name),
            is_dir: file.is_directory,
            size: file.size,
            packed: None,
            modified: file
                .has_last_modified_date
                .then(|| nt_to_system(file.last_modified_date))
                .flatten(),
            encrypted: encrypted && file.has_stream,
        })
        .collect())
}

#[cfg(not(target_arch = "wasm32"))]
fn list_rar(path: &Path, password: Option<&str>) -> Result<Vec<Entry>, Error> {
    let archive = match password {
        Some(password) => unrar::Archive::with_password(path, password),
        None => unrar::Archive::new(path),
    };
    let mut entries = Vec::new();
    for header in archive
        .open_for_listing()
        .map_err(|e| rar_error(e, password))?
    {
        let header = header.map_err(|e| rar_error(e, password))?;
        entries.push(Entry {
            path: normalize(&header.filename.to_string_lossy()),
            is_dir: header.is_directory(),
            size: header.unpacked_size,
            packed: None,
            modified: dos_time(header.file_time),
            encrypted: header.is_encrypted(),
        });
    }
    Ok(entries)
}

/// The uncompressed size, where the format makes it cheap to find.
fn single_file_size(path: &Path, format: Format) -> Option<u64> {
    match format {
        Format::Gz => {
            // ISIZE: the last four bytes, modulo 4 GiB.
            let mut file = File::open(path).ok()?;
            file.seek(SeekFrom::End(-4)).ok()?;
            let mut size = [0; 4];
            file.read_exact(&mut size).ok()?;
            Some(u32::from_le_bytes(size).into())
        }
        Format::Xz => lzma_rust2::xz_uncompressed_size(File::open(path).ok()?).ok(),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Unpacking

/// Unpacks `archive` into `root`, which should be empty.
pub async fn unpack(
    archive: &Path,
    root: &Path,
    selection: Option<&Selection>,
    password: Option<&str>,
    p: &Progress,
) -> Result<(), Error> {
    let format = Format::detect(archive).ok_or_else(|| Error::Other("not an archive".into()))?;
    let size = disk::metadata(archive).map_or(0, |m| m.len());
    let mut out = Unpacker {
        root,
        selection,
        p,
        buffer: vec![0; BUFFER_SIZE],
        symlinks: Vec::new(),
        dirs: Vec::new(),
    };
    let result = match format {
        Format::Zip => {
            p.add_totals(size, 0);
            unpack_zip(archive, password, &mut out).await
        }
        Format::SevenZ => {
            p.add_totals(size, 0);
            unpack_7z(archive, password, &mut out)
        }
        Format::Rar => unpack_rar(archive, password, &mut out),
        Format::Gz | Format::Xz | Format::Zst | Format::Bz2 => {
            p.add_totals(size, 1);
            let modified = disk::metadata(archive).and_then(|m| m.modified()).ok();
            let mut data = decoder(format, open(archive, Some(p))?)?;
            out.file(
                &super::stem(archive),
                Meta {
                    mode: None,
                    modified,
                },
                &mut data,
            )
        }
        Format::Tar | Format::TarGz | Format::TarXz | Format::TarZst | Format::TarBz2 => {
            p.add_totals(size, 0);
            unpack_tar(archive, format, &mut out).await
        }
    };
    let result = result.and_then(|()| out.finish());
    if p.cancelled() {
        return Err(Error::Cancelled);
    }
    result
}

#[derive(Clone, Copy, Default)]
struct Meta {
    mode: Option<u32>,
    modified: Option<SystemTime>,
}

/// Writes entries below `root`: checks names, creates folders, reports
/// progress, and sets times and permissions.
struct Unpacker<'a> {
    root: &'a Path,
    selection: Option<&'a Selection>,
    p: &'a Progress,
    buffer: Vec<u8>,
    /// Made last, so no file is ever written through a link from the archive.
    symlinks: Vec<(PathBuf, PathBuf)>,
    /// Get their times and permissions last, after their contents are written.
    dirs: Vec<(PathBuf, Meta)>,
}

impl Unpacker<'_> {
    /// Where an entry goes, or `None` to skip it.
    fn target(&self, name: &str) -> Option<PathBuf> {
        let name = normalize(name);
        let name = match self.selection {
            Some(selection) => selection.map(&name)?,
            None => name,
        };
        match safe_relative(&name) {
            Some(relative) => Some(self.root.join(relative)),
            None => {
                if !name.is_empty() {
                    self.p
                        .error(Path::new(&name), "skipped: it would be outside the folder");
                }
                None
            }
        }
    }

    fn make_parent(&self, path: &Path) -> io::Result<()> {
        match path.parent() {
            Some(parent) => disk::create_dir_all(parent),
            None => Ok(()),
        }
    }

    fn dir(&mut self, name: &str, meta: Meta) {
        let Some(path) = self.target(name) else {
            return;
        };
        match disk::create_dir_all(&path) {
            Ok(()) => self.dirs.push((path, meta)),
            Err(err) => self.p.error(&path, err),
        }
    }

    /// Errors reading `data` end the extraction; errors writing only skip the file.
    fn file(&mut self, name: &str, meta: Meta, data: &mut dyn Read) -> Result<(), Error> {
        self.p.add_done(0, 1);
        let Some(path) = self.target(name) else {
            return Ok(());
        };
        self.p.set_current(&path);
        let mut out = match self.make_parent(&path).and_then(|()| {
            // Later entries with the same name replace earlier ones, like tar does.
            let _ = disk::remove_file(&path);
            File::create_new(&path)
        }) {
            Ok(out) => Some(out),
            Err(err) => {
                self.p.error(&path, err);
                None
            }
        };
        loop {
            let n = match data.read(&mut self.buffer) {
                Ok(0) => break,
                Ok(n) => n,
                Err(err) => {
                    drop(out);
                    let _ = disk::remove_file(&path);
                    return Err(err.into());
                }
            };
            if let Some(file) = &mut out
                && let Err(err) = file.write_all(&self.buffer[..n])
            {
                self.p.error(&path, err);
                out = None;
                let _ = disk::remove_file(&path);
            }
        }
        if let Some(file) = out {
            if let Some(modified) = meta.modified {
                let _ = file.set_modified(modified);
            }
            set_mode(&path, meta.mode);
        }
        Ok(())
    }

    fn symlink(&mut self, name: &str, target: &Path) {
        self.p.add_done(0, 1);
        if let Some(path) = self.target(name) {
            self.symlinks.push((path, target.to_path_buf()));
        }
    }

    fn hardlink(&mut self, name: &str, existing: &str) {
        self.p.add_done(0, 1);
        let Some(path) = self.target(name) else {
            return;
        };
        let Some(existing) = self.target(existing) else {
            return self
                .p
                .error(&path, "skipped: links to an item that wasn't extracted");
        };
        let result = self.make_parent(&path).and_then(|()| {
            let _ = disk::remove_file(&path);
            disk::hard_link(&existing, &path).or_else(|_| disk::copy(&existing, &path).map(drop))
        });
        if let Err(err) = result {
            self.p.error(&path, err);
        }
    }

    fn finish(&mut self) -> Result<(), Error> {
        for (path, target) in std::mem::take(&mut self.symlinks) {
            if let Err(err) = self
                .make_parent(&path)
                .and_then(|()| make_symlink(&target, &path))
            {
                self.p.error(&path, err);
            }
        }
        // Deepest first, so setting a parent's time comes after its children change it.
        for (path, meta) in self.dirs.drain(..).rev() {
            if let Some(modified) = meta.modified {
                let _ = disk::set_file_mtime(&path, modified);
            }
            set_mode(&path, meta.mode);
        }
        Ok(())
    }
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: Option<u32>) {
    use std::os::unix::fs::PermissionsExt;
    // Owner rwx at least, so nothing extracted ends up unreadable or stuck.
    if let Some(mode) = mode.map(|m| m & 0o777).filter(|&m| m != 0) {
        let _ = disk::set_permissions(path, disk::Permissions::from_mode(mode | 0o600));
    }
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: Option<u32>) {}

/// Unpacked symlinks are only tried, so the browser just reports them.
#[cfg(target_arch = "wasm32")]
fn make_symlink(_target: &Path, _path: &Path) -> io::Result<()> {
    Err(io::ErrorKind::Unsupported.into())
}

#[cfg(unix)]
fn make_symlink(target: &Path, path: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, path)
}

#[cfg(windows)]
fn make_symlink(target: &Path, path: &Path) -> io::Result<()> {
    // Usually needs developer mode; the error explains it if not.
    std::os::windows::fs::symlink_file(target, path)
}

async fn unpack_zip(
    path: &Path,
    password: Option<&str>,
    out: &mut Unpacker<'_>,
) -> Result<(), Error> {
    let mut archive = zip::ZipArchive::new(open(path, Some(out.p))?).map_err(zip_error)?;
    out.p.add_totals(0, archive.len() as u64);
    for i in 0..archive.len() {
        out.p.pause().await;
        let encrypted = archive.by_index_raw(i).map_err(zip_error)?.encrypted();
        let mut file = if encrypted {
            let password = password.ok_or(Error::NeedsPassword)?;
            match archive.by_index_decrypt(i, password.as_bytes()) {
                Err(zip::result::ZipError::InvalidPassword) => return Err(Error::WrongPassword),
                file => file.map_err(zip_error)?,
            }
        } else {
            archive.by_index(i).map_err(zip_error)?
        };
        let name = file.name().map_err(zip_error)?.into_owned();
        let meta = Meta {
            mode: file.unix_mode(),
            modified: file.last_modified().and_then(zip_time),
        };
        if file.is_dir() {
            out.dir(&name, meta);
        } else if file.is_symlink() {
            let mut target = String::new();
            file.read_to_string(&mut target)?;
            out.symlink(&name, Path::new(&target));
        } else {
            let result = out.file(&name, meta, &mut file);
            // ZipCrypto accepts 1 in 256 wrong passwords; the checksum catches them.
            if encrypted && matches!(result, Err(Error::Other(_))) {
                return Err(Error::WrongPassword);
            }
            result?;
        }
    }
    Ok(())
}

fn decoder<'a, R: Read + 'a>(format: Format, inner: R) -> Result<Box<dyn Read + 'a>, Error> {
    Ok(match format {
        Format::Tar => Box::new(inner),
        Format::TarGz | Format::Gz => Box::new(flate2::read::MultiGzDecoder::new(inner)),
        Format::TarXz | Format::Xz => Box::new(lzma_rust2::XzReader::new(inner, true)),
        #[cfg(not(target_arch = "wasm32"))]
        Format::TarZst | Format::Zst => Box::new(zstd::Decoder::new(inner)?),
        #[cfg(target_arch = "wasm32")]
        Format::TarZst | Format::Zst => return Err(Error::Other(super::NOT_IN_BROWSER.into())),
        Format::TarBz2 | Format::Bz2 => Box::new(bzip2::read::MultiBzDecoder::new(inner)),
        Format::Zip | Format::SevenZ | Format::Rar => unreachable!("not a stream format"),
    })
}

async fn unpack_tar(path: &Path, format: Format, out: &mut Unpacker<'_>) -> Result<(), Error> {
    let mut archive = tar::Archive::new(decoder(format, open(path, Some(out.p))?)?);
    for entry in archive.entries()? {
        out.p.pause().await;
        let mut entry = entry?;
        // The total isn't known up front, so it grows along.
        out.p.add_totals(0, 1);
        let name = entry.path()?.to_string_lossy().into_owned();
        let header = entry.header();
        let meta = Meta {
            mode: header.mode().ok(),
            modified: header.mtime().ok().map(unix_time),
        };
        let kind = header.entry_type();
        let link = entry.link_name()?.map(|l| l.into_owned());
        match (kind, link) {
            (tar::EntryType::Directory, _) => out.dir(&name, meta),
            (tar::EntryType::Symlink, Some(target)) => out.symlink(&name, &target),
            (tar::EntryType::Link, Some(existing)) => {
                out.hardlink(&name, &existing.to_string_lossy())
            }
            (tar::EntryType::Regular | tar::EntryType::Continuous, _) => {
                out.file(&name, meta, &mut entry)?
            }
            // Devices, fifos, and extensions the tar crate already applied.
            _ => out.p.add_done(0, 1),
        }
    }
    Ok(())
}

fn open_7z<'a>(
    path: &Path,
    password: Option<&str>,
    p: Option<&'a Progress>,
) -> Result<sevenz_rust2::ArchiveReader<Tracked<'a, BufReader<File>>>, Error> {
    let file = File::open(path).map_err(|e| Error::Other(format!("cannot open: {e}")))?;
    let source = Tracked::new(BufReader::with_capacity(BUFFER_SIZE, file), p);
    let secret = password.map_or_else(sevenz_rust2::Password::empty, Into::into);
    let mut reader =
        sevenz_rust2::ArchiveReader::new(source, secret).map_err(|e| sevenz_error(e, password))?;
    reader.set_thread_count(threads());
    Ok(reader)
}

fn unpack_7z(path: &Path, password: Option<&str>, out: &mut Unpacker) -> Result<(), Error> {
    let mut reader = open_7z(path, password, Some(out.p))?;
    out.p.add_totals(0, reader.archive().files.len() as u64);
    let mut failure = None;
    let result = reader.for_each_entries(|entry, data| {
        let name = entry.name.as_str();
        let mode = (entry.has_windows_attributes && entry.windows_attributes & UNIX_EXTENSION != 0)
            .then_some(entry.windows_attributes >> 16);
        let meta = Meta {
            mode,
            modified: entry
                .has_last_modified_date
                .then(|| nt_to_system(entry.last_modified_date))
                .flatten(),
        };
        if entry.is_directory {
            out.dir(name, meta);
        } else if mode.is_some_and(|m| m & S_IFMT == S_IFLNK) {
            let mut target = String::new();
            if let Err(err) = data.read_to_string(&mut target) {
                failure = Some(err.into());
                return Ok(false);
            }
            out.symlink(name, Path::new(&target));
        } else if let Err(err) = out.file(name, meta, data) {
            failure = Some(err);
            return Ok(false);
        }
        Ok(true)
    });
    if let Some(failure) = failure {
        // A failed read in an encrypted archive almost always means a wrong password.
        return Err(match failure {
            Error::Other(_) if password.is_some() => Error::WrongPassword,
            failure => failure,
        });
    }
    result.map_err(|e| sevenz_error(e, password))
}

#[cfg(not(target_arch = "wasm32"))]
fn unpack_rar(path: &Path, password: Option<&str>, out: &mut Unpacker) -> Result<(), Error> {
    // unrar reads the file itself, so progress counts unpacked bytes instead.
    let entries = list_rar(path, password)?;
    out.p
        .add_totals(entries.iter().map(|e| e.size).sum(), entries.len() as u64);
    let archive = match password {
        Some(password) => unrar::Archive::with_password(path, password),
        None => unrar::Archive::new(path),
    };
    let mut archive = archive
        .open_for_processing()
        .map_err(|e| rar_error(e, password))?;
    while let Some(header) = archive.read_header().map_err(|e| rar_error(e, password))? {
        if out.p.cancelled() {
            return Err(Error::Cancelled);
        }
        let entry = header.entry();
        let name = entry.filename.to_string_lossy().into_owned();
        let size = entry.unpacked_size;
        let meta = Meta {
            mode: None,
            modified: dos_time(entry.file_time),
        };
        archive = if entry.is_directory() {
            out.dir(&name, meta);
            header.skip()
        } else if let Some(target) = out.target(&name) {
            out.p.set_current(&target);
            if let Err(err) = out.make_parent(&target) {
                out.p.error(&target, err);
            }
            header.extract_to(&target)
        } else {
            header.skip()
        }
        .map_err(|e| rar_error(e, password))?;
        out.p.add_done(size, 1);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Errors and times

fn zip_error(err: zip::result::ZipError) -> Error {
    match err {
        zip::result::ZipError::InvalidPassword => Error::WrongPassword,
        zip::result::ZipError::Io(err) => err.into(),
        err => Error::Other(err.to_string()),
    }
}

fn sevenz_error(err: sevenz_rust2::Error, password: Option<&str>) -> Error {
    use sevenz_rust2::Error as E;
    match err {
        E::PasswordRequired if password.is_some() => Error::WrongPassword,
        E::PasswordRequired => Error::NeedsPassword,
        E::MaybeBadPassword(_) => Error::WrongPassword,
        E::Io(err, _) if err.kind() == io::ErrorKind::Interrupted => Error::Cancelled,
        err => Error::Other(err.to_string()),
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn rar_error(err: unrar::error::UnrarError, password: Option<&str>) -> Error {
    use unrar::error::Code;
    match err.code {
        Code::MissingPassword if password.is_some() => Error::WrongPassword,
        Code::MissingPassword => Error::NeedsPassword,
        Code::BadPassword => Error::WrongPassword,
        _ => Error::Other(err.to_string()),
    }
}

/// 7z's time unit, and how many of them after 1601 Unix time starts.
const NT_TICKS_PER_SEC: u64 = 10_000_000;
const NT_UNIX_EPOCH: u64 = 11_644_473_600 * NT_TICKS_PER_SEC;

/// A time in a 7z archive, if it's after 1970. Not sevenz_rust2's conversions:
/// they go by way of 1601, which `SystemTime` can't reach in the browser.
fn nt_to_system(time: sevenz_rust2::NtTime) -> Option<SystemTime> {
    let ticks = u64::from(time).checked_sub(NT_UNIX_EPOCH)?;
    let nanos = (ticks % NT_TICKS_PER_SEC) as u32 * 100;
    Some(UNIX_EPOCH + Duration::new(ticks / NT_TICKS_PER_SEC, nanos))
}

/// A time for a 7z archive, if it's after 1970; see [`nt_to_system`].
#[cfg(any(test, target_arch = "wasm32"))]
pub(super) fn system_to_nt(time: SystemTime) -> Option<sevenz_rust2::NtTime> {
    let since = time.duration_since(UNIX_EPOCH).ok()?;
    let ticks = since.as_secs().checked_mul(NT_TICKS_PER_SEC)?;
    let ticks = ticks.checked_add(u64::from(since.subsec_nanos()) / 100)?;
    Some(ticks.checked_add(NT_UNIX_EPOCH)?.into())
}

fn unix_time(seconds: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(seconds)
}

/// Zip times are local time without a zone.
fn zip_time(time: zip::DateTime) -> Option<SystemTime> {
    let naive = chrono::NaiveDateTime::try_from(time).ok()?;
    let local = naive.and_local_timezone(chrono::Local).earliest()?;
    Some(local.into())
}

/// MS-DOS date and time, as rar headers store them, in local time.
#[cfg(not(target_arch = "wasm32"))]
fn dos_time(time: u32) -> Option<SystemTime> {
    let date = chrono::NaiveDate::from_ymd_opt(
        1980 + (time >> 25) as i32,
        (time >> 21) & 0xf,
        (time >> 16) & 0x1f,
    )?;
    let naive = date.and_hms_opt((time >> 11) & 0x1f, (time >> 5) & 0x3f, (time & 0x1f) * 2)?;
    Some(naive.and_local_timezone(chrono::Local).earliest()?.into())
}

/// RAR needs unrar, a C++ library the browser build can't include.
#[cfg(target_arch = "wasm32")]
fn list_rar(_path: &Path, _password: Option<&str>) -> Result<Vec<Entry>, Error> {
    Err(Error::Other(super::NOT_IN_BROWSER.into()))
}

#[cfg(target_arch = "wasm32")]
fn unpack_rar(_path: &Path, _password: Option<&str>, _out: &mut Unpacker) -> Result<(), Error> {
    Err(Error::Other(super::NOT_IN_BROWSER.into()))
}

#[cfg(test)]
mod nt_time_tests {
    use super::*;

    #[test]
    fn nt_times_convert_without_going_before_1970() {
        let time = UNIX_EPOCH + Duration::new(1_700_000_000, 123_456_700);
        let nt = system_to_nt(time).unwrap();
        // What sevenz_rust2 makes of it, where its conversions work.
        assert_eq!(nt, sevenz_rust2::NtTime::try_from(time).unwrap());
        assert_eq!(nt_to_system(nt), Some(time));
        assert_eq!(nt_to_system(sevenz_rust2::NtTime::new(0)), None);
    }
}
