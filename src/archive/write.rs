//! Creating archives.

use disk::{File, Metadata};
use std::{
    io::{self, BufWriter, Read, Write},
    num::NonZeroU64,
    path::{Path, PathBuf},
    time::SystemTime,
};

use super::{CompressJob, Format};
use crate::{
    fs::unique_path,
    jobs::{self, Outcome, Progress},
};

/// 7z keeps Unix permissions in the high half of the Windows attributes.
const UNIX_EXTENSION: u32 = 0x8000;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_ARCHIVE: u32 = 0x20;
/// Files up to this much data share a solid 7z block.
const SOLID_BLOCK: u64 = 2 << 30;

enum Kind {
    Dir,
    File,
    Symlink(PathBuf),
}

/// Something to put in the archive.
struct Item {
    path: PathBuf,
    /// `/`-separated name inside the archive.
    name: String,
    kind: Kind,
    meta: Metadata,
}

pub async fn compress(job: CompressJob, p: &Progress) {
    let CompressJob {
        sources,
        dest,
        format,
        level,
        password,
        encrypt_names,
    } = job;
    let (Some(dir), Some(name)) = (dest.parent(), dest.file_name()) else {
        return p.error(&dest, "invalid archive name");
    };
    let dest = unique_path(dir, name);
    for source in &sources {
        jobs::scan(source, p);
    }
    let mut items = Vec::new();
    for source in &sources {
        let name = crate::fs::display_name(source);
        collect(source, name, &dest, &mut items, p);
    }
    if p.cancelled() {
        return;
    }
    let password = password.filter(|pw| !pw.is_empty() && format.supports_password());
    let result = async {
        let file = File::create_new(&dest)?;
        let out = BufWriter::with_capacity(1 << 20, file);
        match format {
            Format::Zip => write_zip(out, &items, level, password.as_deref(), p).await,
            Format::SevenZ => write_7z(out, &items, level, password.as_deref(), encrypt_names, p),
            Format::Tar => finish(write_tar(out, &items, p).await?),
            Format::TarGz => {
                let gz = flate2::write::GzEncoder::new(out, flate2::Compression::new(level));
                finish(write_tar(gz, &items, p).await?.finish()?)
            }
            Format::TarXz => {
                let mut options = lzma_rust2::XzOptions::with_preset(level);
                let block = 3 * u64::from(options.lzma_options.dict_size);
                options.set_block_size(NonZeroU64::new(block.max(1 << 20)));
                #[cfg(not(target_arch = "wasm32"))]
                let xz = lzma_rust2::XzWriterMt::new(out, options, threads())?;
                // The browser can't start the threads.
                #[cfg(target_arch = "wasm32")]
                let xz = lzma_rust2::XzWriter::new(out, options)?;
                finish(write_tar(xz, &items, p).await?.finish()?)
            }
            #[cfg(target_arch = "wasm32")]
            Format::TarZst => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                super::NOT_IN_BROWSER,
            )),
            #[cfg(not(target_arch = "wasm32"))]
            Format::TarZst => {
                let mut zst = zstd::Encoder::new(out, zstd_level(level))?;
                zst.multithread(threads())?;
                finish(write_tar(zst, &items, p).await?.finish()?)
            }
            Format::TarBz2 => {
                let bz =
                    bzip2::write::BzEncoder::new(out, bzip2::Compression::new(level.clamp(1, 9)));
                finish(write_tar(bz, &items, p).await?.finish()?)
            }
            Format::Rar | Format::Gz | Format::Xz | Format::Zst | Format::Bz2 => {
                Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "can't create this kind of archive",
                ))
            }
        }
    }
    .await;
    match result {
        Ok(()) if !p.cancelled() => p.set_outcome(Outcome::Reveal(dest)),
        Ok(()) => {
            let _ = disk::remove_file(&dest);
        }
        Err(err) => {
            if err.kind() != io::ErrorKind::AlreadyExists {
                let _ = disk::remove_file(&dest);
            }
            if !p.cancelled() {
                p.error(&dest, err);
            }
        }
    }
}

