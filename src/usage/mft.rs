//! Scanning a whole NTFS drive by reading its master file table: one record
//! per file, read front to back in big chunks, instead of listing every
//! folder. A hard drive doesn't have to seek from folder to folder, so it
//! takes seconds where listing takes minutes. Reading the volume directly
//! needs an administrator, so without one the app runs a copy of itself
//! through UAC ([`FLAG`]) that writes the entries to a temp file.

// Only Windows reads volumes; elsewhere the parsing is just tested.
#![cfg_attr(not(windows), allow(dead_code))]

use std::{
    ffi::OsString,
    io::{self, Read, Write},
    path::{Component, Path, Prefix},
};

/// The argument that makes the app read a volume's table instead of starting.
pub const FLAG: &str = "--read-mft";

/// The root folder's record.
pub const ROOT_RECORD: u32 = 5;

/// Records below this are NTFS's own files ($MFT, $Bitmap, …), which listing
/// a folder doesn't show; their children (under $Extend) drop out with them.
const FIRST_USER_RECORD: u32 = 24;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    Dir,
    File,
    /// A symlink or junction: counted, not followed.
    Link,
}

/// One name of a file or folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub record: u32,
    pub parent: u32,
    pub kind: EntryKind,
    pub disk: u64,
    pub apparent: u64,
    /// Seconds since 1970.
    pub modified: i64,
    /// How many names (hard links) the record has, short DOS names aside.
    pub links: u16,
    pub name: Vec<u16>,
}

impl Entry {
    pub fn os_name(&self) -> OsString {
        #[cfg(windows)]
        {
            std::os::windows::ffi::OsStringExt::from_wide(&self.name)
        }
        #[cfg(not(windows))]
        {
            String::from_utf16_lossy(&self.name).into()
        }
    }
}

/// How the table can be read, for a folder that's a whole NTFS drive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// The app is an administrator and opens the volume itself.
    Direct,
    /// Through a helper started with UAC.
    Elevate,
}

/// The drive letter of a folder that's a drive's root, like `C:\`.
pub fn drive_letter(root: &Path) -> Option<char> {
    let mut parts = root.components();
    let letter = match parts.next()? {
        Component::Prefix(prefix) => match prefix.kind() {
            Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => letter as char,
            _ => return None,
        },
        _ => return None,
    };
    (parts.next()? == Component::RootDir && parts.next().is_none()).then_some(letter)
}

/// Whether, and how, `root` can be scanned from the table.
pub fn access(root: &Path) -> Option<Access> {
    let letter = drive_letter(root)?;
    platform::is_ntfs(letter).then(|| {
        if platform::open_volume(letter).is_ok() {
            Access::Direct
        } else {
            Access::Elevate
        }
    })
}

/// Every name on the drive whose root is `root`.
pub fn read(root: &Path, access: Access) -> io::Result<Vec<Entry>> {
    let letter = drive_letter(root)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "not a drive's root"))?;
    match access {
        Access::Direct => platform::read_volume(letter),
        Access::Elevate => platform::read_elevated(letter),
    }
}

/// If the app was started as the helper, does its work and returns the
/// exit code.
pub fn run_helper() -> Option<i32> {
    let mut args = std::env::args_os().skip(1);
    if args.next()? != FLAG {
        return None;
    }
    let (Some(letter), Some(out)) = (args.next(), args.next()) else {
        return Some(2);
    };
    let letter = letter.to_string_lossy().chars().next().unwrap_or('C');
    let result = platform::read_volume(letter).and_then(|entries| {
        let mut file = io::BufWriter::new(disk::File::create(out)?);
        write_entries(&mut file, &entries)?;
        file.flush()
    });
    Some(match result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("{error}");
            1
        }
    })
}

// ---- Parsing ----

fn u16_at(buf: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(buf.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(buf: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(buf.get(at..at + 4)?.try_into().ok()?))
}

fn u64_at(buf: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(buf.get(at..at + 8)?.try_into().ok()?))
}

/// Little-endian UTF-16 bytes as code units.
fn utf16(bytes: &[u8]) -> Vec<u16> {
    bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&c| u16::from_le_bytes(c))
        .collect()
}

