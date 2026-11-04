//! Reads what a video holds with ffprobe, and exports it with ffmpeg.

use std::{
    collections::HashSet,
    ffi::{OsStr, OsString},
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Mutex, TryLockError,
        mpsc::{self, RecvTimeoutError},
    },
    time::Duration,
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    fs::unique_path,
    jobs::{Outcome, Progress},
};

/// Encoding takes all the processors there are, so exports wait their turn.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

/// How to say ffmpeg is missing.
pub const NOT_INSTALLED: &str = "ffmpeg isn't installed. Install it from your package manager or ffmpeg.org, then open this tab again.";

/// A subtitle stream of the source.
#[derive(Clone, Debug, PartialEq)]
pub struct Subtitle {
    /// In the whole file, as `-map 0:N` means it.
    index: u64,
    codec: String,
}

/// What a video holds.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Source {
    /// Seconds.
    pub duration: f64,
    /// Bytes.
    pub size: u64,
    /// Where the file's own clock starts, which players and `-ss` count from.
    start_time: f64,
    video_index: u64,
    video_codec: String,
    width: u32,
    height: u32,
    fps: f64,
    color_primaries: Option<String>,
    color_transfer: Option<String>,
    color_space: Option<String>,
    audio_codecs: Vec<String>,
    subtitles: Vec<Subtitle>,
    chapters: usize,
    /// Tags saying where it was filmed.
    location: Vec<(String, String)>,
}

impl Source {
    pub fn is_hdr(&self) -> bool {
        matches!(
            self.color_transfer.as_deref(),
            Some("smpte2084" | "arib-std-b67")
        )
    }

    pub fn has_location(&self) -> bool {
        !self.location.is_empty()
    }

    pub fn has_subtitles_or_chapters(&self) -> bool {
        !self.subtitles.is_empty() || self.chapters > 0
    }
}

/// What this ffmpeg can do.
#[derive(Clone, Debug, Default)]
pub struct Tools {
    encoders: HashSet<String>,
    /// The filter converting HDR to SDR needs; only some builds have it.
    zscale: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum Codec {
    H264,
    Hevc,
    Av1,
    Vp9,
}

impl Codec {
    fn encoder(self, tools: &Tools) -> Option<&'static str> {
        let candidates: &[&str] = match self {
            Self::H264 => &["libx264", "libopenh264"],
            Self::Hevc => &["libx265"],
            Self::Av1 => &["libsvtav1", "libaom-av1"],
            Self::Vp9 => &["libvpx-vp9"],
        };
        candidates
            .iter()
            .find(|name| tools.encoders.contains(**name))
            .copied()
    }

    fn of_source(name: &str) -> Option<Self> {
        match name {
            "h264" => Some(Self::H264),
            "hevc" => Some(Self::Hevc),
            "av1" => Some(Self::Av1),
            "vp9" => Some(Self::Vp9),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Format {
    /// The source's: copied if nothing else changes.
    #[default]
    Original,
    Mp4H264,
    Mp4Hevc,
    Mp4Av1,
    WebmVp9,
    WebmAv1,
    MkvH264,
}

impl Format {
    pub const ALL: [Self; 7] = [
        Self::Original,
        Self::Mp4H264,
        Self::Mp4Hevc,
        Self::Mp4Av1,
        Self::WebmVp9,
        Self::WebmAv1,
        Self::MkvH264,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Original => "Same as the original",
            Self::Mp4H264 => "MP4 (H.264)",
            Self::Mp4Hevc => "MP4 (H.265)",
            Self::Mp4Av1 => "MP4 (AV1)",
            Self::WebmVp9 => "WebM (VP9)",
            Self::WebmAv1 => "WebM (AV1)",
            Self::MkvH264 => "MKV (H.264)",
        }
    }

    fn target(self) -> Option<(&'static str, Codec)> {
        match self {
            Self::Original => None,
            Self::Mp4H264 => Some(("mp4", Codec::H264)),
            Self::Mp4Hevc => Some(("mp4", Codec::Hevc)),
            Self::Mp4Av1 => Some(("mp4", Codec::Av1)),
            Self::WebmVp9 => Some(("webm", Codec::Vp9)),
            Self::WebmAv1 => Some(("webm", Codec::Av1)),
            Self::MkvH264 => Some(("mkv", Codec::H264)),
        }
    }

    /// Whether this ffmpeg can make it.
    pub fn available(self, tools: &Tools) -> bool {
        self.target()
            .is_none_or(|(_, codec)| codec.encoder(tools).is_some())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Quality {
    High,
    #[default]
    Balanced,
    Small,
}

impl Quality {
    pub const ALL: [Self; 3] = [Self::High, Self::Balanced, Self::Small];

    pub fn label(self) -> &'static str {
        match self {
            Self::High => "High",
            Self::Balanced => "Balanced",
            Self::Small => "Smaller file",
        }
    }

    /// Picks the value for this quality.
    fn pick<T>(self, high: T, balanced: T, small: T) -> T {
        match self {
            Self::High => high,
            Self::Balanced => balanced,
            Self::Small => small,
        }
    }
}

/// What to carry over from the source.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Keep {
    pub details: bool,
    pub location: bool,
    pub subtitles: bool,
    pub hdr: bool,
}

impl Default for Keep {
    fn default() -> Self {
        Self {
            details: true,
            location: true,
            subtitles: true,
            hdr: true,
        }
    }
}

/// What the editor asks for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    pub format: Format,
    pub quality: Quality,
    /// Re-encode to cut on the exact frame, rather than copy from a keyframe.
    pub exact: bool,
    pub keep: Keep,
}

impl Settings {
    /// Whether the streams can be copied as they are.
    pub fn copies(&self, source: &Source) -> bool {
        self.format == Format::Original && !self.exact && (!source.is_hdr() || self.keep.hdr)
    }

