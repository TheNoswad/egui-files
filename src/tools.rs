//! ffmpeg and libmpv: other people's programs, which the app runs but doesn't
//! ship. They're found where the user said, then where the app downloaded them
//! if asked to, then on the system.

// The browser has nothing to download these for.
#![cfg_attr(target_arch = "wasm32", allow(dead_code))]

use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use egui::{Button, Id, Modal, Ui};
use egui_phosphor::regular as icon;

#[cfg(not(target_arch = "wasm32"))]
use crate::jobs::JobKind;
use crate::jobs::Jobs;

static CUSTOM_FFMPEG: Mutex<Option<PathBuf>> = Mutex::new(None);
/// Counts changes to what's found, so what failed for want of it can try again.
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// How many times what's found may have changed.
pub fn generation() -> u64 {
    GENERATION.load(Ordering::Relaxed)
}

fn changed() {
    GENERATION.fetch_add(1, Ordering::Relaxed);
}

/// The ffmpeg the user chose, if they did.
pub fn custom_ffmpeg() -> Option<PathBuf> {
    CUSTOM_FFMPEG
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .clone()
}

pub fn set_custom_ffmpeg(path: Option<PathBuf>) {
    *CUSTOM_FFMPEG.lock().unwrap_or_else(|err| err.into_inner()) = path;
    changed();
}

/// Where the app keeps what it downloads.
pub fn downloads_dir() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|dirs| dirs.data_local_dir().join("egui-files").join("tools"))
}

/// This platform's name for a program.
fn program_file(name: &str) -> String {
    format!("{name}{}", std::env::consts::EXE_SUFFIX)
}

/// How to run ffmpeg's program `name`: its bare name means the system's.
fn program(name: &str) -> PathBuf {
    if let Some(custom) = custom_ffmpeg() {
        return if name == "ffmpeg" {
            custom
        } else {
            custom.with_file_name(program_file(name))
        };
    }
    let downloaded = downloads_dir().map(|dir| dir.join(program_file(name)));
    match downloaded {
        Some(path) if disk::is_file(&path) => path,
        _ => PathBuf::from(name),
    }
}

pub fn ffmpeg() -> PathBuf {
    program("ffmpeg")
}

pub fn ffprobe() -> PathBuf {
    program("ffprobe")
}

/// The libmpv the app downloaded, if there is one.
pub fn downloaded_libmpv() -> Option<PathBuf> {
    let path = downloads_dir()?.join(LIBMPV_FILE);
    disk::is_file(&path).then_some(path)
}

const LIBMPV_FILE: &str = "libmpv-2.dll";

/// What the app can fetch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    Ffmpeg,
    Libmpv,
}

const FFMPEG_RELEASES: &str = "https://github.com/BtbN/FFmpeg-Builds/releases/download/latest";
const LIBMPV_RELEASE: &str =
    "https://api.github.com/repos/shinchiro/mpv-winbuild-cmake/releases/latest";

static FETCHING_FFMPEG: AtomicBool = AtomicBool::new(false);
static FETCHING_LIBMPV: AtomicBool = AtomicBool::new(false);

impl Tool {
    /// Whether there's a build of it for this computer. Linux has libmpv in
    /// its package managers.
    pub fn offered(self) -> bool {
        let x86_64 = cfg!(target_arch = "x86_64");
        match self {
            Self::Ffmpeg => x86_64 && (cfg!(windows) || cfg!(target_os = "linux")),
            Self::Libmpv => x86_64 && cfg!(windows),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Ffmpeg => "ffmpeg",
            Self::Libmpv => "libmpv",
        }
    }

    fn fetching(self) -> &'static AtomicBool {
        match self {
            Self::Ffmpeg => &FETCHING_FFMPEG,
            Self::Libmpv => &FETCHING_LIBMPV,
        }
    }

    /// What the user should know before saying yes.
    fn terms(self) -> &'static str {
        match self {
            Self::Ffmpeg => {
                "This downloads an LGPL build of FFmpeg from github.com/BtbN/FFmpeg-Builds, about 175 MB, for this app to use.\n\nIt's built by a third party, and has no x264 or x265, so its H.264 is from OpenH264 and it can only encode H.265 with a hardware encoder. For those, install FFmpeg yourself instead."
            }
            Self::Libmpv => {
                "This downloads libmpv from github.com/shinchiro/mpv-winbuild-cmake, about 30 MB, for this app to play videos with.\n\nIt's built by a third party, and includes the codecs it plays videos with."
            }
        }
    }
}