/// A file reference's record number (the top 16 bits are a sequence number).
fn record_of(reference: u64) -> u64 {
    reference & 0xFFFF_FFFF_FFFF
}

/// Puts back the last two bytes of each 512-byte stride, which NTFS swaps
/// for a check value to catch torn writes. False if the record is torn.
pub fn fixup(buf: &mut [u8]) -> bool {
    let (Some(offset), Some(count)) = (u16_at(buf, 4), u16_at(buf, 6)) else {
        return false;
    };
    let (offset, count) = (offset as usize, count as usize);
    if count == 0 || offset + count * 2 > buf.len() || (count - 1) * 512 > buf.len() {
        return false;
    }
    let check = [buf[offset], buf[offset + 1]];
    for i in 1..count {
        let end = i * 512;
        if buf[end - 2..end] != check {
            return false;
        }
        let saved = offset + i * 2;
        buf[end - 2] = buf[saved];
        buf[end - 1] = buf[saved + 1];
    }
    true
}

pub type Extents = Vec<(Option<u64>, u64)>;

/// A non-resident attribute's extents: (first cluster, or None for a hole;
/// length in clusters).
pub fn data_runs(mut runs: &[u8]) -> Option<Extents> {
    let mut out = Vec::new();
    let mut lcn: i64 = 0;
    while let Some(&header) = runs.first() {
        if header == 0 {
            break;
        }
        let (len_size, off_size) = ((header & 0x0F) as usize, (header >> 4) as usize);
        if len_size == 0 || len_size > 8 || off_size > 8 || runs.len() < 1 + len_size + off_size {
            return None;
        }
        let mut length = 0u64;
        for (i, &b) in runs[1..1 + len_size].iter().enumerate() {
            length |= (b as u64) << (8 * i);
        }
        let start = if off_size == 0 {
            None
        } else {
            let bytes = &runs[1 + len_size..1 + len_size + off_size];
            let mut delta = 0i64;
            for (i, &b) in bytes.iter().enumerate() {
                delta |= (b as i64) << (8 * i);
            }
            // Sign-extend.
            let shift = 64 - 8 * off_size as u32;
            delta = (delta << shift) >> shift;
            lcn = lcn.checked_add(delta)?;
            Some(u64::try_from(lcn).ok()?)
        };
        out.push((start, length));
        runs = &runs[1 + len_size + off_size..];
    }
    Some(out)
}

#[derive(Clone, Copy, Default)]
struct Record {
    used: bool,
    dir: bool,
    disk: u64,
    apparent: u64,
    modified: i64,
    attributes: u32,
    reparse_tag: u32,
    names: u16,
}

const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
/// Reparse tags with this bit stand for another name (symlinks, junctions);
/// others (cloud files, dedup, compression) are the file itself.
const NAME_SURROGATE: u32 = 0x2000_0000;

/// Collects records as they're read, in any order.
#[derive(Default)]
pub struct Parser {
    records: Vec<Record>,
    names: Vec<(u32, u32, Vec<u16>)>,
}

impl Parser {
    /// Reads record `index`; `buf` is the whole record, fixups not yet
    /// applied.
    pub fn record(&mut self, index: u64, buf: &mut [u8]) {
        if buf.get(0..4) != Some(b"FILE") || !fixup(buf) {
            return;
        }
        let _ = self.parse(index, buf);
    }

    fn parse(&mut self, index: u64, buf: &[u8]) -> Option<()> {
        let flags = u16_at(buf, 0x16)?;
        if flags & 1 == 0 {
            return None;
        }
        // Extension records carry attributes that didn't fit in their base.
        let base = record_of(u64_at(buf, 0x20)?);
        let target = u32::try_from(if base != 0 { base } else { index }).ok()?;
        if target == u32::MAX {
            return None;
        }
        if self.records.len() <= target as usize {
            self.records.resize(target as usize + 1, Record::default());
        }
        if base == 0 {
            let record = &mut self.records[target as usize];
            record.used = true;
            record.dir = flags & 2 != 0;
        }
        let end = (u32_at(buf, 0x18)? as usize).min(buf.len());
        let mut at = u16_at(buf, 0x14)? as usize;
        while at + 16 <= end {
            let kind = u32_at(buf, at)?;
            if kind == 0xFFFF_FFFF {
                break;
            }
            let length = u32_at(buf, at + 4)? as usize;
            if length < 16 || at + length > end {
                break;
            }
            let attr = &buf[at..at + length];
            let _ = self.attribute(target, kind, attr);
            at += length;
        }
        Some(())
    }

