//! Reads what a video holds with ffprobe, and exports it with ffmpeg.

use std::{
    collections::HashSet,
    ffi::{OsStr, OsString},
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Mutex, OnceLock, TryLockError,
        mpsc::{self, RecvTimeoutError},
    },
    time::Duration,
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::crop::{Crop, View};
use crate::{
    fs::unique_path,
    jobs::{Outcome, Progress},
};

/// Encoding takes all the processors there are, so exports wait their turn.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

/// How to say ffmpeg is missing.
pub const NOT_INSTALLED: &str = "ffmpeg isn't installed. Install it from your package manager or ffmpeg.org, then open this tab again.";

/// Below this a video is only smears: not worth an encode that would take as
/// long as a good one.
const MIN_VIDEO_BITRATE: f64 = 30_000.0;
/// The shorter side and frame rate of an animation unless asked otherwise.
pub const ANIMATION_SIDE: u32 = 480;
pub const ANIMATION_FPS: u32 = 15;
/// The smallest an animation is shrunk to in search of a target size.
const SMALLEST_ANIMATION_SIDE: f64 = 32.0;
/// How many encodes at a target size to try before saying it can't be done.
const MAX_ATTEMPTS: usize = 4;
/// Animations aim less exactly, having only their picture's size to change.
const MAX_ANIMATION_ATTEMPTS: usize = 6;
/// What's aimed at, of a target size: the rest is for the container, and for
/// encoders landing a little over.
const TARGET_MARGIN: f64 = 0.97;
/// Bits for each pixel of each frame, below which a video looks poor, and at
/// which it looks fine: enough for a guide, however much codecs differ.
const POOR_BPP: f64 = 0.02;
const FINE_BPP: f64 = 0.04;

/// An audio stream of the source.
#[derive(Clone, Debug, PartialEq)]
pub struct AudioStream {
    /// In the whole file, as `-map 0:N` means it.
    pub index: u64,
    codec: String,
    /// What to call it: its language, channels and so on.
    pub label: String,
}

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
    /// As shown, so a phone's upright video is taller than it's wide.
    width: u32,
    height: u32,
    fps: f64,
    color_primaries: Option<String>,
    color_transfer: Option<String>,
    color_space: Option<String>,
    pub audio: Vec<AudioStream>,
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

    pub fn fps(&self) -> f64 {
        self.fps
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// Its picture's width over its height.
    pub fn aspect(&self) -> f32 {
        self.width as f32 / self.height.max(1) as f32
    }
}

/// What this ffmpeg can do.
#[derive(Clone, Debug, Default)]
pub struct Tools {
    encoders: HashSet<String>,
    /// The filter converting HDR to SDR needs; only some builds have it.
    zscale: bool,
    /// Hardware encoders that made a video here just now. ffmpeg lists all it
    /// was built with, whatever the computer has.
    hardware: Vec<Hardware>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Hardware {
    encoder: String,
    /// The GPU's device, for VAAPI.
    device: Option<String>,
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

    /// A hardware encoder for this that works here.
    fn hardware(self, tools: &Tools) -> Option<&Hardware> {
        let prefix = match self {
            Self::H264 => "h264_",
            Self::Hevc => "hevc_",
            Self::Av1 => "av1_",
            Self::Vp9 => return None,
        };
        tools
            .hardware
            .iter()
            .find(|hardware| hardware.encoder.starts_with(prefix))
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
    Mp3,
    M4a,
    Opus,
    Flac,
    Wav,
    Gif,
    WebpAnimated,
}

impl Format {
    pub const ALL: [Self; 14] = [
        Self::Original,
        Self::Mp4H264,
        Self::Mp4Hevc,
        Self::Mp4Av1,
        Self::WebmVp9,
        Self::WebmAv1,
        Self::MkvH264,
        Self::Mp3,
        Self::M4a,
        Self::Opus,
        Self::Flac,
        Self::Wav,
        Self::Gif,
        Self::WebpAnimated,
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
            Self::Mp3 => "MP3",
            Self::M4a => "M4A (AAC)",
            Self::Opus => "Opus",
            Self::Flac => "FLAC (lossless)",
            Self::Wav => "WAV (lossless)",
            Self::Gif => "GIF",
            Self::WebpAnimated => "WebP",
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
            Self::Mp3
            | Self::M4a
            | Self::Opus
            | Self::Flac
            | Self::Wav
            | Self::Gif
            | Self::WebpAnimated => None,
        }
    }

    /// For audio on its own: the extension, the encoder and, if it has one,
    /// the usual bitrate in kbit/s.
    fn audio_only(self) -> Option<(&'static str, &'static str, Option<u32>)> {
        match self {
            Self::Mp3 => Some(("mp3", "libmp3lame", Some(192))),
            Self::M4a => Some(("m4a", "aac", Some(192))),
            Self::Opus => Some(("opus", "libopus", Some(128))),
            Self::Flac => Some(("flac", "flac", None)),
            Self::Wav => Some(("wav", "pcm_s16le", None)),
            _ => None,
        }
    }

    /// For a looping picture without sound: the extension and the encoder.
    fn animated(self) -> Option<(&'static str, &'static str)> {
        match self {
            Self::Gif => Some(("gif", "gif")),
            Self::WebpAnimated => Some(("webp", "libwebp_anim")),
            _ => None,
        }
    }

    pub fn is_animated(self) -> bool {
        self.animated().is_some()
    }

    pub fn is_audio_only(self) -> bool {
        self.audio_only().is_some()
    }

    /// Whether it has a bitrate to choose.
    pub fn is_lossless(self) -> bool {
        matches!(self, Self::Flac | Self::Wav)
    }

    /// Whether this ffmpeg can make it.
    pub fn available(self, tools: &Tools) -> bool {
        if let Some((_, encoder, _)) = self.audio_only() {
            return tools.encoders.contains(encoder);
        }
        if let Some((_, encoder)) = self.animated() {
            return tools.encoders.contains(encoder);
        }
        self.target()
            .is_none_or(|(_, codec)| codec.encoder(tools).is_some())
    }

    /// The codec it encodes video in, if that's known before seeing a source.
    fn codec(self) -> Option<Codec> {
        self.target().map(|(_, codec)| codec)
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

/// How far the picture is turned clockwise.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Turn {
    #[default]
    None,
    Quarter,
    Half,
    ThreeQuarters,
}

impl Turn {
    pub fn clockwise(self) -> Self {
        match self {
            Self::None => Self::Quarter,
            Self::Quarter => Self::Half,
            Self::Half => Self::ThreeQuarters,
            Self::ThreeQuarters => Self::None,
        }
    }

    pub fn counter_clockwise(self) -> Self {
        self.clockwise().clockwise().clockwise()
    }

    /// Its width and height trade places.
    pub fn swaps_sides(self) -> bool {
        matches!(self, Self::Quarter | Self::ThreeQuarters)
    }

    pub fn degrees(self) -> u32 {
        match self {
            Self::None => 0,
            Self::Quarter => 90,
            Self::Half => 180,
            Self::ThreeQuarters => 270,
        }
    }
}

/// Which audio to keep.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioChoice {
    #[default]
    All,
    None,
    /// The stream with this index in the file.
    One(u64),
}

/// What the editor asks for.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
// Tabs saved before a setting existed get its default.
#[serde(default)]
pub struct Settings {
    pub format: Format,
    pub quality: Quality,
    /// Re-encode to cut on the exact frame, rather than copy from a keyframe.
    pub exact: bool,
    pub keep: Keep,
    /// Aim for a file size rather than a quality.
    pub use_target: bool,
    /// In millions of bytes, so a limit in either kind of megabyte is met.
    pub target_mb: f64,
    /// The shorter side to shrink to, in pixels.
    pub resolution: Option<u32>,
    /// The frame rate to lower it to.
    pub fps: Option<u32>,
    pub audio: AudioChoice,
    /// `None` keeps it as it is where it can be, and picks otherwise.
    pub audio_kbps: Option<u32>,
    /// Encode on the GPU, where there's an encoder for it.
    pub hardware: bool,
    /// The part of the picture to keep.
    pub crop: Option<Crop>,
    /// The shape to keep the crop to, as width and height.
    pub crop_shape: Option<(u32, u32)>,
    pub turn: Turn,
    pub flip_horizontal: bool,
    pub flip_vertical: bool,
    /// How many times as fast it plays.
    pub speed: f64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            format: Format::default(),
            quality: Quality::default(),
            exact: false,
            keep: Keep::default(),
            use_target: false,
            target_mb: 25.0,
            resolution: None,
            fps: None,
            audio: AudioChoice::default(),
            audio_kbps: None,
            hardware: false,
            crop: None,
            crop_shape: None,
            turn: Turn::None,
            flip_horizontal: false,
            flip_vertical: false,
            speed: 1.0,
        }
    }
}