    /// The output's extension, for a source with the extension `original`.
    pub fn extension(&self, source: &Source, original: &str) -> Result<String, String> {
        Ok(self.target(source, original)?.0)
    }

    /// The container and, when re-encoding, the codec. An original that can't
    /// be made again as it is (mpeg4 in an .avi, say) becomes H.264 in an MP4.
    fn target(&self, source: &Source, original: &str) -> Result<(String, Option<Codec>), String> {
        let original = original.to_ascii_lowercase();
        if self.copies(source) {
            return Ok((original, None));
        }
        if let Some((extension, codec)) = self.format.target() {
            return Ok((extension.to_owned(), Some(codec)));
        }
        Ok(match Codec::of_source(&source.video_codec) {
            Some(codec) if fits(&original, codec) => (original, Some(codec)),
            _ => ("mp4".to_owned(), Some(Codec::H264)),
        })
    }
}

/// Whether the container `extension` is one we make `codec` in.
fn fits(extension: &str, codec: Codec) -> bool {
    match extension {
        "mp4" | "m4v" | "mov" | "mkv" => true,
        "webm" => matches!(codec, Codec::Vp9 | Codec::Av1),
        _ => false,
    }
}

/// One export.
pub struct ClipJob {
    pub input: PathBuf,
    /// The folder to save in, and the name without its extension.
    pub dir: PathBuf,
    pub name: String,
    /// Seconds.
    pub start: f64,
    pub end: f64,
    pub settings: Settings,
    pub source: Source,
    pub tools: Tools,
}

impl ClipJob {
    pub fn title(&self) -> String {
        format!("Exporting {}", self.name)
    }
}

/// A program of ffmpeg's, found on the PATH.
fn command(program: &str) -> Command {
    let mut command = Command::new(program);
    command.stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

/// Runs a program of ffmpeg's to the end, for what it prints.
fn output(command: &mut Command) -> Result<String, String> {
    let output = command.output().map_err(|err| match err.kind() {
        std::io::ErrorKind::NotFound => NOT_INSTALLED.to_owned(),
        _ => err.to_string(),
    })?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(last_line(&String::from_utf8_lossy(&output.stderr)))
    }
}

fn last_line(text: &str) -> String {
    text.lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("ffmpeg failed")
        .to_owned()
}

/// The file in a phone's mount that `path` means, or `path` itself.
fn reachable(path: &Path) -> PathBuf {
    crate::phones::mount_path(path).unwrap_or_else(|| path.to_path_buf())
}

/// What this ffmpeg can do. Blocks while it starts.
pub fn tools() -> Result<Tools, String> {
    let encoders = output(command("ffmpeg").args(["-hide_banner", "-encoders"]))?;
    let filters = output(command("ffmpeg").args(["-hide_banner", "-filters"]))?;
    Ok(Tools {
        encoders: parse_names(&encoders),
        zscale: parse_names(&filters).contains("zscale"),
    })
}

/// The second column of `ffmpeg -encoders` and `-filters`, after their headers.
fn parse_names(text: &str) -> HashSet<String> {
    text.lines()
        .skip_while(|line| !line.trim_start().starts_with("---"))
        .skip(1)
        .filter_map(|line| line.split_whitespace().nth(1))
        .map(str::to_owned)
        .collect()
}

/// Reads what `path` holds. Blocks while ffprobe reads it.
pub fn probe(path: &Path) -> Result<Source, String> {
    let text = output(
        command("ffprobe")
            .args([
                "-v",
                "error",
                "-show_format",
                "-show_streams",
                "-show_chapters",
                "-of",
                "json",
            ])
            .arg(reachable(path)),
    )?;
    parse_probe(&text)
}

fn parse_probe(text: &str) -> Result<Source, String> {
    let root: Value = serde_json::from_str(text).map_err(|err| err.to_string())?;
    let number = |value: &Value, key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .and_then(|text| text.parse::<f64>().ok())
    };
    let text_of = |value: &Value, key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .filter(|text| *text != "unknown")
            .map(str::to_owned)
    };
    let format = root.get("format").ok_or("Not a media file")?;
    let mut source = Source {
        duration: number(format, "duration").unwrap_or(0.0),
        size: number(format, "size").unwrap_or(0.0) as u64,
        start_time: number(format, "start_time").unwrap_or(0.0),
        chapters: root
            .get("chapters")
            .and_then(Value::as_array)
            .map_or(0, Vec::len),
        ..Source::default()
    };
    if let Some(tags) = format.get("tags").and_then(Value::as_object) {
        source.location = tags
            .iter()
            .filter(|(key, _)| is_location_tag(key))
            .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_owned())))
            .collect();
    }
    let mut found_video = false;
    let streams = root.get("streams").and_then(Value::as_array);
    for stream in streams.into_iter().flatten() {
        let codec = text_of(stream, "codec_name").unwrap_or_default();
        let index = stream.get("index").and_then(Value::as_u64).unwrap_or(0);
        match stream.get("codec_type").and_then(Value::as_str) {
            Some("video") if !found_video && !is_cover(stream) => {
                found_video = true;
                source.video_index = index;
                source.video_codec = codec;
                let side = |key| stream.get(key).and_then(Value::as_u64).unwrap_or(0) as u32;
                source.width = side("width");
                source.height = side("height");
                source.fps = stream
                    .get("avg_frame_rate")
                    .and_then(Value::as_str)
                    .and_then(parse_rate)
                    .unwrap_or(30.0);
                source.color_primaries = text_of(stream, "color_primaries");
                source.color_transfer = text_of(stream, "color_transfer");
                source.color_space = text_of(stream, "color_space");
            }
            Some("audio") => source.audio_codecs.push(codec),
            Some("subtitle") => source.subtitles.push(Subtitle { index, codec }),
            _ => {}
        }
    }
    if !found_video {
        return Err("This file has no video.".to_owned());
    }
    if source.duration <= 0.0 {
        return Err("Can't tell how long this video is.".to_owned());
    }
    Ok(source)
}