    fn attribute(&mut self, target: u32, kind: u32, attr: &[u8]) -> Option<()> {
        let resident = attr[8] == 0;
        let named = attr[9] != 0;
        let value = || -> Option<&[u8]> {
            let len = u32_at(attr, 0x10)? as usize;
            let off = u16_at(attr, 0x14)? as usize;
            attr.get(off..off + len)
        };
        let record = &mut self.records[target as usize];
        match kind {
            // $STANDARD_INFORMATION
            0x10 if resident => {
                let v = value()?;
                record.modified = filetime_to_unix(u64_at(v, 0x08)? as i64);
                record.attributes = u32_at(v, 0x20)?;
            }
            // $FILE_NAME
            0x30 if resident => {
                let v = value()?;
                let parent = u32::try_from(record_of(u64_at(v, 0)?)).ok()?;
                let (len, namespace) = (*v.get(0x40)? as usize, *v.get(0x41)?);
                // The short 8.3 alias of a long name.
                if namespace == 2 {
                    return None;
                }
                let bytes = v.get(0x42..0x42 + len * 2)?;
                let name = utf16(bytes);
                record.names = record.names.saturating_add(1);
                self.names.push((target, parent, name));
            }
            // $DATA: the unnamed stream is the file; named ones (like the
            // compressed copy of a CompactOS file) take space too.
            0x80 if resident => {
                if !named {
                    record.apparent = u32_at(attr, 0x10)? as u64;
                }
            }
            0x80 => {
                // Only the first extent of a stream says how big it is.
                if u64_at(attr, 0x10)? != 0 {
                    return None;
                }
                let flags = u16_at(attr, 0x0C)?;
                let allocated = u64_at(attr, 0x28)?;
                let size = u64_at(attr, 0x30)?;
                // Compressed or sparse: what's actually allocated.
                let disk = if flags & 0x8001 != 0 {
                    u64_at(attr, 0x40).unwrap_or(allocated)
                } else {
                    allocated
                };
                record.disk = record.disk.saturating_add(disk);
                if !named {
                    record.apparent = size;
                }
            }
            // $REPARSE_POINT
            0xC0 if resident => {
                record.reparse_tag = u32_at(value()?, 0)?;
            }
            _ => {}
        }
        Some(())
    }

    /// Every name of a file or folder in use, NTFS's own files left out.
    pub fn finish(self) -> Vec<Entry> {
        let Self { records, names } = self;
        names
            .into_iter()
            .filter_map(|(record, parent, name)| {
                if record < FIRST_USER_RECORD || record == parent {
                    return None;
                }
                let r = records.get(record as usize)?;
                if !r.used {
                    return None;
                }
                let kind = if r.attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
                    && r.reparse_tag & NAME_SURROGATE != 0
                {
                    EntryKind::Link
                } else if r.dir {
                    EntryKind::Dir
                } else {
                    EntryKind::File
                };
                let file = kind == EntryKind::File;
                Some(Entry {
                    record,
                    parent,
                    kind,
                    disk: if file { r.disk } else { 0 },
                    apparent: if file { r.apparent } else { 0 },
                    modified: r.modified,
                    links: r.names,
                    name,
                })
            })
            .collect()
    }
}

/// 100 ns ticks since 1601 to seconds since 1970.
fn filetime_to_unix(filetime: i64) -> i64 {
    if filetime <= 0 {
        0
    } else {
        filetime / 10_000_000 - 11_644_473_600
    }
}

