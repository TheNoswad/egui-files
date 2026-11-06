//! The tab for trimming a video and choosing how to export it.

use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
};

use egui::{
    Align, Align2, Button, Color32, ComboBox, FontId, Id, Key, Layout, Modal, Rect, RichText,
    ScrollArea, Sense, Stroke, TextEdit, TextStyle, Ui, emath::GuiRounding, pos2, vec2,
};
use egui_phosphor::regular as icon;
use serde::{Deserialize, Serialize};

use super::{
    crop::{self, Crop, Grab, View},
    ffmpeg::{self, AudioChoice, ClipJob, Format, Keep, Quality, Settings, Source, Tools},
};
use crate::{
    archive::FolderPicker,
    fs::{display_name, human_size},
    jobs::JobKind,
    tab::Shared,
    tools::Tool,
    video,
};

/// The shortest range worth cutting, in seconds.
const MIN_LENGTH: f64 = 0.1;
/// How far from a handle, in points, a press still takes it: fingers are wider
/// than pointers.
const GRAB: f32 = if cfg!(target_os = "android") {
    20.0
} else {
    8.0
};
/// How far in from its sides the timeline's ends are. A phone's need to be clear
/// of the screen's edges, where dragging sideways is the system's back gesture.
const TIMELINE_INSET: f32 = if cfg!(target_os = "android") {
    36.0
} else {
    GRAB
};
/// The height of the timeline's bar, and of the ruler under it.
const TRACK_HEIGHT: f32 = 42.0;
const RULER_HEIGHT: f32 = 16.0;
/// Room under the picture for the timeline and the rows under it.
const CONTROLS_HEIGHT: f32 = 142.0;
/// How far from the timeline, in points, a drag goes finely.
const FINE_DISTANCE: f32 = if cfg!(target_os = "android") {
    60.0
} else {
    40.0
};
/// How much of the pointer's movement a fine drag follows.
const FINE_SHARE: f64 = 0.1;
/// The shortest stretch of time the timeline zooms in to, in seconds.
const MIN_VIEW: f64 = 0.5;
/// How far apart, in points, the ruler's labelled ticks are at least.
const TICK_GAP: f32 = 64.0;
/// Steps between the ruler's ticks, in seconds.
const TICK_STEPS: [f64; 20] = [
    0.01, 0.02, 0.05, 0.1, 0.2, 0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0,
    900.0, 1800.0, 3600.0, 7200.0,
];
/// The width of the settings column beside the video.
const SETTINGS_WIDTH: f32 = 320.0;
/// The width of the names in front of each setting, so the settings line up.
const LABEL_WIDTH: f32 = 84.0;
/// Shorter sides to shrink to, largest first.
pub(super) const RESOLUTIONS: [u32; 6] = [2160, 1440, 1080, 720, 480, 360];
const FRAME_RATES: [u32; 5] = [60, 50, 30, 24, 15];
/// Audio bitrates to choose from, in kbit/s.
const AUDIO_BITRATES: [u32; 7] = [64, 96, 128, 160, 192, 256, 320];
/// Target sizes a click away, in MB.
const TARGET_PRESETS: [f64; 5] = [10.0, 25.0, 50.0, 100.0, 500.0];
/// Speeds to choose from.
const SPEEDS: [f64; 8] = [0.25, 0.5, 0.75, 1.0, 1.25, 1.5, 2.0, 4.0];
/// Shapes to keep a crop to, as width and height.
const CROP_SHAPES: [(u32, u32); 5] = [(16, 9), (9, 16), (1, 1), (4, 3), (4, 5)];
/// How far from the crop's edges, in points, a press still takes them.
const CROP_REACH: f32 = if cfg!(target_os = "android") {
    24.0
} else {
    10.0
};
/// How far in from its sides the picture is while cropping. As with the
/// timeline, a phone's crop has to be clear of the screen's edges.
const CROP_INSET: f32 = if cfg!(target_os = "android") {
    36.0
} else {
    0.0
};

type Loaded = Result<(Source, Tools), String>;
/// The start a keyframe was asked for, and when known, the answer.
type KeyframeLookup = (f64, Arc<OnceLock<Result<f64, String>>>);

/// A part of the timeline the pointer can drag.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Handle {
    Start,
    End,
    Playhead,
}

impl Handle {
    /// What a press at `x` takes, the closer handle if it's near both.
    fn at(x: f32, start: f32, end: f32) -> Self {
        let (to_start, to_end) = ((x - start).abs(), (x - end).abs());
        if to_start <= GRAB && to_start <= to_end {
            Self::Start
        } else if to_end <= GRAB {
            Self::End
        } else {
            Self::Playhead
        }
    }
}

/// What's being dragged, given what was: a drag keeps what its press took,
/// however far the pointer has gone from it.
fn grabbed(
    dragging: Option<Handle>,
    pressed: bool,
    x: f32,
    start: f32,
    end: f32,
) -> Option<Handle> {
    if !pressed {
        None
    } else {
        dragging.or(Some(Handle::at(x, start, end)))
    }
}

/// `range` after moving one end to `time`, which can't pass the other.
fn moved(range: (f64, f64), handle: Handle, time: f64, duration: f64) -> (f64, f64) {
    let (start, end) = range;
    match handle {
        Handle::Start => (time.clamp(0.0, (end - MIN_LENGTH).max(0.0)), end),
        Handle::End => (
            start,
            time.clamp(start + MIN_LENGTH, duration.max(MIN_LENGTH)),
        ),
        Handle::Playhead => range,
    }
}

/// Whether playing from `position` should go back to the clip's `start`: it's
/// at the `end`, or would be, within a frame or so.
fn replays(position: f64, start: f64, end: f64) -> bool {
    position >= end - 0.05 && position <= end + 0.05 && start < end
}

/// A time with its milliseconds, as cuts need.
fn precise_time(seconds: f64) -> String {
    let millis = (seconds.max(0.0) * 1000.0).round() as u64;
    format!(
        "{}.{:03}",
        video::format_time((millis / 1000) as f64),
        millis % 1000
    )
}

/// A typed time: seconds, minutes and seconds, or hours, minutes and seconds,
/// the seconds with a fraction if wanted.
fn parse_time(text: &str) -> Option<f64> {
    let text = text.trim().replace(',', ".");
    let parts: Vec<&str> = text.split(':').collect();
    if text.is_empty() || parts.len() > 3 {
        return None;
    }
    let mut total = 0.0;
    for (i, part) in parts.iter().enumerate() {
        let last = i + 1 == parts.len();
        // Only the seconds have a fraction.
        let digits = !part.is_empty() && part.chars().all(|c| c.is_ascii_digit());
        let fraction = part.chars().all(|c| c.is_ascii_digit() || c == '.')
            && part.chars().any(|c| c.is_ascii_digit());
        if !(digits || (last && fraction)) {
            return None;
        }
        let value: f64 = part.parse().ok()?;
        if i > 0 && value >= 60.0 {
            return None;
        }
        total = total * 60.0 + value;
    }
    Some(total)
}

/// The time `frames` frames on from `position`, on the video's frame grid.
fn stepped(position: f64, frames: i64, fps: f64, duration: f64) -> f64 {
    let fps = if fps > 0.0 { fps } else { 30.0 };
    let frame = (position * fps).round() + frames as f64;
    (frame / fps).clamp(0.0, duration)
}

/// The step between the ruler's labelled ticks, for `span` seconds across
/// `width` points.
fn tick_step(span: f64, width: f32) -> f64 {
    let wanted = span / f64::from(width.max(1.0)) * f64::from(TICK_GAP);
    TICK_STEPS
        .into_iter()
        .find(|step| *step >= wanted)
        .unwrap_or_else(|| (wanted / 3600.0).ceil() * 3600.0)
}

/// A tick's time, as precise as ticks `step` apart need.
fn tick_label(seconds: f64, step: f64) -> String {
    if step >= 1.0 {
        return video::format_time(seconds.round());
    }
    let digits = if step >= 0.1 { 1 } else { 2 };
    let scale = 10_u64.pow(digits);
    let units = (seconds.max(0.0) * scale as f64).round() as u64;
    format!(
        "{}.{:0width$}",
        video::format_time((units / scale) as f64),
        units % scale,
        width = digits as usize
    )
}

/// `view` moved `by` seconds, within the video.
fn panned(view: (f64, f64), by: f64, duration: f64) -> (f64, f64) {
    let span = (view.1 - view.0).min(duration);
    let start = (view.0 + by).clamp(0.0, (duration - span).max(0.0));
    (start, start + span)
}

/// `view` zoomed in `factor` times (less than 1 zooms out), keeping `at`
/// where it is.
fn zoomed(view: (f64, f64), factor: f64, at: f64, duration: f64) -> (f64, f64) {
    let span = view.1 - view.0;
    let new_span = (span / factor).clamp(MIN_VIEW.min(duration), duration);
    let share = ((at - view.0) / span.max(f64::EPSILON)).clamp(0.0, 1.0);
    let start = at - new_span * share;
    panned((start, start + new_span), 0.0, duration)
}

/// A view of the clip from `start` to `end`, with a little either side.
fn around(start: f64, end: f64, duration: f64) -> (f64, f64) {
    let margin = (end - start) * 0.1;
    let span = (end - start + margin * 2.0).max(MIN_VIEW.min(duration));
    let middle = (start + end) / 2.0;
    panned((middle - span / 2.0, middle + span / 2.0), 0.0, duration)
}

/// The largest value in `low..=high` that `works`, for something that works
/// up to some point and not past it; `None` if not even `low` does.
fn largest(low: f64, high: f64, works: impl Fn(f64) -> bool) -> Option<f64> {
    if !works(low) {
        return None;
    }
    if works(high) {
        return Some(high);
    }
    let (mut low, mut high) = (low, high);
    for _ in 0..48 {
        let middle = (low + high) / 2.0;
        if works(middle) {
            low = middle;
        } else {
            high = middle;
        }
    }
    Some(low)
}

/// The smallest value in `low..=high` that `works`, for something that
/// works from some point on; `None` if not even `high` does.
fn smallest(low: f64, high: f64, works: impl Fn(f64) -> bool) -> Option<f64> {
    if !works(high) {
        return None;
    }
    if works(low) {
        return Some(low);
    }
    let (mut low, mut high) = (low, high);
    for _ in 0..48 {
        let middle = (low + high) / 2.0;
        if works(middle) {
            high = middle;
        } else {
            low = middle;
        }
    }
    Some(high)
}

/// A frame drawn in `rect`, turned and mirrored as `view` says.
fn picture_mesh(texture: egui::TextureId, rect: Rect, view: View) -> egui::Shape {
    let mut mesh = egui::Mesh::with_texture(texture);
    for (corner, on_view) in [
        (rect.left_top(), (0.0, 0.0)),
        (rect.right_top(), (1.0, 0.0)),
        (rect.right_bottom(), (1.0, 1.0)),
        (rect.left_bottom(), (0.0, 1.0)),
    ] {
        let (u, v) = view.to_source(on_view);
        mesh.vertices.push(egui::epaint::Vertex {
            pos: corner,
            uv: pos2(u, v),
            color: Color32::WHITE,
        });
    }
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(0, 2, 3);
    egui::Shape::mesh(mesh)
}

