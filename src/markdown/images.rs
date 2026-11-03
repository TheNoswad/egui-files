//! The pictures in rendered documents: read or fetched in the background,
//! decoded, and kept as textures.
//!
//! Remote pictures are only fetched for documents that may (see
//! [`Images::size`]): fetching tells the server someone is reading.

use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::{
        Arc, OnceLock,
        mpsc::{self, Receiver, Sender},
    },
};

use egui::{ColorImage, Context, TextureHandle, TextureOptions, Vec2};

use super::parse::is_remote;

/// Files or downloads at once.
const MAX_LOADING: usize = 4;
/// Longest side a picture is decoded to.
const MAX_SIDE: u32 = 2048;
/// Stops reading or downloading past this.
const MAX_BYTES: usize = 32 << 20;
/// Textures kept past this are dropped, those unused the longest first.
const BUDGET_BYTES: usize = 200 << 20;
/// A vector picture is drawn this many times its size, to stay sharp on
/// high density screens and when zoomed.
const SVG_SCALE: f32 = 2.0;

enum Slot {
    Waiting,
    Ready {
        texture: TextureHandle,
        size: Vec2,
        last_used: u64,
    },
    Failed,
}

struct Done {
    key: String,
    result: Result<(ColorImage, Vec2), String>,
}

/// Decoded pictures, by absolute path or URL.
pub struct Images {
    slots: HashMap<String, Slot>,
    queue: VecDeque<String>,
    loading: usize,
    sender: Sender<Done>,
    results: Receiver<Done>,
    frame: u64,
    /// Goes up when a picture arrives: the documents showing it lay out again.
    pub generation: u64,
}

impl Default for Images {
    fn default() -> Self {
        let (sender, results) = mpsc::channel();
        Self {
            slots: HashMap::new(),
            queue: VecDeque::new(),
            loading: 0,
            sender,
            results,
            frame: 0,
            generation: 0,
        }
    }
}

impl Images {
    /// Takes in what finished and starts what's waiting. Call once per frame.
    pub fn begin_frame(&mut self, ctx: &Context) {
        self.frame += 1;
        for Done { key, result } in self.results.try_iter().collect::<Vec<_>>() {
            self.loading = self.loading.saturating_sub(1);
            self.generation += 1;
            let slot = match result {
                Ok((image, size)) => Slot::Ready {
                    texture: ctx.load_texture(&key, image, TextureOptions::LINEAR),
                    size,
                    last_used: self.frame,
                },
                Err(err) => {
                    eprintln!("markdown image {key}: {err}");
                    Slot::Failed
                }
            };
            self.slots.insert(key, slot);
        }
        while self.loading < MAX_LOADING {
            let Some(key) = self.queue.pop_back() else {
                break;
            };
            self.loading += 1;
            start(ctx, key, self.sender.clone());
        }
        self.evict();
    }

    /// Asks for a picture. `None` until it's here, and if it never will be.
    pub fn size(&mut self, key: &str) -> Option<Vec2> {
        if !self.slots.contains_key(key) {
            self.slots.insert(key.to_owned(), Slot::Waiting);
            self.queue.push_back(key.to_owned());
        }
        match self.slots.get(key) {
            Some(Slot::Ready { size, .. }) => Some(*size),
            _ => None,
        }
    }

    /// The texture to draw, if it's here.
    pub fn texture(&mut self, key: &str) -> Option<&TextureHandle> {
        match self.slots.get_mut(key) {
            Some(Slot::Ready {
                texture, last_used, ..
            }) => {
                *last_used = self.frame;
                Some(texture)
            }
            _ => None,
        }
    }

    fn evict(&mut self) {
        let bytes = |texture: &TextureHandle| {
            let [w, h] = texture.size();
            w * h * 4
        };
        let mut used: usize = self
            .slots
            .values()
            .map(|slot| match slot {
                Slot::Ready { texture, .. } => bytes(texture),
                _ => 0,
            })
            .sum();
        if used <= BUDGET_BYTES {
            return;
        }
        let mut by_age: Vec<(u64, String)> = self
            .slots
            .iter()
            .filter_map(|(key, slot)| match slot {
                Slot::Ready { last_used, .. } => Some((*last_used, key.clone())),
                _ => None,
            })
            .collect();
        by_age.sort_unstable();
        for (last_used, key) in by_age {
            // What's on screen stays, or it would come back straight away.
            if used <= BUDGET_BYTES * 4 / 5 || last_used + 1 >= self.frame {
                break;
            }
            if let Some(Slot::Ready { texture, .. }) = self.slots.remove(&key) {
                used -= bytes(&texture);
                self.generation += 1;
            }
        }
    }
}