/// What the boot sector says about the volume.
#[derive(Debug, PartialEq, Eq)]
pub struct Geometry {
    pub cluster: u64,
    pub record: u64,
    /// Where record 0 ($MFT itself) is, in bytes.
    pub mft_offset: u64,
}

pub fn geometry(boot: &[u8]) -> io::Result<Geometry> {
    let bad = |what: &str| io::Error::new(io::ErrorKind::InvalidData, what.to_owned());
    if boot.get(3..11) != Some(b"NTFS    ") {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "not an NTFS volume",
        ));
    }
    let sector = u16_at(boot, 0x0B).ok_or_else(|| bad("short boot sector"))? as u64;
    let raw = boot[0x0D];
    let per_cluster = if raw > 0x80 {
        1u64 << (256 - raw as u32)
    } else {
        raw as u64
    };
    let cluster = sector * per_cluster;
    let mft_lcn = u64_at(boot, 0x30).ok_or_else(|| bad("short boot sector"))?;
    let per_record = boot[0x40] as i8;
    let record = if per_record > 0 {
        per_record as u64 * cluster
    } else {
        1u64 << (-(per_record as i32)) as u32
    };
    if cluster == 0 || !(512..=65536).contains(&record) || record % 512 != 0 {
        return Err(bad("odd NTFS geometry"));
    }
    Ok(Geometry {
        cluster,
        record,
        mft_offset: mft_lcn * cluster,
    })
}

/// Where the table itself is: its extents and length, from record 0.
pub fn mft_extents(record0: &mut [u8], cluster: u64) -> io::Result<(Extents, u64)> {
    let bad = |what: &str| io::Error::new(io::ErrorKind::InvalidData, what.to_owned());
    if record0.get(0..4) != Some(b"FILE") || !fixup(record0) {
        return Err(bad("the table's first record is damaged"));
    }
    let buf = &*record0;
    let end = (u32_at(buf, 0x18).unwrap_or(0) as usize).min(buf.len());
    let mut at = u16_at(buf, 0x14).unwrap_or(0) as usize;
    while at + 16 <= end {
        let kind = u32_at(buf, at).unwrap_or(0xFFFF_FFFF);
        let length = u32_at(buf, at + 4).unwrap_or(0) as usize;
        if kind == 0xFFFF_FFFF || length < 16 || at + length > end {
            break;
        }
        let attr = &buf[at..at + length];
        if kind == 0x80 && attr[8] != 0 && attr[9] == 0 {
            let size = u64_at(attr, 0x30).ok_or_else(|| bad("short $DATA"))?;
            let runs_at = u16_at(attr, 0x20).ok_or_else(|| bad("short $DATA"))? as usize;
            let runs = attr
                .get(runs_at..)
                .and_then(data_runs)
                .ok_or_else(|| bad("bad extents"))?;
            let clusters: u64 = runs.iter().map(|r| r.1).sum();
            // A table fragmented beyond one record keeps the rest of its
            // extents elsewhere; listing folders handles that.
            if clusters * cluster < size {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "the table is too fragmented",
                ));
            }
            return Ok((runs, size));
        }
        at += length;
    }
    Err(bad("the table has no $DATA"))
}

// ---- Passing entries from the helper ----

const MAGIC: &[u8; 8] = b"EFMFT\x01\0\0";

pub fn write_entries(out: &mut impl Write, entries: &[Entry]) -> io::Result<()> {
    out.write_all(MAGIC)?;
    out.write_all(&(entries.len() as u64).to_le_bytes())?;
    for e in entries {
        out.write_all(&e.record.to_le_bytes())?;
        out.write_all(&e.parent.to_le_bytes())?;
        out.write_all(&[match e.kind {
            EntryKind::Dir => 0,
            EntryKind::File => 1,
            EntryKind::Link => 2,
        }])?;
        out.write_all(&e.links.to_le_bytes())?;
        out.write_all(&e.disk.to_le_bytes())?;
        out.write_all(&e.apparent.to_le_bytes())?;
        out.write_all(&e.modified.to_le_bytes())?;
        out.write_all(&(e.name.len() as u16).to_le_bytes())?;
        for c in &e.name {
            out.write_all(&c.to_le_bytes())?;
        }
    }
    Ok(())
}

