//! Preview of a single entry: image, video player, text excerpt, or file info.

use std::{
    io::Read,
    path::{Path, PathBuf},
    time::SystemTime,
};

use egui::{Align, Align2, Color32, FontId, Layout, Rect, RichText, Sense, Ui, Vec2, pos2, vec2};
use egui_phosphor::regular as icon;

use crate::{
    fs::{self, Entry},
    images::{self, ImageCache, Lookup},
    tab::Action,
    video::{self, Videos},
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
    videos: &mut Videos,
    texts: &mut TextCache,
    max_side: u32,
    actions: &mut Vec<Action>,
) {
    let mut dimensions = None;
    let mut duration = None;
    ui.vertical_centered(|ui| {
        ui.add_space(8.0);
        if entry.is_dir {
            big_icon(ui, entry);
        } else if video::is_video(&entry.path) {
            (dimensions, duration) = video_player(ui, entry, images, videos);
        } else if images::is_image(&entry.path) {
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
        if let Some(duration) = duration {
            ui.weak(video::format_time(duration));
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

/// Draws a video with play/pause and seeking; returns its size and duration once known.
fn video_player(
    ui: &mut Ui,
    entry: &Entry,
    images: &mut ImageCache,
    videos: &mut Videos,
) -> (Option<[u32; 2]>, Option<f64>) {
    let max_size = vec2(
        ui.available_width(),
        (ui.available_height() * 0.6).max(160.0),
    );
    let thumbnail = images
        .get_ready(&entry.path, entry.modified, images::THUMBNAIL_SIZE)
        .map(|image| (image.texture.id(), image.texture.size_vec2()));
    let player = match videos.player(ui.id().with("video"), &entry.path, video::Mode::Preview) {
        Ok(player) => player,
        Err(err) => {
            image(ui, entry, images, images::THUMBNAIL_SIZE);
            ui.colored_label(
                ui.visuals().warn_fg_color,
                format!("Can't play videos: {err}"),
            );
            return (None, None);
        }
    };
    let status = player.status();

    let source_size = status
        .video_size
        .map(|[w, h]| vec2(w as f32, h as f32))
        .or(thumbnail.map(|(_, size)| size))
        .unwrap_or(vec2(16.0, 9.0));
    let size = source_size * (max_size.x / source_size.x).min(max_size.y / source_size.y);
    let pixels = size * ui.pixels_per_point();
    player.set_size([pixels.x.round() as u32, pixels.y.round() as u32]);

    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    let painter = ui.painter_at(rect);
    let full_uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
    painter.rect_filled(rect, 0.0, Color32::BLACK);
    if let Some(frame) = player.frame() {
        painter.image(frame.id(), rect, full_uv, Color32::WHITE);
    } else if let Some((texture, texture_size)) = thumbnail {
        painter.image(texture, fit(texture_size, rect), full_uv, Color32::WHITE);
    }
    if let Some(err) = &status.error {
        painter.text(
            rect.center(),
            Align2::CENTER_CENTER,
            err,
            FontId::proportional(13.0),
            ui.visuals().error_fg_color,
        );
    } else if status.paused {
        painter.circle_filled(rect.center(), 24.0, Color32::from_black_alpha(150));
        painter.text(
            rect.center() + vec2(2.0, 0.0),
            Align2::CENTER_CENTER,
            icon::PLAY,
            FontId::proportional(24.0),
            Color32::WHITE,
        );
    }
    let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
    if response.clicked() {
        player.set_paused(!status.paused);
    }

    ui.add_space(4.0);
    let mut position = status.position;
    ui.scope(|ui| {
        ui.spacing_mut().slider_width = size.x;
        let slider =
            egui::Slider::new(&mut position, 0.0..=status.duration.max(0.001)).show_value(false);
        if ui.add_enabled(status.duration > 0.0, slider).changed() {
            player.seek(position);
        }
    });
    ui.allocate_ui_with_layout(
        vec2(size.x, ui.spacing().interact_size.y),
        Layout::left_to_right(Align::Center),
        |ui| {
            let play = if status.paused {
                icon::PLAY
            } else {
                icon::PAUSE
            };
            if ui
                .button(play)
                .on_hover_text(if status.paused { "Play" } else { "Pause" })
                .clicked()
            {
                player.set_paused(!status.paused);
            }
            let mute = if status.muted || status.volume == 0.0 {
                icon::SPEAKER_SLASH
            } else if status.volume < 50.0 {
                icon::SPEAKER_LOW
            } else {
                icon::SPEAKER_HIGH
            };
            if ui
                .button(mute)
                .on_hover_text(if status.muted { "Unmute" } else { "Mute" })
                .clicked()
            {
                player.set_muted(!status.muted);
            }
            let mut volume = status.volume;
            ui.spacing_mut().slider_width = 64.0;
            let slider = ui
                .add(egui::Slider::new(&mut volume, 0.0..=100.0).show_value(false))
                .on_hover_text(format!("Volume {volume:.0}%"));
            if slider.changed() {
                player.set_volume(volume);
                if status.muted {
                    player.set_muted(false);
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.monospace(format!(
                    "{} / {}",
                    video::format_time(position),
                    video::format_time(status.duration)
                ));
            });
        },
    );
    (
        status.video_size,
        (status.duration > 0.0).then_some(status.duration),
    )
}

/// Fits `size` into `bounds`, centered.
pub fn fit(size: Vec2, bounds: Rect) -> Rect {
    let scale = (bounds.width() / size.x).min(bounds.height() / size.y);
    Rect::from_center_size(bounds.center(), size * scale)
}
