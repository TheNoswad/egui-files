//! Turns the frames Android's decoders give, in YUV, into what a texture wants.

/// One plane of a frame, as Android lays it out.
pub struct Plane<'a> {
    pub data: &'a [u8],
    /// Bytes from one row to the next.
    pub row_stride: usize,
    /// Bytes from one sample to the next in a row.
    pub pixel_stride: usize,
}

impl Plane<'_> {
    fn at(&self, x: usize, y: usize) -> i32 {
        let index = y * self.row_stride + x * self.pixel_stride;
        // Some phones give a last row without its padding.
        i32::from(self.data.get(index).copied().unwrap_or(128))
    }
}

/// Which pixel of the frame each pixel of the picture shows, once turned
/// clockwise by `rotation` degrees.
fn source_of(
    (x, y): (usize, usize),
    (width, height): (usize, usize),
    (frame_width, frame_height): (usize, usize),
    rotation: u32,
) -> (usize, usize) {
    let (across, down) = match rotation {
        90 => (
            y * frame_width / height,
            (width - 1 - x) * frame_height / width,
        ),
        180 => (
            (width - 1 - x) * frame_width / width,
            (height - 1 - y) * frame_height / height,
        ),
        270 => (
            (height - 1 - y) * frame_width / height,
            x * frame_height / width,
        ),
        _ => (x * frame_width / width, y * frame_height / height),
    };
    (
        across.min(frame_width.saturating_sub(1)),
        down.min(frame_height.saturating_sub(1)),
    )
}

/// Draws a frame `size` pixels large into `out`, as RGBA, picking the nearest
/// pixels: it's done for each frame, on the processor. `planes` are Y, U and V
/// with U and V at half size, and `frame` Y's width and height.
pub fn to_rgba(
    [y_plane, u_plane, v_plane]: &[Plane; 3],
    frame: (usize, usize),
    rotation: u32,
    size: [usize; 2],
    out: &mut Vec<u8>,
) {
    let [width, height] = size;
    out.resize(width * height * 4, 255);
    if width == 0 || height == 0 || frame.0 == 0 || frame.1 == 0 {
        return;
    }
    // Which colors the numbers mean: BT.709 for HD, BT.601 for less, each in
    // the 16 to 235 range video uses. Scaled by 256.
    let hd = frame.0.max(frame.1) >= 1280;
    let (red_v, green_u, green_v, blue_u) = if hd {
        (459, 55, 136, 541)
    } else {
        (409, 100, 208, 516)
    };
    for (index, pixel) in out.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let shown = (index % width, index / width);
        let (x, y) = source_of(shown, (width, height), frame, rotation);
        let luma = 298 * (y_plane.at(x, y) - 16);
        let u = u_plane.at(x / 2, y / 2) - 128;
        let v = v_plane.at(x / 2, y / 2) - 128;
        let clamp = |value: i32| ((value + 128) >> 8).clamp(0, 255) as u8;
        pixel[0] = clamp(luma + red_v * v);
        pixel[1] = clamp(luma - green_u * u - green_v * v);
        pixel[2] = clamp(luma + blue_u * u);
        pixel[3] = 255;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 4 by 2 frame: its top left pixel white, all else black, with room for
    /// `pad` bytes at the end of each row. U and V are side by side, as phones
    /// lay them out.
    fn convert(rotation: u32, size: [usize; 2], pad: usize) -> Vec<u8> {
        let stride = 4 + pad;
        let mut luma = vec![16; stride * 2];
        luma[0] = 235;
        let chroma = vec![128; (4 + pad) * 2];
        let planes = [
            Plane {
                data: &luma,
                row_stride: stride,
                pixel_stride: 1,
            },
            Plane {
                data: &chroma,
                row_stride: stride,
                pixel_stride: 2,
            },
            Plane {
                data: &chroma[1..],
                row_stride: stride,
                pixel_stride: 2,
            },
        ];
        let mut out = Vec::new();
        to_rgba(&planes, (4, 2), rotation, size, &mut out);
        out
    }

    fn is_white(out: &[u8], width: usize, x: usize, y: usize) -> bool {
        let at = (y * width + x) * 4;
        out[at] > 250 && out[at + 1] > 250 && out[at + 2] > 250 && out[at + 3] == 255
    }

    #[test]
    fn colors_come_out_right() {
        let out = convert(0, [4, 2], 0);
        assert!(is_white(&out, 4, 0, 0));
        assert_eq!(&out[4..8], &[0, 0, 0, 255], "black, and opaque");
        // Padding at the ends of rows doesn't shift anything.
        assert_eq!(convert(0, [4, 2], 12), out);

        // Pure red in BT.601.
        let (luma, u, v) = ([81u8], [90u8], [240u8]);
        let plane = |data| Plane {
            data,
            row_stride: 1,
            pixel_stride: 1,
        };
        let mut out = Vec::new();
        to_rgba(
            &[plane(&luma), plane(&u), plane(&v)],
            (1, 1),
            0,
            [1, 1],
            &mut out,
        );
        assert!(out[0] > 250 && out[1] < 6 && out[2] < 6, "{out:?}");
    }

    #[test]
    fn frames_turn_clockwise() {
        // Shown upright, the frame is 2 wide and 4 tall.
        let out = convert(90, [2, 4], 0);
        assert!(is_white(&out, 2, 1, 0), "top left goes to the top right");
        let out = convert(180, [4, 2], 0);
        assert!(is_white(&out, 4, 3, 1), "to the opposite corner");
        let out = convert(270, [2, 4], 0);
        assert!(is_white(&out, 2, 0, 3), "to the bottom left");
    }

    #[test]
    fn scales_to_the_size_asked() {
        let out = convert(0, [8, 4], 0);
        assert_eq!(out.len(), 8 * 4 * 4);
        assert!(is_white(&out, 8, 1, 1) && !is_white(&out, 8, 2, 2));
        assert!(convert(0, [0, 0], 0).is_empty());
    }

    #[test]
    fn a_short_last_row_does_no_harm() {
        let luma = [16u8; 3];
        let plane = |data| Plane {
            data,
            row_stride: 4,
            pixel_stride: 1,
        };
        let mut out = Vec::new();
        to_rgba(
            &[plane(&luma), plane(&luma), plane(&luma)],
            (4, 2),
            0,
            [4, 2],
            &mut out,
        );
        assert_eq!(out.len(), 4 * 2 * 4);
    }
}
