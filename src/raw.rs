//! Camera raw files, as far as a file manager needs them: the JPEGs cameras
//! store in them, and their camera details.
//!
//! Developing the sensor's data would take seconds and give other colours
//! than the camera's own, and the Rust decoders for it are LGPL. Only the
//! parts that are needed are read, so a server's files stay quick.
//!
//! Files can say anything here, so everything's checked against what's there.

use std::{
    collections::HashSet,
    io::{Read, Seek, SeekFrom},
    path::Path,
};

const EXTENSIONS: &[&str] = &[
    "cr2", "cr3", "nef", "nrw", "arw", "dng", "raf", "orf", "rw2", "pef", "srw",
];

/// Directories followed at most, in a file that may be looping or made up.
const MAX_IFDS: usize = 64;
/// Boxes looked at in a CR3 at most.
const MAX_BOXES: usize = 4096;
/// As much of a JPEG as is read to learn its size: past any metadata.
const JPEG_HEAD_BYTES: usize = 128 * 1024;
/// As much of a directory as is read at first, enough for nearly all of them.
const IFD_HEAD_BYTES: usize = 2048;
/// Longest value copied for camera details: others are private blobs.
const MAX_DETAIL_BYTES: u64 = 64 * 1024;
/// All of a directory's values copied for camera details, at most.
const MAX_DIRECTORY_BYTES: u64 = 1024 * 1024;
/// Places a JPEG might be that are looked at. Files have a few at most.
const MAX_SPANS: usize = 16;
/// A JPEG at least this share of the sensor's sides (in 1/100) is the whole
/// photo: the sensor's size counts a border of pixels that aren't in it.
const WHOLE_PHOTO_PERCENT: u64 = 95;
/// Smaller than this, it's no picture worth showing.
const MIN_SIDE: u32 = 32;

pub fn is_raw(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

/// A JPEG found in a raw file.
#[derive(Debug)]
pub struct Preview {
    pub jpeg: Vec<u8>,
    /// The JPEG's size, as stored.
    pub size: [u32; 2],
    /// Exif's orientation of the picture.
    pub orientation: u8,
    /// The size of the photo the sensor took, as stored.
    pub sensor: [u32; 2],
}

/// The smallest of the file's pictures that's at least `side` long, or else
/// its biggest.
pub fn preview<R: Read + Seek>(file: &mut R, side: u32) -> Option<Preview> {
    let mut source = Source::new(file)?;
    let layout = Layout::of(&mut source)?;
    let found = pictures(&mut source, &layout);
    let &(_, biggest) = found.last()?;
    // Some cameras pad their small ones with bars to fit a fixed shape.
    let (span, jpeg) = found
        .iter()
        .filter(|(_, jpeg)| jpeg.same_shape(&biggest))
        .find(|(_, jpeg)| jpeg.longest() >= side)
        .or(found.last())
        .copied()?;
    let bytes = source.read(span.start, span.len.try_into().ok()?)?;
    Some(Preview {
        jpeg: bytes,
        size: jpeg.size,
        orientation: orientation(&layout, Some(&biggest)),
        sensor: photo_size(&layout, Some(&biggest))?,
    })
}

/// The size of the photo as it's shown, turned as it should be.
pub fn size<R: Read + Seek>(file: &mut R) -> Option<[u32; 2]> {
    let mut source = Source::new(file)?;
    let layout = Layout::of(&mut source)?;
    let found = pictures(&mut source, &layout);
    let biggest = found.last().map(|(_, jpeg)| jpeg);
    let [width, height] = photo_size(&layout, biggest)?;
    Some(if is_turned(orientation(&layout, biggest)) {
        [height, width]
    } else {
        [width, height]
    })
}

/// Whether an Exif orientation shows a picture turned a quarter.
pub fn is_turned(orientation: u8) -> bool {
    (5..=8).contains(&orientation)
}

/// What the file has that can be shown, smallest first.
fn pictures<R: Read + Seek>(source: &mut Source<'_, R>, layout: &Layout) -> Vec<(Span, Jpeg)> {
    let mut found: Vec<(Span, Jpeg)> = layout
        .spans
        .iter()
        .filter_map(|&span| Some((span, jpeg_info(source, span)?)))
        .collect();
    found.sort_by_key(|(_, jpeg)| jpeg.longest());
    found
}

fn orientation(layout: &Layout, biggest: Option<&Jpeg>) -> u8 {
    layout
        .orientation
        .or(biggest.and_then(|jpeg| jpeg.orientation))
        .unwrap_or(1)
}

fn photo_size(layout: &Layout, biggest: Option<&Jpeg>) -> Option<[u32; 2]> {
    let Some(jpeg) = biggest else {
        return layout.sensor;
    };
    Some(match layout.sensor {
        Some(sensor)
            if u64::from(jpeg.longest()) * 100
                < u64::from(sensor[0].max(sensor[1])) * WHOLE_PHOTO_PERCENT =>
        {
            sensor
        }
        _ => jpeg.size,
    })
}

/// The file's camera details, as a small TIFF file of their own.
pub fn exif<R: Read + Seek>(file: &mut R) -> Option<Vec<u8>> {
    let mut source = Source::new(file)?;
    let layout = Layout::of(&mut source)?;
    let details = match layout.details {
        Some(details) => details,
        None => {
            // In the biggest JPEG's own Exif data.
            let found = pictures(&mut source, &layout);
            let &(_, jpeg) = found.last()?;
            Details::of_tiff(&mut source, jpeg.exif?)?
        }
    };
    details.write(&mut source)
}

/// Reads parts of the file, never more than it has.
struct Source<'a, R> {
    file: &'a mut R,
    len: u64,
}

