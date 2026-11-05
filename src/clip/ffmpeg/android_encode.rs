//! Makes a file with Android's own classes, as tracks that each run on their
//! own and meet in a `MediaMuxer`. A track is copied from the file if it can
//! be, or decoded and encoded again if not.
//!
//! There's no Java of the app's own to take callbacks, so each track is pushed
//! on a little at a time, and asked for what it has.
//!
//! Video frames come out of the decoder as pictures in memory and go to the
//! encoder the same way, shrunk on the way by [`super::scale`]. Slower than
//! drawing between them, but it needs no OpenGL, and all of it can be tested.

use std::{
    path::Path,
    time::{Duration, Instant},
};

use jni::{
    Env, JValue, jni_sig, jni_str,
    objects::{JByteBuffer, JObject, JObjectArray},
    refs::Global,
};

use super::{
    AudioChoice, ClipJob, Codec, MAX_ATTEMPTS, MIN_VIDEO_BITRATE, Turn,
    android::{new_extractor, number_of, release, sample_time, seek_to, text_of, track_format},
    android_logic::{self, FrameRate},
    scale::{Orientation, Scaler, Source as PlaneIn, Target as PlaneOut},
    smaller_bitrate,
    tempo::Tempo,
};
use crate::{android::with_env, clip::crop::Crop, jobs::Progress};

type Ref = Global<JObject<'static>>;

/// How long a wait for a codec may be, in microseconds: short, since there's
/// always another to look at.
const WAIT: i64 = 1000;
/// `MediaCodec`'s answers instead of a buffer.
const OUTPUT_FORMAT_CHANGED: i32 = -2;
/// `MediaCodec.BUFFER_FLAG_*`.
const FLAG_KEY_FRAME: i32 = 1;
const FLAG_CODEC_CONFIG: i32 = 2;
const FLAG_END_OF_STREAM: i32 = 4;
const FLAG_PARTIAL_FRAME: i32 = 8;
/// `MediaExtractor.SAMPLE_FLAG_*`.
const SAMPLE_SYNC: i32 = 1;
const SAMPLE_PARTIAL: i32 = 4;
/// `MediaCodec.CONFIGURE_FLAG_ENCODE`.
const CONFIGURE_ENCODE: i32 = 1;
/// `MediaCodecInfo.CodecCapabilities.COLOR_FormatYUV420Flexible`.
const YUV_FLEXIBLE: i32 = 0x7F42_0888;
/// `MediaFormat.COLOR_TRANSFER_SDR_VIDEO`.
const TRANSFER_SDR: i32 = 3;
/// `AudioFormat.ENCODING_PCM_16BIT`.
const PCM_16: i32 = 2;
/// `MediaCodecInfo.EncoderCapabilities.BITRATE_MODE_*`.
const BITRATE_VBR: i32 = 1;
const BITRATE_CBR: i32 = 2;
/// Seconds between keyframes: what makes a video easy to seek in.
const KEYFRAME_SECONDS: i32 = 2;
/// Room for one sample being copied, unless the track says it needs more.
const SAMPLE_BUFFER: i32 = 8 << 20;
/// How far past the end to go on reading, for pictures stored out of order.
const LOOK_PAST_END: i64 = 2_000_000;
/// With nothing happening for this long, something's stuck.
const STUCK: Duration = Duration::from_secs(30);
/// A turn this long means the app itself was stopped for a while.
const FROZEN: Duration = Duration::from_secs(2);

/// Writes the output: at a target size, as many times as it takes to fit.
pub fn make(
    job: &ClipJob,
    extension: &str,
    temporary: &Path,
    length: f64,
    p: &Progress,
) -> Result<(), String> {
    let mut rates = Rates::for_job(job, length)?;
    let Some(limit) = job.settings.target_bytes() else {
        return encode(job, extension, temporary, rates, p);
    };
    for attempt in 0..MAX_ATTEMPTS {
        if p.cancelled() {
            return Ok(());
        }
        let _ = disk::remove_file(temporary);
        p.set_status(&if attempt == 0 {
            format!("{}: encoding", job.name)
        } else {
            format!("{}: was too big, encoding smaller", job.name)
        });
        p.set_time_done(0.0);
        encode(job, extension, temporary, rates, p)?;
        if p.cancelled() {
            return Ok(());
        }
        let size = disk::metadata(temporary)
            .map_err(|err| err.to_string())?
            .len() as f64;
        if size <= limit {
            return Ok(());
        }
        let video = rates.video_bps.unwrap_or(0.0);
        // What matters is how long it plays for.
        let next = smaller_bitrate(video, size, limit, length / job.settings.speed);
        if next < MIN_VIDEO_BITRATE {
            break;
        }
        rates.video_bps = Some(next);
    }
    Err(format!(
        "Couldn't get it under {} MB. Try a lower resolution or frame rate.",
        job.settings.target_mb
    ))
}

/// The bitrates asked for: `None` where there's nothing that's asked, so it's
/// up to the quality, or copied.
#[derive(Clone, Copy)]
struct Rates {
    video_bps: Option<f64>,
    audio_kbps: Option<u32>,
}

impl Rates {
    fn for_job(job: &ClipJob, length: f64) -> Result<Self, String> {
        if job.settings.target_bytes().is_some() {
            let fit = job.settings.fit(&job.source, length)?;
            return Ok(Self {
                video_bps: Some(fit.video_bps),
                audio_kbps: Some(fit.audio_kbps),
            });
        }
        Ok(Self {
            video_bps: None,
            audio_kbps: job.settings.audio_kbps,
        })
    }
}

fn encode(
    job: &ClipJob,
    extension: &str,
    output: &Path,
    rates: Rates,
    p: &Progress,
) -> Result<(), String> {
    if job.settings.audio == AudioChoice::None && job.settings.format.is_audio_only() {
        return Err("There's no audio chosen to save.".to_owned());
    }
    if !job.settings.copies(&job.source)
        && !job.settings.format.is_audio_only()
        && let Ok((_, Some(codec))) = job.settings.container(&job.source, extension)
        && !job.tools.encoders.contains(mime_of(codec))
    {
        return Err(format!(
            "This phone has no {} encoder. Choose another format.",
            codec_label(codec)
        ));
    }
    let input = job
        .input
        .to_str()
        .ok_or("the file's name isn't valid Unicode")?;
    let output = output
        .to_str()
        .ok_or("the file's name isn't valid Unicode")?;
    with_env("export a video", |env| {
        let mut tracks = Vec::new();
        let result = (|| -> jni::errors::Result<Result<(), String>> {
            if !job.settings.format.is_audio_only() {
                tracks.push(video_track(env, job, input, extension, rates)?);
            }
            for stream in job.settings.audio_streams(&job.source) {
                tracks.push(audio_track(
                    env,
                    job,
                    input,
                    extension,
                    stream.index as i32,
                    &stream.codec,
                    rates,
                )?);
            }
            run(env, job, extension, output, &mut tracks, p)
        })();
        let caught = env.exception_catch();
        for track in &mut tracks {
            track.close(env);
        }
        caught?;
        result
    })?
}

