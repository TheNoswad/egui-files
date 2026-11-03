//! The web demo's files: a home folder, made in memory as the app starts.
//!
//! The pictures are drawn here and the archives packed here, so the demo
//! needs no files of its own; the project is some of the app's own source.
//! Nothing is kept: a reload starts over.

use std::{
    io::{self, Write},
    path::Path,
    time::{Duration, SystemTime},
};

use image::{ImageFormat, Rgb, RgbImage};

pub const HOME: &str = "/home/demo";

const README: &str = "\
# Welcome to the egui-files demo

This is egui-files, a tabbed file browser, running in your browser. Its
files are made up and kept in memory: change anything you like. A reload
puts everything back.

Things to try:

- Open a few tabs (Ctrl+T), and drag one beside another to split the view.
- Switch between the list, grid and columns views; the grid shows thumbnails.
- Preview a picture or a source file with F3.
- Look inside the archives in Downloads, and extract one.
- Copy, rename, trash and restore files.
- Pick another theme under View → Theme.

What needs the desktop app: the terminal, playing videos, opening files in
other apps, and RAR and zstd archives.
";

const NOTES: &str = "\
# Notes

## Groceries

- [x] coffee
- [ ] oranges
- [ ] bread

## Ideas

1. A bookshelf for the hallway
2. Learn the names of the stars in Orion
3. *Finally* fix the squeaky door

> The best time to plant a tree was twenty years ago. The second best time is now.
";

const LETTER: &str = "\
Dear Sam,

Thanks for the postcard from the coast. The lighthouse looks just as you
described it, and I can almost hear the gulls.

The garden is doing well. The tomatoes came in early this year, and the
neighbours' cat has taken to sleeping under the bean poles.

Write again soon,
Alex
";

const BUDGET: &str = "\
Month,Rent,Food,Transport,Fun,Saved
January,950,310,85,120,400
February,950,295,85,90,450
March,950,330,110,140,350
April,950,300,85,160,380
May,950,325,95,200,300
June,950,290,85,180,420
";

const TODO: &str = "\
- Water the plants
- Call the dentist
- Return the library books (due Friday)
- Back up the photos
";

const PROFILE: &str = "\
# Hidden files start with a dot; View → Show hidden files shows them.
export EDITOR=nano
";

/// Some of the app's own source, as a project to look through.
const PROJECT: &[(&str, &str)] = &[
    ("Cargo.toml", include_str!("../Cargo.toml")),
    ("LICENSE-MIT", include_str!("../LICENSE-MIT")),
    ("src/main.rs", include_str!("main.rs")),
    ("src/background.rs", include_str!("background.rs")),
    ("src/watcher.rs", include_str!("watcher.rs")),
    ("src/demo.rs", include_str!("demo.rs")),
    ("disk/Cargo.toml", include_str!("../disk/Cargo.toml")),
    ("disk/src/lib.rs", include_str!("../disk/src/lib.rs")),
    ("disk/src/memory.rs", include_str!("../disk/src/memory.rs")),
    (
        "web/demo/index.html",
        include_str!("../web/demo/index.html"),
    ),
];

/// Makes the demo's home folder. The pictures and archives take a moment,
/// so they're made after the first frame.
pub fn create() {
    if let Err(err) = create_files() {
        log::error!("demo files: {err}");
    }
    crate::background::spawn("demo pictures", || {
        if let Err(err) = create_pictures().and_then(|()| create_archives()) {
            log::error!("demo pictures: {err}");
        }
    });
}

fn create_files() -> io::Result<()> {
    let home = Path::new(HOME);
    for dir in ["Documents", "Downloads", "Pictures", "Projects", ".cache"] {
        disk::create_dir_all(home.join(dir))?;
    }
    file(&home.join("README.md"), README, 0.0)?;
    file(&home.join(".profile"), PROFILE, 40.0)?;

    let docs = home.join("Documents");
    file(&docs.join("Notes.md"), NOTES, 1.5)?;
    file(&docs.join("Letter to Sam.txt"), LETTER, 6.0)?;
    file(&docs.join("Budget.csv"), BUDGET, 12.0)?;
    file(&docs.join("To do.txt"), TODO, 0.2)?;

    let project = home.join("Projects/egui-files");
    for (name, text) in PROJECT {
        let path = project.join(name);
        disk::create_dir_all(path.parent().unwrap_or(&project))?;
        file(&path, text, 3.0)?;
    }
    file(&project.join(".gitignore"), "/target\n", 30.0)?;
    disk::symlink("egui-files", home.join("Projects/current"))?;
    Ok(())
}