fn is_cover(stream: &Value) -> bool {
    stream
        .pointer("/disposition/attached_pic")
        .and_then(Value::as_i64)
        == Some(1)
}

/// `location`, `location-eng`, `com.apple.quicktime.location.ISO6709`…
fn is_location_tag(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key.contains("location") || key.contains("gps")
}

/// A frame rate as ffprobe writes it, like `30000/1001`.
fn parse_rate(text: &str) -> Option<f64> {
    let (top, bottom) = text.split_once('/')?;
    let (top, bottom) = (top.parse::<f64>().ok()?, bottom.parse::<f64>().ok()?);
    (bottom > 0.0 && top > 0.0).then(|| top / bottom)
}

/// Where a copy asked to start at `at` really starts: the last keyframe, as
/// nothing before one can be decoded. Blocks while ffprobe reads around it.
pub fn keyframe_before(path: &Path, source: &Source, at: f64) -> Result<f64, String> {
    if at <= 0.0 {
        return Ok(0.0);
    }
    let path = reachable(path);
    let absolute = at + source.start_time;
    // Reading only around it keeps this quick in a file of hours; few videos
    // space their keyframes further apart than the first of these.
    for window in [15.0, 120.0, f64::INFINITY] {
        let from = (absolute - window).max(0.0);
        let text = output(
            command("ffprobe")
                .args(["-v", "error", "-select_streams"])
                .arg(source.video_index.to_string())
                .args(["-show_entries", "packet=pts_time,flags", "-of", "csv=p=0"])
                .arg("-read_intervals")
                .arg(format!("{from:.6}%{:.6}", absolute + 0.001))
                .arg(&path),
        )?;
        if let Some(keyframe) = parse_keyframe(&text, absolute) {
            return Ok((keyframe - source.start_time).max(0.0));
        }
        if from == 0.0 {
            break;
        }
    }
    Ok(0.0)
}

/// The last keyframe at or before `at`, in lines of `pts_time,flags`.
fn parse_keyframe(text: &str, at: f64) -> Option<f64> {
    text.lines()
        .filter_map(|line| {
            let (time, flags) = line.split_once(',')?;
            let time = time.parse::<f64>().ok()?;
            (flags.starts_with('K') && time <= at + 0.001).then_some(time)
        })
        .fold(None, |last: Option<f64>, time| {
            Some(last.map_or(time, |last| last.max(time)))
        })
}

/// Whether audio in `codec` can go into the container `extension` as it is.
fn audio_fits(extension: &str, codec: &str) -> bool {
    match extension {
        "mp4" | "m4v" | "mov" => matches!(codec, "aac" | "mp3" | "ac3" | "eac3" | "alac" | "opus"),
        "webm" => matches!(codec, "opus" | "vorbis"),
        "mkv" => true,
        _ => false,
    }
}

/// What subtitles in `codec` become in the container `extension`; `None`
/// if they can't go in, as pictures can't into MP4 and WebM.
fn subtitle_encoder(extension: &str, codec: &str) -> Option<&'static str> {
    let text = matches!(
        codec,
        "subrip" | "srt" | "ass" | "ssa" | "mov_text" | "webvtt" | "text"
    );
    match extension {
        "mp4" | "m4v" | "mov" => text.then_some("mov_text"),
        "webm" => text.then_some("webvtt"),
        "mkv" if codec == "mov_text" => Some("srt"),
        "mkv" => Some("copy"),
        _ => None,
    }
}

fn is_mov(extension: &str) -> bool {
    matches!(extension, "mp4" | "m4v" | "mov")
}

/// ffmpeg's arguments to encode the video.
fn video_args(
    codec: Codec,
    extension: &str,
    job: &ClipJob,
    tone_map: bool,
) -> Result<Vec<String>, String> {
    let quality = job.settings.quality;
    let encoder = codec
        .encoder(&job.tools)
        .ok_or_else(|| format!("This ffmpeg has no encoder for {codec:?}."))?;
    let mut args: Vec<String> = vec!["-c:v".into(), encoder.into()];
    let mut add = |more: &[&str]| args.extend(more.iter().map(|&arg| arg.to_owned()));
    let crf =
        |high: u32, balanced: u32, small: u32| quality.pick(high, balanced, small).to_string();
    match encoder {
        "libx264" => add(&["-preset", "medium", "-crf", &crf(18, 23, 28)]),
        "libx265" => add(&["-preset", "medium", "-crf", &crf(20, 26, 31)]),
        "libsvtav1" => add(&["-preset", "8", "-crf", &crf(24, 32, 42)]),
        "libaom-av1" => add(&[
            "-cpu-used",
            "6",
            "-row-mt",
            "1",
            "-crf",
            &crf(24, 32, 42),
            "-b:v",
            "0",
        ]),
        "libvpx-vp9" => add(&[
            "-deadline",
            "good",
            "-cpu-used",
            "3",
            "-row-mt",
            "1",
            "-crf",
            &crf(24, 32, 40),
            "-b:v",
            "0",
        ]),
        // OpenH264 is the one H.264 encoder LGPL builds have, and takes only
        // a bitrate: so many bits for each pixel of each frame.
        _ => {
            let source = &job.source;
            let pixels = f64::from(source.width) * f64::from(source.height) * source.fps;
            let bitrate = (pixels * quality.pick(0.15, 0.08, 0.04)) as u64;
            add(&["-b:v", &bitrate.to_string()]);
        }
    }
    if codec == Codec::Hevc && is_mov(extension) {
        // Apple's players want this tag for H.265.
        add(&["-tag:v", "hvc1"]);
    }
    let source = &job.source;
    if source.is_hdr() && !tone_map {
        add(&["-pix_fmt", "yuv420p10le"]);
        for (flag, value) in [
            ("-color_primaries", &source.color_primaries),
            ("-color_trc", &source.color_transfer),
            ("-colorspace", &source.color_space),
        ] {
            if let Some(value) = value {
                add(&[flag, value]);
            }
        }
    } else {
        // What every player can show.
        add(&["-pix_fmt", "yuv420p"]);
    }
    Ok(args)
}

