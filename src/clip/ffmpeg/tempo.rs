//! Changes how fast sound plays without changing its pitch, as Android has
//! nothing to do it with when exporting.
//!
//! This is WSOLA: pieces of the sound overlap to make the new length. Each is
//! taken from about where it should be, moved a little to wherever it lines up
//! best with what's already there, so the waves add up rather than cancel out.

/// How long each piece is, in seconds: long enough for a low voice to line up.
const PIECE_SECONDS: f64 = 0.04;
/// How far a piece may move to line up, in seconds.
const REACH_SECONDS: f64 = 0.012;
/// Looking at only some of the samples is plenty to find the best place.
const COMPARE_EVERY: usize = 4;
const SEARCH_EVERY: usize = 2;

/// Sound going in, as 16-bit samples of each channel in turn, and coming out at
/// another speed.
pub struct Tempo {
    speed: f64,
    channels: usize,
    /// In frames: one sample of each channel.
    piece: usize,
    hop: usize,
    reach: usize,
    window: Vec<f32>,
    /// Sound not yet used up, without what's been dropped from the front.
    input: Vec<i16>,
    /// Frames dropped from the front of `input` so far.
    dropped: usize,
    frames_in: usize,
    frames_out: usize,
    /// How many pieces have been made.
    pieces: usize,
    /// Where what was last used goes on to, in frames from the very start.
    natural: Option<usize>,
    /// The new sound still being added to.
    overlap: Vec<f32>,
}

impl Tempo {
    pub fn new(speed: f64, rate: u32, channels: usize) -> Self {
        let channels = channels.max(1);
        let hop = ((f64::from(rate) * PIECE_SECONDS / 2.0) as usize).max(16);
        let piece = hop * 2;
        // A window whose overlapping halves add up to one.
        let window = (0..piece)
            .map(|at| {
                let phase = std::f64::consts::PI * at as f64 / piece as f64;
                (phase.sin() * phase.sin()) as f32
            })
            .collect();
        Self {
            speed,
            channels,
            piece,
            hop,
            reach: ((f64::from(rate) * REACH_SECONDS) as usize).max(SEARCH_EVERY),
            window,
            input: Vec::new(),
            dropped: 0,
            frames_in: 0,
            frames_out: 0,
            pieces: 0,
            natural: None,
            overlap: vec![0.0; piece * channels],
        }
    }

    /// Takes in `samples`, and adds to `out` what can be made so far.
    pub fn push(&mut self, samples: &[i16], out: &mut Vec<i16>) {
        self.input.extend_from_slice(samples);
        self.frames_in += samples.len() / self.channels;
        while self.make_piece(false, out) {}
        self.drop_used();
    }

    /// Adds to `out` the rest of what's made of everything taken in.
    pub fn finish(&mut self, out: &mut Vec<i16>) {
        let wanted = (self.frames_in as f64 / self.speed).round() as usize;
        while self.frames_out < wanted && self.make_piece(true, out) {}
        // It ends where the sound does, not where the last piece does.
        let extra = self.frames_out.saturating_sub(wanted);
        out.truncate(out.len() - (extra * self.channels).min(out.len()));
        self.frames_out -= extra;
    }

    /// Where the next piece should come from, in frames from the very start.
    fn ideal(&self) -> usize {
        (self.pieces as f64 * self.hop as f64 * self.speed) as usize
    }

    /// Makes one more piece, if there's the sound for it, or with `ending`, if
    /// there's any left.
    fn make_piece(&mut self, ending: bool, out: &mut Vec<i16>) -> bool {
        let ideal = self.ideal();
        let available = self.dropped + self.input.len() / self.channels;
        if !ending && available < ideal + self.reach + self.piece {
            return false;
        }
        if ending && ideal >= available && self.pieces > 0 && self.overlap_is_all_out() {
            return false;
        }
        let from = self.best_place(ideal, available);
        let first = self.pieces == 0;
        for at in 0..self.piece {
            // The very start isn't faded in, as nothing's fading out under it.
            let weight = if first && at < self.hop {
                1.0
            } else {
                self.window[at]
            };
            for channel in 0..self.channels {
                let sample = self.sample(from + at, channel);
                self.overlap[at * self.channels + channel] += f32::from(sample) * weight;
            }
        }
        let done = self.hop * self.channels;
        out.extend(
            self.overlap[..done]
                .iter()
                .map(|value| value.round().clamp(-32768.0, 32767.0) as i16),
        );
        self.overlap.copy_within(done.., 0);
        let length = self.overlap.len();
        self.overlap[length - done..].fill(0.0);
        self.frames_out += self.hop;
        self.pieces += 1;
        self.natural = Some(from + self.hop);
        true
    }

    /// A safety for `finish`, which otherwise always wants more.
    fn overlap_is_all_out(&self) -> bool {
        self.overlap.iter().all(|value| *value == 0.0)
    }

    fn sample(&self, frame: usize, channel: usize) -> i16 {
        frame
            .checked_sub(self.dropped)
            .and_then(|frame| self.input.get(frame * self.channels + channel))
            .copied()
            .unwrap_or(0)
    }

    /// All channels of a frame, together.
    fn mixed(&self, frame: usize) -> f32 {
        (0..self.channels)
            .map(|channel| f32::from(self.sample(frame, channel)))
            .sum()
    }