pub fn read_entries(input: &mut impl Read) -> io::Result<Vec<Entry>> {
    fn take<const N: usize>(input: &mut impl Read) -> io::Result<[u8; N]> {
        let mut buf = [0; N];
        input.read_exact(&mut buf)?;
        Ok(buf)
    }
    if &take::<8>(input)? != MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not a table dump",
        ));
    }
    let count = u64::from_le_bytes(take(input)?);
    let mut entries = Vec::with_capacity(count.min(1 << 24) as usize);
    for _ in 0..count {
        let record = u32::from_le_bytes(take(input)?);
        let parent = u32::from_le_bytes(take(input)?);
        let kind = match take::<1>(input)?[0] {
            0 => EntryKind::Dir,
            1 => EntryKind::File,
            _ => EntryKind::Link,
        };
        let links = u16::from_le_bytes(take(input)?);
        let disk = u64::from_le_bytes(take(input)?);
        let apparent = u64::from_le_bytes(take(input)?);
        let modified = i64::from_le_bytes(take(input)?);
        let len = u16::from_le_bytes(take(input)?) as usize;
        let mut raw = vec![0u8; len * 2];
        input.read_exact(&mut raw)?;
        let name = utf16(&raw);
        entries.push(Entry {
            record,
            parent,
            kind,
            disk,
            apparent,
            modified,
            links,
            name,
        });
    }
    Ok(entries)
}

#[cfg(windows)]
mod platform {
    use std::fs::File;
    use std::{io, os::windows::fs::FileExt};

    use super::{Entry, Parser, geometry, mft_extents};

    pub fn is_ntfs(letter: char) -> bool {
        use windows::{Win32::Storage::FileSystem::GetVolumeInformationW, core::HSTRING};
        let root = HSTRING::from(format!(r"{letter}:\"));
        let mut fs_name = [0u16; 32];
        // SAFETY: `fs_name` is writable for its length; the rest are unused.
        let ok =
            unsafe { GetVolumeInformationW(&root, None, None, None, None, Some(&mut fs_name)) };
        ok.is_ok() && String::from_utf16_lossy(&fs_name).trim_end_matches('\0') == "NTFS"
    }

    pub fn open_volume(letter: char) -> io::Result<File> {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE: the
        // volume stays in use while it's read.
        std::fs::OpenOptions::new()
            .read(true)
            .share_mode(7)
            .open(format!(r"\\.\{letter}:"))
    }

    fn read_at(file: &File, buf: &mut [u8], mut offset: u64) -> io::Result<()> {
        let mut filled = 0;
        while filled < buf.len() {
            let n = file.seek_read(&mut buf[filled..], offset)?;
            if n == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            filled += n;
            offset += n as u64;
        }
        Ok(())
    }

    /// Bytes read at a time; a multiple of every cluster and record size.
    const CHUNK: u64 = 4 << 20;

    pub fn read_volume(letter: char) -> io::Result<Vec<Entry>> {
        let volume = open_volume(letter)?;
        // Reads of a volume must be whole sectors, which can be 4 KiB.
        let mut boot = vec![0u8; 4096];
        read_at(&volume, &mut boot, 0)?;
        let geometry = geometry(&boot)?;
        let first = geometry.cluster.max(geometry.record).max(4096);
        let mut record0 = vec![0u8; first as usize];
        read_at(&volume, &mut record0, geometry.mft_offset)?;
        let (runs, size) = mft_extents(&mut record0[..geometry.record as usize], geometry.cluster)?;

        let total = size / geometry.record;
        let mut parser = Parser::default();
        let mut index = 0u64;
        let mut buf = vec![0u8; CHUNK as usize];
        'runs: for (start, clusters) in runs {
            let bytes = clusters * geometry.cluster;
            let Some(start) = start else {
                index += bytes / geometry.record;
                continue;
            };
            let mut done = 0;
            while done < bytes {
                let n = (bytes - done).min(CHUNK);
                let chunk = &mut buf[..n as usize];
                read_at(&volume, chunk, start * geometry.cluster + done)?;
                for record in chunk.chunks_exact_mut(geometry.record as usize) {
                    if index >= total {
                        break 'runs;
                    }
                    parser.record(index, record);
                    index += 1;
                }
                done += n;
            }
        }
        Ok(parser.finish())
    }

    pub fn read_elevated(letter: char) -> io::Result<Vec<Entry>> {
        use crate::properties::elevate::{Cleanup, run_as_admin, temp_stem};
        let out = std::env::temp_dir().join(format!("{}.bin", temp_stem("mft")));
        let _cleanup = Cleanup(vec![out.clone()]);
        run_as_admin(&format!("{} {letter} \"{}\"", super::FLAG, out.display()))?;
        let mut file = io::BufReader::new(File::open(&out)?);
        super::read_entries(&mut file)
    }
}

#[cfg(not(windows))]
mod platform {
    use std::io;