/// How an encode at a target size spends it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Fit {
    pub video_bps: f64,
    pub audio_kbps: u32,
    /// Bits for each pixel of each frame.
    bpp: f64,
}

impl Fit {
    pub fn looks_poor(&self) -> bool {
        self.bpp < POOR_BPP
    }
}

impl Settings {
    /// What a new tab starts from, after an export with these. Choices that
    /// leave something out of the file, or put it in, are made for each video.
    pub fn for_new_tab(mut self) -> Self {
        let fresh = Self::default();
        self.keep = fresh.keep;
        self.audio = fresh.audio;
        self.exact = fresh.exact;
        // These are about this video's picture.
        self.crop = fresh.crop;
        self.crop_shape = fresh.crop_shape;
        self.turn = fresh.turn;
        self.flip_horizontal = fresh.flip_horizontal;
        self.flip_vertical = fresh.flip_vertical;
        self.speed = fresh.speed;
        self
    }

    /// Whether the video can be copied as it is.
    pub fn copies(&self, source: &Source) -> bool {
        self.format == Format::Original
            && !self.exact
            && (!source.is_hdr() || self.keep.hdr)
            && !self.use_target
            && self.scale_to(source).is_none()
            && self.fps_to(source).is_none()
            && self.crop.is_none()
            && self.turn == Turn::None
            && !self.flip_horizontal
            && !self.flip_vertical
            && !self.changes_speed()
    }

    /// How the picture is shown, with what's been asked of it.
    pub fn view(&self) -> View {
        if self.format.is_audio_only() {
            return View {
                turn: Turn::None,
                flip_horizontal: false,
                flip_vertical: false,
            };
        }
        View {
            turn: self.turn,
            flip_horizontal: self.flip_horizontal,
            flip_vertical: self.flip_vertical,
        }
    }

    /// The crop's shape, width over height, as fractions of the original
    /// picture's. The crop comes before the turn, but its shape is what's
    /// wanted of the result, so a turn swaps it.
    pub fn crop_ratio(&self, source: &Source) -> Option<f32> {
        let (width, height) = self.crop_shape?;
        let (width, height) = if self.turn.swaps_sides() {
            (height, width)
        } else {
            (width, height)
        };
        Some(width as f32 / height as f32 / source.aspect())
    }

    pub fn changes_speed(&self) -> bool {
        (self.speed - 1.0).abs() > 0.001
    }

    /// The width and height of the picture after cropping and turning, before
    /// any shrinking.
    pub fn visible_size(&self, source: &Source) -> (u32, u32) {
        let (width, height) = match self.crop {
            Some(crop) => {
                let (width, height, _, _) = crop.pixels(source.width, source.height);
                (width, height)
            }
            None => (source.width, source.height),
        };
        if self.turn.swaps_sides() {
            (height, width)
        } else {
            (width, height)
        }
    }

    /// The shorter side of [`Self::visible_size`], which "1080p" means either
    /// way up.
    pub fn visible_short_side(&self, source: &Source) -> u32 {
        let (width, height) = self.visible_size(source);
        width.min(height)
    }

    /// Whether a hardware encoder could do this.
    pub fn hardware_available(&self, source: &Source, tools: &Tools, original: &str) -> bool {
        let codec = self
            .container(source, original)
            .ok()
            .and_then(|(_, codec)| codec);
        codec.is_some_and(|codec| codec.hardware(tools).is_some())
    }

    /// Whether something other than the exact cut makes it re-encode, so
    /// that it cuts on the frame anyway.
    pub fn encodes_anyway(&self, source: &Source) -> bool {
        !Self {
            exact: false,
            ..*self
        }
        .copies(source)
    }

    /// The size to aim under, in bytes.
    pub fn target_bytes(&self) -> Option<f64> {
        (self.use_target && !self.format.is_audio_only()).then_some(self.target_mb * 1e6)
    }

    /// The shorter side to shrink to: never larger than it is. An animation
    /// at full size would be enormous, so it's small unless asked otherwise.
    fn scale_to(&self, source: &Source) -> Option<u32> {
        let wanted = if self.format.is_audio_only() {
            None
        } else if self.format.is_animated() {
            Some(self.resolution.unwrap_or(ANIMATION_SIDE))
        } else {
            self.resolution
        };
        wanted.filter(|side| *side < self.visible_short_side(source))
    }

    /// The frame rate to lower to: never higher than it is.
    fn fps_to(&self, source: &Source) -> Option<u32> {
        let wanted = if self.format.is_audio_only() {
            None
        } else if self.format.is_animated() {
            Some(self.fps.unwrap_or(ANIMATION_FPS))
        } else {
            self.fps
        };
        wanted.filter(|fps| f64::from(*fps) < source.fps - 0.5)
    }

    /// The pixels of each frame it comes out with, and its frames each second.
    fn output_rate(&self, source: &Source) -> (f64, f64) {
        let (width, height) = self.visible_size(source);
        let scale = self.scale_to(source).map_or(1.0, |side| {
            f64::from(side) / f64::from(self.visible_short_side(source))
        });
        let fps = self.fps_to(source).map_or(source.fps, f64::from);
        (f64::from(width) * f64::from(height) * scale * scale, fps)
    }

    /// The audio streams that go in.
    fn audio_streams<'a>(&self, source: &'a Source) -> Vec<&'a AudioStream> {
        let first = source.audio.iter().take(1);
        match self.audio {
            // A file of audio has room for one, and always has it.
            AudioChoice::All | AudioChoice::None if self.format.is_audio_only() => first.collect(),
            AudioChoice::All => source.audio.iter().collect(),
            AudioChoice::None => Vec::new(),
            AudioChoice::One(index) => source.audio.iter().filter(|a| a.index == index).collect(),
        }
    }

    /// How `length` seconds fit the target size; an error says if they can't.
    pub fn fit(&self, source: &Source, length: f64) -> Result<Fit, String> {
        let bytes = self.target_bytes().unwrap_or(0.0);
        // What matters is how long it plays for.
        let length = (length / self.speed).max(0.001);
        let budget = bytes * 8.0 * TARGET_MARGIN / length;
        let tracks = self.audio_streams(source).len() as f64;
        // The audio gets a little of what there is, within what still sounds
        // right: at a target size, its original could be most of the file.
        let audio_kbps = self.audio_kbps.unwrap_or_else(|| {
            let share = budget * 0.15 / tracks.max(1.0) / 1000.0;
            share.clamp(48.0, 128.0) as u32
        });
        let audio_bps = f64::from(audio_kbps) * 1000.0 * tracks;
        let video_bps = budget - audio_bps;
        if video_bps < MIN_VIDEO_BITRATE {
            let needed = (MIN_VIDEO_BITRATE + audio_bps) * length / 8.0 / TARGET_MARGIN / 1e6;
            return Err(format!(
                "{} MB is too small for this much video. It needs at least {:.1} MB, or less audio.",
                self.target_mb, needed
            ));
        }
        let (pixels, fps) = self.output_rate(source);
        Ok(Fit {
            video_bps,
            audio_kbps,
            bpp: video_bps / (pixels * fps).max(1.0),
        })
    }

    /// A shorter side to shrink to, if at this target size it would look fine.
    pub fn suggested_resolution(&self, source: &Source, fit: &Fit) -> Option<u32> {
        let (pixels, fps) = self.output_rate(source);
        let visible = self.visible_short_side(source);
        let side = f64::from(self.scale_to(source).unwrap_or(visible));
        // Pixels go by the square of the side.
        let fine = side * (fit.video_bps / (pixels * fps).max(1.0) / FINE_BPP).sqrt();
        super::tab::RESOLUTIONS
            .iter()
            .copied()
            .find(|preset| f64::from(*preset) <= fine && f64::from(*preset) < side)
    }

    /// The output's extension, for a source with the extension `original`.
    pub fn extension(&self, source: &Source, original: &str) -> Result<String, String> {
        Ok(self.container(source, original)?.0)
    }

    /// The container and, when re-encoding, the codec. An original that can't
    /// be made again as it is (mpeg4 in an .avi, say) becomes H.264 in an MP4.
    fn container(
        &self,
        source: &Source,
        original: &str,
    ) -> Result<(String, Option<Codec>), String> {
        let original = original.to_ascii_lowercase();
        if let Some((extension, _, _)) = self.format.audio_only() {
            return Ok((extension.to_owned(), None));
        }
        if let Some((extension, _)) = self.format.animated() {
            return Ok((extension.to_owned(), None));
        }
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
        Err(first_error(&String::from_utf8_lossy(&output.stderr)))
    }
}

