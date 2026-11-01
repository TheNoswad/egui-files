//! Image size, camera details and audio/video streams, read in the
//! background.

use std::{
    fs::File,
    io::BufReader,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
};

use chrono::{Local, NaiveDateTime, TimeZone};
use exif::{Exif, In, Tag, Value};

use crate::{images, video};

#[derive(Clone, Debug, Default)]
pub struct Media {
    pub dimensions: Option<[u32; 2]>,
    /// Camera details in words: "Camera", "Exposure"…
    pub camera: Vec<(&'static str, String)>,
    /// Where the photo was taken, in decimal degrees.
    pub location: Option<(f64, f64)>,
    /// Every other EXIF tag, named in words.
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

/// Tags shown in words above, or binary blobs not worth showing.
const SKIPPED_TAGS: &[Tag] = &[
    Tag::Make,
    Tag::Model,
    Tag::LensModel,
    Tag::LensMake,
    Tag::DateTimeOriginal,
    Tag::ExposureTime,
    Tag::FNumber,
    Tag::PhotographicSensitivity,
    Tag::FocalLength,
    Tag::FocalLengthIn35mmFilm,
    Tag::Flash,
    Tag::Orientation,
    Tag::Software,
    Tag::PixelXDimension,
    Tag::PixelYDimension,
    Tag::ImageWidth,
    Tag::ImageLength,
    Tag::GPSLatitude,
    Tag::GPSLatitudeRef,
    Tag::GPSLongitude,
    Tag::GPSLongitudeRef,
    Tag::MakerNote,
    Tag::UserComment,
    Tag::JPEGInterchangeFormat,
    Tag::JPEGInterchangeFormatLength,
    Tag::ExifVersion,
    Tag::FlashpixVersion,
    Tag::ComponentsConfiguration,
    Tag::InteroperabilityIndex,
    Tag::InteroperabilityVersion,
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
    media.camera = camera(&exif);
    media.location = gps(&exif);
    for field in exif.fields() {
        if field.ifd_num == In::PRIMARY && !SKIPPED_TAGS.contains(&field.tag) {
            let value = field.display_value().with_unit(&exif).to_string();
            media.exif_rest.push((
                words(&field.tag.to_string()),
                value.trim_matches('"').to_owned(),
            ));
        }
    }
}

fn text(exif: &Exif, tag: Tag) -> Option<String> {
    let field = exif.get_field(tag, In::PRIMARY)?;
    let text = field.display_value().to_string();
    let text = text.trim_matches('"').trim();
    (!text.is_empty()).then(|| text.to_owned())
}

fn number(exif: &Exif, tag: Tag) -> Option<f64> {
    match &exif.get_field(tag, In::PRIMARY)?.value {
        Value::Rational(v) => v.first().map(|r| r.to_f64()),
        Value::SRational(v) => v.first().map(|r| r.to_f64()),
        value => value.get_uint(0).map(f64::from),
    }
}

/// "1.89" not "1.8900000".
fn short(n: f64) -> String {
    let text = format!("{n:.2}");
    text.trim_end_matches('0').trim_end_matches('.').to_owned()
}

/// The details people look for, in words.
fn camera(exif: &Exif) -> Vec<(&'static str, String)> {
    let mut rows = Vec::new();
    let make = text(exif, Tag::Make);
    let model = text(exif, Tag::Model);
    let camera = match (make, model) {
        // Models often repeat the make: "Canon" + "Canon EOS R6".
        (Some(make), Some(model)) if model.to_lowercase().starts_with(&make.to_lowercase()) => {
            Some(model)
        }
        (Some(make), Some(model)) => Some(format!("{make} {model}")),
        (make, model) => make.or(model),
    };
    if let Some(camera) = camera {
        rows.push(("Camera", camera));
    }
    if let Some(lens) = text(exif, Tag::LensModel) {
        rows.push(("Lens", lens));
    }
    if let Some(taken) = text(exif, Tag::DateTimeOriginal) {
        let local = NaiveDateTime::parse_from_str(&taken, "%Y-%m-%d %H:%M:%S")
            .ok()
            .and_then(|t| Local.from_local_datetime(&t).earliest())
            .map(|t| crate::dates::format_exact(t, crate::dates::clock_format()));
        rows.push(("Taken", local.unwrap_or(taken)));
    }
    let mut exposure = Vec::new();
    if let Some(t) = number(exif, Tag::ExposureTime).filter(|t| *t > 0.0) {
        exposure.push(if t >= 1.0 {
            format!("{} s", short(t))
        } else {
            format!("1/{} s", (1.0 / t).round())
        });
    }
    if let Some(f) = number(exif, Tag::FNumber) {
        exposure.push(format!("f/{}", short(f)));
    }
    if let Some(iso) = number(exif, Tag::PhotographicSensitivity) {
        exposure.push(format!("ISO {iso}"));
    }
    if !exposure.is_empty() {
        rows.push(("Exposure", exposure.join(" · ")));
    }
    if let Some(focal) = number(exif, Tag::FocalLength) {
        let mut text = format!("{} mm", short(focal));
        if let Some(equiv) = number(exif, Tag::FocalLengthIn35mmFilm).filter(|e| *e > 0.0) {
            text.push_str(&format!(" ({equiv} mm full-frame)"));
        }
        rows.push(("Focal length", text));
    }
    if let Some(flash) = number(exif, Tag::Flash) {
        let fired = flash as u32 & 1 != 0;
        rows.push(("Flash", if fired { "Fired" } else { "Off" }.to_owned()));
    }
    if let Some(orientation) = number(exif, Tag::Orientation) {
        let text = match orientation as u32 {
            1 => None,
            3 => Some("Rotated 180°"),
            6 => Some("Rotated 90° clockwise"),
            8 => Some("Rotated 90° counterclockwise"),
            2 | 4 | 5 | 7 => Some("Mirrored"),
            _ => None,
        };
        if let Some(text) = text {
            rows.push(("Orientation", text.to_owned()));
        }
    }
    if let Some(software) = text(exif, Tag::Software) {
        rows.push(("Software", software));
    }
    rows
}

/// "DateTimeOriginal" as "Date time original".
fn words(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    let chars: Vec<char> = name.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        let next_lower = chars.get(i + 1).is_some_and(|n| n.is_lowercase());
        let prev_lower = i > 0 && chars[i - 1].is_lowercase();
        if i > 0 && c.is_uppercase() && (prev_lower || next_lower) {
            out.push(' ');
        }
        if i > 0 && c.is_uppercase() && next_lower {
            out.extend(c.to_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// Latitude and longitude in decimal degrees.
fn gps(exif: &Exif) -> Option<(f64, f64)> {
    let degrees = |tag, reference| -> Option<f64> {
        let field = exif.get_field(tag, In::PRIMARY)?;
        let Value::Rational(parts) = &field.value else {
            return None;
        };
        let [d, m, s] = parts.get(0..3)? else {
            return None;
        };
        let value = d.to_f64() + m.to_f64() / 60.0 + s.to_f64() / 3600.0;
        let negative = text(exif, reference).is_some_and(|r| r.contains('S') || r.contains('W'));
        Some(if negative { -value } else { value })
    };
    let lat = degrees(Tag::GPSLatitude, Tag::GPSLatitudeRef)?;
    let lon = degrees(Tag::GPSLongitude, Tag::GPSLongitudeRef)?;
    Some((lat, lon))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_names_in_words() {
        assert_eq!(words("DateTimeOriginal"), "Date time original");
        assert_eq!(words("ISOSpeed"), "ISO speed");
        assert_eq!(words("Make"), "Make");
    }

    #[test]
    fn short_numbers() {
        assert_eq!(short(1.89), "1.89");
        assert_eq!(short(2.0), "2");
        assert_eq!(short(5.430001), "5.43");
    }
}
