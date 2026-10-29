//! Preview of a single entry: image, text excerpt, or file info.

use std::{
    io::Read,
    path::{Path, PathBuf},
    time::SystemTime,
};

use egui::{Align, Layout, RichText, Ui, vec2};
use egui_phosphor::regular as icon;

use crate::{
    fs::{self, Entry},
    images::{self, ImageCache, Lookup},
    tab::Action,
};

/// Bytes of a file read for a text preview.
const TEXT_BYTES: u64 = 64 * 1024;
const TEXT_CACHE_LEN: usize = 8;

struct TextPreview {
    path: PathBuf,
    modified: Option<SystemTime>,
    /// `None` if the file looks binary or could not be read.
    text: Option<String>,
    truncated: bool,
}

/// The last few text previews, so switching tabs doesn't re-read files every frame.
#[derive(Default)]
pub struct TextCache {
    entries: Vec<TextPreview>,
}

impl TextCache {
    fn get(&mut self, entry: &Entry) -> &TextPreview {
        let position = self
            .entries
            .iter()
            .position(|t| t.path == entry.path && t.modified == entry.modified);
        let index = match position {
            Some(index) => index,
            None => {
                if self.entries.len() >= TEXT_CACHE_LEN {
                    self.entries.remove(0);
                }
                let (text, truncated) =
                    read_text(&entry.path).map_or((None, false), |(t, tr)| (Some(t), tr));
                self.entries.push(TextPreview {
                    path: entry.path.clone(),
                    modified: entry.modified,
                    text,
                    truncated,
                });
                self.entries.len() - 1
            }
        };
        &self.entries[index]
    }
}

/// Reads the start of a file as text, or `None` if it looks binary.
fn read_text(path: &Path) -> Option<(String, bool)> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(TEXT_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    let truncated = bytes.len() as u64 > TEXT_BYTES;
    bytes.truncate(TEXT_BYTES as usize);
    if bytes.contains(&0) {
        return None;
    }
    Some((String::from_utf8_lossy(&bytes).into_owned(), truncated))
}

/// Shows a preview of `entry`. Images are decoded at most `max_side` pixels.
pub fn show(
    ui: &mut Ui,
    entry: &Entry,
    images: &mut ImageCache,
    texts: &mut TextCache,
    max_side: u32,
    actions: &mut Vec<Action>,
) {
    let mut dimensions = None;
    ui.vertical_centered(|ui| {
        ui.add_space(8.0);
        if entry.is_dir {
            big_icon(ui, entry);
        } else if images::is_supported(&entry.path) {
            dimensions = image(ui, entry, images, max_side);
        } else {
            match texts.get(entry) {
                TextPreview {
                    text: Some(text),
                    truncated,
                    ..
                } if !text.trim().is_empty() => {
                    egui::Frame::group(ui.style()).show(ui, |ui| {
                        ui.with_layout(Layout::top_down(Align::Min), |ui| {
                            ui.add(
                                egui::Label::new(
                                    RichText::new(text.as_str()).monospace().size(11.0),
                                )
                                .wrap(),
                            );
                            if *truncated {
                                ui.weak("…");
                            }
                        });
                    });
                }
                _ => big_icon(ui, entry),
            }
        }

        ui.add_space(8.0);
        ui.add(egui::Label::new(RichText::new(&entry.name).strong()).wrap());
        ui.add_space(4.0);
        ui.weak(entry.kind());
        if !entry.is_dir {
            ui.weak(fs::human_size(entry.size));
        }
        if let Some([w, h]) = dimensions {
            ui.weak(format!("{w} × {h} px"));
        }
        ui.weak(fs::format_time(entry.modified));
        ui.add_space(8.0);
        if ui
            .button(format!("{} Open", icon::ARROW_SQUARE_OUT))
            .clicked()
        {
            actions.push(Action::Activate(entry.path.clone()));
        }
    });
}

fn big_icon(ui: &mut Ui, entry: &Entry) {
    ui.label(RichText::new(entry.icon()).size(64.0));
}

/// Draws the image fitted to the available width; returns its original size once loaded.
fn image(ui: &mut Ui, entry: &Entry, images: &mut ImageCache, max_side: u32) -> Option<[u32; 2]> {
    let max_size = vec2(
        ui.available_width(),
        (ui.available_height() * 0.6).max(160.0),
    );
    // While the full image decodes, fall back to a cached thumbnail.
    let full_ready = matches!(
        images.get(&entry.path, entry.modified, max_side),
        Lookup::Ready(_)
    );
    let side =
        if !full_ready && images.is_ready(&entry.path, entry.modified, images::THUMBNAIL_SIZE) {
            images::THUMBNAIL_SIZE
        } else {
            max_side
        };
    match images.get(&entry.path, entry.modified, side) {
        Lookup::Ready(image) => {
            let texture_size = image.texture.size_vec2() / ui.pixels_per_point();
            let scale = (max_size.x / texture_size.x)
                .min(max_size.y / texture_size.y)
                .min(1.0);
            ui.image((image.texture.id(), texture_size * scale));
            Some(image.original_size)
        }
        Lookup::Pending => {
            ui.add_sized(vec2(max_size.x, 64.0), egui::Spinner::new());
            None
        }
        Lookup::Failed(err) => {
            big_icon(ui, entry);
            ui.colored_label(ui.visuals().error_fg_color, err);
            None
        }
        Lookup::Unsupported => {
            big_icon(ui, entry);
            None
        }
    }
}
