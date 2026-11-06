//! Stills along the editor's timeline, grabbed in the background: the whole
//! video thinly first, then more of what's zoomed into.

use std::{
    collections::{BTreeMap, HashSet},
    path::PathBuf,
    sync::{Arc, Mutex},
};

use egui::{Color32, ColorImage, Painter, Rect, TextureHandle, TextureOptions, pos2};

use super::ffmpeg::{self, Source, Still};

/// How many stills are grabbed at once.
const WORKERS: usize = if cfg!(target_os = "android") { 1 } else { 2 };
/// The narrowest a still is drawn, in points.
const MIN_WIDTH: f32 = 12.0;
/// The most stills kept; the furthest from what's in view go past this.
const MOST: usize = 600;

/// What the workers and the strip pass each other.
#[derive(Default)]
struct Shared {
    /// Times wanted, by the millisecond, the most wanted first.
    wanted: Vec<u64>,
    /// Being grabbed now.
    taken: HashSet<u64>,
    /// Grabbed, for the strip to pick up.
    done: Vec<(u64, Option<Still>)>,
    working: usize,
}

enum Frame {
    Shown(TextureHandle),
    Failed,
}

pub struct Strip {
    path: PathBuf,
    source: Source,
    /// Stills' height in pixels.
    height: u32,
    shared: Arc<Mutex<Shared>>,
    frames: BTreeMap<u64, Frame>,
    /// For the stills' texture names.
    id: u64,
}

impl Strip {
    pub fn new(id: u64, path: PathBuf, source: Source) -> Self {
        Self {
            path,
            source,
            height: 0,
            shared: Arc::default(),
            frames: BTreeMap::new(),
            id,
        }
    }

    /// Draws stills across `bar`, which shows `view` of the video, asking
    /// for those it hasn't got. Nothing until the first comes.
    pub fn show(&mut self, ctx: &egui::Context, painter: &Painter, bar: Rect, view: (f64, f64)) {
        let height = (bar.height() * ctx.pixels_per_point()).round().max(2.0) as u32;
        if height != self.height {
            // Another scale: the stills would blur.
            self.height = height;
            self.frames.clear();
            self.shared.lock().unwrap().wanted.clear();
        }
        self.collect(ctx);

        let duration = self.source.duration.max(f64::EPSILON);
        let span = (view.1 - view.0).max(f64::EPSILON);
        let width = (bar.height() * self.source.aspect()).max(MIN_WIDTH);
        // Seconds a still covers. Slots keep to multiples of it, so the
        // stills move with the video when it's panned.
        let slot = span * f64::from(width / bar.width().max(1.0));
        let step = grid_step(duration, slot, self.source.fps());
        let x_of = |time: f64| bar.left() + bar.width() * ((time - view.0) / span) as f32;
        let painter = painter.with_clip_rect(painter.clip_rect().intersect(bar));
        let first = (view.0 / slot).floor() as i64;
        let last = (view.1 / slot).floor() as i64;
        let mut wanted = Vec::new();
        for index in first..=last.min(first + 1000) {
            let time = (index as f64 + 0.5) * slot;
            let left = x_of(index as f64 * slot);
            let rect = Rect::from_x_y_ranges(left..=left + width, bar.y_range());
            if let Some(texture) = self.nearest(time) {
                let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
                painter.image(texture.id(), rect, uv, Color32::WHITE);
            }
            let key = to_key(((time / step).round() * step).min(duration));
            wanted.push((index - first, key));
        }
        // Spread out: the whole strip thinly first, then the gaps.
        wanted.sort_by_key(|(index, _)| (*index as u32).reverse_bits());
        let mut shared = self.shared.lock().unwrap();
        let mut seen = HashSet::new();
        let fresh: Vec<u64> = wanted
            .into_iter()
            .map(|(_, key)| key)
            .filter(|key| seen.insert(*key))
            .filter(|key| !self.frames.contains_key(key) && !shared.taken.contains(key))
            .collect();
        shared.wanted = fresh;
        while !shared.wanted.is_empty() && shared.working < WORKERS {
            shared.working += 1;
            self.start_worker(ctx);
        }
        drop(shared);
        self.forget_far(view);
    }

