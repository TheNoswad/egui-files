//! The video thread on Android: a `MediaPlayer` for each video, drawing into
//! an `ImageReader` whose frames go into the texture.
//!
//! There's no Java of the app's own to take callbacks, so it asks instead:
//! whether there's a new frame, and how far along it is.

use std::{
    collections::HashMap,
    path::Path,
    sync::{
        Arc, OnceLock,
        mpsc::{Receiver, RecvTimeoutError},
    },
    time::{Duration, Instant},
};

use egui::{ColorImage, Context, TextureOptions};
use jni::{
    Env, JValue, jni_sig, jni_str,
    objects::{JByteBuffer, JObject, JObjectArray, JString},
    refs::Global,
};

use super::{
    Command, Message, Mode, Open, Status,
    yuv::{self, Plane},
};
use crate::android::with_env;

/// `ImageFormat.YUV_420_888`.
const YUV_420_888: i32 = 0x23;
/// `MediaPlayer.SEEK_CLOSEST`: the very frame, not the keyframe before it.
const SEEK_CLOSEST: i32 = 3;
/// `MediaMetadataRetriever`'s keys.
const KEY_WIDTH: i32 = 18;
const KEY_HEIGHT: i32 = 19;
const KEY_ROTATION: i32 = 24;
/// `RunningAppProcessInfo.IMPORTANCE_VISIBLE`: past this, the app's off screen.
const IMPORTANCE_VISIBLE: i32 = 200;
/// How often to see whether the app's still on screen, while something plays.
const BACKGROUND_CHECK: Duration = Duration::from_millis(250);

type Ref = Global<JObject<'static>>;

pub(super) fn spawn(ctx: Context, receiver: Receiver<Message>, error: Arc<OnceLock<String>>) {
    let _ = &error;
    std::thread::Builder::new()
        .name("video".into())
        .spawn(move || run(&ctx, &receiver))
        .expect("failed to spawn video thread");
}