    use super::Entry;

    fn unsupported() -> io::Error {
        io::Error::new(io::ErrorKind::Unsupported, "only on Windows")
    }

    pub fn is_ntfs(_letter: char) -> bool {
        false
    }

    pub fn open_volume(_letter: char) -> io::Result<()> {
        Err(unsupported())
    }

    pub fn read_volume(_letter: char) -> io::Result<Vec<Entry>> {
        Err(unsupported())
    }

    pub fn read_elevated(_letter: char) -> io::Result<Vec<Entry>> {
        Err(unsupported())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RECORD: usize = 1024;

    fn put16(buf: &mut [u8], at: usize, v: u16) {
        buf[at..at + 2].copy_from_slice(&v.to_le_bytes());
    }
    fn put32(buf: &mut [u8], at: usize, v: u32) {
        buf[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }
    fn put64(buf: &mut [u8], at: usize, v: u64) {
        buf[at..at + 8].copy_from_slice(&v.to_le_bytes());
    }

    /// Builds a record the way NTFS lays one out, check values applied.
    struct Builder {
        buf: Vec<u8>,
        at: usize,
    }

    impl Builder {
        fn new(flags: u16, base: u64) -> Self {
            let mut buf = vec![0u8; RECORD];
            buf[0..4].copy_from_slice(b"FILE");
            put16(&mut buf, 4, 0x30); // fixup array
            put16(&mut buf, 6, 3); // check value + 2 strides
            put16(&mut buf, 0x14, 0x38); // first attribute
            put16(&mut buf, 0x16, flags);
            put64(&mut buf, 0x20, base);
            Self { buf, at: 0x38 }
        }

        fn resident(mut self, kind: u32, name: &[u16], value: &[u8]) -> Self {
            let name_at = 0x18;
            let value_at = (name_at + name.len() * 2 + 7) & !7;
            let len = (value_at + value.len() + 7) & !7;
            let a = self.at;
            put32(&mut self.buf, a, kind);
            put32(&mut self.buf, a + 4, len as u32);
            self.buf[a + 9] = name.len() as u8;
            put16(&mut self.buf, a + 0x0A, name_at as u16);
            put32(&mut self.buf, a + 0x10, value.len() as u32);
            put16(&mut self.buf, a + 0x14, value_at as u16);
            for (i, c) in name.iter().enumerate() {
                put16(&mut self.buf, a + name_at + i * 2, *c);
            }
            self.buf[a + value_at..a + value_at + value.len()].copy_from_slice(value);
            self.at += len;
            self
        }

        fn nonresident(mut self, flags: u16, allocated: u64, size: u64, compressed: u64) -> Self {
            let len = 0x48 + 8;
            let a = self.at;
            put32(&mut self.buf, a, 0x80);
            put32(&mut self.buf, a + 4, len as u32);
            self.buf[a + 8] = 1;
            put16(&mut self.buf, a + 0x0C, flags);
            put16(&mut self.buf, a + 0x20, 0x48);
            put64(&mut self.buf, a + 0x28, allocated);
            put64(&mut self.buf, a + 0x30, size);
            put64(&mut self.buf, a + 0x38, size);
            put64(&mut self.buf, a + 0x40, compressed);
            self.buf[a + 0x48..a + 0x4C].copy_from_slice(&[0x21, 0x10, 0x00, 0x01]);
            self.at += len;
            self
        }

        fn standard(self, modified: i64, attributes: u32) -> Self {
            let mut v = vec![0u8; 0x48];
            put64(&mut v, 0x08, modified as u64);
            put32(&mut v, 0x20, attributes);
            self.resident(0x10, &[], &v)
        }

        fn file_name(self, parent: u64, namespace: u8, name: &str) -> Self {
            let wide: Vec<u16> = name.encode_utf16().collect();
            let mut v = vec![0u8; 0x42 + wide.len() * 2];
            put64(&mut v, 0, parent | (1 << 48));
            v[0x40] = wide.len() as u8;
            v[0x41] = namespace;
            for (i, c) in wide.iter().enumerate() {
                put16(&mut v, 0x42 + i * 2, *c);
            }
            self.resident(0x30, &[], &v)
        }

        fn build(mut self) -> Vec<u8> {
            let a = self.at;
            put32(&mut self.buf, a, 0xFFFF_FFFF);
            put32(&mut self.buf, 0x18, (a + 8) as u32);
            // What NTFS does on write: stash each stride's tail, put the
            // check value there.
            let check = [0xAB, 0xCD];
            self.buf[0x30..0x32].copy_from_slice(&check);
            for i in 1..3 {
                let end = i * 512;
                let (x, y) = (self.buf[end - 2], self.buf[end - 1]);
                self.buf[0x30 + i * 2] = x;
                self.buf[0x30 + i * 2 + 1] = y;
                self.buf[end - 2..end].copy_from_slice(&check);
            }
            self.buf
        }
    }

    const UNIX_EPOCH_FILETIME: i64 = 116_444_736_000_000_000;

    #[test]
    fn decodes_extents() {
        // 0x18 clusters at 0x5634, then 0x10 more starting 0x10 earlier,
        // then a 4-cluster hole.
        let runs = [0x21, 0x18, 0x34, 0x56, 0x11, 0x10, 0xF0, 0x01, 0x04, 0x00];
        assert_eq!(
            data_runs(&runs).unwrap(),
            vec![(Some(0x5634), 0x18), (Some(0x5624), 0x10), (None, 4)]
        );
    }

    #[test]
    fn fixups_catch_torn_records() {
        let mut good = Builder::new(1, 0).build();
        assert!(fixup(&mut good.clone()));
        good[1022] ^= 1;
        assert!(!fixup(&mut good));
    }

    #[test]
    fn reads_files_folders_links_and_hard_links() {
        let mut parser = Parser::default();
        let at = |seconds: i64| UNIX_EPOCH_FILETIME + seconds * 10_000_000;
        // A folder in the root.
        let mut dir = Builder::new(3, 0)
            .standard(at(100), 0x10)
            .file_name(5, 1, "Games")
            .build();
        parser.record(30, &mut dir);
        // A file with a long name, its DOS alias, and a hard link elsewhere.
        let mut file = Builder::new(1, 0)
            .standard(at(200), 0x20)
            .file_name(30, 1, "Long file name.mp4")
            .file_name(30, 2, "LONGFI~1.MP4")
            .file_name(5, 1, "again.mp4")
            .nonresident(0, 8192, 5000, 0)
            .build();
        parser.record(31, &mut file);
        // A sparse file: only what's allocated counts on disk.
        let mut sparse = Builder::new(1, 0)
            .standard(at(0), 0)
            .file_name(30, 3, "sparse.img")
            .nonresident(0x8000, 1 << 30, 1 << 30, 4096)
            .build();
        parser.record(32, &mut sparse);
        // A junction: counted, not followed.
        let mut junction = Builder::new(3, 0)
            .standard(at(0), 0x410)
            .file_name(5, 1, "Link")
            .resident(0xC0, &[], &0xA000_0003u32.to_le_bytes())
            .build();
        parser.record(33, &mut junction);
        // A small file kept inside its record, with its data in an
        // extension record.
        let mut small = Builder::new(1, 0)
            .standard(at(0), 0)
            .file_name(5, 1, "note.txt")
            .build();
        parser.record(34, &mut small);
        let mut extension = Builder::new(1, 34).resident(0x80, &[], b"hello").build();
        parser.record(35, &mut extension);
        // Deleted, and one of NTFS's own files.
        let mut deleted = Builder::new(0, 0).file_name(5, 1, "gone").build();
        parser.record(36, &mut deleted);
        let mut system = Builder::new(1, 0).file_name(5, 3, "$Bitmap").build();
        parser.record(6, &mut system);

        let mut entries = parser.finish();
        entries.sort_by_key(|e| (e.record, e.parent));
        let names: Vec<(u32, u32, String)> = entries
            .iter()
            .map(|e| (e.record, e.parent, String::from_utf16_lossy(&e.name)))
            .collect();
        assert_eq!(
            names,
            vec![
                (30, 5, "Games".into()),
                (31, 5, "again.mp4".into()),
                (31, 30, "Long file name.mp4".into()),
                (32, 30, "sparse.img".into()),
                (33, 5, "Link".into()),
                (34, 5, "note.txt".into()),
            ]
        );
        assert_eq!(entries[0].kind, EntryKind::Dir);
        assert_eq!(entries[0].modified, 100);
        let long = &entries[2];
        assert_eq!(
            (long.kind, long.disk, long.apparent, long.links),
            (EntryKind::File, 8192, 5000, 2)
        );
        assert_eq!((entries[3].disk, entries[3].apparent), (4096, 1 << 30));
        assert_eq!(entries[4].kind, EntryKind::Link);
        assert_eq!((entries[5].disk, entries[5].apparent), (0, 5));
    }

    #[test]
    fn finds_the_table_from_the_boot_sector() {
        let mut boot = vec![0u8; 512];
        boot[3..11].copy_from_slice(b"NTFS    ");
        put16(&mut boot, 0x0B, 512);
        boot[0x0D] = 8;
        put64(&mut boot, 0x30, 786_432);
        boot[0x40] = 0xF6; // -10: 1024-byte records
        assert_eq!(
            geometry(&boot).unwrap(),
            Geometry {
                cluster: 4096,
                record: 1024,
                mft_offset: 786_432 * 4096,
            }
        );
        boot[3] = b'X';
        assert_eq!(
            geometry(&boot).unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );

        let mut record0 = Builder::new(1, 0)
            .file_name(5, 3, "$MFT")
            .nonresident(0, 0x10 * 4096, 0x10 * 4096, 0)
            .build();
        let (runs, size) = mft_extents(&mut record0, 4096).unwrap();
        assert_eq!(runs, vec![(Some(0x100), 0x10)]);
        assert_eq!(size, 0x10 * 4096);
    }

    #[test]
    fn entries_survive_the_trip_from_the_helper() {
        let entries = vec![
            Entry {
                record: 40,
                parent: 5,
                kind: EntryKind::File,
                disk: 4096,
                apparent: 12,
                modified: 1_700_000_000,
                links: 1,
                name: "ünïcode.txt".encode_utf16().collect(),
            },
            Entry {
                record: 41,
                parent: 40,
                kind: EntryKind::Link,
                disk: 0,
                apparent: 0,
                modified: 0,
                links: 1,
                name: Vec::new(),
            },
        ];
        let mut bytes = Vec::new();
        write_entries(&mut bytes, &entries).unwrap();
        assert_eq!(read_entries(&mut bytes.as_slice()).unwrap(), entries);
        assert!(read_entries(&mut &bytes[..bytes.len() - 1]).is_err());
    }

    #[test]
    fn only_drive_roots_have_letters() {
        assert_eq!(drive_letter(Path::new("/home")), None);
        #[cfg(windows)]
        {
            assert_eq!(drive_letter(Path::new(r"C:\")), Some('C'));
            assert_eq!(drive_letter(Path::new(r"\\?\S:\")), Some('S'));
            assert_eq!(drive_letter(Path::new(r"C:\Users")), None);
            assert_eq!(drive_letter(Path::new(r"\\server\share\")), None);
        }
    }
}
