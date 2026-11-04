//! The tab for trimming a video and choosing how to export it.

use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
};

use egui::{
    Align2, Button, Color32, ComboBox, FontId, Id, Key, Layout, Modal, Rect, RichText, ScrollArea,
    Sense, Stroke, TextEdit, Ui, pos2, vec2,
};
use egui_phosphor::regular as icon;
use serde::{Deserialize, Serialize};

use super::ffmpeg::{self, ClipJob, Format, Quality, Settings, Source, Tools};
use crate::{
    archive::FolderPicker,
    fs::{display_name, human_size},
    jobs::JobKind,
    tab::Shared,
    video,
};

/// The shortest range worth cutting, in seconds.
const MIN_LENGTH: f64 = 0.1;
/// How far from a handle, in points, a press still takes it.
const GRAB: f32 = 8.0;
/// The width of the settings column beside the video.
const SETTINGS_WIDTH: f32 = 290.0;

type Loaded = Result<(Source, Tools), String>;
/// The start a keyframe was asked for, and when known, the answer.
type KeyframeLookup = (f64, Arc<OnceLock<Result<f64, String>>>);

/// A part of the timeline the pointer can drag.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

/// What the tab worked out once, or is working out.
#[derive(Default)]
struct Runtime {
    loaded: Option<Arc<OnceLock<Loaded>>>,
    keyframe: Option<KeyframeLookup>,
    drag: Option<Handle>,
    /// Where playing was last frame, to stop on passing the range's end.
    last_position: f64,
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
                ui.centered_and_justified(|ui| {
                    ui.colored_label(ui.visuals().warn_fg_color, err);
                });
                return;
            }
            Some(Ok(loaded)) => loaded,
        };
        let end = self.end.unwrap_or(source.duration).min(source.duration);
        self.end = Some(end);
        self.start = self.start.clamp(0.0, (end - MIN_LENGTH).max(0.0));

        let width = ui.available_width();
        if width > SETTINGS_WIDTH * 2.5 {
            let height = ui.available_height();
            ui.horizontal_top(|ui| {
                let video = width - SETTINGS_WIDTH - ui.spacing().item_spacing.x - 8.0;
                let down = Layout::top_down(egui::Align::Min);
                ui.allocate_ui_with_layout(vec2(video, height), down, |ui| {
                    ui.set_width(video);
                    self.player(ui, shared, &source, height);
                });
                ui.separator();
                ui.vertical(|ui| {
                    ScrollArea::vertical()
                        .id_salt("clip-settings")
                        .show(ui, |ui| self.settings(ui, shared, &source, &tools));
                });
            });
        } else {
            ScrollArea::vertical().id_salt("clip-all").show(ui, |ui| {
                self.player(ui, shared, &source, (width * 0.8).max(240.0));
                ui.separator();
                self.settings(ui, shared, &source, &tools);
            });
        }
        self.choose_folder(ui.ctx(), shared.show_hidden);
    }

    /// The video, its timeline and its controls, `height` tall at most.
    fn player(&mut self, ui: &mut Ui, shared: &mut Shared, source: &Source, height: f32) {
        let id = Id::new(("clip-video", self.id));
        let width = ui.available_width();
        let player = match shared.videos.player(id, &self.clip, video::Mode::Preview) {
            Ok(player) => player,
            Err(err) => {
                ui.colored_label(
                    ui.visuals().warn_fg_color,
                    format!("Can't play videos: {err}"),
                );
                return;
            }
        };
        let status = player.status();

        // Room for the timeline and the row under it.
        let room = vec2(width, (height - 96.0).max(120.0));
        let source_size = status.video_size.map_or(vec2(16.0, 9.0), |[w, h]| {
            vec2(w.max(1) as f32, h.max(1) as f32)
        });
        let scale = (room.x / source_size.x).min(room.y / source_size.y);
        let (area, response) = ui.allocate_exact_size(room, Sense::click());
        let rect = Rect::from_center_size(area.center(), source_size * scale);
        let shown = player.render_for(rect, ui.pixels_per_point());
        let media = crate::theme::colors(ui.ctx()).media.clone();
        let painter = ui.painter_at(area);
        painter.rect_filled(area, 0.0, media.backdrop);
        if let Some(frame) = player.frame() {
            let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
            painter.image(frame.id(), shown, uv, Color32::WHITE);
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
        let clicked_video = response.clicked();

        // Stops at the end of the range when playing reaches it, but lets it
        // play on from past there.
        let end = self.end.unwrap_or(source.duration);
        if !status.paused && self.runtime.last_position < end && status.position >= end {
            player.set_paused(true);
            player.seek(end);
        }
        self.runtime.last_position = status.position;

        ui.add_space(6.0);
        let (track, timeline) = ui.allocate_exact_size(vec2(width, 34.0), Sense::click_and_drag());
        let inner = track.shrink2(vec2(GRAB, 0.0));
        let duration = source.duration.max(MIN_LENGTH);
        let x_of =
            |time: f64| inner.left() + inner.width() * (time / duration).clamp(0.0, 1.0) as f32;
        let (start_x, end_x) = (x_of(self.start), x_of(end));
        let time_of = |x: f32| f64::from((x - inner.left()) / inner.width().max(1.0)) * duration;

        let pressed = timeline.is_pointer_button_down_on() || timeline.clicked();
        let x = timeline.interact_pointer_pos().map_or(start_x, |pos| pos.x);
        self.runtime.drag = grabbed(self.runtime.drag, pressed, x, start_x, end_x);
        if let Some(handle) = self.runtime.drag
            && let Some(pos) = timeline.interact_pointer_pos()
        {
            let time = time_of(pos.x).clamp(0.0, source.duration);
            let (start, end) = moved((self.start, end), handle, time, source.duration);
            self.start = start;
            self.end = Some(end);
            let shown = match handle {
                Handle::Start => start,
                Handle::End => end,
                Handle::Playhead => time,
            };
            if timeline.is_pointer_button_down_on() || timeline.clicked() {
                player.seek(shown);
            }
        }
        let end = self.end.unwrap_or(end);
        let (start_x, end_x) = (x_of(self.start), x_of(end));

        let visuals = ui.visuals();
        let bar = Rect::from_x_y_ranges(inner.x_range(), track.y_range().shrink(9.0));
        let painter = ui.painter_at(track);
        painter.rect_filled(bar, 3.0, visuals.widgets.inactive.bg_fill);
        let chosen = Rect::from_x_y_ranges(start_x..=end_x, bar.y_range());
        let accent = visuals.selection.bg_fill;
        painter.rect_filled(chosen, 0.0, accent.gamma_multiply(0.6));
        for x in [start_x, end_x] {
            let handle = Rect::from_center_size(pos2(x, track.center().y), vec2(6.0, 26.0));
            painter.rect_filled(handle, 2.0, accent);
        }
        let playhead = x_of(status.position);
        painter.line_segment(
            [pos2(playhead, track.top()), pos2(playhead, track.bottom())],
            Stroke::new(2.0, visuals.strong_text_color()),
        );
        timeline
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .on_hover_text("Drag the handles to choose what to keep; I and O set them too");

        let keys = shared.focused_tab == Some(self.id)
            && !ui.ctx().text_edit_focused()
            && ui.ctx().memory(|m| m.top_modal_layer().is_none());
        let (space, mark_start, mark_end) = if keys {
            let free = ui.ctx().memory(|m| m.focused().is_none());
            ui.input(|i| {
                (
                    free && i.key_pressed(Key::Space),
                    i.key_pressed(Key::I),
                    i.key_pressed(Key::O),
                )
            })
        } else {
            (false, false, false)
        };

        ui.horizontal(|ui| {
            let play = if status.paused {
                icon::PLAY
            } else {
                icon::PAUSE
            };
            if ui
                .button(play)
                .on_hover_text(if status.paused { "Play" } else { "Pause" })
                .clicked()
                || space
                || clicked_video
            {
                // At the end of the clip, playing means seeing it again.
                if status.paused && replays(status.position, self.start, end) {
                    player.seek(self.start);
                    self.runtime.last_position = self.start;
                }
                player.set_paused(!status.paused);
            }
            if ui
                .button(format!("{} Start here", icon::BRACKETS_SQUARE))
                .on_hover_text("Start the clip at this frame (I)")
                .clicked()
                || mark_start
            {
                let range = (self.start, end);
                self.start = moved(range, Handle::Start, status.position, source.duration).0;
            }
            if ui
                .button(format!("{} End here", icon::BRACKETS_SQUARE))
                .on_hover_text("End the clip at this frame (O)")
                .clicked()
                || mark_end
            {
                let range = (self.start, end);
                let moved = moved(range, Handle::End, status.position, source.duration);
                self.end = Some(moved.1);
            }
            ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
                ui.monospace(format!(
                    "{} / {}",
                    precise_time(status.position),
                    video::format_time(source.duration)
                ));
            });
        });
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

    /// The range, the format and what's kept, and the Export button.
    fn settings(&mut self, ui: &mut Ui, shared: &mut Shared, source: &Source, tools: &Tools) {
        let end = self.end.unwrap_or(source.duration);
        ui.heading(format!("{} Edit video", icon::FILM_SLATE));
        ui.label(format!(
            "{} to {}  ({})",
            precise_time(self.start),
            precise_time(end),
            precise_time(end - self.start)
        ));
        ui.add_space(6.0);

        egui::Grid::new(("clip-format", self.id))
            .num_columns(2)
            .spacing(vec2(8.0, 6.0))
            .show(ui, |ui| {
                ui.label("Format:");
                ComboBox::from_id_salt(("format", self.id))
                    .selected_text(self.settings.format.label())
                    .width(170.0)
                    .show_ui(ui, |ui| {
                        for format in Format::ALL {
                            ui.add_enabled_ui(format.available(tools), |ui| {
                                ui.selectable_value(
                                    &mut self.settings.format,
                                    format,
                                    format.label(),
                                )
                                .on_disabled_hover_text("Your ffmpeg has no encoder for this");
                            });
                        }
                    });
                ui.end_row();
            });

        let plain_format = self.settings.format == Format::Original;
        ui.add_enabled_ui(plain_format, |ui| {
            ui.checkbox(&mut self.settings.exact, "Frame-exact cut")
                .on_hover_text(
                    "Re-encodes to start on the exact frame. Without it, the streams are copied: it's quick and loses nothing, but can only start on a keyframe.",
                );
        });

        let copies = self.settings.copies(source);
        if copies {
            let keyframe = self.keyframe(ui, source);
            let known_keyframe = keyframe.as_ref().and_then(|k| k.as_ref().ok()).copied();
            match keyframe {
                Some(Ok(keyframe)) if (keyframe - self.start).abs() > 0.001 => {
                    ui.weak(format!(
                        "Starts at {}, the keyframe before your mark",
                        precise_time(keyframe)
                    ));
                }
                Some(Err(err)) => {
                    ui.colored_label(ui.visuals().warn_fg_color, err);
                }
                _ => {}
            }
            let from = known_keyframe.unwrap_or(self.start);
            let share = (end - from) / source.duration.max(MIN_LENGTH);
            ui.weak(format!(
                "About {}",
                human_size((source.size as f64 * share) as u64)
            ));
        } else {
            ui.horizontal(|ui| {
                ui.label("Quality:");
                for quality in Quality::ALL {
                    ui.selectable_value(&mut self.settings.quality, quality, quality.label());
                }
            });
        }

        ui.add_space(6.0);
        ui.label("Keep:");
        let keep = &mut self.settings.keep;
        ui.checkbox(&mut keep.details, "File details")
            .on_hover_text("Title, creation date, camera and similar tags");
        if source.has_location() {
            ui.checkbox(&mut keep.location, "Location")
                .on_hover_text("Where it was filmed. Anyone you send the file to can read this.");
        }
        if source.has_subtitles_or_chapters() {
            ui.checkbox(&mut keep.subtitles, "Subtitles and chapters")
                .on_hover_text("Picture subtitles can't go into MP4 or WebM, and are left out");
        }
        if source.is_hdr() {
            ui.checkbox(&mut keep.hdr, "HDR")
                .on_hover_text("Without it, or in H.264, the video is converted to SDR");
        }

        ui.add_space(6.0);
        ui.separator();
        self.output(ui, shared, source, tools);
    }

    /// Where it's saved, and the Export button.
    fn output(&mut self, ui: &mut Ui, shared: &mut Shared, source: &Source, tools: &Tools) {
        let original = self
            .clip
            .extension()
            .and_then(OsStr::to_str)
            .unwrap_or_default();
        let extension = self.settings.extension(source, original);
        let clip = self.clip.clone();
        let name = self.runtime.name.get_or_insert_with(|| {
            let stem = clip.file_stem().unwrap_or_default().to_string_lossy();
            format!("{stem} (edited)")
        });
        ui.horizontal(|ui| {
            ui.label("Name:");
            let room = (ui.available_width() - 56.0).max(60.0);
            ui.add(TextEdit::singleline(name).desired_width(room));
            if let Ok(extension) = &extension {
                ui.weak(format!(".{extension}"));
            }
        });
        let name = name.trim().to_owned();
        let name_invalid = name.is_empty() || name.contains(['/', std::path::MAIN_SEPARATOR]);

        let dir = self.dir().to_path_buf();
        ui.horizontal(|ui| {
            ui.label("Save in:");
            ui.add(egui::Label::new(RichText::new(display_name(&dir)).weak()).truncate())
                .on_hover_text(dir.display().to_string());
        });
        if ui
            .button(format!("{} Choose folder…", icon::FOLDER_OPEN))
            .clicked()
        {
            self.runtime.picker = Some(FolderPicker::new(dir.clone()));
            self.runtime.choosing_folder = true;
        }

        ui.add_space(6.0);
        let label = RichText::new(format!("{} Export", icon::EXPORT)).strong();
        if ui
            .add_enabled(!name_invalid && extension.is_ok(), Button::new(label))
            .clicked()
        {
            shared.jobs.start(JobKind::Clip(Box::new(ClipJob {
                input: self.clip.clone(),
                dir,
                name,
                start: self.start,
                end: self.end.unwrap_or(source.duration),
                settings: self.settings,
                source: source.clone(),
                tools: tools.clone(),
            })));
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
    fn a_press_takes_the_closest_handle() {
        assert_eq!(Handle::at(100.0, 100.0, 300.0), Handle::Start);
        assert_eq!(Handle::at(305.0, 100.0, 300.0), Handle::End);
        assert_eq!(Handle::at(200.0, 100.0, 300.0), Handle::Playhead);
        // Handles side by side.
        assert_eq!(Handle::at(101.0, 100.0, 104.0), Handle::Start);
        assert_eq!(Handle::at(103.0, 100.0, 104.0), Handle::End);
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
}