impl<'a, R: Read + Seek> Source<'a, R> {
    fn new(file: &'a mut R) -> Option<Self> {
        let len = file.seek(SeekFrom::End(0)).ok()?;
        Some(Self { file, len })
    }

    /// Up to `len` bytes at `at`; `None` if there's nothing there.
    fn read(&mut self, at: u64, len: usize) -> Option<Vec<u8>> {
        let left = self.len.checked_sub(at).filter(|left| *left > 0)?;
        let len = (len as u64).min(left);
        self.file.seek(SeekFrom::Start(at)).ok()?;
        let mut bytes = Vec::new();
        (&mut *self.file).take(len).read_to_end(&mut bytes).ok()?;
        (!bytes.is_empty()).then_some(bytes)
    }

    /// Exactly `len` bytes at `at`.
    fn exact(&mut self, at: u64, len: usize) -> Option<Vec<u8>> {
        self.read(at, len).filter(|bytes| bytes.len() == len)
    }
}

/// A run of bytes in the file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Span {
    start: u64,
    len: u64,
}

#[derive(Default)]
struct Layout {
    /// Where there might be JPEGs. Some will be the sensor's data, in
    /// JPEG's lossless form, which only looking inside tells apart.
    spans: Vec<Span>,
    orientation: Option<u8>,
    sensor: Option<[u32; 2]>,
    details: Option<Details>,
}

impl Layout {
    fn of<R: Read + Seek>(source: &mut Source<'_, R>) -> Option<Self> {
        let head = source.read(0, 96)?;
        let mut layout = if head.starts_with(b"FUJIFILMCCD-RAW") {
            Self::of_raf(&head)?
        } else if head.get(4..8) == Some(b"ftyp") {
            Self::of_cr3(source)?
        } else {
            Self::of_tiff(source)?
        };
        let mut seen = HashSet::new();
        layout.spans.retain(|span| seen.insert(span.start));
        layout.spans.truncate(MAX_SPANS);
        Some(layout)
    }

    /// Fujifilm's: its header says where the JPEG is, which is full size
    /// and has the camera details.
    fn of_raf(head: &[u8]) -> Option<Self> {
        let u32_at = |at: usize| {
            Some(u64::from(u32::from_be_bytes(
                head.get(at..at + 4)?.try_into().ok()?,
            )))
        };
        Some(Self {
            spans: vec![Span {
                start: u32_at(84)?,
                len: u32_at(88)?,
            }],
            ..Self::default()
        })
    }

    fn of_tiff<R: Read + Seek>(source: &mut Source<'_, R>) -> Option<Self> {
        let tiff = Tiff::at(source, 0)?;
        let mut layout = Self {
            // Panasonic's first directory is its own; its JPEG has them.
            details: Details::of_tiff(source, 0).filter(|_| tiff.magic != MAGIC_PANASONIC),
            ..Self::default()
        };
        let mut sizes = Sizes::default();
        let mut seen = HashSet::new();
        let mut pending = vec![(tiff.first, Directory::Image)];
        while let Some((at, directory)) = pending.pop() {
            if seen.len() >= MAX_IFDS || !seen.insert(at) {
                continue;
            }
            let Some((entries, next)) = tiff.entries(source, at) else {
                continue;
            };
            let mut image = Image::default();
            for entry in &entries {
                match (directory, entry.tag) {
                    (Directory::Image, 0x0100) => image.width = tiff.int(entry),
                    (Directory::Image, 0x0101) => image.height = tiff.int(entry),
                    (Directory::Image, 0x0111) => image.strip = tiff.int(entry),
                    (Directory::Image, 0x0117) => image.strip_len = tiff.int(entry),
                    (Directory::Image, 0x0201) => image.jpeg = tiff.int(entry),
                    (Directory::Image, 0x0202) => image.jpeg_len = tiff.int(entry),
                    (Directory::Image, 0x0112) if at == tiff.first => {
                        layout.orientation = tiff.int(entry).map(|o| o as u8);
                    }
                    (Directory::Image, 0x8769) => {
                        if let Some(exif) = tiff.int(entry) {
                            pending.push((exif, Directory::Exif));
                        }
                    }
                    (Directory::Image, 0x014A) => {
                        pending.extend(
                            tiff.ints(source, entry, MAX_IFDS)
                                .into_iter()
                                .map(|sub| (sub, Directory::Image)),
                        );
                    }
                    // Panasonic's: the JPEG, right there as the value.
                    (Directory::Image, 0x002E) => {
                        if tiff.magic == MAGIC_PANASONIC {
                            let start = u64::from(read_u32(&entry.value, tiff.le));
                            layout.spans.push(Span {
                                start,
                                len: u64::from(entry.count),
                            });
                        }
                    }
                    (Directory::Exif, 0x927C) => {
                        layout.spans.extend(olympus_preview(source, &tiff, entry));
                    }
                    // Panasonic's sensor size, in its own numbering.
                    (Directory::Image, 0x0002) if tiff.magic == MAGIC_PANASONIC => {
                        image.width = tiff.int(entry);
                    }
                    (Directory::Image, 0x0003) if tiff.magic == MAGIC_PANASONIC => {
                        image.height = tiff.int(entry);
                    }
                    (Directory::Exif, 0xA002) => sizes.exif_width = tiff.int(entry),
                    (Directory::Exif, 0xA003) => sizes.exif_height = tiff.int(entry),
                    _ => {}
                }
            }
            sizes.see(&image);
            layout.spans.extend(image.spans());
            if next != 0 && directory == Directory::Image {
                pending.push((next, directory));
            }
        }
        layout.sensor = sizes.sensor();
        Some(layout)
    }