fn finish(mut out: impl Write) -> io::Result<()> {
    out.flush()
}

fn threads() -> u32 {
    std::thread::available_parallelism().map_or(1, |n| n.get() as u32)
}

/// zstd goes to 19 (22 with huge windows); spread 0–9 over the useful range.
#[cfg(not(target_arch = "wasm32"))]
fn zstd_level(level: u32) -> i32 {
    [1, 1, 2, 3, 4, 6, 9, 12, 15, 19][level.min(9) as usize]
}

/// Lists `path` and everything under it, skipping the archive being written.
fn collect(path: &Path, name: String, dest: &Path, items: &mut Vec<Item>, p: &Progress) {
    if p.cancelled() || path == dest {
        return;
    }
    let meta = match disk::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) => return p.error(path, err),
    };
    let kind = if meta.is_symlink() {
        match disk::read_link(path) {
            Ok(target) => Kind::Symlink(target),
            Err(err) => return p.error(path, err),
        }
    } else if meta.is_dir() {
        Kind::Dir
    } else {
        Kind::File
    };
    let is_dir = matches!(kind, Kind::Dir);
    items.push(Item {
        path: path.to_path_buf(),
        name: name.clone(),
        kind,
        meta,
    });
    if is_dir {
        let mut children: Vec<_> = match disk::read_dir(path) {
            Ok(entries) => entries.flatten().collect(),
            Err(err) => return p.error(path, err),
        };
        children.sort_by_key(|e| e.file_name());
        for child in children {
            let child_name = format!("{name}/{}", child.file_name().to_string_lossy());
            collect(&child.path(), child_name, dest, items, p);
        }
    }
}

/// Reads a source file, counting progress and stopping once cancelled. It
/// opens the file on first read, so solid 7z blocks don't hold every file open.
struct Source<'a> {
    path: &'a Path,
    file: Option<File>,
    done: bool,
    p: &'a Progress,
}

impl<'a> Source<'a> {
    fn new(path: &'a Path, p: &'a Progress) -> Self {
        Self {
            path,
            file: None,
            done: false,
            p,
        }
    }
}

impl Read for Source<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.p.cancelled() {
            // Not `Interrupted`: readers retry those.
            return Err(io::Error::other("cancelled"));
        }
        if self.done {
            return Ok(0);
        }
        if self.file.is_none() {
            self.p.set_current(self.path);
            self.file = Some(File::open(self.path)?);
        }
        let n = self.file.as_mut().map_or(Ok(0), |f| f.read(buf))?;
        if n == 0 {
            self.file = None;
            self.done = true;
            self.p.add_done(0, 1);
        }
        self.p.add_done(n as u64, 0);
        Ok(n)
    }
}

#[cfg(unix)]
fn mode(meta: &Metadata) -> u32 {
    std::os::unix::fs::PermissionsExt::mode(&meta.permissions())
}

#[cfg(not(unix))]
fn mode(meta: &Metadata) -> u32 {
    match (meta.is_dir(), meta.permissions().readonly()) {
        (true, _) => 0o040755,
        (false, true) => 0o100444,
        (false, false) => 0o100644,
    }
}

fn zip_time(time: Option<SystemTime>) -> zip::DateTime {
    time.and_then(|t| {
        let local = chrono::DateTime::<chrono::Local>::from(t).naive_local();
        zip::DateTime::try_from(local).ok()
    })
    .unwrap_or_default()
}