/// A button offering to fetch `tool`, asking first.
pub fn offer(ui: &mut Ui, jobs: &mut Jobs, tool: Tool) {
    if !tool.offered() {
        return;
    }
    let asking = Id::new(("tools-asking", tool.name()));
    let fetching = tool.fetching().load(Ordering::Relaxed);
    let label = if fetching {
        format!("{} Downloading {}…", icon::DOWNLOAD_SIMPLE, tool.name())
    } else {
        format!("{} Download {}…", icon::DOWNLOAD_SIMPLE, tool.name())
    };
    if ui.add_enabled(!fetching, Button::new(label)).clicked() {
        ui.ctx().data_mut(|data| data.insert_temp(asking, true));
    }
    if !ui
        .ctx()
        .data(|data| data.get_temp::<bool>(asking))
        .unwrap_or(false)
    {
        return;
    }
    let (mut start, mut cancel) = (false, false);
    let modal = Modal::new(asking.with("modal")).show(ui.ctx(), |ui| {
        ui.set_width(crate::widgets::modal_width(ui, 420.0));
        ui.heading(format!("Download {}?", tool.name()));
        ui.add_space(4.0);
        ui.label(tool.terms());
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            start = ui.button("Download").clicked();
            cancel = ui.button("Cancel").clicked();
        });
    });
    if start {
        #[cfg(not(target_arch = "wasm32"))]
        {
            tool.fetching().store(true, Ordering::Relaxed);
            jobs.start(JobKind::Download(tool));
        }
        #[cfg(target_arch = "wasm32")]
        let _ = jobs;
    }
    if start || cancel || modal.should_close() {
        ui.ctx().data_mut(|data| data.remove_temp::<bool>(asking));
    }
}

/// A release's file to fetch.
#[derive(Debug, PartialEq, Eq)]
struct Asset {
    url: String,
    name: String,
    /// In lowercase hexadecimal.
    sha256: String,
}

/// This platform's archive of ffmpeg.
fn ffmpeg_asset_name() -> &'static str {
    if cfg!(windows) {
        "ffmpeg-master-latest-win64-lgpl.zip"
    } else {
        "ffmpeg-master-latest-linux64-lgpl.tar.xz"
    }
}

/// The hash of `name` in a `sha256sum` listing.
fn parse_checksum(listing: &str, name: &str) -> Option<String> {
    listing.lines().find_map(|line| {
        let (hash, file) = line.split_once(char::is_whitespace)?;
        (file.trim().trim_start_matches('*') == name).then(|| hash.to_ascii_lowercase())
    })
}

/// The 64-bit libmpv archive in a release as GitHub describes it, which also
/// comes in builds for newer processors only.
fn parse_libmpv_release(json: &str) -> Result<Asset, String> {
    let release: serde_json::Value = serde_json::from_str(json).map_err(|err| err.to_string())?;
    let assets = release.get("assets").and_then(|a| a.as_array());
    let asset = assets
        .into_iter()
        .flatten()
        .find(|asset| {
            asset
                .get("name")
                .and_then(|name| name.as_str())
                .and_then(|name| name.strip_prefix("mpv-dev-x86_64-"))
                .is_some_and(|rest| !rest.starts_with("v3") && rest.ends_with(".7z"))
        })
        .ok_or("the release has no libmpv for this computer")?;
    let text = |key: &str| asset.get(key).and_then(|value| value.as_str());
    let sha256 = text("digest")
        .and_then(|digest| digest.strip_prefix("sha256:"))
        .ok_or("the release doesn't say what its file's hash should be")?;
    Ok(Asset {
        url: text("browser_download_url")
            .ok_or("the release has no link to its file")?
            .to_owned(),
        name: text("name").unwrap_or_default().to_owned(),
        sha256: sha256.to_ascii_lowercase(),
    })
}

