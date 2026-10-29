//! Video playback through libmpv.
//!
//! A single background thread owns a headless OpenGL context and every mpv
//! instance. mpv decodes (with hardware decoding where available), scales and
//! converts colors on the GPU, rendering each frame at exactly the size it is
//! shown; only that final frame is copied into an egui texture.
//!
//! Like [`crate::images::ImageCache`], players are requested every frame and
//! dropped once nothing asks for them.

#[cfg(target_os = "linux")]
mod egl;
mod mpv;
#[cfg(target_os = "linux")]
mod renderer;

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Sender},
    },
};

use egui::{ColorImage, Context, Id, Rect, TextureHandle, TextureOptions, emath::GuiRounding};
use serde::{Deserialize, Serialize};

const EXTENSIONS: &[&str] = &[
    "mp4", "m4v", "mkv", "webm", "mov", "avi", "wmv", "flv", "mpg", "mpeg", "ts", "m2ts", "mts",
    "3gp", "ogv", "vob",
];

pub fn is_video(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Starts paused on the first frame, with sound.
    Preview,
    /// Plays immediately, silent and looping.
    Hover,
}

/// How mpv scales video to the size it's shown at.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Quality {
    Fast,
    Default,
    /// Sharper scaling; cheap at preview sizes on any discrete GPU.
    #[default]
    High,
}

impl Quality {
    pub const ALL: [Self; 3] = [Self::Fast, Self::Default, Self::High];

    pub fn label(self) -> &'static str {
        match self {
            Self::Fast => "Fast",
            Self::Default => "Default",
            Self::High => "High",
        }
    }

    /// mpv options for this level.
    fn options(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Self::Fast => &[("profile", "fast")],
            Self::Default => &[],
            // mpv's high-quality profile only sharpens upscaling, but
            // previews are mostly downscaled, where the default (hermite) is soft.
            Self::High => &[("profile", "high-quality"), ("dscale", "lanczos")],
        }
    }
}

/// Volume of new players until the user changes it, in percent.
pub const DEFAULT_VOLUME: f64 = 50.0;

/// Written by the video thread; the UI only adjusts it optimistically.
#[derive(Clone, Debug, Default)]
pub struct Status {
    /// Which [`Player::load`] this describes; older values are ignored.
    generation: u64,
    /// Seconds.
    pub position: f64,
    /// Seconds; 0 until known.
    pub duration: f64,
    pub paused: bool,
    pub muted: bool,
    /// Percent, 0–100.
    pub volume: f64,
    /// Stopped on the last frame.
    pub eof: bool,
    /// Display size of the video, once loaded.
    pub video_size: Option<[u32; 2]>,
    /// Whether the texture shows a frame of the current file.
    pub has_frame: bool,
    pub error: Option<String>,
}

enum Command {
    Load {
        path: PathBuf,
        generation: u64,
    },
    /// Render target size in pixels.
    Resize([u32; 2]),
    Pause(bool),
    Seek(f64),
    Mute(bool),
    Volume(f64),
}

struct Open {
    id: u64,
    mode: Mode,
    texture: TextureHandle,
    status: Arc<Mutex<Status>>,
    volume: f64,
    quality: Quality,
}

enum Message {
    Open(Open),
    Command(u64, Command),
    Close(u64),
    /// mpv has events or a new frame for this player.
    Wake(u64),
}

/// A playing video. Dropping it stops playback.
pub struct Player {
    id: u64,
    mode: Mode,
    sender: Sender<Message>,
    path: PathBuf,
    generation: u64,
    texture: TextureHandle,
    status: Arc<Mutex<Status>>,
    size: [u32; 2],
    /// Shared by all players, so the last volume chosen sticks.
    volume: Arc<AtomicU64>,
}

impl Player {
    fn send(&self, command: Command) {
        let _ = self.sender.send(Message::Command(self.id, command));
    }