/// Converts HDR to SDR.
const TONE_MAP: &str = "zscale=t=linear:npl=100,format=gbrpf32le,zscale=p=bt709,\
                        tonemap=tonemap=hable:desat=0,zscale=t=bt709:m=bt709:r=tv,format=yuv420p";

/// How far past a keyframe to ask a copy to start, to land on that keyframe.
/// Seeking goes to the keyframe at or before the time, but ffmpeg moves the
/// time back by 3/23 s first in containers it can't seek by display time, for
/// videos with B-frames. MP4 can, and hides what's before the time asked for,
/// so there it's only enough for ffprobe's rounding: 10 µs loses a frame at
/// 60 fps.
fn copy_margin(extension: &str) -> f64 {
    if is_mov(extension) { 0.000_001 } else { 0.14 }
}

/// ffmpeg's arguments for a job, given where it writes. A copy's `start` is
/// the keyframe it starts on by now.
fn plan(job: &ClipJob, extension: &str, output: &Path) -> Result<Vec<OsString>, String> {
    let ClipJob {
        source, settings, ..
    } = job;
    let (_, codec) = settings.target(source, extension)?;
    let keep = settings.keep;
    let mut args: Vec<OsString> = Vec::new();
    let mut add = |more: &[&str]| args.extend(more.iter().map(OsString::from));
    add(&["-hide_banner", "-v", "error", "-nostdin", "-nostats", "-n"]);
    add(&["-progress", "pipe:1"]);
    let seek = job.start
        + if settings.copies(source) {
            copy_margin(extension)
        } else {
            0.0
        };
    if seek > 0.0 {
        add(&["-ss", &format!("{seek:.6}")]);
    }
    if job.end < source.duration {
        add(&["-t", &format!("{:.6}", job.end - seek)]);
    }
    args.push("-i".into());
    args.push(reachable(&job.input).into());
    let mut add = |more: &[&str]| args.extend(more.iter().map(OsString::from));

    add(&["-map", &format!("0:{}", source.video_index), "-map", "0:a?"]);
    let copying = codec.is_none();
    let mut subtitle_args = Vec::new();
    if keep.subtitles {
        let mut mapped = 0;
        for subtitle in &source.subtitles {
            let encoder = if copying {
                Some("copy")
            } else {
                subtitle_encoder(extension, &subtitle.codec)
            };
            if let Some(encoder) = encoder {
                subtitle_args.push("-map".to_owned());
                subtitle_args.push(format!("0:{}", subtitle.index));
                subtitle_args.push(format!("-c:s:{mapped}"));
                subtitle_args.push(encoder.to_owned());
                mapped += 1;
            }
        }
    }
    let subtitle_args: Vec<&str> = subtitle_args.iter().map(String::as_str).collect();
    add(&subtitle_args);

    if let Some(codec) = codec {
        // H.264 here is 8-bit, which can't hold HDR.
        let tone_map = source.is_hdr() && (!keep.hdr || codec == Codec::H264);
        if tone_map {
            if !job.tools.zscale {
                return Err(
                    "Converting HDR to SDR needs an ffmpeg with zimg. Keep HDR, in a format that can, or install another build."
                        .to_owned(),
                );
            }
            add(&["-vf", TONE_MAP]);
        }
        let video = video_args(codec, extension, job, tone_map)?;
        let video: Vec<&str> = video.iter().map(String::as_str).collect();
        add(&video);
        if source.audio_codecs.iter().all(|c| audio_fits(extension, c)) {
            add(&["-c:a", "copy"]);
        } else if extension == "webm" {
            add(&["-c:a", "libopus", "-b:a", "160k"]);
        } else {
            add(&["-c:a", "aac", "-b:a", "192k"]);
        }
    } else {
        add(&["-c:v", "copy", "-c:a", "copy"]);
    }

    add(&["-map_chapters", if keep.subtitles { "0" } else { "-1" }]);
    add(&["-map_metadata", if keep.details { "0" } else { "-1" }]);
    // Either the tags stayed and these blank them, or they went and these put
    // them back.
    if keep.details != keep.location {
        for (key, value) in &source.location {
            let value = if keep.location { value.as_str() } else { "" };
            add(&["-metadata", &format!("{key}={value}")]);
        }
    }
    if is_mov(extension) {
        // Phones' tags have names MP4's own list hasn't.
        add(&["-movflags", "+faststart+use_metadata_tags"]);
    }
    args.push(output.into());
    Ok(args)
}

/// The value of ffmpeg's progress line saying how far it's got, in seconds.
fn parse_progress(line: &str) -> Option<f64> {
    let micros = line.strip_prefix("out_time_us=")?.trim();
    Some(micros.parse::<f64>().ok()? / 1_000_000.0)
}

