//! Background image decoding with an LRU cache of GPU textures. Thumbnails
//! come from the shared system cache when possible, which is also how videos
//! get theirs (see [`crate::thumbnails`]).
//!
//! Views call [`ImageCache::get`] every frame for the images they want to
//! draw. Missing images are queued for a pool of worker threads, newest
//! request first, so whatever is on screen right now is decoded first.
//!
//! A server's pictures take a while to arrive, so those have workers of their
//! own, more of them, as most of their time goes on waiting. A JPEG's own
//! small thumbnail, if it has one, is shown first (see [`crate::exif`]); those
//! still on screen are replaced by sharper ones made of the whole file, one at
//! a time and only when nothing else is waiting.

use std::{
    collections::HashMap,
    io::{BufRead, Read, Seek},
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
/// Workers for a server's pictures, nearly all the time waiting for it.
const REMOTE_WORKERS: usize = 8;
/// As much of a server's JPEG as is read to find its thumbnail: Exif data
/// can't be much longer than this, and comes first.
const JPEG_HEAD_BYTES: u64 = 72 * 1024;

/// Whether a picture can be made of the file: an image, or a video's thumbnail.
pub fn is_supported(path: &Path) -> bool {
    is_image(path) || crate::video::is_video(path)
}

pub fn is_image(path: &Path) -> bool {
    crate::raw::is_raw(path)
        || path
            .extension()
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
    /// For a raw photo, the size of the JPEG in it that's shown.
    pub embedded_size: Option<[u32; 2]>,
}

impl Image {
    fn byte_size(&self) -> usize {
        let [w, h] = self.texture.size();
        w * h * 4
    }
}

enum Slot {
    Pending,
    Ready {
        image: Image,
        last_used: u64,
        /// It's a JPEG's own small thumbnail: there's a sharper one to make.
        provisional: bool,
        /// That's been asked for.
        sharpening: bool,
    },
    Failed(String),
}

pub enum Lookup<'a> {
    Ready(&'a Image),
    Pending,
    Failed(&'a str),
    Unsupported,
}

struct Decoded {
    image: ColorImage,
    original_size: [u32; 2],
    embedded_size: Option<[u32; 2]>,
    provisional: bool,
}

struct Done {
    key: Key,
    result: Result<Decoded, String>,
    /// It was to replace a provisional picture.
    sharpening: bool,
}

#[derive(Default)]
struct Jobs {
    /// Pictures to decode with the frame that asked for them, newest last.
    waiting: Vec<(u64, Key)>,
    /// Provisional pictures to replace, newest last.
    to_sharpen: Vec<Key>,
    /// One is being replaced, until the UI has it: it may be on screen and
    /// look wanted until then.
    sharpening: bool,
}

impl Jobs {
    /// The next job, if there's one to start, and whether it's to sharpen.
    fn next(&mut self) -> Option<(Key, bool)> {
        // The newest frame's first, as what it asked for is what's on screen;
        // and in the order it asked, as views draw in reading order, so what's
        // on screen fills in that way.
        if let Some(&(newest, _)) = self.waiting.last() {
            let first = self.waiting.partition_point(|(frame, _)| *frame < newest);
            let (_, key) = self.waiting.remove(first);
            return Some((key, false));
        }
        if self.sharpening {
            return None;
        }
        let key = self.to_sharpen.pop()?;
        self.sharpening = true;
        Some((key, true))
    }
}

struct Queue {
    jobs: Mutex<Jobs>,
    ready: Condvar,
    shutdown: AtomicBool,
    /// How many are decoding, at most `max_decoding`: pictures take much
    /// memory and CPU, even with more workers waiting for files.
    decoding: Mutex<usize>,
    decoded: Condvar,
    max_decoding: usize,
}

impl Queue {
    fn new(max_decoding: usize) -> Self {
        Self {
            jobs: Mutex::default(),
            ready: Condvar::new(),
            shutdown: AtomicBool::new(false),
            decoding: Mutex::new(0),
            decoded: Condvar::new(),
            max_decoding,
        }
    }

    /// Waits for a turn to decode, which lasts as long as what's returned.
    fn turn_to_decode(&self) -> DecodingTurn<'_> {
        let mut decoding = self.decoding.lock().expect("image queue poisoned");
        while *decoding >= self.max_decoding {
            decoding = self.decoded.wait(decoding).expect("image queue poisoned");
        }
        *decoding += 1;
        DecodingTurn(self)
    }
}

struct DecodingTurn<'a>(&'a Queue);

