//! Preview of a single entry: image, video player, text excerpt, or file info.

use std::{
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
    time::SystemTime,
};

use egui::{Align, Align2, Color32, FontId, Layout, Rect, RichText, Sense, Ui, Vec2, pos2, vec2};
use egui_phosphor::regular as icon;

use crate::{
    archive::{self, ListState},
    editor::{
        self,
        highlight::{self, Highlighter, LineSpans, ThemeColors},
    },
    fs::{self, Entry},
    images::{self, ImageCache, Lookup},
    markdown,
    tab::{Action, Shared},
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
    syntax: &'static str,
    colors: Option<Colors>,
    /// Rendered, for a markdown document.
    markdown: Option<Box<markdown::MdView>>,
    /// A server's file, still being read in the background.
    loading: Option<std::sync::mpsc::Receiver<Option<(String, bool)>>>,
}

/// Syntax colors for a preview, for one theme.
struct Colors {
    theme: &'static str,
    /// Until it's done.
    highlighter: Option<Highlighter>,
    spans: Vec<LineSpans>,
}

impl TextPreview {
    /// Highlights the text in the background, and picks up the results.
    fn update_colors(&mut self, ctx: &egui::Context, theme: &'static str) {
        let Some(text) = &self.text else {
            return;
        };
        if self.syntax == highlight::PLAIN_TEXT {
            return;
        }
        if self.colors.as_ref().is_none_or(|c| c.theme != theme) {
            let highlighter = Highlighter::new(ctx, self.syntax, theme);
            highlighter.update(1, Arc::from(text.as_str()));
            self.colors = Some(Colors {
                theme,
                highlighter: Some(highlighter),
                spans: Vec::new(),
            });
        }
        let colors = self.colors.as_mut().expect("just set");
        if let Some(result) = colors.highlighter.as_ref().and_then(Highlighter::poll) {
            colors.spans = result.lines;
            if result.done {
                colors.highlighter = None;
            }
        }
    }

    fn spans(&self) -> &[LineSpans] {
        self.colors.as_ref().map_or(&[], |c| &c.spans)
    }
}

/// The last few text previews, so switching tabs doesn't re-read files every frame.
#[derive(Default)]
pub struct TextCache {
    entries: Vec<TextPreview>,
}

impl TextCache {
    fn get(&mut self, entry: &Entry) -> &mut TextPreview {
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
                // A server's file is read in the background: it may take a while.
                let (read, loading) = if crate::remote::is_remote(&entry.path) {
                    let (send, receive) = std::sync::mpsc::channel();
                    let path = entry.path.clone();
                    crate::background::spawn("preview a server's file", move || {
                        let _ = send.send(read_text(&path));
                        crate::remote::repaint();
                    });
                    (None, Some(receive))
                } else {
                    (read_text(&entry.path), None)
                };
                let (text, truncated) = read.map_or((None, false), |(t, tr)| (Some(t), tr));
                let first_line = text.as_deref().and_then(|t| t.lines().next()).unwrap_or("");
                self.entries.push(TextPreview {
                    path: entry.path.clone(),
                    modified: entry.modified,
                    syntax: highlight::detect(&entry.path, first_line),
                    text,
                    truncated,
                    colors: None,
                    markdown: None,
                    loading,
                });
                self.entries.len() - 1
            }
        };
        let preview = &mut self.entries[index];
        if let Some(read) = preview.loading.as_ref().and_then(|r| r.try_recv().ok()) {
            preview.loading = None;
            if let Some((text, truncated)) = read {
                let first_line = text.lines().next().unwrap_or("");
                preview.syntax = highlight::detect(&preview.path, first_line);
                preview.text = Some(text);
                preview.truncated = truncated;
            }
        }
        preview
    }
}

