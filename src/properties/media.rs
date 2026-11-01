//! Image size, EXIF tags and audio/video streams, read in the background.

use std::{
    fs::File,
    io::BufReader,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
};

use crate::{images, video};

#[derive(Clone, Debug, Default)]
pub struct Media {
    pub dimensions: Option<[u32; 2]>,
    /// The tags most people look for, by name.
    pub exif: Vec<(String, String)>,
    /// Every other tag.
    pub exif_rest: Vec<(String, String)>,
    pub av: Option<video::MediaInfo>,
    pub error: Option<String>,
}

/// Whether there's anything to read for `path`.
pub fn applies(path: &Path) -> bool {
    images::is_image(path) || video::is_video(path) || video::is_audio(path)
}

pub struct Probe {
    pub path: PathBuf,
    cell: Arc<OnceLock<Media>>,
}

impl Probe {
    pub fn start(ctx: &egui::Context, path: PathBuf) -> Self {
        let cell = Arc::new(OnceLock::new());
        let (c, ctx, p) = (cell.clone(), ctx.clone(), path.clone());
        std::thread::Builder::new()
            .name("properties-media".into())
            .spawn(move || {
                let _ = c.set(read(&p));
                ctx.request_repaint();
            })
            .expect("spawn thread");
        Self { path, cell }
    }

    pub fn get(&self) -> Option<&Media> {
        self.cell.get()
    }
}

/// EXIF tags shown first, in this order.
const MAIN_TAGS: &[exif::Tag] = &[
    exif::Tag::Make,
    exif::Tag::Model,
    exif::Tag::LensModel,
    exif::Tag::DateTimeOriginal,
    exif::Tag::ExposureTime,
    exif::Tag::FNumber,
    exif::Tag::PhotographicSensitivity,
    exif::Tag::FocalLength,
    exif::Tag::Flash,
    exif::Tag::Orientation,
    exif::Tag::Software,
];

/// Tags that are binary blobs, not worth showing.
const SKIPPED_TAGS: &[exif::Tag] = &[
    exif::Tag::MakerNote,
    exif::Tag::UserComment,
    exif::Tag::JPEGInterchangeFormat,
    exif::Tag::JPEGInterchangeFormatLength,
    exif::Tag::ExifVersion,
    exif::Tag::FlashpixVersion,
    exif::Tag::ComponentsConfiguration,
    exif::Tag::InteroperabilityIndex,
    exif::Tag::InteroperabilityVersion,
];

fn read(path: &Path) -> Media {
    let mut media = Media::default();
    if images::is_image(path) {
        match image::image_dimensions(path) {
            Ok((w, h)) => media.dimensions = Some([w, h]),
            Err(e) => media.error = Some(e.to_string()),
        }
        read_exif(path, &mut media);
    } else if video::is_video(path) || video::is_audio(path) {
        match video::probe(path) {
            Ok(info) => media.av = Some(info),
            Err(e) => media.error = Some(e),
        }
    }
    media
}

fn read_exif(path: &Path, media: &mut Media) {
    let Ok(file) = File::open(path) else { return };
    let Ok(exif) = exif::Reader::new().read_from_container(&mut BufReader::new(file)) else {
        return;
    };
    let value = |field: &exif::Field| {
        field
            .display_value()
            .with_unit(&exif)
            .to_string()
            .trim_matches('"')
            .to_owned()
    };
    for tag in MAIN_TAGS {
        if let Some(field) = exif.get_field(*tag, exif::In::PRIMARY) {
            media.exif.push((tag.to_string(), value(field)));
        }
    }
    if let Some(location) = gps(&exif) {
        media.exif.push(("Location".to_owned(), location));
    }
    for field in exif.fields() {
        if field.ifd_num == exif::In::PRIMARY
            && !MAIN_TAGS.contains(&field.tag)
            && !SKIPPED_TAGS.contains(&field.tag)
        {
            media.exif_rest.push((field.tag.to_string(), value(field)));
        }
    }
}

/// Latitude and longitude in decimal degrees.
fn gps(exif: &exif::Exif) -> Option<String> {
    let degrees = |tag, reference| -> Option<f64> {
        let field = exif.get_field(tag, exif::In::PRIMARY)?;
        let exif::Value::Rational(parts) = &field.value else {
            return None;
        };
        let [d, m, s] = parts.get(0..3)? else {
            return None;
        };
        let value = d.to_f64() + m.to_f64() / 60.0 + s.to_f64() / 3600.0;
        let negative = exif
            .get_field(reference, exif::In::PRIMARY)
            .map(|f| f.display_value().to_string())
            .is_some_and(|r| r.contains('S') || r.contains('W'));
        Some(if negative { -value } else { value })
    };
    let lat = degrees(exif::Tag::GPSLatitude, exif::Tag::GPSLatitudeRef)?;
    let lon = degrees(exif::Tag::GPSLongitude, exif::Tag::GPSLongitudeRef)?;
    Some(format!("{lat:.6}, {lon:.6}"))
}