    fn status_mut(&self) -> std::sync::MutexGuard<'_, Status> {
        self.status.lock().expect("video status poisoned")
    }

    /// The status of the current file; defaults until the video thread has loaded it.
    pub fn status(&self) -> Status {
        let status = self.status_mut();
        if status.generation == self.generation {
            status.clone()
        } else {
            Status {
                generation: self.generation,
                paused: self.mode == Mode::Preview,
                muted: status.muted,
                volume: status.volume,
                ..Status::default()
            }
        }
    }

    /// The current frame, once the current file has one.
    pub fn frame(&self) -> Option<&TextureHandle> {
        let status = self.status_mut();
        (status.has_frame && status.generation == self.generation).then_some(&self.texture)
    }

    /// Renders frames for `rect`, snapped to whole pixels so they are drawn
    /// 1:1 rather than resampled (which blurs them); returns the snapped rect
    /// to paint the frame into.
    pub fn render_for(&mut self, rect: Rect, pixels_per_point: f32) -> Rect {
        let snapped = rect.round_to_pixels(pixels_per_point);
        let pixels = (snapped.size() * pixels_per_point).round();
        self.set_size([pixels.x as u32, pixels.y as u32]);
        snapped
    }

    /// Sets the render size in pixels; frames are rendered at exactly this size.
    fn set_size(&mut self, size: [u32; 2]) {
        let size = size.map(|side| side.clamp(1, 4096));
        if size != self.size {
            self.size = size;
            self.send(Command::Resize(size));
        }
    }

    pub fn set_paused(&self, paused: bool) {
        let mut status = self.status_mut();
        if !paused && status.eof {
            status.eof = false;
            self.send(Command::Seek(0.0));
        }
        status.paused = paused;
        self.send(Command::Pause(paused));
    }

    pub fn seek(&self, seconds: f64) {
        let mut status = self.status_mut();
        status.position = seconds;
        status.eof = false;
        self.send(Command::Seek(seconds));
    }

    pub fn set_muted(&self, muted: bool) {
        self.status_mut().muted = muted;
        self.send(Command::Mute(muted));
    }

    /// Sets the volume in percent; also the volume of players opened later.
    pub fn set_volume(&self, volume: f64) {
        let volume = volume.clamp(0.0, 100.0);
        self.status_mut().volume = volume;
        self.volume.store(volume.to_bits(), Ordering::Relaxed);
        self.send(Command::Volume(volume));
    }

    fn load(&mut self, path: &Path) {
        self.path = path.to_path_buf();
        self.generation += 1;
        self.send(Command::Load {
            path: self.path.clone(),
            generation: self.generation,
        });
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        let _ = self.sender.send(Message::Close(self.id));
    }
}

struct Backend {
    sender: Sender<Message>,
    /// Set by the video thread if it could not start.
    error: Arc<OnceLock<String>>,
}

impl Backend {
    fn start(ctx: &Context) -> Self {
        let (sender, receiver) = mpsc::channel();
        let error = Arc::new(OnceLock::new());
        #[cfg(target_os = "linux")]
        renderer::spawn(ctx.clone(), receiver, sender.clone(), error.clone());
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (ctx, receiver);
            let _ = error.set("video playback is not implemented on this platform yet".into());
        }
        Self { sender, error }
    }
}

struct Slot {
    player: Player,
    last_used: u64,
}

pub struct Videos {
    ctx: Context,
    backend: Option<Backend>,
    players: HashMap<Id, Slot>,
    next_id: u64,
    frame: u64,
    /// Volume in percent, as `f64` bits.
    volume: Arc<AtomicU64>,
    quality: Quality,
}

impl Videos {
    pub fn new(ctx: &Context) -> Self {
        Self {
            ctx: ctx.clone(),
            backend: None,
            players: HashMap::new(),
            next_id: 0,
            frame: 0,
            volume: Arc::new(AtomicU64::new(DEFAULT_VOLUME.to_bits())),
            quality: Quality::default(),
        }
    }

    pub fn quality(&self) -> Quality {
        self.quality
    }

    /// Restarts open players, since mpv's scalers are set when it starts.
    pub fn set_quality(&mut self, quality: Quality) {
        if quality != self.quality {
            self.quality = quality;
            self.players.clear();
        }
    }

    /// Volume in percent for players with sound.
    pub fn volume(&self) -> f64 {
        f64::from_bits(self.volume.load(Ordering::Relaxed))
    }

    pub fn set_volume(&mut self, volume: f64) {
        let volume = if volume.is_finite() {
            volume.clamp(0.0, 100.0)
        } else {
            DEFAULT_VOLUME
        };
        self.volume.store(volume.to_bits(), Ordering::Relaxed);
    }

    /// Stops players nothing asked for last frame. Call once per frame.
    pub fn begin_frame(&mut self) {
        self.frame += 1;
        let frame = self.frame;
        self.players.retain(|_, slot| slot.last_used + 1 >= frame);
    }

