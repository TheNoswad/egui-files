//! Dragging files and folders within the app.
//!
//! Drops onto a folder move within a drive and copy across drives, like most
//! file managers. Holding Ctrl forces a copy, Shift forces a move.

use std::path::{Path, PathBuf};

use egui::{Context, DragAndDrop, Id, Modifiers, Response, Stroke, StrokeKind};

use crate::{fs, jobs::JobKind};

/// The drag-and-drop payload.
pub struct FileDrag {
    pub paths: Vec<PathBuf>,
}

pub enum DropTarget {
    Folder(PathBuf),
    Trash,
}

/// Starts dragging `paths` if the user began dragging `response` this frame.
pub fn source(response: &Response, paths: impl FnOnce() -> Vec<PathBuf>) {
    if response.drag_started() {
        let paths = paths();
        if !paths.is_empty() {
            DragAndDrop::set_payload(&response.ctx, FileDrag { paths });
        }
    }
}

/// Makes `response` a drop target: highlights it while files are held over it
/// and returns the job to start when they are dropped.
///
/// Overlapping targets (a folder row over its tab's background) are resolved
/// in call order: the first target under the pointer each frame wins.
pub fn target(response: &Response, target: DropTarget) -> Option<JobKind> {
    let ctx = &response.ctx;
    let payload = DragAndDrop::payload::<FileDrag>(ctx)?;
    let modifiers = ctx.input(|i| i.modifiers);
    if !response.contains_pointer() || claimed(ctx) || !accepts(&payload.paths, &target, modifiers)
    {
        return None;
    }
    claim(ctx);

    let stroke = Stroke::new(2.0, ctx.global_style().visuals.selection.stroke.color);
    ctx.layer_painter(response.layer_id).rect_stroke(
        response.rect,
        3.0,
        stroke,
        StrokeKind::Inside,
    );

    if !ctx.input(|i| i.pointer.any_released()) {
        return None;
    }
    let payload = DragAndDrop::take_payload::<FileDrag>(ctx)?;
    let paths = payload.paths.clone();
    Some(match target {
        DropTarget::Trash => JobKind::Trash { paths },
        DropTarget::Folder(dest) => {
            if should_copy(&paths, &dest, modifiers) {
                JobKind::Copy {
                    sources: paths,
                    dest,
                }
            } else {
                JobKind::Move {
                    sources: paths,
                    dest,
                }
            }
        }
    })
}

fn claim_id() -> Id {
    Id::new("egui-files-dnd-claim")
}

fn claimed(ctx: &Context) -> bool {
    let pass = ctx.cumulative_pass_nr();
    ctx.data(|d| d.get_temp::<u64>(claim_id())) == Some(pass)
}

fn claim(ctx: &Context) {
    let pass = ctx.cumulative_pass_nr();
    ctx.data_mut(|d| d.insert_temp(claim_id(), pass));
}

fn accepts(paths: &[PathBuf], target: &DropTarget, modifiers: Modifiers) -> bool {
    match target {
        DropTarget::Trash => true,
        DropTarget::Folder(dest) => {
            // Not into itself or a subfolder of itself.
            let into_itself = paths.iter().any(|p| dest.starts_with(p));
            // Moving into the folder it is already in does nothing.
            let no_op =
                !modifiers.command && paths.iter().all(|p| p.parent() == Some(dest.as_path()));
            !into_itself && !no_op
        }
    }
}

fn should_copy(paths: &[PathBuf], dest: &Path, modifiers: Modifiers) -> bool {
    if modifiers.command {
        return true;
    }
    if modifiers.shift {
        return false;
    }
    !paths.first().is_some_and(|p| same_device(p, dest))
}

#[cfg(unix)]
fn same_device(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let dev = |p: &Path| std::fs::symlink_metadata(p).map(|m| m.dev()).ok();
    dev(a).is_some() && dev(a) == dev(b)
}

#[cfg(windows)]
fn same_device(a: &Path, b: &Path) -> bool {
    // Compare drive prefixes (`C:`, `\\server\share`).
    a.components().next() == b.components().next()
}

/// Draws a label next to the pointer while files are being dragged.
pub fn paint_preview(ctx: &Context) {
    let Some(payload) = DragAndDrop::payload::<FileDrag>(ctx) else {
        return;
    };
    let Some(pos) = ctx.pointer_latest_pos() else {
        return;
    };
    ctx.set_cursor_icon(egui::CursorIcon::Grabbing);
    let text = match payload.paths.as_slice() {
        [single] => format!(
            "{} {}",
            fs::icon_for(single, single.is_dir(), false),
            fs::display_name(single)
        ),
        paths => format!("{} {} items", egui_phosphor::regular::FILES, paths.len()),
    };
    let hint = if ctx.input(|i| i.modifiers.command) {
        "  (copy)"
    } else {
        ""
    };
    egui::Area::new(Id::new("egui-files-dnd-preview"))
        .order(egui::Order::Tooltip)
        .fixed_pos(pos + egui::vec2(16.0, 16.0))
        .interactable(false)
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.label(format!("{text}{hint}"));
            });
        });
}
