//! Reading and copying videos with Android's own classes. Copying only, so
//! far: nothing is decoded or encoded, so it's quick and loses nothing.

use std::path::Path;

use jni::{
    Env, JValue, jni_sig, jni_str,
    objects::{JObject, JString},
};

use super::{AudioChoice, AudioStream, ClipJob, Format, Source, Subtitle, Tools, android_logic};
use crate::{android::with_env, jobs::Progress};

/// `MediaMetadataRetriever.METADATA_KEY_LOCATION`.
const KEY_LOCATION: i32 = 23;
/// `MediaExtractor.SEEK_TO_PREVIOUS_SYNC`.
const SEEK_TO_PREVIOUS_SYNC: i32 = 0;
/// `MediaExtractor.SAMPLE_FLAG_*` and what `MediaCodec.BUFFER_FLAG_*` calls
/// the same things.
const SAMPLE_SYNC: i32 = 1;
const SAMPLE_PARTIAL: i32 = 4;
const BUFFER_KEY_FRAME: i32 = 1;
const BUFFER_PARTIAL: i32 = 8;
/// Room for one sample, unless a track says it needs more.
const SAMPLE_BUFFER: i32 = 8 << 20;

/// What can be done here: AAC audio copied into an M4A is all there's an
/// "encoder" for yet.
pub fn tools() -> Tools {
    Tools {
        encoders: ["aac".to_owned()].into_iter().collect(),
        zscale: false,
        hardware: Vec::new(),
        copies_only: true,
    }
}

fn new_extractor<'local>(
    env: &mut Env<'local>,
    path: &str,
) -> jni::errors::Result<JObject<'local>> {
    let extractor = env.new_object(
        jni_str!("android/media/MediaExtractor"),
        jni_sig!("()V"),
        &[],
    )?;
    let file = env.new_string(path)?;
    env.call_method(
        &extractor,
        jni_str!("setDataSource"),
        jni_sig!("(Ljava/lang/String;)V"),
        &[JValue::Object(&file)],
    )?;
    Ok(extractor)
}

/// Lets go of a Java object, whatever went wrong before: a pending exception
/// would stop any call being made, so it's put aside first and returned.
fn release<T>(
    env: &mut Env,
    object: &JObject,
    result: jni::errors::Result<T>,
) -> jni::errors::Result<T> {
    let caught = env.exception_catch();
    let _ = env.call_method(object, jni_str!("release"), jni_sig!("()V"), &[]);
    if env.exception_check() {
        env.exception_clear();
    }
    caught?;
    result
}

fn track_format<'local>(
    env: &mut Env<'local>,
    extractor: &JObject,
    index: i32,
) -> jni::errors::Result<JObject<'local>> {
    env.call_method(
        extractor,
        jni_str!("getTrackFormat"),
        jni_sig!("(I)Landroid/media/MediaFormat;"),
        &[JValue::Int(index)],
    )?
    .l()
}

fn text_of(env: &mut Env, format: &JObject, key: &str) -> jni::errors::Result<Option<String>> {
    let key = env.new_string(key)?;
    let value = env
        .call_method(
            format,
            jni_str!("getString"),
            jni_sig!("(Ljava/lang/String;)Ljava/lang/String;"),
            &[JValue::Object(&key)],
        )?
        .l()?;
    if value.is_null() {
        return Ok(None);
    }
    Ok(Some(env.cast_local::<JString>(value)?.try_to_string(env)?))
}

/// A number from a track's format, whichever kind of number it's kept as.
fn number_of(env: &mut Env, format: &JObject, key: &str) -> jni::errors::Result<Option<f64>> {
    let key = env.new_string(key)?;
    let value = env
        .call_method(
            format,
            jni_str!("getNumber"),
            jni_sig!("(Ljava/lang/String;)Ljava/lang/Number;"),
            &[JValue::Object(&key)],
        )?
        .l()?;
    if value.is_null() {
        return Ok(None);
    }
    let value = env
        .call_method(&value, jni_str!("doubleValue"), jni_sig!("()D"), &[])?
        .d()?;
    Ok(Some(value))
}

fn track_count(env: &mut Env, extractor: &JObject) -> jni::errors::Result<i32> {
    env.call_method(extractor, jni_str!("getTrackCount"), jni_sig!("()I"), &[])?
        .i()
}