/// Pulls the files named `wanted` out of an archive into `dir`, whichever
/// folder in it they're in, and fails if any isn't there.
fn take_from_archive(
    archive: &Path,
    name: &str,
    wanted: &[&str],
    dir: &Path,
) -> Result<(), String> {
    let mut found = Vec::new();
    let mut take = |entry: &str, data: &mut dyn std::io::Read| -> Result<bool, String> {
        // Only ever the last part of its path, so it can't lead elsewhere.
        let file = entry.rsplit(['/', '\\']).next().unwrap_or(entry);
        if !wanted.contains(&file) || found.iter().any(|done| done == file) {
            return Ok(false);
        }
        let part = dir.join(format!("{file}.part"));
        let mut out = disk::File::create(&part).map_err(|err| err.to_string())?;
        std::io::copy(data, &mut out).map_err(|err| err.to_string())?;
        drop(out);
        set_runnable(&part);
        disk::rename(&part, dir.join(file)).map_err(|err| err.to_string())?;
        found.push(file.to_owned());
        Ok(true)
    };
    let source = disk::File::open(archive).map_err(|err| err.to_string())?;
    if name.ends_with(".zip") {
        let mut zip = zip::ZipArchive::new(source).map_err(|err| err.to_string())?;
        for index in 0..zip.len() {
            let mut entry = zip.by_index(index).map_err(|err| err.to_string())?;
            let entry_name = entry.name().map_err(|err| err.to_string())?.into_owned();
            if entry.is_file() {
                take(&entry_name, &mut entry)?;
            }
        }
    } else if name.ends_with(".tar.xz") {
        let mut tar = tar::Archive::new(lzma_rust2::XzReader::new(source, true));
        for entry in tar.entries().map_err(|err| err.to_string())? {
            let mut entry = entry.map_err(|err| err.to_string())?;
            let entry_name = entry
                .path()
                .map_err(|err| err.to_string())?
                .to_string_lossy()
                .into_owned();
            if entry.header().entry_type().is_file() {
                take(&entry_name, &mut entry)?;
            }
        }
    } else {
        let mut reader = sevenz_rust2::ArchiveReader::new(source, sevenz_rust2::Password::empty())
            .map_err(|err| err.to_string())?;
        let mut failure = None;
        reader
            .for_each_entries(|entry, data| {
                if !entry.is_directory
                    && let Err(err) = take(entry.name.as_str(), data)
                {
                    failure = Some(err);
                    return Ok(false);
                }
                Ok(true)
            })
            .map_err(|err| err.to_string())?;
        if let Some(failure) = failure {
            return Err(failure);
        }
    }
    let missing: Vec<&&str> = wanted
        .iter()
        .filter(|file| !found.iter().any(|done| done == *file))
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!("the download didn't have {missing:?} in it"))
    }
}

#[cfg(unix)]
fn set_runnable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = disk::set_permissions(path, disk::Permissions::from_mode(0o755));
}

#[cfg(not(unix))]
fn set_runnable(_path: &Path) {}

#[cfg(not(target_arch = "wasm32"))]
mod fetch {
    use std::{
        io::Write,
        ops::ControlFlow,
        path::Path,
        sync::mpsc::{self, RecvTimeoutError},
        time::Duration,
    };

    use sha2::{Digest, Sha256};

    use super::*;
    use crate::jobs::Progress;

    /// Asked for by name: GitHub's interface turns away requests without one.
    fn request(url: &str) -> ehttp::Request {
        let mut request = ehttp::Request::get(url);
        request.headers.insert("User-Agent", "egui-files");
        request
    }

    fn text(url: &str) -> Result<String, String> {
        let response = ehttp::fetch_blocking(&request(url))?;
        if !response.ok {
            return Err(format!(
                "{url}: {} {}",
                response.status, response.status_text
            ));
        }
        Ok(String::from_utf8_lossy(&response.bytes).into_owned())
    }

    fn locate(tool: Tool) -> Result<Asset, String> {
        match tool {
            Tool::Ffmpeg => {
                let name = ffmpeg_asset_name();
                let listing = text(&format!("{FFMPEG_RELEASES}/checksums.sha256"))?;
                let sha256 = parse_checksum(&listing, name)
                    .ok_or_else(|| format!("no checksum listed for {name}"))?;
                Ok(Asset {
                    url: format!("{FFMPEG_RELEASES}/{name}"),
                    name: name.to_owned(),
                    sha256,
                })
            }
            Tool::Libmpv => parse_libmpv_release(&text(LIBMPV_RELEASE)?),
        }
    }

    /// What comes down from a download.
    enum Piece {
        Length(u64),
        Bytes(Vec<u8>),
        Done,
        Failed(String),
    }