/// A heading over a group of settings.
fn section(ui: &mut Ui, title: &str) {
    ui.add_space(16.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new(title).heading().strong());
        ui.add(egui::Separator::default().horizontal());
    });
    ui.add_space(4.0);
}

/// Whether there's too little room for settings' names in front of them, so
/// they go above instead.
fn cramped(ui: &Ui) -> bool {
    ui.available_width() < LABEL_WIDTH + 190.0
}

/// The width for a dropdown that would like `wanted`, within what there is.
fn combo_width(ui: &Ui, wanted: f32) -> f32 {
    wanted.min(ui.available_width() - 8.0).max(60.0)
}

/// A setting with its name in front, lined up with the others, or above it
/// where there's no room for that.
fn row<R>(ui: &mut Ui, name: &str, add: impl FnOnce(&mut Ui) -> R) -> R {
    if cramped(ui) {
        ui.label(name);
        return ui.horizontal_wrapped(add).inner;
    }
    ui.horizontal(|ui| {
        let height = ui.spacing().interact_size.y;
        let (rect, _) = ui.allocate_exact_size(vec2(LABEL_WIDTH, height), Sense::hover());
        ui.painter().text(
            rect.left_center(),
            Align2::LEFT_CENTER,
            name,
            TextStyle::Body.resolve(ui.style()),
            ui.visuals().text_color(),
        );
        add(ui)
    })
    .inner
}

/// Something under the settings, lined up with them rather than their names.
fn indented<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    if cramped(ui) {
        return add(ui);
    }
    let left = (LABEL_WIDTH + ui.spacing().item_spacing.x) as i8;
    egui::Frame::NONE
        .inner_margin(egui::Margin {
            left,
            ..egui::Margin::ZERO
        })
        .show(ui, add)
        .inner
}

/// A remark about the setting above.
fn note(ui: &mut Ui, text: impl Into<RichText>) {
    indented(ui, |ui| ui.add(egui::Label::new(text.into()).wrap()));
}

/// A remark in the weak colour.
fn weak_note(ui: &mut Ui, text: impl Into<String>) {
    note(ui, RichText::new(text.into()).weak());
}

/// A setting that's stopping the export.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reveal {
    Format,
    Size,
}

/// A change that makes a target size work.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Fix {
    Picture(ffmpeg::Suggestion),
    /// A larger target, in MB.
    Target(f64),
    /// A shorter clip, in seconds.
    Length(f64),
    NoAudio,
}

/// What the keyboard asked of the player this frame.
#[derive(Default)]
struct Keys {
    play: bool,
    mark_start: bool,
    mark_end: bool,
    frames: i64,
    seconds: f64,
    to_start: bool,
    to_end: bool,
}

/// What the tab worked out once, or is working out.
#[derive(Default)]
struct Runtime {
    loaded: Option<Arc<OnceLock<Loaded>>>,
    /// [`crate::tools::generation`] when it was asked.
    loaded_generation: u64,
    keyframe: Option<KeyframeLookup>,
    drag: Option<Handle>,
    /// A drag of the crop: what it took, the crop then, and where it began.
    crop_drag: Option<(Grab, Crop, (f32, f32))>,
    /// Where playing was last frame, to stop on passing the range's end.
    last_position: f64,
    /// Where a drag on the timeline has got to, and the pointer with it.
    drag_time: Option<f64>,
    drag_x: f32,
    /// What the drag last showed, to land on exactly when it ends.
    drag_shown: Option<f64>,
    /// The drag is going slowly, for precision.
    fine: bool,
    /// Why Export can't go yet, after it was pressed, until when to say so.
    export_note: Option<(String, f64)>,
    /// A setting to point out after Export was pressed: which, until when,
    /// and whether it's been scrolled to.
    reveal: Option<(Reveal, f64, bool)>,
    /// The stretch of time the timeline shows, when it's not all of it.
    zoom: Option<(f64, f64)>,
    /// A time being typed, and which end of the clip it's for.
    typing: Option<(Handle, String)>,
    /// How tall the bar with the Export button was last frame.
    footer_height: f32,
    /// Stills along the timeline.
    strip: Option<super::strip::Strip>,
    /// How wide the player's buttons were last frame, to centre them.
    transport_width: f32,
    name: Option<String>,
    picker: Option<FolderPicker>,
    choosing_folder: bool,
    dir: Option<PathBuf>,
}

#[derive(Serialize, Deserialize)]
pub struct ClipTab {
    pub id: u64,
    /// The video; also marks saved state as an editor of one.
    pub clip: PathBuf,
    #[serde(default)]
    start: f64,
    /// `None` until the video's length is known.
    #[serde(default)]
    end: Option<f64>,
    #[serde(default)]
    settings: Settings,
    #[serde(skip)]
    runtime: Runtime,
}

impl ClipTab {
    /// A new editor starts from the defaults, whatever was done with the last.
    pub fn new(id: u64, clip: PathBuf) -> Self {
        Self {
            id,
            clip,
            start: 0.0,
            end: None,
            settings: Settings::default(),
            runtime: Runtime::default(),
        }
    }

    pub fn title(&self) -> String {
        format!("{} {}", icon::FILM_SLATE, display_name(&self.clip))
    }

    /// What's known of the video, starting to find out if nothing's asked yet.
    fn loaded(&mut self, ctx: &egui::Context) -> Option<Loaded> {
        // It may have failed for want of what's since been installed.
        let generation = crate::tools::generation();
        if generation != self.runtime.loaded_generation {
            self.runtime.loaded_generation = generation;
            if self
                .runtime
                .loaded
                .as_ref()
                .and_then(|cell| cell.get())
                .is_some_and(Result::is_err)
            {
                self.runtime.loaded = None;
            }
        }
        let cell = self.runtime.loaded.get_or_insert_with(|| {
            let cell = Arc::new(OnceLock::new());
            let (result, ctx, path) = (cell.clone(), ctx.clone(), self.clip.clone());
            crate::background::spawn("clip-probe", move || {
                let loaded = ffmpeg::tools().and_then(|tools| Ok((ffmpeg::probe(&path)?, tools)));
                let _ = result.set(loaded);
                ctx.request_repaint();
            });
            cell
        });
        cell.get().cloned()
    }