/// Reads what `path` holds.
pub fn probe(path: &Path) -> Result<Source, String> {
    let size = disk::metadata(path).map(|m| m.len()).unwrap_or(0);
    let path = path.to_str().ok_or("the file's name isn't valid Unicode")?;
    let location = location_of(path).unwrap_or(None);
    let source = with_env("read a video", |env| {
        let extractor = new_extractor(env, path)?;
        let result = read_tracks(env, &extractor);
        release(env, &extractor, result)
    })?;
    let mut source = source;
    source.size = size;
    if let Some(location) = location {
        source.location = vec![("location".to_owned(), location)];
    }
    if source.video_codec.is_empty() {
        return Err("This file has no video.".to_owned());
    }
    if source.duration <= 0.0 {
        return Err("Can't tell how long this video is.".to_owned());
    }
    Ok(source)
}

fn read_tracks(env: &mut Env, extractor: &JObject) -> jni::errors::Result<Source> {
    let mut source = Source {
        fps: 30.0,
        ..Source::default()
    };
    for index in 0..track_count(env, extractor)? {
        let format = track_format(env, extractor, index)?;
        let mime = text_of(env, &format, "mime")?.unwrap_or_default();
        let length = number_of(env, &format, "durationUs")?.unwrap_or(0.0) / 1_000_000.0;
        source.duration = source.duration.max(length);
        if mime.starts_with("video/") && source.video_codec.is_empty() {
            let side = |value: Option<f64>| value.unwrap_or(0.0) as u32;
            let (width, height) = (
                side(number_of(env, &format, "width")?),
                side(number_of(env, &format, "height")?),
            );
            let rotation = side(number_of(env, &format, "rotation-degrees")?);
            let turned = rotation % 180 == 90;
            source.width = if turned { height } else { width };
            source.height = if turned { width } else { height };
            source.rotation = rotation;
            source.video_index = index as u64;
            source.video_codec = android_logic::codec_name(&mime);
            source.fps = number_of(env, &format, "frame-rate")?.unwrap_or(30.0);
            // `MediaFormat.COLOR_TRANSFER_ST2084` and `_HLG`.
            source.color_transfer = match number_of(env, &format, "color-transfer")? {
                Some(6.0) => Some("smpte2084".to_owned()),
                Some(7.0) => Some("arib-std-b67".to_owned()),
                _ => None,
            };
        } else if mime.starts_with("audio/") {
            let codec = android_logic::codec_name(&mime);
            let mut parts = vec![format!("Track {}", source.audio.len() + 1)];
            let language = text_of(env, &format, "language")?;
            parts.extend(
                language
                    .as_deref()
                    .and_then(android_logic::language_label)
                    .map(str::to_owned),
            );
            parts.push(codec.clone());
            if let Some(channels) = number_of(env, &format, "channel-count")? {
                parts.push(format!("{channels} ch"));
            }
            source.audio.push(AudioStream {
                index: index as u64,
                codec,
                label: parts.join(", "),
            });
        } else if mime.starts_with("text/") {
            source.subtitles.push(Subtitle {
                index: index as u64,
                codec: android_logic::codec_name(&mime),
            });
        }
    }
    Ok(source)
}

fn location_of(path: &str) -> Result<Option<String>, String> {
    with_env("read a video's location", |env| {
        let retriever = env.new_object(
            jni_str!("android/media/MediaMetadataRetriever"),
            jni_sig!("()V"),
            &[],
        )?;
        let file = env.new_string(path)?;
        let result = (|| -> jni::errors::Result<Option<String>> {
            env.call_method(
                &retriever,
                jni_str!("setDataSource"),
                jni_sig!("(Ljava/lang/String;)V"),
                &[JValue::Object(&file)],
            )?;
            let value = env
                .call_method(
                    &retriever,
                    jni_str!("extractMetadata"),
                    jni_sig!("(I)Ljava/lang/String;"),
                    &[JValue::Int(KEY_LOCATION)],
                )?
                .l()?;
            if value.is_null() {
                return Ok(None);
            }
            Ok(Some(env.cast_local::<JString>(value)?.try_to_string(env)?))
        })();
        release(env, &retriever, result)
    })
}

