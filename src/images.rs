//! Background image decoding with an LRU cache of GPU textures. Thumbnails
//! come from the shared system cache when possible, which is also how videos
//! get theirs (see [`crate::thumbnails`]).
//!
//! Views call [`ImageCache::get`] every frame for the images they want to
//! draw. Missing images are queued for a pool of worker threads, newest
//! request first, so whatever is on screen right now is decoded first.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    time::SystemTime,
};

use egui::{ColorImage, Context, TextureHandle, TextureOptions};
use image::ImageDecoder;

use crate::thumbnails;

/// Longest side of thumbnails used by the grid and columns views.
pub const THUMBNAIL_SIZE: u32 = 256;
/// Longest side of images shown in the preview pane.
pub const PREVIEW_SIZE: u32 = 1600;

/// GPU memory kept alive for textures at most: ~400 square thumbnails
/// (256 KiB each), or fewer when large previews are cached too.
const BUDGET_BYTES: usize = 100 << 20;
/// Queued jobs beyond this are dropped, oldest first (e.g. after fast scrolling).
const MAX_QUEUE: usize = 256;

const EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "bmp", "ico", "tif", "tiff",
];

/// Largest size looked up in the system thumbnail cache; bigger requests
/// (previews) decode the original.
const MAX_CACHED_SIDE: u32 = 1024;

/// Whether a picture can be made of the file: an image, or a video's thumbnail.
pub fn is_supported(path: &Path) -> bool {
    is_image(path) || crate::video::is_video(path)
}