/// Runs the tracks into a new muxer.
fn run(
    env: &mut Env,
    job: &ClipJob,
    extension: &str,
    output: &str,
    tracks: &mut [Track],
    p: &Progress,
) -> jni::errors::Result<Result<(), String>> {
    let file = env.new_string(output)?;
    let muxer = env.new_object(
        jni_str!("android/media/MediaMuxer"),
        jni_sig!("(Ljava/lang/String;I)V"),
        &[
            JValue::Object(&file),
            JValue::Int(android_logic::muxer_format(extension)),
        ],
    )?;
    let result = (|| {
        if !job.settings.format.is_audio_only() && !bakes_rotation(job, extension) {
            env.call_method(
                &muxer,
                jni_str!("setOrientationHint"),
                jni_sig!("(I)V"),
                &[JValue::Int(job.source.rotation as i32)],
            )?;
        }
        let location = job
            .source
            .location
            .first()
            .filter(|_| job.settings.keep.location && extension != "webm")
            .and_then(|(_, text)| android_logic::parse_location(text));
        if let Some((latitude, longitude)) = location {
            env.call_method(
                &muxer,
                jni_str!("setLocation"),
                jni_sig!("(FF)V"),
                &[JValue::Float(latitude), JValue::Float(longitude)],
            )?;
        }
        drive(env, &muxer, tracks, p)
    })();
    // Whatever's made, it's only worth stopping for if it's all there.
    release(env, &muxer, result)
}

/// Pushes the tracks on till they're done. They all need to have said what
/// they'll write before the muxer can start, which encoders only know after
/// their first frames; after that, whichever is furthest behind goes next, so
/// they're written side by side.
fn drive(
    env: &mut Env,
    muxer: &JObject,
    tracks: &mut [Track],
    p: &Progress,
) -> jni::errors::Result<Result<(), String>> {
    let mut started = false;
    let mut moved_at = Instant::now();
    let mut last_turn = Instant::now();
    loop {
        if p.cancelled() {
            return Ok(Ok(()));
        }
        // A long gap between turns is Android having stopped the app, not a
        // stuck encoder, so the wait starts again.
        if last_turn.elapsed() > FROZEN {
            moved_at = Instant::now();
        }
        last_turn = Instant::now();
        let moved = if started {
            let Some(next) = tracks
                .iter_mut()
                .filter(|track| !track.finished())
                .min_by_key(|track| track.written())
            else {
                break;
            };
            env.with_local_frame(32, |env| next.push(env, Some(muxer)))?
        } else {
            let mut moved = false;
            for track in tracks.iter_mut() {
                moved |= env.with_local_frame(32, |env| track.push(env, None))?;
            }
            if tracks.iter().all(Track::is_ready) {
                for track in tracks.iter_mut() {
                    track.add_to(env, muxer)?;
                }
                if !tracks.iter().any(|track| track.output.is_some()) {
                    return Ok(Err("There was nothing in that part of the file.".to_owned()));
                }
                env.call_method(muxer, jni_str!("start"), jni_sig!("()V"), &[])?;
                started = true;
            }
            moved
        };
        if moved {
            moved_at = Instant::now();
            let written = tracks.iter().map(Track::written).min().unwrap_or(0);
            p.set_time_done(written as f64 / 1e6);
        } else if moved_at.elapsed() > STUCK {
            return Ok(Err(
                "The phone's encoder stopped responding. Try again, or a lower resolution."
                    .to_owned(),
            ));
        }
    }
    if tracks
        .first()
        .is_some_and(|track| track.is_video && track.output.is_none())
    {
        return Ok(Err(
            "There were no frames in that part of the file.".to_owned()
        ));
    }
    env.call_method(muxer, jni_str!("stop"), jni_sig!("()V"), &[])?;
    Ok(Ok(()))
}

/// A track on its way to the muxer.
struct Track {
    is_video: bool,
    /// The muxer's number for it, once added.
    output: Option<i32>,
    /// What it'll write, once known.
    format: Option<Ref>,
    /// How far it's got, in microseconds of the output.
    written: i64,
    finished: bool,
    how: How,
}

enum How {
    Copy(Copied),
    Video(Box<VideoCodec>),
    Audio(Box<AudioCodec>),
}

impl Track {
    fn written(&self) -> i64 {
        self.written
    }

    fn finished(&self) -> bool {
        self.finished
    }

    /// Whether the muxer can start, as far as this track goes.
    fn is_ready(&self) -> bool {
        self.format.is_some() || self.finished
    }

    fn add_to(&mut self, env: &mut Env, muxer: &JObject) -> jni::errors::Result<()> {
        if let Some(format) = &self.format {
            let output = env
                .call_method(
                    muxer,
                    jni_str!("addTrack"),
                    jni_sig!("(Landroid/media/MediaFormat;)I"),
                    &[JValue::Object(format)],
                )?
                .i()?;
            self.output = Some(output);
        }
        Ok(())
    }

    /// Does a little; whether anything came of it. With no muxer yet, stops
    /// short of writing anything.
    fn push(&mut self, env: &mut Env, muxer: Option<&JObject>) -> jni::errors::Result<bool> {
        match &mut self.how {
            How::Copy(copy) => {
                if self.format.is_none() {
                    self.format = Some(env.new_global_ref(&copy.format)?);
                }
                let Some(muxer) = muxer else {
                    return Ok(false);
                };
                let output = self.output.unwrap_or(0);
                copy.push(env, muxer, output, &mut self.written, &mut self.finished)?;
                Ok(true)
            }
            How::Video(video) => {
                let mut moved = video.feed(env)?;
                moved |= video.pass_frames(env)?;
                if let Some(encoder) = &mut video.encoder {
                    moved |= encoder.drain(
                        env,
                        muxer,
                        self.output,
                        &mut self.format,
                        &mut self.written,
                        &mut self.finished,
                    )?;
                } else if video.decoded_all {
                    self.finished = true;
                }
                Ok(moved)
            }
            How::Audio(audio) => {
                let mut moved = audio.feed(env)?;
                moved |= audio.pass_sound(env)?;
                if let Some(encoder) = &mut audio.encoder {
                    moved |= encoder.drain(
                        env,
                        muxer,
                        self.output,
                        &mut self.format,
                        &mut self.written,
                        &mut self.finished,
                    )?;
                } else if audio.decoded_all {
                    // There was no sound in that part of it.
                    self.finished = true;
                }
                Ok(moved)
            }
        }
    }

