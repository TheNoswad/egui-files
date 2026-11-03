//! Dragging files and folders within the app.
//!
//! Drops onto a folder move within a drive and copy across drives, like most
//! file managers. Holding Ctrl forces a copy, Shift forces a move.

use std::path::{Path, PathBuf};

use egui::{Context, DragAndDrop, Id, Modifiers, Rect, Response, Stroke, StrokeKind};

use crate::{
    archive::{ExtractJob, ExtractMode, Selection},
    fs,
    haptics::Haptic,
    jobs::JobKind,
};

/// The drag-and-drop payload.
pub struct FileDrag {
    pub paths: Vec<PathBuf>,
    /// Dragged in from another app. While hovering, `paths` may still be
    /// empty: some apps only hand over the files on drop.
    pub external: bool,
    /// Entries dragged out of an archive tab; `paths` is empty. Dropping
    /// them on a folder extracts them there.
    pub archive: Option<ArchiveDrag>,
    /// A place from the sidebar: it can only be dropped on a tab bar or a
    /// tab's content to open it, never moved or copied (dragging Home onto a
    /// folder would move it).
    pub link_only: bool,
}

pub struct ArchiveDrag {
    pub archive: PathBuf,
    pub selection: Selection,
    pub password: Option<String>,
}

/// Starts dragging entries of an archive if the user began dragging `response`.
pub fn archive_source(response: &Response, drag: impl FnOnce() -> Option<ArchiveDrag>) {
    if response.drag_started()
        && let Some(drag) = drag()
    {
        DragAndDrop::set_payload(
            &response.ctx,
            FileDrag {
                paths: Vec::new(),
                external: false,
                archive: Some(drag),
                link_only: false,
            },
        );
    }
}

/// Starts dragging a sidebar place if the user began dragging `response`.
pub fn place_source(response: &Response, path: &Path) {
    if response.drag_started() {
        DragAndDrop::set_payload(
            &response.ctx,
            FileDrag {
                paths: vec![path.to_owned()],
                external: false,
                archive: None,
                link_only: true,
            },
        );
    }
}

pub enum DropTarget {
    Folder(PathBuf),
    Trash,
}

/// Starts dragging `paths` if the user began dragging `response` this frame.
pub fn source(response: &Response, paths: impl FnOnce() -> Vec<PathBuf>) {
    if response.drag_started() {
        start(&response.ctx, paths());
    }
}

