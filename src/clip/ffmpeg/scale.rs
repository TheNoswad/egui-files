//! Shrinks one plane of a video frame, on the processor, between the layouts
//! Android's codecs give: rows with padding, and samples with gaps between them.

/// A plane to read: the sample at (x, y) is at
/// `offset + y * row_stride + x * pixel_stride`.
#[derive(Clone, Copy)]
pub struct Source<'a> {
    pub data: &'a [u8],
    pub offset: usize,
    pub row_stride: usize,
    pub pixel_stride: usize,
    pub width: usize,
    pub height: usize,
}

impl Source<'_> {
    /// Copies row `y` into `row`, which is one sample each. Some codecs leave
    /// the padding off the last row, and what isn't there stays as it was.
    fn read_row(&self, y: usize, row: &mut [u8]) {
        let start = self.offset + y * self.row_stride;
        let Some(data) = self.data.get(start..) else {
            return;
        };
        if self.pixel_stride == 1 {
            let length = row.len().min(data.len());
            row[..length].copy_from_slice(&data[..length]);
        } else {
            for (sample, from) in row.iter_mut().zip(data.iter().step_by(self.pixel_stride)) {
                *sample = *from;
            }
        }
    }
}

/// A plane to write.
pub struct Target<'a> {
    pub data: &'a mut [u8],
    pub row_stride: usize,
    pub pixel_stride: usize,
    pub width: usize,
    pub height: usize,
}

impl Target<'_> {
    fn write_row(&mut self, y: usize, row: &[u8]) {
        let Some(data) = self.data.get_mut(y * self.row_stride..) else {
            return;
        };
        if self.pixel_stride == 1 {
            let length = row.len().min(data.len());
            data[..length].copy_from_slice(&row[..length]);
        } else {
            for (to, sample) in data.iter_mut().step_by(self.pixel_stride).zip(row) {
                *to = *sample;
            }
        }
    }
}

/// How much of which samples make up each sample of a line. Weights add up to
/// [`ONE`].
#[derive(Default)]
struct Line {
    /// For each output sample, the first input sample and how many it takes.
    spans: Vec<(u32, u32)>,
    weights: Vec<u32>,
}

const ONE: u32 = 1 << 14;

impl Line {
    /// A triangle as wide as the step, or as wide as two samples when growing:
    /// nearly what ffmpeg's bilinear scaler does, but shrinking far looks at
    /// everything it passes, rather than flickering between what it lands on.
    fn new(from: usize, to: usize) -> Self {
        let mut line = Self::default();
        if from == 0 || to == 0 {
            return line;
        }
        let step = from as f64 / to as f64;
        let reach = step.max(1.0);
        for index in 0..to {
            let center = (index as f64 + 0.5) * step - 0.5;
            let first = ((center - reach).ceil().max(0.0)) as usize;
            let last = ((center + reach).floor() as usize).min(from - 1);
            let raw: Vec<f64> = (first..=last)
                .map(|at| (1.0 - (at as f64 - center).abs() / reach).max(0.0))
                .collect();
            let total: f64 = raw.iter().sum();
            let mut weights: Vec<u32> = raw
                .iter()
                .map(|weight| (weight / total * f64::from(ONE)) as u32)
                .collect();
            // What rounding lost goes to the biggest, so flat stays flat.
            let lost = ONE - weights.iter().sum::<u32>();
            if let Some(biggest) = weights.iter_mut().max() {
                *biggest += lost;
            }
            line.spans.push((first as u32, weights.len() as u32));
            line.weights.extend(weights);
        }
        line
    }
}

/// Scales planes of one size to another, keeping what it worked out and its
/// working room between frames.
#[derive(Default)]
pub struct Scaler {
    size: (usize, usize, usize, usize),
    across: Line,
    down: Line,
    /// Each input row, already the output's width.
    narrowed: Vec<u8>,
    row: Vec<u8>,
    out: Vec<u8>,
    /// The picture before it's turned.
    unturned: Vec<u8>,
}