    pub fn ui(&mut self, ui: &mut Ui, shared: &mut Shared) {
        // A minimized window still runs frames, with no room at all.
        if ui.available_width() < 1.0 || ui.available_height() < 1.0 {
            return;
        }
        ui.style_mut().interaction.selectable_labels = false;
        let (source, tools) = match self.loaded(ui.ctx()) {
            None => {
                ui.centered_and_justified(|ui| ui.spinner());
                return;
            }
            Some(Err(err)) => {
                let missing = err == ffmpeg::NOT_INSTALLED;
                ui.vertical_centered(|ui| {
                    ui.add_space(ui.available_height() / 3.0);
                    ui.colored_label(ui.visuals().warn_fg_color, err);
                    if missing {
                        ui.add_space(8.0);
                        crate::tools::offer(ui, &mut shared.jobs, Tool::Ffmpeg);
                    }
                    ui.add_space(4.0);
                    if ui
                        .button(format!("{} Try again", icon::ARROW_CLOCKWISE))
                        .clicked()
                    {
                        self.runtime.loaded = None;
                    }
                });
                return;
            }
            Some(Ok(loaded)) => loaded,
        };
        // Remembered from another video, which may have had what this hasn't.
        if !tools.can_make(self.settings.format, &source) {
            self.settings.format = Format::Original;
        }
        let end = self.end.unwrap_or(source.duration).min(source.duration);
        self.end = Some(end);
        self.start = self.start.clamp(0.0, (end - MIN_LENGTH).max(0.0));

        let width = ui.available_width();
        let height = ui.available_height();
        // The Export bar stays in sight under whatever scrolls.
        let above_footer = (height - self.runtime.footer_height).max(80.0);
        if width > SETTINGS_WIDTH * 2.5 {
            ui.horizontal_top(|ui| {
                let video = width - SETTINGS_WIDTH - ui.spacing().item_spacing.x - 8.0;
                let down = Layout::top_down(Align::Min);
                ui.allocate_ui_with_layout(vec2(video, height), down, |ui| {
                    ui.set_width(video);
                    self.player(ui, shared, &source, height);
                });
                ui.separator();
                ui.vertical(|ui| {
                    ScrollArea::vertical()
                        .id_salt(("clip-settings", self.id))
                        .max_height(above_footer)
                        .auto_shrink([false, false])
                        .show(ui, |ui| self.settings(ui, &source, &tools));
                    self.footer(ui, shared, &source, &tools);
                });
            });
        } else {
            // Each tab's own, or a new one opens where the last was scrolled to.
            ScrollArea::vertical()
                .id_salt(("clip-all", self.id))
                .max_height(above_footer)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    self.player(ui, shared, &source, (height * 0.6).max(240.0));
                    self.settings(ui, &source, &tools);
                });
            self.footer(ui, shared, &source, &tools);
        }
        self.choose_folder(ui.ctx(), shared.show_hidden);
    }

    /// The video, its timeline and its controls, `height` tall at most.
    fn player(&mut self, ui: &mut Ui, shared: &mut Shared, source: &Source, height: f32) {
        let id = Id::new(("clip-video", self.id));
        let width = ui.available_width();
        let focused = shared.focused_tab == Some(self.id);
        let exact = shared.videos.exact_scrubbing();
        let player = match shared.videos.player(id, &self.clip, video::Mode::Preview) {
            Ok(player) => player,
            Err(err) => {
                ui.colored_label(
                    ui.visuals().warn_fg_color,
                    format!("Can't play videos: {err}"),
                );
                crate::tools::offer(ui, &mut shared.jobs, Tool::Libmpv);
                return;
            }
        };
        let status = player.status();

        // Its shape is known from the probe before the player has a frame.
        let source_size = status
            .video_size
            .unwrap_or([source.width(), source.height()]);
        let source_size = if source_size[0] == 0 || source_size[1] == 0 {
            vec2(16.0, 9.0)
        } else {
            vec2(source_size[0] as f32, source_size[1] as f32)
        };
        let view = self.settings.view();
        let view_size = if view.swaps_sides() {
            vec2(source_size.y, source_size.x)
        } else {
            source_size
        };
        let cropping = self.settings.crop.is_some() && !self.settings.format.is_audio_only();
        let inset = if cropping { CROP_INSET } else { 0.0 };
        // As big as fits, and no taller than the picture, so the timeline is
        // right under it.
        let most = (height - CONTROLS_HEIGHT).max(120.0);
        let fit = ((width - inset * 2.0).max(1.0) / view_size.x).min(most / view_size.y);
        let room = vec2(width, (view_size.y * fit).max(60.0).round());
        let scale = ((room.x - inset * 2.0).max(1.0) / view_size.x).min(room.y / view_size.y);
        let sense = if cropping {
            Sense::click_and_drag()
        } else {
            Sense::click()
        };
        let (area, response) = ui.allocate_exact_size(room, sense);
        // mpv renders it upright, at the size it'll be shown; turning it is
        // only a matter of which corner of the frame goes where.
        let rect = Rect::from_center_size(area.center(), source_size * scale);
        let pixels_per_point = ui.pixels_per_point();
        let rendered = player.render_for(rect, pixels_per_point);
        let shown = if view.swaps_sides() {
            let size = vec2(rendered.height(), rendered.width());
            Rect::from_center_size(rendered.center(), size).round_to_pixels(pixels_per_point)
        } else {
            rendered
        };
        player.set_speed(self.settings.speed);
        let media = crate::theme::colors(ui.ctx()).media.clone();
        let painter = ui.painter_at(area);
        painter.rect_filled(area, 0.0, media.backdrop);
        if let Some(frame) = player.frame() {
            painter.add(picture_mesh(frame.id(), shown, view));
        }
        if let Some(err) = &status.error {
            painter.text(
                area.center(),
                Align2::CENTER_CENTER,
                err,
                FontId::proportional(13.0),
                ui.visuals().error_fg_color,
            );
        }
        if cropping {
            self.crop_overlay(ui, &response, area, shown, source);
        }
        // While cropping, the picture is for that.
        let clicked_video = response.clicked() && !cropping;

        // Stops at the end of the range when playing reaches it, but lets it
        // play on from past there.
        let end = self.end.unwrap_or(source.duration);
        if !status.paused && self.runtime.last_position < end && status.position >= end {
            player.set_paused(true);
            player.seek(end);
        }
        self.runtime.last_position = status.position;

        let keys = if focused
            && !ui.ctx().text_edit_focused()
            && ui.ctx().memory(|m| m.top_modal_layer().is_none())
        {
            // Space would also press a focused button, and arrows move in a
            // focused list.
            let free = ui.ctx().memory(|m| m.focused().is_none());
            ui.input(|i| {
                let steps = i64::from(i.key_pressed(Key::ArrowRight))
                    - i64::from(i.key_pressed(Key::ArrowLeft));
                let (frames, seconds) = if !free {
                    (0, 0.0)
                } else if i.modifiers.shift {
                    (0, steps as f64)
                } else {
                    (steps, 0.0)
                };
                Keys {
                    play: free && i.key_pressed(Key::Space),
                    mark_start: i.key_pressed(Key::I),
                    mark_end: i.key_pressed(Key::O),
                    frames,
                    seconds,
                    to_start: free && i.key_pressed(Key::Home),
                    to_end: free && i.key_pressed(Key::End),
                }
            })
        } else {
            Keys::default()
        };

        ui.add_space(4.0);
        self.timeline(ui, player, &status, source, exact);
        self.transport(ui, player, &status, source, &keys, clicked_video);
        self.trim_row(ui, player, &status, source, &keys);
    }

    /// The bar to choose the clip on, with a ruler of times under it.
    fn timeline(
        &mut self,
        ui: &mut Ui,
        player: &video::Player,
        status: &video::Status,
        source: &Source,
        exact: bool,
    ) {
        let width = ui.available_width();
        let duration = source.duration.max(MIN_LENGTH);
        let (rect, timeline) = ui.allocate_exact_size(
            vec2(width, TRACK_HEIGHT + RULER_HEIGHT),
            Sense::click_and_drag(),
        );
        let track = Rect::from_min_size(rect.min, vec2(width, TRACK_HEIGHT));
        let inner = rect.x_range().shrink(TIMELINE_INSET);
        let time_at = |x: f32, view: (f64, f64)| {
            view.0 + f64::from((x - inner.min) / inner.span().max(1.0)) * (view.1 - view.0)
        };

        // Ctrl and the wheel, or a pinch, zoom; scrolling sideways pans.
        let mut view = self.runtime.zoom.unwrap_or((0.0, duration));
        let mut factor = 1.0_f32;
        if crate::touch::zoom_gesture(ui, rect, &mut factor, 0.01..=100.0, 1.0) {
            let at = ui
                .input(|i| i.pointer.latest_pos())
                .map_or((view.0 + view.1) / 2.0, |pos| time_at(pos.x, view));
            view = zoomed(view, f64::from(factor), at, duration);
        }
        if timeline.hovered() {
            let sideways = ui.input(|i| i.smooth_scroll_delta.x);
            if sideways != 0.0 {
                let by = -f64::from(sideways / inner.span().max(1.0)) * (view.1 - view.0);
                view = panned(view, by, duration);
            }
        }
        // Keeps the playhead in sight while it plays.
        if !status.paused
            && self.runtime.drag.is_none()
            && (status.position > view.1 || status.position < view.0)
        {
            let span = view.1 - view.0;
            view = panned((status.position, status.position + span), 0.0, duration);
        }
        self.runtime.zoom = (view.1 - view.0 < duration - 1e-6).then_some(view);

        let span = (view.1 - view.0).max(f64::EPSILON);
        let x_of = |time: f64| inner.min + inner.span() * ((time - view.0) / span) as f32;
        let end = self.end.unwrap_or(source.duration);
        let (start_x, end_x) = (x_of(self.start), x_of(end));

        let pressed = timeline.is_pointer_button_down_on() || timeline.clicked();
        let was = self.runtime.drag;
        let x = timeline.interact_pointer_pos().map_or(start_x, |pos| pos.x);
        self.runtime.drag = grabbed(was, pressed, x, start_x, end_x);
        if let Some(handle) = self.runtime.drag
            && let Some(pos) = timeline.interact_pointer_pos()
        {
            // Shift, or the pointer well away from the bar, slows it down.
            let away = (pos.y - rect.center().y).abs() > rect.height() / 2.0 + FINE_DISTANCE;
            let fine = away || ui.input(|i| i.modifiers.shift);
            let time = match self.runtime.drag_time {
                Some(time) if was.is_some() && fine => {
                    let moved = f64::from(pos.x - self.runtime.drag_x);
                    time + moved * span / f64::from(inner.span().max(1.0)) * FINE_SHARE
                }
                _ => time_at(pos.x, view),
            }
            .clamp(0.0, source.duration);
            self.runtime.drag_x = pos.x;
            self.runtime.fine = fine;
            self.runtime.drag_time = Some(time);
            let (start, end) = moved((self.start, end), handle, time, source.duration);
            self.start = start;
            self.end = Some(end);
            let shown = match handle {
                Handle::Start => start,
                Handle::End => end,
                Handle::Playhead => time,
            };
            if pressed && self.runtime.drag_shown != Some(shown) {
                player.seek_to(shown, exact);
            }
            self.runtime.drag_shown = Some(shown);
        } else if self.runtime.drag.is_none() {
            // Let go: it lands on the very frame, if it was only near.
            if let Some(shown) = self.runtime.drag_shown.take()
                && !exact
            {
                player.seek(shown);
            }
            self.runtime.drag_time = None;
        }
        // Where a copy would really start, if not where it's asked to.
        let keyframe = if self.settings.copies(source) {
            self.keyframe(ui, source).and_then(Result::ok)
        } else {
            None
        };
        let end = self.end.unwrap_or(end);
        let (start_x, end_x) = (x_of(self.start), x_of(end));
        let hovered = timeline
            .hover_pos()
            .map(|pos| Handle::at(pos.x, start_x, end_x))
            .filter(|handle| *handle != Handle::Playhead);
        let held = self.runtime.drag.filter(|h| *h != Handle::Playhead);

        let visuals = ui.visuals().clone();
        let painter = ui.painter_at(rect);
        let bar = Rect::from_x_y_ranges(inner, track.y_range().shrink(3.0));
        painter.rect_filled(bar, 4.0, visuals.extreme_bg_color);
        let (id, clip) = (self.id, &self.clip);
        self.runtime
            .strip
            .get_or_insert_with(|| super::strip::Strip::new(id, clip.clone(), source.clone()))
            .show(ui.ctx(), &painter, bar, view);
        let accent = visuals.selection.bg_fill;
        let chosen = Rect::from_x_y_ranges(
            start_x.max(bar.left())..=end_x.min(bar.right()),
            bar.y_range(),
        );
        // What's left out is dimmed.
        let shade = Color32::from_black_alpha(170);
        let before = Rect::from_x_y_ranges(bar.left()..=start_x.min(bar.right()), bar.y_range());
        let after = Rect::from_x_y_ranges(end_x.max(bar.left())..=bar.right(), bar.y_range());
        for outside in [before, after] {
            if outside.width() > 0.0 {
                painter.rect_filled(outside, 0.0, shade);
            }
        }
        if chosen.width() > 0.0 {
            painter.rect_filled(chosen, 0.0, accent.gamma_multiply(0.12));
            let edge = Stroke::new(2.0, accent);
            painter.line_segment([chosen.left_top(), chosen.right_top()], edge);
            painter.line_segment([chosen.left_bottom(), chosen.right_bottom()], edge);
        }
        if let Some(keyframe) = keyframe.filter(|k| *k < self.start - 0.001) {
            let warn = visuals.warn_fg_color;
            let x = x_of(keyframe);
            let extra = Rect::from_x_y_ranges(x..=start_x, bar.y_range());
            painter.rect_filled(extra, 0.0, warn.gamma_multiply(0.3));
            painter.line_segment(
                [pos2(x, bar.top()), pos2(x, bar.bottom())],
                Stroke::new(1.5, warn),
            );
        }
        let grip = visuals.selection.stroke.color;
        for (x, which) in [(start_x, Handle::Start), (end_x, Handle::End)] {
            let wide = if held.or(hovered) == Some(which) {
                10.0
            } else {
                8.0
            };
            let handle =
                Rect::from_center_size(pos2(x, bar.center().y), vec2(wide, bar.height() + 6.0));
            painter.rect_filled(handle, 3.0, accent);
            let middle = handle.center();
            painter.line_segment(
                [middle - vec2(0.0, 5.0), middle + vec2(0.0, 5.0)],
                Stroke::new(1.5, grip),
            );
        }

        // The ends' own times, pointing out from the clip, then the ruler's
        // where there's room.
        let ruler_top = track.bottom() + 1.0;
        let small = FontId::proportional(11.0);
        let strong = visuals.strong_text_color();
        let weak = visuals.weak_text_color();
        let start_label = painter.layout_no_wrap(precise_time(self.start), small.clone(), strong);
        let end_label = painter.layout_no_wrap(precise_time(end), small.clone(), strong);
        let start_left = (start_x - start_label.size().x - 3.0).max(rect.left());
        let start_rect = Rect::from_min_size(pos2(start_left, ruler_top), start_label.size());
        let end_left = (end_x + 3.0).min(rect.right() - end_label.size().x);
        let end_rect = Rect::from_min_size(pos2(end_left, ruler_top), end_label.size());
        let step = tick_step(span, inner.span());
        let first = (view.0 / step).ceil() as i64;
        let last = (view.1 / step).floor() as i64;
        for tick in first..=last.min(first + 500) {
            let time = tick as f64 * step;
            let x = x_of(time);
            painter.line_segment(
                [pos2(x, track.bottom() - 3.0), pos2(x, bar.bottom())],
                Stroke::new(1.0, weak.gamma_multiply(0.6)),
            );
            let label = painter.layout_no_wrap(tick_label(time, step), small.clone(), weak);
            // Kept inside at the ends, rather than left out.
            let left = (x - label.size().x / 2.0).clamp(
                rect.left(),
                (rect.right() - label.size().x).max(rect.left()),
            );
            let at = Rect::from_min_size(pos2(left, ruler_top), label.size());
            let clear = |other: Rect| !at.expand(4.0).intersects(other);
            if clear(start_rect) && clear(end_rect) {
                painter.galley(at.min, label, weak);
            }
        }
        painter.galley(start_rect.min, start_label, strong);
        painter.galley(end_rect.min, end_label, strong);

        // Where the drag is, which the player may not have caught up with.
        let playhead = x_of(self.runtime.drag_shown.unwrap_or(status.position));
        if rect.x_range().contains(playhead) {
            painter.line_segment(
                [
                    pos2(playhead, track.top()),
                    pos2(playhead, bar.bottom() + 2.0),
                ],
                Stroke::new(2.0, strong),
            );
            painter.add(egui::Shape::convex_polygon(
                vec![
                    pos2(playhead - 5.0, track.top()),
                    pos2(playhead + 5.0, track.top()),
                    pos2(playhead, track.top() + 6.0),
                ],
                strong,
                Stroke::NONE,
            ));
        }
        if self.runtime.drag.is_some() {
            let hint = if self.runtime.fine {
                "Fine: a tenth of the speed"
            } else {
                "Hold Shift, or drag away from the bar, to go finer"
            };
            let galley = painter.layout_no_wrap(hint.to_owned(), small.clone(), Color32::WHITE);
            let at = Rect::from_center_size(
                pos2(rect.center().x, track.top() - galley.size().y / 2.0 - 8.0),
                galley.size(),
            );
            // Over the bottom of the picture, outside the timeline.
            let over = ui.painter();
            over.rect_filled(
                at.expand2(vec2(6.0, 3.0)),
                4.0,
                Color32::from_black_alpha(190),
            );
            over.galley(at.min, galley, Color32::WHITE);
        }
        let cursor = if held.or(hovered).is_some() {
            egui::CursorIcon::ResizeHorizontal
        } else {
            egui::CursorIcon::PointingHand
        };
        timeline.on_hover_cursor(cursor);
    }

    /// Play, step and jump, the zoom, and where it's at.
    fn transport(
        &mut self,
        ui: &mut Ui,
        player: &video::Player,
        status: &video::Status,
        source: &Source,
        keys: &Keys,
        clicked_video: bool,
    ) {
        let end = self.end.unwrap_or(source.duration);
        let time = format!(
            "{} / {}",
            precise_time(status.position),
            video::format_time(source.duration)
        );
        // Too narrow for the time beside the centred buttons, it goes under.
        let font = egui::TextStyle::Monospace.resolve(ui.style());
        let time_width = ui
            .painter()
            .layout_no_wrap(time.clone(), font, Color32::WHITE)
            .size()
            .x;
        let compact = ui.available_width()
            < 2.0 * (time_width + 12.0) + self.runtime.transport_width.max(150.0);
        let height = ui.spacing().interact_size.y;
        let (row, _) = ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::hover());
        let child = |ui: &mut Ui, rect: Rect, layout: Layout| {
            ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(layout))
        };
        // The time on the left, the buttons in the middle, zooming on the right.
        if !compact {
            child(ui, row, Layout::left_to_right(Align::Center)).monospace(&time);
        }
        let mut zoom_ui = child(ui, row, Layout::right_to_left(Align::Center));
        let zoomed = self.runtime.zoom.is_some();
        let (zoom_icon, zoom_hover) = if zoomed {
            (icon::MAGNIFYING_GLASS_MINUS, "Show the whole video")
        } else {
            (
                icon::MAGNIFYING_GLASS_PLUS,
                "Zoom in on the clip (or Ctrl and the wheel over the timeline)",
            )
        };
        if zoom_ui
            .selectable_label(zoomed, zoom_icon)
            .on_hover_text(zoom_hover)
            .clicked()
        {
            self.runtime.zoom = if zoomed {
                None
            } else {
                Some(around(self.start, end, source.duration.max(MIN_LENGTH)))
            };
        }
        // Centred by their width last frame.
        let middle = Rect::from_center_size(
            row.center(),
            vec2(self.runtime.transport_width.max(1.0), height),
        );
        let mut buttons = child(ui, middle, Layout::left_to_right(Align::Center));
        {
            let ui = &mut buttons;
            let to_start = ui
                .button(icon::SKIP_BACK)
                .on_hover_text("Go to the clip's start (Home)")
                .clicked()
                || keys.to_start;
            let back = ui
                .button(icon::CARET_LEFT)
                .on_hover_text("Back a frame (←, or a second with Shift)")
                .clicked();
            let play = if status.paused {
                icon::PLAY
            } else {
                icon::PAUSE
            };
            let toggle = ui
                .button(play)
                .on_hover_text(if status.paused {
                    "Play (Space)"
                } else {
                    "Pause (Space)"
                })
                .clicked()
                || keys.play
                || clicked_video;
            let forward = ui
                .button(icon::CARET_RIGHT)
                .on_hover_text("On a frame (→, or a second with Shift)")
                .clicked();
            let to_end = ui
                .button(icon::SKIP_FORWARD)
                .on_hover_text("Go to the clip's end (End)")
                .clicked()
                || keys.to_end;
            if toggle {
                // At the end of the clip, playing means seeing it again.
                if status.paused && replays(status.position, self.start, end) {
                    player.seek(self.start);
                    self.runtime.last_position = self.start;
                }
                player.set_paused(!status.paused);
            }
            let frames = keys.frames + i64::from(forward) - i64::from(back);
            let target = if to_start {
                Some(self.start)
            } else if to_end {
                Some(end)
            } else if frames != 0 {
                Some(stepped(
                    status.position,
                    frames,
                    source.fps(),
                    source.duration,
                ))
            } else if keys.seconds != 0.0 {
                Some((status.position + keys.seconds).clamp(0.0, source.duration))
            } else {
                None
            };
            if let Some(target) = target {
                // Stepping is for looking at frames, so it stops.
                if !status.paused && !to_start && !to_end {
                    player.set_paused(true);
                }
                player.seek(target);
                self.runtime.last_position = target;
            }
        }
        let used = buttons.min_rect().width();
        if (used - self.runtime.transport_width).abs() > 0.5 {
            self.runtime.transport_width = used;
            ui.ctx().request_discard("centring the player's buttons");
        }
        if compact {
            ui.vertical_centered(|ui| ui.monospace(time));
        }
    }

    /// The clip's ends: set at the playhead, or typed.
    fn trim_row(
        &mut self,
        ui: &mut Ui,
        player: &video::Player,
        status: &video::Status,
        source: &Source,
        keys: &Keys,
    ) {
        let end = self.end.unwrap_or(source.duration);
        // Side by side where there's room, else one above the other, so each
        // button stays with its time.
        let stacked = ui.available_width() < 360.0;
        let body = |ui: &mut Ui| {
            let (set_start, typed_start) = ui
                .horizontal(|ui| {
                    let set = ui
                        .button(format!("{} Start here", icon::BRACKETS_SQUARE))
                        .on_hover_text("Start the clip at the playhead (I)")
                        .clicked();
                    (
                        set || keys.mark_start,
                        self.time_field(ui, Handle::Start, self.start),
                    )
                })
                .inner;
            if !stacked {
                ui.add_space(8.0);
            }
            let (set_end, typed_end) = ui
                .horizontal(|ui| {
                    let set = ui
                        .button(format!("{} End here", icon::BRACKETS_SQUARE))
                        .on_hover_text("End the clip at the playhead (O)")
                        .clicked();
                    (set || keys.mark_end, self.time_field(ui, Handle::End, end))
                })
                .inner;

            let duration = source.duration;
            if let Some(time) = typed_start.or(set_start.then_some(status.position)) {
                self.start = moved((self.start, end), Handle::Start, time, duration).0;
                if typed_start.is_some() {
                    player.seek(self.start);
                }
            }
            let end = self.end.unwrap_or(end);
            if let Some(time) = typed_end.or(set_end.then_some(status.position)) {
                let end = moved((self.start, end), Handle::End, time, duration).1;
                self.end = Some(end);
                if typed_end.is_some() {
                    player.seek(end);
                }
            }
        };
        if stacked {
            ui.vertical(body);
        } else {
            ui.horizontal(body);
        }
        let end = self.end.unwrap_or(end);
        let mut length = format!("Keeps {}", precise_time(end - self.start));
        if self.settings.changes_speed() {
            let plays = precise_time((end - self.start) / self.settings.speed);
            length.push_str(&format!(", which plays for {plays}"));
        }
        // On the same line as the length, so its turning up later doesn't
        // move the settings under the pointer.
        let warning = if self.settings.copies(source) {
            match self.keyframe(ui, source) {
                Some(Ok(keyframe)) if (keyframe - self.start).abs() > 0.001 => Some(format!(
                    "Copied, so it starts at the keyframe before, {}. Frame-exact cuts where you asked.",
                    precise_time(keyframe)
                )),
                Some(Err(err)) => Some(err),
                _ => None,
            }
        } else {
            None
        };
        ui.horizontal_wrapped(|ui| {
            ui.weak(length);
            if let Some(warning) = warning {
                ui.colored_label(ui.visuals().warn_fg_color, warning);
            }
        });
    }

    /// A box to type one end of the clip in; the time, once it's entered.
    fn time_field(&mut self, ui: &mut Ui, which: Handle, value: f64) -> Option<f64> {
        let typing = self.runtime.typing.clone().filter(|(h, _)| *h == which);
        let mut text = typing
            .as_ref()
            .map_or_else(|| precise_time(value), |(_, text)| text.clone());
        let valid = parse_time(&text).is_some();
        let mut edit = TextEdit::singleline(&mut text)
            .id(Id::new(("clip-time", self.id, which)))
            .desired_width(78.0)
            .font(TextStyle::Monospace);
        if !valid {
            edit = edit.text_color(ui.visuals().error_fg_color);
        }
        let response = ui
            .add(edit)
            .on_hover_text("Type a time, like 1:23.5, and press Enter");
        if response.has_focus() {
            self.runtime.typing = Some((which, text));
            None
        } else if response.lost_focus() {
            self.runtime.typing = None;
            let cancelled = ui.input(|i| i.key_pressed(Key::Escape));
            (!cancelled).then(|| parse_time(&text)).flatten()
        } else {
            if typing.is_some() {
                self.runtime.typing = None;
            }
            None
        }
    }

    /// Dims what's cropped away over the picture at `shown`, and lets the
    /// pointer change what isn't.
    fn crop_overlay(
        &mut self,
        ui: &Ui,
        response: &egui::Response,
        area: Rect,
        shown: Rect,
        source: &Source,
    ) {
        let Some(crop) = self.settings.crop else {
            self.runtime.crop_drag = None;
            return;
        };
        let view = self.settings.view();
        let size = shown.size().max(vec2(1.0, 1.0));
        // Everything's worked out on the original picture, whichever way
        // it's shown.
        let fraction = |pos: egui::Pos2| {
            view.to_source((
                (pos.x - shown.min.x) / size.x,
                (pos.y - shown.min.y) / size.y,
            ))
        };
        let reach = (CROP_REACH / size.x, CROP_REACH / size.y);
        let reach = if view.swaps_sides() {
            (reach.1, reach.0)
        } else {
            reach
        };
        let ratio = self.settings.crop_ratio(source);
        let pointer = response
            .hover_pos()
            .or_else(|| response.interact_pointer_pos());
        let hovered =
            pointer.and_then(|pos| Grab::at(&crop, fraction(pos), reach, ratio.is_some()));

        if !response.is_pointer_button_down_on() {
            self.runtime.crop_drag = None;
        } else if self.runtime.crop_drag.is_none()
            && let Some(pos) = response.interact_pointer_pos()
            && let Some(grab) = Grab::at(&crop, fraction(pos), reach, ratio.is_some())
        {
            self.runtime.crop_drag = Some((grab, crop, fraction(pos)));
        }
        let mut crop = crop;
        if let Some((grab, began, from)) = self.runtime.crop_drag
            && let Some(pos) = response.interact_pointer_pos()
        {
            let (x, y) = fraction(pos);
            crop = crop::dragged(began, grab, (x - from.0, y - from.1), ratio);
            self.settings.crop = Some(crop);
        }
        let held = self.runtime.crop_drag.map(|(grab, _, _)| grab).or(hovered);
        if let Some(grab) = held {
            ui.ctx().set_cursor_icon(view.cursor(grab));
        }

        let on_screen = |point: (f32, f32)| {
            let (x, y) = view.to_view(point);
            shown.min + vec2(x * size.x, y * size.y)
        };
        let kept = Rect::from_two_pos(
            on_screen((crop.x, crop.y)),
            on_screen((crop.x + crop.w, crop.y + crop.h)),
        );
        let painter = ui.painter_at(area);
        let dim = Color32::from_black_alpha(150);
        for part in [
            Rect::from_min_max(shown.min, pos2(shown.max.x, kept.min.y)),
            Rect::from_min_max(pos2(shown.min.x, kept.max.y), shown.max),
            Rect::from_min_max(pos2(shown.min.x, kept.min.y), pos2(kept.min.x, kept.max.y)),
            Rect::from_min_max(pos2(kept.max.x, kept.min.y), pos2(shown.max.x, kept.max.y)),
        ] {
            painter.rect_filled(part, 0.0, dim);
        }
        let line = Stroke::new(1.5, Color32::WHITE);
        painter.rect_stroke(kept, 0.0, line, egui::StrokeKind::Inside);
        for corner in [kept.min, kept.max, kept.left_bottom(), kept.right_top()] {
            let handle = Rect::from_center_size(corner, vec2(9.0, 9.0));
            painter.rect_filled(handle, 1.0, Color32::WHITE);
        }
    }

    /// Asks where a copy starting at the current start really would, if
    /// nothing has yet; the answer, once it's known.
    fn keyframe(&mut self, ui: &Ui, source: &Source) -> Option<Result<f64, String>> {
        let stale = self
            .runtime
            .keyframe
            .as_ref()
            .is_none_or(|(asked, _)| *asked != self.start);
        // Not for every step of a drag.
        if stale && self.runtime.drag.is_none() {
            let cell = Arc::new(OnceLock::new());
            let (result, ctx, path) = (cell.clone(), ui.ctx().clone(), self.clip.clone());
            let (source, start) = (source.clone(), self.start);
            crate::background::spawn("clip-keyframe", move || {
                let _ = result.set(ffmpeg::keyframe_before(&path, &source, start));
                ctx.request_repaint();
            });
            self.runtime.keyframe = Some((self.start, cell));
        }
        let (asked, cell) = self.runtime.keyframe.as_ref()?;
        (*asked == self.start)
            .then(|| cell.get().cloned())
            .flatten()
    }

    /// The settings, in groups.
    fn settings(&mut self, ui: &mut Ui, source: &Source, tools: &Tools) {
        let end = self.end.unwrap_or(source.duration);
        section(ui, "Output");
        self.format_ui(ui, source, tools);
        let format = self.settings.format;
        if !format.is_audio_only() {
            self.size_ui(ui, source, tools, end);
            section(ui, "Picture");
            self.picture_ui(ui, source);
        }
        if !source.audio.is_empty() && !format.is_animated() {
            section(ui, "Audio");
            self.audio_ui(ui, source);
        }
        ui.add_space(16.0);
        self.more_ui(ui, source, tools);
        ui.add_space(8.0);
    }

    /// The settings seldom changed, folded away, with what's changed of them
    /// on the fold.
    fn more_ui(&mut self, ui: &mut Ui, source: &Source, tools: &Tools) {
        let format = self.settings.format;
        let hardware = self.settings.hardware
            && self
                .settings
                .hardware_available(source, tools, self.original_extension());
        let mut changed = Vec::new();
        if self.settings.changes_speed() {
            changed.push(format!("{}× speed", self.settings.speed));
        }
        if hardware && !format.is_audio_only() {
            changed.push("hardware encoding".to_owned());
        }
        if self.settings.keep != Keep::default() && !format.is_animated() {
            changed.push("some metadata left out".to_owned());
        }
        // As big as the sections' headings, with what's changed small after.
        let mut title = egui::text::LayoutJob::default();
        let style = ui.style();
        RichText::new("More").heading().strong().append_to(
            &mut title,
            style,
            egui::FontSelection::Default,
            egui::Align::Center,
        );
        if !changed.is_empty() {
            RichText::new(format!("  {}", changed.join(", ")))
                .color(ui.visuals().weak_text_color())
                .append_to(
                    &mut title,
                    style,
                    egui::FontSelection::Default,
                    egui::Align::Center,
                );
        }
        egui::CollapsingHeader::new(title)
            .id_salt(("clip-more", self.id))
            .show(ui, |ui| {
                self.speed_ui(ui);
                if !format.is_audio_only() {
                    self.encoder_ui(ui, source, tools);
                }
                // An animation has no tags to speak of.
                if !format.is_animated() {
                    self.keep_ui(ui, source, tools);
                }
            });
    }

    /// What's carried over from the source.
    fn keep_ui(&mut self, ui: &mut Ui, source: &Source, tools: &Tools) {
        const LOCATION: &str = "Where it was filmed. Anyone you send the file to can read this.";
        ui.add_space(4.0);
        if tools.phone() {
            // The muxer takes a location, except in WebM, and has nowhere for
            // the rest.
            let extension = self.settings.extension(source, self.original_extension());
            let webm = extension.is_ok_and(|extension| extension == "webm");
            if source.has_location() && !webm {
                indented(ui, |ui| {
                    ui.checkbox(&mut self.settings.keep.location, "Keep location")
                        .on_hover_text(LOCATION);
                });
            }
            weak_note(
                ui,
                if source.has_subtitles_or_chapters() {
                    "Other metadata, like the title and date, and subtitles and chapters, aren't kept on Android."
                } else {
                    "Other metadata, like the title and date, isn't kept on Android."
                },
            );
            return;
        }
        let speed_changes = self.settings.changes_speed();
        let keep = &mut self.settings.keep;
        indented(ui, |ui| {
            ui.vertical(|ui| {
                ui.checkbox(&mut keep.details, "Keep metadata")
                    .on_hover_text("The title, creation date, camera and other tags");
                if source.has_location() {
                    ui.checkbox(&mut keep.location, "Keep location")
                        .on_hover_text(LOCATION);
                }
                if source.has_subtitles_or_chapters() {
                    ui.add_enabled_ui(!speed_changes, |ui| {
                        ui.checkbox(&mut keep.subtitles, "Keep subtitles and chapters")
                            .on_hover_text(
                                "Picture subtitles can't go into MP4 or WebM, and are left out",
                            )
                            .on_disabled_hover_text(
                                "They can't follow a change of speed, and are left out",
                            );
                    });
                }
                if source.is_hdr() {
                    ui.checkbox(&mut keep.hdr, "Keep HDR")
                        .on_hover_text("Without it, or in H.264, the video is converted to SDR");
                }
            });
        });
    }

    /// What's kept of the picture, how big, and which way up.
    fn picture_ui(&mut self, ui: &mut Ui, source: &Source) {
        let animated = self.settings.format.is_animated();
        row(ui, "Resolution", |ui| {
            let short_side = self.settings.visible_short_side(source);
            let no_side = if animated {
                format!("Auto ({}p at most)", ffmpeg::ANIMATION_SIDE)
            } else {
                format!("Original ({short_side}p)")
            };
            let selected = self
                .settings
                .resolution
                .filter(|side| *side < short_side)
                .map_or(no_side.clone(), |side| format!("{side}p"));
            ComboBox::from_id_salt(("resolution", self.id))
                .selected_text(selected)
                .width(combo_width(ui, 150.0))
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.settings.resolution, None, no_side);
                    for side in RESOLUTIONS.into_iter().filter(|s| *s < short_side) {
                        let label = format!("{side}p");
                        ui.selectable_value(&mut self.settings.resolution, Some(side), label);
                    }
                });
        });
        row(ui, "Frame rate", |ui| {
            let lower = |fps: &u32| f64::from(*fps) < source.fps() - 0.5;
            let no_rate = if animated {
                format!("Auto ({} fps at most)", ffmpeg::ANIMATION_FPS)
            } else {
                format!("Original ({} fps)", (source.fps() * 100.0).round() / 100.0)
            };
            let selected = self
                .settings
                .fps
                .filter(lower)
                .map_or(no_rate.clone(), |fps| format!("{fps} fps"));
            ComboBox::from_id_salt(("fps", self.id))
                .selected_text(selected)
                .width(combo_width(ui, 150.0))
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.settings.fps, None, no_rate);
                    for fps in FRAME_RATES.into_iter().filter(|fps| lower(fps)) {
                        let label = format!("{fps} fps");
                        ui.selectable_value(&mut self.settings.fps, Some(fps), label);
                    }
                });
        });

        row(ui, "Crop", |ui| {
            let mut cropping = self.settings.crop.is_some();
            if ui
                .checkbox(&mut cropping, "")
                .on_hover_text("Drag the rectangle on the video, by its inside, edges or corners")
                .changed()
            {
                self.settings.crop = cropping.then(|| match self.settings.crop_ratio(source) {
                    Some(ratio) => Crop::centered(ratio),
                    None => Crop::FULL,
                });
            }
            if cropping {
                let label = |shape: Option<(u32, u32)>| {
                    shape.map_or("Free".to_owned(), |(width, height)| {
                        format!("{width}:{height}")
                    })
                };
                let before = self.settings.crop_shape;
                ComboBox::from_id_salt(("crop-shape", self.id))
                    .selected_text(label(before))
                    .width(70.0)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.settings.crop_shape, None, label(None));
                        for shape in CROP_SHAPES {
                            let name = label(Some(shape));
                            ui.selectable_value(&mut self.settings.crop_shape, Some(shape), name);
                        }
                    });
                if self.settings.crop_shape != before
                    && let Some(ratio) = self.settings.crop_ratio(source)
                {
                    self.settings.crop = Some(Crop::centered(ratio));
                }
                let (width, height, _, _) = self
                    .settings
                    .crop
                    .unwrap_or(Crop::FULL)
                    .pixels(source.width(), source.height());
                ui.weak(format!("{width} × {height}"));
            } else {
                ui.weak("Off");
            }
        });
        row(ui, "Rotate", |ui| {
            let turn = self.settings.turn;
            if ui
                .button(icon::ARROW_COUNTER_CLOCKWISE)
                .on_hover_text("Rotate left")
                .clicked()
            {
                self.settings.turn = turn.counter_clockwise();
            }
            if ui
                .button(icon::ARROW_CLOCKWISE)
                .on_hover_text("Rotate right")
                .clicked()
            {
                self.settings.turn = turn.clockwise();
            }
            ui.weak(format!("{}°", self.settings.turn.degrees()));
            // A shape is of the result, so it needs another rectangle now.
            if self.settings.turn.swaps_sides() != turn.swaps_sides()
                && self.settings.crop.is_some()
                && let Some(ratio) = self.settings.crop_ratio(source)
            {
                self.settings.crop = Some(Crop::centered(ratio));
            }
        });
        row(ui, "Mirror", |ui| {
            ui.toggle_value(
                &mut self.settings.flip_horizontal,
                format!("{} Horizontal", icon::FLIP_HORIZONTAL),
            )
            .on_hover_text("Swap left and right");
            ui.toggle_value(
                &mut self.settings.flip_vertical,
                format!("{} Vertical", icon::FLIP_VERTICAL),
            )
            .on_hover_text("Swap top and bottom");
        });
    }

    /// How fast it plays.
    fn speed_ui(&mut self, ui: &mut Ui) {
        row(ui, "Video speed", |ui| {
            let label = |speed: f64| format!("{speed}×");
            ComboBox::from_id_salt(("speed", self.id))
                .selected_text(label(self.settings.speed))
                .width(combo_width(ui, 150.0))
                .show_ui(ui, |ui| {
                    for speed in SPEEDS {
                        ui.selectable_value(&mut self.settings.speed, speed, label(speed));
                    }
                });
        });
    }

    /// Which encoder, and how hard it tries.
    fn encoder_ui(&mut self, ui: &mut Ui, source: &Source, tools: &Tools) {
        let available = self
            .settings
            .hardware_available(source, tools, self.original_extension());
        let hardware = self.settings.hardware && available;
        let encodes = !self.settings.copies(source)
            && !self.settings.format.is_audio_only()
            && !self.settings.format.is_animated();
        if available {
            indented(ui, |ui| {
                ui.checkbox(&mut self.settings.hardware, "Hardware encoding")
                    .on_hover_text(
                        "Encodes on the GPU: much faster, but the file is larger for the same quality.",
                    );
            });
        }
        if !tools.phone() && !hardware && encodes {
            row(ui, "Encoding", |ui| {
                for effort in ffmpeg::Effort::ALL {
                    ui.radio_value(&mut self.settings.effort, effort, effort.label())
                        .on_hover_text(
                            "How hard the encoder works: Fast takes less time, but the file is bigger or a little worse.",
                        );
                }
            });
        }
    }

    fn format_ui(&mut self, ui: &mut Ui, source: &Source, tools: &Tools) {
        let format_row = ui
            .scope(|ui| {
                row(ui, "Format", |ui| {
                    ComboBox::from_id_salt(("format", self.id))
                        .selected_text(self.settings.format.label())
                        .width(combo_width(ui, 170.0))
                        // All of them, without scrolling to find the audio ones.
                        .height(480.0)
                        .show_ui(ui, |ui| self.format_list(ui, source, tools));
                });
            })
            .response;
        self.point_out(ui, Reveal::Format, format_row.rect);
        if self.settings.format.is_audio_only() {
            return;
        }
        let anyway = self.settings.encodes_anyway(source);
        indented(ui, |ui| {
            ui.add_enabled_ui(!anyway, |ui| {
                let mut exact = self.settings.exact || anyway;
                let hover = "Re-encodes to start on the exact frame. Without it, the video is copied: it's quick and loses nothing, but can only start on a keyframe.";
                if ui
                    .checkbox(&mut exact, "Frame-exact cut")
                    .on_hover_text(hover)
                    .on_disabled_hover_text(
                        "These settings re-encode, which cuts on the frame anyway",
                    )
                    .changed()
                    && !anyway
                {
                    self.settings.exact = exact;
                }
            });
        });
    }

    /// The formats to choose from, in groups.
    fn format_list(&mut self, ui: &mut Ui, source: &Source, tools: &Tools) {
        let (mut audio_begun, mut animation_begun) = (false, false);
        for format in Format::ALL {
            if format.is_audio_only() && !audio_begun {
                audio_begun = true;
                ui.separator();
                ui.weak("Audio only");
            }
            if format.is_animated() && !animation_begun {
                animation_begun = true;
                ui.separator();
                ui.weak("Animation, no sound");
            }
            let no_audio = format.is_audio_only() && source.audio.is_empty();
            ui.add_enabled_ui(tools.can_make(format, source), |ui| {
                ui.selectable_value(&mut self.settings.format, format, format.label())
                    .on_disabled_hover_text(if no_audio {
                        "This video has no audio"
                    } else if tools.phone() {
                        "This phone can't make this yet"
                    } else {
                        "Your ffmpeg has no encoder for this"
                    });
            });
        }
    }

    /// Outlines `rect` for a moment, scrolled into sight, if it's the setting
    /// stopping the export that was just pressed.
    fn point_out(&mut self, ui: &Ui, which: Reveal, rect: Rect) {
        let Some((reveal, until, scrolled)) = self.runtime.reveal else {
            return;
        };
        if reveal != which {
            return;
        }
        if !scrolled {
            ui.scroll_to_rect(rect, Some(Align::Center));
            self.runtime.reveal = Some((reveal, until, true));
        }
        let now = ui.input(|i| i.time);
        if now > until {
            self.runtime.reveal = None;
            return;
        }
        // Pulses, so it's seen.
        let strength = (((until - now) * 6.0).sin() * 0.5 + 0.5) as f32;
        let warn = ui.visuals().warn_fg_color;
        ui.painter().rect_stroke(
            rect.expand(3.0),
            4.0,
            Stroke::new(2.0, warn.gamma_multiply(0.4 + strength * 0.6)),
            egui::StrokeKind::Outside,
        );
        ui.ctx().request_repaint();
    }

    /// How big it comes out: by quality, or a size to fit under.
    fn size_ui(&mut self, ui: &mut Ui, source: &Source, tools: &Tools, end: f64) {
        let size = ui
            .scope(|ui| {
                row(ui, "Size", |ui| {
                    ui.selectable_value(&mut self.settings.use_target, false, "By quality");
                    ui.selectable_value(&mut self.settings.use_target, true, "Target size");
                });
                if self.settings.use_target {
                    self.target_ui(ui, source, tools, end);
                } else if self.settings.copies(source) {
                    weak_note(ui, "Copied as it is: quick, and it loses nothing.");
                } else {
                    row(ui, "Quality", |ui| {
                        for quality in Quality::ALL {
                            ui.selectable_value(
                                &mut self.settings.quality,
                                quality,
                                quality.label(),
                            );
                        }
                    });
                }
            })
            .response;
        self.point_out(ui, Reveal::Size, size.rect);
        if tools.phone() && source.is_hdr() && !self.settings.copies(source) {
            weak_note(ui, "HDR comes out as standard colour when re-encoded.");
        }
    }

    /// The size to aim for, and how that will go.
    fn target_ui(&mut self, ui: &mut Ui, source: &Source, tools: &Tools, end: f64) {
        row(ui, "At most", |ui| {
            ui.add(
                egui::DragValue::new(&mut self.settings.target_mb)
                    .range(0.1..=100_000.0)
                    .speed(1.0)
                    .suffix(" MB"),
            );
        });
        indented(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                for preset in TARGET_PRESETS {
                    let chosen = (self.settings.target_mb - preset).abs() < 0.001;
                    if ui.selectable_label(chosen, format!("{preset}")).clicked() {
                        self.settings.target_mb = preset;
                    }
                }
            });
        });
        if self.settings.format.is_animated() {
            weak_note(
                ui,
                "Shrinks the picture until it fits, which can take a few tries.",
            );
            return;
        }
        let length = end - self.start;
        let warn = ui.visuals().warn_fg_color;
        let fit = self.settings.fit(source, tools, length);
        match &fit {
            Ok(fit) => {
                weak_note(
                    ui,
                    format!(
                        "Video at {:.0} kbit/s, audio at {} kbit/s",
                        fit.video_bps / 1000.0,
                        fit.audio_kbps
                    ),
                );
                if fit.looks_poor() {
                    let hardware = self.settings.hardware
                        && self.settings.hardware_available(
                            source,
                            tools,
                            self.original_extension(),
                        );
                    note(
                        ui,
                        RichText::new(if hardware {
                            "It will look poor at this size, and a hardware encoder may not get this small."
                        } else {
                            "It will look poor at this size."
                        })
                        .color(warn),
                    );
                }
                if self
                    .settings
                    .reads_twice(source, tools, self.original_extension())
                {
                    weak_note(
                        ui,
                        "Goes through the video twice, so it takes longer than other exports.",
                    );
                }
                // Copying would keep all the quality there is.
                let share = length / source.duration.max(MIN_LENGTH);
                let copied = source.size as f64 * share;
                if self.settings.format == Format::Original
                    && copied < self.settings.target_mb * 1e6 * 0.9
                {
                    weak_note(
                        ui,
                        format!(
                            "It would already fit without re-encoding, at about {}.",
                            human_size(copied as u64)
                        ),
                    );
                }
            }
            Err(_) => note(
                ui,
                RichText::new(self.too_small(source, tools, length)).color(warn),
            ),
        }
        let fits = fit.is_ok();
        if fit.is_ok_and(|fit| !fit.looks_poor()) {
            return;
        }
        let fixes = self.fixes(source, tools, length, fits);
        if fixes.is_empty() {
            return;
        }
        weak_note(
            ui,
            if fits {
                "To make it look better:"
            } else {
                "To make it fit:"
            },
        );
        for (label, fix) in fixes {
            let clicked = indented(ui, |ui| ui.add(Button::new(label).wrap()).clicked());
            if clicked {
                match fix {
                    Fix::Picture(suggestion) => {
                        self.settings.resolution = suggestion.side.or(self.settings.resolution);
                        self.settings.fps = suggestion.fps.or(self.settings.fps);
                        self.settings.hardware &= !suggestion.software;
                    }
                    Fix::Target(mb) => self.settings.target_mb = mb,
                    Fix::Length(seconds) => self.end = Some(self.start + seconds),
                    Fix::NoAudio => self.settings.audio = AudioChoice::None,
                }
            }
        }
    }

    /// Says what a target size is too small for.
    fn too_small(&self, source: &Source, tools: &Tools, length: f64) -> String {
        let visible = self.settings.visible_short_side(source);
        let side = self
            .settings
            .resolution
            .filter(|side| *side < visible)
            .unwrap_or(visible);
        let fps = self
            .settings
            .fps
            .map(f64::from)
            .filter(|fps| *fps < source.fps() - 0.5)
            .unwrap_or(source.fps());
        let hardware = self.settings.hardware
            && self
                .settings
                .hardware_available(source, tools, self.original_extension());
        format!(
            "{} MB isn't enough for {} of {side}p at {} fps{}.",
            self.settings.target_mb,
            video::format_time(length / self.settings.speed),
            (fps * 100.0).round() / 100.0,
            if hardware {
                " on the hardware encoder"
            } else {
                ""
            },
        )
    }

    /// Changes that would make a target size fit (or, where it already fits,
    /// look fine), each with what its button says.
    fn fixes(&self, source: &Source, tools: &Tools, length: f64, fits: bool) -> Vec<(String, Fix)> {
        let enough = |settings: &Settings, length: f64| {
            settings
                .fit(source, tools, length)
                .is_ok_and(|fit| !fits || !fit.looks_poor())
        };
        let mut fixes = Vec::new();
        if let Some(suggestion) = self.settings.suggested_output(source, tools, length) {
            let label = if suggestion.software {
                format!("Switch to {}", suggestion.label())
            } else {
                format!("Lower the picture to {}", suggestion.label())
            };
            fixes.push((label, Fix::Picture(suggestion)));
        }
        let at_target = |mb: f64| Settings {
            target_mb: mb,
            ..self.settings
        };
        if let Some(least) = smallest(self.settings.target_mb, 100_000.0, |mb| {
            enough(&at_target(mb), length)
        }) {
            let mb = TARGET_PRESETS
                .into_iter()
                .find(|preset| *preset >= least)
                .unwrap_or_else(|| (least / 10.0).ceil() * 10.0);
            fixes.push((format!("Raise the target to {mb} MB"), Fix::Target(mb)));
        }
        if let Some(longest) = largest(0.0, length, |seconds| enough(&self.settings, seconds)) {
            let seconds = (longest * 10.0).floor() / 10.0;
            if seconds >= 1.0 && seconds < length - 0.05 {
                fixes.push((
                    format!(
                        "Keep only {}, ending at {}",
                        video::format_time(seconds),
                        precise_time(self.start + seconds)
                    ),
                    Fix::Length(seconds),
                ));
            }
        }
        let silent = Settings {
            audio: AudioChoice::None,
            ..self.settings
        };
        if !source.audio.is_empty()
            && self.settings.audio != AudioChoice::None
            && enough(&silent, length)
        {
            fixes.push(("Remove the audio".to_owned(), Fix::NoAudio));
        }
        fixes
    }

    /// Which audio goes in, and how much of the file it gets.
    fn audio_ui(&mut self, ui: &mut Ui, source: &Source) {
        let audio_only = self.settings.format.is_audio_only();
        let track_label = |choice: AudioChoice| match choice {
            AudioChoice::All if !audio_only => "All tracks".to_owned(),
            AudioChoice::One(index) => source
                .audio
                .iter()
                .find(|a| a.index == index)
                .map_or_else(String::new, |a| a.label.clone()),
            // A file of audio has room for one: the first.
            _ => source
                .audio
                .first()
                .map_or_else(String::new, |a| a.label.clone()),
        };
        if audio_only {
            // A file of audio always has some.
            if self.settings.audio == AudioChoice::None {
                self.settings.audio = AudioChoice::All;
            }
        } else {
            indented(ui, |ui| {
                let mut keep = self.settings.audio != AudioChoice::None;
                if ui.checkbox(&mut keep, "Keep audio").changed() {
                    self.settings.audio = if keep {
                        AudioChoice::All
                    } else {
                        AudioChoice::None
                    };
                }
            });
        }
        if source.audio.len() > 1 && self.settings.audio != AudioChoice::None {
            row(ui, "Tracks", |ui| {
                ComboBox::from_id_salt(("audio", self.id))
                    .selected_text(track_label(self.settings.audio))
                    .width(combo_width(ui, 170.0))
                    .show_ui(ui, |ui| {
                        if !audio_only {
                            let label = track_label(AudioChoice::All);
                            ui.selectable_value(&mut self.settings.audio, AudioChoice::All, label);
                        }
                        for stream in &source.audio {
                            let choice = AudioChoice::One(stream.index);
                            let label = stream.label.clone();
                            ui.selectable_value(&mut self.settings.audio, choice, label);
                        }
                    });
            });
        }
        let has_rate = !self.settings.format.is_lossless()
            && (audio_only || self.settings.audio != AudioChoice::None);
        if has_rate {
            row(ui, "Bitrate", |ui| {
                let stops: Vec<Option<u32>> = std::iter::once(None)
                    .chain(AUDIO_BITRATES.map(Some))
                    .collect();
                let mut index = stops
                    .iter()
                    .position(|stop| *stop == self.settings.audio_kbps)
                    .unwrap_or(0);
                ui.spacing_mut().slider_width = (ui.available_width() - 90.0).clamp(60.0, 150.0);
                ui.add(egui::Slider::new(&mut index, 0..=stops.len() - 1).show_value(false))
                    .on_hover_text("Auto keeps the audio as it is where it can");
                self.settings.audio_kbps = stops[index];
                ui.label(
                    self.settings
                        .audio_kbps
                        .map_or("Auto".to_owned(), |kbps| format!("{kbps} kbit/s")),
                );
            });
        }
    }

    fn original_extension(&self) -> &str {
        self.clip
            .extension()
            .and_then(OsStr::to_str)
            .unwrap_or_default()
    }

    /// The bar under everything, with the Export button; it remembers its
    /// height so what's above can leave room for it.
    fn footer(&mut self, ui: &mut Ui, shared: &mut Shared, source: &Source, tools: &Tools) {
        let response = ui
            .scope(|ui| {
                ui.separator();
                self.output(ui, shared, source, tools);
            })
            .response;
        let height = response.rect.height() + ui.spacing().item_spacing.y;
        if (height - self.runtime.footer_height).abs() > 0.5 {
            self.runtime.footer_height = height;
            ui.ctx().request_repaint();
        }
    }

    /// Roughly how big the file comes out, where that's known.
    fn size_estimate(&mut self, ui: &Ui, source: &Source) -> Option<String> {
        let end = self.end.unwrap_or(source.duration);
        if self.settings.target_bytes().is_some() {
            return Some(format!("≤ {} MB", self.settings.target_mb));
        }
        if !self.settings.copies(source) {
            return None;
        }
        let from = self
            .keyframe(ui, source)
            .and_then(Result::ok)
            .unwrap_or(self.start);
        let share = (end - from) / source.duration.max(MIN_LENGTH);
        Some(format!(
            "≈ {}",
            human_size((source.size as f64 * share) as u64)
        ))
    }

    /// Where it's saved, and the Export button.
    fn output(&mut self, ui: &mut Ui, shared: &mut Shared, source: &Source, tools: &Tools) {
        let extension = self.settings.extension(source, self.original_extension());
        let estimate = self.size_estimate(ui, source);
        let clip = self.clip.clone();
        let name = self.runtime.name.get_or_insert_with(|| {
            let stem = clip.file_stem().unwrap_or_default().to_string_lossy();
            format!("{stem} (edited)")
        });
        let trimmed = name.trim();
        let name_problem = if trimmed.is_empty() {
            Some("Give it a name")
        } else if trimmed.contains(['/', std::path::MAIN_SEPARATOR])
            || (cfg!(windows) && trimmed.contains(['<', '>', ':', '"', '|', '?', '*']))
        {
            Some("Names can't have slashes or the like in them")
        } else {
            None
        };
        ui.horizontal(|ui| {
            let room = (ui.available_width() - 48.0).max(60.0);
            let mut edit = TextEdit::singleline(name)
                .desired_width(room)
                .hint_text("Name");
            if name_problem.is_some() {
                edit = edit.text_color(ui.visuals().error_fg_color);
            }
            let name_field = ui.add(edit.id(Id::new(("clip-name", self.id))));
            name_field.on_hover_text("The new file's name");
            if let Ok(extension) = &extension {
                ui.weak(format!(".{extension}"));
            }
        });
        let name = name.trim().to_owned();

        let end = self.end.unwrap_or(source.duration);
        let fits = !self.settings.use_target
            || self.settings.format.is_audio_only()
            || self.settings.format.is_animated()
            || self.settings.fit(source, tools, end - self.start).is_ok();
        // What's stopping it, and the setting to point out.
        let problem = if let Some(problem) = name_problem {
            Some((problem.to_owned(), None))
        } else if let Err(err) = &extension {
            Some((err.clone(), Some(Reveal::Format)))
        } else if !fits {
            let too_small = format!(
                "{} MB is too small for these settings. Ways to make it fit are under Size.",
                self.settings.target_mb
            );
            Some((too_small, Some(Reveal::Size)))
        } else {
            None
        };
        let now = ui.input(|i| i.time);
        // Said until it's put right, or for a while.
        if problem.is_none()
            || self
                .runtime
                .export_note
                .as_ref()
                .is_some_and(|(_, until)| now > *until)
        {
            self.runtime.export_note = None;
        }
        let dir = self.dir().to_path_buf();
        let row_height = ui.spacing().interact_size.y.max(28.0);
        let right_to_left = Layout::right_to_left(Align::Center);
        ui.allocate_ui_with_layout(
            vec2(ui.available_width(), row_height),
            right_to_left,
            |ui| {
                let visuals = ui.visuals();
                // Locked, but it still says why when pressed.
                let (glyph, text, fill) = if problem.is_some() {
                    (
                        icon::LOCK_SIMPLE,
                        visuals.weak_text_color(),
                        visuals.widgets.inactive.weak_bg_fill,
                    )
                } else {
                    (
                        icon::EXPORT,
                        visuals.selection.stroke.color,
                        visuals.selection.bg_fill,
                    )
                };
                let label = RichText::new(format!("{glyph} Export"))
                    .strong()
                    .color(text);
                let export = ui.add(
                    Button::new(label)
                        .fill(fill)
                        .min_size(vec2(96.0, row_height)),
                );
                let export = match &problem {
                    Some((problem, _)) => export.on_hover_text(problem),
                    None => export.on_hover_text("Adds it to the jobs, which run one at a time"),
                };
                if export.clicked()
                    && let Some((problem, reveal)) = &problem
                {
                    self.runtime.export_note = Some((problem.clone(), now + 6.0));
                    match reveal {
                        Some(reveal) => self.runtime.reveal = Some((*reveal, now + 2.5, false)),
                        None => ui.memory_mut(|m| m.request_focus(Id::new(("clip-name", self.id)))),
                    }
                    ui.ctx()
                        .request_repaint_after(std::time::Duration::from_secs_f64(6.1));
                } else if export.clicked() {
                    shared.jobs.start(JobKind::Clip(Box::new(ClipJob {
                        input: self.clip.clone(),
                        dir: dir.clone(),
                        name,
                        start: self.start,
                        end,
                        settings: self.settings,
                        source: source.clone(),
                        tools: tools.clone(),
                    })));
                }
                if let Some((note, _)) = &self.runtime.export_note {
                    egui::Area::new(Id::new(("clip-export-note", self.id)))
                        .order(egui::Order::Foreground)
                        .pivot(Align2::RIGHT_BOTTOM)
                        .fixed_pos(export.rect.right_top() - vec2(0.0, 6.0))
                        .show(ui.ctx(), |ui| {
                            egui::Frame::popup(ui.style()).show(ui, |ui| {
                                ui.set_max_width(260.0);
                                let warn = ui.visuals().warn_fg_color;
                                ui.label(RichText::new(note).color(warn));
                            });
                        });
                }
                if let Some(estimate) = estimate {
                    ui.weak(estimate)
                        .on_hover_text("Roughly how big the file comes out");
                }
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    let folder = format!("{} {}", icon::FOLDER_OPEN, display_name(&dir));
                    if ui
                        .add(Button::new(folder).truncate())
                        .on_hover_text(format!(
                            "Saves in {}. Click to choose another folder.",
                            dir.display()
                        ))
                        .clicked()
                    {
                        self.runtime.picker = Some(FolderPicker::new(dir.clone()));
                        self.runtime.choosing_folder = true;
                    }
                });
            },
        );
        if tools.phone() {
            ui.label(
                RichText::new("Stay in the app while exporting: Android pauses it soon after you leave, until you're back.")
                    .small()
                    .weak(),
            );
        }
    }

    /// Beside the original, unless another folder was chosen.
    fn dir(&self) -> &Path {
        self.runtime
            .dir
            .as_deref()
            .or_else(|| self.clip.parent())
            .unwrap_or(Path::new("."))
    }

    fn choose_folder(&mut self, ctx: &egui::Context, show_hidden: bool) {
        if !self.runtime.choosing_folder {
            return;
        }
        let Some(picker) = &mut self.runtime.picker else {
            return;
        };
        let (mut done, mut cancel) = (false, false);
        let modal = Modal::new(Id::new(("clip-folder", self.id))).show(ctx, |ui| {
            ui.set_width(crate::widgets::modal_width(ui, 440.0));
            ui.heading("Save in");
            picker.ui(ui, show_hidden);
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                done = ui.button("Choose").clicked();
                cancel = ui.button("Cancel").clicked();
            });
        });
        if done {
            self.runtime.dir = Some(picker.current.clone());
        }
        if done || cancel || modal.should_close() {
            self.runtime.choosing_folder = false;
            self.runtime.picker = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_turned_picture_has_its_corners_in_the_right_places() {
        let texture = egui::TextureId::Managed(1);
        let rect = Rect::from_min_size(pos2(10.0, 20.0), vec2(90.0, 160.0));
        let view = Settings {
            turn: ffmpeg::Turn::Quarter,
            ..Settings::default()
        }
        .view();
        let egui::Shape::Mesh(mesh) = picture_mesh(texture, rect, view) else {
            panic!("not a mesh");
        };
        assert!(mesh.is_valid());
        assert_eq!(mesh.vertices[0].pos, rect.left_top());
        // Turned clockwise, the frame's bottom left is at the top left.
        assert_eq!(mesh.vertices[0].uv, pos2(0.0, 1.0));
        assert_eq!(mesh.vertices[1].uv, pos2(0.0, 0.0));
    }

    #[test]
    fn a_press_takes_the_closest_handle() {
        assert_eq!(Handle::at(100.0, 100.0, 300.0), Handle::Start);
        assert_eq!(Handle::at(305.0, 100.0, 300.0), Handle::End);
        assert_eq!(Handle::at(200.0, 100.0, 300.0), Handle::Playhead);
        // Handles side by side.
        assert_eq!(Handle::at(101.0, 100.0, 104.0), Handle::Start);
        assert_eq!(Handle::at(103.0, 100.0, 104.0), Handle::End);
    }

    #[test]
    fn a_new_editor_starts_from_the_defaults() {
        assert_eq!(
            ClipTab::new(1, "/v/a.mp4".into()).settings,
            Settings::default()
        );
    }

    #[test]
    fn a_drag_keeps_what_it_took() {
        let press = grabbed(None, true, 100.0, 100.0, 300.0);
        assert_eq!(press, Some(Handle::Start));
        // The pointer has left the handle, which only follows next frame.
        let far = grabbed(press, true, 180.0, 100.0, 300.0);
        assert_eq!(far, Some(Handle::Start));
        assert_eq!(grabbed(far, false, 180.0, 100.0, 300.0), None);
    }

    #[test]
    fn handles_cannot_cross() {
        let range = (10.0, 20.0);
        assert_eq!(moved(range, Handle::Start, 5.0, 60.0), (5.0, 20.0));
        assert_eq!(moved(range, Handle::Start, 25.0, 60.0).0, 20.0 - MIN_LENGTH);
        assert_eq!(moved(range, Handle::End, 30.0, 60.0), (10.0, 30.0));
        assert_eq!(moved(range, Handle::End, 2.0, 60.0).1, 10.0 + MIN_LENGTH);
        assert_eq!(moved(range, Handle::End, 99.0, 60.0).1, 60.0);
        assert_eq!(moved(range, Handle::Playhead, 15.0, 60.0), range);
    }

    #[test]
    fn playing_at_the_end_replays() {
        assert!(replays(20.0, 5.0, 20.0));
        assert!(replays(19.98, 5.0, 20.0));
        assert!(!replays(12.0, 5.0, 20.0));
        assert!(!replays(25.0, 5.0, 20.0), "past it, it plays on");
    }

    #[test]
    fn times_keep_their_milliseconds() {
        assert_eq!(precise_time(12.5), "0:12.500");
        assert_eq!(precise_time(3661.042), "1:01:01.042");
    }

    #[test]
    fn saved_state_comes_back() {
        let mut tab = ClipTab::new(3, "/v/a.mp4".into());
        tab.start = 2.5;
        tab.end = Some(9.0);
        tab.settings.exact = true;
        tab.settings.keep.location = false;
        let saved = ron::ser::to_string(&tab).unwrap();
        let back: ClipTab = ron::from_str(&saved).unwrap();
        assert_eq!((back.id, back.start, back.end), (3, 2.5, Some(9.0)));
        assert_eq!(back.settings, tab.settings);
    }
    #[test]
    fn typed_times_are_read() {
        assert_eq!(parse_time("12"), Some(12.0));
        assert_eq!(parse_time(" 1:23.5 "), Some(83.5));
        assert_eq!(parse_time("1:01:01.042"), Some(3661.042));
        assert_eq!(parse_time("0,25"), Some(0.25));
        assert_eq!(parse_time(".5"), Some(0.5));
        for bad in [
            "", "abc", "1:60", "1:2:3:4", "-1", "1e3", "inf", "1.5:00", ":30", "1:",
        ] {
            assert_eq!(parse_time(bad), None, "{bad:?}");
        }
        // What's shown can be typed back.
        assert_eq!(parse_time(&precise_time(3661.042)), Some(3661.042));
    }

    #[test]
    fn steps_land_on_frames() {
        assert_eq!(stepped(1.0, 1, 30.0, 10.0), 31.0 / 30.0);
        assert_eq!(stepped(1.01, -1, 30.0, 10.0), 29.0 / 30.0);
        assert_eq!(stepped(0.0, -1, 30.0, 10.0), 0.0);
        assert_eq!(stepped(10.0, 5, 30.0, 10.0), 10.0);
        // An unknown rate still steps.
        assert!(stepped(1.0, 1, 0.0, 10.0) > 1.0);
    }

    #[test]
    fn ruler_ticks_have_room() {
        // Ten minutes across 640 points: 64 points is a minute.
        assert_eq!(tick_step(600.0, 640.0), 60.0);
        assert_eq!(tick_step(2.0, 640.0), 0.2);
        // Past the largest step, whole hours.
        assert_eq!(tick_step(36_000.0, 100.0), 7.0 * 3600.0);
        assert_eq!(tick_label(83.0, 1.0), "1:23");
        assert_eq!(tick_label(83.25, 0.5), "1:23.3");
        assert_eq!(tick_label(83.25, 0.05), "1:23.25");
    }

    #[test]
    fn zooming_keeps_the_point_and_the_video() {
        let view = zoomed((0.0, 100.0), 2.0, 50.0, 100.0);
        assert_eq!(view, (25.0, 75.0));
        // Near an end, it stays within the video.
        assert_eq!(zoomed((0.0, 100.0), 2.0, 0.0, 100.0), (0.0, 50.0));
        // Out past the whole, it's the whole.
        assert_eq!(zoomed((25.0, 75.0), 0.1, 50.0, 100.0), (0.0, 100.0));
        // Never closer than the least.
        let close = zoomed((0.0, 100.0), 1e6, 50.0, 100.0);
        assert!((close.1 - close.0 - MIN_VIEW).abs() < 1e-9);
        assert_eq!(panned((90.0, 100.0), 5.0, 100.0), (90.0, 100.0));
        assert_eq!(panned((10.0, 20.0), -15.0, 100.0), (0.0, 10.0));
        assert_eq!(around(40.0, 50.0, 100.0), (39.0, 51.0));
    }

    #[test]
    fn searches_find_the_edge() {
        let under = |x: f64| x <= 42.0;
        assert!((largest(0.0, 100.0, under).unwrap() - 42.0).abs() < 1e-6);
        assert_eq!(largest(0.0, 10.0, under), Some(10.0));
        assert_eq!(largest(50.0, 100.0, under), None);
        let over = |x: f64| x >= 97.4;
        let least = smallest(10.0, 100_000.0, over).unwrap();
        assert!(least >= 97.4 && least - 97.4 < 1e-6);
        assert_eq!(smallest(100.0, 200.0, over), Some(100.0));
        assert_eq!(smallest(0.0, 50.0, over), None);
    }
}