async fn write_zip<W: Write + io::Seek>(
    out: W,
    items: &[Item],
    level: u32,
    password: Option<&str>,
    p: &Progress,
) -> io::Result<()> {
    use zip::{CompressionMethod, write::SimpleFileOptions};
    let mut zip = zip::ZipWriter::new(out);
    let base = if level == 0 {
        SimpleFileOptions::default().compression_method(CompressionMethod::Stored)
    } else {
        SimpleFileOptions::default()
            .compression_method(CompressionMethod::Deflated)
            .compression_level(Some(level.into()))
    };
    for item in items {
        p.pause().await;
        let options = base
            .last_modified_time(zip_time(item.meta.modified().ok()))
            .unix_permissions(mode(&item.meta) & 0o7777);
        match &item.kind {
            Kind::Dir => zip.add_directory(format!("{}/", item.name), options)?,
            Kind::Symlink(target) => {
                zip.add_symlink(&item.name, target.to_string_lossy(), options)?;
                p.add_done(0, 1);
            }
            Kind::File => {
                let options = options.large_file(item.meta.len() > u32::MAX.into());
                let options = match password {
                    Some(password) => options.with_aes_encryption(zip::AesMode::Aes256, password),
                    None => options,
                };
                zip.start_file(&item.name, options)?;
                let mut source = Source::new(&item.path, p);
                if let Err(err) = io::copy(&mut source, &mut zip) {
                    if p.cancelled() {
                        return Err(err);
                    }
                    // Keep going without it; the entry stays, possibly cut short.
                    p.error(&item.path, err);
                }
            }
        }
    }
    finish(zip.finish()?)
}

/// The header's type, size, mode and time, from `meta`.
#[cfg(not(target_arch = "wasm32"))]
fn set_metadata(header: &mut tar::Header, meta: &Metadata) {
    header.set_metadata_in_mode(meta, tar::HeaderMode::Complete);
}

/// tar takes only std's metadata, so this does what it does on Windows.
#[cfg(target_arch = "wasm32")]
fn set_metadata(header: &mut tar::Header, meta: &Metadata) {
    let (entry_type, mode) = if meta.is_dir() {
        (tar::EntryType::Directory, 0o755)
    } else if meta.is_symlink() {
        (tar::EntryType::Symlink, 0o777)
    } else {
        (tar::EntryType::Regular, 0o644)
    };
    header.set_entry_type(entry_type);
    header.set_size(if meta.is_file() { meta.len() } else { 0 });
    header.set_mode(if meta.permissions().readonly() {
        mode & 0o555
    } else {
        mode
    });
    if let Some(since) = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
    {
        header.set_mtime(since.as_secs());
    }
}

async fn write_tar<W: Write>(out: W, items: &[Item], p: &Progress) -> io::Result<W> {
    let mut tar = tar::Builder::new(out);
    tar.follow_symlinks(false);
    for item in items {
        p.pause().await;
        let mut header = tar::Header::new_gnu();
        set_metadata(&mut header, &item.meta);
        match &item.kind {
            Kind::Dir => {
                header.set_size(0);
                tar.append_data(&mut header, format!("{}/", item.name), io::empty())?;
            }
            Kind::Symlink(target) => {
                header.set_size(0);
                tar.append_link(&mut header, &item.name, target)?;
                p.add_done(0, 1);
            }
            Kind::File => {
                // Opened up front, so an unreadable file is skipped rather than
                // leaving a header without its data.
                if let Err(err) = File::open(&item.path) {
                    p.error(&item.path, err);
                    continue;
                }
                let size = item.meta.len();
                header.set_size(size);
                // Exactly the size in the header, even if the file changes meanwhile.
                let data = Source::new(&item.path, p).chain(io::repeat(0)).take(size);
                tar.append_data(&mut header, &item.name, data)?;
            }
        }
    }
    tar.into_inner()
}

/// The dictionary size xz and 7-Zip use at each level.
fn dict_size(level: u32) -> u64 {
    const MIB: u64 = 1 << 20;
    [
        MIB / 4,
        MIB,
        2 * MIB,
        4 * MIB,
        4 * MIB,
        8 * MIB,
        8 * MIB,
        16 * MIB,
        32 * MIB,
        64 * MIB,
    ][level.min(9) as usize]
}

