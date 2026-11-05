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

/// What's needed of a file's audio that goes in a container: whether `codec`
/// can be copied into it as it is.
pub fn audio_copies(extension: &str, codec: &str) -> bool {
    match extension {
        "webm" => matches!(codec, "opus" | "vorbis"),
        _ => codec == "aac",
    }
}

/// The type of audio encoder a container takes, and its usual kbit/s.
pub fn audio_encoder(extension: &str) -> (&'static str, u32) {
    if extension == "webm" {
        ("audio/opus", 128)
    } else {
        ("audio/mp4a-latm", 192)
    }
}

/// The picture's width and height, shrunk so its shorter side is `side`, both
/// even, as encoders need.
pub fn output_size((width, height): (u32, u32), side: Option<u32>) -> (u32, u32) {
    let even = |value: f64| (((value / 2.0).round() as u32) * 2).max(2);
    let short = f64::from(width.min(height).max(1));
    let scale = side.map_or(1.0, |side| (f64::from(side) / short).min(1.0));
    (
        even(f64::from(width) * scale),
        even(f64::from(height) * scale),
    )
}

/// A rectangle (x, y, width, height) of the upright picture, as a rectangle of
/// the frame stored `rotation` degrees short of upright, whose size is `stored`.
pub fn stored_rect(
    (x, y, width, height): (u32, u32, u32, u32),
    rotation: u32,
    (stored_width, stored_height): (u32, u32),
) -> (u32, u32, u32, u32) {
    match rotation {
        90 => (y, stored_height.saturating_sub(x + width), height, width),
        180 => (
            stored_width.saturating_sub(x + width),
            stored_height.saturating_sub(y + height),
            width,
            height,
        ),
        270 => (stored_width.saturating_sub(y + height), x, height, width),
        _ => (x, y, width, height),
    }
}

/// The video bitrate for a quality given in bits for each pixel of each frame.
/// H.264 needs the most; never more than the original had, which adds nothing.
pub fn video_bitrate(bpp: f64, is_h264: bool, pixels_per_second: f64, source_bps: f64) -> f64 {
    let wanted = bpp * pixels_per_second * if is_h264 { 1.0 } else { 0.65 };
    let limit = if source_bps > 0.0 {
        source_bps
    } else {
        f64::MAX
    };
    wanted.min(limit).max(100_000.0)
}

/// Picks which frames stay at a lower frame rate: those that have reached the
/// next time due, which then moves on past them.
pub struct FrameRate {
    next: i64,
    step: i64,
}

impl FrameRate {
    pub fn new(start: i64, fps: Option<u32>) -> Self {
        Self {
            next: start,
            step: fps.map_or(0, |fps| 1_000_000 / i64::from(fps.max(1))),
        }
    }

    pub fn keeps(&mut self, time: i64) -> bool {
        if self.step == 0 {
            return true;
        }
        if time < self.next {
            return false;
        }
        while self.next <= time {
            self.next += self.step;
        }
        true
    }
}