impl Drop for DecodingTurn<'_> {
    fn drop(&mut self) {
        // Not when poisoned, which is a panic on its way anyway.
        if let Ok(mut decoding) = self.0.decoding.lock() {
            *decoding -= 1;
            self.0.decoded.notify_one();
        }
    }
}

pub struct ImageCache {
    ctx: Context,
    queue: Arc<Queue>,
    /// For a server's pictures, once there are any.
    remote_queue: Option<Arc<Queue>>,
    decoders: usize,
    sender: Sender<Done>,
    results: Receiver<Done>,
    slots: HashMap<Key, Slot>,
    frame: u64,
    /// No worker thread could start (the browser can't): `begin_frame`
    /// decodes instead.
    inline: bool,
}

impl ImageCache {
    pub fn new(ctx: &Context) -> Self {
        let (sender, results) = mpsc::channel();
        let decoders = std::thread::available_parallelism()
            .map_or(2, |n| n.get().saturating_sub(1))
            .clamp(1, 4);
        let queue = Arc::new(Queue::new(decoders));
        let started = start_workers(&queue, decoders, "image-decoder", &sender, ctx);
        Self {
            ctx: ctx.clone(),
            queue,
            remote_queue: None,
            decoders,
            sender,
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
        for Done {
            key,
            result,
            sharpening,
        } in done
        {
            if sharpening && let Some(queue) = &self.remote_queue {
                queue.jobs.lock().expect("image queue poisoned").sharpening = false;
                queue.ready.notify_all();
            }
            let slot = match result {
                Ok(decoded) => {
                    let name = key.path.display().to_string();
                    let texture =
                        self.ctx
                            .load_texture(name, decoded.image, TextureOptions::LINEAR);
                    Slot::Ready {
                        image: Image {
                            texture,
                            original_size: decoded.original_size,
                            embedded_size: decoded.embedded_size,
                        },
                        last_used: self.frame,
                        provisional: decoded.provisional,
                        sharpening: false,
                    }
                }
                // What's on screen will do.
                Err(_) if sharpening => {
                    if let Some(Slot::Ready { provisional, .. }) = self.slots.get_mut(&key) {
                        *provisional = false;
                    }
                    continue;
                }
                Err(err) => Slot::Failed(err),
            };
            self.slots.insert(key, slot);
        }
        self.forget_unseen_sharpening();
        self.evict();
    }

    /// Only what's on screen is worth a whole file: this frame and the last
    /// are as far back as that's known.
    fn forget_unseen_sharpening(&mut self) {
        let Some(queue) = &self.remote_queue else {
            return;
        };
        let frame = self.frame;
        let slots = &mut self.slots;
        queue
            .jobs
            .lock()
            .expect("image queue poisoned")
            .to_sharpen
            .retain(|key| match slots.get_mut(key) {
                Some(Slot::Ready {
                    last_used,
                    sharpening,
                    ..
                }) => {
                    let seen = *last_used + 1 >= frame;
                    *sharpening = seen;
                    seen
                }
                _ => false,
            });
    }

    /// Without worker threads, decodes the newest request, one a frame so
    /// the UI keeps responding. In the browser, of those whose files are here:
    /// reading one that isn't would stop the page until it is.
    fn decode_inline(&self) -> Option<Done> {
        let key = {
            let mut jobs = self.queue.jobs.lock().expect("image queue poisoned");
            let jobs = &mut jobs.waiting;
            #[cfg(target_arch = "wasm32")]
            let here = |key: &Key| crate::demo::is_here(&key.path);
            #[cfg(not(target_arch = "wasm32"))]
            let here = |_: &Key| true;
            // In the same order as `Jobs::next`.
            let (newest, _) = jobs.iter().rev().find(|(_, key)| here(key))?;
            let newest = *newest;
            let index = jobs
                .iter()
                .position(|(frame, key)| *frame == newest && here(key))?;
            jobs.remove(index).1
        };
        let result = decode(&key, false, &self.queue);
        self.ctx.request_repaint();
        Some(Done {
            key,
            result,
            sharpening: false,
        })
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
                Slot::Ready {
                    image, last_used, ..
                } => Some((*last_used, image.byte_size(), key.clone())),
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

    /// Tries again the pictures in `root` that couldn't be read: a phone's,
    /// which fail while it's away and are there once it's back.
    pub fn retry_failed_within(&mut self, root: &Path) {
        self.slots
            .retain(|key, slot| !(matches!(slot, Slot::Failed(_)) && key.path.starts_with(root)));
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
            Slot::Ready {
                image,
                last_used,
                provisional,
                sharpening,
            } => {
                *last_used = self.frame;
                if *provisional
                    && !*sharpening
                    && let Some(queue) = &self.remote_queue
                {
                    *sharpening = true;
                    let mut jobs = queue.jobs.lock().expect("image queue poisoned");
                    jobs.to_sharpen.push(key);
                    queue.ready.notify_one();
                }
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

    /// The queue for a picture at `path`.
    fn queue_for(&mut self, path: &Path) -> Arc<Queue> {
        if !crate::remote::is_remote(path) {
            return self.queue.clone();
        }
        if self.remote_queue.is_none() {
            let queue = Arc::new(Queue::new(self.decoders));
            let started = start_workers(
                &queue,
                REMOTE_WORKERS,
                "image-fetcher",
                &self.sender,
                &self.ctx,
            );
            // No threads to spare: the others will have to do.
            self.remote_queue = Some(if started == 0 {
                self.queue.clone()
            } else {
                queue
            });
        }
        self.remote_queue.clone().expect("just set")
    }

    fn enqueue(&mut self, key: Key) {
        let queue = self.queue_for(&key.path);
        let mut jobs = queue.jobs.lock().expect("image queue poisoned");
        let jobs = &mut jobs.waiting;
        jobs.push((self.frame, key));
        if jobs.len() > MAX_QUEUE {
            let excess = jobs.len() - MAX_QUEUE;
            let dropped: Vec<(u64, Key)> = jobs.drain(..excess).collect();
            for (_, key) in dropped {
                // Forget it so it is requested again if it comes back into view.
                self.slots.remove(&key);
            }
        }
        queue.ready.notify_one();
    }
}

impl Drop for ImageCache {
    fn drop(&mut self) {
        for queue in std::iter::once(&self.queue).chain(&self.remote_queue) {
            queue.shutdown.store(true, Ordering::Relaxed);
            queue.ready.notify_all();
        }
    }
}

/// Starts up to `count` workers on `queue`, returning how many did.
fn start_workers(
    queue: &Arc<Queue>,
    count: usize,
    name: &str,
    sender: &Sender<Done>,
    ctx: &Context,
) -> usize {
    (0..count)
        .filter(|i| {
            let (queue, sender, ctx) = (queue.clone(), sender.clone(), ctx.clone());
            std::thread::Builder::new()
                .name(format!("{name}-{i}"))
                .spawn(move || worker(&queue, &sender, &ctx))
                .is_ok()
        })
        .count()
}

fn worker(queue: &Queue, sender: &Sender<Done>, ctx: &Context) {
    loop {
        let (key, sharpening) = {
            let mut jobs = queue.jobs.lock().expect("image queue poisoned");
            loop {
                if queue.shutdown.load(Ordering::Relaxed) {
                    return;
                }
                if let Some(job) = jobs.next() {
                    break job;
                }
                jobs = queue.ready.wait(jobs).expect("image queue poisoned");
            }
        };
        let result = decode(&key, !sharpening, queue);
        let done = Done {
            key,
            result,
            sharpening,
        };
        if sender.send(done).is_err() {
            return;
        }
        ctx.request_repaint();
    }
}

/// Decodes a picture, from the file's own thumbnail if `may_be_provisional`
/// and that'll do for now.
fn decode(key: &Key, may_be_provisional: bool, queue: &Queue) -> Result<Decoded, String> {
    let Key {
        path,
        modified,
        max_side,
    } = key;
    let (path, modified, max_side) = (path.as_path(), *modified, *max_side);
    let cached = (max_side <= MAX_CACHED_SIDE)
        .then(|| thumbnails::from_cache(path, modified, max_side))
        .flatten();
    let thumbnail = match cached {
        Some(thumbnail) => Some(thumbnail),
        None if crate::video::is_video(path) && !crate::remote::is_proton(path) => {
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
        return Ok(Decoded {
            image: ColorImage::from_rgba_unmultiplied(size, &image),
            original_size,
            embedded_size: None,
            provisional: false,
        });
    }

    if let Some(decoded) = proton_thumbnail(path, modified, max_side) {
        return Ok(decoded);
    }
    let mut file = disk::File::open(path).map_err(|e| e.to_string())?;
    if crate::raw::is_raw(path) {
        return decode_raw(&mut file, path, modified, max_side, queue);
    }
    if !crate::remote::is_remote(path) {
        let _turn = queue.turn_to_decode();
        return decode_file(std::io::BufReader::new(file), path, modified, max_side);
    }
    // All of it, up front: decoders go back and forth, each time a round trip.
    let mut bytes = Vec::new();
    if may_be_provisional && max_side <= MAX_CACHED_SIDE && is_jpeg(path) {
        (&mut file)
            .take(JPEG_HEAD_BYTES)
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        if let Some(decoded) = from_embedded_thumbnail(&bytes, max_side) {
            return Ok(decoded);
        }
    }
    file.read_to_end(&mut bytes).map_err(|e| e.to_string())?;
    drop(file);
    let _turn = queue.turn_to_decode();
    decode_file(std::io::Cursor::new(bytes), path, modified, max_side)
}

/// The Drive keeps a small picture of each picture in it, which saves
/// fetching and decrypting the whole file to show it small.
fn proton_thumbnail(path: &Path, modified: Option<SystemTime>, max_side: u32) -> Option<Decoded> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        const SIDE: u32 = 512;
        if max_side > SIDE || !disk::proton::is_proton(path) {
            return None;
        }
        let bytes = disk::proton::thumbnail(path).ok().flatten()?;
        decode_file(std::io::Cursor::new(bytes), path, modified, max_side).ok()
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = (path, modified, max_side);
        None
    }
}

fn is_jpeg(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("jpg") || e.eq_ignore_ascii_case("jpeg"))
}

/// A picture made of the thumbnail in `head`, the start of a JPEG file. Some
/// cameras pad theirs with bars to fit a fixed shape, so one not the shape of
/// the picture won't do.
fn from_embedded_thumbnail(head: &[u8], max_side: u32) -> Option<Decoded> {
    let embedded = crate::exif::embedded_thumbnail(head)?;
    let mut image =
        image::load_from_memory_with_format(embedded.jpeg, image::ImageFormat::Jpeg).ok()?;
    let (width, height) = (u64::from(image.width()), u64::from(image.height()));
    // Too small to be worth showing, if it's anything at all.
    if width.max(height) < 32 {
        return None;
    }
    if let Some([full_width, full_height]) = embedded.size {
        let (full_width, full_height) = (u64::from(full_width), u64::from(full_height));
        let difference = (width * full_height).abs_diff(height * full_width);
        if difference * 50 > width * full_height {
            return None;
        }
    }
    if let Some(orientation) = image::metadata::Orientation::from_exif(embedded.orientation) {
        image.apply_orientation(orientation);
    }
    let original_size = embedded
        .size
        .map_or([image.width(), image.height()], |size| {
            // Stored one way, shown turned a quarter.
            if (5..=8).contains(&embedded.orientation) {
                [size[1], size[0]]
            } else {
                size
            }
        });
    // The size it would've been, so nothing moves when the sharper one comes.
    let side = max_side.min(original_size[0].max(original_size[1]));
    let provisional = image.width().max(image.height()) < side;
    let image = if provisional {
        image.resize(side, side, image::imageops::FilterType::Triangle)
    } else {
        image.thumbnail(side, side)
    };
    let rgba = image.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    Some(Decoded {
        image: ColorImage::from_rgba_unmultiplied(size, &rgba),
        original_size,
        embedded_size: None,
        provisional,
    })
}

/// Shows the JPEG the camera stored in a raw photo, of the size that's wanted.
fn decode_raw(
    file: &mut disk::File,
    path: &Path,
    modified: Option<SystemTime>,
    max_side: u32,
    queue: &Queue,
) -> Result<Decoded, String> {
    let preview = crate::raw::preview(file, max_side)
        .ok_or_else(|| "This raw file has no picture stored in it".to_owned())?;
    let _turn = queue.turn_to_decode();
    let mut image = image::load_from_memory_with_format(&preview.jpeg, image::ImageFormat::Jpeg)
        .map_err(|e| e.to_string())?;
    if let Some(orientation) = image::metadata::Orientation::from_exif(preview.orientation) {
        image.apply_orientation(orientation);
    }
    let turned = |[w, h]: [u32; 2]| {
        if crate::raw::is_turned(preview.orientation) {
            [h, w]
        } else {
            [w, h]
        }
    };
    let original_size = turned(preview.sensor);
    if image.width() > max_side || image.height() > max_side {
        image = image.thumbnail(max_side, max_side);
    }
    if max_side <= MAX_CACHED_SIDE {
        thumbnails::keep_raw(path, modified, &image, original_size);
    }
    let rgba = image.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    Ok(Decoded {
        image: ColorImage::from_rgba_unmultiplied(size, &rgba),
        original_size,
        embedded_size: Some(turned(preview.size)),
        provisional: false,
    })
}

fn decode_file(
    file: impl BufRead + Seek,
    path: &Path,
    modified: Option<SystemTime>,
    max_side: u32,
) -> Result<Decoded, String> {
    let mut decoder = image::ImageReader::new(file)
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
    thumbnails::keep_remote(path, modified, &image);
    if image.width() > max_side || image.height() > max_side {
        image = image.thumbnail(max_side, max_side);
    }
    let rgba = image.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    Ok(Decoded {
        image: ColorImage::from_rgba_unmultiplied(size, &rgba),
        original_size,
        embedded_size: None,
        provisional: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_and_downscales_in_background() {
        let dir = std::env::temp_dir().join(format!("egui-files-images-{}", std::process::id()));
        disk::create_dir_all(&dir).unwrap();
        let path = dir.join("wide.png");
        image::RgbaImage::from_pixel(600, 300, image::Rgba([255, 0, 0, 255]))
            .save(&path)
            .unwrap();
        let broken = dir.join("broken.jpg");
        disk::write(&broken, b"not a jpeg").unwrap();

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

        // It stays failed, even once it's fine, until told to try again.
        disk::copy(&path, &broken).unwrap();
        cache.begin_frame();
        assert!(matches!(cache.get(&broken, None, 100), Lookup::Failed(_)));
        cache.retry_failed_within(Path::new("/somewhere/else"));
        assert!(matches!(cache.get(&broken, None, 100), Lookup::Failed(_)));
        cache.retry_failed_within(&dir);
        while !matches!(cache.get(&broken, None, 100), Lookup::Ready(_)) {
            assert!(web_time::Instant::now() < deadline, "decode timed out");
            cache.begin_frame();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        disk::remove_dir_all(dir).unwrap();
    }

    /// A JPEG of the given size, of one color.
    fn jpeg_of(size: [u32; 2]) -> Vec<u8> {
        let image = image::RgbImage::from_pixel(size[0], size[1], image::Rgb([200, 30, 30]));
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgb8(image)
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Jpeg,
            )
            .unwrap();
        bytes
    }

    #[test]
    fn small_embedded_thumbnails_are_provisional_at_full_size() {
        let head = crate::exif::tests::jpeg_start(&jpeg_of([160, 120]), 1, [4000, 3000]);
        let decoded = from_embedded_thumbnail(&head, 256).unwrap();
        assert_eq!(decoded.image.size, [256, 192]);
        assert_eq!(decoded.original_size, [4000, 3000]);
        assert!(decoded.provisional);
    }

    #[test]
    fn embedded_thumbnails_turn_as_the_picture_does() {
        let head = crate::exif::tests::jpeg_start(&jpeg_of([160, 120]), 6, [4000, 3000]);
        let decoded = from_embedded_thumbnail(&head, 256).unwrap();
        assert_eq!(decoded.image.size, [192, 256]);
        assert_eq!(decoded.original_size, [3000, 4000]);
    }

    #[test]
    fn big_enough_embedded_thumbnails_are_final() {
        let head = crate::exif::tests::jpeg_start(&jpeg_of([320, 240]), 1, [4000, 3000]);
        let decoded = from_embedded_thumbnail(&head, 256).unwrap();
        assert_eq!(decoded.image.size, [256, 192]);
        assert!(!decoded.provisional);
    }

    #[test]
    fn embedded_thumbnails_of_another_shape_are_passed_over() {
        // 4:3, for a 3:2 picture: there would be bars.
        let head = crate::exif::tests::jpeg_start(&jpeg_of([160, 120]), 1, [6000, 4000]);
        assert!(from_embedded_thumbnail(&head, 256).is_none());
    }

    #[test]
    fn sharpening_waits_for_other_jobs_and_goes_one_at_a_time() {
        let key = |name: &str| Key {
            path: PathBuf::from(name),
            modified: None,
            max_side: THUMBNAIL_SIZE,
        };
        let mut jobs = Jobs {
            waiting: vec![(1, key("old")), (2, key("new"))],
            to_sharpen: vec![key("a"), key("b")],
            sharpening: false,
        };
        assert_eq!(jobs.next(), Some((key("new"), false)));
        assert_eq!(jobs.next(), Some((key("old"), false)));
        assert_eq!(jobs.next(), Some((key("b"), true)));
        assert_eq!(jobs.next(), None, "one's still going");
        jobs.waiting.push((3, key("another")));
        assert_eq!(jobs.next(), Some((key("another"), false)));
        jobs.sharpening = false;
        assert_eq!(jobs.next(), Some((key("a"), true)));
    }

    /// What's on screen fills in as it's read, and what's scrolled to comes
    /// ahead of what's been scrolled past.
    #[test]
    fn a_frames_pictures_load_in_the_order_it_asked_for_them() {
        let key = |name: &str| Key {
            path: PathBuf::from(name),
            modified: None,
            max_side: THUMBNAIL_SIZE,
        };
        let mut jobs = Jobs {
            waiting: vec![
                (1, key("first screen, top")),
                (1, key("first screen, bottom")),
                (2, key("scrolled to, top")),
                (2, key("scrolled to, bottom")),
            ],
            to_sharpen: Vec::new(),
            sharpening: false,
        };
        let mut next = || jobs.next().map(|(key, _)| key.path);
        assert_eq!(next(), Some("scrolled to, top".into()));
        assert_eq!(next(), Some("scrolled to, bottom".into()));
        assert_eq!(next(), Some("first screen, top".into()));
        assert_eq!(next(), Some("first screen, bottom".into()));
        assert_eq!(next(), None);
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
                embedded_size: None,
            };
            let last_used = cache.frame;
            cache.slots.insert(
                key,
                Slot::Ready {
                    image,
                    last_used,
                    provisional: false,
                    sharpening: false,
                },
            );
        }
        cache.begin_frame();
        let ready: Vec<_> = (0..8)
            .filter(|i| cache.is_ready(Path::new(&format!("{i}.png")), None, side as u32))
            .collect();
        assert_eq!(ready, [3, 4, 5, 6, 7]);
    }
}
