//! The small picture cameras and phones keep in a JPEG's Exif data, right at
//! the start of the file: from a server, much quicker to get than the JPEG.
//! Files can say anything here, so everything's checked against what's there.

/// What the start of a JPEG says about its thumbnail.
#[derive(Debug, PartialEq, Eq)]
pub struct Embedded<'a> {
    /// The thumbnail, itself a JPEG.
    pub jpeg: &'a [u8],
    /// Exif's orientation of the picture, which holds for the thumbnail too.
    pub orientation: u8,
    /// The size of the whole picture as stored, before any turning.
    pub size: Option<[u32; 2]>,
}

/// Finds the thumbnail in `head`, the start of a JPEG file.
pub fn embedded_thumbnail(head: &[u8]) -> Option<Embedded<'_>> {
    let (exif, frame) = segments(head)?;
    let tiff = Tiff::new(exif?)?;
    let first = tiff.u32_at(4)? as usize;

    let mut orientation = 1;
    let mut exif_pointer = None;
    let after_first = tiff.entries(first, |tag, value| match tag {
        0x0112 => orientation = value as u8,
        0x8769 => exif_pointer = Some(value as usize),
        _ => {}
    })?;
    let second = tiff.u32_at(after_first)? as usize;
    if second == 0 {
        return None;
    }
    let (mut start, mut len, mut compression) = (None, None, None);
    tiff.entries(second, |tag, value| match tag {
        0x0103 => compression = Some(value),
        0x0201 => start = Some(value as usize),
        0x0202 => len = Some(value as usize),
        _ => {}
    })?;
    // 6 is JPEG; anything else is pixels of some kind, if it's said at all.
    if compression.is_some_and(|c| c != 6) {
        return None;
    }
    let start = start?;
    let jpeg = tiff.0.get(start..start.checked_add(len?)?)?;
    if !jpeg.starts_with(&[0xFF, 0xD8]) {
        return None;
    }

    let (mut width, mut height) = (None, None);
    if let Some(pointer) = exif_pointer {
        let _ = tiff.entries(pointer, |tag, value| match tag {
            0xA002 => width = Some(value),
            0xA003 => height = Some(value),
            _ => {}
        });
    }
    let said = width.zip(height).map(|(w, h)| [w, h]);
    let size = frame.or(said).filter(|&[w, h]| w > 0 && h > 0);
    Some(Embedded {
        jpeg,
        orientation,
        size,
    })
}

/// A JPEG's Exif data and picture size, as far as `head` goes.
#[allow(clippy::type_complexity)]
fn segments(head: &[u8]) -> Option<(Option<&[u8]>, Option<[u32; 2]>)> {
    if !head.starts_with(&[0xFF, 0xD8]) {
        return None;
    }
    let (mut exif, mut frame) = (None, None);
    let mut at = 2;
    while let (Some(&0xFF), Some(&marker)) = (head.get(at), head.get(at + 1)) {
        match marker {
            // Padding before a marker.
            0xFF => {
                at += 1;
                continue;
            }
            // Where the picture itself starts, or the end.
            0xDA | 0xD9 => break,
            // On their own, with no length.
            0x01 | 0xD0..=0xD7 => {
                at += 2;
                continue;
            }
            _ => {}
        }
        let Some(len) = head.get(at + 2..at + 4) else {
            break;
        };
        let len = usize::from(u16::from_be_bytes([len[0], len[1]]));
        if len < 2 {
            break;
        }
        let Some(segment) = head.get(at + 4..at + 2 + len) else {
            break;
        };
        match marker {
            0xE1 if exif.is_none() => exif = segment.strip_prefix(b"Exif\0\0"),
            // Frame headers: every 0xCn, but for these, which are others.
            0xC0..=0xCF if !matches!(marker, 0xC4 | 0xC8 | 0xCC) => {
                if let [_, h0, h1, w0, w1, ..] = *segment {
                    frame = Some([
                        u32::from(u16::from_be_bytes([w0, w1])),
                        u32::from(u16::from_be_bytes([h0, h1])),
                    ]);
                }
                break;
            }
            _ => {}
        }
        at += 2 + len;
    }
    Some((exif, frame))
}

/// Exif's data is laid out like a TIFF file: offsets are from its start, in
/// the byte order it begins by saying.
struct Tiff<'a>(&'a [u8], bool);

impl<'a> Tiff<'a> {
    fn new(data: &'a [u8]) -> Option<Self> {
        match data.get(..4)? {
            b"II*\0" => Some(Self(data, true)),
            b"MM\0*" => Some(Self(data, false)),
            _ => None,
        }
    }

    fn u16_at(&self, at: usize) -> Option<u16> {
        let bytes = self.0.get(at..at.checked_add(2)?)?;
        let bytes = [bytes[0], bytes[1]];
        Some(if self.1 {
            u16::from_le_bytes(bytes)
        } else {
            u16::from_be_bytes(bytes)
        })
    }

    fn u32_at(&self, at: usize) -> Option<u32> {
        let bytes = self.0.get(at..at.checked_add(4)?)?;
        let bytes = [bytes[0], bytes[1], bytes[2], bytes[3]];
        Some(if self.1 {
            u32::from_le_bytes(bytes)
        } else {
            u32::from_be_bytes(bytes)
        })
    }