/// What to load for an image in a document in `dir`. `None` for what can't
/// be: other schemes, and remote pictures when they aren't allowed.
pub fn resolve(url: &str, dir: &Path, allow_remote: bool) -> Option<String> {
    if is_remote(url) {
        return allow_remote.then(|| url.to_owned());
    }
    if url.contains("://") && !url.starts_with("file://") || url.starts_with("data:") {
        return None;
    }
    super::links::local_path(url, dir).map(|(path, _)| path.display().to_string())
}

fn start(ctx: &Context, key: String, sender: Sender<Done>) {
    let finish = {
        let ctx = ctx.clone();
        move |key: String, sender: &Sender<Done>, result: Result<Vec<u8>, String>| {
            let ctx = ctx.clone();
            let sender = sender.clone();
            // Decoding off the downloading thread: on the web that's the same
            // one, but it's then at least a frame later.
            crate::background::spawn("markdown-image", move || {
                let result = result.and_then(|bytes| decode(&bytes));
                let _ = sender.send(Done { key, result });
                ctx.request_repaint();
            });
        }
    };
    if is_remote(&key) {
        fetch(key, sender, finish);
    } else {
        crate::background::spawn("markdown-image-read", move || {
            let result = read(Path::new(&key));
            finish(key, &sender, result);
        });
    }
}

fn read(path: &Path) -> Result<Vec<u8>, String> {
    let meta = disk::metadata(path).map_err(|err| err.to_string())?;
    if meta.len() > MAX_BYTES as u64 {
        return Err("too large".to_owned());
    }
    disk::read(PathBuf::from(path)).map_err(|err| err.to_string())
}

/// Downloads a picture, giving up if it's larger than any picture should be.
fn fetch(
    url: String,
    sender: Sender<Done>,
    finish: impl Fn(String, &Sender<Done>, Result<Vec<u8>, String>) + Send + 'static,
) {
    use std::ops::ControlFlow;

    use ehttp::streaming::Part;

    let bytes = std::sync::Mutex::new(Vec::new());
    let request = ehttp::Request::get(&url);
    ehttp::streaming::fetch(request, move |part| match part {
        Err(err) => {
            finish(url.clone(), &sender, Err(err));
            ControlFlow::Break(())
        }
        Ok(Part::Response(response)) => {
            if response.ok {
                ControlFlow::Continue(())
            } else {
                let status = format!("{} {}", response.status, response.status_text);
                finish(url.clone(), &sender, Err(status));
                ControlFlow::Break(())
            }
        }
        Ok(Part::Chunk(chunk)) => {
            let mut bytes = bytes.lock().expect("no panic while holding it");
            if chunk.is_empty() {
                finish(url.clone(), &sender, Ok(std::mem::take(&mut *bytes)));
                ControlFlow::Break(())
            } else if bytes.len() + chunk.len() > MAX_BYTES {
                finish(url.clone(), &sender, Err("too large".to_owned()));
                ControlFlow::Break(())
            } else {
                bytes.extend_from_slice(&chunk);
                ControlFlow::Continue(())
            }
        }
    });
}

/// The picture in the bytes, and the size to show it at, in points.
fn decode(bytes: &[u8]) -> Result<(ColorImage, Vec2), String> {
    if looks_like_svg(bytes) {
        return decode_svg(bytes);
    }
    let image = image::load_from_memory(bytes).map_err(|err| err.to_string())?;
    let size = Vec2::new(image.width() as f32, image.height() as f32);
    let image = if image.width().max(image.height()) > MAX_SIDE {
        image.thumbnail(MAX_SIDE, MAX_SIDE)
    } else {
        image
    };
    let rgba = image.to_rgba8();
    let pixels = [rgba.width() as usize, rgba.height() as usize];
    Ok((
        ColorImage::from_rgba_unmultiplied(pixels, rgba.as_raw()),
        size,
    ))
}