    /// The player for `id`, playing `path`; created or switched over as needed.
    pub fn player(&mut self, id: Id, path: &Path, mode: Mode) -> Result<&mut Player, String> {
        let backend = self
            .backend
            .get_or_insert_with(|| Backend::start(&self.ctx));
        if let Some(error) = backend.error.get() {
            return Err(error.clone());
        }
        let slot = match self.players.entry(id) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => {
                self.next_id += 1;
                let id = self.next_id;
                let texture = self.ctx.load_texture(
                    format!("video-{id}"),
                    ColorImage::filled([1, 1], egui::Color32::BLACK),
                    TextureOptions::LINEAR,
                );
                let volume = f64::from_bits(self.volume.load(Ordering::Relaxed));
                let status = Arc::new(Mutex::new(Status {
                    paused: mode == Mode::Preview,
                    muted: mode == Mode::Hover,
                    volume,
                    ..Status::default()
                }));
                let _ = backend.sender.send(Message::Open(Open {
                    id,
                    mode,
                    texture: texture.clone(),
                    status: status.clone(),
                    volume,
                    quality: self.quality,
                }));
                let mut player = Player {
                    id,
                    mode,
                    sender: backend.sender.clone(),
                    path: PathBuf::new(),
                    generation: 0,
                    texture,
                    status,
                    size: [0, 0],
                    volume: self.volume.clone(),
                };
                player.load(path);
                entry.insert(Slot {
                    player,
                    last_used: self.frame,
                })
            }
        };
        slot.last_used = self.frame;
        if slot.player.path != path {
            slot.player.load(path);
        }
        Ok(&mut slot.player)
    }
}

#[cfg(test)]
impl Videos {
    pub fn players(&self) -> impl Iterator<Item = (Id, &Player)> {
        self.players.iter().map(|(id, slot)| (*id, &slot.player))
    }
}

#[cfg(test)]
impl Player {
    pub fn size(&self) -> [u32; 2] {
        self.size
    }
}

