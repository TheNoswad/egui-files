//! The terminal itself: `alacritty_terminal` runs the emulation and the PTY,
//! and this draws its grid with egui and translates input. Native only.

use std::{
    borrow::Cow,
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc,
        mpsc::{self, Receiver, Sender},
    },
};

use alacritty_terminal::{
    Term,
    event::{Event as TermEvent, EventListener, WindowSize},
    event_loop::{EventLoop, EventLoopSender, Msg},
    grid::{Dimensions, Scroll},
    index::{Column, Direction, Point},
    selection::{Selection, SelectionType},
    sync::FairMutex,
    term::{Config, TermMode, cell::Flags, color::Colors, point_to_viewport, viewport_to_point},
    tty,
    vte::ansi::{Color, CursorShape, Rgb},
};
use egui::{
    Color32, Context, CursorIcon, EventFilter, FontId, Id, Key, Modifiers, PointerButton, Pos2,
    Rect, Sense, Stroke, TextStyle, Ui, Vec2, pos2, vec2,
};
use serde::{Deserialize, Serialize};

use super::{FollowState, TerminalResponse, default_ansi, follow_menu, open_menu};

/// Seconds to wait for the shell to report the folder it was sent to.
#[cfg(target_os = "linux")]
const CD_TIMEOUT: f64 = 2.0;

/// Saved with the panel's layout: which terminals there are, and where their
/// shells were, so they start there again.
#[derive(Default, Serialize, Deserialize)]
pub struct Terminal {
    pub id: u64,
    /// Keep the shell in the focused tab's folder, and the tab in the shell's.
    pub follow: bool,
    /// The folder to start the shell in, overriding the focused tab's.
    #[serde(default)]
    pub dir: Option<PathBuf>,
    /// A program to run instead of the shell ("Open with" a terminal app).
    /// Not saved: after a restart the terminal has a shell.
    #[serde(skip)]
    command: Option<crate::open_with::Launch>,
    /// Give this terminal keyboard focus the next time it's drawn.
    #[serde(skip)]
    focus_requested: bool,
    #[serde(skip)]
    session: Option<Session>,
    /// Why the shell couldn't be started.
    #[serde(skip)]
    error: Option<String>,
    /// The folder the shell was last seen in, or last sent to.
    #[serde(skip)]
    synced: Option<PathBuf>,
    /// A `cd` sent to the shell that it hasn't carried out yet, and when.
    #[cfg(target_os = "linux")]
    #[serde(skip)]
    pending_cd: Option<(PathBuf, f64)>,
    /// Following onto a server: the folder there the shell was last sent
    /// to with ssh, or that the tab moved to while the shell was busy (so
    /// leaving ssh doesn't go straight back in).
    #[cfg(target_os = "linux")]
    #[serde(skip)]
    remote_sent: Option<PathBuf>,
    /// The tab's folder, as last seen.
    #[cfg(target_os = "linux")]
    #[serde(skip)]
    last_folder: Option<PathBuf>,
    /// The shell has been at a prompt since it started.
    #[cfg(target_os = "linux")]
    #[serde(skip)]
    prompted: bool,
    /// In ssh on a server, gone there following the tab.
    #[cfg(target_os = "linux")]
    #[serde(skip)]
    in_server: Option<InServer>,
    /// A key already sent with Alt; drop the text event that follows it.
    #[serde(skip)]
    skip_text: bool,
    /// Scroll-wheel movement not yet turned into whole lines.
    #[serde(skip)]
    scroll_acc: f32,
    /// The last cell reported to a mouse-aware program while dragging.
    #[serde(skip)]
    last_mouse_cell: Option<(usize, usize)>,
    /// Ctrl and Alt tapped on the extra keys, for the next key typed.
    #[serde(skip)]
    sticky: Modifiers,
}

impl Terminal {
    pub fn new(id: u64, follow: bool) -> Self {
        Self {
            id,
            follow,
            focus_requested: true,
            ..Default::default()
        }
    }

    /// A terminal running `command` instead of the shell; it closes when the
    /// program exits.
    pub fn with_command(id: u64, command: crate::open_with::Launch) -> Self {
        Self {
            dir: command.dir.clone(),
            command: Some(command),
            ..Self::new(id, false)
        }
    }

    /// Remembers the shell's folder, so a restart opens the shell there.
    pub fn remember_dir(&mut self) {
        #[cfg(target_os = "linux")]
        let current = self.session.as_ref().and_then(Session::shell_cwd);
        #[cfg(not(target_os = "linux"))]
        let current = None;
        if let Some(dir) = current.or_else(|| self.synced.clone()) {
            self.dir = Some(dir);
        }
    }

    pub fn widget_id(&self) -> Id {
        Id::new(("terminal", self.id))
    }

    pub fn request_focus(&mut self) {
        self.focus_requested = true;
    }

    /// The title the shell (or program) set, else the program's name.
    pub fn title(&self) -> Option<&str> {
        let program = self.command.as_ref().map(|c| {
            let name = Path::new(&c.program).file_name();
            name.and_then(|n| n.to_str()).unwrap_or(&c.program)
        });
        self.session
            .as_ref()
            .and_then(|s| s.title.as_deref())
            .or(program)
    }

    /// Starts or stops following the focused folder. A shell that starts
    /// following goes to the tab's folder, rather than taking the tab along.
    pub fn set_follow(&mut self, follow: bool) {
        self.follow = follow;
        #[cfg(target_os = "linux")]
        if follow && let Some(cwd) = self.session.as_ref().and_then(Session::shell_cwd) {
            self.synced = Some(cwd);
            self.pending_cd = None;
        }
    }

    /// Draws the terminal into the rest of `ui`, starting the shell in
    /// `folder` (the focused browser tab's) if it isn't running yet.
    /// `follow` is for the context menu.
    pub fn ui(
        &mut self,
        ui: &mut Ui,
        folder: Option<&Path>,
        follow: FollowState,
    ) -> TerminalResponse {
        let mut response = TerminalResponse::default();
        let mut rect = ui.available_rect_before_wrap();
        // Phone keyboards lack Esc, Ctrl, the arrows and so on: a row of
        // them goes under the terminal.
        let touch = crate::touch::is_touch(ui.ctx());
        let keys_rect = touch.then(|| {
            let keys = rect.with_min_y(rect.bottom() - crate::touch::TOUCH_SIZE);
            rect.max.y = keys.top();
            keys
        });
        // A pinch sizes the text, for every terminal.
        let scale_id = Id::new("terminal_text_scale");
        let mut scale = ui.data_mut(|d| *d.get_persisted_mut_or(scale_id, 1.0_f32));
        if crate::touch::pinch(ui, rect, &mut scale, 0.5..=3.0) {
            ui.data_mut(|d| d.insert_persisted(scale_id, scale));
        }
        let mut font = TextStyle::Monospace.resolve(ui.style());
        font.size *= scale;
        let cell = ui
            .ctx()
            .fonts_mut(|f| vec2(f.glyph_width(&font, 'M'), f.row_height(&font)));
        let size = TermSize {
            columns: ((rect.width() / cell.x) as usize).max(2),
            lines: ((rect.height() / cell.y) as usize).max(1),
        };

        if let Some(error) = &self.error {
            ui.label(format!("Couldn't start a shell: {error}"));
            if ui.button("Retry").clicked() {
                self.error = None;
            }
            return response;
        }
        if self.session.is_none() {
            // A following shell starts with the tab; others where they were
            // left, if that folder still exists.
            let saved = self.dir.take().filter(|d| !self.follow && d.is_dir());
            // A server's folder is reached with ssh once the shell's up.
            let start = saved
                .as_deref()
                .or(folder)
                .filter(|f| !crate::remote::is_remote(f));
            let shell = match &self.command {
                Some(command) => Some(tty::Shell::new(
                    command.program.clone(),
                    command.args.clone(),
                )),
                None => user_shell(),
            };
            match Session::spawn(ui.ctx(), start, shell, size, cell) {
                Ok(session) => {
                    self.session = Some(session);
                    self.synced = start.map(Path::to_path_buf);
                }
                Err(err) => {
                    self.error = Some(err.to_string());
                    return response;
                }
            }
        }
        let Some(session) = &mut self.session else {
            return response;
        };

        let palette = Palette::new(ui.ctx());
        session.process_events(ui.ctx(), &palette);
        if session.exited {
            response.close = true;
            return response;
        }
        session.resize(size, cell);

        let id = self.widget_id();
        let widget = ui.interact(rect, id, Sense::click_and_drag());
        ui.advance_cursor_after_rect(rect);
        if std::mem::take(&mut self.focus_requested)
            || widget.clicked()
            || widget.drag_started()
            || widget.secondary_clicked()
        {
            widget.request_focus();
        }
        let focused = widget.has_focus();
        if focused {
            ui.memory_mut(|m| {
                m.set_focus_lock_filter(
                    id,
                    EventFilter {
                        tab: true,
                        horizontal_arrows: true,
                        vertical_arrows: true,
                        escape: true,
                    },
                )
            });
            self.keyboard(ui.ctx());
        }
        let grid = Grid { rect, cell, size };
        let mouse_mode = self.mouse(ui, &widget, &grid);
        if widget.hovered() {
            ui.ctx().set_cursor_icon(if mouse_mode {
                CursorIcon::Default
            } else {
                CursorIcon::Text
            });
        }
        if !mouse_mode {
            widget.context_menu(|ui| self.context_menu(ui, follow, &mut response));
        }

        let Some(session) = &self.session else {
            return response;
        };
        paint(ui, session, &grid, &font, &palette, focused);
        // Where the text cursor is, which brings up a phone's keyboard. Only
        // in the mobile layout: on the desktop, an input method would then
        // send typed text as IME events, which the terminal doesn't read.
        if focused && touch {
            let cursor = session.term.lock().grid().cursor.point;
            let row = cursor.line.0.max(0) as usize;
            let cursor_rect = grid.cell_rect(cursor.column.0, row, 1);
            ui.ctx().output_mut(|o| {
                o.ime = Some(egui::output::IMEOutput {
                    purpose: egui::IMEPurpose::Terminal,
                    rect,
                    cursor_rect,
                    should_interrupt_composition: false,
                });
            });
        }
        if let Some(keys_rect) = keys_rect {
            let mut keys_ui = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(keys_rect)
                    .layout(egui::Layout::left_to_right(egui::Align::Center)),
            );
            if self.extra_keys(&mut keys_ui) {
                ui.memory_mut(|m| m.request_focus(id));
            }
        }