    /// Lets go of what it holds, whatever went wrong before.
    fn close(&mut self, env: &mut Env) {
        let mut objects: Vec<&Ref> = Vec::new();
        match &self.how {
            How::Copy(copy) => objects.push(&copy.extractor),
            How::Video(video) => {
                objects.extend([&video.reader.extractor, &video.reader.decoder]);
                objects.extend(video.encoder.as_ref().map(|encoder| &encoder.codec));
            }
            How::Audio(audio) => {
                objects.extend([&audio.reader.extractor, &audio.reader.decoder]);
                objects.extend(audio.encoder.as_ref().map(|encoder| &encoder.codec));
            }
        }
        for object in objects {
            let _ = env.call_method(object, jni_str!("release"), jni_sig!("()V"), &[]);
            if env.exception_check() {
                env.exception_clear();
            }
        }
    }
}

fn opened_extractor(env: &mut Env, input: &str, track: i32, from: f64) -> jni::errors::Result<Ref> {
    let extractor = new_extractor(env, input)?;
    env.call_method(
        &extractor,
        jni_str!("selectTrack"),
        jni_sig!("(I)V"),
        &[JValue::Int(track)],
    )?;
    seek_to(env, &extractor, from)?;
    env.new_global_ref(&extractor)
}

fn micros(seconds: f64) -> i64 {
    (seconds * 1_000_000.0) as i64
}

fn video_track(
    env: &mut Env,
    job: &ClipJob,
    input: &str,
    extension: &str,
    rates: Rates,
) -> jni::errors::Result<Track> {
    let index = job.source.video_index as i32;
    let settings = &job.settings;
    let how = if settings.copies(&job.source) {
        How::Copy(Copied::new(env, job, input, index)?)
    } else {
        let codec = match settings.container(&job.source, extension) {
            Ok((_, Some(codec))) => codec,
            _ => Codec::H264,
        };
        let reader = Reader::new(env, job, input, index, job.source.is_hdr())?;
        How::Video(Box::new(VideoCodec {
            reader,
            encoder: None,
            frames: FrameRate::new(
                micros(job.start),
                settings.fps_to(&job.source),
                job.source.fps(),
                settings.speed,
            ),
            speed: settings.speed,
            range: (micros(job.start), micros(job.end)),
            waiting: None,
            sent_end: false,
            decoded_all: false,
            mime: mime_of(codec),
            is_h264: codec == Codec::H264,
            side: settings.scale_to(&job.source),
            // Slower has the same frames for longer, so there are fewer each second.
            fps: settings
                .fps_to(&job.source)
                .map_or(job.source.fps(), f64::from)
                * settings.speed.min(1.0),
            quality_bpp: settings.quality.pick(0.14, 0.08, 0.04),
            video_bps: rates.video_bps,
            source_bps: source_bps(job),
            size: (0, 0),
            rect: (0, 0, 0, 0),
            crop: settings.crop,
            rotation: job.source.rotation,
            way: orientation(job, extension),
            scaler: [Scaler::default(), Scaler::default()],
        }))
    };
    Ok(Track {
        is_video: true,
        output: None,
        format: None,
        written: 0,
        finished: false,
        how,
    })
}

/// Whether the frames themselves are turned upright, rather than a note left for
/// the player. WebM can't hold such a note, and a picture that's being cropped,
/// turned or mirrored may as well come out plain.
fn bakes_rotation(job: &ClipJob, extension: &str) -> bool {
    let settings = &job.settings;
    !settings.copies(&job.source)
        && (extension == "webm"
            || settings.crop.is_some()
            || settings.turn != Turn::None
            || settings.flip_horizontal
            || settings.flip_vertical)
}

/// Which way frames are drawn, given what's asked.
fn orientation(job: &ClipJob, extension: &str) -> Orientation {
    if !bakes_rotation(job, extension) {
        return Orientation::default();
    }
    let settings = &job.settings;
    Orientation {
        turn: (job.source.rotation + settings.turn.degrees()) % 360,
        flip_horizontal: settings.flip_horizontal,
        flip_vertical: settings.flip_vertical,
    }
}

fn source_bps(job: &ClipJob) -> f64 {
    job.source.size as f64 * 8.0 / job.source.duration.max(0.001)
}

fn codec_label(codec: Codec) -> &'static str {
    match codec {
        Codec::H264 => "H.264",
        Codec::Hevc => "H.265",
        Codec::Av1 => "AV1",
        Codec::Vp9 => "VP9",
    }
}

fn mime_of(codec: Codec) -> &'static str {
    match codec {
        Codec::H264 => "video/avc",
        Codec::Hevc => "video/hevc",
        Codec::Av1 => "video/av01",
        Codec::Vp9 => "video/x-vnd.on2.vp9",
    }
}

fn audio_track(
    env: &mut Env,
    job: &ClipJob,
    input: &str,
    extension: &str,
    index: i32,
    codec: &str,
    rates: Rates,
) -> jni::errors::Result<Track> {
    let speed = job.settings.speed;
    let how = if rates.audio_kbps.is_none()
        && !job.settings.changes_speed()
        && android_logic::audio_copies(extension, codec)
    {
        How::Copy(Copied::new(env, job, input, index)?)
    } else {
        let (mime, usual) = android_logic::audio_encoder(extension);
        How::Audio(Box::new(AudioCodec {
            reader: Reader::new(env, job, input, index, false)?,
            encoder: None,
            mime,
            kbps: rates.audio_kbps.unwrap_or(usual),
            range: (micros(job.start), micros(job.end)),
            speed,
            tempo: None,
            stretched: Vec::new(),
            flushed: false,
            rate: 0,
            channels: 0,
            waiting: None,
            first: None,
            sent_frames: 0,
            sent_end: false,
            decoded_all: false,
        }))
    };
    Ok(Track {
        is_video: false,
        output: None,
        format: None,
        written: 0,
        finished: false,
        how,
    })
}

/// A track taken from the file as it is.
struct Copied {
    extractor: Ref,
    format: Ref,
    buffer: Ref,
    info: Ref,
    range: (i64, i64),
}

impl Copied {
    fn new(env: &mut Env, job: &ClipJob, input: &str, index: i32) -> jni::errors::Result<Self> {
        let extractor = opened_extractor(env, input, index, job.start)?;
        let format = track_format(env, &extractor, index)?;
        let room = number_of(env, &format, "max-input-size")?
            .map_or(SAMPLE_BUFFER, |size| SAMPLE_BUFFER.max(size as i32));
        let buffer = env
            .call_static_method(
                jni_str!("java/nio/ByteBuffer"),
                jni_str!("allocateDirect"),
                jni_sig!("(I)Ljava/nio/ByteBuffer;"),
                &[JValue::Int(room)],
            )?
            .l()?;
        Ok(Self {
            format: env.new_global_ref(&format)?,
            buffer: env.new_global_ref(&buffer)?,
            info: new_info(env)?,
            extractor,
            range: (micros(job.start), micros(job.end)),
        })
    }

