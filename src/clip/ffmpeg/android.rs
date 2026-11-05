//! Reading videos with Android's own classes. [`super::android_encode`] makes
//! new ones with them.

use std::path::Path;

use jni::{
    Env, JValue, jni_sig, jni_str,
    objects::{JObject, JObjectArray, JString},
};

use super::{AudioStream, Source, Subtitle, Tools, android_logic};
use crate::android::with_env;

/// `MediaMetadataRetriever.METADATA_KEY_LOCATION`.
const KEY_LOCATION: i32 = 23;
/// `MediaExtractor.SEEK_TO_PREVIOUS_SYNC`.
const SEEK_TO_PREVIOUS_SYNC: i32 = 0;
/// `MediaCodecList.REGULAR_CODECS`: those that run in real time.
const REGULAR_CODECS: i32 = 0;

/// What can be done here: which types this phone has an encoder for.
pub fn tools() -> Result<Tools, String> {
    let mut encoders = encoder_types()?;
    // What ffmpeg calls the one audio format so far.
    if encoders.contains("audio/mp4a-latm") {
        encoders.insert("aac".to_owned());
    }
    Ok(Tools {
        encoders,
        zscale: false,
        hardware: Vec::new(),
        phone: true,
    })
}

fn encoder_types() -> Result<std::collections::HashSet<String>, String> {
    with_env("list the phone's encoders", |env| {
        let list = env.new_object(
            jni_str!("android/media/MediaCodecList"),
            jni_sig!("(I)V"),
            &[JValue::Int(REGULAR_CODECS)],
        )?;
        let infos = env
            .call_method(
                &list,
                jni_str!("getCodecInfos"),
                jni_sig!("()[Landroid/media/MediaCodecInfo;"),
                &[],
            )?
            .l()?;
        let infos = env.cast_local::<JObjectArray>(infos)?;
        let mut types = std::collections::HashSet::new();
        for index in 0..infos.len(env)? {
            env.with_local_frame(8, |env| -> jni::errors::Result<()> {
                let info = infos.get_element(env, index)?;
                let encoder = env
                    .call_method(&info, jni_str!("isEncoder"), jni_sig!("()Z"), &[])?
                    .z()?;
                if !encoder {
                    return Ok(());
                }
                let names = env
                    .call_method(
                        &info,
                        jni_str!("getSupportedTypes"),
                        jni_sig!("()[Ljava/lang/String;"),
                        &[],
                    )?
                    .l()?;
                let names = env.cast_local::<JObjectArray>(names)?;
                for at in 0..names.len(env)? {
                    let name = names.get_element(env, at)?;
                    let name = env.cast_local::<JString>(name)?.try_to_string(env)?;
                    types.insert(name.to_ascii_lowercase());
                }
                Ok(())
            })?;
        }
        Ok(types)
    })
}

pub(super) fn new_extractor<'local>(
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
pub(super) fn release<T>(
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

pub(super) fn track_format<'local>(
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

pub(super) fn text_of(
    env: &mut Env,
    format: &JObject,
    key: &str,
) -> jni::errors::Result<Option<String>> {
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
pub(super) fn number_of(
    env: &mut Env,
    format: &JObject,
    key: &str,
) -> jni::errors::Result<Option<f64>> {
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

pub(super) fn seek_to(env: &mut Env, extractor: &JObject, seconds: f64) -> jni::errors::Result<()> {
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

pub(super) fn sample_time(env: &mut Env, extractor: &JObject) -> jni::errors::Result<i64> {
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