        #[cfg(target_os = "linux")]
        if self.follow
            && let Some(folder) = folder
        {
            response.navigate = self.follow(ui.input(|i| i.time), folder);
        }
        #[cfg(not(target_os = "linux"))]
        let _ = folder;
        response
    }

    fn context_menu(&mut self, ui: &mut Ui, follow: FollowState, response: &mut TerminalResponse) {
        let Some(session) = &self.session else {
            return;
        };
        let selection = session.term.lock().selection_to_string();
        if ui
            .add_enabled(
                selection.is_some(),
                egui::Button::new("Copy").shortcut_text("Ctrl+Shift+C"),
            )
            .clicked()
        {
            ui.ctx().copy_text(selection.unwrap_or_default());
            ui.close();
        }
        if ui
            .add(egui::Button::new("Paste").shortcut_text("Ctrl+Shift+V"))
            .clicked()
        {
            // The clipboard arrives as a paste event next frame.
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::RequestPaste);
            self.focus_requested = true;
            ui.close();
        }
        if ui.button("Clear scrollback").clicked() {
            let mut term = session.term.lock();
            term.grid_mut().clear_history();
            term.selection = None;
            ui.close();
        }
        ui.separator();
        open_menu(ui, response);
        ui.separator();
        response.follow = follow_menu(ui, self.follow, follow);
        ui.separator();
        if ui
            .add(egui::Button::new("Close terminal").shortcut_text("Ctrl+Shift+W"))
            .clicked()
        {
            response.close = true;
            ui.close();
        }
    }

    /// Sends typed keys, pastes and copies to the shell. Takes the events so
    /// the rest of the app doesn't act on them.
    /// The keys phone keyboards lack, in a row. Returns whether one was
    /// tapped, to keep the keyboard focus in the terminal.
    fn extra_keys(&mut self, ui: &mut Ui) -> bool {
        const KEYS: [(&str, ExtraKey); 16] = [
            ("Esc", ExtraKey::Key(Key::Escape)),
            ("Tab", ExtraKey::Key(Key::Tab)),
            ("Ctrl", ExtraKey::Ctrl),
            ("Alt", ExtraKey::Alt),
            (
                egui_phosphor::regular::ARROW_LEFT,
                ExtraKey::Key(Key::ArrowLeft),
            ),
            (
                egui_phosphor::regular::ARROW_DOWN,
                ExtraKey::Key(Key::ArrowDown),
            ),
            (
                egui_phosphor::regular::ARROW_UP,
                ExtraKey::Key(Key::ArrowUp),
            ),
            (
                egui_phosphor::regular::ARROW_RIGHT,
                ExtraKey::Key(Key::ArrowRight),
            ),
            ("Home", ExtraKey::Key(Key::Home)),
            ("End", ExtraKey::Key(Key::End)),
            ("PgUp", ExtraKey::Key(Key::PageUp)),
            ("PgDn", ExtraKey::Key(Key::PageDown)),
            ("-", ExtraKey::Text("-")),
            ("/", ExtraKey::Text("/")),
            ("|", ExtraKey::Text("|")),
            ("~", ExtraKey::Text("~")),
        ];
        let Some(session) = &self.session else {
            return false;
        };
        let mode = *session.term.lock().mode();
        let mut tapped = false;
        egui::ScrollArea::horizontal()
            .scroll_source(crate::touch::scroll_source(ui.ctx()))
            .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
            .show(ui, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    for (label, key) in KEYS {
                        let on = match key {
                            ExtraKey::Ctrl => self.sticky.ctrl,
                            ExtraKey::Alt => self.sticky.alt,
                            _ => false,
                        };
                        let button = egui::Button::selectable(on, label)
                            .min_size(vec2(44.0, crate::touch::TOUCH_SIZE - 6.0));
                        if !ui.add(button).clicked() {
                            continue;
                        }
                        tapped = true;
                        let bytes = match key {
                            ExtraKey::Ctrl => {
                                self.sticky.ctrl = !self.sticky.ctrl;
                                None
                            }
                            ExtraKey::Alt => {
                                self.sticky.alt = !self.sticky.alt;
                                None
                            }
                            ExtraKey::Key(key) => {
                                key_bytes(key, std::mem::take(&mut self.sticky), mode)
                            }
                            ExtraKey::Text(text) => {
                                Some(sticky_text(text, std::mem::take(&mut self.sticky)))
                            }
                        };
                        if let Some(bytes) = bytes {
                            let mut term = session.term.lock();
                            term.scroll_display(Scroll::Bottom);
                            term.selection = None;
                            drop(term);
                            session.write(bytes);
                        }
                    }
                });
            });
        tapped
    }

    fn keyboard(&mut self, ctx: &Context) {
        let Some(session) = &self.session else {
            return;
        };
        let mode = *session.term.lock().mode();
        let sticky = &mut self.sticky;
        let mut out = Vec::new();
        let mut copy = false;
        let mut scroll = None;
        ctx.input_mut(|i| {
            let held = i.modifiers;
            i.events.retain(|event| {
                match event {
                    egui::Event::Text(text) => {
                        if !std::mem::take(&mut self.skip_text) {
                            // Ctrl or Alt tapped on the extra keys.
                            out.extend(sticky_text(text, std::mem::take(sticky)));
                        }
                    }
                    egui::Event::Key {
                        key,
                        pressed,
                        modifiers,
                        ..
                    } => {
                        if !pressed {
                            return false;
                        }
                        // Ctrl or Alt tapped on the extra keys, for keys
                        // that don't type text (Enter, Backspace…).
                        let modifiers = if sticky.any() && !key_types_text(*key) {
                            *modifiers | std::mem::take(sticky)
                        } else {
                            *modifiers
                        };
                        let modifiers = &modifiers;
                        if let Some(lines) = scrollback_key(*key, *modifiers) {
                            scroll = Some(lines);
                        } else if let Some(bytes) = key_bytes(*key, *modifiers, mode) {
                            self.skip_text = modifiers.alt && !modifiers.ctrl;
                            out.extend_from_slice(&bytes);
                        }
                    }
                    // egui-winit turns Ctrl+C/X/V (with or without Shift) into
                    // these, without a key event.
                    egui::Event::Copy => {
                        if held.shift {
                            copy = true;
                        } else {
                            out.push(0x03);
                        }
                    }
                    egui::Event::Cut => {
                        if !held.shift {
                            out.push(0x18);
                        }
                    }
                    egui::Event::Paste(text) => out.extend(paste_bytes(text, mode)),
                    _ => return true,
                }
                false
            });
        });

        let mut term = session.term.lock();
        if copy && let Some(text) = term.selection_to_string() {
            ctx.copy_text(text);
        }
        if let Some(scroll) = scroll {
            term.scroll_display(scroll);
        }
        if !out.is_empty() {
            term.scroll_display(Scroll::Bottom);
            term.selection = None;
            drop(term);
            session.write(out);
        }
    }

    /// Selection, scrolling, and mouse reporting for programs that ask for it.
    /// Returns whether the program receives the mouse.
    fn mouse(&mut self, ui: &Ui, widget: &egui::Response, grid: &Grid) -> bool {
        let Some(session) = &self.session else {
            return false;
        };
        let mut term = session.term.lock();
        let mode = *term.mode();
        let shift = ui.input(|i| i.modifiers.shift);
        // Shift bypasses mouse reporting, like most terminals.
        let reporting = mode.intersects(TermMode::MOUSE_MODE) && !shift;
        let mut out = Vec::new();

        let touch = crate::touch::is_touch(ui.ctx());
        let holding = crate::touch::holding(ui.ctx());
        if widget.hovered() || widget.dragged() {
            // In the mobile layout a drag scrolls, as the wheel does,
            // unless it follows a long press, which selects.
            if touch && widget.dragged() && !holding {
                self.scroll_acc += widget.drag_delta().y;
            }
            self.scroll_acc += ui.input(|i| {
                i.events
                    .iter()
                    .map(|e| match e {
                        egui::Event::MouseWheel { unit, delta, .. } => match unit {
                            egui::MouseWheelUnit::Point => delta.y,
                            egui::MouseWheelUnit::Line => delta.y * grid.cell.y,
                            egui::MouseWheelUnit::Page => delta.y * grid.rect.height(),
                        },
                        _ => 0.0,
                    })
                    .sum::<f32>()
            });
            let lines = (self.scroll_acc / grid.cell.y).trunc() as i32;
            self.scroll_acc -= lines as f32 * grid.cell.y;
            if lines != 0 {
                let pos = ui
                    .input(|i| i.pointer.latest_pos())
                    .unwrap_or(grid.rect.min);
                let (col, row, _) = grid.cell_at(pos);
                if reporting {
                    let button = if lines > 0 { 64 } else { 65 };
                    for _ in 0..lines.abs() {
                        mouse_report(&mut out, mode, button, col, row, true);
                    }
                } else if mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL) {
                    let arrow: &[u8] = match (lines > 0, mode.contains(TermMode::APP_CURSOR)) {
                        (true, true) => b"\x1bOA",
                        (true, false) => b"\x1b[A",
                        (false, true) => b"\x1bOB",
                        (false, false) => b"\x1b[B",
                    };
                    for _ in 0..lines.abs() {
                        out.extend_from_slice(arrow);
                    }
                } else {
                    term.scroll_display(Scroll::Delta(lines));
                }
            }
        } else {
            self.scroll_acc = 0.0;
        }

        if reporting {
            let events: Vec<_> = ui.input(|i| {
                i.events
                    .iter()
                    .filter_map(|e| match e {
                        egui::Event::PointerButton {
                            pos,
                            button,
                            pressed,
                            ..
                        } if grid.rect.contains(*pos) => Some((*pos, *button, *pressed)),
                        _ => None,
                    })
                    .collect()
            });
            for (pos, button, pressed) in events {
                let code = match button {
                    PointerButton::Primary => 0,
                    PointerButton::Middle => 1,
                    PointerButton::Secondary => 2,
                    _ => continue,
                };
                let (col, row, _) = grid.cell_at(pos);
                mouse_report(&mut out, mode, code, col, row, pressed);
                self.last_mouse_cell = pressed.then_some((col, row));
            }
            if widget.dragged()
                && mode.intersects(TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION)
                && let Some(pos) = widget.interact_pointer_pos()
            {
                let (col, row, _) = grid.cell_at(pos);
                if self.last_mouse_cell != Some((col, row)) {
                    self.last_mouse_cell = Some((col, row));
                    mouse_report(&mut out, mode, 32, col, row, true);
                }
            }
        } else if let Some(pos) = widget.interact_pointer_pos() {
            let (col, row, side) = grid.cell_at(pos);
            let offset = term.grid().display_offset();
            let point = viewport_to_point(offset, Point::new(row, Column(col)));
            let select = |ty| Some(Selection::new(ty, point, side));
            if widget.triple_clicked() {
                term.selection = select(SelectionType::Lines);
            } else if widget.double_clicked() {
                term.selection = select(SelectionType::Semantic);
            } else if if touch {
                crate::touch::long_press_started(ui.ctx())
            } else {
                widget.drag_started_by(PointerButton::Primary)
            } {
                term.selection = select(SelectionType::Simple);
            } else if widget.dragged_by(PointerButton::Primary) && (!touch || holding) {
                if let Some(selection) = &mut term.selection {
                    selection.update(point, side);
                }
                // Scroll while dragging past the top or bottom.
                if pos.y < grid.rect.top() {
                    term.scroll_display(Scroll::Delta(1));
                } else if pos.y > grid.rect.bottom() {
                    term.scroll_display(Scroll::Delta(-1));
                }
            } else if widget.clicked() {
                term.selection = None;
            }
        }
        drop(term);
        if !out.is_empty() {
            session.write(out);
        }
        reporting
    }

    /// Keeps the shell and the focused tab in the same folder while the shell
    /// is at a prompt. Returns a folder the shell moved to.
    #[cfg(target_os = "linux")]
    fn follow(&mut self, now: f64, folder: &Path) -> Option<PathBuf> {
        let session = self.session.as_ref()?;
        let idle = session.shell_idle();
        self.prompted |= idle;
        // Back here from ssh, or still there.
        if idle {
            self.in_server = None;
        } else if self.in_server.is_some() {
            return self.follow_in_server(now, folder);
        }
        if crate::remote::is_remote(folder) {
            return self.follow_onto_server(folder, idle);
        }
        self.remote_sent = None;
        self.last_folder = Some(folder.to_path_buf());
        if !idle {
            return None;
        }
        let cwd = session.shell_cwd()?;
        // The shell reports its folder with symlinks resolved.
        let real = disk::canonicalize(folder).unwrap_or_else(|_| folder.to_path_buf());
        if let Some((target, since)) = &self.pending_cd {
            if cwd != *target && now - since < CD_TIMEOUT {
                return None;
            }
            self.pending_cd = None;
        }
        if self.synced.as_ref() != Some(&cwd) {
            // The user changed folder in the shell.
            self.synced = Some(cwd.clone());
            return (cwd != real).then_some(cwd);
        }
        if cwd != real {
            // The tab moved: clear the prompt's line, then cd. The leading
            // space keeps it out of the shell's history.
            let mut command = b"\x05\x15 cd -- ".to_vec();
            command.extend_from_slice(shell_quote(folder).as_bytes());
            command.push(b'\r');
            session.write(command);
            self.synced = Some(real.clone());
            self.pending_cd = Some((real, now));
        }
        None
    }
}

