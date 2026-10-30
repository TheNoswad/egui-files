//! Thumbnails from the shared freedesktop.org thumbnail cache
//! (`~/.cache/thumbnails`), so files other apps already thumbnailed load
//! instantly, and video thumbnails made with ffmpegthumbnailer or ffmpeg.

use std::{
    io::Cursor,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::SystemTime,
};

use image::RgbaImage;
use md5::{Digest, Md5};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC};

/// Cache directories by thumbnail size, smallest first.
const SIZES: &[(&str, u32)] = &[
    ("normal", 128),
    ("large", 256),
    ("x-large", 512),
    ("xx-large", 1024),
];

/// RFC 3986 path characters, as Qt/KDE encode file URIs.
const URI_PATH: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~')
    .remove(b'/')
    .remove(b'!')
    .remove(b'$')
    .remove(b'&')
    .remove(b'\'')
    .remove(b'(')
    .remove(b')')
    .remove(b'*')
    .remove(b'+')
    .remove(b',')
    .remove(b';')
    .remove(b'=')
    .remove(b':')
    .remove(b'@');
/// GLib additionally escapes these.
const URI_PATH_GLIB: &AsciiSet = &URI_PATH.add(b';').add(b'~');

pub struct Thumbnail {
    pub image: RgbaImage,
    /// Size of the original, if known.
    pub original_size: Option<[u32; 2]>,
}

pub fn cache_dir() -> Option<PathBuf> {
    // Tests mustn't read or fill the user's real cache.
    if cfg!(windows) || cfg!(test) {
        return None;
    }
    Some(directories::BaseDirs::new()?.cache_dir().join("thumbnails"))
}

fn mtime_secs(modified: SystemTime) -> Option<u64> {
    Some(
        modified
            .duration_since(SystemTime::UNIX_EPOCH)
            .ok()?
            .as_secs(),
    )
}

/// The file URIs other apps may have hashed for `path`, preferred first.
fn uris(path: &Path) -> Vec<String> {
    #[cfg(unix)]
    let bytes = std::os::unix::ffi::OsStrExt::as_bytes(path.as_os_str());
    #[cfg(not(unix))]
    let bytes = path.to_string_lossy();
    #[cfg(not(unix))]
    let bytes = bytes.as_bytes();
    let mut uris: Vec<String> = [URI_PATH, URI_PATH_GLIB]
        .iter()
        .map(|set| format!("file://{}", percent_encoding::percent_encode(bytes, set)))
        .collect();
    uris.dedup();
    uris
}

fn cache_name(uri: &str) -> String {
    format!("{:x}.png", Md5::digest(uri.as_bytes()))
}

/// Looks up a valid cached thumbnail of at least `max_side`, downscaled to fit.
pub fn from_cache(path: &Path, modified: Option<SystemTime>, max_side: u32) -> Option<Thumbnail> {
    let dir = cache_dir()?;
    if !path.is_absolute() || path.starts_with(&dir) {
        return None;
    }
    let mtime = mtime_secs(modified?)?;
    let names: Vec<String> = uris(path).iter().map(|uri| cache_name(uri)).collect();
    SIZES
        .iter()
        .filter(|(_, side)| *side >= max_side)
        .flat_map(|(size, _)| {
            let dir = dir.join(size);
            names.iter().map(move |name| dir.join(name))
        })
        .find_map(|file| read_cached(&file, mtime, max_side))
}

fn read_cached(file: &Path, mtime: u64, max_side: u32) -> Option<Thumbnail> {
    let bytes = std::fs::read(file).ok()?;
    let reader = png::Decoder::new(Cursor::new(&bytes)).read_info().ok()?;
    let text = |key: &str| {
        reader
            .info()
            .uncompressed_latin1_text
            .iter()
            .find(|chunk| chunk.keyword == key)
            .and_then(|chunk| chunk.text.parse::<u64>().ok())
    };
    // Stale if the file changed since; the spec requires this check.
    if text("Thumb::MTime")? != mtime {
        return None;
    }
    let original_size = text("Thumb::Image::Width")
        .zip(text("Thumb::Image::Height"))
        .map(|(w, h)| [w as u32, h as u32]);
    let image = image::load_from_memory_with_format(&bytes, image::ImageFormat::Png).ok()?;
    let image = if image.width() > max_side || image.height() > max_side {
        image.thumbnail(max_side, max_side)
    } else {
        image
    };
    Some(Thumbnail {
        image: image.to_rgba8(),
        original_size,
    })
}

/// Grabs a representative frame of a video and saves it to the shared cache.
pub fn generate_video(
    path: &Path,
    modified: Option<SystemTime>,
    max_side: u32,
) -> Result<Thumbnail, String> {
    let png = run_thumbnailer(path, max_side)?;
    let image = image::load_from_memory_with_format(&png, image::ImageFormat::Png)
        .map_err(|e| e.to_string())?
        .to_rgba8();
    if let Some(mtime) = modified.and_then(mtime_secs) {
        // Best effort: it's only a cache.
        let _ = save_to_cache(path, mtime, &image);
    }
    Ok(Thumbnail {
        image,
        original_size: None,
    })
}