pub fn is_image(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Key {
    path: PathBuf,
    modified: Option<SystemTime>,
    max_side: u32,
}

pub struct Image {
    pub texture: TextureHandle,
    /// Size of the original image, before downscaling.
    pub original_size: [u32; 2],
}

impl Image {
    fn byte_size(&self) -> usize {
        let [w, h] = self.texture.size();
        w * h * 4
    }
}

enum Slot {
    Pending,
    Ready { image: Image, last_used: u64 },
    Failed(String),
}

pub enum Lookup<'a> {
    Ready(&'a Image),
    Pending,
    Failed(&'a str),
    Unsupported,
}

struct Done {
    key: Key,
    result: Result<(ColorImage, [u32; 2]), String>,
}

#[derive(Default)]
struct Queue {
    jobs: Mutex<Vec<Key>>,
    ready: Condvar,
    shutdown: AtomicBool,
}

pub struct ImageCache {
    ctx: Context,
    queue: Arc<Queue>,
    results: Receiver<Done>,
    slots: HashMap<Key, Slot>,
    frame: u64,
    /// No worker thread could start (the browser can't): `begin_frame`
    /// decodes instead.
    inline: bool,
}

impl ImageCache {
    pub fn new(ctx: &Context) -> Self {
        let queue = Arc::new(Queue::default());
        let (sender, results) = mpsc::channel();
        let workers = std::thread::available_parallelism()
            .map_or(2, |n| n.get().saturating_sub(1))
            .clamp(1, 4);
        let started = (0..workers)
            .filter(|i| {
                let (queue, sender, ctx) = (queue.clone(), sender.clone(), ctx.clone());
                std::thread::Builder::new()
                    .name(format!("image-decoder-{i}"))
                    .spawn(move || worker(&queue, &sender, &ctx))
                    .is_ok()
            })
            .count();
        Self {
            ctx: ctx.clone(),
            queue,
            results,
            slots: HashMap::new(),
            frame: 0,
            inline: started == 0,
        }
    }

    /// Uploads finished decodes and evicts old textures. Call once per frame.
    pub fn begin_frame(&mut self) {
        self.frame += 1;
        let mut done: Vec<Done> = self.results.try_iter().collect();
        if self.inline {
            done.extend(self.decode_inline());
        }
        for Done { key, result } in done {
            let slot = match result {
                Ok((color_image, original_size)) => {
                    let name = key.path.display().to_string();
                    let texture = self
                        .ctx
                        .load_texture(name, color_image, TextureOptions::LINEAR);
                    Slot::Ready {
                        image: Image {
                            texture,
                            original_size,
                        },
                        last_used: self.frame,
                    }
                }
                Err(err) => Slot::Failed(err),
            };
            self.slots.insert(key, slot);
        }
        self.evict();
    }

    /// Without worker threads, decodes the newest request, one a frame so
    /// the UI keeps responding.
    fn decode_inline(&self) -> Option<Done> {
        let key = self
            .queue
            .jobs
            .lock()
            .expect("image queue poisoned")
            .pop()?;
        let result = decode(&key.path, key.modified, key.max_side);
        self.ctx.request_repaint();
        Some(Done { key, result })
    }

    fn evict(&mut self) {
        let used: usize = self
            .slots
            .values()
            .map(|slot| match slot {
                Slot::Ready { image, .. } => image.byte_size(),
                _ => 0,
            })
            .sum();
        if used <= BUDGET_BYTES {
            return;
        }
        let mut by_age: Vec<(u64, usize, Key)> = self
            .slots
            .iter()
            .filter_map(|(key, slot)| match slot {
                Slot::Ready { image, last_used } => {
                    Some((*last_used, image.byte_size(), key.clone()))
                }
                _ => None,
            })
            .collect();
        by_age.sort_unstable_by_key(|(last_used, ..)| *last_used);
        // Evict down to 80% so this doesn't run every frame.
        let mut excess = used - BUDGET_BYTES * 4 / 5;
        for (last_used, size, key) in by_age {
            if excess == 0 || last_used == self.frame {
                break;
            }
            self.slots.remove(&key);
            excess = excess.saturating_sub(size);
        }
    }

    /// Returns the image if decoded, queueing it otherwise.
    pub fn get(&mut self, path: &Path, modified: Option<SystemTime>, max_side: u32) -> Lookup<'_> {
        if !is_supported(path) {
            return Lookup::Unsupported;
        }
        let key = Key {
            path: path.to_path_buf(),
            modified,
            max_side,
        };
        if !self.slots.contains_key(&key) {
            self.enqueue(key.clone());
            self.slots.insert(key.clone(), Slot::Pending);
        }
        match self.slots.get_mut(&key).expect("inserted above") {
            Slot::Pending => Lookup::Pending,
            Slot::Failed(err) => Lookup::Failed(err),
            Slot::Ready { image, last_used } => {
                *last_used = self.frame;
                Lookup::Ready(image)
            }
        }
    }

    /// Like [`Self::get`], but only returns ready images.
    pub fn get_ready(
        &mut self,
        path: &Path,
        modified: Option<SystemTime>,
        max_side: u32,
    ) -> Option<&Image> {
        match self.get(path, modified, max_side) {
            Lookup::Ready(image) => Some(image),
            _ => None,
        }
    }

    /// Whether the image is decoded, without queueing it.
    pub fn is_ready(&self, path: &Path, modified: Option<SystemTime>, max_side: u32) -> bool {
        let key = Key {
            path: path.to_path_buf(),
            modified,
            max_side,
        };
        matches!(self.slots.get(&key), Some(Slot::Ready { .. }))
    }

    fn enqueue(&mut self, key: Key) {
        let mut jobs = self.queue.jobs.lock().expect("image queue poisoned");
        jobs.push(key);
        if jobs.len() > MAX_QUEUE {
            let excess = jobs.len() - MAX_QUEUE;
            let dropped: Vec<Key> = jobs.drain(..excess).collect();
            for key in dropped {
                // Forget it so it is requested again if it comes back into view.
                self.slots.remove(&key);
            }
        }
        self.queue.ready.notify_one();
    }
}

impl Drop for ImageCache {
    fn drop(&mut self) {
        self.queue.shutdown.store(true, Ordering::Relaxed);
        self.queue.ready.notify_all();
    }
}

fn worker(queue: &Queue, sender: &Sender<Done>, ctx: &Context) {
    loop {
        let key = {
            let mut jobs = queue.jobs.lock().expect("image queue poisoned");
            loop {
                if queue.shutdown.load(Ordering::Relaxed) {
                    return;
                }
                // Newest first: the most recent requests are what is on screen.
                if let Some(key) = jobs.pop() {
                    break key;
                }
                jobs = queue.ready.wait(jobs).expect("image queue poisoned");
            }
        };
        let result = decode(&key.path, key.modified, key.max_side);
        if sender.send(Done { key, result }).is_err() {
            return;
        }
        ctx.request_repaint();
    }
}

fn decode(
    path: &Path,
    modified: Option<SystemTime>,
    max_side: u32,
) -> Result<(ColorImage, [u32; 2]), String> {
    let cached = (max_side <= MAX_CACHED_SIDE)
        .then(|| thumbnails::from_cache(path, modified, max_side))
        .flatten();
    let thumbnail = match cached {
        Some(thumbnail) => Some(thumbnail),
        None if crate::video::is_video(path) => {
            Some(thumbnails::generate_video(path, modified, max_side)?)
        }
        None => None,
    };
    if let Some(thumbnail) = thumbnail {
        let image = thumbnail.image;
        let size = [image.width() as usize, image.height() as usize];
        let original_size = thumbnail
            .original_size
            .unwrap_or([image.width(), image.height()]);
        return Ok((
            ColorImage::from_rgba_unmultiplied(size, &image),
            original_size,
        ));
    }

    let mut decoder = image::ImageReader::open(path)
        .map_err(|e| e.to_string())?
        .with_guessed_format()
        .map_err(|e| e.to_string())?
        .into_decoder()
        .map_err(|e| e.to_string())?;
    let orientation = decoder.orientation().ok();
    let mut image = image::DynamicImage::from_decoder(decoder).map_err(|e| e.to_string())?;
    if let Some(orientation) = orientation {
        image.apply_orientation(orientation);
    }
    let original_size = [image.width(), image.height()];
    if image.width() > max_side || image.height() > max_side {
        image = image.thumbnail(max_side, max_side);
    }
    let rgba = image.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    Ok((
        ColorImage::from_rgba_unmultiplied(size, &rgba),
        original_size,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_and_downscales_in_background() {
        let dir = std::env::temp_dir().join(format!("egui-files-images-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("wide.png");
        image::RgbaImage::from_pixel(600, 300, image::Rgba([255, 0, 0, 255]))
            .save(&path)
            .unwrap();
        let broken = dir.join("broken.jpg");
        std::fs::write(&broken, b"not a jpeg").unwrap();

        let ctx = Context::default();
        let mut cache = ImageCache::new(&ctx);
        assert!(matches!(
            cache.get(&dir.join("a.txt"), None, 64),
            Lookup::Unsupported
        ));

        let deadline = web_time::Instant::now() + std::time::Duration::from_secs(10);
        let (mut image_done, mut broken_done) = (false, false);
        while !(image_done && broken_done) {
            assert!(web_time::Instant::now() < deadline, "decode timed out");
            cache.begin_frame();
            if let Lookup::Ready(image) = cache.get(&path, None, 100) {
                assert_eq!(image.original_size, [600, 300]);
                assert_eq!(image.texture.size(), [100, 50]);
                image_done = true;
            }
            broken_done = matches!(cache.get(&broken, None, 100), Lookup::Failed(_));
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn evicts_least_recently_used_by_size() {
        let ctx = Context::default();
        let mut cache = ImageCache::new(&ctx);
        // Eight 16 MiB textures exceed the budget; eviction goes down to 80%.
        let side = 2048;
        for i in 0..8 {
            cache.frame += 1;
            let key = Key {
                path: PathBuf::from(format!("{i}.png")),
                modified: None,
                max_side: side as u32,
            };
            let texture = ctx.load_texture(
                i.to_string(),
                ColorImage::filled([side, side], egui::Color32::BLACK),
                TextureOptions::LINEAR,
            );
            let image = Image {
                texture,
                original_size: [side as u32; 2],
            };
            let last_used = cache.frame;
            cache.slots.insert(key, Slot::Ready { image, last_used });
        }
        cache.begin_frame();
        let ready: Vec<_> = (0..8)
            .filter(|i| cache.is_ready(Path::new(&format!("{i}.png")), None, side as u32))
            .collect();
        assert_eq!(ready, [3, 4, 5, 6, 7]);
    }
}