/// Reads the start of a file as text, or `None` if it looks binary.
fn read_text(path: &Path) -> Option<(String, bool)> {
    let mut bytes = Vec::new();
    disk::File::open(path)
        .ok()?
        .take(TEXT_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    let truncated = bytes.len() as u64 > TEXT_BYTES;
    bytes.truncate(TEXT_BYTES as usize);
    if bytes.contains(&0) {
        return None;
    }
    let text = String::from_utf8_lossy(&bytes).into_owned();
    crate::theme::fonts::want(&text);
    Some((text, truncated))
}

/// The preview pane, showing the selection of the tab files were last browsed in.
pub fn pane(ui: &mut Ui, entry: Option<&Entry>, shared: &mut Shared, actions: &mut Vec<Action>) {
    egui::ScrollArea::vertical()
        .scroll_source(crate::touch::scroll_source(ui.ctx()))
        .id_salt("preview-pane")
        .auto_shrink(false)
        .show(ui, |ui| match entry {
            Some(entry) => {
                // The properties below name and describe the item, so the
                // preview leaves that to them and only adds its buttons.
                let editable = show(ui, entry, shared, images::PREVIEW_SIZE, actions, false);
                ui.add_space(4.0);
                crate::properties::Properties::preview_ui(ui, &entry.path, shared, &mut |ui| {
                    buttons(ui, entry, editable, actions);
                });
            }
            None => {
                ui.add_space(16.0);
                ui.vertical_centered(|ui| ui.weak("Select one item to preview"));
            }
        });
}

/// Shows a preview of `entry`. Images are decoded at most `max_side` pixels.
/// With `with_details`, also its name, metadata and Open/Edit buttons.
/// Returns whether it can be edited as text.
pub fn show(
    ui: &mut Ui,
    entry: &Entry,
    shared: &mut Shared,
    max_side: u32,
    actions: &mut Vec<Action>,
    with_details: bool,
) -> bool {
    let Shared {
        images,
        videos,
        jobs,
        texts,
        archives,
        editor: editor_prefs,
        markdown_images,
        ..
    } = shared;
    let mut dimensions = None;
    let mut duration = None;
    let mut editable = false;
    let is_archive = !entry.is_dir && archive::is_archive(&entry.path);
    ui.vertical_centered(|ui| {
        ui.add_space(8.0);
        if entry.is_dir || is_archive {
            big_icon(ui, entry);
        } else if video::is_video(&entry.path) {
            (dimensions, duration) = video_player(ui, entry, images, videos, jobs);
        } else if images::is_image(&entry.path) {
            dimensions = image(ui, entry, images, max_side);
        } else {
            // Text can be arbitrarily long, so its details and buttons go first
            // where they stay reachable.
            let preview = texts.get(entry);
            editable = preview.text.is_some() && editor::can_edit(&entry.path, false);
            if preview.text.as_ref().is_some_and(|t| !t.trim().is_empty()) {
                if with_details {
                    details(ui, entry, None, None, editable, actions);
                    ui.add_space(8.0);
                }
                let theme = editor_prefs.theme(ui);
                let colors = ThemeColors::of(theme, ui.visuals());
                if markdown::is_markdown(&entry.path) {
                    let view = preview.markdown.get_or_insert_default();
                    view.sync(ui.ctx(), preview.text.as_deref().unwrap_or_default());
                    let dir = markdown::view::base_dir(&entry.path);
                    let options = markdown::Options {
                        dir: &dir,
                        syntax_theme: theme,
                        code_fg: colors.foreground,
                        code_bg: colors.background,
                        allow_remote: editor_prefs.markdown.remote_images
                            == markdown::RemoteImages::Always,
                        editable: false,
                        scroll_to: None,
                        compact: true,
                        hits: None,
                        caret: None,
                    };
                    ui.with_layout(Layout::top_down(Align::Min), |ui| {
                        let id = egui::Id::new(("preview-markdown", &entry.path));
                        let top = ui.next_widget_position();
                        let clicks = view.show_inline(ui, id, &options, markdown_images);
                        for click in clicks {
                            let markdown::Click::Link { link, .. } = click else {
                                continue;
                            };
                            let target = markdown::links::resolve(&link, &dir);
                            // A heading in what's showing: the pane scrolls to it.
                            if let markdown::Target::Anchor(anchor) = &target {
                                if let Some(y) = view.anchor_y(anchor) {
                                    let at = top + egui::vec2(0.0, y);
                                    let rect = egui::Rect::from_min_size(at, egui::vec2(1.0, 1.0));
                                    ui.scroll_to_rect(rect, Some(egui::Align::Min));
                                }
                                continue;
                            }
                            follow_link(ui.ctx(), target, actions);
                        }
                        if preview.truncated {
                            ui.weak("…");
                        }
                    });
                    return;
                }
                preview.update_colors(ui.ctx(), theme);
                let spans = preview.spans();
                let text = preview.text.as_deref().unwrap_or_default();
                egui::Frame::group(ui.style())
                    .fill(colors.background)
                    .show(ui, |ui| {
                        ui.set_min_width(ui.available_width());
                        ui.with_layout(Layout::top_down(Align::Min), |ui| {
                            let job = highlight::layout_job(
                                text,
                                spans,
                                FontId::monospace(11.0),
                                colors.foreground,
                                ui.available_width(),
                            );
                            ui.add(egui::Label::new(job).wrap());
                            if preview.truncated {
                                ui.colored_label(colors.foreground, "…");
                            }
                        });
                    });
            } else {
                big_icon(ui, entry);
                if with_details {
                    ui.add_space(8.0);
                    details(ui, entry, None, None, editable, actions);
                }
            }
            return;
        }
        if with_details {
            ui.add_space(8.0);
            details(ui, entry, dimensions, duration, false, actions);
        }
    });
    if is_archive {
        ui.add_space(8.0);
        archive_contents(ui, &entry.path, archives);
    }
    editable
}

/// Follows a link in a previewed document: another document or file opens as
/// if it was chosen, and a program is shown in its folder rather than run.
fn follow_link(ctx: &egui::Context, target: markdown::Target, actions: &mut Vec<Action>) {
    match target {
        markdown::Target::Web(url) => markdown::links::open_web(ctx, url),
        markdown::Target::Document(path, _) => actions.push(Action::Activate(path)),
        markdown::Target::File(path) => {
            let run =
                disk::metadata(&path).is_ok_and(|meta| markdown::links::is_program(&path, &meta));
            actions.push(if run {
                Action::Reveal(path)
            } else {
                Action::Activate(path)
            });
        }
        markdown::Target::Anchor(_) | markdown::Target::Refused => {}
    }
}

/// The name, metadata and Open/Edit buttons under (or above) a preview.
fn details(
    ui: &mut Ui,
    entry: &Entry,
    dimensions: Option<[u32; 2]>,
    duration: Option<f64>,
    editable: bool,
    actions: &mut Vec<Action>,
) {
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
    crate::dates::label(ui, entry.modified, true);
    ui.add_space(8.0);
    buttons(ui, entry, editable, actions);
}

/// Open, and Edit for text, centered.
pub fn buttons(ui: &mut Ui, entry: &Entry, editable: bool, actions: &mut Vec<Action>) {
    ui.horizontal(|ui| {
        let buttons = if editable { 2.0 } else { 1.0 };
        let width = 70.0 * buttons;
        ui.add_space(((ui.available_width() - width) / 2.0).max(0.0));
        if ui
            .button(format!("{} Open", icon::ARROW_SQUARE_OUT))
            .clicked()
        {
            actions.push(Action::Activate(entry.path.clone()));
        }
        if editable
            && ui
                .button(format!("{} Edit", icon::NOTE_PENCIL))
                .on_hover_text(crate::keymap::hint(
                    ui.ctx(),
                    "Edit in a tab",
                    crate::keymap::Command::Edit,
                ))
                .clicked()
        {
            actions.push(Action::Edit(entry.path.clone()));
        }
    });
}

/// How many top-level entries of an archive the preview lists.
const ARCHIVE_ROWS: usize = 30;

fn archive_contents(ui: &mut Ui, path: &Path, archives: &mut archive::Listings) {
    let listing = match archives.get(path) {
        ListState::Ready(listing) => listing,
        ListState::Loading => {
            ui.vertical_centered(|ui| {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.weak("Reading contents…");
                });
            });
            return;
        }
        ListState::NeedsPassword { .. } => {
            ui.vertical_centered(|ui| {
                ui.weak(format!(
                    "{} Encrypted: open it to enter the password",
                    icon::LOCK
                ));
            });
            return;
        }
        ListState::Failed(error) => {
            ui.vertical_centered(|ui| {
                ui.colored_label(ui.visuals().error_fg_color, error);
            });
            return;
        }
    };
    // Top-level items, with folders the archive only implies.
    let mut top: Vec<(String, bool)> = Vec::new();
    for entry in &listing.entries {
        let (name, nested) = match entry.path.split_once('/') {
            Some((name, _)) => (name, true),
            None => (entry.path.as_str(), false),
        };
        let is_dir = nested || entry.is_dir;
        match top.iter_mut().find(|(n, _)| n == name) {
            Some(existing) => existing.1 |= is_dir,
            None if !name.is_empty() => top.push((name.to_owned(), is_dir)),
            None => {}
        }
    }
    top.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase()))
    });

    let total = listing.total_size();
    egui::Grid::new("archive-info")
        .num_columns(2)
        .show(ui, |ui| {
            let row = |ui: &mut Ui, label: &str, value: String| {
                ui.weak(label);
                ui.label(value);
                ui.end_row();
            };
            row(ui, "Format", listing.format.label().to_owned());
            row(ui, "Files", listing.files().to_string());
            row(ui, "Unpacked", fs::human_size(total));
            if total > 0 {
                row(
                    ui,
                    "Ratio",
                    format!("{:.0}%", listing.archive_size as f64 * 100.0 / total as f64),
                );
            }
            if listing.encrypted() {
                row(ui, "Encrypted", format!("{} Yes", icon::LOCK));
            }
        });
    ui.add_space(6.0);
    ui.strong("Contents");
    for (name, is_dir) in top.iter().take(ARCHIVE_ROWS) {
        let glyph = fs::icon_for(Path::new(name), *is_dir, false);
        ui.add(egui::Label::new(format!("{glyph} {name}")).truncate());
    }
    if top.len() > ARCHIVE_ROWS {
        ui.weak(format!("… and {} more", top.len() - ARCHIVE_ROWS));
    }
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
            let backdrop = crate::theme::colors(ui.ctx()).media.image_backdrop;
            egui::Frame::NONE
                .fill(backdrop.unwrap_or(Color32::TRANSPARENT))
                .show(ui, |ui| {
                    ui.image((image.texture.id(), texture_size * scale))
                });
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
    jobs: &mut crate::jobs::Jobs,
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
            crate::tools::offer(ui, jobs, crate::tools::Tool::Libmpv);
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
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    let video_rect = player.render_for(rect, ui.pixels_per_point());
    let painter = ui.painter_at(rect.union(video_rect));
    let full_uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
    let media = crate::theme::colors(ui.ctx()).media.clone();
    painter.rect_filled(rect, 0.0, media.backdrop);
    if let Some(frame) = player.frame() {
        painter.image(frame.id(), video_rect, full_uv, Color32::WHITE);
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
        painter.circle_filled(rect.center(), 24.0, media.overlay);
        painter.text(
            rect.center() + vec2(2.0, 0.0),
            Align2::CENTER_CENTER,
            icon::PLAY,
            FontId::proportional(24.0),
            media.overlay_icon,
        );
    }
    let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
    if response.clicked() {
        player.set_paused(!status.paused);
    }

    ui.add_space(4.0);
    let mut position = status.position;
    // Sliders lay themselves out left-aligned in the full width; a row of the
    // video's width keeps the scrubber centered under it.
    ui.allocate_ui_with_layout(
        vec2(size.x, ui.spacing().interact_size.y),
        Layout::left_to_right(Align::Center),
        |ui| {
            // A phone's screen edges are the system's back gesture, and dragging
            // from the start of a video would be taken for one.
            let inset = if cfg!(target_os = "android") {
                24.0
            } else {
                0.0
            };
            ui.add_space(inset);
            ui.spacing_mut().slider_width = (size.x - inset * 2.0).max(0.0);
            let slider = egui::Slider::new(&mut position, 0.0..=status.duration.max(0.001))
                .show_value(false);
            if ui.add_enabled(status.duration > 0.0, slider).changed() {
                player.seek(position);
            }
        },
    );
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