#[cfg(target_os = "linux")]
impl Terminal {
    /// Follows the tab onto a server: from a prompt, ssh there, into the
    /// folder, with the user's shell. Coming back out of ssh doesn't go
    /// back in, unless the tab moves again.
    fn follow_onto_server(&mut self, folder: &Path, idle: bool) -> Option<PathBuf> {
        let moved = self.last_folder.as_deref() != Some(folder);
        self.last_folder = Some(folder.to_path_buf());
        if moved && !idle && self.prompted {
            self.remote_sent = Some(folder.to_path_buf());
        }
        if !idle || self.remote_sent.as_deref() == Some(folder) {
            return None;
        }
        // Reached through KDE Connect: there's no ssh into it from here.
        if crate::phones::is_phone(folder) {
            return None;
        }
        // Once the app's connection is up, to share it: no password again.
        // If it failed, ssh asks for itself.
        use disk::sftp::Status;
        if matches!(
            disk::sftp::status(folder),
            Status::Connecting | Status::Disconnected
        ) {
            return None;
        }
        let command = ssh_command(folder)?;
        let authority = disk::sftp::parse(folder)?.authority;
        let session = self.session.as_ref()?;
        // What the shell here said of itself is no longer news.
        *session.marks() = Marks::default();
        let mut keys = b"\x05\x15 ".to_vec();
        keys.extend_from_slice(command.as_bytes());
        keys.push(b'\r');
        session.write(keys);
        self.remote_sent = Some(folder.to_path_buf());
        self.in_server = Some(InServer {
            authority,
            host: None,
            synced: None,
        });
        None
    }

    /// In ssh on a server: keeps its shell and the tab in the same folder,
    /// as here, if the shell says when it's at a prompt and where (see
    /// ENTER). Only at a prompt, with nothing typed since but what's being
    /// typed there, so a program that's running never gets a `cd`.
    fn follow_in_server(&mut self, now: f64, folder: &Path) -> Option<PathBuf> {
        self.last_folder = Some(folder.to_path_buf());
        // Leaving ssh doesn't go back in, unless the tab moves again.
        if crate::remote::is_remote(folder) {
            self.remote_sent = Some(folder.to_path_buf());
        }
        let session = self.session.as_ref()?;
        let (prompt, cwd) = {
            let marks = session.marks();
            (marks.prompt, marks.cwd.clone())
        };
        let (host, cwd) = cwd?;
        let full_screen = session.term.lock().mode().contains(TermMode::ALT_SCREEN);
        let server = self.in_server.as_mut()?;
        // The computer first heard from: not another reached from it.
        match &server.host {
            None => server.host = Some(host),
            Some(first) if *first != host => return None,
            Some(_) => {}
        }
        if !prompt || full_screen {
            return None;
        }
        if let Some((target, since)) = &self.pending_cd {
            if *target != Path::new(&cwd) && now - since < CD_TIMEOUT {
                return None;
            }
            self.pending_cd = None;
        }
        if server.synced.as_deref() != Some(cwd.as_str()) {
            let first = server.synced.is_none();
            server.synced = Some(cwd.clone());
            if !first {
                // The user changed folder there: the tab goes too.
                let to = disk::sftp::url(&server.authority, &cwd);
                self.remote_sent = Some(to.clone());
                self.last_folder = Some(to.clone());
                return (to != folder).then_some(to);
            }
        }
        // The tab moved, on this server: the shell goes too.
        let location = disk::sftp::parse(folder)?;
        if location.authority != server.authority
            || location.path.starts_with("/~")
            || location.path == cwd
        {
            return None;
        }
        let mut keys = b"\x05\x15 cd -- ".to_vec();
        keys.extend_from_slice(quote(&location.path).as_bytes());
        keys.push(b'\r');
        session.write(keys);
        server.synced = Some(location.path.clone());
        self.pending_cd = Some((PathBuf::from(location.path), now));
        None
    }
}