    fn push(
        &mut self,
        env: &mut Env,
        muxer: &JObject,
        output: i32,
        written: &mut i64,
        finished: &mut bool,
    ) -> jni::errors::Result<()> {
        let (start, end) = self.range;
        let time = sample_time(env, &self.extractor)?;
        let size = read_sample(env, &self.extractor, &self.buffer)?;
        // Past the end by enough that nothing more's coming: pictures stored
        // out of order can still be ahead of it.
        if size < 0 || time < 0 || time > end + LOOK_PAST_END {
            *finished = true;
            return Ok(());
        }
        // Before the start, audio that came with the keyframe; after the end,
        // what was read past it.
        if time >= start && time <= end {
            let flags = env
                .call_method(
                    &self.extractor,
                    jni_str!("getSampleFlags"),
                    jni_sig!("()I"),
                    &[],
                )?
                .i()?;
            let mut buffer_flags = 0;
            if flags & SAMPLE_SYNC != 0 {
                buffer_flags |= FLAG_KEY_FRAME;
            }
            if flags & SAMPLE_PARTIAL != 0 {
                buffer_flags |= FLAG_PARTIAL_FRAME;
            }
            set_info(env, &self.info, 0, size, time - start, buffer_flags)?;
            write_sample(env, muxer, output, &self.buffer, &self.info)?;
            *written = (*written).max(time - start);
        }
        let more = env
            .call_method(&self.extractor, jni_str!("advance"), jni_sig!("()Z"), &[])?
            .z()?;
        if !more {
            *finished = true;
        }
        Ok(())
    }
}

/// A track read from the file and decoded.
struct Reader {
    extractor: Ref,
    decoder: Ref,
    info: Ref,
    /// How far ahead to stop reading.
    stop_after: i64,
    read_all: bool,
}

impl Reader {
    fn new(
        env: &mut Env,
        job: &ClipJob,
        input: &str,
        index: i32,
        to_sdr: bool,
    ) -> jni::errors::Result<Self> {
        let extractor = opened_extractor(env, input, index, job.start)?;
        let format = track_format(env, &extractor, index)?;
        let mime = text_of(env, &format, "mime")?.unwrap_or_default();
        let is_video = mime.starts_with("video/");
        if is_video {
            set_int(env, &format, "color-format", YUV_FLEXIBLE)?;
            if to_sdr {
                // Android's own tone mapping, on the phones that have it.
                set_int(env, &format, "color-transfer-request", TRANSFER_SDR)?;
            }
        } else {
            set_int(env, &format, "pcm-encoding", PCM_16)?;
        }
        let decoder = create_codec(env, &mime, false)?;
        configure(env, &decoder, &format, 0)?;
        start(env, &decoder)?;
        Ok(Self {
            extractor,
            decoder,
            info: new_info(env)?,
            stop_after: micros(job.end) + LOOK_PAST_END,
            read_all: false,
        })
    }

    /// Hands the decoder a sample, if it has room for one.
    fn feed(&mut self, env: &mut Env) -> jni::errors::Result<bool> {
        if self.read_all {
            return Ok(false);
        }
        let index = dequeue_input(env, &self.decoder, 0)?;
        if index < 0 {
            return Ok(false);
        }
        let buffer = input_buffer(env, &self.decoder, index)?;
        let time = sample_time(env, &self.extractor)?;
        let size = read_sample(env, &self.extractor, &buffer)?;
        if size < 0 || time < 0 || time > self.stop_after {
            queue_input(env, &self.decoder, index, 0, 0, FLAG_END_OF_STREAM)?;
            self.read_all = true;
            return Ok(true);
        }
        queue_input(env, &self.decoder, index, size, time, 0)?;
        env.call_method(&self.extractor, jni_str!("advance"), jni_sig!("()Z"), &[])?;
        Ok(true)
    }
}

/// An encoder, and what comes out of it.
struct Encoder {
    codec: Ref,
    info: Ref,
}

impl Encoder {
    /// Passes on what's come out, once there's somewhere to put it. Until then
    /// it only waits to hear the format, as what's behind that can wait.
    fn drain(
        &mut self,
        env: &mut Env,
        muxer: Option<&JObject>,
        output: Option<i32>,
        format: &mut Option<Ref>,
        written: &mut i64,
        finished: &mut bool,
    ) -> jni::errors::Result<bool> {
        if format.is_some() && muxer.is_none() {
            return Ok(false);
        }
        let index = dequeue_output(env, &self.codec, &self.info, 0)?;
        if index == OUTPUT_FORMAT_CHANGED {
            let new = output_format(env, &self.codec)?;
            *format = Some(env.new_global_ref(&new)?);
            return Ok(true);
        }
        if index < 0 {
            return Ok(false);
        }
        let (_, size, time, flags) = read_info(env, &self.info)?;
        if size > 0
            && flags & FLAG_CODEC_CONFIG == 0
            && let (Some(muxer), Some(output)) = (muxer, output)
        {
            let buffer = output_buffer(env, &self.codec, index)?;
            write_sample(env, muxer, output, &buffer, &self.info)?;
            *written = (*written).max(time);
        }
        release_output(env, &self.codec, index)?;
        if flags & FLAG_END_OF_STREAM != 0 {
            *finished = true;
        }
        Ok(true)
    }
}

/// A video track decoded and encoded again.
struct VideoCodec {
    reader: Reader,
    encoder: Option<Encoder>,
    frames: FrameRate,
    speed: f64,
    range: (i64, i64),
    /// A decoded frame waiting for room in the encoder, and its time.
    waiting: Option<(i32, i64)>,
    sent_end: bool,
    decoded_all: bool,
    mime: &'static str,
    is_h264: bool,
    side: Option<u32>,
    fps: f64,
    quality_bpp: f64,
    video_bps: Option<f64>,
    source_bps: f64,
    size: (u32, u32),
    /// The part of each stored frame that's used: x, y, width and height.
    rect: (u32, u32, u32, u32),
    /// What's kept of the upright picture.
    crop: Option<Crop>,
    /// How far the frames are stored short of upright.
    rotation: u32,
    way: Orientation,
    /// For the brightness and for the colors, which are a different size.
    scaler: [Scaler; 2],
}

impl VideoCodec {
    fn feed(&mut self, env: &mut Env) -> jni::errors::Result<bool> {
        self.reader.feed(env)
    }