#[cfg(test)]
mod tests {
    use egui::Context;

    use super::*;

    /// In a pane wider than the video (its height is the limit), the scrubber
    /// used to stay left-aligned. Needs libmpv, ffmpeg and a GPU.
    #[test]
    #[ignore]
    fn video_controls_stay_centered_under_the_video() {
        let dir = std::env::temp_dir().join(format!("egui-files-scrubber-{}", std::process::id()));
        disk::create_dir_all(&dir).unwrap();
        let status = std::process::Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=s=320x180:d=1",
            ])
            .args(["-pix_fmt", "yuv420p"])
            .arg(dir.join("clip.mp4"))
            .status()
            .unwrap();
        assert!(status.success());
        let entry = fs::read_dir(&dir).unwrap().remove(0);

        let ctx = Context::default();
        let mut shared = Shared {
            show_hidden: false,
            show_thumbnails: true,
            icon_size: 64.0,
            list_scale: 1.0,
            clipboard: None,
            focused_tab: None,
            preview_shown: true,
            app_actions: Vec::new(),
            usage: Default::default(),
            properties: Default::default(),
            images: ImageCache::new(&ctx),
            videos: Videos::new(&ctx),
            texts: TextCache::default(),
            markdown_images: Default::default(),
            jobs: crate::jobs::Jobs::new(&ctx),
            archives: archive::Listings::new(&ctx),
            archive_prefs: Default::default(),
            clip_settings: Default::default(),
            open_with: Default::default(),
            editor: Default::default(),
        };
        let input = || egui::RawInput {
            screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(900.0, 300.0))),
            ..Default::default()
        };
        for _ in 0..3 {
            shared.videos.begin_frame();
            let mut output = ctx.run_ui(input(), |ui| {
                show(ui, &entry, &mut shared, 256, &mut Vec::new(), true);
            });
            output.textures_delta.clear();
        }

        let widgets: Vec<_> = ctx.viewport(|viewport| {
            let widgets = &viewport.prev_pass.widgets;
            widgets
                .layer_ids()
                .flat_map(|layer| widgets.get_layer(layer))
                .map(|widget| (widget.rect, widget.sense))
                .collect()
        });
        let video = widgets
            .iter()
            .find(|(rect, sense)| *sense == Sense::click() && rect.height() > 100.0)
            .expect("video rect")
            .0;
        assert!(
            video.width() < 600.0,
            "the test needs a height-limited video"
        );
        let scrubber = widgets
            .iter()
            .find(|(rect, sense)| sense.senses_drag() && (rect.width() - video.width()).abs() < 2.0)
            .expect("scrubber")
            .0;
        assert!(
            (scrubber.center().x - video.center().x).abs() < 1.0,
            "scrubber {scrubber:?} not under video {video:?}"
        );
        disk::remove_dir_all(dir).unwrap();
    }
}