fn run_thumbnailer(path: &Path, max_side: u32) -> Result<Vec<u8>, String> {
    let side = max_side.to_string();
    let mut ffmpegthumbnailer = Command::new("ffmpegthumbnailer");
    ffmpegthumbnailer
        .arg("-i")
        .arg(path)
        .args(["-o", "-", "-c", "png", "-s", &side]);
    let mut ffmpeg = Command::new("ffmpeg");
    ffmpeg
        .args(["-v", "error", "-nostdin", "-i"])
        .arg(path)
        .args([
            "-vf",
            &format!("thumbnail,scale={side}:{side}:force_original_aspect_ratio=decrease"),
            "-frames:v",
            "1",
            "-f",
            "image2pipe",
            "-c:v",
            "png",
            "-",
        ]);
    for command in [&mut ffmpegthumbnailer, &mut ffmpeg] {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        match command.output() {
            Ok(output) if output.status.success() && !output.stdout.is_empty() => {
                return Ok(output.stdout);
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let message = stderr.lines().last().unwrap_or("no frame").trim();
                return Err(format!("thumbnailer failed: {message}"));
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err.to_string()),
        }
    }
    Err("install ffmpegthumbnailer or ffmpeg for video thumbnails".into())
}

fn save_to_cache(path: &Path, mtime: u64, image: &RgbaImage) -> Option<()> {
    let longest = image.width().max(image.height());
    let (name, _) = SIZES.iter().find(|(_, side)| *side >= longest)?;
    let dir = cache_dir()?.join(name);
    let uri = uris(path).into_iter().next()?;
    create_private_dir(&dir)?;

    let mut bytes = Vec::new();
    let mut encoder = png::Encoder::new(&mut bytes, image.width(), image.height());
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .add_text_chunk("Thumb::URI".into(), uri.clone())
        .ok()?;
    encoder
        .add_text_chunk("Thumb::MTime".into(), mtime.to_string())
        .ok()?;
    encoder
        .add_text_chunk("Software".into(), "egui-files".into())
        .ok()?;
    encoder
        .write_header()
        .ok()?
        .write_image_data(image.as_raw())
        .ok()?;

    // Write then rename, so other apps never read a partial file.
    let file = dir.join(cache_name(&uri));
    let temp = dir.join(format!(
        ".{}.egui-files-{}",
        cache_name(&uri),
        std::process::id()
    ));
    write_private(&temp, &bytes)?;
    std::fs::rename(&temp, &file)
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&temp);
        })
        .ok()
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> Option<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .ok()
}

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> Option<()> {
    std::fs::create_dir_all(dir).ok()
}

/// Thumbnails reveal file contents, so the spec asks for owner-only access.
#[cfg(unix)]
fn write_private(path: &Path, bytes: &[u8]) -> Option<()> {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .ok()?
        .write_all(bytes)
        .ok()
}

#[cfg(not(unix))]
fn write_private(path: &Path, bytes: &[u8]) -> Option<()> {
    std::fs::write(path, bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uris_follow_the_thumbnail_spec() {
        // From the freedesktop.org thumbnail spec's example.
        assert_eq!(
            cache_name("file:///home/jens/photos/me.png"),
            "c6ee772d9e49320e97ec29a7eb5b1697.png"
        );
        assert_eq!(
            uris(Path::new("/home/a b/ä;~.mp4")),
            [
                "file:///home/a%20b/%C3%A4;~.mp4",
                "file:///home/a%20b/%C3%A4%3B%7E.mp4"
            ]
        );
        assert_eq!(uris(Path::new("/x/y.mp4")).len(), 1);
    }

    #[test]
    fn reads_back_saved_thumbnails_and_rejects_stale_ones() {
        let dir = std::env::temp_dir().join(format!("egui-files-thumbs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.png");
        let mut bytes = Vec::new();
        let mut encoder = png::Encoder::new(&mut bytes, 300, 150);
        encoder.set_color(png::ColorType::Rgba);
        encoder
            .add_text_chunk("Thumb::MTime".into(), "42".into())
            .unwrap();
        encoder
            .add_text_chunk("Thumb::Image::Width".into(), "4000".into())
            .unwrap();
        encoder
            .add_text_chunk("Thumb::Image::Height".into(), "2000".into())
            .unwrap();
        encoder
            .write_header()
            .unwrap()
            .write_image_data(&vec![255; 300 * 150 * 4])
            .unwrap();
        std::fs::write(&file, bytes).unwrap();

        let thumbnail = read_cached(&file, 42, 256).unwrap();
        assert_eq!(thumbnail.image.dimensions(), (256, 128));
        assert_eq!(thumbnail.original_size, Some([4000, 2000]));
        assert!(read_cached(&file, 43, 256).is_none());

        std::fs::remove_dir_all(dir).unwrap();
    }
}