/// A shell on a server, reached with ssh by following the tab.
#[cfg(target_os = "linux")]
struct InServer {
    /// The server, as the tab's paths name it.
    authority: String,
    /// The computer's name as its shell gives it, once it has.
    host: Option<String>,
    /// The folder the shell was last seen in, or sent to.
    synced: Option<String>,
}

/// Quoted for a POSIX shell (and fish, and csh: none of them reads `'\''`
/// differently).
#[cfg(target_os = "linux")]
fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// The environment variable that holds ENTER for the shells here, so the
/// line typed to follow onto a server stays short.
#[cfg(target_os = "linux")]
const ENTER_VAR: &str = "EGUI_FILES_ENTER";

/// Run on a server by `sh`, with a folder (`~` for home): goes there, then
/// starts the user's shell as a login shell. bash and fish get a hook that
/// says, with each prompt, that it's a prompt and where (OSC 133 and OSC 7),
/// added to the user's own; other shells start as they are, and say so only
/// if they do anyway. Nothing is written there. One line, as csh would have
/// it, with nothing fish reads otherwise in single quotes.
#[cfg(target_os = "linux")]
const ENTER: &str = concat!(
    r#"case $1 in '' | '~') cd ;; '~/'*) cd -- "$HOME/${1#'~/'}" ;; *) cd -- "$1" ;; esac; "#,
    r#"s=${SHELL:-/bin/sh}; case ${s##*/} in "#,
    r#"bash) PROMPT_COMMAND='printf "\033]7;file://%s%s\007\033]133;A\007" "$HOSTNAME" "$PWD"'"${PROMPT_COMMAND:+;$PROMPT_COMMAND}"; export PROMPT_COMMAND; exec "$s" -l ;; "#,
    r#"fish) exec "$s" -l -C 'function __egui_files_prompt --on-event fish_prompt; printf "\e]7;file://%s%s\a\e]133;A\a" $hostname $PWD; end' ;; "#,
    r#"*) exec "$s" -l ;; esac"#,
);

/// What the shells here hold in ENTER_VAR: ENTER, as ssh runs it there.
#[cfg(target_os = "linux")]
fn enter_command() -> String {
    format!("exec sh -c {} sh", quote(ENTER))
}

/// What to type to reach `folder` on its server with ssh, in a login shell
/// there (see ENTER).
#[cfg(target_os = "linux")]
fn ssh_command(folder: &Path) -> Option<String> {
    let location = disk::sftp::parse(folder)?;
    let target = disk::sftp::Target::parse(&location.authority)?;
    let mut command = "ssh -t".to_owned();
    // Over the app's connection, if it has one: no password again.
    if let Some(path) = disk::sftp::control_path() {
        command.push_str(&format!(" -o {}", quote(&format!("ControlPath={path}"))));
    }
    if let Some(port) = target.port {
        command.push_str(&format!(" -p {port}"));
    }
    if let Some(user) = &target.user {
        command.push_str(&format!(" -l {}", quote(user)));
    }
    // `~` is the home folder there, before it's known where.
    let folder = location
        .path
        .strip_prefix('/')
        .filter(|p| *p == "~" || p.starts_with("~/"));
    let folder = folder.unwrap_or(&location.path);
    // ssh joins what follows the host into the command line there: ENTER,
    // then the folder, quoted for there and again for here.
    command.push_str(&format!(
        " -- {} \"${ENTER_VAR}\" {}",
        quote(&target.host),
        quote(&quote(folder))
    ));
    Some(command)
}

/// The shell to run: the user's login shell, except in tests, where its
/// setup may prompt for things.
fn user_shell() -> Option<tty::Shell> {
    cfg!(test).then(|| tty::Shell::new("/bin/sh".into(), Vec::new()))
}

#[cfg(target_os = "linux")]
fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', r"'\''"))
}

// ---------------------------------------------------------------------------
// What the shell says of itself

/// What the shell has said of itself, in escape codes it writes along with
/// its prompt: whether it's at a prompt (OSC 133's marks) and which folder
/// it's in (OSC 7). Shells that say nothing leave it all unknown.
#[cfg(target_os = "linux")]
#[derive(Default, Debug)]
struct Marks {
    /// At a prompt: the last mark was the prompt's, and nothing that would
    /// start a command (Enter) has been sent since.
    prompt: bool,
    /// The computer and folder, as of the last prompt.
    cwd: Option<(String, String)>,
}

/// Picks escape codes out of what the shell writes, as it goes by.
#[cfg(target_os = "linux")]
#[derive(Default)]
struct OscScanner {
    state: OscState,
    payload: Vec<u8>,
}

#[cfg(target_os = "linux")]
#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum OscState {
    #[default]
    Text,
    Escape,
    /// In an OSC, after `ESC ]`.
    Osc,
    /// An ESC in an OSC: `ESC \` ends it.
    OscEscape,
}

#[cfg(target_os = "linux")]
impl OscScanner {
    /// OSCs longer than this aren't ones looked for.
    const MAX: usize = 4096;

    fn feed(&mut self, bytes: &[u8], marks: &std::sync::Mutex<Marks>) {
        for &byte in bytes {
            self.state = match (self.state, byte) {
                (OscState::Text, 0x1b) => OscState::Escape,
                (OscState::Text, _) => OscState::Text,
                (OscState::Escape, b']') => {
                    self.payload.clear();
                    OscState::Osc
                }
                (OscState::Escape, 0x1b) => OscState::Escape,
                (OscState::Escape, _) => OscState::Text,
                (OscState::Osc, 0x07) => {
                    self.finish(marks);
                    OscState::Text
                }
                (OscState::Osc, 0x1b) => OscState::OscEscape,
                (OscState::Osc, _) if self.payload.len() < Self::MAX => {
                    self.payload.push(byte);
                    OscState::Osc
                }
                (OscState::Osc, _) => OscState::Text,
                (OscState::OscEscape, b'\\') => {
                    self.finish(marks);
                    OscState::Text
                }
                (OscState::OscEscape, b']') => {
                    self.payload.clear();
                    OscState::Osc
                }
                (OscState::OscEscape, _) => OscState::Text,
            };
        }
    }

    fn finish(&mut self, marks: &std::sync::Mutex<Marks>) {
        let text = String::from_utf8_lossy(&self.payload);
        let mut marks = marks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(mark) = text.strip_prefix("133;") {
            match mark.as_bytes().first() {
                // The prompt starts, or ends where typing starts.
                Some(b'A' | b'B') => marks.prompt = true,
                // A command starts.
                Some(b'C') => marks.prompt = false,
                _ => {}
            }
        } else if let Some(url) = text.strip_prefix("7;file://") {
            let (host, path) = url.split_at(url.find('/').unwrap_or(url.len()));
            if !path.is_empty() {
                marks.cwd = Some((host.to_owned(), percent_decode(path)));
            }
        }
    }
}

/// `%xx` decoded, as OSC 7's paths are encoded; anything else kept as it is.
#[cfg(target_os = "linux")]
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if bytes[i] == b'%'
            && let (Some(h), Some(l)) = (
                bytes.get(i + 1).and_then(|&b| hex(b)),
                bytes.get(i + 2).and_then(|&b| hex(b)),
            )
        {
            out.push((h * 16 + l) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The PTY, with what the shell writes looked through for [`Marks`] on its
/// way to the terminal.
#[cfg(target_os = "linux")]
struct WatchedPty {
    pty: tty::Pty,
    reader: WatchedReader,
}

#[cfg(target_os = "linux")]
struct WatchedReader {
    /// The PTY's own file, opened again.
    file: std::fs::File,
    scanner: OscScanner,
    marks: Arc<std::sync::Mutex<Marks>>,
}

#[cfg(target_os = "linux")]
impl std::io::Read for WatchedReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = std::io::Read::read(&mut self.file, buf)?;
        self.scanner.feed(&buf[..n], &self.marks);
        Ok(n)
    }
}

#[cfg(target_os = "linux")]
impl tty::EventedReadWrite for WatchedPty {
    type Reader = WatchedReader;
    type Writer = std::fs::File;