/// Writes `text` to `path`, last changed `days` ago.
fn file(path: &Path, text: &str, days: f64) -> io::Result<()> {
    disk::write(path, text)?;
    disk::set_file_mtime(path, ago(days))
}

fn ago(days: f64) -> SystemTime {
    disk::now() - Duration::from_secs_f64(days * 86_400.0)
}

// ---------------------------------------------------------------------------
// Pictures

fn create_pictures() -> io::Result<()> {
    let dir = Path::new(HOME).join("Pictures");
    let pictures: [(&str, RgbImage, ImageFormat, f64); 4] = [
        (
            "Mandelbrot.png",
            mandelbrot(720, 480),
            ImageFormat::Png,
            2.0,
        ),
        ("Sunset.jpg", sunset(800, 500), ImageFormat::Jpeg, 9.0),
        ("Ripples.png", ripples(600, 600), ImageFormat::Png, 15.0),
        ("Plaid.png", plaid(512, 512), ImageFormat::Png, 21.0),
    ];
    for (name, image, format, days) in pictures {
        let mut bytes = io::Cursor::new(Vec::new());
        image
            .write_to(&mut bytes, format)
            .map_err(io::Error::other)?;
        let path = dir.join(name);
        disk::write(&path, bytes.into_inner())?;
        disk::set_file_mtime(&path, ago(days))?;
    }
    Ok(())
}

/// A color around the hue circle, `t` from 0 to 1.
fn rainbow(t: f32) -> Rgb<u8> {
    let channel = |offset: f32| {
        let v = 0.5 + 0.5 * (std::f32::consts::TAU * (t + offset)).cos();
        (v * 255.0) as u8
    };
    Rgb([channel(0.0), channel(0.33), channel(0.67)])
}

fn mandelbrot(width: u32, height: u32) -> RgbImage {
    const MAX: u32 = 96;
    RgbImage::from_fn(width, height, |x, y| {
        let cx = -2.3 + 3.2 * x as f32 / width as f32;
        let cy = -1.0 + 2.0 * y as f32 / height as f32;
        let (mut zx, mut zy) = (0.0f32, 0.0f32);
        let mut i = 0;
        while i < MAX && zx * zx + zy * zy < 16.0 {
            (zx, zy) = (zx * zx - zy * zy + cx, 2.0 * zx * zy + cy);
            i += 1;
        }
        if i == MAX {
            return Rgb([8, 6, 24]);
        }
        // Smooth bands, not steps, fading to the night far from the set.
        let smooth = i as f32 + 1.0 - (zx * zx + zy * zy).ln().ln() / std::f32::consts::LN_2;
        let Rgb(color) = rainbow(0.55 + smooth / 24.0);
        let light = (smooth / 18.0).clamp(0.0, 1.0);
        Rgb(color.map(|c| (8.0 + (c as f32 - 8.0) * light) as u8))
    })
}

fn sunset(width: u32, height: u32) -> RgbImage {
    let horizon = height as f32 * 0.68;
    let (sun_x, sun_y, sun_r) = (width as f32 * 0.6, horizon - 20.0, height as f32 * 0.11);
    let mix = |a: [f32; 3], b: [f32; 3], t: f32| {
        Rgb([0, 1, 2].map(|i| (a[i] + (b[i] - a[i]) * t.clamp(0.0, 1.0)) as u8))
    };
    RgbImage::from_fn(width, height, |x, y| {
        let (fx, fy) = (x as f32, y as f32);
        let far = horizon - 30.0 * (fx / 90.0).sin() - 18.0 * (fx / 37.0 + 1.0).sin();
        let near = horizon + 40.0 + 25.0 * (fx / 140.0 + 2.0).sin();
        if fy > near {
            return mix([40.0, 20.0, 50.0], [15.0, 8.0, 25.0], (fy - near) / 80.0);
        }
        if fy > far {
            return mix([90.0, 40.0, 80.0], [50.0, 25.0, 60.0], (fy - far) / 60.0);
        }
        let d = ((fx - sun_x).powi(2) + (fy - sun_y).powi(2)).sqrt();
        if d < sun_r {
            return Rgb([255, 236, 170]);
        }
        let glow = (1.0 - (d - sun_r) / (height as f32 * 0.5)).max(0.0).powi(2);
        let sky = mix([30.0, 40.0, 110.0], [250.0, 140.0, 80.0], fy / horizon);
        Rgb(sky.0.map(|c| (c as f32 + glow * 90.0).min(255.0) as u8))
    })
}