/// Starts dragging `paths`, if there are any.
pub fn start(ctx: &Context, paths: Vec<PathBuf>) {
    if !paths.is_empty() {
        DragAndDrop::set_payload(
            ctx,
            FileDrag {
                paths,
                external: false,
                archive: None,
                link_only: false,
            },
        );
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
    if !response.contains_pointer() || claimed(ctx) || !accepts(&payload, &target, modifiers) {
        return None;
    }
    claim(ctx);

    let stroke = Stroke::new(2.0, crate::theme::colors(ctx).files.drop_target);
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
    if let (Some(drag), DropTarget::Folder(dest)) = (&payload.archive, &target) {
        let mut job = ExtractJob::new(drag.archive.clone(), dest.clone(), ExtractMode::Into, false);
        job.selection = Some(drag.selection.clone());
        job.password = drag.password.clone();
        crate::haptics::play(Haptic::Confirm);
        return Some(JobKind::Extract(job));
    }
    let paths = payload.paths.clone();
    if paths.is_empty() {
        return None;
    }
    crate::haptics::play(Haptic::Confirm);
    Some(match target {
        DropTarget::Trash => JobKind::Trash { paths },
        DropTarget::Folder(dest) => {
            // Files from other apps are copied unless Shift asks for a move.
            let copy = if payload.external {
                !modifiers.shift
            } else {
                should_copy(&paths, &dest, modifiers)
            };
            if copy {
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

/// Makes the empty part of a tab bar at `rect` a drop target: folders
/// dropped there open in new tabs, and are returned on the drop.
pub fn new_tab_target(ctx: &Context, rect: Rect) -> Option<Vec<PathBuf>> {
    let payload = DragAndDrop::payload::<FileDrag>(ctx)?;
    let over = ctx
        .pointer_latest_pos()
        .is_some_and(|pos| rect.contains(pos));
    // Other apps may only say what they're dragging on the drop.
    let unknown = payload.external && payload.paths.is_empty();
    if !over
        || claimed(ctx)
        || payload.archive.is_some()
        || !unknown && !payload.paths.iter().any(|p| p.is_dir())
    {
        return None;
    }
    claim(ctx);

    let stroke = Stroke::new(2.0, crate::theme::colors(ctx).files.drop_target);
    let layer = egui::LayerId::new(egui::Order::Foreground, Id::new("new-tab-target"));
    ctx.layer_painter(layer)
        .rect_stroke(rect, 3.0, stroke, StrokeKind::Inside);

    if !ctx.input(|i| i.pointer.any_released()) {
        return None;
    }
    let payload = DragAndDrop::take_payload::<FileDrag>(ctx)?;
    crate::haptics::play(Haptic::Confirm);
    Some(
        payload
            .paths
            .iter()
            .filter(|p| p.is_dir())
            .cloned()
            .collect(),
    )
}

/// The sidebar place being dragged over `rect`, unless something else took
/// it, and whether it was dropped this frame (which ends the drag).
pub fn place_over(ctx: &Context, rect: Rect) -> Option<(PathBuf, bool)> {
    let payload = DragAndDrop::payload::<FileDrag>(ctx)?;
    let over = ctx
        .pointer_latest_pos()
        .is_some_and(|pos| rect.contains(pos));
    if !over || claimed(ctx) || !payload.link_only {
        return None;
    }
    claim(ctx);
    let path = payload.paths.first()?.clone();
    let dropped = ctx.input(|i| i.pointer.any_released());
    if dropped {
        DragAndDrop::clear_payload(ctx);
    }
    Some((path, dropped))
}

fn accepts(payload: &FileDrag, target: &DropTarget, modifiers: Modifiers) -> bool {
    let paths = &payload.paths;
    if payload.link_only {
        return false;
    }
    if payload.archive.is_some() {
        return matches!(target, DropTarget::Folder(_));
    }
    if payload.external && paths.is_empty() {
        // Not known until dropped.
        return true;
    }
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
    // The other app draws its own drag icon.
    if payload.external {
        return;
    }
    let Some(pos) = ctx.pointer_latest_pos() else {
        return;
    };
    ctx.set_cursor_icon(egui::CursorIcon::Grabbing);
    let text = label(&payload);
    let hint = if ctx.input(|i| i.modifiers.command) {
        "  (copy)"
    } else {
        ""
    };
    egui::Area::new(Id::new("egui-files-dnd-preview"))
        .order(egui::Order::Tooltip)
        .fixed_pos(pos + egui::vec2(16.0, 16.0))
        // Follow the pointer off the edge instead of sticking to it; the
        // platform takes over the drag once the pointer leaves the window.
        .constrain(false)
        .interactable(false)
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.label(format!("{text}{hint}"));
            });
        });
}

/// What the drag label says: the file's name, or how many are dragged.
pub fn label(payload: &FileDrag) -> String {
    if let Some(drag) = &payload.archive {
        match drag.selection.paths.as_slice() {
            [single] => format!(
                "{} {}",
                egui_phosphor::regular::FILE_ARROW_DOWN,
                single.rsplit('/').next().unwrap_or(single)
            ),
            many => format!(
                "{} {} items from {}",
                egui_phosphor::regular::FILE_ARROW_DOWN,
                many.len(),
                fs::display_name(&drag.archive)
            ),
        }
    } else {
        match payload.paths.as_slice() {
            [single] => format!(
                "{} {}",
                fs::icon_for(single, single.is_dir(), false),
                fs::display_name(single)
            ),
            paths => format!("{} {} items", egui_phosphor::regular::FILES, paths.len()),
        }
    }
}

// ---------------------------------------------------------------------------
// Drags from other apps

// Only Linux reports drags from other apps this way (Windows has its own
// drop target); tests use it everywhere.
#[cfg(any(target_os = "linux", test))]
pub use external::*;

#[cfg(any(target_os = "linux", test))]
mod external {
    use super::*;

    /// A drag from another app, as reported by the platform.
    pub enum ExternalEvent {
        /// Files are held over the window at this position (in points).
        Hover(egui::Pos2),
        /// The drag left the window without dropping.
        Leave,
        Drop {
            pos: egui::Pos2,
            paths: Vec<PathBuf>,
            /// The drag started in this app and came back (e.g. via the platform).
            internal: bool,
        },
    }

    /// Feeds an external drag into egui as if it were an internal one, so the
    /// usual drop targets highlight and handle it.
    pub fn inject_external(ctx: &Context, raw: &mut egui::RawInput, event: ExternalEvent) {
        let is_external_drag = || DragAndDrop::payload::<FileDrag>(ctx).is_some_and(|p| p.external);
        match event {
            ExternalEvent::Hover(pos) => {
                if DragAndDrop::payload::<FileDrag>(ctx).is_none() {
                    DragAndDrop::set_payload(
                        ctx,
                        FileDrag {
                            paths: Vec::new(),
                            external: true,
                            archive: None,
                            link_only: false,
                        },
                    );
                }
                raw.events.push(egui::Event::PointerMoved(pos));
            }
            ExternalEvent::Leave => {
                if is_external_drag() {
                    DragAndDrop::clear_payload(ctx);
                }
                raw.events.push(egui::Event::PointerGone);
            }
            ExternalEvent::Drop {
                pos,
                paths,
                internal,
            } => {
                DragAndDrop::set_payload(
                    ctx,
                    FileDrag {
                        paths,
                        external: !internal,
                        archive: None,
                        link_only: false,
                    },
                );
                raw.events.push(egui::Event::PointerMoved(pos));
                raw.events.push(egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: ctx.input(|i| i.modifiers),
                });
            }
        }
    }

    // ---------------------------------------------------------------------------
    // text/uri-list, the format file managers exchange file lists in

    /// Formats paths as a `text/uri-list` of `file://` URIs.
    pub fn uri_list(paths: &[PathBuf]) -> String {
        paths
            .iter()
            .map(|path| format!("file://{}\r\n", percent_encode(path)))
            .collect()
    }

    /// Parses the local files out of a `text/uri-list`.
    pub fn parse_uri_list(text: &str) -> Vec<PathBuf> {
        text.lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .filter_map(|line| line.strip_prefix("file://"))
            // `file://host/path`: only local files are usable.
            .filter_map(|rest| match rest.find('/') {
                Some(0) => Some(rest),
                Some(i) if &rest[..i] == "localhost" => Some(&rest[i..]),
                _ => None,
            })
            .filter_map(percent_decode)
            .collect()
    }

    fn percent_encode(path: &Path) -> String {
        let bytes = path_bytes(path);
        let mut out = String::with_capacity(bytes.len());
        for &b in bytes.iter() {
            if b.is_ascii_alphanumeric() || b"/-_.~".contains(&b) {
                out.push(b as char);
            } else {
                out.push_str(&format!("%{b:02X}"));
            }
        }
        out
    }

    fn percent_decode(text: &str) -> Option<PathBuf> {
        let bytes = text.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'%' {
                let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 3;
            } else {
                out.push(bytes[i]);
                i += 1;
            }
        }
        Some(path_from_bytes(out))
    }

    #[cfg(unix)]
    fn path_bytes(path: &Path) -> std::borrow::Cow<'_, [u8]> {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes().into()
    }

    #[cfg(unix)]
    fn path_from_bytes(bytes: Vec<u8>) -> PathBuf {
        use std::os::unix::ffi::OsStringExt;
        std::ffi::OsString::from_vec(bytes).into()
    }

    #[cfg(not(unix))]
    fn path_bytes(path: &Path) -> std::borrow::Cow<'_, [u8]> {
        path.to_string_lossy()
            .replace('\\', "/")
            .into_bytes()
            .into()
    }

    #[cfg(not(unix))]
    fn path_from_bytes(bytes: Vec<u8>) -> PathBuf {
        String::from_utf8_lossy(&bytes).into_owned().into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_list_round_trips_odd_names() {
        let paths = vec![
            PathBuf::from("/home/me/My File (1).txt"),
            PathBuf::from("/tmp/100%/ünïcödé #tag?.md"),
        ];
        let list = uri_list(&paths);
        assert!(list.starts_with("file:///home/me/My%20File%20%281%29.txt\r\n"));
        assert_eq!(parse_uri_list(&list), paths);
    }

    #[test]
    fn parses_what_other_apps_send() {
        let text = "# comment\r\nfile:///a/b%20c\nfile://localhost/d\r\nhttps://example.com/x\r\nfile://otherhost/e\r\n\r\n";
        assert_eq!(
            parse_uri_list(text),
            [PathBuf::from("/a/b c"), PathBuf::from("/d")]
        );
    }
}