    /// Canon's newer files, boxes as in an MP4.
    fn of_cr3<R: Read + Seek>(source: &mut Source<'_, R>) -> Option<Self> {
        let mut layout = Self::default();
        let mut details = Details::default();
        let mut budget = MAX_BOXES;
        let len = source.len;
        for (kind, body) in boxes(source, 0, len, &mut budget) {
            match &kind {
                b"moov" => {
                    let children = boxes(source, body.start, body.end(), &mut budget);
                    for (kind, body) in children {
                        if &kind == b"uuid"
                            && source.exact(body.start, 16)?.as_slice() == UUID_CANON
                        {
                            layout.of_canon_box(source, body, &mut details, &mut budget);
                        }
                    }
                }
                b"uuid" if source.exact(body.start, 16)?.as_slice() == UUID_PREVIEW => {
                    // The box inside, after the UUID and 8 bytes of its own.
                    let inner = body.start + 24;
                    for (kind, body) in boxes(source, inner, body.end(), &mut budget) {
                        if &kind == b"PRVW" {
                            layout.spans.extend(jpeg_within(source, body));
                        }
                    }
                }
                _ => {}
            }
        }
        if let Some(root) = details.root {
            let tiff = root;
            if let Some((entries, _)) = tiff.entries(source, tiff.first) {
                let (mut width, mut height) = (None, None);
                for entry in &entries {
                    match entry.tag {
                        0x0100 => width = tiff.int(entry),
                        0x0101 => height = tiff.int(entry),
                        0x0112 => layout.orientation = tiff.int(entry).map(|o| o as u8),
                        _ => {}
                    }
                }
                layout.sensor = pair(width, height);
            }
        }
        layout.details = details.root.map(|_| details);
        Some(layout)
    }

    /// Canon's metadata: the small picture, and a TIFF for each of the
    /// camera details' parts.
    fn of_canon_box<R: Read + Seek>(
        &mut self,
        source: &mut Source<'_, R>,
        body: Span,
        details: &mut Details,
        budget: &mut usize,
    ) {
        for (kind, inner) in boxes(source, body.start + 16, body.end(), budget) {
            match &kind {
                b"THMB" => self.spans.extend(jpeg_within(source, inner)),
                b"CMT1" => details.root = Tiff::at(source, inner.start),
                b"CMT2" => details.exif = Tiff::at(source, inner.start),
                b"CMT4" => details.gps = Tiff::at(source, inner.start),
                _ => {}
            }
        }
    }
}

const UUID_CANON: &[u8] = &[
    0x85, 0xC0, 0xB6, 0x87, 0x82, 0x0F, 0x11, 0xE0, 0x81, 0x11, 0xF4, 0xCE, 0x46, 0x2B, 0x6A, 0x48,
];
const UUID_PREVIEW: &[u8] = &[
    0xEA, 0xF4, 0x2B, 0x5E, 0x1C, 0x98, 0x4B, 0x88, 0xB9, 0xFB, 0xB7, 0xDC, 0x40, 0x6E, 0x4D, 0x16,
];

impl Span {
    fn end(self) -> u64 {
        self.start.saturating_add(self.len)
    }
}

/// The boxes between `start` and `end`, each with what's inside it.
fn boxes<R: Read + Seek>(
    source: &mut Source<'_, R>,
    start: u64,
    end: u64,
    budget: &mut usize,
) -> Vec<([u8; 4], Span)> {
    let end = end.min(source.len);
    let mut found = Vec::new();
    let mut at = start;
    while at < end && *budget > 0 {
        *budget -= 1;
        let Some(head) = source.exact(at, 16) else {
            break;
        };
        let kind: [u8; 4] = [head[4], head[5], head[6], head[7]];
        let size = u64::from(u32::from_be_bytes([head[0], head[1], head[2], head[3]]));
        let (header, size) = match size {
            0 => (8, end - at),
            1 => (
                16,
                u64::from_be_bytes([
                    head[8], head[9], head[10], head[11], head[12], head[13], head[14], head[15],
                ]),
            ),
            size => (8, size),
        };
        if size < header || at.saturating_add(size) > end {
            break;
        }
        found.push((
            kind,
            Span {
                start: at + header,
                len: size - header,
            },
        ));
        at += size;
    }
    found
}

/// Olympus keep their picture's place in the camera's own notes. Those are a
/// TIFF directory with a header saying which kind they are.
fn olympus_preview<R: Read + Seek>(
    source: &mut Source<'_, R>,
    tiff: &Tiff,
    entry: &Entry,
) -> Option<Span> {
    let at = tiff
        .base
        .checked_add(u64::from(read_u32(&entry.value, tiff.le)))?;
    let head = source.exact(at, 16)?;
    let own = |first: u64, order: &[u8]| Tiff {
        base: at,
        le: order == b"II",
        first,
        magic: MAGIC_TIFF,
    };
    let note = if head.starts_with(b"OLYMPUS\0") {
        own(12, &head[8..10])
    } else if head.starts_with(b"OM SYSTEM\0") {
        own(16, &head[12..14])
    } else if head.starts_with(b"OLYMP\0") {
        Tiff {
            first: at.checked_sub(tiff.base)? + 8,
            ..*tiff
        }
    } else {
        return None;
    };
    let (entries, _) = note.entries(source, note.first)?;
    let settings = entries.iter().find(|e| e.tag == 0x2020)?;
    let settings = u64::from(read_u32(&settings.value, note.le));
    let (entries, _) = note.entries(source, settings)?;
    let number = |tag: u16| note.int(entries.iter().find(|e| e.tag == tag)?);
    Some(Span {
        start: note.base.checked_add(number(0x0101)?)?,
        len: number(0x0102)?,
    })
}