    /// Takes a frame from the decoder and gives it to the encoder.
    fn pass_frames(&mut self, env: &mut Env) -> jni::errors::Result<bool> {
        let mut moved = false;
        let (start, end) = self.range;
        if self.waiting.is_none() && !self.decoded_all {
            let decoder = &self.reader.decoder;
            let index = dequeue_output(env, decoder, &self.reader.info, WAIT)?;
            if index >= 0 {
                moved = true;
                let (_, size, time, flags) = read_info(env, &self.reader.info)?;
                if time >= end && flags & FLAG_END_OF_STREAM == 0 {
                    release_output(env, decoder, index)?;
                    self.decoded_all = true;
                } else if size > 0 && time >= start && time < end && self.frames.keeps(time) {
                    self.waiting = Some((index, time));
                } else {
                    release_output(env, decoder, index)?;
                }
                if flags & FLAG_END_OF_STREAM != 0 {
                    self.decoded_all = true;
                }
            }
        }
        if let Some((index, time)) = self.waiting {
            if self.encoder.is_none() {
                self.start_encoder(env, index)?;
            }
            let Some(encoder) = &self.encoder else {
                return Ok(moved);
            };
            let room = dequeue_input(env, &encoder.codec, WAIT)?;
            if room >= 0 {
                copy_frame(
                    env,
                    &self.reader.decoder,
                    index,
                    &encoder.codec,
                    room,
                    ((time - start) as f64 / self.speed) as i64,
                    Frame {
                        size: self.size,
                        rect: self.rect,
                        way: self.way,
                    },
                    &mut self.scaler,
                )?;
                release_output(env, &self.reader.decoder, index)?;
                self.waiting = None;
                moved = true;
            }
        } else if self.decoded_all
            && !self.sent_end
            && let Some(encoder) = &self.encoder
        {
            let room = dequeue_input(env, &encoder.codec, WAIT)?;
            if room >= 0 {
                queue_input(env, &encoder.codec, room, 0, 0, FLAG_END_OF_STREAM)?;
                self.sent_end = true;
                moved = true;
            }
        }
        Ok(moved)
    }

    /// Starts the encoder, now the first frame says what the picture really is.
    fn start_encoder(&mut self, env: &mut Env, index: i32) -> jni::errors::Result<()> {
        let image = output_image(env, &self.reader.decoder, index)?;
        let (_, _, width, height) = crop_of(env, &image)?;
        close_image(env, &image)?;
        let stored = (width as u32, height as u32);
        let upright = if self.rotation % 180 == 90 {
            (stored.1, stored.0)
        } else {
            stored
        };
        self.rect = match self.crop {
            Some(crop) => {
                let (crop_width, crop_height, x, y) = crop.pixels(upright.0, upright.1);
                android_logic::stored_rect((x, y, crop_width, crop_height), self.rotation, stored)
            }
            None => (0, 0, stored.0, stored.1),
        };
        let (_, _, used_width, used_height) = self.rect;
        let drawn = if self.way.turn % 180 == 90 {
            (used_height, used_width)
        } else {
            (used_width, used_height)
        };
        let (out_width, out_height) = android_logic::output_size(drawn, self.side);
        self.size = (out_width, out_height);
        let per_second = f64::from(out_width) * f64::from(out_height) * self.fps;
        let bitrate = self.video_bps.unwrap_or_else(|| {
            android_logic::video_bitrate(
                self.quality_bpp,
                self.is_h264,
                per_second,
                self.source_bps,
            )
        });
        let mime = env.new_string(self.mime)?;
        let format = env
            .call_static_method(
                jni_str!("android/media/MediaFormat"),
                jni_str!("createVideoFormat"),
                jni_sig!("(Ljava/lang/String;II)Landroid/media/MediaFormat;"),
                &[
                    JValue::Object(&mime),
                    JValue::Int(out_width as i32),
                    JValue::Int(out_height as i32),
                ],
            )?
            .l()?;
        set_int(env, &format, "color-format", YUV_FLEXIBLE)?;
        set_int(env, &format, "bitrate", bitrate as i32)?;
        set_int(env, &format, "frame-rate", self.fps.round().max(1.0) as i32)?;
        set_int(env, &format, "i-frame-interval", KEYFRAME_SECONDS)?;
        // The same numbers mean the same colors only if it's told what they are.
        let decoded = output_format(env, &self.reader.decoder)?;
        for key in ["color-standard", "color-range", "color-transfer"] {
            if let Some(value) = number_of(env, &decoded, key)? {
                set_int(env, &format, key, value as i32)?;
            }
        }
        let codec = create_codec(env, self.mime, true)?;
        // Holding to a rate is what lands near a size, where it can.
        let mode = if self.video_bps.is_some() && supports_cbr(env, &codec, self.mime)? {
            BITRATE_CBR
        } else {
            BITRATE_VBR
        };
        set_int(env, &format, "bitrate-mode", mode)?;
        configure(env, &codec, &format, CONFIGURE_ENCODE)?;
        start(env, &codec)?;
        self.encoder = Some(Encoder {
            codec,
            info: new_info(env)?,
        });
        Ok(())
    }
}

/// Whether the encoder can hold to a constant bitrate.
fn supports_cbr(env: &mut Env, codec: &JObject, mime: &str) -> jni::errors::Result<bool> {
    let info = env
        .call_method(
            codec,
            jni_str!("getCodecInfo"),
            jni_sig!("()Landroid/media/MediaCodecInfo;"),
            &[],
        )?
        .l()?;
    let mime = env.new_string(mime)?;
    let capabilities = env
        .call_method(
            &info,
            jni_str!("getCapabilitiesForType"),
            jni_sig!("(Ljava/lang/String;)Landroid/media/MediaCodecInfo$CodecCapabilities;"),
            &[JValue::Object(&mime)],
        )?
        .l()?;
    let encoder = env
        .call_method(
            &capabilities,
            jni_str!("getEncoderCapabilities"),
            jni_sig!("()Landroid/media/MediaCodecInfo$EncoderCapabilities;"),
            &[],
        )?
        .l()?;
    env.call_method(
        &encoder,
        jni_str!("isBitrateModeSupported"),
        jni_sig!("(I)Z"),
        &[JValue::Int(BITRATE_CBR)],
    )?
    .z()
}

/// One plane of an `Image`, valid until it closes.
struct ImagePlane {
    start: *mut u8,
    length: usize,
    row_stride: usize,
    pixel_stride: usize,
}

fn planes_of(env: &mut Env, image: &JObject) -> jni::errors::Result<[ImagePlane; 3]> {
    let planes = env
        .call_method(
            image,
            jni_str!("getPlanes"),
            jni_sig!("()[Landroid/media/Image$Plane;"),
            &[],
        )?
        .l()?;
    let planes = env.cast_local::<JObjectArray>(planes)?;
    let mut read = |index: usize| -> jni::errors::Result<ImagePlane> {
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
        Ok(ImagePlane {
            start: env.get_direct_buffer_address(&buffer)?,
            length: env.get_direct_buffer_capacity(&buffer)?,
            row_stride: row_stride.max(0) as usize,
            pixel_stride: pixel_stride.max(1) as usize,
        })
    };
    Ok([read(0)?, read(1)?, read(2)?])
}

/// The part of an `Image` that's picture: its left, top, width and height.
fn crop_of(env: &mut Env, image: &JObject) -> jni::errors::Result<(usize, usize, usize, usize)> {
    let rect = env
        .call_method(
            image,
            jni_str!("getCropRect"),
            jni_sig!("()Landroid/graphics/Rect;"),
            &[],
        )?
        .l()?;
    let mut field = |name: &str| -> jni::errors::Result<i32> {
        let value = match name {
            "left" => env.get_field(&rect, jni_str!("left"), jni_sig!("I"))?,
            "top" => env.get_field(&rect, jni_str!("top"), jni_sig!("I"))?,
            "right" => env.get_field(&rect, jni_str!("right"), jni_sig!("I"))?,
            _ => env.get_field(&rect, jni_str!("bottom"), jni_sig!("I"))?,
        };
        value.i()
    };
    let (left, top) = (field("left")?, field("top")?);
    let (right, bottom) = (field("right")?, field("bottom")?);
    Ok((
        left.max(0) as usize,
        top.max(0) as usize,
        (right - left).max(0) as usize,
        (bottom - top).max(0) as usize,
    ))
}

