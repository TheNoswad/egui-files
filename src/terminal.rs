//! A terminal running the user's shell, shown in the terminal panel.
//!
//! The terminal emulation and the PTY come from `alacritty_terminal`; this
//! module draws the grid with egui and translates input. On Linux a terminal
//! set to follow the focused folder `cd`s along while it sits at a prompt, and
//! `cd` in its shell navigates the focused tab.

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
use egui_dock::Split;
use serde::{Deserialize, Serialize};

/// Seconds to wait for the shell to report the folder it was sent to.
#[cfg(target_os = "linux")]
const CD_TIMEOUT: f64 = 2.0;

/// What the panel should do after a terminal was drawn.
#[derive(Default)]
pub struct TerminalResponse {
    /// The shell changed folder; show it in the focused tab.
    pub navigate: Option<PathBuf>,
    /// The shell exited or the user closed the terminal.
    pub close: bool,
    /// Open another terminal next to this one, or in a new tab when `None`.
    pub open: Option<Option<Split>>,
    /// Change which terminals follow the focused folder.
    pub follow: Option<Follow>,
}

/// A change to which terminals follow the focused folder.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Follow {
    /// This terminal follows, or stops.
    This(bool),
    /// Every terminal follows, or none does.
    All(bool),
    /// Terminals opened from now on follow.
    New(bool),
}