pub fn format_time(seconds: f64) -> String {
    let total = seconds.max(0.0) as u64;
    let (hours, minutes, seconds) = (total / 3600, total / 60 % 60, total % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Plays a clip whose top half is red and bottom half blue, and checks the
    /// rendered frame. Needs libmpv, ffmpeg and a GPU.
    #[test]
    #[ignore]
    fn renders_frames_upright_at_requested_size() {
        let dir = std::env::temp_dir().join(format!("egui-files-video-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("halves.mp4");
        let status = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
            .arg("color=red:s=128x64:d=2[a];color=blue:s=128x64:d=2[b];[a][b]vstack")
            .args(["-pix_fmt", "yuv420p"])
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());

        let ctx = Context::default();
        let mut videos = Videos::new(&ctx);
        let id = Id::new("test");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let image = loop {
            assert!(std::time::Instant::now() < deadline, "no frame rendered");
            videos.begin_frame();
            let player = videos.player(id, &path, Mode::Preview).unwrap();
            let status = player.status();
            assert_eq!(status.error, None);
            if let Some([w, h]) = status.video_size {
                assert_eq!([w, h], [128, 128]);
                player.set_size([w / 2, h / 2]);
            }
            let has_frame = player.frame().is_some();
            let mut delta = ctx.tex_manager().write().take_delta();
            let frame = delta
                .set
                .values()
                .flatten()
                .find_map(|delta| match &delta.image {
                    egui::ImageData::Color(image) if image.size == [64, 64] => Some(image.clone()),
                    _ => None,
                });
            delta.clear();
            if let Some(image) = frame {
                assert!(has_frame || player.frame().is_some());
                break image;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        let top = image.pixels[64 * 8 + 32];
        let bottom = image.pixels[64 * 56 + 32];
        assert!(
            top.r() > 200 && top.b() < 50 && top.a() == 255,
            "top is {top:?}"
        );
        assert!(bottom.b() > 200 && bottom.r() < 50, "bottom is {bottom:?}");
        assert!(
            videos
                .player(id, &path, Mode::Preview)
                .unwrap()
                .status()
                .paused
        );

        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Switching between videos of the same height used to report a size of
    /// [width, 0], since mpv only reports changed properties. Needs libmpv,
    /// ffmpeg and a GPU.
    #[test]
    #[ignore]
    fn switching_videos_never_shows_partial_sizes_or_stale_frames() {
        let dir = std::env::temp_dir().join(format!("egui-files-switch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let clip = |name: &str, size: &str, color: &str| {
            let path = dir.join(name);
            let status = std::process::Command::new("ffmpeg")
                .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
                .arg(format!("color={color}:s={size}:d=2"))
                .args(["-pix_fmt", "yuv420p"])
                .arg(&path)
                .status()
                .unwrap();
            assert!(status.success());
            path
        };
        let wide = clip("wide.mp4", "320x180", "red");
        let narrow = clip("narrow.mp4", "240x180", "blue");

        let ctx = Context::default();
        let mut videos = Videos::new(&ctx);
        let id = Id::new("test");
        for (path, size, color) in [
            (&wide, [320, 180], [255, 0, 0]),
            (&narrow, [240, 180], [0, 0, 255]),
            (&wide, [320, 180], [255, 0, 0]),
        ] {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                assert!(
                    std::time::Instant::now() < deadline,
                    "{path:?} never showed"
                );
                videos.begin_frame();
                let player = videos.player(id, path, Mode::Preview).unwrap();
                let status = player.status();
                if let Some(video_size) = status.video_size {
                    assert_eq!(video_size, size, "size of the wrong file or half-known");
                }
                let shown = player.frame().is_some();
                let mut delta = ctx.tex_manager().write().take_delta();
                let last = delta
                    .set
                    .get(&player.texture.id())
                    .and_then(|deltas| deltas.last())
                    .and_then(|delta| {
                        let egui::ImageData::Color(image) = &delta.image;
                        // The center: right after switching, frames may be letterboxed.
                        let [w, h] = image.size;
                        image.pixels.get(h / 2 * w + w / 2).copied()
                    });
                delta.clear();
                if shown && let Some(pixel) = last {
                    let [r, _, b] = color;
                    assert!(
                        pixel.r().abs_diff(r) < 40 && pixel.b().abs_diff(b) < 40,
                        "frame of the wrong file shown: {pixel:?}"
                    );
                    assert_eq!(status.volume, DEFAULT_VOLUME);
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn frames_are_rendered_for_whole_pixel_rects() {
        let ctx = Context::default();
        let (sender, _receiver) = mpsc::channel();
        let mut player = Player {
            id: 1,
            mode: Mode::Preview,
            sender,
            path: PathBuf::new(),
            generation: 0,
            texture: ctx.load_texture(
                "t",
                ColorImage::filled([1, 1], egui::Color32::BLACK),
                TextureOptions::LINEAR,
            ),
            status: Arc::default(),
            size: [0, 0],
            volume: Arc::default(),
        };
        let ppp = 1.25;
        let rect = Rect::from_min_size(egui::pos2(10.3, 20.7), egui::vec2(200.45, 112.6));
        let snapped = player.render_for(rect, ppp);
        for value in [snapped.min.x, snapped.min.y, snapped.max.x, snapped.max.y] {
            let pixels = value * ppp;
            assert!(
                (pixels - pixels.round()).abs() < 1e-3,
                "{value} is not on a pixel"
            );
        }
        let size = snapped.size() * ppp;
        assert_eq!(
            player.size(),
            [size.x.round() as u32, size.y.round() as u32]
        );
        assert!((snapped.center() - rect.center()).length() < 1.0);
    }

    /// Needs libmpv, ffmpeg and a GPU.
    #[test]
    #[ignore]
    fn every_quality_level_plays() {
        let dir = std::env::temp_dir().join(format!("egui-files-quality-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("clip.mp4");
        let status = std::process::Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=s=320x180:d=1",
            ])
            .args(["-pix_fmt", "yuv420p"])
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());
        let ctx = Context::default();
        let mut videos = Videos::new(&ctx);
        for quality in Quality::ALL {
            videos.set_quality(quality);
            assert_eq!(videos.players().count(), 0, "players restart");
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                assert!(
                    std::time::Instant::now() < deadline,
                    "{quality:?} rendered nothing"
                );
                videos.begin_frame();
                let player = videos.player(Id::new("q"), &path, Mode::Preview).unwrap();
                assert_eq!(player.status().error, None, "{quality:?}");
                player.render_for(
                    Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(100.0, 56.0)),
                    1.0,
                );
                let has_frame = player.frame().is_some();
                ctx.tex_manager().write().take_delta().clear();
                if has_frame {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