/// What went wrong, from ffmpeg's errors: the first says why, and the ones
/// after only what that led to. Without the `[h264_vaapi @ 0x55…]` it may start
/// with.
fn first_error(text: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("ffmpeg failed");
    line.strip_prefix('[')
        .and_then(|rest| rest.split_once("] "))
        .filter(|(tag, _)| tag.contains(" @ "))
        .map_or(line, |(_, message)| message)
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
        hardware: working_hardware(&parse_names(&encoders)),
        encoders: parse_names(&encoders),
        zscale: parse_names(&filters).contains("zscale"),
    })
}

/// The hardware encoders that work here. They're tried once for the whole run
/// of the app, as each takes a moment.
fn working_hardware(encoders: &HashSet<String>) -> Vec<Hardware> {
    static WORKING: OnceLock<Vec<Hardware>> = OnceLock::new();
    WORKING
        .get_or_init(|| {
            let mut working = Vec::new();
            for codec in ["h264", "hevc", "av1"] {
                for kind in ["nvenc", "qsv", "vaapi"] {
                    let encoder = format!("{codec}_{kind}");
                    if !encoders.contains(&encoder) {
                        continue;
                    }
                    let devices = if kind == "vaapi" {
                        render_nodes().into_iter().map(Some).collect()
                    } else {
                        vec![None]
                    };
                    let found = devices
                        .into_iter()
                        .map(|device| Hardware {
                            encoder: encoder.clone(),
                            device,
                        })
                        .find(hardware_works);
                    working.extend(found);
                }
            }
            working
        })
        .clone()
}

/// The GPUs VAAPI can use.
fn render_nodes() -> Vec<String> {
    let Ok(entries) = disk::read_dir("/dev/dri") else {
        return Vec::new();
    };
    let mut nodes: Vec<String> = entries
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("renderD"))
        .map(|name| format!("/dev/dri/{name}"))
        .collect();
    nodes.sort();
    nodes
}