/// How a frame goes from the decoder to the encoder.
struct Frame {
    /// The encoder's width and height.
    size: (u32, u32),
    /// The part used of what's decoded: x, y, width and height.
    rect: (u32, u32, u32, u32),
    way: Orientation,
}

/// Moves a frame from the decoder to the encoder, at the encoder's size.
#[allow(clippy::too_many_arguments)]
fn copy_frame(
    env: &mut Env,
    decoder: &JObject,
    index: i32,
    encoder: &JObject,
    room: i32,
    time: i64,
    frame: Frame,
    scalers: &mut [Scaler; 2],
) -> jni::errors::Result<()> {
    let (width, height) = (frame.size.0 as usize, frame.size.1 as usize);
    let (rect_x, rect_y) = (frame.rect.0 as usize, frame.rect.1 as usize);
    let (rect_width, rect_height) = (frame.rect.2 as usize, frame.rect.3 as usize);
    let image = output_image(env, decoder, index)?;
    let target = input_image(env, encoder, room)?;
    let result = (|| -> jni::errors::Result<()> {
        let (left, top, _, _) = crop_of(env, &image)?;
        let (left, top) = (left + rect_x, top + rect_y);
        let mut from = planes_of(env, &image)?;
        // Ten-bit video comes as two bytes a sample, with the top eight bits in
        // the second, while the image claims to be 8-bit. Its colors give it
        // away: each is four bytes on, not two. Only where it isn't tone mapped.
        let high_byte = usize::from(from[1].pixel_stride == 4);
        if high_byte == 1 {
            from[0].pixel_stride = 2;
        }
        let to = planes_of(env, &target)?;
        for (plane, (from, to)) in from.iter().zip(&to).enumerate() {
            let chroma = usize::from(plane > 0);
            // SAFETY: Android's own direct buffers, valid until the images
            // close, after this. U and V can share memory, but only one
            // plane's target exists at a time, and the source is another image.
            let (data, out) = unsafe {
                (
                    std::slice::from_raw_parts(from.start.cast_const(), from.length),
                    std::slice::from_raw_parts_mut(to.start, to.length),
                )
            };
            let source = PlaneIn {
                data,
                offset: (top >> chroma) * from.row_stride
                    + (left >> chroma) * from.pixel_stride
                    + high_byte,
                row_stride: from.row_stride,
                pixel_stride: from.pixel_stride,
                width: (rect_width + chroma) >> chroma,
                height: (rect_height + chroma) >> chroma,
            };
            let mut target = PlaneOut {
                data: out,
                row_stride: to.row_stride,
                pixel_stride: to.pixel_stride,
                width: width >> chroma,
                height: height >> chroma,
            };
            scalers[chroma].scale_oriented(source, &mut target, frame.way);
        }
        Ok(())
    })();
    let caught = env.exception_catch();
    let closed = close_image(env, &target).and(close_image(env, &image));
    caught?;
    result?;
    closed?;
    let size = width * height * 3 / 2;
    queue_input(env, encoder, room, size as i32, time, 0)
}

/// Decoded sound waiting for room in the encoder.
#[derive(Clone, Copy)]
struct Sound {
    index: i32,
    offset: usize,
    remaining: usize,
}

/// An audio track decoded and encoded again.
struct AudioCodec {
    reader: Reader,
    encoder: Option<Encoder>,
    mime: &'static str,
    kbps: u32,
    range: (i64, i64),
    speed: f64,
    /// Changing the speed, and what's come of it waiting for the encoder.
    tempo: Option<Tempo>,
    stretched: Vec<u8>,
    flushed: bool,
    rate: u32,
    channels: u32,
    waiting: Option<Sound>,
    /// When the first sound kept is in the output.
    first: Option<i64>,
    sent_frames: u64,
    sent_end: bool,
    decoded_all: bool,
}

impl AudioCodec {
    fn feed(&mut self, env: &mut Env) -> jni::errors::Result<bool> {
        self.reader.feed(env)
    }

    fn frame_bytes(&self) -> usize {
        self.channels.max(1) as usize * 2
    }

    fn read_format(&mut self, env: &mut Env) -> jni::errors::Result<()> {
        let format = output_format(env, &self.reader.decoder)?;
        self.rate = number_of(env, &format, "sample-rate")?.map_or(self.rate, |rate| rate as u32);
        self.channels =
            number_of(env, &format, "channel-count")?.map_or(self.channels, |count| count as u32);
        Ok(())
    }

    fn changes_speed(&self) -> bool {
        (self.speed - 1.0).abs() > 0.001
    }

    /// Passes on what the speed change has made.
    fn pass_stretched(&mut self, env: &mut Env) -> jni::errors::Result<bool> {
        let mut moved = false;
        if self.stretched.is_empty() && !self.decoded_all {
            moved |= self.take_sound(env)?;
        }
        if self.decoded_all && !self.flushed {
            self.flushed = true;
            if let Some(tempo) = &mut self.tempo {
                let mut samples = Vec::new();
                tempo.finish(&mut samples);
                self.stretched
                    .extend(samples.iter().flat_map(|sample| sample.to_le_bytes()));
            }
        }
        if !self.stretched.is_empty() && self.encoder.is_none() {
            self.start_encoder(env)?;
        }
        let Some(encoder) = &self.encoder else {
            return Ok(moved);
        };
        if !self.stretched.is_empty() {
            let room = dequeue_input(env, &encoder.codec, WAIT)?;
            if room < 0 {
                return Ok(moved);
            }
            let bytes = self.frame_bytes();
            let target = input_buffer(env, &encoder.codec, room)?;
            let target = env.cast_local::<JByteBuffer>(target)?;
            let capacity = env.get_direct_buffer_capacity(&target)?;
            let to = env.get_direct_buffer_address(&target)?;
            let size = self.stretched.len().min(capacity / bytes * bytes);
            // SAFETY: Android's own direct buffer, with room for `size`.
            unsafe {
                std::ptr::copy_nonoverlapping(self.stretched.as_ptr(), to, size);
            }
            let time = self.first.unwrap_or(0)
                + (self.sent_frames as f64 * 1e6 / f64::from(self.rate.max(1))) as i64;
            queue_input(env, &encoder.codec, room, size as i32, time, 0)?;
            self.sent_frames += (size / bytes) as u64;
            self.stretched.drain(..size);
            moved = true;
        } else if self.flushed && !self.sent_end {
            let room = dequeue_input(env, &encoder.codec, WAIT)?;
            if room >= 0 {
                queue_input(env, &encoder.codec, room, 0, 0, FLAG_END_OF_STREAM)?;
                self.sent_end = true;
                moved = true;
            }
        }
        Ok(moved)
    }