fn write_7z<W: Write + io::Seek>(
    out: W,
    items: &[Item],
    level: u32,
    password: Option<&str>,
    encrypt_names: bool,
    p: &Progress,
) -> io::Result<()> {
    use sevenz_rust2::{
        ArchiveEntry, ArchiveWriter, EncoderConfiguration, SourceReader,
        encoder_options::{AesEncoderOptions, Lzma2Options},
    };
    let to_io = |err: sevenz_rust2::Error| match err {
        sevenz_rust2::Error::Io(err, _) => err,
        err => io::Error::other(err.to_string()),
    };
    let mut writer = ArchiveWriter::new(out).map_err(to_io)?;
    let lzma2 = if level == 0 {
        Lzma2Options::from_level(0)
    } else {
        Lzma2Options::from_level_mt(level, threads(), (4 * dict_size(level)).max(1 << 20))
    };
    let mut methods: Vec<EncoderConfiguration> = Vec::new();
    if let Some(password) = password {
        methods.push(AesEncoderOptions::new(password.into()).into());
    }
    methods.push(lzma2.into());
    writer.set_content_methods(methods);
    writer.set_encrypt_header(password.is_some() && encrypt_names);

    let entry = |item: &Item| {
        #[cfg(not(target_arch = "wasm32"))]
        let mut entry = ArchiveEntry::from_path(&item.path, item.name.clone());
        // `from_path` goes to std for what it knows, which fails in the browser.
        #[cfg(target_arch = "wasm32")]
        let mut entry = {
            let mut entry = if item.meta.is_dir() {
                ArchiveEntry::new_directory(&item.name)
            } else {
                ArchiveEntry::new_file(&item.name)
            };
            if let Some(modified) = item
                .meta
                .modified()
                .ok()
                .and_then(super::read::system_to_nt)
            {
                entry.has_last_modified_date = true;
                entry.last_modified_date = modified;
            }
            entry
        };
        let dos = if item.meta.is_dir() {
            FILE_ATTRIBUTE_DIRECTORY
        } else {
            FILE_ATTRIBUTE_ARCHIVE
        };
        entry.has_windows_attributes = true;
        entry.windows_attributes = dos | UNIX_EXTENSION | (mode(&item.meta) << 16);
        entry
    };
    let mut batch: Vec<(ArchiveEntry, &Item)> = Vec::new();
    let mut batch_size = 0;
    let flush = |writer: &mut ArchiveWriter<W>, batch: &mut Vec<(ArchiveEntry, &Item)>| {
        if batch.is_empty() {
            return Ok(());
        }
        let (entries, readers): (Vec<_>, Vec<_>) = batch
            .drain(..)
            .map(|(entry, item)| (entry, SourceReader::new(Source::new(&item.path, p))))
            .unzip();
        writer
            .push_archive_entries(entries, readers)
            .map(drop)
            .map_err(to_io)
    };
    for item in items {
        match &item.kind {
            Kind::Dir => {
                writer
                    .push_archive_entry::<&[u8]>(entry(item), None)
                    .map_err(to_io)?;
            }
            Kind::Symlink(target) => {
                let target = target.to_string_lossy().into_owned();
                writer
                    .push_archive_entry(entry(item), Some(target.as_bytes()))
                    .map_err(to_io)?;
                p.add_done(0, 1);
            }
            Kind::File if item.meta.len() == 0 => {
                // Empty files are stored without a stream.
                writer
                    .push_archive_entry::<&[u8]>(entry(item), None)
                    .map_err(to_io)?;
                p.add_done(0, 1);
            }
            Kind::File => {
                if let Err(err) = File::open(&item.path) {
                    p.error(&item.path, err);
                    continue;
                }
                if batch_size + item.meta.len() > SOLID_BLOCK {
                    flush(&mut writer, &mut batch)?;
                    batch_size = 0;
                }
                batch_size += item.meta.len();
                batch.push((entry(item), item));
            }
        }
    }
    flush(&mut writer, &mut batch)?;
    finish(writer.finish()?)
}