fn run(ctx: &Context, receiver: &Receiver<Message>) {
    let mut players: HashMap<u64, AndroidPlayer> = HashMap::new();
    let mut checked = Instant::now();
    loop {
        // Often enough for frames while something's playing; a paused video
        // only gets new ones after seeking, which comes as a message.
        let wait = if players.values().any(|player| !player.state.paused) {
            Duration::from_millis(8)
        } else {
            Duration::from_millis(40)
        };
        let first = match receiver.recv_timeout(wait) {
            Ok(message) => Some(message),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        for message in first.into_iter().chain(receiver.try_iter()) {
            match message {
                Message::Open(open) => {
                    players.insert(open.id, AndroidPlayer::new(open));
                }
                Message::Close(id) => {
                    if let Some(mut player) = players.remove(&id) {
                        player.unload();
                    }
                }
                Message::Command(id, command) => {
                    if let Some(player) = players.get_mut(&id) {
                        player.command(command, ctx);
                    }
                }
                // Nothing here wakes it: it looks for itself.
                Message::Wake(_) => {}
            }
        }
        for player in players.values_mut() {
            player.tick(ctx);
        }
        // Nobody wants to hear a video they've left, and nothing tells this
        // thread they have.
        let playing = players.values().any(|player| !player.state.paused);
        if playing && checked.elapsed() >= BACKGROUND_CHECK {
            checked = Instant::now();
            if in_background() {
                for player in players.values_mut().filter(|player| !player.state.paused) {
                    player.command(Command::Pause(true), ctx);
                }
            }
        }
    }
    for player in players.values_mut() {
        player.unload();
    }
}

/// Whether the app's off screen. If it can't tell, it says not.
fn in_background() -> bool {
    with_env("see whether the app's on screen", |env| {
        let info = env.new_object(
            jni_str!("android/app/ActivityManager$RunningAppProcessInfo"),
            jni_sig!("()V"),
            &[],
        )?;
        env.call_static_method(
            jni_str!("android/app/ActivityManager"),
            jni_str!("getMyMemoryState"),
            jni_sig!("(Landroid/app/ActivityManager$RunningAppProcessInfo;)V"),
            &[JValue::Object(&info)],
        )?;
        let importance = env
            .get_field(&info, jni_str!("importance"), jni_sig!("I"))?
            .i()?;
        Ok(importance > IMPORTANCE_VISIBLE)
    })
    .unwrap_or(false)
}

/// A file being played.
struct Loaded {
    player: Ref,
    reader: Ref,
    /// The frame's size before turning, and how far to turn it clockwise.
    width: usize,
    height: usize,
    rotation: u32,
}

struct AndroidPlayer {
    mode: Mode,
    texture: egui::TextureHandle,
    status: Arc<std::sync::Mutex<Status>>,
    state: Status,
    loaded: Option<Loaded>,
    /// How big to draw frames; unknown at first.
    size: [u32; 2],
    speed: f64,
    pixels: Vec<u8>,
}

impl AndroidPlayer {
    fn new(open: Open) -> Self {
        let state = open.status.lock().expect("video status poisoned").clone();
        Self {
            mode: open.mode,
            texture: open.texture,
            status: open.status,
            state,
            loaded: None,
            size: [0, 0],
            speed: 1.0,
            pixels: Vec::new(),
        }
    }

    fn publish(&mut self, ctx: &Context) {
        *self.status.lock().expect("video status poisoned") = self.state.clone();
        ctx.request_repaint();
    }

    fn report(&mut self, result: Result<(), String>, ctx: &Context) {
        if let Err(err) = result {
            self.state.error = Some(err);
            self.publish(ctx);
        }
    }

    fn command(&mut self, command: Command, ctx: &Context) {
        let result = match command {
            Command::Load { path, generation } => {
                self.state.generation = generation;
                self.state.has_frame = false;
                self.state.error = None;
                self.state.video_size = None;
                self.state.position = 0.0;
                self.state.duration = 0.0;
                self.state.eof = false;
                self.state.paused = self.mode == Mode::Preview;
                self.unload();
                let result = self.load(&path);
                self.publish(ctx);
                result
            }
            Command::Resize(size) => {
                // A paused video shows the last frame stretched until there's
                // a new one, as it hasn't kept it.
                self.size = size;
                Ok(())
            }
            Command::Pause(paused) => {
                self.state.paused = paused;
                self.set_playing(!paused)
            }
            Command::Seek(seconds) => {
                self.state.position = seconds;
                self.state.eof = false;
                self.seek(seconds)
            }
            Command::Mute(muted) => {
                self.state.muted = muted;
                self.apply_volume()
            }
            Command::Volume(volume) => {
                self.state.volume = volume;
                self.apply_volume()
            }
            Command::Speed(speed) => {
                self.speed = speed;
                // It'd start a paused video: this waits for the next play.
                if self.state.paused {
                    Ok(())
                } else {
                    self.apply_speed()
                }
            }
        };
        self.report(result, ctx);
    }

    fn load(&mut self, path: &Path) -> Result<(), String> {
        let path = path.to_str().ok_or("the file's name isn't valid Unicode")?;
        let (width, height, rotation) = metadata(path)?;
        let hover = self.mode == Mode::Hover;
        let (player, reader) = with_env("open a video", |env| {
            let reader = env
                .call_static_method(
                    jni_str!("android/media/ImageReader"),
                    jni_str!("newInstance"),
                    jni_sig!("(IIII)Landroid/media/ImageReader;"),
                    &[
                        JValue::Int(width as i32),
                        JValue::Int(height as i32),
                        JValue::Int(YUV_420_888),
                        JValue::Int(3),
                    ],
                )?
                .l()?;
            let surface = env
                .call_method(
                    &reader,
                    jni_str!("getSurface"),
                    jni_sig!("()Landroid/view/Surface;"),
                    &[],
                )?
                .l()?;
            let player =
                env.new_object(jni_str!("android/media/MediaPlayer"), jni_sig!("()V"), &[])?;
            env.call_method(
                &player,
                jni_str!("setSurface"),
                jni_sig!("(Landroid/view/Surface;)V"),
                &[JValue::Object(&surface)],
            )?;
            let file = env.new_string(path)?;
            env.call_method(
                &player,
                jni_str!("setDataSource"),
                jni_sig!("(Ljava/lang/String;)V"),
                &[JValue::Object(&file)],
            )?;
            env.call_method(&player, jni_str!("prepare"), jni_sig!("()V"), &[])?;
            env.call_method(
                &player,
                jni_str!("setLooping"),
                jni_sig!("(Z)V"),
                &[JValue::Bool(hover)],
            )?;
            let duration = env
                .call_method(&player, jni_str!("getDuration"), jni_sig!("()I"), &[])?
                .i()?;
            let player = env.new_global_ref(player)?;
            let reader = env.new_global_ref(reader)?;
            Ok((player, reader, duration))
        })
        .map(|(player, reader, duration)| {
            self.state.duration = f64::from(duration.max(0)) / 1000.0;
            (player, reader)
        })?;
        self.loaded = Some(Loaded {
            player,
            reader,
            width,
            height,
            rotation,
        });
        self.state.video_size = Some(if rotation % 180 == 90 {
            [height as u32, width as u32]
        } else {
            [width as u32, height as u32]
        });
        self.apply_volume()?;
        if hover {
            self.set_playing(true)
        } else {
            // Shows the first frame, which it otherwise wouldn't until playing.
            self.seek(0.0)
        }
    }

    fn unload(&mut self) {
        let Some(loaded) = self.loaded.take() else {
            return;
        };
        let _ = with_env("close a video", |env| {
            env.call_method(&loaded.player, jni_str!("release"), jni_sig!("()V"), &[])?;
            env.call_method(&loaded.reader, jni_str!("close"), jni_sig!("()V"), &[])?;
            Ok(())
        });
    }

    fn set_playing(&mut self, playing: bool) -> Result<(), String> {
        let Some(loaded) = &self.loaded else {
            return Ok(());
        };
        with_env("play a video", |env| {
            if playing {
                env.call_method(&loaded.player, jni_str!("start"), jni_sig!("()V"), &[])?;
            } else {
                env.call_method(&loaded.player, jni_str!("pause"), jni_sig!("()V"), &[])?;
            }
            Ok(())
        })?;
        if playing && self.speed != 1.0 {
            self.apply_speed()?;
        }
        Ok(())
    }

    fn apply_speed(&self) -> Result<(), String> {
        let Some(loaded) = &self.loaded else {
            return Ok(());
        };
        let speed = self.speed as f32;
        with_env("change a video's speed", |env| {
            let params = env.new_object(
                jni_str!("android/media/PlaybackParams"),
                jni_sig!("()V"),
                &[],
            )?;
            let params = env
                .call_method(
                    &params,
                    jni_str!("setSpeed"),
                    jni_sig!("(F)Landroid/media/PlaybackParams;"),
                    &[JValue::Float(speed)],
                )?
                .l()?;
            env.call_method(
                &loaded.player,
                jni_str!("setPlaybackParams"),
                jni_sig!("(Landroid/media/PlaybackParams;)V"),
                &[JValue::Object(&params)],
            )?;
            Ok(())
        })
    }

    fn seek(&self, seconds: f64) -> Result<(), String> {
        let Some(loaded) = &self.loaded else {
            return Ok(());
        };
        with_env("seek in a video", |env| {
            env.call_method(
                &loaded.player,
                jni_str!("seekTo"),
                jni_sig!("(JI)V"),
                &[
                    JValue::Long((seconds.max(0.0) * 1000.0) as i64),
                    JValue::Int(SEEK_CLOSEST),
                ],
            )?;
            Ok(())
        })
    }

    fn apply_volume(&self) -> Result<(), String> {
        let Some(loaded) = &self.loaded else {
            return Ok(());
        };
        let volume = if self.state.muted {
            0.0
        } else {
            (self.state.volume / 100.0) as f32
        };
        with_env("set a video's volume", |env| {
            env.call_method(
                &loaded.player,
                jni_str!("setVolume"),
                jni_sig!("(FF)V"),
                &[JValue::Float(volume), JValue::Float(volume)],
            )?;
            Ok(())
        })
    }

    /// Takes the newest frame, if there's one, and how far along it is.
    fn tick(&mut self, ctx: &Context) {
        let Some(loaded) = &self.loaded else {
            return;
        };
        let (width, height, rotation) = (loaded.width, loaded.height, loaded.rotation);
        let size = if self.size.contains(&0) {
            self.state
                .video_size
                .unwrap_or([16, 16])
                .map(|side| side.min(512))
        } else {
            self.size
        };
        let size = size.map(|side| side as usize);
        let pixels = &mut self.pixels;
        let result = with_env("read a video frame", |env| {
            let drew = draw_frame(env, &loaded.reader, (width, height), rotation, size, pixels)?;
            let position = env
                .call_method(
                    &loaded.player,
                    jni_str!("getCurrentPosition"),
                    jni_sig!("()I"),
                    &[],
                )?
                .i()?;
            let playing = env
                .call_method(&loaded.player, jni_str!("isPlaying"), jni_sig!("()Z"), &[])?
                .z()?;
            Ok((drew, position, playing))
        });
        match result {
            Ok((drew, position, playing)) => {
                let position = f64::from(position.max(0)) / 1000.0;
                let mut changed = drew;
                if drew {
                    let image = ColorImage::from_rgba_premultiplied(size, &self.pixels);
                    self.texture.set(image, TextureOptions::LINEAR);
                    self.state.has_frame = true;
                }
                if !self.state.paused && (position - self.state.position).abs() > 0.001 {
                    self.state.position = position;
                    changed = true;
                }
                // It stopped on its own: the end of the video.
                if !self.state.paused && !playing {
                    self.state.paused = true;
                    self.state.eof = true;
                    self.state.position = self.state.duration;
                    changed = true;
                }
                if changed {
                    self.publish(ctx);
                }
            }
            Err(err) => self.report(Err(err), ctx),
        }
    }
}

/// The size before turning, and how far to turn it, of the video at `path`.
fn metadata(path: &str) -> Result<(usize, usize, u32), String> {
    with_env("read a video's details", |env| {
        let retriever = env.new_object(
            jni_str!("android/media/MediaMetadataRetriever"),
            jni_sig!("()V"),
            &[],
        )?;
        let file = env.new_string(path)?;
        let result = (|| -> jni::errors::Result<_> {
            env.call_method(
                &retriever,
                jni_str!("setDataSource"),
                jni_sig!("(Ljava/lang/String;)V"),
                &[JValue::Object(&file)],
            )?;
            let mut number = |key: i32| -> jni::errors::Result<u32> {
                let value = env
                    .call_method(
                        &retriever,
                        jni_str!("extractMetadata"),
                        jni_sig!("(I)Ljava/lang/String;"),
                        &[JValue::Int(key)],
                    )?
                    .l()?;
                if value.is_null() {
                    return Ok(0);
                }
                let value = env.cast_local::<JString>(value)?;
                Ok(value.try_to_string(env)?.trim().parse().unwrap_or(0))
            };
            Ok((
                number(KEY_WIDTH)?,
                number(KEY_HEIGHT)?,
                number(KEY_ROTATION)?,
            ))
        })();
        // Released either way, once what went wrong is out of the way.
        let caught = env.exception_catch();
        let _ = env.call_method(&retriever, jni_str!("release"), jni_sig!("()V"), &[]);
        if env.exception_check() {
            env.exception_clear();
        }
        caught?;
        let (width, height, rotation) = result?;
        Ok((width.max(1) as usize, height.max(1) as usize, rotation))
    })
}

/// Draws the newest of `reader`'s frames into `pixels`; whether it had one.
fn draw_frame(
    env: &mut Env,
    reader: &Ref,
    frame: (usize, usize),
    rotation: u32,
    size: [usize; 2],
    pixels: &mut Vec<u8>,
) -> jni::errors::Result<bool> {
    let image = env
        .call_method(
            reader,
            jni_str!("acquireLatestImage"),
            jni_sig!("()Landroid/media/Image;"),
            &[],
        )?
        .l()?;
    if image.is_null() {
        return Ok(false);
    }
    let result = copy_image(env, &image, frame, rotation, size, pixels);
    // Back to the reader, or it soon has none left to give.
    let caught = env.exception_catch();
    let _ = env.call_method(&image, jni_str!("close"), jni_sig!("()V"), &[]);
    if env.exception_check() {
        env.exception_clear();
    }
    caught?;
    result?;
    Ok(true)
}

fn copy_image(
    env: &mut Env,
    image: &JObject,
    (width, height): (usize, usize),
    rotation: u32,
    size: [usize; 2],
    pixels: &mut Vec<u8>,
) -> jni::errors::Result<()> {
    let planes = env
        .call_method(
            image,
            jni_str!("getPlanes"),
            jni_sig!("()[Landroid/media/Image$Plane;"),
            &[],
        )?
        .l()?;
    let planes = env.cast_local::<JObjectArray>(planes)?;
    let mut read = Vec::with_capacity(3);
    for index in 0..3 {
        let plane = planes.get_element(env, index)?;
        let buffer = env
            .call_method(
                &plane,
                jni_str!("getBuffer"),
                jni_sig!("()Ljava/nio/ByteBuffer;"),
                &[],
            )?
            .l()?;
        let row_stride = env
            .call_method(&plane, jni_str!("getRowStride"), jni_sig!("()I"), &[])?
            .i()?;
        let pixel_stride = env
            .call_method(&plane, jni_str!("getPixelStride"), jni_sig!("()I"), &[])?
            .i()?;
        let buffer = env.cast_local::<JByteBuffer>(buffer)?;
        let start = env.get_direct_buffer_address(&buffer)?;
        let length = env.get_direct_buffer_capacity(&buffer)?;
        // SAFETY: Android's own direct buffer, valid until the image closes,
        // which is after the last use of this.
        let data = unsafe { std::slice::from_raw_parts(start.cast_const(), length) };
        read.push(Plane {
            data,
            row_stride: row_stride.max(0) as usize,
            pixel_stride: pixel_stride.max(1) as usize,
        });
    }
    // Ten-bit video comes as two bytes a sample, with the top eight bits in the
    // second, while the image claims to be 8-bit. Its colors give it away: each
    // is four bytes on, not two. Seen on a Pixel 7a.
    if read.get(1).is_some_and(|u| u.pixel_stride == 4) {
        read[0].pixel_stride = 2;
        for plane in &mut read {
            plane.data = plane.data.get(1..).unwrap_or_default();
        }
    }
    let [y, u, v]: [Plane; 3] = read
        .try_into()
        .map_err(|_| jni::errors::Error::JavaException)?;
    yuv::to_rgba(&[y, u, v], (width, height), rotation, size, pixels);
    Ok(())
}