/// Runs a job, leaving the output only if all of it was made.
pub fn export(job: ClipJob, p: &Progress) {
    p.set_time_total(job.end - job.start);
    let Some(_turn) = wait_for_turn(&job, p) else {
        return;
    };
    p.set_current(&job.input);
    if let Err((path, err)) = run(job, p) {
        p.error(&path, err);
    }
}

/// Waits for exports started earlier; `None` if cancelled meanwhile.
fn wait_for_turn<'a>(job: &ClipJob, p: &Progress) -> Option<std::sync::MutexGuard<'a, ()>> {
    let mut told = false;
    loop {
        if p.cancelled() {
            return None;
        }
        match ONE_AT_A_TIME.try_lock() {
            Ok(guard) => return Some(guard),
            // An export that panicked has nothing left to protect.
            Err(TryLockError::Poisoned(err)) => return Some(err.into_inner()),
            Err(TryLockError::WouldBlock) => {
                if !told {
                    told = true;
                    p.set_status(&format!("{}: waiting for other exports", job.name));
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

fn run(mut job: ClipJob, p: &Progress) -> Result<(), (PathBuf, String)> {
    let input = job.input.clone();
    let fail = |err: String| (input.clone(), err);
    let original = job
        .input
        .extension()
        .and_then(OsStr::to_str)
        .unwrap_or_default()
        .to_owned();
    let extension = job
        .settings
        .extension(&job.source, &original)
        .map_err(fail)?;
    if job.settings.copies(&job.source) {
        job.start = keyframe_before(&job.input, &job.source, job.start).map_err(fail)?;
    }
    p.set_time_total(job.end - job.start);

    let dir = reachable(&job.dir);
    let temporary = unique_path(&dir, OsStr::new(&format!(".{}.part.{extension}", job.name)));
    let args = plan(&job, &extension, &temporary).map_err(fail)?;
    let result = encode(&args, p);
    let result = result.and_then(|()| {
        if p.cancelled() {
            return Ok(None);
        }
        // Looked for last, so nothing that appeared meanwhile is overwritten.
        let name = format!("{}.{extension}", job.name);
        let last = unique_path(&dir, OsStr::new(&name));
        disk::rename(&temporary, &last).map_err(|err| err.to_string())?;
        Ok(Some(last))
    });
    match result {
        Ok(Some(last)) => {
            // Shown where the editor was asked to save, on a phone too.
            let shown = job.dir.join(last.file_name().unwrap_or_default());
            p.set_outcome(Outcome::Reveal(shown));
            Ok(())
        }
        Ok(None) => {
            let _ = disk::remove_file(&temporary);
            Ok(())
        }
        Err(err) => {
            let _ = disk::remove_file(&temporary);
            Err(fail(err))
        }
    }
}

/// Runs ffmpeg with `args`, showing its progress, until it ends or the job is
/// cancelled.
fn encode(args: &[OsString], p: &Progress) -> Result<(), String> {
    let mut child = command("ffmpeg")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| match err.kind() {
            std::io::ErrorKind::NotFound => NOT_INSTALLED.to_owned(),
            _ => err.to_string(),
        })?;
    let (lines, progress) = mpsc::channel();
    if let Some(stdout) = child.stdout.take() {
        crate::background::spawn("ffmpeg-progress", move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if lines.send(line).is_err() {
                    break;
                }
            }
        });
    }
    let (errors_done, errors) = mpsc::channel();
    if let Some(mut stderr) = child.stderr.take() {
        crate::background::spawn("ffmpeg-errors", move || {
            let mut text = String::new();
            let _ = stderr.read_to_string(&mut text);
            let _ = errors_done.send(text);
        });
    }
    loop {
        match progress.recv_timeout(Duration::from_millis(100)) {
            Ok(line) => {
                if let Some(seconds) = parse_progress(&line) {
                    p.set_time_done(seconds);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if p.cancelled() {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(());
        }
    }
    let status = child.wait().map_err(|err| err.to_string())?;
    if status.success() {
        return Ok(());
    }
    let text = errors
        .recv_timeout(Duration::from_secs(2))
        .unwrap_or_default();
    Err(last_line(&text))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROBED: &str = r#"{
        "streams": [
            {"index": 0, "codec_type": "video", "codec_name": "mjpeg",
             "width": 64, "height": 64, "disposition": {"attached_pic": 1}},
            {"index": 1, "codec_type": "video", "codec_name": "hevc", "width": 3840,
             "height": 2160, "avg_frame_rate": "30000/1001", "color_transfer": "smpte2084",
             "color_primaries": "bt2020", "color_space": "bt2020nc",
             "disposition": {"attached_pic": 0}},
            {"index": 2, "codec_type": "audio", "codec_name": "aac"},
            {"index": 3, "codec_type": "subtitle", "codec_name": "hdmv_pgs_subtitle"},
            {"index": 4, "codec_type": "subtitle", "codec_name": "subrip"}
        ],
        "chapters": [{"id": 0}],
        "format": {"duration": "90.5", "size": "1000", "start_time": "1.400000",
            "tags": {"title": "Holiday", "location": "+37.7+122.4/",
                     "com.apple.quicktime.location.ISO6709": "+37.7+122.4/"}}
    }"#;

    fn tools(encoders: &[&str]) -> Tools {
        Tools {
            encoders: encoders.iter().map(|&e| e.to_owned()).collect(),
            zscale: true,
        }
    }

    fn job(settings: Settings) -> ClipJob {
        ClipJob {
            input: "/in/a.mkv".into(),
            dir: "/in".into(),
            name: "a (edited)".into(),
            start: 10.0,
            end: 20.0,
            settings,
            source: parse_probe(PROBED).unwrap(),
            tools: tools(&["libx264", "libx265", "libsvtav1", "libvpx-vp9"]),
        }
    }

    fn planned(job: &ClipJob, extension: &str) -> Vec<String> {
        plan(job, extension, Path::new("/in/out"))
            .unwrap()
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    /// Whether `args` holds `flag` followed by `value`.
    fn has(args: &[String], flag: &str, value: &str) -> bool {
        args.windows(2).any(|w| w[0] == flag && w[1] == value)
    }

    #[test]
    fn reads_what_a_video_holds() {
        let source = parse_probe(PROBED).unwrap();
        assert_eq!(source.video_index, 1, "the cover isn't the video");
        assert_eq!((source.width, source.height), (3840, 2160));
        assert!((source.fps - 29.97).abs() < 0.01);
        assert!(source.is_hdr());
        assert_eq!(source.location.len(), 2, "not the title");
        assert_eq!(source.chapters, 1);
        assert_eq!(source.subtitles.len(), 2);
    }

    #[test]
    fn refuses_what_it_cannot_cut() {
        let no_video = r#"{"streams": [{"codec_type": "audio"}], "format": {"duration": "9"}}"#;
        assert!(parse_probe(no_video).is_err());
        let no_length = r#"{"streams": [{"codec_type": "video"}], "format": {}}"#;
        assert!(parse_probe(no_length).is_err());
        assert!(parse_probe("not json").is_err());
    }

    #[test]
    fn reads_ffmpegs_lists_and_progress() {
        let list = " V..... = Video\n ------\n V....D libx264    H.264\n A....D aac   AAC\n";
        let names = parse_names(list);
        assert!(names.contains("libx264") && names.contains("aac"));
        assert!(!names.contains("="), "not the legend");
        assert_eq!(parse_progress("out_time_us=1500000"), Some(1.5));
        assert_eq!(parse_progress("out_time_us=N/A"), None);
        assert_eq!(parse_progress("frame=12"), None);
    }

    #[test]
    fn finds_the_keyframe_before_a_time() {
        let packets = "0.000000,K__\n1.000000,___\n2.000000,K__\n3.000000,___\n4.000000,K__\n";
        assert_eq!(parse_keyframe(packets, 3.5), Some(2.0));
        assert_eq!(parse_keyframe(packets, 2.0), Some(2.0));
        assert_eq!(parse_keyframe("N/A,K__\n1.000000,___\n", 3.0), None);
    }

    #[test]
    fn copies_only_a_plain_trim() {
        let source = parse_probe(PROBED).unwrap();
        let plain = Settings::default();
        assert!(plain.copies(&source));
        let exact = Settings {
            exact: true,
            ..plain
        };
        assert!(!exact.copies(&source));
        let converted = Settings {
            format: Format::WebmVp9,
            ..plain
        };
        assert!(!converted.copies(&source));
        let mut sdr = plain;
        sdr.keep.hdr = false;
        assert!(!sdr.copies(&source), "SDR from HDR has to re-encode");

        let args = planned(&job(plain), "mkv");
        assert!(has(&args, "-c:v", "copy"));
        assert!(has(&args, "-ss", "10.140000"), "past the keyframe");
        assert!(has(&args, "-t", "9.860000"), "to the same end");
        assert!(has(&args, "-map", "0:1"), "the video, not its cover");
    }

    #[test]
    fn frame_exact_keeps_the_originals_codec_and_container() {
        let settings = Settings {
            exact: true,
            ..Settings::default()
        };
        let mut source = parse_probe(PROBED).unwrap();
        assert_eq!(settings.extension(&source, "MKV").unwrap(), "mkv");
        let args = planned(&job(settings), "mkv");
        assert!(has(&args, "-c:v", "libx265"));
        assert!(has(&args, "-pix_fmt", "yuv420p10le"), "stays HDR");
        assert!(has(&args, "-color_trc", "smpte2084"));

        source.video_codec = "mpeg4".to_owned();
        assert_eq!(settings.extension(&source, "avi").unwrap(), "mp4");
    }

    #[test]
    fn h264_cannot_hold_hdr() {
        let settings = Settings {
            format: Format::Mp4H264,
            ..Settings::default()
        };
        let args = planned(&job(settings), "mp4");
        assert!(has(&args, "-vf", TONE_MAP));
        assert!(has(&args, "-pix_fmt", "yuv420p"));

        let mut job = job(settings);
        job.tools.zscale = false;
        assert!(plan(&job, "mp4", Path::new("/out")).is_err());
    }

    #[test]
    fn subtitles_go_only_where_they_can() {
        let settings = Settings {
            format: Format::Mp4H264,
            ..Settings::default()
        };
        let args = planned(&job(settings), "mp4");
        assert!(!has(&args, "-map", "0:3"), "MP4 can't hold pictures");
        assert!(has(&args, "-map", "0:4"));
        assert!(has(&args, "-c:s:0", "mov_text"));

        let mut without = settings;
        without.keep.subtitles = false;
        let args = planned(&job(without), "mp4");
        assert!(!has(&args, "-map", "0:4"));
        assert!(has(&args, "-map_chapters", "-1"));
    }

    #[test]
    fn location_and_details_are_kept_apart() {
        let tag = "location=+37.7+122.4/";
        let mut settings = Settings::default();
        let args = planned(&job(settings), "mkv");
        assert!(has(&args, "-map_metadata", "0"));
        assert!(!args.iter().any(|arg| arg.starts_with("location=")));

        settings.keep.location = false;
        let args = planned(&job(settings), "mkv");
        assert!(has(&args, "-metadata", "location="), "blanked");

        settings.keep.details = false;
        let args = planned(&job(settings), "mkv");
        assert!(has(&args, "-map_metadata", "-1"));
        assert!(!args.iter().any(|arg| arg.starts_with("location")));

        settings.keep.location = true;
        let args = planned(&job(settings), "mkv");
        assert!(has(&args, "-metadata", tag), "put back");
    }

    #[test]
    fn missing_encoders_are_said() {
        let few = tools(&["libx264"]);
        assert!(Format::Mp4H264.available(&few));
        assert!(Format::Original.available(&few));
        assert!(!Format::WebmVp9.available(&few));
        let lgpl = tools(&["libopenh264"]);
        assert_eq!(Codec::H264.encoder(&lgpl), Some("libopenh264"));
    }

    /// What ffprobe says of a file.
    fn entries(path: &Path, what: &str) -> String {
        output(
            command("ffprobe")
                .args(["-v", "error", "-show_entries", what, "-of", "csv=p=0"])
                .arg(path),
        )
        .unwrap()
    }

    /// Makes a video with a keyframe every 2 seconds, tags and a location.
    /// `None` without ffmpeg.
    fn sample(dir: &Path) -> Option<PathBuf> {
        let path = dir.join("sample.mp4");
        let made = command("ffmpeg")
            .args(["-v", "error", "-f", "lavfi", "-i"])
            .arg("testsrc2=size=160x90:rate=10:duration=10")
            .args(["-f", "lavfi", "-i", "sine=frequency=440:duration=10"])
            .args(["-c:v", "libx264", "-g", "20", "-c:a", "aac"])
            .args(["-metadata", "title=Holiday"])
            .args(["-metadata", "location=+37.7+122.4/"])
            .arg(&path)
            .status();
        made.is_ok_and(|status| status.success()).then_some(path)
    }

    fn run_job(dir: &Path, input: &Path, settings: Settings, range: (f64, f64)) -> PathBuf {
        let source = probe(input).unwrap();
        let p = Progress::default();
        export(
            ClipJob {
                input: input.to_path_buf(),
                dir: dir.to_path_buf(),
                name: "sample (edited)".to_owned(),
                start: range.0,
                end: range.1,
                settings,
                source,
                tools: super::tools().unwrap(),
            },
            &p,
        );
        assert_eq!(p.errors(), Vec::<String>::new());
        let Some(Outcome::Reveal(path)) = p.take_outcome() else {
            panic!("no output");
        };
        path
    }

    fn duration(path: &Path) -> f64 {
        entries(path, "format=duration").trim().parse().unwrap()
    }

    /// A hash of the first frame shown, from `at` seconds if given.
    fn first_frame(path: &Path, at: Option<f64>) -> String {
        let mut ffmpeg = command("ffmpeg");
        ffmpeg.args(["-v", "error"]);
        if let Some(at) = at {
            ffmpeg.arg("-ss").arg(at.to_string());
        }
        let text = output(ffmpeg.arg("-i").arg(path).args([
            "-map",
            "0:v",
            "-frames:v",
            "1",
            "-f",
            "framemd5",
            "-",
        ]))
        .unwrap();
        let line = text.lines().last().unwrap();
        line.split_whitespace().last().unwrap().to_owned()
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("clip-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn exports_a_real_video() {
        let dir = temp_dir("export");
        let Some(input) = sample(&dir) else {
            eprintln!("no ffmpeg: skipped");
            return;
        };
        let source = probe(&input).unwrap();
        assert!(source.has_location());
        assert_eq!(keyframe_before(&input, &source, 3.3).unwrap(), 2.0);

        // A fast cut starts on the keyframe before the mark.
        let plain = Settings::default();
        let fast = run_job(&dir, &input, plain, (3.3, 7.0));
        assert_eq!(fast.extension().unwrap(), "mp4");
        assert!((duration(&fast) - 5.0).abs() < 0.3, "{}", duration(&fast));
        let at_keyframe = first_frame(&input, Some(2.0));
        assert_eq!(first_frame(&fast, None), at_keyframe, "the keyframe in MP4");
        // Not with a lead-in of audio before the picture starts.
        let video_start = entries(&fast, "stream=start_time");
        assert!(
            video_start
                .lines()
                .all(|time| time.trim().parse::<f64>().unwrap() < 0.05),
            "{video_start}"
        );
        assert!(entries(&fast, "format_tags=title").contains("Holiday"));
        assert!(entries(&fast, "format_tags=location").contains("+37.7"));

        // Matroska lands on its keyframe too, whose time isn't a round one.
        let mkv = dir.join("sample.mkv");
        let remuxed = command("ffmpeg")
            .args(["-v", "error", "-i"])
            .arg(&input)
            .args(["-c", "copy"])
            .arg(&mkv)
            .status();
        assert!(remuxed.unwrap().success());
        let out = run_job(&dir, &mkv, plain, (3.3, 7.0));
        assert_eq!(out.extension().unwrap(), "mkv");
        let length = duration(&out);
        assert!((4.7..=5.8).contains(&length), "{length}");
        assert_eq!(first_frame(&out, None), at_keyframe, "the keyframe in MKV");

        // Never over what's there.
        let again = run_job(&dir, &input, plain, (3.3, 7.0));
        assert_ne!(again, fast);

        // A frame-exact one starts on the mark.
        let exact = Settings {
            exact: true,
            ..plain
        };
        let cut = run_job(&dir, &input, exact, (3.3, 7.0));
        assert!((duration(&cut) - 3.7).abs() < 0.3, "{}", duration(&cut));

        // Without the location, the rest stays.
        let mut private = plain;
        private.keep.location = false;
        let out = run_job(&dir, &input, private, (0.0, 4.0));
        assert!(entries(&out, "format_tags=title").contains("Holiday"));
        assert!(!entries(&out, "format_tags=location").contains("+37.7"));

        // Without details, the location can stay on its own.
        let mut only_location = plain;
        only_location.keep.details = false;
        let out = run_job(&dir, &input, only_location, (0.0, 4.0));
        assert!(!entries(&out, "format_tags=title").contains("Holiday"));
        assert!(entries(&out, "format_tags=location").contains("+37.7"));

        // Nothing half-made is left behind.
        let leftovers = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().contains(".part."))
            .count();
        assert_eq!(leftovers, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn converts_to_webm() {
        let dir = temp_dir("convert");
        let Some(input) = sample(&dir) else {
            eprintln!("no ffmpeg: skipped");
            return;
        };
        let settings = Settings {
            format: Format::WebmVp9,
            quality: Quality::Small,
            ..Settings::default()
        };
        if !settings.format.available(&super::tools().unwrap()) {
            eprintln!("no VP9 encoder: skipped");
            return;
        }
        let out = run_job(&dir, &input, settings, (0.0, 2.0));
        assert_eq!(out.extension().unwrap(), "webm");
        assert!(entries(&out, "stream=codec_name").contains("vp9"));
        assert!(entries(&out, "stream=codec_name").contains("opus"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A video with a chapter and subtitles, in Matroska.
    fn sample_with_extras(dir: &Path) -> Option<PathBuf> {
        let plain = sample(dir)?;
        let srt = dir.join("subs.srt");
        std::fs::write(&srt, "1\n00:00:01,000 --> 00:00:08,000\nHello\n").unwrap();
        let chapters = dir.join("chapters.txt");
        std::fs::write(
            &chapters,
            ";FFMETADATA1\n[CHAPTER]\nTIMEBASE=1/1\nSTART=0\nEND=10\ntitle=One\n",
        )
        .unwrap();
        let path = dir.join("extras.mkv");
        let made = command("ffmpeg")
            .args(["-v", "error", "-i"])
            .arg(&plain)
            .arg("-i")
            .arg(&srt)
            .arg("-i")
            .arg(&chapters)
            .args(["-map", "0", "-map", "1", "-map_metadata", "2", "-c", "copy"])
            .arg(&path)
            .status();
        made.is_ok_and(|status| status.success()).then_some(path)
    }

    #[test]
    fn subtitles_and_chapters_survive_when_asked() {
        let dir = temp_dir("extras");
        let Some(input) = sample_with_extras(&dir) else {
            eprintln!("no ffmpeg: skipped");
            return;
        };
        assert!(probe(&input).unwrap().has_subtitles_or_chapters());
        let streams = |path: &Path| entries(path, "stream=codec_type");

        let copied = run_job(&dir, &input, Settings::default(), (0.0, 6.0));
        assert!(streams(&copied).contains("subtitle"));
        assert!(!entries(&copied, "chapter=id").trim().is_empty());

        let converted = Settings {
            format: Format::Mp4H264,
            ..Settings::default()
        };
        let out = run_job(&dir, &input, converted, (0.0, 6.0));
        assert!(entries(&out, "stream=codec_name").contains("mov_text"));
        assert!(!entries(&out, "chapter=id").trim().is_empty());

        let mut without = Settings::default();
        without.keep.subtitles = false;
        let out = run_job(&dir, &input, without, (0.0, 6.0));
        assert!(!streams(&out).contains("subtitle"));
        assert!(entries(&out, "chapter=id").trim().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hdr_is_kept_or_converted() {
        let dir = temp_dir("hdr");
        let path = dir.join("hdr.mkv");
        let made = command("ffmpeg")
            .args(["-v", "error", "-f", "lavfi", "-i"])
            .arg("testsrc2=size=160x90:rate=10:duration=3")
            .arg("-vf")
            .arg("setparams=color_primaries=bt2020:color_trc=smpte2084:colorspace=bt2020nc")
            .args(["-c:v", "libx265", "-pix_fmt", "yuv420p10le"])
            .args(["-x265-params", "log-level=error"])
            .arg(&path)
            .status();
        if !made.is_ok_and(|status| status.success()) {
            eprintln!("no ffmpeg with x265: skipped");
            return;
        }
        assert!(probe(&path).unwrap().is_hdr());
        let color = |path: &Path| entries(path, "stream=color_transfer,pix_fmt");

        let exact = Settings {
            exact: true,
            ..Settings::default()
        };
        let kept = run_job(&dir, &path, exact, (0.5, 2.0));
        assert!(color(&kept).contains("smpte2084"), "{}", color(&kept));
        assert!(color(&kept).contains("yuv420p10le"), "{}", color(&kept));

        if super::tools().unwrap().zscale {
            let mut sdr = Settings::default();
            sdr.keep.hdr = false;
            let converted = run_job(&dir, &path, sdr, (0.5, 2.0));
            assert!(
                !color(&converted).contains("smpte2084"),
                "{}",
                color(&converted)
            );
            assert!(
                color(&converted).contains("yuv420p,"),
                "{}",
                color(&converted)
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cancelling_leaves_nothing() {
        let dir = temp_dir("cancel");
        let Some(input) = sample(&dir) else {
            eprintln!("no ffmpeg: skipped");
            return;
        };
        let source = probe(&input).unwrap();
        let p = Progress::default();
        p.cancel_for_test();
        export(
            ClipJob {
                input,
                dir: dir.clone(),
                name: "gone".to_owned(),
                start: 0.0,
                end: source.duration,
                settings: Settings::default(),
                source,
                tools: super::tools().unwrap(),
            },
            &p,
        );
        assert!(p.take_outcome().is_none());
        let names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name())
            .collect();
        assert_eq!(names, [OsString::from("sample.mp4")]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