    /// Puts sound the decoder has through the speed change, leaving none waiting.
    fn stretch_sound(
        &mut self,
        env: &mut Env,
        index: i32,
        offset: usize,
        frames: usize,
    ) -> jni::errors::Result<()> {
        let bytes = self.frame_bytes();
        let source = output_buffer(env, &self.reader.decoder, index)?;
        let source = env.cast_local::<JByteBuffer>(source)?;
        let from = env.get_direct_buffer_address(&source)?;
        // SAFETY: Android's own direct buffer, holding what the decoder said it
        // does, until it's released, below.
        let data =
            unsafe { std::slice::from_raw_parts(from.add(offset).cast_const(), frames * bytes) };
        let samples: Vec<i16> = data
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| i16::from_le_bytes(*pair))
            .collect();
        let (speed, rate, channels) = (self.speed, self.rate, self.channels as usize);
        let tempo = self
            .tempo
            .get_or_insert_with(|| Tempo::new(speed, rate, channels));
        let mut out = Vec::new();
        tempo.push(&samples, &mut out);
        self.stretched
            .extend(out.iter().flat_map(|sample| sample.to_le_bytes()));
        release_output(env, &self.reader.decoder, index)
    }

    fn pass_sound(&mut self, env: &mut Env) -> jni::errors::Result<bool> {
        if self.changes_speed() {
            return self.pass_stretched(env);
        }
        let mut moved = false;
        if self.waiting.is_none() && !self.decoded_all {
            moved |= self.take_sound(env)?;
        }
        if self.waiting.is_some() && self.encoder.is_none() {
            self.start_encoder(env)?;
        }
        let Some(encoder) = &self.encoder else {
            return Ok(moved);
        };
        if let Some(sound) = self.waiting {
            let room = dequeue_input(env, &encoder.codec, WAIT)?;
            if room < 0 {
                return Ok(moved);
            }
            let bytes = self.frame_bytes();
            let target = input_buffer(env, &encoder.codec, room)?;
            let target = env.cast_local::<JByteBuffer>(target)?;
            let capacity = env.get_direct_buffer_capacity(&target)?;
            let to = env.get_direct_buffer_address(&target)?;
            let source = output_buffer(env, &self.reader.decoder, sound.index)?;
            let source = env.cast_local::<JByteBuffer>(source)?;
            let from = env.get_direct_buffer_address(&source)?;
            // Whole frames, as many as there's room for.
            let size = sound.remaining.min(capacity / bytes * bytes);
            // SAFETY: Android's own direct buffers, with `size` bytes in each:
            // what the decoder said it has, and what the encoder has room for.
            unsafe {
                std::ptr::copy_nonoverlapping(from.add(sound.offset).cast_const(), to, size);
            }
            let time = self.first.unwrap_or(0)
                + (self.sent_frames as f64 * 1e6 / f64::from(self.rate.max(1))) as i64;
            queue_input(env, &encoder.codec, room, size as i32, time, 0)?;
            self.sent_frames += (size / bytes) as u64;
            moved = true;
            if size == sound.remaining {
                release_output(env, &self.reader.decoder, sound.index)?;
                self.waiting = None;
            } else {
                self.waiting = Some(Sound {
                    offset: sound.offset + size,
                    remaining: sound.remaining - size,
                    ..sound
                });
            }
        } else if self.decoded_all && !self.sent_end {
            let room = dequeue_input(env, &encoder.codec, WAIT)?;
            if room >= 0 {
                queue_input(env, &encoder.codec, room, 0, 0, FLAG_END_OF_STREAM)?;
                self.sent_end = true;
                moved = true;
            }
        }
        Ok(moved)
    }

    /// Takes what the decoder has, keeping the part that's in the range.
    fn take_sound(&mut self, env: &mut Env) -> jni::errors::Result<bool> {
        let decoder = &self.reader.decoder;
        let index = dequeue_output(env, decoder, &self.reader.info, WAIT)?;
        if index == OUTPUT_FORMAT_CHANGED {
            self.read_format(env)?;
            return Ok(true);
        }
        if index < 0 {
            return Ok(false);
        }
        let (offset, size, time, flags) = read_info(env, &self.reader.info)?;
        if self.rate == 0 {
            self.read_format(env)?;
        }
        let bytes = self.frame_bytes();
        let frames = size.max(0) as usize / bytes;
        let window = android_logic::audio_window(time, frames, self.rate.max(1), self.range);
        let mut kept = false;
        match window {
            None => self.decoded_all = true,
            Some((skip, count)) if count > 0 => {
                if self.first.is_none() {
                    let skipped = skip as f64 * 1e6 / f64::from(self.rate.max(1));
                    let first = (time + skipped as i64 - self.range.0).max(0);
                    self.first = Some((first as f64 / self.speed) as i64);
                }
                let start = offset.max(0) as usize + skip * bytes;
                if self.changes_speed() {
                    self.stretch_sound(env, index, start, count)?;
                } else {
                    self.waiting = Some(Sound {
                        index,
                        offset: start,
                        remaining: count * bytes,
                    });
                }
                kept = true;
            }
            Some(_) => {}
        }
        if !kept {
            release_output(env, &self.reader.decoder, index)?;
        }
        if flags & FLAG_END_OF_STREAM != 0 {
            self.decoded_all = true;
        }
        Ok(true)
    }

    fn start_encoder(&mut self, env: &mut Env) -> jni::errors::Result<()> {
        let mime = env.new_string(self.mime)?;
        let format = env
            .call_static_method(
                jni_str!("android/media/MediaFormat"),
                jni_str!("createAudioFormat"),
                jni_sig!("(Ljava/lang/String;II)Landroid/media/MediaFormat;"),
                &[
                    JValue::Object(&mime),
                    JValue::Int(self.rate as i32),
                    JValue::Int(self.channels.max(1) as i32),
                ],
            )?
            .l()?;
        set_int(env, &format, "bitrate", self.kbps as i32 * 1000)?;
        let codec = create_codec(env, self.mime, true)?;
        configure(env, &codec, &format, CONFIGURE_ENCODE)?;
        start(env, &codec)?;
        self.encoder = Some(Encoder {
            codec,
            info: new_info(env)?,
        });
        Ok(())
    }
}

fn new_info(env: &mut Env) -> jni::errors::Result<Ref> {
    let info = env.new_object(
        jni_str!("android/media/MediaCodec$BufferInfo"),
        jni_sig!("()V"),
        &[],
    )?;
    env.new_global_ref(&info)
}

fn set_info(
    env: &mut Env,
    info: &JObject,
    offset: i32,
    size: i32,
    time: i64,
    flags: i32,
) -> jni::errors::Result<()> {
    env.call_method(
        info,
        jni_str!("set"),
        jni_sig!("(IIJI)V"),
        &[
            JValue::Int(offset),
            JValue::Int(size),
            JValue::Long(time),
            JValue::Int(flags),
        ],
    )?;
    Ok(())
}