/// Which frames of decoded audio to keep, of `frames` starting at `time`
/// microseconds, for a range: how many to skip, and how many after that. `None`
/// once past the range's end, which ends the track.
pub fn audio_window(
    time: i64,
    frames: usize,
    rate: u32,
    (start, end): (i64, i64),
) -> Option<(usize, usize)> {
    if time >= end {
        return None;
    }
    let frames_in = |micros: i64| (micros.max(0) as f64 * f64::from(rate) / 1e6) as usize;
    let skip = frames_in(start - time).min(frames);
    let until = frames_in(end - time).min(frames);
    Some((skip, until.saturating_sub(skip)))
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
    fn audio_goes_where_it_fits() {
        assert!(audio_copies("mp4", "aac"));
        assert!(audio_copies("m4a", "aac"));
        assert!(!audio_copies("mp4", "opus"));
        assert!(audio_copies("webm", "opus"));
        assert!(!audio_copies("webm", "aac"));
        assert_eq!(audio_encoder("webm").0, "audio/opus");
        assert_eq!(audio_encoder("mp4").0, "audio/mp4a-latm");
    }

    #[test]
    fn sizes_come_out_even_and_never_grow() {
        assert_eq!(output_size((1920, 1080), None), (1920, 1080));
        assert_eq!(output_size((1920, 1080), Some(720)), (1280, 720));
        // A phone's upright video is stored on its side: the short side's the same.
        assert_eq!(output_size((1080, 1920), Some(720)), (720, 1280));
        assert_eq!(output_size((1920, 1080), Some(2160)), (1920, 1080));
        assert_eq!(output_size((853, 481), None), (854, 482));
        assert_eq!(output_size((1920, 1080), Some(1)), (2, 2));
    }

    #[test]
    fn a_rectangle_of_the_picture_is_found_in_the_stored_frame() {
        // A frame 8 by 4, with a 3 by 1 rectangle of it at (1, 0):
        //   . a a a . . . .
        //   . . . . . . . .   (and two rows more)
        let rect = (1, 0, 3, 1);
        let stored = (8, 4);
        assert_eq!(stored_rect(rect, 0, stored), rect);
        // Stored upside down, it's at the far corner.
        assert_eq!(stored_rect(rect, 180, stored), (4, 3, 3, 1));
        // Shown turned a quarter clockwise the frame is 4 wide and 8 high, and
        // what's along the top of that is down the left of the frame.
        assert_eq!(stored_rect((0, 1, 1, 3), 90, stored), (1, 3, 3, 1));
        // And a quarter the other way, along the top is down the right... of a
        // frame turned back, so its right end, reading up.
        assert_eq!(stored_rect((0, 1, 1, 3), 270, stored), (4, 0, 3, 1));
    }

    #[test]
    fn bitrates_follow_the_picture_up_to_the_original() {
        let hd = 1920.0 * 1080.0 * 30.0;
        assert_eq!(video_bitrate(0.08, true, hd, 0.0).round(), 4_976_640.0);
        assert!(video_bitrate(0.08, false, hd, 0.0) < video_bitrate(0.08, true, hd, 0.0));
        assert_eq!(video_bitrate(0.08, true, hd, 1_000_000.0), 1_000_000.0);
        assert_eq!(video_bitrate(0.08, true, 100.0, 0.0), 100_000.0);
    }

    #[test]
    fn a_lower_frame_rate_keeps_the_frames_it_is_due() {
        // 60 fps to 30: every other frame.
        let mut rate = FrameRate::new(1000, Some(30));
        let kept = (0..60)
            .map(|frame| 1000 + frame * 16_683)
            .filter(|time| rate.keeps(*time))
            .count();
        assert_eq!(kept, 30);
        let mut all = FrameRate::new(0, None);
        assert!(all.keeps(0) && all.keeps(1) && all.keeps(1));
        // Starts from the start, wherever frames fall.
        let mut late = FrameRate::new(0, Some(10));
        assert!(late.keeps(250_000));
        assert!(!late.keeps(290_000));
        assert!(late.keeps(300_000));
    }

    #[test]
    fn audio_is_cut_to_the_range() {
        let range = (1_000_000, 2_000_000);
        // Wholly before it: nothing kept, but not the end.
        assert_eq!(audio_window(0, 1000, 1000, range), Some((1000, 0)));
        // Across the start.
        assert_eq!(audio_window(500_000, 1000, 1000, range), Some((500, 500)));
        // Inside it.
        assert_eq!(audio_window(1_200_000, 100, 1000, range), Some((0, 100)));
        // Across the end.
        assert_eq!(audio_window(1_900_000, 300, 1000, range), Some((0, 100)));
        // Past it.
        assert_eq!(audio_window(2_000_000, 100, 1000, range), None);
    }

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