/// The panel-wide side of folder following, for the menus.
#[derive(Clone, Copy, Default)]
pub struct FollowState {
    /// Every terminal follows.
    pub all: bool,
    /// New terminals follow.
    pub new: bool,
}

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
    #[serde(skip)]
    pending_cd: Option<(PathBuf, f64)>,
    /// A key already sent with Alt; drop the text event that follows it.
    #[serde(skip)]
    skip_text: bool,
    /// Scroll-wheel movement not yet turned into whole lines.
    #[serde(skip)]
    scroll_acc: f32,
    /// The last cell reported to a mouse-aware program while dragging.
    #[serde(skip)]
    last_mouse_cell: Option<(usize, usize)>,
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
        let font = TextStyle::Monospace.resolve(ui.style());
        let cell = ui
            .ctx()
            .fonts_mut(|f| vec2(f.glyph_width(&font, 'M'), f.row_height(&font)));
        let rect = ui.available_rect_before_wrap();
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
            let start = saved.as_deref().or(folder);
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

        let palette = Palette::new(ui.visuals());
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
    fn keyboard(&mut self, ctx: &Context) {
        let Some(session) = &self.session else {
            return;
        };
        let mode = *session.term.lock().mode();
        let mut out = Vec::new();
        let mut copy = false;
        let mut scroll = None;
        ctx.input_mut(|i| {
            let held = i.modifiers;
            i.events.retain(|event| {
                match event {
                    egui::Event::Text(text) => {
                        if !std::mem::take(&mut self.skip_text) {
                            out.extend_from_slice(text.as_bytes());
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

        if widget.hovered() {
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
            } else if widget.drag_started_by(PointerButton::Primary) {
                term.selection = select(SelectionType::Simple);
            } else if widget.dragged_by(PointerButton::Primary) {
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
        if !session.shell_idle() {
            return None;
        }
        let cwd = session.shell_cwd()?;
        // The shell reports its folder with symlinks resolved.
        let real = std::fs::canonicalize(folder).unwrap_or_else(|_| folder.to_path_buf());
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

/// The shell to run: the user's login shell, except in tests, where its
/// setup may prompt for things.
fn user_shell() -> Option<tty::Shell> {
    cfg!(test).then(|| tty::Shell::new("/bin/sh".into(), Vec::new()))
}

/// Buttons to open another terminal.
pub fn open_menu(ui: &mut Ui, response: &mut TerminalResponse) {
    for (label, shortcut, open) in [
        ("New tab", "Ctrl+Shift+T", None),
        ("Split right", "Ctrl+Shift+E", Some(Split::Right)),
        ("Split below", "Ctrl+Shift+O", Some(Split::Below)),
    ] {
        if ui
            .add(egui::Button::new(label).shortcut_text(shortcut))
            .clicked()
        {
            response.open = Some(open);
            ui.close();
        }
    }
}

/// Menu items for folder following; `follow` is this terminal's setting.
pub fn follow_menu(ui: &mut Ui, follow: bool, state: FollowState) -> Option<Follow> {
    let mut change = None;
    let mut this = follow;
    if ui
        .checkbox(&mut this, "Follow folder")
        .on_hover_text(
            "Keep the shell in the focused tab's folder, and the tab in the shell's (Ctrl+Shift+L)",
        )
        .changed()
    {
        change = Some(Follow::This(this));
    }
    let all = if state.all {
        "No terminal follows"
    } else {
        "All terminals follow"
    };
    if ui
        .add(egui::Button::new(all).shortcut_text("Ctrl+Shift+Alt+L"))
        .clicked()
    {
        change = Some(Follow::All(!state.all));
    }
    let mut new = state.new;
    if ui
        .checkbox(&mut new, "New terminals follow")
        .on_hover_text("Terminals opened from now on start out following the folder")
        .changed()
    {
        change = Some(Follow::New(new));
    }
    if change.is_some() {
        ui.close();
    }
    change
}

#[cfg(target_os = "linux")]
fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', r"'\''"))
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
        let env: HashMap<String, String> = [("TERM", "xterm-256color"), ("COLORTERM", "truecolor")]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
        let options = tty::Options {
            shell,
            working_directory: cwd.map(Path::to_path_buf),
            env,
            ..Default::default()
        };
        let pty = tty::new(&options, size.window_size(cell), 0)?;
        #[cfg(target_os = "linux")]
        let shell = (pty.child().id(), pty.file().try_clone()?);

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
        })
    }

    fn write(&self, bytes: impl Into<Cow<'static, [u8]>>) {
        let _ = self.sender.send(Msg::Input(bytes.into()));
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
        std::fs::read_link(format!("/proc/{}/cwd", self.shell.0)).ok()
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
    ansi: [Color32; 16],
    selection_bg: Color32,
    selection_fg: Color32,
}

const DARK_ANSI: [u32; 16] = [
    0x000000, 0xcd3131, 0x0dbc79, 0xe5e510, 0x2472c8, 0xbc3fbc, 0x11a8cd, 0xe5e5e5, //
    0x666666, 0xf14c4c, 0x23d18b, 0xf5f543, 0x3b8eea, 0xd670d6, 0x29b8db, 0xffffff,
];
const LIGHT_ANSI: [u32; 16] = [
    0x000000, 0xcd3131, 0x00a000, 0x949800, 0x0451a5, 0xbc05bc, 0x0598bc, 0x555555, //
    0x666666, 0xcd3131, 0x14ce14, 0xb5ba00, 0x0451a5, 0xbc05bc, 0x0598bc, 0xa5a5a5,
];

fn hex(rgb: u32) -> Color32 {
    let [_, r, g, b] = rgb.to_be_bytes();
    Color32::from_rgb(r, g, b)
}

fn dim(color: Color32) -> Color32 {
    let [r, g, b, _] = color.to_array();
    let f = |c: u8| (c as f32 * 0.66) as u8;
    Color32::from_rgb(f(r), f(g), f(b))
}

impl Palette {
    fn new(visuals: &egui::Visuals) -> Self {
        let ansi = if visuals.dark_mode {
            DARK_ANSI
        } else {
            LIGHT_ANSI
        };
        Self {
            fg: visuals.strong_text_color(),
            bg: visuals.extreme_bg_color,
            ansi: ansi.map(hex),
            selection_bg: visuals.selection.bg_fill,
            selection_fg: visuals.selection.stroke.color,
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
            256 | 258 | 267 => self.fg,
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
    use super::*;

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
            let start = std::time::Instant::now();
            loop {
                let t = start.elapsed().as_secs_f64();
                // Answer the shell's terminal queries, as the UI does each frame.
                let palette = Palette::new(&egui::Visuals::dark());
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
        let real = std::fs::canonicalize(&dir).unwrap();
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
        let start = std::time::Instant::now();
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
        let start = std::time::Instant::now();
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