/// A `BufferInfo`'s offset, size, time and flags.
fn read_info(env: &mut Env, info: &JObject) -> jni::errors::Result<(i32, i32, i64, i32)> {
    Ok((
        env.get_field(info, jni_str!("offset"), jni_sig!("I"))?
            .i()?,
        env.get_field(info, jni_str!("size"), jni_sig!("I"))?.i()?,
        env.get_field(info, jni_str!("presentationTimeUs"), jni_sig!("J"))?
            .j()?,
        env.get_field(info, jni_str!("flags"), jni_sig!("I"))?.i()?,
    ))
}

fn read_sample(env: &mut Env, extractor: &JObject, buffer: &JObject) -> jni::errors::Result<i32> {
    env.call_method(
        extractor,
        jni_str!("readSampleData"),
        jni_sig!("(Ljava/nio/ByteBuffer;I)I"),
        &[JValue::Object(buffer), JValue::Int(0)],
    )?
    .i()
}

fn write_sample(
    env: &mut Env,
    muxer: &JObject,
    output: i32,
    buffer: &JObject,
    info: &JObject,
) -> jni::errors::Result<()> {
    env.call_method(
        muxer,
        jni_str!("writeSampleData"),
        jni_sig!("(ILjava/nio/ByteBuffer;Landroid/media/MediaCodec$BufferInfo;)V"),
        &[
            JValue::Int(output),
            JValue::Object(buffer),
            JValue::Object(info),
        ],
    )?;
    Ok(())
}

fn set_int(env: &mut Env, format: &JObject, key: &str, value: i32) -> jni::errors::Result<()> {
    let key = env.new_string(key)?;
    env.call_method(
        format,
        jni_str!("setInteger"),
        jni_sig!("(Ljava/lang/String;I)V"),
        &[JValue::Object(&key), JValue::Int(value)],
    )?;
    Ok(())
}

fn create_codec(env: &mut Env, mime: &str, encoder: bool) -> jni::errors::Result<Ref> {
    let mime = env.new_string(mime)?;
    let codec = if encoder {
        env.call_static_method(
            jni_str!("android/media/MediaCodec"),
            jni_str!("createEncoderByType"),
            jni_sig!("(Ljava/lang/String;)Landroid/media/MediaCodec;"),
            &[JValue::Object(&mime)],
        )?
    } else {
        env.call_static_method(
            jni_str!("android/media/MediaCodec"),
            jni_str!("createDecoderByType"),
            jni_sig!("(Ljava/lang/String;)Landroid/media/MediaCodec;"),
            &[JValue::Object(&mime)],
        )?
    }
    .l()?;
    env.new_global_ref(&codec)
}

fn configure(
    env: &mut Env,
    codec: &JObject,
    format: &JObject,
    flags: i32,
) -> jni::errors::Result<()> {
    let null = JObject::null();
    env.call_method(
        codec,
        jni_str!("configure"),
        jni_sig!(
            "(Landroid/media/MediaFormat;Landroid/view/Surface;Landroid/media/MediaCrypto;I)V"
        ),
        &[
            JValue::Object(format),
            JValue::Object(&null),
            JValue::Object(&null),
            JValue::Int(flags),
        ],
    )?;
    Ok(())
}

fn start(env: &mut Env, codec: &JObject) -> jni::errors::Result<()> {
    env.call_method(codec, jni_str!("start"), jni_sig!("()V"), &[])?;
    Ok(())
}

fn dequeue_input(env: &mut Env, codec: &JObject, wait: i64) -> jni::errors::Result<i32> {
    env.call_method(
        codec,
        jni_str!("dequeueInputBuffer"),
        jni_sig!("(J)I"),
        &[JValue::Long(wait)],
    )?
    .i()
}

fn dequeue_output(
    env: &mut Env,
    codec: &JObject,
    info: &JObject,
    wait: i64,
) -> jni::errors::Result<i32> {
    env.call_method(
        codec,
        jni_str!("dequeueOutputBuffer"),
        jni_sig!("(Landroid/media/MediaCodec$BufferInfo;J)I"),
        &[JValue::Object(info), JValue::Long(wait)],
    )?
    .i()
}

fn input_buffer<'local>(
    env: &mut Env<'local>,
    codec: &JObject,
    index: i32,
) -> jni::errors::Result<JObject<'local>> {
    env.call_method(
        codec,
        jni_str!("getInputBuffer"),
        jni_sig!("(I)Ljava/nio/ByteBuffer;"),
        &[JValue::Int(index)],
    )?
    .l()
}

fn output_buffer<'local>(
    env: &mut Env<'local>,
    codec: &JObject,
    index: i32,
) -> jni::errors::Result<JObject<'local>> {
    env.call_method(
        codec,
        jni_str!("getOutputBuffer"),
        jni_sig!("(I)Ljava/nio/ByteBuffer;"),
        &[JValue::Int(index)],
    )?
    .l()
}

fn input_image<'local>(
    env: &mut Env<'local>,
    codec: &JObject,
    index: i32,
) -> jni::errors::Result<JObject<'local>> {
    env.call_method(
        codec,
        jni_str!("getInputImage"),
        jni_sig!("(I)Landroid/media/Image;"),
        &[JValue::Int(index)],
    )?
    .l()
}

fn output_image<'local>(
    env: &mut Env<'local>,
    codec: &JObject,
    index: i32,
) -> jni::errors::Result<JObject<'local>> {
    env.call_method(
        codec,
        jni_str!("getOutputImage"),
        jni_sig!("(I)Landroid/media/Image;"),
        &[JValue::Int(index)],
    )?
    .l()
}

fn close_image(env: &mut Env, image: &JObject) -> jni::errors::Result<()> {
    env.call_method(image, jni_str!("close"), jni_sig!("()V"), &[])?;
    Ok(())
}

fn output_format<'local>(
    env: &mut Env<'local>,
    codec: &JObject,
) -> jni::errors::Result<JObject<'local>> {
    env.call_method(
        codec,
        jni_str!("getOutputFormat"),
        jni_sig!("()Landroid/media/MediaFormat;"),
        &[],
    )?
    .l()
}

fn queue_input(
    env: &mut Env,
    codec: &JObject,
    index: i32,
    size: i32,
    time: i64,
    flags: i32,
) -> jni::errors::Result<()> {
    env.call_method(
        codec,
        jni_str!("queueInputBuffer"),
        jni_sig!("(IIIJI)V"),
        &[
            JValue::Int(index),
            JValue::Int(0),
            JValue::Int(size),
            JValue::Long(time),
            JValue::Int(flags),
        ],
    )?;
    Ok(())
}

fn release_output(env: &mut Env, codec: &JObject, index: i32) -> jni::errors::Result<()> {
    env.call_method(
        codec,
        jni_str!("releaseOutputBuffer"),
        jni_sig!("(IZ)V"),
        &[JValue::Int(index), JValue::Bool(false)],
    )?;
    Ok(())
}
