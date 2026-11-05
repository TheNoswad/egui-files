//! The parts of exporting on Android that don't need Android.

/// `MediaMuxer.OutputFormat`.
pub const MUXER_MPEG_4: i32 = 0;
pub const MUXER_WEBM: i32 = 1;

/// The extension a copy gets: `MediaMuxer` only makes MP4 and WebM of what
/// video usually comes in.
pub fn output_extension(original: &str) -> String {
    if original.eq_ignore_ascii_case("webm") {
        "webm".to_owned()
    } else {
        "mp4".to_owned()
    }
}

pub fn muxer_format(extension: &str) -> i32 {
    if extension == "webm" {
        MUXER_WEBM
    } else {
        MUXER_MPEG_4
    }
}

/// What ffprobe would call a codec, from Android's name for it.
pub fn codec_name(mime: &str) -> String {
    match mime {
        "video/avc" => "h264",
        "video/hevc" => "hevc",
        "video/av01" => "av1",
        "video/x-vnd.on2.vp9" => "vp9",
        "video/x-vnd.on2.vp8" => "vp8",
        "audio/mp4a-latm" => "aac",
        "audio/opus" => "opus",
        "audio/vorbis" => "vorbis",
        "audio/mpeg" => "mp3",
        "audio/flac" => "flac",
        other => other.rsplit('/').next().unwrap_or(other),
    }
    .to_owned()
}

/// The language to show for a track, if `tag` names one. A phone's recordings
/// leave it blank, which comes out as three backticks.
pub fn language_label(tag: &str) -> Option<&str> {
    (matches!(tag.len(), 2 | 3) && tag != "und" && tag.bytes().all(|b| b.is_ascii_alphabetic()))
        .then_some(tag)
}

/// A location like `+37.7749-122.4194/`, as latitude and longitude.
pub fn parse_location(text: &str) -> Option<(f32, f32)> {
    let text = text.trim().trim_end_matches('/');
    // The longitude starts at the second sign.
    let split = 1 + text.get(1..)?.find(['+', '-'])?;
    let (latitude, rest) = text.split_at(split);
    // There may be an altitude after it, at a third sign.
    let end = rest
        .get(1..)
        .and_then(|after| after.find(['+', '-']))
        .map_or(rest.len(), |at| at + 1);
    let (latitude, longitude): (f32, f32) = (latitude.parse().ok()?, rest[..end].parse().ok()?);
    ((-90.0..=90.0).contains(&latitude) && (-180.0..=180.0).contains(&longitude))
        .then_some((latitude, longitude))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_real_languages_are_shown() {
        assert_eq!(language_label("eng"), Some("eng"));
        assert_eq!(language_label("en"), Some("en"));
        assert_eq!(language_label("und"), None);
        assert_eq!(language_label("```"), None);
        assert_eq!(language_label(""), None);
    }

    #[test]
    fn copies_go_into_what_the_muxer_makes() {
        assert_eq!(output_extension("WEBM"), "webm");
        for original in ["mp4", "mov", "mkv", "3gp", "m4v"] {
            assert_eq!(output_extension(original), "mp4");
        }
        assert_eq!(muxer_format("webm"), MUXER_WEBM);
        assert_eq!(muxer_format("mp4"), MUXER_MPEG_4);
        assert_eq!(muxer_format("m4a"), MUXER_MPEG_4);
    }

    #[test]
    fn names_codecs_as_the_rest_of_the_app_does() {
        assert_eq!(codec_name("video/avc"), "h264");
        assert_eq!(codec_name("audio/mp4a-latm"), "aac");
        assert_eq!(codec_name("video/x-odd"), "x-odd");
    }

    #[test]
    fn reads_locations() {
        assert_eq!(
            parse_location("+37.7749-122.4194/"),
            Some((37.7749, -122.4194))
        );
        assert_eq!(parse_location("-33.8+151.2+058.0/"), Some((-33.8, 151.2)));
        assert_eq!(parse_location("nonsense"), None);
        assert_eq!(parse_location("+95.0+10.0/"), None, "no such latitude");
        assert_eq!(parse_location(""), None);
    }
}