fn ripples(width: u32, height: u32) -> RgbImage {
    let sources = [(0.3, 0.35), (0.72, 0.6), (0.45, 0.8)];
    RgbImage::from_fn(width, height, |x, y| {
        let (u, v) = (x as f32 / width as f32, y as f32 / height as f32);
        let wave: f32 = sources
            .iter()
            .map(|(sx, sy)| (((u - sx).powi(2) + (v - sy).powi(2)).sqrt() * 60.0).sin())
            .sum();
        rainbow(0.5 + wave / 12.0 + u * 0.2)
    })
}

fn plaid(width: u32, height: u32) -> RgbImage {
    RgbImage::from_fn(width, height, |x, y| {
        let band = |n: u32| match n % 64 {
            0..20 => 1.0,
            20..26 | 40..44 => 0.35,
            _ => 0.6,
        };
        let (a, b) = (band(x), band(y + 10));
        let thread = if (x + y) % 4 < 2 { a } else { b };
        Rgb([
            (180.0 * thread) as u8,
            (40.0 * a * b + 20.0) as u8,
            (60.0 + 90.0 * (1.0 - thread)) as u8,
        ])
    })
}

// ---------------------------------------------------------------------------
// Archives

/// Documents and the project, packed, to look inside and extract.
fn create_archives() -> io::Result<()> {
    let home = Path::new(HOME);
    let docs: Vec<(String, Vec<u8>)> = ["Notes.md", "Letter to Sam.txt", "Budget.csv"]
        .iter()
        .map(|name| {
            let data = disk::read(home.join("Documents").join(name))?;
            Ok((format!("Documents/{name}"), data))
        })
        .collect::<io::Result<_>>()?;
    let downloads = home.join("Downloads");
    write_zip(&downloads.join("Documents backup.zip"), &docs)?;
    disk::set_file_mtime(downloads.join("Documents backup.zip"), ago(4.0))?;

    let project: Vec<(String, Vec<u8>)> = PROJECT
        .iter()
        .map(|(name, text)| (format!("egui-files/{name}"), text.as_bytes().to_vec()))
        .collect();
    write_tar_gz(&downloads.join("egui-files-source.tar.gz"), &project)?;
    disk::set_file_mtime(downloads.join("egui-files-source.tar.gz"), ago(7.0))?;

    let pictures: Vec<(String, Vec<u8>)> = ["Mandelbrot.png", "Ripples.png"]
        .iter()
        .map(|name| {
            Ok((
                name.to_string(),
                disk::read(home.join("Pictures").join(name))?,
            ))
        })
        .collect::<io::Result<_>>()?;
    write_zip(&downloads.join("Pictures.zip"), &pictures)?;
    disk::set_file_mtime(downloads.join("Pictures.zip"), ago(1.0))
}

fn write_zip(path: &Path, files: &[(String, Vec<u8>)]) -> io::Result<()> {
    // A fixed time: zip would ask std's clock, which the browser lacks.
    let time =
        zip::DateTime::from_date_and_time(2026, 10, 1, 9, 30, 0).map_err(io::Error::other)?;
    let options = zip::write::SimpleFileOptions::default().last_modified_time(time);
    let mut zip = zip::ZipWriter::new(disk::File::create(path)?);
    for (name, data) in files {
        zip.start_file(name.as_str(), options)
            .map_err(io::Error::other)?;
        zip.write_all(data)?;
    }
    zip.finish().map_err(io::Error::other)?;
    Ok(())
}

fn write_tar_gz(path: &Path, files: &[(String, Vec<u8>)]) -> io::Result<()> {
    let gz =
        flate2::write::GzEncoder::new(disk::File::create(path)?, flate2::Compression::default());
    let mut tar = tar::Builder::new(gz);
    let mtime = ago(7.0)
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    for (name, data) in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(mtime);
        tar.append_data(&mut header, name, data.as_slice())?;
    }
    tar.into_inner()?.finish()?;
    Ok(())
}