fn seek_to(env: &mut Env, extractor: &JObject, seconds: f64) -> jni::errors::Result<()> {
    env.call_method(
        extractor,
        jni_str!("seekTo"),
        jni_sig!("(JI)V"),
        &[
            JValue::Long((seconds.max(0.0) * 1_000_000.0) as i64),
            JValue::Int(SEEK_TO_PREVIOUS_SYNC),
        ],
    )?;
    Ok(())
}

fn sample_time(env: &mut Env, extractor: &JObject) -> jni::errors::Result<i64> {
    env.call_method(extractor, jni_str!("getSampleTime"), jni_sig!("()J"), &[])?
        .j()
}

/// Where a copy asked to start at `at` really starts: the keyframe before it.
pub fn keyframe_before(path: &Path, source: &Source, at: f64) -> Result<f64, String> {
    if at <= 0.0 {
        return Ok(0.0);
    }
    let path = path.to_str().ok_or("the file's name isn't valid Unicode")?;
    let track = source.video_index as i32;
    with_env("find a keyframe", |env| {
        let extractor = new_extractor(env, path)?;
        let result = (|| -> jni::errors::Result<f64> {
            env.call_method(
                &extractor,
                jni_str!("selectTrack"),
                jni_sig!("(I)V"),
                &[JValue::Int(track)],
            )?;
            seek_to(env, &extractor, at)?;
            let time = sample_time(env, &extractor)?;
            Ok(if time < 0 {
                0.0
            } else {
                time as f64 / 1_000_000.0
            })
        })();
        release(env, &extractor, result)
    })
}

/// Which tracks go in.
fn chosen_tracks(job: &ClipJob) -> Vec<i32> {
    let audio_only = job.settings.format.is_audio_only();
    let mut tracks = Vec::new();
    if !audio_only {
        tracks.push(job.source.video_index as i32);
    }
    tracks.extend(
        job.settings
            .audio_streams(&job.source)
            .into_iter()
            .map(|stream| stream.index as i32),
    );
    tracks
}

/// Copies what `job` asks for to `output`. Stops early, leaving a file that the
/// caller removes, if the job's cancelled.
pub fn copy(job: &ClipJob, extension: &str, output: &Path, p: &Progress) -> Result<(), String> {
    if job.settings.format == Format::M4a
        && job
            .settings
            .audio_streams(&job.source)
            .first()
            .is_none_or(|stream| stream.codec != "aac")
    {
        return Err("Only AAC audio can go into an M4A here, as nothing's encoded yet.".to_owned());
    }
    if job.settings.audio == AudioChoice::None && job.settings.format.is_audio_only() {
        return Err("There's no audio chosen to save.".to_owned());
    }
    let input = job
        .input
        .to_str()
        .ok_or("the file's name isn't valid Unicode")?;
    let output = output
        .to_str()
        .ok_or("the file's name isn't valid Unicode")?;
    let tracks = chosen_tracks(job);
    let location = job
        .source
        .location
        .first()
        .filter(|_| job.settings.keep.location)
        .and_then(|(_, text)| android_logic::parse_location(text));
    let format = android_logic::muxer_format(extension);
    with_env("export a video", |env| {
        let extractor = new_extractor(env, input)?;
        let result = (|| -> jni::errors::Result<()> {
            let file = env.new_string(output)?;
            let muxer = env.new_object(
                jni_str!("android/media/MediaMuxer"),
                jni_sig!("(Ljava/lang/String;I)V"),
                &[JValue::Object(&file), JValue::Int(format)],
            )?;
            let result = write_samples(env, &extractor, &muxer, job, &tracks, location, p);
            release(env, &muxer, result)
        })();
        release(env, &extractor, result)
    })
}