    /// Goes through the numbers in the directory at `at`, returning where
    /// the offset of the next directory is.
    fn entries(&self, at: usize, mut found: impl FnMut(u16, u32)) -> Option<usize> {
        let count = usize::from(self.u16_at(at)?);
        let first = at.checked_add(2)?;
        for index in 0..count {
            let entry = first.checked_add(index.checked_mul(12)?)?;
            let tag = self.u16_at(entry)?;
            // A short or a long, kept in the entry; others aren't wanted.
            let value = match self.u16_at(entry + 2)? {
                3 => u32::from(self.u16_at(entry + 8)?),
                4 => self.u32_at(entry + 8)?,
                _ => continue,
            };
            found(tag, value);
        }
        first.checked_add(count.checked_mul(12)?)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn entry(out: &mut Vec<u8>, tag: u16, kind: u16, value: u32) {
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(&kind.to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes());
        if kind == 3 {
            out.extend_from_slice(&(value as u16).to_le_bytes());
            out.extend_from_slice(&[0, 0]);
        } else {
            out.extend_from_slice(&value.to_le_bytes());
        }
    }

    /// A JPEG's start, with `thumbnail` in its Exif data, then a frame header.
    pub(crate) fn jpeg_start(thumbnail: &[u8], orientation: u16, size: [u32; 2]) -> Vec<u8> {
        // Header, then the first directory (3 entries), the second (3) and
        // the Exif one (2), each with a count and a next offset.
        let first = 8u32;
        let second = first + 2 + 3 * 12 + 4;
        let exif = second + 2 + 3 * 12 + 4;
        let data = exif + 2 + 2 * 12 + 4;

        let mut tiff = b"II*\0".to_vec();
        tiff.extend_from_slice(&first.to_le_bytes());
        tiff.extend_from_slice(&3u16.to_le_bytes());
        entry(&mut tiff, 0x0112, 3, u32::from(orientation));
        entry(&mut tiff, 0x010F, 2, 0);
        entry(&mut tiff, 0x8769, 4, exif);
        tiff.extend_from_slice(&second.to_le_bytes());
        tiff.extend_from_slice(&3u16.to_le_bytes());
        entry(&mut tiff, 0x0103, 3, 6);
        entry(&mut tiff, 0x0201, 4, data);
        entry(&mut tiff, 0x0202, 4, thumbnail.len() as u32);
        tiff.extend_from_slice(&0u32.to_le_bytes());
        tiff.extend_from_slice(&2u16.to_le_bytes());
        entry(&mut tiff, 0xA002, 4, size[0]);
        entry(&mut tiff, 0xA003, 4, size[1]);
        tiff.extend_from_slice(&0u32.to_le_bytes());
        tiff.extend_from_slice(thumbnail);

        let mut out = vec![0xFF, 0xD8];
        // Something first, as there often is.
        out.extend_from_slice(&[0xFF, 0xE0, 0, 4, 0, 0]);
        let len = (tiff.len() + 8) as u16;
        out.extend_from_slice(&[0xFF, 0xE1]);
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(b"Exif\0\0");
        out.extend_from_slice(&tiff);
        out
    }

    #[test]
    fn finds_the_thumbnail_and_what_goes_with_it() {
        let thumbnail = [0xFF, 0xD8, 1, 2, 3];
        let head = jpeg_start(&thumbnail, 6, [4000, 3000]);
        assert_eq!(
            embedded_thumbnail(&head),
            Some(Embedded {
                jpeg: &thumbnail,
                orientation: 6,
                size: Some([4000, 3000]),
            })
        );
    }

    #[test]
    fn the_frame_header_has_the_last_word_on_size() {
        let mut head = jpeg_start(&[0xFF, 0xD8], 1, [4000, 3000]);
        head.extend_from_slice(&[0xFF, 0xC0, 0, 8, 8, 0, 100, 0, 200, 3]);
        assert_eq!(
            embedded_thumbnail(&head).unwrap().size,
            Some([200, 100]),
            "height comes first there"
        );
    }

    #[test]
    fn what_isnt_a_thumbnail_is_none() {
        assert_eq!(embedded_thumbnail(b""), None);
        assert_eq!(embedded_thumbnail(b"\x89PNG\r\n"), None);
        // A JPEG without Exif data.
        assert_eq!(
            embedded_thumbnail(&[0xFF, 0xD8, 0xFF, 0xDB, 0, 2, 0xFF, 0xDA]),
            None
        );
        // "Thumbnail" data that isn't a JPEG.
        assert_eq!(
            embedded_thumbnail(&jpeg_start(&[1, 2, 3], 1, [10, 10])),
            None
        );
    }

    #[test]
    fn cut_short_or_scrambled_it_never_reads_out_of_bounds() {
        let thumbnail = [0xFF, 0xD8, 1, 2, 3, 4, 5, 6];
        let head = jpeg_start(&thumbnail, 3, [640, 480]);
        for len in 0..head.len() {
            // All of the thumbnail is needed for it to be found.
            assert_eq!(embedded_thumbnail(&head[..len]), None, "cut at {len}");
        }
        for at in 0..head.len() {
            for value in [0x00, 0x01, 0x7F, 0x80, 0xFF] {
                let mut scrambled = head.clone();
                scrambled[at] = value;
                let _ = embedded_thumbnail(&scrambled);
            }
        }
    }
}