    unsafe fn register(
        &mut self,
        poll: &Arc<polling::Poller>,
        interest: polling::Event,
        mode: polling::PollMode,
    ) -> std::io::Result<()> {
        // SAFETY: as the caller promises us, so we promise the PTY.
        unsafe { self.pty.register(poll, interest, mode) }
    }

    fn reregister(
        &mut self,
        poll: &Arc<polling::Poller>,
        interest: polling::Event,
        mode: polling::PollMode,
    ) -> std::io::Result<()> {
        self.pty.reregister(poll, interest, mode)
    }

    fn deregister(&mut self, poll: &Arc<polling::Poller>) -> std::io::Result<()> {
        self.pty.deregister(poll)
    }

    fn reader(&mut self) -> &mut Self::Reader {
        &mut self.reader
    }

    fn writer(&mut self) -> &mut Self::Writer {
        self.pty.writer()
    }
}

#[cfg(target_os = "linux")]
impl tty::EventedPty for WatchedPty {
    fn next_child_event(&mut self) -> Option<tty::ChildEvent> {
        self.pty.next_child_event()
    }
}

#[cfg(target_os = "linux")]
impl alacritty_terminal::event::OnResize for WatchedPty {
    fn on_resize(&mut self, size: WindowSize) {
        self.pty.on_resize(size);
    }
}

// ---------------------------------------------------------------------------
// The shell session

/// Forwards terminal events to the UI thread and wakes it.
#[derive(Clone)]
struct Listener {
    tx: Sender<TermEvent>,
    ctx: Context,
}