/// Encodes a moment of black, giving up on a driver that hangs.
fn hardware_works(hardware: &Hardware) -> bool {
    let mut ffmpeg = command("ffmpeg");
    ffmpeg.args(["-hide_banner", "-v", "quiet", "-nostdin"]);
    if let Some(device) = &hardware.device {
        ffmpeg.args(["-vaapi_device", device]);
    }
    ffmpeg.args(["-f", "lavfi", "-i", "color=black:s=1280x720:d=0.2:r=30"]);
    if hardware.device.is_some() {
        ffmpeg.args(["-vf", "format=nv12,hwupload"]);
    }
    ffmpeg
        .args(["-c:v", &hardware.encoder, "-f", "null", "-"])
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let Ok(mut child) = ffmpeg.spawn() else {
        return false;
    };
    for _ in 0..100 {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(_) => break,
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    false
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
                let (width, height) = (side("width"), side("height"));
                // Phones film sideways and say which way up it goes.
                let turned = rotation(stream).rem_euclid(180) == 90;
                source.width = if turned { height } else { width };
                source.height = if turned { width } else { height };
                source.fps = stream
                    .get("avg_frame_rate")
                    .and_then(Value::as_str)
                    .and_then(parse_rate)
                    .unwrap_or(30.0);
                source.color_primaries = text_of(stream, "color_primaries");
                source.color_transfer = text_of(stream, "color_transfer");
                source.color_space = text_of(stream, "color_space");
            }
            Some("audio") => {
                let label = audio_label(stream, &codec, source.audio.len() + 1);
                source.audio.push(AudioStream {
                    index,
                    codec,
                    label,
                });
            }
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

/// The degrees a video is turned when shown.
fn rotation(stream: &Value) -> i64 {
    stream
        .get("side_data_list")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find_map(|data| data.get("rotation").and_then(Value::as_i64))
        .unwrap_or(0)
}

/// What to call an audio stream, among those of a file.
fn audio_label(stream: &Value, codec: &str, number: usize) -> String {
    let tag = |key| stream.pointer(key).and_then(Value::as_str);
    let mut parts = vec![format!("Track {number}")];
    parts.extend(tag("/tags/title").map(str::to_owned));
    parts.extend(
        tag("/tags/language")
            .filter(|l| *l != "und")
            .map(str::to_owned),
    );
    parts.push(codec.to_owned());
    if let Some(channels) = stream.get("channels").and_then(Value::as_u64) {
        parts.push(format!("{channels} ch"));
    }
    parts.join(", ")
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

/// What one run of ffmpeg is for.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Encode {
    /// For a target size: the video's bits a second, and the audio's kbit/s.
    rate: Option<(f64, u32)>,
    pass: Pass,
    /// For an animation at a target size: the shorter side to shrink to.
    side: Option<u32>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Pass {
    #[default]
    Only,
    /// Reads the video to see where the bits are best spent.
    First,
    Second,
}

/// Whether ffmpeg can run this encoder twice.
fn has_two_passes(encoder: &str) -> bool {
    matches!(encoder, "libx264" | "libx265" | "libvpx-vp9" | "libaom-av1")
}

/// The encoder a job uses for `codec`.
fn encoder_for(job: &ClipJob, codec: Codec) -> Result<Hardware, String> {
    if job.settings.hardware
        && let Some(hardware) = codec.hardware(&job.tools)
    {
        return Ok(hardware.clone());
    }
    let encoder = codec
        .encoder(&job.tools)
        .ok_or_else(|| format!("This ffmpeg has no encoder for {codec:?}."))?;
    Ok(Hardware {
        encoder: encoder.to_owned(),
        device: None,
    })
}

/// ffmpeg's arguments to encode the video.
fn video_args(
    encoder: &str,
    codec: Codec,
    extension: &str,
    job: &ClipJob,
    tone_map: bool,
    encode: Encode,
) -> Vec<String> {
    let quality = job.settings.quality;
    let mut args: Vec<String> = vec!["-c:v".into(), encoder.into()];
    let mut add = |more: &[&str]| args.extend(more.iter().map(|&arg| arg.to_owned()));
    let crf =
        |high: u32, balanced: u32, small: u32| quality.pick(high, balanced, small).to_string();
    let rate = encode.rate.map(|(video, _)| (video as u64).to_string());
    // For encoders that keep to a bitrate only if told how closely.
    let cap = encode
        .rate
        .map(|(video, _)| ((video * 1.5) as u64).to_string());
    match encoder {
        "libx264" => {
            add(&["-preset", "medium"]);
            match &rate {
                Some(rate) => add(&["-b:v", rate]),
                None => add(&["-crf", &crf(18, 23, 28)]),
            }
        }
        "libx265" => {
            add(&["-preset", "medium"]);
            match &rate {
                Some(rate) => add(&["-b:v", rate]),
                None => add(&["-crf", &crf(20, 26, 31)]),
            }
            // x265 takes its passes this way, and writes to the terminal.
            let params = match encode.pass {
                Pass::Only => "log-level=error".to_owned(),
                Pass::First => format!("log-level=error:pass=1:stats={STATS}.log"),
                Pass::Second => format!("log-level=error:pass=2:stats={STATS}.log"),
            };
            add(&["-x265-params", &params]);
        }
        "libsvtav1" => {
            add(&["-preset", "8"]);
            match &rate {
                Some(rate) => add(&["-b:v", rate]),
                None => add(&["-crf", &crf(24, 32, 42)]),
            }
        }
        "libaom-av1" => {
            add(&["-cpu-used", "6", "-row-mt", "1"]);
            match &rate {
                Some(rate) => add(&["-b:v", rate]),
                None => add(&["-crf", &crf(24, 32, 42), "-b:v", "0"]),
            }
        }
        "libvpx-vp9" => {
            add(&["-deadline", "good", "-cpu-used", "3", "-row-mt", "1"]);
            match &rate {
                Some(rate) => add(&["-b:v", rate]),
                None => add(&["-crf", &crf(24, 32, 40), "-b:v", "0"]),
            }
        }
        "libopenh264" => {
            // OpenH264 is the one H.264 encoder LGPL builds have, and takes
            // only a bitrate: so many bits for each pixel of each frame.
            let (pixels, fps) = job.settings.output_rate(&job.source);
            let bitrate = (pixels * fps * quality.pick(0.15, 0.08, 0.04)) as u64;
            add(&["-b:v", rate.as_deref().unwrap_or(&bitrate.to_string())]);
        }
        hardware => match (&rate, &cap) {
            (Some(rate), Some(cap)) => {
                add(&["-b:v", rate, "-maxrate", cap]);
                if hardware.ends_with("_vaapi") {
                    add(&["-rc_mode", "VBR"]);
                }
            }
            _ if hardware.ends_with("_nvenc") => add(&["-rc", "vbr", "-cq", &crf(19, 25, 31)]),
            _ if hardware.ends_with("_vaapi") => {
                add(&["-rc_mode", "CQP", "-global_quality", &crf(19, 25, 31)]);
            }
            _ => add(&["-global_quality", &crf(19, 25, 31)]),
        },
    }
    if encoder != "libx265" && has_two_passes(encoder) {
        match encode.pass {
            Pass::Only => {}
            Pass::First => add(&["-pass", "1", "-passlogfile", STATS]),
            Pass::Second => add(&["-pass", "2", "-passlogfile", STATS]),
        }
    }
    if codec == Codec::Hevc && is_mov(extension) {
        // Apple's players want this tag for H.265.
        add(&["-tag:v", "hvc1"]);
    }
    let source = &job.source;
    let ten_bit = source.is_hdr() && !tone_map;
    // VAAPI has its pictures in the GPU by now, as the filters left them.
    if !encoder.ends_with("_vaapi") {
        let hardware = !encoder.starts_with("lib");
        add(&[
            "-pix_fmt",
            match (ten_bit, hardware) {
                (true, true) => "p010le",
                (true, false) => "yuv420p10le",
                (false, true) => "nv12",
                // What every player can show.
                (false, false) => "yuv420p",
            },
        ]);
    }
    if ten_bit {
        for (flag, value) in [
            ("-color_primaries", &source.color_primaries),
            ("-color_trc", &source.color_transfer),
            ("-colorspace", &source.color_space),
        ] {
            if let Some(value) = value {
                add(&[flag, value]);
            }
        }
    }
    args
}

/// The name a two-pass encode's notes go by, in a folder of their own.
const STATS: &str = "pass";

/// The filters a re-encode runs the video through: what's kept of the picture,
/// then which way up it goes, then how big, then how fast.
fn video_filters(
    job: &ClipJob,
    encode: Encode,
    tone_map: bool,
    ten_bit: bool,
    vaapi: bool,
) -> Option<String> {
    let ClipJob {
        source, settings, ..
    } = job;
    let mut filters: Vec<String> = Vec::new();
    if tone_map {
        filters.push(TONE_MAP.to_owned());
    }
    if let Some(crop) = settings.crop {
        let (width, height, x, y) = crop.pixels(source.width, source.height);
        filters.push(format!("crop={width}:{height}:{x}:{y}"));
    }
    match settings.turn {
        Turn::None => {}
        Turn::Quarter => filters.push("transpose=clock".to_owned()),
        Turn::Half => filters.push("hflip,vflip".to_owned()),
        Turn::ThreeQuarters => filters.push("transpose=cclock".to_owned()),
    }
    if settings.flip_horizontal {
        filters.push("hflip".to_owned());
    }
    if settings.flip_vertical {
        filters.push("vflip".to_owned());
    }
    if let Some(side) = encode.side.or_else(|| settings.scale_to(source)) {
        let (width, height) = settings.visible_size(source);
        filters.push(if width >= height {
            format!("scale=-2:{side}:flags=lanczos")
        } else {
            format!("scale={side}:-2:flags=lanczos")
        });
    }
    if settings.changes_speed() {
        filters.push(format!("setpts=PTS/{}", settings.speed));
    }
    if let Some(fps) = settings.fps_to(source) {
        filters.push(format!("fps={fps}"));
    } else if settings.changes_speed() {
        // Or it would change along with the speed.
        filters.push(format!("fps={:.3}", source.fps));
    }
    if vaapi {
        // Last, as the others work on pictures in memory.
        filters.push(format!(
            "format={},hwupload",
            if ten_bit { "p010" } else { "nv12" }
        ));
    }
    if settings.format == Format::Gif {
        // The best 256 colors for this video, not some general ones.
        let dither =
            settings
                .quality
                .pick("sierra2_4a", "bayer:bayer_scale=3", "bayer:bayer_scale=1");
        filters.push(format!(
            "split[a][b];[a]palettegen=stats_mode=diff[p];[b][p]paletteuse=dither={dither}"
        ));
    }
    (!filters.is_empty()).then(|| filters.join(","))
}

/// `atempo` filters for a speed: each only goes from half to double.
fn tempo_filters(mut speed: f64) -> String {
    let mut filters = Vec::new();
    while speed > 2.0 {
        filters.push("atempo=2".to_owned());
        speed /= 2.0;
    }
    while speed < 0.5 {
        filters.push("atempo=0.5".to_owned());
        speed /= 0.5;
    }
    filters.push(format!("atempo={speed}"));
    filters.join(",")
}

const NO_ZIMG: &str = "Converting HDR to SDR needs an ffmpeg with zimg. Keep HDR, in a format that can, or install another build.";

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
fn plan(
    job: &ClipJob,
    extension: &str,
    output: &Path,
    encode: Encode,
) -> Result<Vec<OsString>, String> {
    let ClipJob {
        source, settings, ..
    } = job;
    let (_, codec) = settings.container(source, extension)?;
    let audio_only = settings.format.audio_only();
    let keep = settings.keep;
    let hardware = codec.map(|codec| encoder_for(job, codec)).transpose()?;
    let mut args: Vec<OsString> = Vec::new();
    let mut add = |more: &[&str]| args.extend(more.iter().map(OsString::from));
    add(&["-hide_banner", "-v", "error", "-nostdin", "-nostats", "-n"]);
    add(&["-progress", "pipe:1"]);
    if let Some(device) = hardware.as_ref().and_then(|h| h.device.as_deref()) {
        add(&["-vaapi_device", device]);
    }
    let animated = settings.format.animated();
    let copying_video = codec.is_none() && audio_only.is_none() && animated.is_none();
    let seek = job.start
        + if copying_video {
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

    let streams = if animated.is_some() {
        Vec::new()
    } else {
        settings.audio_streams(source)
    };
    if audio_only.is_none() {
        add(&["-map", &format!("0:{}", source.video_index)]);
    } else if streams.is_empty() {
        return Err("This video has no audio.".to_owned());
    }
    if encode.pass != Pass::First {
        for stream in &streams {
            add(&["-map", &format!("0:{}", stream.index)]);
        }
    }
    let mut subtitle_args = Vec::new();
    // Subtitles can't follow a change of speed: they'd be at the wrong times.
    let keeps_subtitles = keep.subtitles && !settings.changes_speed();
    if keeps_subtitles && audio_only.is_none() && animated.is_none() && encode.pass != Pass::First {
        let mut mapped = 0;
        for subtitle in &source.subtitles {
            let encoder = if copying_video {
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

    if let Some((_, encoder)) = animated {
        // Neither has room for HDR.
        let tone_map = source.is_hdr();
        if tone_map && !job.tools.zscale {
            return Err(NO_ZIMG.to_owned());
        }
        if let Some(filters) = video_filters(job, encode, tone_map, false, false) {
            add(&["-vf", &filters]);
        }
        add(&["-c:v", encoder, "-an", "-sn", "-loop", "0"]);
        if settings.format == Format::WebpAnimated {
            let quality = settings.quality.pick("85", "70", "50");
            add(&["-quality", quality]);
        }
        add(&["-map_metadata", "-1", "-map_chapters", "-1"]);
        args.push(output.into());
        return Ok(args);
    }

    if let (Some(codec), Some(hardware)) = (codec, &hardware) {
        // H.264 here is 8-bit, which can't hold HDR.
        let tone_map = source.is_hdr() && (!keep.hdr || codec == Codec::H264);
        if tone_map && !job.tools.zscale {
            return Err(NO_ZIMG.to_owned());
        }
        let ten_bit = source.is_hdr() && !tone_map;
        let vaapi = hardware.device.is_some();
        if let Some(filters) = video_filters(job, encode, tone_map, ten_bit, vaapi) {
            add(&["-vf", &filters]);
        }
        let video = video_args(&hardware.encoder, codec, extension, job, tone_map, encode);
        let video: Vec<&str> = video.iter().map(String::as_str).collect();
        add(&video);
    } else if copying_video {
        add(&["-c:v", "copy"]);
    }

    if encode.pass == Pass::First {
        // Only the notes are wanted of this one.
        add(&["-an", "-sn", "-f", "null"]);
        args.push("-".into());
        return Ok(args);
    }

    let tempo = settings
        .changes_speed()
        .then(|| tempo_filters(settings.speed));
    if let Some(tempo) = &tempo
        && !streams.is_empty()
    {
        add(&["-af", tempo]);
    }
    let kbps = encode.rate.map(|(_, audio)| audio).or(settings.audio_kbps);
    if let Some((_, encoder, usual)) = audio_only {
        add(&["-vn", "-c:a", encoder]);
        if let Some(kbps) = usual.and(settings.audio_kbps).or(usual) {
            add(&["-b:a", &format!("{kbps}k")]);
        }
    } else if streams.is_empty() {
        add(&["-an"]);
    } else if let Some(kbps) = kbps {
        let encoder = if extension == "webm" {
            "libopus"
        } else {
            "aac"
        };
        add(&["-c:a", encoder, "-b:a", &format!("{kbps}k")]);
    } else if tempo.is_none()
        && (copying_video || streams.iter().all(|a| audio_fits(extension, &a.codec)))
    {
        add(&["-c:a", "copy"]);
    } else if extension == "webm" {
        add(&["-c:a", "libopus", "-b:a", "160k"]);
    } else {
        add(&["-c:a", "aac", "-b:a", "192k"]);
    }

    add(&["-map_chapters", if keeps_subtitles { "0" } else { "-1" }]);
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
    p.set_time_total((job.end - job.start) / job.settings.speed);
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
    let length = job.end - job.start;
    p.set_time_total(length / job.settings.speed);

    let dir = reachable(&job.dir);
    let temporary = unique_path(&dir, OsStr::new(&format!(".{}.part.{extension}", job.name)));
    let result = make(&job, &extension, &temporary, length, p).map_err(|err| {
        if job.settings.hardware
            && job
                .settings
                .hardware_available(&job.source, &job.tools, &original)
        {
            format!("{err} Try without the hardware encoder.")
        } else {
            err
        }
    });
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

/// Writes the output to `temporary`: at a target size, as many times as it
/// takes to fit.
fn make(
    job: &ClipJob,
    extension: &str,
    temporary: &Path,
    length: f64,
    p: &Progress,
) -> Result<(), String> {
    let Some(limit) = job.settings.target_bytes() else {
        let args = plan(job, extension, temporary, Encode::default())?;
        return encode(&args, None, 0.0, p);
    };
    if job.settings.format.is_animated() {
        return make_animation(job, extension, temporary, limit, p);
    }
    let fit = job.settings.fit(&job.source, length)?;
    // From here on, how long it plays for.
    let length = length / job.settings.speed;
    let codec = job.settings.format.codec().or_else(|| {
        let (_, codec) = job.settings.container(&job.source, extension).ok()?;
        codec
    });
    let encoder = match codec {
        Some(codec) => encoder_for(job, codec)?.encoder,
        None => return Err("Can't aim for a size without encoding.".to_owned()),
    };
    let mut video_bps = fit.video_bps;
    let rate = |video_bps| Some((video_bps, fit.audio_kbps));
    let two_passes = has_two_passes(&encoder);
    let notes = if two_passes {
        Some(Notes::new().map_err(|err| format!("Can't make a temporary folder: {err}"))?)
    } else {
        None
    };
    let cwd = notes.as_ref().map(|notes| notes.dir.as_path());
    let mut base = 0.0;
    if two_passes {
        p.set_time_total(length * 2.0);
        p.set_status(&format!("{}: reading the video", job.name));
        let first = Encode {
            rate: rate(video_bps),
            pass: Pass::First,
            side: None,
        };
        encode(&plan(job, extension, temporary, first)?, cwd, 0.0, p)?;
        base = length;
    }
    let pass = if two_passes { Pass::Second } else { Pass::Only };
    for attempt in 0..MAX_ATTEMPTS {
        if p.cancelled() {
            return Ok(());
        }
        // ffmpeg won't write over a try that came out too big.
        let _ = disk::remove_file(temporary);
        p.set_status(&if attempt == 0 {
            format!("{}: encoding", job.name)
        } else {
            format!("{}: was too big, encoding smaller", job.name)
        });
        let this = Encode {
            rate: rate(video_bps),
            pass,
            side: None,
        };
        encode(&plan(job, extension, temporary, this)?, cwd, base, p)?;
        if p.cancelled() {
            return Ok(());
        }
        let size = disk::metadata(temporary)
            .map_err(|err| err.to_string())?
            .len() as f64;
        if size <= limit {
            return Ok(());
        }
        video_bps = smaller_bitrate(video_bps, size, limit, length);
        if video_bps < MIN_VIDEO_BITRATE {
            break;
        }
    }
    Err(format!(
        "Couldn't get it under {} MB. Try a lower resolution or frame rate.",
        job.settings.target_mb
    ))
}

/// Writes an animation small enough for `limit` bytes. These have no bitrate to
/// ask for, but their size goes with their picture's: shrinks it till it fits.
fn make_animation(
    job: &ClipJob,
    extension: &str,
    temporary: &Path,
    limit: f64,
    p: &Progress,
) -> Result<(), String> {
    let settings = &job.settings;
    let mut side = None;
    for attempt in 0..MAX_ANIMATION_ATTEMPTS {
        if p.cancelled() {
            return Ok(());
        }
        let _ = disk::remove_file(temporary);
        p.set_status(&if attempt == 0 {
            format!("{}: encoding", job.name)
        } else {
            format!("{}: was too big, trying it smaller", job.name)
        });
        let this = Encode {
            side,
            ..Encode::default()
        };
        encode(&plan(job, extension, temporary, this)?, None, 0.0, p)?;
        if p.cancelled() {
            return Ok(());
        }
        let size = disk::metadata(temporary)
            .map_err(|err| err.to_string())?
            .len() as f64;
        if size <= limit {
            return Ok(());
        }
        let shown = side
            .or_else(|| settings.scale_to(&job.source))
            .unwrap_or_else(|| settings.visible_short_side(&job.source));
        let next = smaller_side(f64::from(shown), size, limit);
        if next < SMALLEST_ANIMATION_SIDE {
            break;
        }
        side = Some(next as u32);
    }
    Err(format!(
        "Couldn't get it under {} MB. Try a shorter clip or a lower frame rate.",
        settings.target_mb
    ))
}

/// The side to try after `side` made `size` bytes against a `limit`: the size
/// goes with the area, so with the side's square. Even, for the encoders, and
/// always smaller than it was.
fn smaller_side(side: f64, size: f64, limit: f64) -> f64 {
    let wanted = side * (limit * TARGET_MARGIN / size).sqrt();
    ((wanted / 2.0).floor() * 2.0).min(side - 2.0)
}

/// The video bitrate to try after `bitrate` made `size` bytes against a `limit`:
/// less by what was over, and by the margin, but at most half as much.
fn smaller_bitrate(bitrate: f64, size: f64, limit: f64, length: f64) -> f64 {
    let over = (size - limit * TARGET_MARGIN) * 8.0 / length.max(0.001);
    (bitrate - over).max(bitrate / 2.0)
}

/// A folder of its own for a two-pass encode's notes, which encoders can only
/// be given a name for, and gone when this is.
struct Notes {
    dir: PathBuf,
}

impl Notes {
    fn new() -> std::io::Result<Self> {
        let name = format!("egui-files-clip-{}", disk::process_id());
        let dir = unique_path(&std::env::temp_dir(), OsStr::new(&name));
        disk::create_dir(&dir)?;
        Ok(Self { dir })
    }
}

impl Drop for Notes {
    fn drop(&mut self) {
        if let Ok(entries) = disk::read_dir(&self.dir) {
            for entry in entries.flatten() {
                let _ = disk::remove_file(entry.path());
            }
        }
        let _ = disk::remove_dir(&self.dir);
    }
}

/// Runs ffmpeg with `args`, in `cwd` if given, showing its progress from `base`
/// seconds on, until it ends or the job is cancelled.
fn encode(args: &[OsString], cwd: Option<&Path>, base: f64, p: &Progress) -> Result<(), String> {
    let mut ffmpeg = command("ffmpeg");
    if let Some(cwd) = cwd {
        ffmpeg.current_dir(cwd);
    }
    let mut child = ffmpeg
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
                    p.set_time_done(base + seconds);
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
    Err(first_error(&text))
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
            hardware: Vec::new(),
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
        plan(job, extension, Path::new("/in/out"), Encode::default())
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
        assert!(plan(&job, "mp4", Path::new("/out"), Encode::default()).is_err());
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

    #[test]
    fn upright_videos_are_measured_upright() {
        let turned = r#"{"streams": [{"codec_type": "video", "width": 1920, "height": 1080,
            "side_data_list": [{"rotation": -90}]}], "format": {"duration": "9"}}"#;
        let source = parse_probe(turned).unwrap();
        assert_eq!((source.width, source.height), (1080, 1920));
        assert_eq!(Settings::default().visible_short_side(&source), 1080);
        let mut job = job(Settings {
            resolution: Some(720),
            ..Settings::default()
        });
        job.source = source;
        let args = planned(&job, "mkv");
        assert!(has(&args, "-vf", "scale=720:-2:flags=lanczos"));
    }

    #[test]
    fn never_scales_up_or_adds_frames() {
        let source = parse_probe(PROBED).unwrap();
        let settings = Settings {
            resolution: Some(4320),
            fps: Some(60),
            ..Settings::default()
        };
        assert!(settings.copies(&source), "nothing to change");
        let settings = Settings {
            resolution: Some(1080),
            fps: Some(24),
            ..Settings::default()
        };
        assert!(!settings.copies(&source));
        let args = planned(&job(settings), "mkv");
        assert!(has(&args, "-vf", "scale=-2:1080:flags=lanczos,fps=24"));
    }

    #[test]
    fn audio_goes_as_asked_without_touching_the_video() {
        let mut settings = Settings {
            audio: AudioChoice::None,
            ..Settings::default()
        };
        assert!(settings.copies(&parse_probe(PROBED).unwrap()));
        let args = planned(&job(settings), "mkv");
        assert!(has(&args, "-c:v", "copy"));
        assert!(args.iter().any(|arg| arg == "-an"));
        assert!(!has(&args, "-map", "0:2"));

        settings.audio = AudioChoice::One(2);
        settings.audio_kbps = Some(96);
        let args = planned(&job(settings), "mkv");
        assert!(has(&args, "-map", "0:2"));
        assert!(has(&args, "-c:v", "copy"));
        assert!(has(&args, "-c:a", "aac") && has(&args, "-b:a", "96k"));
    }

    #[test]
    fn audio_alone_leaves_the_rest_out() {
        let settings = Settings {
            format: Format::Mp3,
            audio: AudioChoice::None,
            ..Settings::default()
        };
        let source = parse_probe(PROBED).unwrap();
        assert_eq!(settings.extension(&source, "mkv").unwrap(), "mp3");
        let mut job = job(settings);
        job.tools.encoders.insert("libmp3lame".to_owned());
        let args = planned(&job, "mp3");
        assert!(args.iter().any(|arg| arg == "-vn"));
        assert!(has(&args, "-map", "0:2"), "its one track, asked or not");
        assert!(!has(&args, "-map", "0:1") && !has(&args, "-map", "0:4"));
        assert!(has(&args, "-c:a", "libmp3lame") && has(&args, "-b:a", "192k"));

        job.source.audio.clear();
        assert!(plan(&job, "mp3", Path::new("/out"), Encode::default()).is_err());
    }

    #[test]
    fn a_target_size_is_shared_out_or_refused() {
        let source = parse_probe(PROBED).unwrap();
        let settings = Settings {
            use_target: true,
            target_mb: 25.0,
            ..Settings::default()
        };
        assert!(!settings.copies(&source));
        let fit = settings.fit(&source, 100.0).unwrap();
        // 25 MB over 100 s is 2 Mbit/s, less the margin.
        assert_eq!(fit.audio_kbps, 128);
        assert!((fit.video_bps - (1_940_000.0 - 128_000.0)).abs() < 1.0);
        assert!(fit.looks_poor(), "4K at under 2 Mbit/s");
        let side = settings.suggested_resolution(&source, &fit).unwrap();
        assert!(side < 2160, "{side}");

        // Too little even for smears.
        let tiny = Settings {
            target_mb: 0.1,
            ..settings
        };
        let err = tiny.fit(&source, 600.0).unwrap_err();
        assert!(err.contains("at least"), "{err}");
        // Audio gets less when there's little to go round, down to a point.
        let small = Settings {
            target_mb: 1.0,
            ..settings
        };
        assert_eq!(small.fit(&source, 60.0).unwrap().audio_kbps, 48);
    }

    #[test]
    fn each_encoder_takes_two_passes_its_own_way() {
        let settings = Settings {
            exact: true,
            use_target: true,
            format: Format::Mp4H264,
            ..Settings::default()
        };
        let encode = |pass| Encode {
            rate: Some((1_000_000.0, 96)),
            pass,
            side: None,
        };
        let plain = |job: &ClipJob, pass| -> Vec<String> {
            plan(job, "mp4", Path::new("/out"), encode(pass))
                .unwrap()
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect()
        };
        let job_264 = job(settings);
        let first = plain(&job_264, Pass::First);
        assert!(has(&first, "-pass", "1") && has(&first, "-b:v", "1000000"));
        assert!(first.iter().any(|arg| arg == "-an"));
        assert!(has(&first, "-f", "null"));
        assert!(!has(&first, "-map", "0:2"), "no audio to read");
        let second = plain(&job_264, Pass::Second);
        assert!(has(&second, "-pass", "2") && has(&second, "-b:a", "96k"));
        assert!(has(&second, "-map", "0:2"));

        let job_265 = job(Settings {
            format: Format::Mp4Hevc,
            ..settings
        });
        let first = plain(&job_265, Pass::First);
        assert!(!first.iter().any(|arg| arg == "-pass"));
        assert!(has(
            &first,
            "-x265-params",
            "log-level=error:pass=1:stats=pass.log"
        ));

        assert!(!has_two_passes("libsvtav1") && !has_two_passes("libopenh264"));
        assert!(!has_two_passes("h264_vaapi"));
    }

    #[test]
    fn errors_say_the_first_thing_that_went_wrong() {
        let errors = "[h264_vaapi @ 0x56] Hardware does not support encoding at size 160x96\n\
                      [vost#0:0/h264_vaapi @ 0x57] Error while opening encoder\n\
                      Conversion failed!\n";
        assert_eq!(
            first_error(errors),
            "Hardware does not support encoding at size 160x96"
        );
        assert_eq!(
            first_error("\nb.mp4: No such file\n"),
            "b.mp4: No such file"
        );
        assert_eq!(first_error(""), "ffmpeg failed");
        assert_eq!(first_error("[not a tag] stays"), "[not a tag] stays");
    }

    #[test]
    fn the_picture_changes_in_the_right_order() {
        let settings = Settings {
            crop: Some(Crop {
                x: 0.5,
                y: 0.0,
                w: 0.5,
                h: 1.0,
            }),
            turn: Turn::Quarter,
            flip_horizontal: true,
            resolution: Some(720),
            speed: 2.0,
            ..Settings::default()
        };
        let source = parse_probe(PROBED).unwrap();
        assert!(!settings.copies(&source));
        // 1920 by 2160, on its side.
        assert_eq!(settings.visible_size(&source), (2160, 1920));
        let args = planned(&job(settings), "mkv");
        assert!(has(
            &args,
            "-vf",
            "crop=1920:2160:1920:0,transpose=clock,hflip,scale=-2:720:flags=lanczos,setpts=PTS/2,fps=29.970"
        ));
    }

    #[test]
    fn each_of_these_rules_out_copying() {
        let source = parse_probe(PROBED).unwrap();
        for changed in [
            Settings {
                crop: Some(Crop::centered(1.0)),
                ..Settings::default()
            },
            Settings {
                turn: Turn::Half,
                ..Settings::default()
            },
            Settings {
                flip_vertical: true,
                ..Settings::default()
            },
            Settings {
                speed: 0.5,
                ..Settings::default()
            },
        ] {
            assert!(!changed.copies(&source), "{changed:?}");
            assert!(changed.encodes_anyway(&source));
        }
    }

    #[test]
    fn a_crop_shape_is_the_shape_of_the_result() {
        let source = parse_probe(PROBED).unwrap();
        let settings = |turn| Settings {
            crop_shape: Some((9, 16)),
            turn,
            ..Settings::default()
        };
        for turn in [Turn::None, Turn::Quarter, Turn::Half, Turn::ThreeQuarters] {
            let mut settings = settings(turn);
            settings.crop = settings.crop_ratio(&source).map(Crop::centered);
            let (width, height) = settings.visible_size(&source);
            let shape = f64::from(width) / f64::from(height);
            assert!(
                (shape - 9.0 / 16.0).abs() < 0.01,
                "{turn:?}: {width} by {height}"
            );
        }
    }

    #[test]
    fn turns_come_back_round() {
        assert_eq!(Turn::None.counter_clockwise(), Turn::ThreeQuarters);
        let mut turn = Turn::None;
        for _ in 0..4 {
            turn = turn.clockwise();
        }
        assert_eq!(turn, Turn::None);
    }

    #[test]
    fn speed_changes_the_audio_and_drops_the_subtitles() {
        assert_eq!(tempo_filters(1.5), "atempo=1.5");
        assert_eq!(tempo_filters(4.0), "atempo=2,atempo=2");
        assert_eq!(tempo_filters(0.25), "atempo=0.5,atempo=0.5");
        let settings = Settings {
            speed: 2.0,
            ..Settings::default()
        };
        let args = planned(&job(settings), "mkv");
        assert!(has(&args, "-af", "atempo=2"));
        assert!(!has(&args, "-c:a", "copy"), "changed audio can't be copied");
        assert!(
            !has(&args, "-map", "0:4"),
            "no subtitles at the wrong times"
        );
        assert!(has(&args, "-map_chapters", "-1"));
        // The target is for what plays, which takes half as long.
        let source = parse_probe(PROBED).unwrap();
        let sized = Settings {
            use_target: true,
            ..settings
        };
        let plain = Settings {
            use_target: true,
            ..Settings::default()
        };
        let quick = sized.fit(&source, 100.0).unwrap().video_bps;
        let slow = plain.fit(&source, 100.0).unwrap().video_bps;
        assert!(quick > slow * 1.9, "{quick} {slow}");
    }

    #[test]
    fn animations_are_small_silent_and_on_their_own_palette() {
        let settings = Settings {
            format: Format::Gif,
            ..Settings::default()
        };
        let source = parse_probe(PROBED).unwrap();
        assert_eq!(settings.extension(&source, "mkv").unwrap(), "gif");
        let mut job = job(settings);
        job.tools.encoders.insert("gif".to_owned());
        let args = planned(&job, "gif");
        let filters = args.windows(2).find(|w| w[0] == "-vf").unwrap()[1].clone();
        assert!(filters.starts_with(TONE_MAP), "{filters}");
        assert!(filters.contains("scale=-2:480"), "{filters}");
        assert!(filters.contains(",fps=15,"), "{filters}");
        assert!(filters.contains("palettegen"), "{filters}");
        assert!(args.iter().any(|arg| arg == "-an"));
        assert!(!has(&args, "-map", "0:2") && !has(&args, "-map", "0:4"));
        assert_eq!(Format::Gif.codec(), None);
        // Audio settings and copying don't come into it.
        assert!(!job.settings.copies(&source));
    }

    #[test]
    fn a_too_big_animation_is_tried_smaller() {
        // Four times too big: half the side, less the margin, evenly.
        assert_eq!(smaller_side(480.0, 4e6, 1e6), 236.0);
        // A hair over still comes down.
        assert_eq!(smaller_side(480.0, 1.0001e6, 1e6), 472.0);
    }

    #[test]
    fn new_tabs_leave_the_picture_alone() {
        let used = Settings {
            crop: Some(Crop::centered(1.0)),
            crop_shape: Some((1, 1)),
            turn: Turn::Half,
            flip_vertical: true,
            speed: 2.0,
            ..Settings::default()
        };
        assert_eq!(used.for_new_tab(), Settings::default());
    }

    #[test]
    fn a_retry_asks_for_less() {
        // 1 MB over 10 s came out of a limit of 900 kB: 100 kB and the margin
        // over, in bits a second.
        let next = smaller_bitrate(800_000.0, 1e6, 9e5, 10.0);
        assert!((next - (800_000.0 - 101_600.0)).abs() < 1.0, "{next}");
        assert_eq!(smaller_bitrate(100_000.0, 1e7, 1e6, 10.0), 50_000.0);
    }

    #[test]
    fn hardware_is_used_only_if_asked_and_there() {
        let mut job = job(Settings {
            format: Format::Mp4H264,
            hardware: true,
            ..Settings::default()
        });
        assert!(has(&planned(&job, "mp4"), "-c:v", "libx264"), "none found");
        job.tools.hardware = vec![Hardware {
            encoder: "h264_vaapi".to_owned(),
            device: Some("/dev/dri/renderD128".to_owned()),
        }];
        let args = planned(&job, "mp4");
        assert!(has(&args, "-c:v", "h264_vaapi"));
        assert!(has(&args, "-vaapi_device", "/dev/dri/renderD128"));
        assert!(args.iter().any(|arg| arg.ends_with("format=nv12,hwupload")));
        assert!(!args.iter().any(|arg| arg == "-pix_fmt"));

        job.settings.format = Format::WebmVp9;
        let args = planned(&job, "webm");
        assert!(has(&args, "-c:v", "libvpx-vp9"), "none for VP9");
        job.settings.hardware = false;
        job.settings.format = Format::Mp4H264;
        assert!(has(&planned(&job, "mp4"), "-c:v", "libx264"));
    }

    #[test]
    fn new_tabs_remember_how_but_not_what_to_leave_out() {
        let mut used = Settings {
            format: Format::WebmVp9,
            quality: Quality::Small,
            use_target: true,
            target_mb: 50.0,
            resolution: Some(720),
            fps: Some(30),
            exact: true,
            audio: AudioChoice::None,
            hardware: true,
            ..Settings::default()
        };
        used.keep.location = false;
        let next = used.for_new_tab();
        assert_eq!(next.format, Format::WebmVp9);
        assert_eq!((next.target_mb, next.resolution), (50.0, Some(720)));
        assert!(next.use_target && next.hardware);
        assert_eq!(next.keep, Keep::default());
        assert_eq!(next.audio, AudioChoice::All);
        assert!(!next.exact);
    }

    #[test]
    fn settings_saved_before_new_ones_existed_load() {
        let old = "(format:Mp4H264,quality:Small,exact:true,keep:(details:true,location:false,subtitles:true,hdr:true))";
        let settings: Settings = ron::from_str(old).unwrap();
        assert_eq!(settings.format, Format::Mp4H264);
        assert!(!settings.keep.location);
        assert_eq!(settings.target_mb, 25.0);
    }

    #[test]
    fn notes_are_cleared_away() {
        let notes = Notes::new().unwrap();
        let dir = notes.dir.clone();
        std::fs::write(dir.join("pass-0.log"), "x").unwrap();
        drop(notes);
        assert!(!dir.exists());
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
    fn target_sizes_are_met() {
        let dir = temp_dir("target");
        let Some(input) = sample(&dir) else {
            eprintln!("no ffmpeg: skipped");
            return;
        };
        let tools = super::tools().unwrap();
        for (format, mb) in [
            (Format::Mp4H264, 0.3),
            (Format::Mp4H264, 0.12),
            (Format::Mp4Hevc, 0.2),
            (Format::WebmVp9, 0.2),
            (Format::Mp4Av1, 0.2),
        ] {
            if !format.available(&tools) {
                eprintln!("no encoder for {format:?}: skipped");
                continue;
            }
            let settings = Settings {
                format,
                use_target: true,
                target_mb: mb,
                ..Settings::default()
            };
            let out = run_job(&dir, &input, settings, (0.0, 10.0));
            let size = std::fs::metadata(&out).unwrap().len() as f64;
            assert!(size <= mb * 1e6, "{format:?} at {mb} MB made {size}");
            assert!(
                size >= mb * 1e6 * 0.3,
                "{format:?} at {mb} MB wasted it: {size}"
            );
            assert!((duration(&out) - 10.0).abs() < 0.5);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resizes_and_lowers_the_frame_rate() {
        let dir = temp_dir("resize");
        let Some(input) = sample(&dir) else {
            eprintln!("no ffmpeg: skipped");
            return;
        };
        let settings = Settings {
            resolution: Some(50),
            fps: Some(5),
            ..Settings::default()
        };
        let out = run_job(&dir, &input, settings, (0.0, 2.0));
        let shape = entries(&out, "stream=width,height,avg_frame_rate");
        assert!(shape.contains("88,50,5/1"), "{shape}");

        // A phone's upright video stays upright.
        let upright = dir.join("upright.mp4");
        let turned = command("ffmpeg")
            .args(["-v", "error", "-display_rotation", "90", "-i"])
            .arg(&input)
            .args(["-c", "copy"])
            .arg(&upright)
            .status();
        assert!(turned.unwrap().success());
        let source = probe(&upright).unwrap();
        assert!(source.height > source.width);
        let settings = Settings {
            resolution: Some(50),
            ..Settings::default()
        };
        let out = run_job(&dir, &upright, settings, (0.0, 2.0));
        let shape = entries(&out, "stream=width,height");
        assert!(shape.contains("50,88"), "{shape}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn audio_comes_off_or_alone() {
        let dir = temp_dir("audio");
        let Some(input) = sample(&dir) else {
            eprintln!("no ffmpeg: skipped");
            return;
        };
        let silent = Settings {
            audio: AudioChoice::None,
            ..Settings::default()
        };
        let out = run_job(&dir, &input, silent, (0.0, 4.0));
        assert!(!entries(&out, "stream=codec_type").contains("audio"));
        let tools = super::tools().unwrap();
        for (format, codec) in [
            (Format::Mp3, "mp3"),
            (Format::M4a, "aac"),
            (Format::Opus, "opus"),
            (Format::Flac, "flac"),
            (Format::Wav, "pcm_s16le"),
        ] {
            if !format.available(&tools) {
                eprintln!("no encoder for {format:?}: skipped");
                continue;
            }
            let settings = Settings {
                format,
                ..Settings::default()
            };
            let out = run_job(&dir, &input, settings, (1.0, 3.0));
            let streams = entries(&out, "stream=codec_name");
            assert_eq!(streams.trim().trim_end_matches(','), codec, "{format:?}");
            assert!((duration(&out) - 2.0).abs() < 0.2, "{format:?}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hardware_encoders_export_if_there_are_any() {
        let dir = temp_dir("hardware");
        // They have a smallest picture they'll take.
        let input = dir.join("large.mp4");
        let made = command("ffmpeg")
            .args(["-v", "error", "-f", "lavfi", "-i"])
            .arg("testsrc2=size=1280x720:rate=30:duration=4")
            .args(["-c:v", "libx264", "-preset", "ultrafast"])
            .arg(&input)
            .status();
        if !made.is_ok_and(|status| status.success()) {
            eprintln!("no ffmpeg: skipped");
            return;
        }
        let tools = super::tools().unwrap();
        for hardware in &tools.hardware {
            let format = match hardware.encoder.split('_').next() {
                Some("h264") => Format::Mp4H264,
                Some("hevc") => Format::Mp4Hevc,
                _ => Format::Mp4Av1,
            };
            let quality = Settings {
                format,
                hardware: true,
                ..Settings::default()
            };
            let out = run_job(&dir, &input, quality, (0.0, 4.0));
            assert!((duration(&out) - 4.0).abs() < 0.5, "{}", hardware.encoder);
            let codec = entries(&out, "stream=codec_name");
            let expected = hardware.encoder.split('_').next().unwrap();
            assert!(
                codec.contains(expected),
                "{} made {codec}",
                hardware.encoder
            );
            let sized = Settings {
                use_target: true,
                target_mb: 0.5,
                resolution: Some(360),
                ..quality
            };
            let out = run_job(&dir, &input, sized, (0.0, 4.0));
            let size = std::fs::metadata(&out).unwrap().len() as f64;
            assert!(size <= 0.5e6, "{} made {size}", hardware.encoder);
            assert!(entries(&out, "stream=height").contains("360"));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn crops_turns_and_changes_speed() {
        let dir = temp_dir("picture");
        let Some(input) = sample(&dir) else {
            eprintln!("no ffmpeg: skipped");
            return;
        };
        let cropped = Settings {
            crop: Some(Crop {
                x: 0.25,
                y: 0.0,
                w: 0.5,
                h: 1.0,
            }),
            ..Settings::default()
        };
        let out = run_job(&dir, &input, cropped, (0.0, 2.0));
        assert!(entries(&out, "stream=width,height").contains("80,90"));

        let turned = Settings {
            turn: Turn::Quarter,
            ..cropped
        };
        let out = run_job(&dir, &input, turned, (0.0, 2.0));
        assert!(entries(&out, "stream=width,height").contains("90,80"));

        // Which way it turned: a white top left corner goes to the top right.
        let marked = dir.join("marked.mp4");
        let made = command("ffmpeg")
            .args(["-v", "error", "-f", "lavfi", "-i"])
            .arg("color=black:s=160x90:r=10:d=1,drawbox=x=0:y=0:w=30:h=30:color=white:t=fill")
            .args(["-c:v", "libx264", "-qp", "0"])
            .arg(&marked)
            .status();
        assert!(made.unwrap().success());
        let pixel = |path: &Path, x: u32, y: u32| {
            let out = command("ffmpeg")
                .args(["-v", "error", "-i"])
                .arg(path)
                .args(["-frames:v", "1", "-vf", &format!("crop=2:2:{x}:{y}")])
                .args(["-f", "rawvideo", "-pix_fmt", "gray", "-"])
                .output()
                .unwrap();
            out.stdout[0]
        };
        let bright = |value: u8| value > 200;
        assert!(bright(pixel(&marked, 5, 5)) && !bright(pixel(&marked, 155, 5)));
        let clockwise = Settings {
            turn: Turn::Quarter,
            ..Settings::default()
        };
        let out = run_job(&dir, &marked, clockwise, (0.0, 1.0));
        assert!(entries(&out, "stream=width,height").contains("90,160"));
        assert!(bright(pixel(&out, 85, 5)), "to the top right");
        assert!(!bright(pixel(&out, 5, 5)));
        let counter = Settings {
            turn: Turn::ThreeQuarters,
            ..Settings::default()
        };
        let out = run_job(&dir, &marked, counter, (0.0, 1.0));
        assert!(bright(pixel(&out, 5, 155)), "to the bottom left");
        let mirrored = Settings {
            flip_horizontal: true,
            ..Settings::default()
        };
        let out = run_job(&dir, &marked, mirrored, (0.0, 1.0));
        assert!(bright(pixel(&out, 155, 5)), "to the other side");
        let upended = Settings {
            flip_vertical: true,
            ..Settings::default()
        };
        let out = run_job(&dir, &marked, upended, (0.0, 1.0));
        assert!(bright(pixel(&out, 5, 85)), "to the bottom");
        let half = Settings {
            turn: Turn::Half,
            ..Settings::default()
        };
        let out = run_job(&dir, &marked, half, (0.0, 1.0));
        assert!(bright(pixel(&out, 155, 85)), "to the opposite corner");

        let fast = Settings {
            speed: 2.0,
            ..Settings::default()
        };
        let out = run_job(&dir, &input, fast, (0.0, 8.0));
        assert!((duration(&out) - 4.0).abs() < 0.3, "{}", duration(&out));
        let frames: f64 = entries(&out, "stream=avg_frame_rate")
            .lines()
            .find_map(|line| parse_rate(line.trim_end_matches(',')))
            .unwrap();
        assert!(
            (frames - 10.0).abs() < 0.5,
            "the frame rate stays: {frames}"
        );

        // Slower than the one atempo can go, and audio alone follows too.
        let slow_audio = Settings {
            format: Format::Flac,
            speed: 0.25,
            ..Settings::default()
        };
        let out = run_job(&dir, &input, slow_audio, (0.0, 2.0));
        assert!((duration(&out) - 8.0).abs() < 0.3, "{}", duration(&out));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn makes_animations() {
        let dir = temp_dir("animation");
        let Some(input) = sample(&dir) else {
            eprintln!("no ffmpeg: skipped");
            return;
        };
        let tools = super::tools().unwrap();
        let gif = Settings {
            format: Format::Gif,
            ..Settings::default()
        };
        if gif.format.available(&tools) {
            let out = run_job(&dir, &input, gif, (0.0, 3.0));
            assert_eq!(out.extension().unwrap(), "gif");
            assert!(entries(&out, "stream=codec_name").contains("gif"));
            assert!(entries(&out, "stream=width,height").contains("160,90"));
            let limited = Settings {
                use_target: true,
                target_mb: 0.1,
                ..gif
            };
            let out = run_job(&dir, &input, limited, (0.0, 10.0));
            let size = std::fs::metadata(&out).unwrap().len() as f64;
            assert!(size <= 0.1e6, "made {size}");
        }
        let webp = Settings {
            format: Format::WebpAnimated,
            ..Settings::default()
        };
        if webp.format.available(&tools) {
            let out = run_job(&dir, &input, webp, (0.0, 3.0));
            assert_eq!(out.extension().unwrap(), "webp");
            let bytes = std::fs::read(&out).unwrap();
            assert!(bytes.windows(4).any(|w| w == b"ANIM"), "it's animated");
            let limited = Settings {
                use_target: true,
                target_mb: 0.1,
                ..webp
            };
            let out = run_job(&dir, &input, limited, (0.0, 10.0));
            let size = std::fs::metadata(&out).unwrap().len() as f64;
            assert!(size <= 0.1e6, "made {size}");
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