fn write_samples(
    env: &mut Env,
    extractor: &JObject,
    muxer: &JObject,
    job: &ClipJob,
    tracks: &[i32],
    location: Option<(f32, f32)>,
    p: &Progress,
) -> jni::errors::Result<()> {
    if !job.settings.format.is_audio_only() {
        env.call_method(
            muxer,
            jni_str!("setOrientationHint"),
            jni_sig!("(I)V"),
            &[JValue::Int(job.source.rotation as i32)],
        )?;
    }
    if let Some((latitude, longitude)) = location {
        env.call_method(
            muxer,
            jni_str!("setLocation"),
            jni_sig!("(FF)V"),
            &[JValue::Float(latitude), JValue::Float(longitude)],
        )?;
    }
    // Which of the muxer's tracks each of the file's becomes.
    let mut outputs = Vec::new();
    let mut room = SAMPLE_BUFFER;
    for &track in tracks {
        env.call_method(
            extractor,
            jni_str!("selectTrack"),
            jni_sig!("(I)V"),
            &[JValue::Int(track)],
        )?;
        let format = track_format(env, extractor, track)?;
        if let Some(size) = number_of(env, &format, "max-input-size")? {
            room = room.max(size as i32);
        }
        let output = env
            .call_method(
                muxer,
                jni_str!("addTrack"),
                jni_sig!("(Landroid/media/MediaFormat;)I"),
                &[JValue::Object(&format)],
            )?
            .i()?;
        outputs.push((track, output));
    }
    let buffer = env
        .call_static_method(
            jni_str!("java/nio/ByteBuffer"),
            jni_str!("allocateDirect"),
            jni_sig!("(I)Ljava/nio/ByteBuffer;"),
            &[JValue::Int(room)],
        )?
        .l()?;
    let info = env.new_object(
        jni_str!("android/media/MediaCodec$BufferInfo"),
        jni_sig!("()V"),
        &[],
    )?;
    env.call_method(muxer, jni_str!("start"), jni_sig!("()V"), &[])?;
    let start = (job.start * 1_000_000.0) as i64;
    let end = (job.end * 1_000_000.0) as i64;
    seek_to(env, extractor, job.start)?;
    let mut written = 0u64;
    loop {
        if p.cancelled() {
            // What's there is thrown away by the caller.
            return Ok(());
        }
        let time = sample_time(env, extractor)?;
        let size = env
            .call_method(
                extractor,
                jni_str!("readSampleData"),
                jni_sig!("(Ljava/nio/ByteBuffer;I)I"),
                &[JValue::Object(&buffer), JValue::Int(0)],
            )?
            .i()?;
        if size < 0 || time < 0 {
            break;
        }
        // Tracks are stored close together, so this is past all of them, with
        // room for pictures stored out of order.
        if time > end + 2_000_000 {
            break;
        }
        // Before the start, audio that came with the keyframe; after the
        // end, what was read past it.
        if time >= start && time <= end {
            let track = env
                .call_method(
                    extractor,
                    jni_str!("getSampleTrackIndex"),
                    jni_sig!("()I"),
                    &[],
                )?
                .i()?;
            let flags = env
                .call_method(extractor, jni_str!("getSampleFlags"), jni_sig!("()I"), &[])?
                .i()?;
            if let Some((_, output)) = outputs.iter().find(|(from, _)| *from == track) {
                let mut buffer_flags = 0;
                if flags & SAMPLE_SYNC != 0 {
                    buffer_flags |= BUFFER_KEY_FRAME;
                }
                if flags & SAMPLE_PARTIAL != 0 {
                    buffer_flags |= BUFFER_PARTIAL;
                }
                env.call_method(
                    &info,
                    jni_str!("set"),
                    jni_sig!("(IIJI)V"),
                    &[
                        JValue::Int(0),
                        JValue::Int(size),
                        JValue::Long(time - start),
                        JValue::Int(buffer_flags),
                    ],
                )?;
                env.call_method(
                    muxer,
                    jni_str!("writeSampleData"),
                    jni_sig!("(ILjava/nio/ByteBuffer;Landroid/media/MediaCodec$BufferInfo;)V"),
                    &[
                        JValue::Int(*output),
                        JValue::Object(&buffer),
                        JValue::Object(&info),
                    ],
                )?;
                written += 1;
                if written.is_multiple_of(32) {
                    p.set_time_done((time - start) as f64 / 1_000_000.0);
                }
            }
        }
        let more = env
            .call_method(extractor, jni_str!("advance"), jni_sig!("()Z"), &[])?
            .z()?;
        if !more {
            break;
        }
    }
    env.call_method(muxer, jni_str!("stop"), jni_sig!("()V"), &[])?;
    Ok(())
}