    /// Takes in what the workers have grabbed.
    fn collect(&mut self, ctx: &egui::Context) {
        let done = std::mem::take(&mut self.shared.lock().unwrap().done);
        for (key, still) in done {
            let frame = match still {
                Some(Still { size, rgba }) => {
                    let image = ColorImage::from_rgba_unmultiplied(size, &rgba);
                    let name = format!("clip-still-{}-{key}", self.id);
                    Frame::Shown(ctx.load_texture(name, image, TextureOptions::LINEAR))
                }
                None => Frame::Failed,
            };
            self.frames.insert(key, frame);
        }
    }

    /// The still closest to `time`, of those there are.
    fn nearest(&self, time: f64) -> Option<&TextureHandle> {
        let key = to_key(time);
        fn shown<'a>((at, frame): (&u64, &'a Frame)) -> Option<(u64, &'a TextureHandle)> {
            match frame {
                Frame::Shown(texture) => Some((*at, texture)),
                Frame::Failed => None,
            }
        }
        let before = self.frames.range(..=key).rev().find_map(shown);
        let after = self.frames.range(key..).find_map(shown);
        match (before, after) {
            (Some(b), Some(a)) => Some(if key - b.0 <= a.0 - key { b.1 } else { a.1 }),
            (one, other) => one.or(other).map(|(_, texture)| texture),
        }
    }

    /// Drops the stills furthest from `view`, past the most kept.
    fn forget_far(&mut self, view: (f64, f64)) {
        if self.frames.len() <= MOST {
            return;
        }
        let middle = to_key((view.0 + view.1) / 2.0);
        let mut keys: Vec<u64> = self.frames.keys().copied().collect();
        keys.sort_by_key(|key| std::cmp::Reverse(key.abs_diff(middle)));
        for key in keys.into_iter().take(self.frames.len() - MOST) {
            self.frames.remove(&key);
        }
    }

    fn start_worker(&self, ctx: &egui::Context) {
        let (shared, ctx) = (self.shared.clone(), ctx.clone());
        let (path, source, height) = (self.path.clone(), self.source.clone(), self.height);
        // A moment that has a frame: the last is a frame's length before the end.
        let last = (source.duration - 1.5 / source.fps().max(1.0)).max(0.0);
        crate::background::spawn("clip-stills", move || {
            let next = || {
                let mut shared = shared.lock().unwrap();
                if shared.wanted.is_empty() {
                    shared.working -= 1;
                    return None;
                }
                let key = shared.wanted.remove(0);
                shared.taken.insert(key);
                Some((key, from_key(key).min(last)))
            };
            let done = |key: u64, still: Result<Still, String>| {
                let mut shared = shared.lock().unwrap();
                shared.taken.remove(&key);
                shared.done.push((key, still.ok()));
                ctx.request_repaint();
            };
            ffmpeg::stills(&path, &source, height, next, done);
        });
    }
}

impl Drop for Strip {
    /// Lets the workers go once they're done with what they've taken.
    fn drop(&mut self) {
        self.shared.lock().unwrap().wanted.clear();
    }
}

fn to_key(seconds: f64) -> u64 {
    (seconds.max(0.0) * 1000.0).round() as u64
}

fn from_key(key: u64) -> f64 {
    key as f64 / 1000.0
}

/// How far apart, in seconds, the times stills are asked for: halves of the
/// whole, no further apart than a slot, so panning finds the same ones again,
/// and no closer than a frame.
fn grid_step(duration: f64, slot: f64, fps: f64) -> f64 {
    let frame = 1.0 / fps.max(1.0);
    let mut step = duration.max(frame);
    while step > slot && step / 2.0 >= frame {
        step /= 2.0;
    }
    step.max(0.001)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stills_are_asked_for_on_a_steady_grid() {
        // A minute in slots of 2 s: halves of 60 s, down to 1.875 s.
        assert_eq!(grid_step(60.0, 2.0, 30.0), 1.875);
        // Never closer than a frame.
        assert_eq!(grid_step(1.0, 0.001, 10.0), 0.125);
    }
}