    /// Writes `url` to `path`, returning its hash; `None` if cancelled.
    fn download(url: &str, path: &Path, p: &Progress) -> Result<Option<String>, String> {
        use ehttp::streaming::Part;
        let (sender, pieces) = mpsc::channel();
        ehttp::streaming::fetch(request(url), move |part| {
            let piece = match part {
                Err(err) => Piece::Failed(err),
                Ok(Part::Response(response)) if !response.ok => {
                    Piece::Failed(format!("{} {}", response.status, response.status_text))
                }
                Ok(Part::Response(response)) => Piece::Length(
                    response
                        .headers
                        .get("content-length")
                        .and_then(|length| length.parse().ok())
                        .unwrap_or(0),
                ),
                Ok(Part::Chunk(chunk)) if chunk.is_empty() => Piece::Done,
                Ok(Part::Chunk(chunk)) => Piece::Bytes(chunk),
            };
            let goes_on = matches!(piece, Piece::Length(_) | Piece::Bytes(_));
            // Gone means cancelled.
            if sender.send(piece).is_err() || !goes_on {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        });
        let mut file = disk::File::create(path).map_err(|err| err.to_string())?;
        let mut hash = Sha256::new();
        loop {
            if p.cancelled() {
                return Ok(None);
            }
            match pieces.recv_timeout(Duration::from_millis(100)) {
                Ok(Piece::Length(length)) => p.add_totals(length, 0),
                Ok(Piece::Bytes(bytes)) => {
                    file.write_all(&bytes).map_err(|err| err.to_string())?;
                    hash.update(&bytes);
                    p.add_done(bytes.len() as u64, 0);
                }
                Ok(Piece::Done) => break,
                Ok(Piece::Failed(err)) => return Err(err),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    return Err("the download stopped".to_owned());
                }
            }
        }
        let hex: String = hash
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        Ok(Some(hex))
    }

    /// Fetches `tool` into `dir`, checks it's what was promised and unpacks it.
    pub(super) fn install_into(tool: Tool, dir: &Path, p: &Progress) -> Result<(), String> {
        disk::create_dir_all(dir).map_err(|err| err.to_string())?;
        p.set_status(&format!("Looking for {}", tool.name()));
        let asset = locate(tool)?;
        p.set_status(&asset.name);
        let archive = dir.join(format!("{}.part", asset.name));
        let result = download(&asset.url, &archive, p).and_then(|hash| {
            let Some(hash) = hash else {
                return Ok(());
            };
            if hash != asset.sha256 {
                return Err(format!(
                    "the download isn't what it should be: its hash is {hash}, not {}",
                    asset.sha256
                ));
            }
            p.set_status(&format!("Unpacking {}", tool.name()));
            let exe = std::env::consts::EXE_SUFFIX;
            let (ffmpeg, ffprobe) = (format!("ffmpeg{exe}"), format!("ffprobe{exe}"));
            let wanted: Vec<&str> = match tool {
                Tool::Ffmpeg => vec![&ffmpeg, &ffprobe],
                Tool::Libmpv => vec![LIBMPV_FILE],
            };
            take_from_archive(&archive, &asset.name, &wanted, dir)
        });
        let _ = disk::remove_file(&archive);
        result
    }