    /// Near `ideal`, where a piece best carries on from what's there.
    fn best_place(&self, ideal: usize, available: usize) -> usize {
        let Some(natural) = self.natural else {
            return ideal;
        };
        let lowest = ideal.saturating_sub(self.reach).max(self.dropped);
        let highest = (ideal + self.reach).min(available.saturating_sub(self.piece));
        if highest <= lowest {
            return ideal;
        }
        let wanted: Vec<f32> = (0..self.piece)
            .step_by(COMPARE_EVERY)
            .map(|at| self.mixed(natural + at))
            .collect();
        let mut best = (ideal.clamp(lowest, highest), f32::MIN);
        for place in (lowest..=highest).step_by(SEARCH_EVERY) {
            let (mut together, mut energy) = (0.0, 1.0);
            for (index, want) in wanted.iter().enumerate() {
                let here = self.mixed(place + index * COMPARE_EVERY);
                together += here * want;
                energy += here * here;
            }
            let score = together / energy.sqrt();
            if score > best.1 {
                best = (place, score);
            }
        }
        best.0
    }

    /// Forgets the sound that nothing can need again.
    fn drop_used(&mut self) {
        let mut needed = self.ideal().saturating_sub(self.reach);
        if let Some(natural) = self.natural {
            needed = needed.min(natural);
        }
        let frames = needed.saturating_sub(self.dropped);
        let frames = frames.min(self.input.len() / self.channels);
        // Not for every little bit: it moves all that's left.
        if frames >= self.piece {
            self.input.drain(..frames * self.channels);
            self.dropped += frames;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;

    fn sine(frequency: f64, seconds: f64, channels: usize) -> Vec<i16> {
        (0..(seconds * f64::from(RATE)) as usize)
            .flat_map(|at| {
                let time = at as f64 / f64::from(RATE);
                let value = (time * frequency * std::f64::consts::TAU).sin() * 10_000.0;
                std::iter::repeat_n(value as i16, channels)
            })
            .collect()
    }

    fn stretched(samples: &[i16], speed: f64, channels: usize, chunk: usize) -> Vec<i16> {
        let mut tempo = Tempo::new(speed, RATE, channels);
        let mut out = Vec::new();
        for part in samples.chunks(chunk) {
            tempo.push(part, &mut out);
        }
        tempo.finish(&mut out);
        out
    }

    /// How often it goes through zero upwards, each second.
    fn pitch(samples: &[i16]) -> f64 {
        let ups = samples
            .windows(2)
            .filter(|pair| pair[0] < 0 && pair[1] >= 0)
            .count();
        ups as f64 / (samples.len() as f64 / f64::from(RATE))
    }

    fn loudness(samples: &[i16]) -> f64 {
        let sum: f64 = samples.iter().map(|v| f64::from(*v) * f64::from(*v)).sum();
        (sum / samples.len() as f64).sqrt()
    }

    #[test]
    fn the_length_changes_and_the_pitch_and_loudness_do_not() {
        let sound = sine(440.0, 1.0, 1);
        for speed in [0.25, 0.5, 0.75, 1.25, 1.5, 2.0, 4.0] {
            let out = stretched(&sound, speed, 1, 4096);
            let wanted = sound.len() as f64 / speed;
            assert!(
                (out.len() as f64 - wanted).abs() <= 1.0,
                "{speed}: {} samples, wanted {wanted}",
                out.len()
            );
            let heard = pitch(&out);
            assert!((heard - 440.0).abs() < 8.0, "{speed}: pitch {heard}");
            let level = loudness(&out) / loudness(&sound);
            assert!((0.9..1.1).contains(&level), "{speed}: loudness {level}");
        }
    }

    #[test]
    fn low_notes_survive_too() {
        let out = stretched(&sine(110.0, 1.0, 1), 2.0, 1, 4096);
        assert!((pitch(&out) - 110.0).abs() < 4.0, "{}", pitch(&out));
    }

    #[test]
    fn the_channels_stay_apart() {
        // Left a tone, right silent.
        let sound: Vec<i16> = sine(440.0, 0.5, 1)
            .into_iter()
            .flat_map(|sample| [sample, 0])
            .collect();
        let out = stretched(&sound, 1.5, 2, 4096);
        assert!(out.iter().skip(1).step_by(2).all(|sample| *sample == 0));
        let left: Vec<i16> = out.iter().step_by(2).copied().collect();
        assert!((pitch(&left) - 440.0).abs() < 8.0);
        assert_eq!(out.len() % 2, 0);
    }

    #[test]
    fn it_does_not_matter_how_the_sound_is_handed_over() {
        let sound = sine(300.0, 0.5, 2);
        let whole = stretched(&sound, 2.0, 2, sound.len());
        let bits = stretched(&sound, 2.0, 2, 1234);
        assert_eq!(whole.len(), bits.len());
        assert!((pitch(&whole) - pitch(&bits)).abs() < 8.0);
    }

    #[test]
    fn silence_and_tiny_sounds_are_fine() {
        assert!(stretched(&[], 2.0, 2, 16).is_empty());
        assert_eq!(stretched(&[0; 24_000], 2.0, 1, 4096).len(), 12_000);
        let out = stretched(&[1000; 10], 0.5, 1, 16);
        assert_eq!(out.len(), 20);
    }

    #[test]
    fn it_does_not_keep_all_the_sound_in_memory() {
        let mut tempo = Tempo::new(2.0, RATE, 2);
        let mut out = Vec::new();
        let sound = sine(440.0, 0.25, 2);
        for _ in 0..30 {
            tempo.push(&sound, &mut out);
            out.clear();
        }
        // Far less than the 7.5 seconds it's been given.
        assert!(tempo.input.len() < sound.len(), "{}", tempo.input.len());
    }
}