/// Where a JPEG starts in a box's first bytes, up to the end of the box.
fn jpeg_within<R: Read + Seek>(source: &mut Source<'_, R>, body: Span) -> Option<Span> {
    let head = source.read(body.start, 64)?;
    let offset = head.windows(3).position(|w| w == [0xFF, 0xD8, 0xFF])? as u64;
    Some(Span {
        start: body.start + offset,
        len: body.len.checked_sub(offset)?,
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Directory {
    Image,
    Exif,
}

/// What a directory says about the picture it's for.
#[derive(Default)]
struct Image {
    width: Option<u64>,
    height: Option<u64>,
    strip: Option<u64>,
    strip_len: Option<u64>,
    jpeg: Option<u64>,
    jpeg_len: Option<u64>,
}

impl Image {
    fn spans(&self) -> impl Iterator<Item = Span> + use<> {
        let span = |start: Option<u64>, len: Option<u64>| {
            Some(Span {
                start: start?,
                len: len?,
            })
        };
        [
            span(self.strip, self.strip_len),
            span(self.jpeg, self.jpeg_len),
        ]
        .into_iter()
        .flatten()
    }
}

/// The sizes a file's directories give.
#[derive(Default)]
struct Sizes {
    exif_width: Option<u64>,
    exif_height: Option<u64>,
    biggest: Option<[u32; 2]>,
}

impl Sizes {
    fn see(&mut self, image: &Image) {
        if let Some(size) = pair(image.width, image.height)
            && self.biggest.is_none_or(|[w, h]| {
                u64::from(size[0]) * u64::from(size[1]) > u64::from(w) * u64::from(h)
            })
        {
            self.biggest = Some(size);
        }
    }

    fn sensor(&self) -> Option<[u32; 2]> {
        pair(self.exif_width, self.exif_height).or(self.biggest)
    }
}

fn pair(width: Option<u64>, height: Option<u64>) -> Option<[u32; 2]> {
    let (width, height) = (width?, height?);
    (width > 0 && height > 0).then_some([u32::try_from(width).ok()?, u32::try_from(height).ok()?])
}

#[derive(Clone, Copy)]
struct Jpeg {
    size: [u32; 2],
    orientation: Option<u8>,
    /// Where its Exif data's TIFF starts in the file.
    exif: Option<u64>,
}

impl Jpeg {
    fn longest(&self) -> u32 {
        self.size[0].max(self.size[1])
    }

    fn same_shape(&self, other: &Self) -> bool {
        let [w, h] = self.size.map(u64::from);
        let [other_w, other_h] = other.size.map(u64::from);
        (w * other_h).abs_diff(h * other_w) * 50 <= w * other_h
    }
}

/// What's known of the picture at `span`, if it's one that can be shown.
fn jpeg_info<R: Read + Seek>(source: &mut Source<'_, R>, span: Span) -> Option<Jpeg> {
    if span.end() > source.len {
        return None;
    }
    let head = source.read(span.start, JPEG_HEAD_BYTES.min(span.len as usize))?;
    if !head.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return None;
    }
    let (mut orientation, mut exif) = (None, None);
    let mut at = 2;
    while let (Some(&0xFF), Some(&marker)) = (head.get(at), head.get(at + 1)) {
        if marker == 0xFF {
            at += 1;
            continue;
        }
        let len = usize::from(u16::from_be_bytes([*head.get(at + 2)?, *head.get(at + 3)?]));
        let body = head.get(at + 4..at + 2 + len)?;
        match marker {
            // Stored the usual way. The sensor's data is lossless (0xC3),
            // and more than 8 bits.
            0xC0..=0xC2 => {
                let [precision, h1, h0, w1, w0, components] = *body.get(..6)? else {
                    return None;
                };
                if precision != 8 || !matches!(components, 1 | 3) {
                    return None;
                }
                let size = [
                    u32::from(u16::from_be_bytes([w1, w0])),
                    u32::from(u16::from_be_bytes([h1, h0])),
                ];
                if size[0].max(size[1]) < MIN_SIDE || size[0].min(size[1]) == 0 {
                    return None;
                }
                return Some(Jpeg {
                    size,
                    orientation,
                    exif,
                });
            }
            0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF | 0xDA | 0xD9 => return None,
            0xE1 if exif.is_none() && body.starts_with(b"Exif\0\0") => {
                let start = span.start + at as u64 + 4 + 6;
                exif = Some(start);
                orientation = orientation_in(&body[6..]);
            }
            _ => {}
        }
        at += 2 + len;
    }
    None
}

/// The orientation in Exif data's first directory.
fn orientation_in(tiff: &[u8]) -> Option<u8> {
    let le = match tiff.get(..2)? {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let u16_at = |at: usize| Some(read_u16(tiff.get(at..at + 2)?, le));
    let first = read_u32(tiff.get(4..8)?, le) as usize;
    if first >= tiff.len() {
        return None;
    }
    let count = usize::from(u16_at(first)?);
    (0..count)
        .map(|i| first + 2 + 12 * i)
        .find(|&at| u16_at(at) == Some(0x0112))
        .and_then(|at| u16_at(at + 8))
        .map(|o| o as u8)
}

fn read_u16(bytes: &[u8], le: bool) -> u16 {
    let bytes = [bytes[0], bytes[1]];
    if le {
        u16::from_le_bytes(bytes)
    } else {
        u16::from_be_bytes(bytes)
    }
}

fn read_u32(bytes: &[u8], le: bool) -> u32 {
    let bytes = [bytes[0], bytes[1], bytes[2], bytes[3]];
    if le {
        u32::from_le_bytes(bytes)
    } else {
        u32::from_be_bytes(bytes)
    }
}

/// A TIFF structure at `base` in the file, whose offsets count from there.
#[derive(Clone, Copy)]
struct Tiff {
    base: u64,
    le: bool,
    first: u64,
    magic: u16,
}

const MAGIC_TIFF: u16 = 42;
const MAGIC_PANASONIC: u16 = 0x55;
const MAGIC_OLYMPUS: [u16; 2] = [0x4F52, 0x5352];

#[derive(Clone, Copy)]
struct Entry {
    tag: u16,
    kind: u16,
    count: u32,
    /// The value, if it fits, or else where it is.
    value: [u8; 4],
}

impl Entry {
    fn size(&self) -> Option<u64> {
        let each = match self.kind {
            1 | 2 | 6 | 7 => 1,
            3 | 8 => 2,
            4 | 9 | 11 | 13 => 4,
            5 | 10 | 12 => 8,
            _ => return None,
        };
        Some(each * u64::from(self.count))
    }
}

impl Tiff {
    fn at<R: Read + Seek>(source: &mut Source<'_, R>, base: u64) -> Option<Self> {
        let head = source.exact(base, 8)?;
        let le = match &head[..2] {
            b"II" => true,
            b"MM" => false,
            _ => return None,
        };
        let magic = read_u16(&head[2..], le);
        if magic != MAGIC_TIFF && magic != MAGIC_PANASONIC && !MAGIC_OLYMPUS.contains(&magic) {
            return None;
        }
        Some(Self {
            base,
            le,
            first: u64::from(read_u32(&head[4..], le)),
            magic,
        })
    }

    /// A directory's entries, and where the next directory is.
    fn entries<R: Read + Seek>(
        &self,
        source: &mut Source<'_, R>,
        at: u64,
    ) -> Option<(Vec<Entry>, u64)> {
        let start = self.base.checked_add(at)?;
        let mut bytes = source.read(start, IFD_HEAD_BYTES)?;
        let count = usize::from(read_u16(bytes.get(..2)?, self.le));
        let needed = 2 + 12 * count + 4;
        if bytes.len() < needed {
            bytes = source.read(start, needed)?;
        }
        let table = bytes.get(2..2 + 12 * count)?;
        let entries = table
            .as_chunks::<12>()
            .0
            .iter()
            .map(|e| Entry {
                tag: read_u16(e, self.le),
                kind: read_u16(&e[2..], self.le),
                count: read_u32(&e[4..], self.le),
                value: [e[8], e[9], e[10], e[11]],
            })
            .collect();
        let next = bytes
            .get(2 + 12 * count..needed)
            .map_or(0, |b| u64::from(read_u32(b, self.le)));
        Some((entries, next))
    }

    /// An entry's only value, if it's a whole number.
    fn int(&self, entry: &Entry) -> Option<u64> {
        if entry.count != 1 {
            return None;
        }
        match entry.kind {
            1 | 7 => Some(u64::from(entry.value[0])),
            3 => Some(u64::from(read_u16(&entry.value, self.le))),
            4 | 13 => Some(u64::from(read_u32(&entry.value, self.le))),
            _ => None,
        }
    }

    /// An entry's first numbers, up to `max`.
    fn ints<R: Read + Seek>(
        &self,
        source: &mut Source<'_, R>,
        entry: &Entry,
        max: usize,
    ) -> Vec<u64> {
        let each = match entry.kind {
            3 => 2,
            4 | 13 => 4,
            _ => return Vec::new(),
        };
        let count = (entry.count as usize).min(max);
        let Some(bytes) = self.value(source, entry, each * count as u64) else {
            return Vec::new();
        };
        bytes
            .chunks_exact(each as usize)
            .map(|b| match each {
                2 => u64::from(read_u16(b, self.le)),
                _ => u64::from(read_u32(b, self.le)),
            })
            .collect()
    }

    /// The first `len` bytes of an entry's value.
    fn value<R: Read + Seek>(
        &self,
        source: &mut Source<'_, R>,
        entry: &Entry,
        len: u64,
    ) -> Option<Vec<u8>> {
        if entry.size()? <= 4 {
            return entry.value.get(..len as usize).map(<[u8]>::to_vec);
        }
        let at = self
            .base
            .checked_add(u64::from(read_u32(&entry.value, self.le)))?;
        source.exact(at, len.try_into().ok()?)
    }
}

/// Where a file's camera details are.
#[derive(Default)]
struct Details {
    root: Option<Tiff>,
    exif: Option<Tiff>,
    gps: Option<Tiff>,
}

/// The tags kept from a file's first directory; the rest is about how the
/// sensor's data is stored.
const ROOT_TAGS: &[u16] = &[
    0x010E, 0x010F, 0x0110, 0x0112, 0x0131, 0x0132, 0x013B, 0x8298,
];
/// Left out of the Exif directory: Interoperability's pointer, MakerNote.
const EXIF_SKIPPED: &[u16] = &[0xA005, 0x927C];
/// Exif's own, when found in the first directory.
const ROOT_EXIF_TAGS: std::ops::RangeInclusive<u16> = 0x829A..=0xA500;

impl Details {
    /// Those in an ordinary TIFF structure at `base`.
    fn of_tiff<R: Read + Seek>(source: &mut Source<'_, R>, base: u64) -> Option<Self> {
        Tiff::at(source, base).map(|tiff| Self {
            root: Some(tiff),
            ..Self::default()
        })
    }

    /// Copies what's wanted into a new TIFF: each part's values, without
    /// the sensor's data.
    fn write<R: Read + Seek>(&self, source: &mut Source<'_, R>) -> Option<Vec<u8>> {
        let root = self.root?;
        let (entries, _) = root.entries(source, root.first)?;
        let find = |tag: u16| entries.iter().find(|e| e.tag == tag).copied();

        // Those from a part of their own, or where the first directory says.
        let part = |source: &mut Source<'_, R>, own: Option<Tiff>, tag: u16| match own {
            Some(tiff) => tiff
                .entries(source, tiff.first)
                .map(|(entries, _)| (tiff, entries)),
            None => {
                let at = root.int(&find(tag)?)?;
                root.entries(source, at).map(|(entries, _)| (root, entries))
            }
        };

        let mut out = Vec::new();
        out.extend_from_slice(if root.le { b"II" } else { b"MM" });
        push_u16(&mut out, 42, root.le);
        push_u32(&mut out, 0, root.le);

        // Some phones' DNGs have these in the first directory instead.
        let exif = part(source, self.exif, 0x8769)
            .or_else(|| {
                let own = entries.iter().filter(|e| ROOT_EXIF_TAGS.contains(&e.tag));
                Some((root, own.copied().collect()))
            })
            .and_then(|(tiff, entries)| {
                let keep = entries
                    .iter()
                    .filter(|e| !EXIF_SKIPPED.contains(&e.tag))
                    .copied()
                    .collect();
                write_directory(&mut out, source, tiff, root.le, keep, &[])
            });
        let gps = part(source, self.gps, 0x8825).and_then(|(tiff, entries)| {
            write_directory(&mut out, source, tiff, root.le, entries, &[])
        });

        let keep = entries
            .iter()
            .filter(|e| ROOT_TAGS.contains(&e.tag))
            .copied()
            .collect();
        let pointers: Vec<(u16, u32)> = [(0x8769, exif), (0x8825, gps)]
            .into_iter()
            .filter_map(|(tag, at)| Some((tag, at?)))
            .collect();
        let first = write_directory(&mut out, source, root, root.le, keep, &pointers)?;
        let first = if root.le {
            first.to_le_bytes()
        } else {
            first.to_be_bytes()
        };
        out[4..8].copy_from_slice(&first);
        Some(out)
    }
}

fn push_u16(out: &mut Vec<u8>, value: u16, le: bool) {
    out.extend_from_slice(&if le {
        value.to_le_bytes()
    } else {
        value.to_be_bytes()
    });
}

fn push_u32(out: &mut Vec<u8>, value: u32, le: bool) {
    out.extend_from_slice(&if le {
        value.to_le_bytes()
    } else {
        value.to_be_bytes()
    });
}

/// Writes a directory of `entries` and `pointers` (numbers to other
/// directories) at the end of `out`, returning where it is. All of a file's
/// parts have the same byte order, or the entries are left out.
fn write_directory<R: Read + Seek>(
    out: &mut Vec<u8>,
    source: &mut Source<'_, R>,
    tiff: Tiff,
    le: bool,
    entries: Vec<Entry>,
    pointers: &[(u16, u32)],
) -> Option<u32> {
    let mut values: Vec<(Entry, Vec<u8>)> = Vec::new();
    if tiff.le == le {
        let mut left = MAX_DIRECTORY_BYTES;
        for mut entry in entries {
            let Some(size) = entry
                .size()
                .filter(|size| *size <= MAX_DETAIL_BYTES.min(left))
            else {
                continue;
            };
            let Some(mut bytes) = tiff.value(source, &entry, size) else {
                continue;
            };
            left -= size;
            // Some pad text with zeros or spaces, which would show up.
            if entry.kind == 2 {
                let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
                bytes.truncate(end);
                while bytes.last() == Some(&b' ') {
                    bytes.pop();
                }
                bytes.push(0);
                entry.count = bytes.len() as u32;
            }
            values.push((entry, bytes));
        }
    }
    for &(tag, at) in pointers {
        let value = if le {
            at.to_le_bytes()
        } else {
            at.to_be_bytes()
        };
        let entry = Entry {
            tag,
            kind: 4,
            count: 1,
            value,
        };
        values.push((entry, value.to_vec()));
    }
    values.sort_by_key(|(entry, _)| entry.tag);

    let start = u32::try_from(out.len()).ok()?;
    let mut after = out.len() + 2 + 12 * values.len() + 4;
    push_u16(out, u16::try_from(values.len()).ok()?, le);
    for (entry, bytes) in &values {
        push_u16(out, entry.tag, le);
        push_u16(out, entry.kind, le);
        push_u32(out, entry.count, le);
        if bytes.len() <= 4 {
            let mut inline = [0; 4];
            inline[..bytes.len()].copy_from_slice(bytes);
            out.extend_from_slice(&inline);
        } else {
            push_u32(out, u32::try_from(after).ok()?, le);
            after += bytes.len();
        }
    }
    push_u32(out, 0, le);
    for (_, bytes) in &values {
        if bytes.len() > 4 {
            out.extend_from_slice(bytes);
        }
    }
    Some(start)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    const LONG: u16 = 4;
    const SHORT: u16 = 3;
    const ASCII: u16 = 2;
    const UNDEFINED: u16 = 7;

    fn jpeg(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        image::DynamicImage::new_rgb8(width, height)
            .write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Jpeg)
            .unwrap();
        bytes
    }

    /// How the sensor's data looks when it's stored as a lossless JPEG.
    fn lossless() -> Vec<u8> {
        vec![
            0xFF, 0xD8, 0xFF, 0xC3, 0, 11, 14, 0x0B, 0xB8, 0x0F, 0xA0, 1, 0, 0x11, 0,
        ]
    }

    fn directory(entries: &[(u16, u16, u32, u32)], next: u32) -> Vec<u8> {
        let mut out = (entries.len() as u16).to_le_bytes().to_vec();
        for (tag, kind, count, value) in entries {
            out.extend_from_slice(&tag.to_le_bytes());
            out.extend_from_slice(&kind.to_le_bytes());
            out.extend_from_slice(&count.to_le_bytes());
            out.extend_from_slice(&value.to_le_bytes());
        }
        out.extend_from_slice(&next.to_le_bytes());
        out
    }

    /// Where what follows a first directory of `count` entries starts.
    fn after_first(count: usize) -> u32 {
        (8 + 2 + 12 * count + 4) as u32
    }

    fn tiff(magic: u16, entries: &[(u16, u16, u32, u32)], tail: &[u8]) -> Vec<u8> {
        let mut out = b"II".to_vec();
        out.extend_from_slice(&magic.to_le_bytes());
        out.extend_from_slice(&8u32.to_le_bytes());
        out.extend(directory(entries, 0));
        out.extend_from_slice(tail);
        out
    }

    /// A TIFF raw file with two JPEGs.
    fn two_jpegs(first: &[u8], second: &[u8]) -> Vec<u8> {
        let at = after_first(4);
        let tail = [first, second].concat();
        tiff(
            42,
            &[
                (0x0111, LONG, 1, at),
                (0x0117, LONG, 1, first.len() as u32),
                (0x0201, LONG, 1, at + first.len() as u32),
                (0x0202, LONG, 1, second.len() as u32),
            ],
            &tail,
        )
    }

    fn preview_of(file: &[u8], side: u32) -> Option<Preview> {
        preview(&mut Cursor::new(file), side)
    }

    fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(body);
        out
    }

    fn cr3() -> Vec<u8> {
        let details = tiff(
            42,
            &[
                (0x0100, LONG, 1, 6000),
                (0x0101, LONG, 1, 4000),
                (0x0112, SHORT, 1, 8),
            ],
            &[],
        );
        let canon = [
            UUID_CANON.to_vec(),
            boxed(b"CMT1", &details),
            boxed(b"THMB", &[&[0; 16][..], &jpeg(160, 120)].concat()),
        ]
        .concat();
        let preview = [
            UUID_PREVIEW.to_vec(),
            vec![0; 8],
            boxed(b"PRVW", &[&[0; 16][..], &jpeg(1620, 1080)].concat()),
        ]
        .concat();
        [
            boxed(b"ftyp", b"crx \0\0\0\x01crx isom"),
            boxed(b"moov", &boxed(b"uuid", &canon)),
            boxed(b"uuid", &preview),
            vec![0; 64],
        ]
        .concat()
    }

    fn raf(jpeg: &[u8]) -> Vec<u8> {
        let mut out = b"FUJIFILMCCD-RAW 0201FF129502".to_vec();
        out.resize(84, 0);
        out.extend_from_slice(&148u32.to_be_bytes());
        out.extend_from_slice(&(jpeg.len() as u32).to_be_bytes());
        out.resize(148, 0);
        out.extend_from_slice(jpeg);
        out
    }

    fn olympus(jpeg: &[u8]) -> Vec<u8> {
        // The first directory, then Exif's, then the camera's notes, whose
        // numbers count from their start.
        let exif_at = after_first(1);
        let note_at = exif_at + 2 + 12 + 4;
        let settings_at = 12 + 2 + 12 + 4;
        let jpeg_at = settings_at + 2 + 2 * 12 + 4;
        let note = [
            b"OLYMPUS\0II\x03\0".to_vec(),
            directory(&[(0x2020, 13, 1, settings_at)], 0),
            directory(
                &[
                    (0x0101, LONG, 1, jpeg_at),
                    (0x0102, LONG, 1, jpeg.len() as u32),
                ],
                0,
            ),
            jpeg.to_vec(),
        ]
        .concat();
        let exif = directory(&[(0x927C, UNDEFINED, note.len() as u32, note_at)], 0);
        tiff(
            0x4F52,
            &[(0x8769, LONG, 1, exif_at)],
            &[exif, note].concat(),
        )
    }

    #[test]
    fn knows_raw_files_by_extension() {
        assert!(is_raw(Path::new("/a/IMG_7504.CR2")));
        assert!(is_raw(Path::new("b.dng")));
        assert!(!is_raw(Path::new("c.jpg")));
        assert!(!is_raw(Path::new("cr2")));
    }

    #[test]
    fn picks_the_smallest_picture_big_enough() {
        let file = two_jpegs(&jpeg(300, 200), &jpeg(1800, 1200));
        assert_eq!(preview_of(&file, 256).unwrap().size, [300, 200]);
        assert_eq!(preview_of(&file, 1600).unwrap().size, [1800, 1200]);
        // Nothing's big enough: the biggest there is.
        assert_eq!(preview_of(&file, 4000).unwrap().size, [1800, 1200]);
    }

    #[test]
    fn passes_over_small_pictures_of_another_shape() {
        // 4:3, for a 3:2 picture: there would be bars.
        let file = two_jpegs(&jpeg(160, 120), &jpeg(600, 400));
        assert_eq!(preview_of(&file, 100).unwrap().size, [600, 400]);
    }

    #[test]
    fn the_sensors_lossless_data_is_no_picture() {
        let file = two_jpegs(&lossless(), &jpeg(600, 400));
        assert_eq!(preview_of(&file, 2000).unwrap().size, [600, 400]);
        let file = two_jpegs(&lossless(), &lossless());
        assert!(preview_of(&file, 256).is_none());
    }

    #[test]
    fn the_jpeg_it_has_is_read_whole_and_valid() {
        let wanted = jpeg(320, 200);
        let file = two_jpegs(&jpeg(64, 40), &wanted);
        assert_eq!(preview_of(&file, 300).unwrap().jpeg, wanted);
    }

    #[test]
    fn says_the_sensors_size_and_turns_it() {
        let picture = jpeg(600, 400);
        let at = after_first(5);
        let file = tiff(
            42,
            &[
                (0x0100, LONG, 1, 6000),
                (0x0101, LONG, 1, 4000),
                (0x0111, LONG, 1, at),
                (0x0112, SHORT, 1, 6),
                (0x0117, LONG, 1, picture.len() as u32),
            ],
            &picture,
        );
        let found = preview_of(&file, 256).unwrap();
        assert_eq!(found.sensor, [6000, 4000]);
        assert_eq!(found.orientation, 6);
        assert_eq!(size(&mut Cursor::new(&file)), Some([4000, 6000]));
    }

    #[test]
    fn a_full_size_jpeg_is_the_photos_size_not_the_sensors() {
        // The sensor's size includes a border that isn't in the photo.
        let picture = jpeg(600, 400);
        let at = after_first(4);
        let file = tiff(
            42,
            &[
                (0x0100, LONG, 1, 610),
                (0x0101, LONG, 1, 408),
                (0x0111, LONG, 1, at),
                (0x0117, LONG, 1, picture.len() as u32),
            ],
            &picture,
        );
        assert_eq!(preview_of(&file, 256).unwrap().sensor, [600, 400]);
    }

    #[test]
    fn with_no_picture_the_size_is_still_known() {
        let file = tiff(42, &[(0x0100, LONG, 1, 4032), (0x0101, LONG, 1, 3024)], &[]);
        assert!(preview_of(&file, 256).is_none());
        assert_eq!(size(&mut Cursor::new(&file)), Some([4032, 3024]));
    }

    #[test]
    fn finds_panasonics_picture() {
        let picture = jpeg(600, 400);
        let file = tiff(
            0x55,
            &[
                (0x0002, SHORT, 1, 4640),
                (0x0003, SHORT, 1, 3480),
                (0x002E, UNDEFINED, picture.len() as u32, after_first(3)),
            ],
            &picture,
        );
        let found = preview_of(&file, 256).unwrap();
        assert_eq!(found.size, [600, 400]);
        assert_eq!(found.sensor, [4640, 3480]);
    }

    #[test]
    fn finds_olympus_picture_in_the_cameras_notes() {
        let picture = jpeg(600, 400);
        assert_eq!(preview_of(&olympus(&picture), 256).unwrap().jpeg, picture);
    }

    #[test]
    fn finds_fujifilms_picture() {
        let picture = jpeg(600, 400);
        let found = preview_of(&raf(&picture), 256).unwrap();
        assert_eq!(found.jpeg, picture);
        assert_eq!(found.sensor, [600, 400]);
    }

    #[test]
    fn finds_cr3s_pictures_and_details() {
        let file = cr3();
        let big = preview_of(&file, 1600).unwrap();
        assert_eq!(big.size, [1620, 1080]);
        assert_eq!(big.sensor, [6000, 4000]);
        assert_eq!(big.orientation, 8);
        assert!(big.jpeg.starts_with(&[0xFF, 0xD8, 0xFF]));
        // The small one's 4:3, with bars, as it's always been.
        assert_eq!(preview_of(&file, 100).unwrap().size, [1620, 1080]);
    }

    #[test]
    fn camera_details_become_a_tiff_of_their_own() {
        let make = b"Canon\0\0\0\0\0";
        let date = b"2020:01:02 03:04:05\0";
        let exif_at = after_first(2);
        let values_at = exif_at + 2 + 2 * 12 + 4;
        let exif = directory(
            &[
                (0x9003, ASCII, date.len() as u32, values_at),
                (0x927C, UNDEFINED, 100, values_at),
            ],
            0,
        );
        let tail = [exif, date.to_vec(), make.to_vec()].concat();
        let file = tiff(
            42,
            &[
                (
                    0x010F,
                    ASCII,
                    make.len() as u32,
                    values_at + date.len() as u32,
                ),
                (0x8769, LONG, 1, exif_at),
            ],
            &tail,
        );
        let bytes = super::exif(&mut Cursor::new(&file)).unwrap();
        let read = ::exif::Reader::new().read_raw(bytes).unwrap();
        let text = |tag| {
            read.get_field(tag, ::exif::In::PRIMARY)
                .unwrap()
                .display_value()
                .to_string()
        };
        // The padding's gone.
        assert_eq!(text(::exif::Tag::Make), "\"Canon\"");
        assert_eq!(text(::exif::Tag::DateTimeOriginal), "2020-01-02 03:04:05");
        assert!(
            read.get_field(::exif::Tag::MakerNote, ::exif::In::PRIMARY)
                .is_none()
        );
    }

    #[test]
    fn a_directory_pointing_at_itself_ends() {
        let mut file = tiff(42, &[(0x0100, LONG, 1, 10)], &[]);
        let next = after_first(1) as usize - 4;
        file[next..next + 4].copy_from_slice(&8u32.to_le_bytes());
        assert!(preview_of(&file, 256).is_none());
    }

    #[test]
    fn a_file_cut_short_or_changed_is_only_ever_not_found() {
        let picture = jpeg(64, 40);
        let files = [
            two_jpegs(&picture, &jpeg(320, 200)),
            olympus(&picture),
            raf(&picture),
            cr3(),
            tiff(0x55, &[(0x002E, UNDEFINED, 100, u32::MAX)], &[]),
            tiff(42, &[(0x014A, LONG, u32::MAX, u32::MAX)], &[]),
        ];
        for file in &files {
            for len in (0..file.len().min(400)).chain([file.len() - 1]) {
                let part = &file[..len];
                let _ = preview_of(part, 256);
                let _ = super::exif(&mut Cursor::new(part));
                let _ = size(&mut Cursor::new(part));
            }
            for at in 0..file.len().min(400) {
                let mut changed = file.clone();
                changed[at] ^= 0xFF;
                let _ = preview_of(&changed, 256);
                let _ = super::exif(&mut Cursor::new(&changed));
                let _ = size(&mut Cursor::new(&changed));
            }
        }
    }
}