    /// The job: fetches `tool` to where the app looks for it.
    pub fn install(tool: Tool, p: &Progress) {
        let result = match downloads_dir() {
            Some(dir) => install_into(tool, &dir, p),
            None => Err("no folder to keep it in".to_owned()),
        };
        tool.fetching().store(false, Ordering::Relaxed);
        if let Err(err) = result {
            p.error(Path::new(tool.name()), err);
        }
        changed();
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub use fetch::install;

/// The settings: which ffmpeg it uses, and fetching what's missing.
pub fn settings_ui(ui: &mut Ui, jobs: &mut Jobs) {
    ui.label("FFmpeg, for exporting videos:");
    let id = Id::new("tools-ffmpeg-path");
    let mut text = ui
        .ctx()
        .data(|data| data.get_temp::<String>(id))
        .unwrap_or_else(|| {
            custom_ffmpeg().map_or_else(String::new, |path| path.display().to_string())
        });
    let edit = ui.add(
        egui::TextEdit::singleline(&mut text)
            .hint_text("Path to ffmpeg (blank to find it)")
            .desired_width(320.0),
    );
    ui.ctx().data_mut(|data| data.insert_temp(id, text.clone()));
    if edit.lost_focus() {
        let chosen = Some(text.trim())
            .filter(|path| !path.is_empty())
            .map(PathBuf::from);
        if chosen != custom_ffmpeg() {
            set_custom_ffmpeg(chosen);
        }
    }
    let using = ffmpeg();
    let found = using.is_absolute() && disk::is_file(&using);
    let bare = using.as_os_str() == OsStr::new("ffmpeg");
    ui.weak(if bare {
        "Using the system's, if it has one".to_owned()
    } else if found {
        format!("Using {}", using.display())
    } else {
        format!("{} isn't there", using.display())
    });
    offer(ui, jobs, Tool::Ffmpeg);
    if Tool::Libmpv.offered() {
        ui.add_space(6.0);
        ui.label("libmpv, for playing videos:");
        ui.weak(match downloaded_libmpv() {
            Some(path) => format!("Using {}", path.display()),
            None => "Using the system's, if it has one".to_owned(),
        });
        offer(ui, jobs, Tool::Libmpv);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_checksum_of_a_file() {
        let listing = "AB12  ffmpeg-a.zip\ncd34 *ffmpeg-b.zip\n";
        assert_eq!(parse_checksum(listing, "ffmpeg-a.zip").unwrap(), "ab12");
        assert_eq!(parse_checksum(listing, "ffmpeg-b.zip").unwrap(), "cd34");
        assert_eq!(parse_checksum(listing, "other.zip"), None);
    }

    #[test]
    fn picks_the_build_for_any_processor() {
        let release = r#"{"assets": [
            {"name": "mpv-dev-aarch64-1.7z", "browser_download_url": "a", "digest": "sha256:AA"},
            {"name": "mpv-dev-x86_64-v3-1.7z", "browser_download_url": "b", "digest": "sha256:BB"},
            {"name": "mpv-dev-x86_64-1.7z", "browser_download_url": "c", "digest": "sha256:CC"}
        ]}"#;
        let asset = parse_libmpv_release(release).unwrap();
        assert_eq!(asset.url, "c");
        assert_eq!(asset.sha256, "cc");

        let without_hash = r#"{"assets": [{"name": "mpv-dev-x86_64-1.7z",
            "browser_download_url": "c"}]}"#;
        assert!(parse_libmpv_release(without_hash).is_err(), "unchecked");
        assert!(parse_libmpv_release(r#"{"assets": []}"#).is_err());
        assert!(parse_libmpv_release("nope").is_err());
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tools-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn takes_only_what_it_wants_from_an_archive() {
        use std::io::Write;
        let dir = temp_dir("zip");
        let path = dir.join("a.zip");
        let mut zip = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
        let options = zip::write::SimpleFileOptions::default();
        for (name, data) in [
            ("build/bin/ffmpeg.exe", "one"),
            ("build/bin/ffprobe.exe", "two"),
            ("build/bin/ffplay.exe", "three"),
            ("../../outside.exe", "four"),
        ] {
            zip.start_file(name, options).unwrap();
            zip.write_all(data.as_bytes()).unwrap();
        }
        zip.finish().unwrap();
        let out = dir.join("out");
        std::fs::create_dir_all(&out).unwrap();
        take_from_archive(&path, "a.zip", &["ffmpeg.exe", "ffprobe.exe"], &out).unwrap();
        assert_eq!(
            std::fs::read_to_string(out.join("ffmpeg.exe")).unwrap(),
            "one"
        );
        assert_eq!(
            std::fs::read_to_string(out.join("ffprobe.exe")).unwrap(),
            "two"
        );
        assert_eq!(std::fs::read_dir(&out).unwrap().count(), 2, "nothing else");
        assert!(!dir.join("outside.exe").exists());

        let err = take_from_archive(&path, "a.zip", &["libmpv-2.dll"], &out).unwrap_err();
        assert!(err.contains("libmpv-2.dll"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_chosen_ffmpeg_brings_its_ffprobe() {
        let chosen = std::env::temp_dir().join("custom").join("ffmpeg");
        set_custom_ffmpeg(Some(chosen.clone()));
        assert_eq!(ffmpeg(), chosen);
        let probe = chosen.with_file_name(program_file("ffprobe"));
        assert_eq!(ffprobe(), probe);
        set_custom_ffmpeg(None);
    }

    /// Fetches the real thing, so it needs the network and takes a while.
    #[test]
    #[ignore = "downloads about 175 MB"]
    fn downloads_the_real_ffmpeg() {
        let dir = temp_dir("download");
        let p = crate::jobs::Progress::default();
        fetch::install_into(Tool::Ffmpeg, &dir, &p).unwrap();
        for name in ["ffmpeg", "ffprobe"] {
            let path = dir.join(program_file(name));
            let out = std::process::Command::new(&path)
                .arg("-version")
                .output()
                .unwrap();
            assert!(out.status.success(), "{name} runs");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