impl EventListener for Listener {
    fn send_event(&self, event: TermEvent) {
        let _ = self.tx.send(event);
        self.ctx.request_repaint();
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct TermSize {
    columns: usize,
    lines: usize,
}

impl Dimensions for TermSize {
    fn total_lines(&self) -> usize {
        self.lines
    }

    fn screen_lines(&self) -> usize {
        self.lines
    }

    fn columns(&self) -> usize {
        self.columns
    }
}

impl TermSize {
    fn window_size(self, cell: Vec2) -> WindowSize {
        WindowSize {
            num_lines: self.lines as u16,
            num_cols: self.columns as u16,
            cell_width: cell.x as u16,
            cell_height: cell.y as u16,
        }
    }
}

struct Session {
    term: Arc<FairMutex<Term<Listener>>>,
    sender: EventLoopSender,
    events: Receiver<TermEvent>,
    size: TermSize,
    cell: Vec2,
    exited: bool,
    title: Option<String>,
    /// The shell's process id and a handle on the PTY, to tell whether the
    /// shell is at a prompt and which folder it's in.
    #[cfg(target_os = "linux")]
    shell: (u32, std::fs::File),
    /// What the shell (or one it ran, like ssh's at the other end) said of
    /// itself.
    #[cfg(target_os = "linux")]
    marks: Arc<std::sync::Mutex<Marks>>,
}

impl Session {
    fn spawn(
        ctx: &Context,
        cwd: Option<&Path>,
        // The user's login shell when `None`.
        shell: Option<tty::Shell>,
        size: TermSize,
        cell: Vec2,
    ) -> std::io::Result<Self> {
        #[allow(unused_mut)]
        let mut env: HashMap<String, String> =
            [("TERM", "xterm-256color"), ("COLORTERM", "truecolor")]
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .collect();
        // What following onto a server runs there (see ENTER), so the line
        // typed for it stays short.
        #[cfg(target_os = "linux")]
        env.insert(ENTER_VAR.to_owned(), enter_command());
        let options = tty::Options {
            shell,
            working_directory: cwd.map(Path::to_path_buf),
            env,
            ..Default::default()
        };
        let pty = tty::new(&options, size.window_size(cell), 0)?;
        #[cfg(target_os = "linux")]
        let shell = (pty.child().id(), pty.file().try_clone()?);
        #[cfg(target_os = "linux")]
        let marks = Arc::new(std::sync::Mutex::new(Marks::default()));
        #[cfg(target_os = "linux")]
        let pty = WatchedPty {
            reader: WatchedReader {
                file: pty.file().try_clone()?,
                scanner: OscScanner::default(),
                marks: marks.clone(),
            },
            pty,
        };

        let (tx, events) = mpsc::channel();
        let listener = Listener {
            tx,
            ctx: ctx.clone(),
        };
        let term = Arc::new(FairMutex::new(Term::new(
            Config::default(),
            &size,
            listener.clone(),
        )));
        let event_loop = EventLoop::new(term.clone(), listener, pty, false, false)?;
        let sender = event_loop.channel();
        event_loop.spawn();
        Ok(Self {
            term,
            sender,
            events,
            size,
            cell,
            exited: false,
            title: None,
            #[cfg(target_os = "linux")]
            shell,
            #[cfg(target_os = "linux")]
            marks,
        })
    }

    fn write(&self, bytes: impl Into<Cow<'static, [u8]>>) {
        let bytes = bytes.into();
        // Enter starts a command, if at a prompt: not at one until the next.
        #[cfg(target_os = "linux")]
        if bytes.iter().any(|&b| b == b'\r' || b == b'\n') {
            self.marks().prompt = false;
        }
        let _ = self.sender.send(Msg::Input(bytes));
    }

    #[cfg(target_os = "linux")]
    fn marks(&self) -> std::sync::MutexGuard<'_, Marks> {
        self.marks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn resize(&mut self, size: TermSize, cell: Vec2) {
        if size != self.size || cell != self.cell {
            self.size = size;
            self.cell = cell;
            self.term.lock().resize(size);
            let _ = self.sender.send(Msg::Resize(size.window_size(cell)));
        }
    }

    fn process_events(&mut self, ctx: &Context, palette: &Palette) {
        while let Ok(event) = self.events.try_recv() {
            match event {
                TermEvent::PtyWrite(text) => self.write(text.into_bytes()),
                TermEvent::ClipboardStore(_, text) => ctx.copy_text(text),
                TermEvent::ColorRequest(index, format) => {
                    let color = palette.color(index, self.term.lock().colors());
                    let [r, g, b, _] = color.to_array();
                    self.write(format(Rgb { r, g, b }).into_bytes());
                }
                TermEvent::TextAreaSizeRequest(format) => {
                    self.write(format(self.size.window_size(self.cell)).into_bytes());
                }
                TermEvent::Title(title) => self.title = Some(title),
                TermEvent::ResetTitle => self.title = None,
                TermEvent::ChildExit(_) | TermEvent::Exit => self.exited = true,
                _ => {}
            }
        }
    }

    /// Whether the shell itself, not a program it started, owns the terminal.
    #[cfg(target_os = "linux")]
    fn shell_idle(&self) -> bool {
        use std::os::fd::AsRawFd;
        let (pid, pty) = &self.shell;
        // SAFETY: the descriptor is owned by `pty` and open.
        let group = unsafe { libc::tcgetpgrp(pty.as_raw_fd()) };
        group > 0 && group as u32 == *pid
    }

    #[cfg(target_os = "linux")]
    fn shell_cwd(&self) -> Option<PathBuf> {
        disk::read_link(format!("/proc/{}/cwd", self.shell.0)).ok()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.sender.send(Msg::Shutdown);
    }
}

// ---------------------------------------------------------------------------
// Input encoding

/// Shift+PageUp/PageDown/Home/End scroll the history instead of reaching the shell.
fn scrollback_key(key: Key, modifiers: Modifiers) -> Option<Scroll> {
    if !modifiers.matches_exact(Modifiers::SHIFT) {
        return None;
    }
    Some(match key {
        Key::PageUp => Scroll::PageUp,
        Key::PageDown => Scroll::PageDown,
        Key::Home => Scroll::Top,
        Key::End => Scroll::Bottom,
        _ => return None,
    })
}

/// The bytes a key sends, for keys that don't arrive as text.
fn key_bytes(key: Key, m: Modifiers, mode: TermMode) -> Option<Vec<u8>> {
    // xterm's modifier parameter: 1 + Shift + 2·Alt + 4·Ctrl.
    let param = 1 + m.shift as u8 + 2 * m.alt as u8 + 4 * m.ctrl as u8;
    let app_cursor = mode.contains(TermMode::APP_CURSOR);
    // Cursor keys and F1–F4: ESC [ 1 ; m X with modifiers, otherwise CSI or SS3.
    let letter = |c: u8, ss3: bool| {
        if param > 1 {
            format!("\x1b[1;{param}{}", c as char).into_bytes()
        } else if ss3 {
            vec![0x1b, b'O', c]
        } else {
            vec![0x1b, b'[', c]
        }
    };
    // Editing keys and F5–F12: ESC [ n ~ or ESC [ n ; m ~.
    let tilde = |n: u8| {
        if param > 1 {
            format!("\x1b[{n};{param}~").into_bytes()
        } else {
            format!("\x1b[{n}~").into_bytes()
        }
    };
    let alt = |bytes: &[u8]| {
        let mut out = if m.alt { vec![0x1b] } else { Vec::new() };
        out.extend_from_slice(bytes);
        out
    };
    Some(match key {
        Key::Enter => alt(b"\r"),
        Key::Tab if m.shift => b"\x1b[Z".to_vec(),
        Key::Tab => b"\t".to_vec(),
        Key::Backspace if m.ctrl => alt(b"\x08"),
        Key::Backspace => alt(b"\x7f"),
        Key::Escape => b"\x1b".to_vec(),
        Key::ArrowUp => letter(b'A', app_cursor),
        Key::ArrowDown => letter(b'B', app_cursor),
        Key::ArrowRight => letter(b'C', app_cursor),
        Key::ArrowLeft => letter(b'D', app_cursor),
        Key::Home => letter(b'H', app_cursor),
        Key::End => letter(b'F', app_cursor),
        Key::Insert => tilde(2),
        Key::Delete => tilde(3),
        Key::PageUp => tilde(5),
        Key::PageDown => tilde(6),
        Key::F1 => letter(b'P', true),
        Key::F2 => letter(b'Q', true),
        Key::F3 => letter(b'R', true),
        Key::F4 => letter(b'S', true),
        Key::F5 => tilde(15),
        Key::F6 => tilde(17),
        Key::F7 => tilde(18),
        Key::F8 => tilde(19),
        Key::F9 => tilde(20),
        Key::F10 => tilde(21),
        Key::F11 => tilde(23),
        Key::F12 => tilde(24),
        _ if m.ctrl => alt(&[ctrl_byte(key)?]),
        // Alt+letter as ESC + letter; egui also sends the plain letter as
        // text, which the caller drops.
        _ if m.alt => {
            let c = key_char(key)?;
            alt(&[if m.shift { c.to_ascii_uppercase() } else { c }])
        }
        _ => return None,
    })
}

/// A key on the terminal's row of extra keys.
#[derive(Clone, Copy)]
enum ExtraKey {
    Key(Key),
    Ctrl,
    Alt,
    Text(&'static str),
}

/// Whether a key types text, which arrives as its own event that the
/// sticky modifiers apply to instead.
fn key_types_text(key: Key) -> bool {
    key.name().chars().count() == 1 || key == Key::Space
}

/// Typed text with Ctrl or Alt from the extra keys: Ctrl turns a letter
/// (and a few symbols) into its control character, Alt adds ESC before it.
fn sticky_text(text: &str, m: Modifiers) -> Vec<u8> {
    let mut out = Vec::new();
    let mut chars = text.chars();
    if let (Some(c), None) = (chars.next(), chars.next())
        && (m.ctrl || m.alt)
    {
        if m.alt {
            out.push(0x1b);
        }
        let ctrl = m
            .ctrl
            .then(|| match c.to_ascii_lowercase() {
                c @ 'a'..='z' => Some(c as u8 - b'a' + 1),
                ' ' | '@' | '2' => Some(0),
                '[' | '3' => Some(0x1b),
                '\\' | '4' => Some(0x1c),
                ']' | '5' => Some(0x1d),
                '^' | '6' => Some(0x1e),
                '_' | '/' | '-' | '7' => Some(0x1f),
                '?' | '8' => Some(0x7f),
                _ => None,
            })
            .flatten();
        match ctrl {
            Some(byte) => out.push(byte),
            None => out.extend_from_slice(text.as_bytes()),
        }
        return out;
    }
    out.extend_from_slice(text.as_bytes());
    out
}

fn key_char(key: Key) -> Option<u8> {
    let name = key.name();
    match name.as_bytes() {
        [c] if c.is_ascii_alphanumeric() => Some(c.to_ascii_lowercase()),
        _ => None,
    }
}

fn ctrl_byte(key: Key) -> Option<u8> {
    Some(match key {
        Key::Space | Key::Num2 => 0,
        Key::OpenBracket => 0x1b,
        Key::Backslash => 0x1c,
        Key::CloseBracket => 0x1d,
        Key::Num6 => 0x1e,
        Key::Slash | Key::Minus => 0x1f,
        _ => match key_char(key)? {
            c @ b'a'..=b'z' => c - b'a' + 1,
            _ => return None,
        },
    })
}

fn paste_bytes(text: &str, mode: TermMode) -> Vec<u8> {
    if mode.contains(TermMode::BRACKETED_PASTE) {
        // Strip escapes so the paste can't end the bracket early.
        let text = text.replace('\x1b', "");
        format!("\x1b[200~{text}\x1b[201~").into_bytes()
    } else {
        text.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
    }
}

/// Encodes a mouse event for a program that enabled mouse reporting.
/// `button`: 0–2 for buttons, +32 for motion, 64/65 for the wheel.
fn mouse_report(
    out: &mut Vec<u8>,
    mode: TermMode,
    button: u8,
    col: usize,
    row: usize,
    pressed: bool,
) {
    if mode.contains(TermMode::SGR_MOUSE) {
        let end = if pressed { 'M' } else { 'm' };
        out.extend(format!("\x1b[<{button};{};{}{end}", col + 1, row + 1).into_bytes());
    } else if col < 223 && row < 223 {
        // The old encoding can't tell which button was released.
        let button = if pressed { button } else { 3 };
        out.extend_from_slice(&[
            0x1b,
            b'[',
            b'M',
            32 + button,
            32 + 1 + col as u8,
            32 + 1 + row as u8,
        ]);
    }
}

// ---------------------------------------------------------------------------
// Drawing

/// Where the grid sits on screen.
struct Grid {
    rect: Rect,
    cell: Vec2,
    size: TermSize,
}

impl Grid {
    /// The column, row and half of the cell under `pos`, clamped to the grid.
    fn cell_at(&self, pos: Pos2) -> (usize, usize, Direction) {
        let rel = (pos - self.rect.min) / self.cell;
        let col = rel.x.max(0.0) as usize;
        let side = if rel.x.fract() < 0.5 {
            Direction::Left
        } else {
            Direction::Right
        };
        (
            col.min(self.size.columns - 1),
            (rel.y.max(0.0) as usize).min(self.size.lines - 1),
            side,
        )
    }

    fn cell_rect(&self, col: usize, row: usize, width: usize) -> Rect {
        Rect::from_min_size(
            self.rect.min + vec2(col as f32 * self.cell.x, row as f32 * self.cell.y),
            vec2(width as f32 * self.cell.x, self.cell.y),
        )
    }
}

struct Palette {
    fg: Color32,
    bg: Color32,
    cursor: Color32,
    ansi: [Color32; 16],
    selection_bg: Color32,
    selection_fg: Color32,
}

fn dim(color: Color32) -> Color32 {
    let [r, g, b, _] = color.to_array();
    let f = |c: u8| (c as f32 * 0.66) as u8;
    Color32::from_rgb(f(r), f(g), f(b))
}

impl Palette {
    fn new(ctx: &Context) -> Self {
        let colors = crate::theme::colors(ctx);
        let t = &colors.terminal;
        Self {
            fg: t.foreground,
            bg: t.background,
            cursor: t.cursor,
            ansi: t
                .ansi
                .unwrap_or_else(|| default_ansi(ctx.global_style().visuals.dark_mode)),
            selection_bg: t.selection,
            selection_fg: t.selection_text,
        }
    }

    /// Color `index` in alacritty's numbering, preferring colors the program set.
    fn color(&self, index: usize, set: &Colors) -> Color32 {
        if let Some(Rgb { r, g, b }) = set[index] {
            return Color32::from_rgb(r, g, b);
        }
        match index {
            0..16 => self.ansi[index],
            16..232 => {
                let i = index - 16;
                let level = |n: usize| if n == 0 { 0 } else { (55 + n * 40) as u8 };
                Color32::from_rgb(level(i / 36), level(i / 6 % 6), level(i % 6))
            }
            232..256 => {
                let v = (8 + (index - 232) * 10) as u8;
                Color32::from_gray(v)
            }
            256 | 267 => self.fg,
            258 => self.cursor,
            257 => self.bg,
            259..267 => dim(self.ansi[index - 259]),
            _ => dim(self.fg),
        }
    }

    fn resolve(&self, color: Color, set: &Colors) -> Color32 {
        match color {
            Color::Spec(Rgb { r, g, b }) => Color32::from_rgb(r, g, b),
            Color::Indexed(i) => self.color(i as usize, set),
            Color::Named(named) => self.color(named as usize, set),
        }
    }
}

/// A stretch of cells on one row drawn with one color.
struct Run {
    row: usize,
    col: usize,
    width: usize,
    color: Color32,
    text: String,
    underline: bool,
}

fn paint(ui: &Ui, session: &Session, grid: &Grid, font: &FontId, palette: &Palette, focused: bool) {
    let painter = ui.painter_at(grid.rect);
    painter.rect_filled(grid.rect, 0.0, palette.bg);

    let term = session.term.lock();
    let content = term.renderable_content();
    let offset = content.display_offset;
    let cursor = point_to_viewport(offset, content.cursor.point)
        .filter(|_| content.cursor.shape != CursorShape::Hidden);
    let cursor_shape = if focused {
        content.cursor.shape
    } else {
        CursorShape::HollowBlock
    };

    let mut backgrounds: Vec<Run> = Vec::new();
    let mut texts: Vec<Run> = Vec::new();
    let push = |runs: &mut Vec<Run>, run: Run, joinable: bool| {
        if joinable
            && let Some(last) = runs.last_mut()
            && last.row == run.row
            && last.col + last.width == run.col
            && last.color == run.color
            && last.underline == run.underline
        {
            last.width += run.width;
            last.text.push_str(&run.text);
        } else {
            runs.push(run);
        }
    };

    for indexed in content.display_iter {
        let cell = indexed.cell;
        if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
            continue;
        }
        let Some(at) = point_to_viewport(offset, indexed.point) else {
            continue;
        };
        let (row, col) = (at.line, at.column.0);
        let width = if cell.flags.contains(Flags::WIDE_CHAR) {
            2
        } else {
            1
        };

        let mut fg = palette.resolve(cell.fg, content.colors);
        let mut bg = palette.resolve(cell.bg, content.colors);
        if cell.flags.contains(Flags::DIM) {
            fg = dim(fg);
        }
        if cell.flags.contains(Flags::INVERSE) {
            std::mem::swap(&mut fg, &mut bg);
        }
        if cell.flags.contains(Flags::HIDDEN) {
            fg = bg;
        }
        if content.selection.is_some_and(|s| s.contains(indexed.point)) {
            fg = palette.selection_fg;
            bg = palette.selection_bg;
        }
        if cursor == Some(at) && cursor_shape == CursorShape::Block {
            (fg, bg) = (bg, palette.color(258, content.colors));
        }

        if bg != palette.bg {
            let run = Run {
                row,
                col,
                width,
                color: bg,
                text: String::new(),
                underline: false,
            };
            push(&mut backgrounds, run, true);
        }
        let underline = cell.flags.intersects(Flags::ALL_UNDERLINES);
        if cell.c != ' ' || underline {
            let mut text = cell.c.to_string();
            if let Some(extra) = cell.zerowidth() {
                text.extend(extra);
            }
            // Only ASCII is sure to be exactly one cell wide in egui's fonts,
            // so draw anything else on its own to keep columns aligned.
            let ascii = cell.c.is_ascii() && width == 1 && text.len() == 1;
            let run = Run {
                row,
                col,
                width,
                color: fg,
                text,
                underline,
            };
            push(&mut texts, run, ascii);
        }
    }

    for run in &backgrounds {
        painter.rect_filled(grid.cell_rect(run.col, run.row, run.width), 0.0, run.color);
    }
    for run in &texts {
        let rect = grid.cell_rect(run.col, run.row, run.width);
        painter.text(
            rect.left_top(),
            egui::Align2::LEFT_TOP,
            &run.text,
            font.clone(),
            run.color,
        );
        if run.underline {
            let y = rect.bottom() - 1.0;
            painter.line_segment(
                [pos2(rect.left(), y), pos2(rect.right(), y)],
                Stroke::new(1.0, run.color),
            );
        }
    }

    if let Some(at) = cursor {
        let rect = grid.cell_rect(at.column.0, at.line, 1);
        let color = palette.color(258, content.colors);
        let stroke = Stroke::new(1.0, color);
        match cursor_shape {
            CursorShape::Beam => {
                painter.line_segment(
                    [rect.left_top(), rect.left_bottom()],
                    Stroke::new(2.0, color),
                );
            }
            CursorShape::Underline => {
                painter.line_segment(
                    [rect.left_bottom(), rect.right_bottom()],
                    Stroke::new(2.0, color),
                );
            }
            CursorShape::HollowBlock => {
                painter.rect_stroke(rect.shrink(0.5), 0.0, stroke, egui::StrokeKind::Inside);
            }
            CursorShape::Block | CursorShape::Hidden => {}
        }
    }
}

#[cfg(test)]
mod tests {
    /// What's typed to follow onto a server reaches ssh as its own words,
    /// whatever's in the path, and the folder reaches the shell there whole.
    #[cfg(target_os = "linux")]
    #[test]
    fn following_onto_a_server_types_an_ssh_command() {
        let root = std::env::temp_dir().join(format!("egui-files-ssh-{}", std::process::id()));
        let dir = root.join("it's a \"dir\" $HOME");
        disk::create_dir_all(&dir).unwrap();
        // The user's shell over there: says where it started, and how.
        let shell = root.join("shell");
        disk::write(&shell, "#!/bin/sh\npwd\necho \"$@\"\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            disk::set_permissions(&shell, disk::Permissions::from_mode(0o755)).unwrap();
        }
        // A stand-in ssh says what it was given, then runs what's for over
        // there as ssh does: joined into one line for the shell there.
        let run = |folder: &str| {
            let command = ssh_command(Path::new(folder)).unwrap();
            let script = format!(
                "ssh() {{ printf '%s\\n' \"$@\"; while [ \"$1\" != -- ]; do shift; done; shift 2; cd; sh -c \"$*\"; }}; {command}"
            );
            let output = std::process::Command::new("/bin/sh")
                .args(["-c", &script])
                .env("SHELL", &shell)
                .env("HOME", &root)
                .env(ENTER_VAR, enter_command())
                .output()
                .unwrap();
            String::from_utf8(output.stdout).unwrap()
        };
        let output = run(&format!("sftp://me@host:2222{}", dir.display()));
        let lines: Vec<&str> = output.lines().collect();
        // Past the shared connection's socket, if there is one.
        let lines: Vec<&str> = match lines.iter().position(|l| *l == "-t") {
            Some(t) if lines.get(t + 1) == Some(&"-o") => {
                assert!(lines[t + 2].starts_with("ControlPath="));
                [&lines[..=t], &lines[t + 3..]].concat()
            }
            _ => lines,
        };
        assert_eq!(lines[..7], ["-t", "-p", "2222", "-l", "me", "--", "host"]);
        assert_eq!(lines[7], enter_command());
        let real = disk::canonicalize(&dir).unwrap();
        // Where it started, and as a login shell.
        assert_eq!(lines[lines.len() - 2..], [real.to_str().unwrap(), "-l"]);
        assert_eq!(ssh_command(Path::new("/local")), None);
        // Home, before it's known where that is.
        let home = |path| {
            run(path)
                .lines()
                .rev()
                .nth(1)
                .unwrap_or_default()
                .to_owned()
        };
        let root_real = disk::canonicalize(&root).unwrap();
        assert_eq!(home("sftp://h/~"), root_real.to_str().unwrap());
        let inside = format!("sftp://h/~/{}", dir.file_name().unwrap().to_str().unwrap());
        assert_eq!(home(&inside), real.to_str().unwrap());
        disk::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn shells_say_where_they_are_and_when_at_a_prompt() {
        let marks = std::sync::Mutex::new(Marks::default());
        let mut scanner = OscScanner::default();
        let state = |marks: &std::sync::Mutex<Marks>| {
            let marks = marks.lock().unwrap();
            (marks.prompt, marks.cwd.clone())
        };
        // In pieces, as reads split them, ended by BEL or ST.
        scanner.feed(b"output\x1b]7;file://box/home/me/a%20b", &marks);
        scanner.feed(b"\x07\x1b]133;A\x1b\\$ ", &marks);
        assert_eq!(
            state(&marks),
            (true, Some(("box".into(), "/home/me/a b".into())))
        );
        scanner.feed(b"\x1b]133;C\x07", &marks);
        assert!(!state(&marks).0);
        // Other escape codes, and ones cut short, change nothing.
        scanner.feed(b"\x1b[31mred\x1b]0;title\x07\x1b]133", &marks);
        scanner.feed(b"\x1bX", &marks);
        assert!(!state(&marks).0);
        scanner.feed(b"\x1b]133;B\x07", &marks);
        assert!(state(&marks).0);
        assert_eq!(percent_decode("/a%2Fb%zz%4"), "/a/b%zz%4");
    }

    /// bash and fish, given ENTER's hooks, mark their prompts and say where
    /// they are; other shells are left as they are.
    #[cfg(target_os = "linux")]
    #[test]
    fn enter_hooks_bash_and_fish() {
        for (shell, script) in [
            ("bash", "eval \"$PROMPT_COMMAND\""),
            ("fish", "emit fish_prompt"),
        ] {
            let Ok(path) = which(shell) else { continue };
            let root =
                std::env::temp_dir().join(format!("egui-files-{shell}-{}", std::process::id()));
            disk::create_dir_all(&root).unwrap();
            // As ENTER would start it, but running `script` rather than
            // waiting for input; the login shell's -l stays.
            let enter = ENTER.replace(
                "exec \"$s\" -l -C '",
                &format!("exec \"$s\" -l -c '{script}' -C '"),
            );
            let enter = enter.replace(
                "export PROMPT_COMMAND; exec \"$s\" -l",
                &format!("export PROMPT_COMMAND; exec \"$s\" -c '{script}'"),
            );
            let output = std::process::Command::new("/bin/sh")
                .args(["-c", &enter, "sh", root.to_str().unwrap()])
                .env("SHELL", &path)
                .env("HOSTNAME", "box")
                .output()
                .unwrap();
            let marks = std::sync::Mutex::new(Marks::default());
            OscScanner::default().feed(&output.stdout, &marks);
            let marks = marks.lock().unwrap();
            let real = disk::canonicalize(&root).unwrap();
            assert!(marks.prompt, "{shell}: {output:?}");
            assert_eq!(
                marks.cwd.as_ref().map(|(_, cwd)| cwd.as_str()),
                Some(real.to_str().unwrap()),
                "{shell}"
            );
            disk::remove_dir_all(root).unwrap();
        }
    }

    #[cfg(target_os = "linux")]
    fn which(program: &str) -> std::io::Result<PathBuf> {
        let output = std::process::Command::new("sh")
            .args(["-c", &format!("command -v {program}")])
            .output()?;
        let path = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if path.is_empty() {
            return Err(std::io::ErrorKind::NotFound.into());
        }
        Ok(PathBuf::from(path))
    }

    use super::*;

    #[test]
    fn sticky_modifiers_apply_to_typed_text() {
        let ctrl = Modifiers::CTRL;
        assert_eq!(sticky_text("c", ctrl), [0x03]);
        assert_eq!(sticky_text("C", ctrl), [0x03]);
        assert_eq!(sticky_text("[", ctrl), [0x1b]);
        assert_eq!(sticky_text("x", Modifiers::ALT), [0x1b, b'x']);
        assert_eq!(sticky_text("d", Modifiers::ALT | ctrl), [0x1b, 0x04]);
        // Nothing to apply, or more than one character: as typed.
        assert_eq!(sticky_text("c", Modifiers::NONE), b"c");
        assert_eq!(sticky_text("ab", ctrl), b"ab");
        assert!(key_types_text(Key::A));
        assert!(!key_types_text(Key::Enter));
    }

    #[test]
    fn encodes_keys() {
        let none = TermMode::empty();
        assert_eq!(key_bytes(Key::Enter, Modifiers::NONE, none).unwrap(), b"\r");
        assert_eq!(key_bytes(Key::C, Modifiers::CTRL, none).unwrap(), [3]);
        assert_eq!(key_bytes(Key::B, Modifiers::ALT, none).unwrap(), b"\x1bb");
        assert_eq!(
            key_bytes(Key::ArrowUp, Modifiers::NONE, none).unwrap(),
            b"\x1b[A"
        );
        assert_eq!(
            key_bytes(Key::ArrowUp, Modifiers::NONE, TermMode::APP_CURSOR).unwrap(),
            b"\x1bOA"
        );
        assert_eq!(
            key_bytes(Key::ArrowLeft, Modifiers::CTRL, none).unwrap(),
            b"\x1b[1;5D"
        );
        assert_eq!(
            key_bytes(Key::Delete, Modifiers::NONE, none).unwrap(),
            b"\x1b[3~"
        );
        assert_eq!(
            key_bytes(Key::F5, Modifiers::SHIFT, none).unwrap(),
            b"\x1b[15;2~"
        );
        // Plain letters arrive as text events.
        assert_eq!(key_bytes(Key::A, Modifiers::NONE, none), None);
    }

    #[test]
    fn brackets_pastes() {
        assert_eq!(paste_bytes("a\nb", TermMode::empty()), b"a\rb");
        assert_eq!(
            paste_bytes("x\x1b[201~y", TermMode::BRACKETED_PASTE),
            b"\x1b[200~x[201~y\x1b[201~"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn quotes_paths() {
        assert_eq!(shell_quote(Path::new("/a b/it's")), r"'/a b/it'\''s'");
    }

    /// Runs a real shell in a PTY, then follows a folder change both ways.
    #[cfg(target_os = "linux")]
    #[test]
    fn shell_follows_folder() {
        let ctx = Context::default();
        let dir = std::env::temp_dir();
        let size = TermSize {
            columns: 80,
            lines: 24,
        };
        // Not the user's shell, whose setup may prompt for things.
        let sh = tty::Shell::new("/bin/sh".into(), Vec::new());
        let mut panel = Terminal {
            session: Some(
                Session::spawn(&ctx, Some(&dir), Some(sh), size, vec2(8.0, 16.0)).unwrap(),
            ),
            follow: true,
            synced: Some(dir.clone()),
            ..Default::default()
        };
        // Calls `follow` until `done` holds or it asks to navigate.
        let wait = |panel: &mut Terminal, folder: &Path, done: &dyn Fn(&Session) -> bool| {
            let start = web_time::Instant::now();
            loop {
                let t = start.elapsed().as_secs_f64();
                // Answer the shell's terminal queries, as the UI does each frame.
                let palette = Palette::new(&ctx);
                panel
                    .session
                    .as_mut()
                    .unwrap()
                    .process_events(&ctx, &palette);
                let nav = panel.follow(t, folder);
                if nav.is_some() || done(panel.session.as_ref().unwrap()) || t > 10.0 {
                    return nav;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        };
        let real = disk::canonicalize(&dir).unwrap();
        let cwd = |s: &Session| s.shell_cwd();
        // Wait for the prompt.
        let prompt = |s: &Session| s.term.lock().grid().cursor.point != Point::default();
        assert_eq!(wait(&mut panel, &dir, &prompt), None);
        assert!(prompt(panel.session.as_ref().unwrap()), "no prompt");
        assert_eq!(cwd(panel.session.as_ref().unwrap()), Some(real.clone()));

        // The tab moves to / and the shell follows.
        assert_eq!(
            wait(&mut panel, Path::new("/"), &|s| cwd(s) == Some("/".into())),
            None
        );
        assert_eq!(cwd(panel.session.as_ref().unwrap()), Some("/".into()));

        // cd in the shell asks the tab to follow.
        panel
            .session
            .as_ref()
            .unwrap()
            .write(format!("cd {}\r", real.display()).into_bytes());
        let nav = wait(&mut panel, Path::new("/"), &|_| false);
        assert_eq!(nav, Some(real.clone()));

        // Turned off, the shell stays put when the tab moves.
        panel.set_follow(false);
        panel.session.as_ref().unwrap().write(b"cd /\r".to_vec());
        let at_root = |s: &Session| cwd(s) == Some("/".into());
        let start = web_time::Instant::now();
        while !at_root(panel.session.as_ref().unwrap()) {
            assert!(start.elapsed().as_secs() < 10, "shell didn't cd");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        // Turned back on, the shell goes to the tab's folder rather than
        // taking the tab to its own.
        panel.set_follow(true);
        assert_eq!(
            wait(&mut panel, &dir, &|s| cwd(s) == Some(real.clone())),
            None
        );
        assert_eq!(cwd(panel.session.as_ref().unwrap()), Some(real));
    }

    /// Types into a focused panel through egui events, and checks the shell
    /// gets it and that the keys don't leak to the rest of the app.
    #[cfg(target_os = "linux")]
    #[test]
    fn typing_reaches_shell() {
        let ctx = Context::default();
        let size = TermSize {
            columns: 80,
            lines: 24,
        };
        let sh = tty::Shell::new("/bin/sh".into(), Vec::new());
        let mut panel = Terminal {
            focus_requested: true,
            session: Some(Session::spawn(&ctx, None, Some(sh), size, vec2(8.0, 16.0)).unwrap()),
            ..Default::default()
        };
        let frame = |panel: &mut Terminal, events: Vec<egui::Event>| {
            let input = egui::RawInput {
                events,
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(800.0, 400.0))),
                ..Default::default()
            };
            let mut leaked = Vec::new();
            let mut output = ctx.run_ui(input, |ui| {
                panel.ui(ui, None, FollowState::default());
                leaked = ui.input(|i| i.events.clone());
            });
            output.textures_delta.clear();
            leaked
        };
        let screen = |panel: &Terminal| {
            let term = panel.session.as_ref().unwrap().term.lock();
            let grid = term.grid();
            (0..grid.screen_lines())
                .map(|l| {
                    let row = &grid[alacritty_terminal::index::Line(l as i32)];
                    (0..grid.columns())
                        .map(|c| row[Column(c)].c)
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
        };

        frame(&mut panel, Vec::new());
        let key = |key, modifiers| egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        };
        let leaked = frame(
            &mut panel,
            vec![
                egui::Event::Text("echo one; echo two".into()),
                key(Key::W, Modifiers::CTRL),
                egui::Event::Text("three".into()),
                key(Key::Enter, Modifiers::NONE),
            ],
        );
        assert!(leaked.is_empty(), "{leaked:?}");

        // Ctrl+W deleted "two", so the shell printed "one" then "three".
        let start = web_time::Instant::now();
        loop {
            frame(&mut panel, Vec::new());
            let lines = screen(&panel);
            let trimmed: Vec<_> = lines.iter().map(|l| l.trim_end()).collect();
            if let Some(i) = trimmed.iter().position(|l| *l == "one") {
                assert_eq!(trimmed.get(i + 1), Some(&"three"), "{trimmed:#?}");
                break;
            }
            assert!(start.elapsed().as_secs() < 10, "{trimmed:#?}");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
}