impl Scaler {
    /// Scales `source` to fit `target` once turned clockwise by `turn` degrees.
    pub fn scale_turned(&mut self, source: Source, target: &mut Target, turn: u32) {
        if turn == 0 {
            self.scale(source, target);
            return;
        }
        let sideways = turn % 180 == 90;
        let (width, height) = if sideways {
            (target.height, target.width)
        } else {
            (target.width, target.height)
        };
        let mut unturned = std::mem::take(&mut self.unturned);
        unturned.resize(width * height, 0);
        self.scale(
            source,
            &mut Target {
                data: &mut unturned,
                row_stride: width,
                pixel_stride: 1,
                width,
                height,
            },
        );
        let mut row = std::mem::take(&mut self.row);
        row.clear();
        row.resize(target.width, 0);
        for y in 0..target.height {
            for (x, sample) in row.iter_mut().enumerate() {
                let (across, down) = match turn {
                    90 => (y, height - 1 - x),
                    180 => (width - 1 - x, height - 1 - y),
                    _ => (width - 1 - y, x),
                };
                *sample = unturned[down * width + across];
            }
            target.write_row(y, &row);
        }
        self.row = row;
        self.unturned = unturned;
    }

    pub fn scale(&mut self, source: Source, target: &mut Target) {
        let (from_x, from_y) = (source.width, source.height);
        let (to_x, to_y) = (target.width, target.height);
        if from_x == 0 || from_y == 0 || to_x == 0 || to_y == 0 {
            return;
        }
        if from_x == to_x && from_y == to_y {
            self.row.clear();
            self.row.resize(to_x, 128);
            for y in 0..to_y {
                source.read_row(y, &mut self.row);
                target.write_row(y, &self.row);
            }
            return;
        }
        let size = (from_x, from_y, to_x, to_y);
        if self.size != size {
            self.size = size;
            self.across = Line::new(from_x, to_x);
            self.down = Line::new(from_y, to_y);
        }
        self.row.clear();
        self.row.resize(from_x, 128);
        self.narrowed.resize(to_x * from_y, 0);
        for (y, out) in self.narrowed.chunks_exact_mut(to_x).enumerate() {
            source.read_row(y, &mut self.row);
            let mut weights = self.across.weights.as_slice();
            for (sample, &(first, count)) in out.iter_mut().zip(&self.across.spans) {
                let (these, rest) = weights.split_at(count as usize);
                weights = rest;
                let taken = &self.row[first as usize..first as usize + count as usize];
                let sum: u32 = these
                    .iter()
                    .zip(taken)
                    .map(|(weight, value)| weight * u32::from(*value))
                    .sum();
                *sample = ((sum + ONE / 2) >> 14) as u8;
            }
        }
        let mut sums = vec![ONE / 2; to_x];
        self.out.resize(to_x, 0);
        let mut weights = self.down.weights.as_slice();
        for (y, &(first, count)) in self.down.spans.iter().enumerate() {
            let (these, rest) = weights.split_at(count as usize);
            weights = rest;
            sums.fill(ONE / 2);
            for (offset, weight) in these.iter().enumerate() {
                let row = (first as usize + offset) * to_x;
                for (sum, value) in sums.iter_mut().zip(&self.narrowed[row..row + to_x]) {
                    *sum += weight * u32::from(*value);
                }
            }
            for (out, sum) in self.out.iter_mut().zip(&sums) {
                *out = (sum >> 14) as u8;
            }
            target.write_row(y, &self.out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tight(data: &[u8], width: usize, height: usize) -> Source<'_> {
        Source {
            data,
            offset: 0,
            row_stride: width,
            pixel_stride: 1,
            width,
            height,
        }
    }

    fn scaled(source: Source, width: usize, height: usize) -> Vec<u8> {
        let mut out = vec![0; width * height];
        Scaler::default().scale(
            source,
            &mut Target {
                data: &mut out,
                row_stride: width,
                pixel_stride: 1,
                width,
                height,
            },
        );
        out
    }

    #[test]
    fn the_same_size_is_a_copy() {
        let data: Vec<u8> = (0..12).collect();
        assert_eq!(scaled(tight(&data, 4, 3), 4, 3), data);
    }

    #[test]
    fn flat_stays_flat() {
        let data = vec![77; 1920 * 1080];
        assert!(
            scaled(tight(&data, 1920, 1080), 1280, 720)
                .iter()
                .all(|v| *v == 77)
        );
        assert!(
            scaled(tight(&data, 1920, 1080), 111, 53)
                .iter()
                .all(|v| *v == 77)
        );
    }

    #[test]
    fn shrinking_keeps_a_gradient_in_order_and_its_brightness() {
        let data: Vec<u8> = (0..200 * 10).map(|i| (i % 200) as u8).collect();
        let out = scaled(tight(&data, 200, 10), 50, 5);
        let row = &out[..50];
        assert!(row.windows(2).all(|pair| pair[0] <= pair[1]));
        let mean = |values: &[u8]| {
            values.iter().map(|v| u64::from(*v)).sum::<u64>() as f64 / values.len() as f64
        };
        assert!((mean(&out) - mean(&data)).abs() < 1.5);
    }

    #[test]
    fn shrinking_far_averages_rather_than_picks() {
        // One line in four is white: picking would give all white or all black.
        let data: Vec<u8> = (0..400)
            .flat_map(|y| {
                let value = if y % 4 == 0 { 255 } else { 0 };
                vec![value; 4]
            })
            .collect();
        let out = scaled(tight(&data, 4, 400), 4, 25);
        for value in out {
            assert!((40..=90).contains(&value), "{value}");
        }
    }

    #[test]
    fn reads_rows_with_padding_and_interleaved_samples() {
        // U and V side by side, with two bytes of padding on each row.
        #[rustfmt::skip]
        let data = [
            1, 101, 2, 102, 0, 0,
            3, 103, 4, 104, 0, 0,
        ];
        let plane = |offset| Source {
            data: &data,
            offset,
            row_stride: 6,
            pixel_stride: 2,
            width: 2,
            height: 2,
        };
        assert_eq!(scaled(plane(0), 2, 2), [1, 2, 3, 4]);
        assert_eq!(scaled(plane(1), 2, 2), [101, 102, 103, 104]);
    }

    #[test]
    fn writes_interleaved_samples_without_touching_the_others() {
        let data = [10, 20, 30, 40];
        let mut out = [9; 12];
        Scaler::default().scale(
            tight(&data, 2, 2),
            &mut Target {
                data: &mut out,
                row_stride: 6,
                pixel_stride: 2,
                width: 2,
                height: 2,
            },
        );
        assert_eq!(out, [10, 9, 20, 9, 9, 9, 30, 9, 40, 9, 9, 9]);
    }

    #[test]
    fn a_last_row_without_its_padding_does_not_panic() {
        // Stride 6, width 4, and the last row ends right after its samples.
        let data = [1, 2, 3, 4, 0, 0, 5, 6, 7, 8];
        let source = Source {
            data: &data,
            offset: 0,
            row_stride: 6,
            pixel_stride: 1,
            width: 4,
            height: 2,
        };
        assert_eq!(scaled(source, 4, 2), [1, 2, 3, 4, 5, 6, 7, 8]);
        // And even shorter than that, in either direction.
        let short = Source {
            data: &data[..8],
            ..source
        };
        assert_eq!(scaled(short, 4, 2)[..4], [1, 2, 3, 4]);
        let mut out = [0; 3];
        Scaler::default().scale(
            tight(&data[..4], 2, 2),
            &mut Target {
                data: &mut out,
                row_stride: 2,
                pixel_stride: 1,
                width: 2,
                height: 2,
            },
        );
        assert_eq!(out, [1, 2, 3]);
    }

    #[test]
    fn turns_the_picture_clockwise() {
        // 1 2 3
        // 4 5 6
        let data = [1, 2, 3, 4, 5, 6];
        let turned = |turn, width: usize, height: usize| {
            let mut out = vec![0; width * height];
            Scaler::default().scale_turned(
                tight(&data, 3, 2),
                &mut Target {
                    data: &mut out,
                    row_stride: width,
                    pixel_stride: 1,
                    width,
                    height,
                },
                turn,
            );
            out
        };
        assert_eq!(turned(0, 3, 2), [1, 2, 3, 4, 5, 6]);
        assert_eq!(turned(90, 2, 3), [4, 1, 5, 2, 6, 3]);
        assert_eq!(turned(180, 3, 2), [6, 5, 4, 3, 2, 1]);
        assert_eq!(turned(270, 2, 3), [3, 6, 2, 5, 1, 4]);
    }

    #[test]
    fn growing_works_too() {
        let data = [0, 200];
        let out = scaled(tight(&data, 2, 1), 4, 1);
        assert!(out.windows(2).all(|pair| pair[0] <= pair[1]));
        assert_eq!(out.len(), 4);
    }
}