fn looks_like_svg(bytes: &[u8]) -> bool {
    let start = &bytes[..bytes.len().min(1024)];
    let text = String::from_utf8_lossy(start);
    let text = text.trim_start_matches('\u{FEFF}').trim_start();
    text.starts_with("<svg")
        || (text.starts_with("<?xml") || text.starts_with("<!--")) && text.contains("<svg")
}

fn decode_svg(bytes: &[u8]) -> Result<(ColorImage, Vec2), String> {
    use resvg::{tiny_skia, usvg};

    let options = usvg::Options {
        // Text in badges and diagrams: what's installed, and a font that's
        // always here, as the browser has none to offer.
        font_family: "Inter".to_owned(),
        fontdb: svg_fonts(),
        ..usvg::Options::default()
    };
    let tree = usvg::Tree::from_data(bytes, &options).map_err(|err| err.to_string())?;
    let size = tree.size();
    let scale = SVG_SCALE.min(MAX_SIDE as f32 / size.width().max(size.height()).max(1.0));
    let width = ((size.width() * scale).ceil() as u32).clamp(1, MAX_SIDE);
    let height = ((size.height() * scale).ceil() as u32).clamp(1, MAX_SIDE);
    let mut pixmap = tiny_skia::Pixmap::new(width, height).ok_or_else(|| "bad size".to_owned())?;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    Ok((
        ColorImage::from_rgba_premultiplied([width as usize, height as usize], pixmap.data()),
        Vec2::new(size.width(), size.height()),
    ))
}

/// Fonts for text in SVGs, found once, the first time one is drawn.
fn svg_fonts() -> Arc<fontdb::Database> {
    static FONTS: OnceLock<Arc<fontdb::Database>> = OnceLock::new();
    FONTS
        .get_or_init(|| {
            let mut fonts = fontdb::Database::new();
            #[cfg(not(target_arch = "wasm32"))]
            fonts.load_system_fonts();
            fonts.load_font_data(crate::theme::fonts::INTER_REGULAR.to_vec());
            fonts.load_font_data(crate::theme::fonts::INTER_BOLD.to_vec());
            fonts.set_sans_serif_family("Inter");
            Arc::new(fonts)
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_what_may_be_loaded_resolves() {
        let dir = Path::new("/docs");
        assert_eq!(resolve("https://a/b.png", dir, false), None);
        assert_eq!(
            resolve("https://a/b.png", dir, true).as_deref(),
            Some("https://a/b.png")
        );
        assert_eq!(
            resolve("pics/a%20b.png", dir, false).as_deref(),
            Some(Path::new("/docs/pics/a b.png").to_str().unwrap())
        );
        assert_eq!(resolve("data:image/png;base64,AAAA", dir, true), None);
        assert_eq!(resolve("ftp://a/b.png", dir, true), None);
    }

    #[test]
    fn pictures_decode_from_their_bytes() {
        let mut png = Vec::new();
        image::DynamicImage::new_rgba8(3, 2)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let (image, size) = decode(&png).unwrap();
        assert_eq!(size, Vec2::new(3.0, 2.0));
        assert_eq!(image.size, [3, 2]);
        assert!(decode(b"not a picture").is_err());
    }

    #[test]
    fn svgs_are_drawn_at_twice_their_size() {
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="10">
            <rect width="20" height="10" fill="red"/></svg>"#;
        let (image, size) = decode(svg).unwrap();
        assert_eq!(size, Vec2::new(20.0, 10.0));
        assert_eq!(image.size, [40, 20]);
        assert_eq!(image.pixels[0], egui::Color32::RED);
        let declared = b"<?xml version=\"1.0\"?>\n<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"4\" height=\"4\"/>";
        assert!(decode(declared).is_ok());
    }

    #[test]
    fn huge_pictures_are_limited() {
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="100000" height="50"/>"#;
        let (image, _) = decode(svg).unwrap();
        assert!(image.size[0] <= MAX_SIDE as usize);
    }
}
