//! Dragging files out to other apps on X11, with the XDND protocol.
//!
//! winit only implements the receiving side of XDND (drops in arrive as
//! `dropped_files`), so this is the source side, on its own X connection and
//! thread:
//!
//! - The UI calls [`X11Dnd::start_drag`] once an in-app drag leaves the
//!   window. egui keeps its own drag going meanwhile, so coming back into one
//!   of our windows just continues it, and dropping there is handled in-app.
//! - The thread polls the pointer instead of grabbing it: winit already holds
//!   the implicit grab from the button press, which another connection can't
//!   take, and it keeps egui seeing the button release.
//! - Over another app's XDND-aware window it sends enter/position/leave,
//!   and on release a drop, then serves the files as a `text/uri-list` from
//!   the `XdndSelection` until the target says it has finished.

use std::{
    path::PathBuf,
    sync::mpsc::{self, Receiver, RecvTimeoutError, Sender},
    time::Duration,
};

use web_time::Instant;

use raw_window_handle::{HasDisplayHandle, RawDisplayHandle};
use x11rb::{
    connection::{Connection, RequestConnection as _},
    protocol::{
        Event,
        xproto::{
            Atom, AtomEnum, ClientMessageEvent, ConnectionExt as _, CreateWindowAux, EventMask,
            KeyButMask, PropMode, SelectionNotifyEvent, SelectionRequestEvent, Window, WindowClass,
        },
    },
    rust_connection::RustConnection,
    wrapper::ConnectionExt as _,
};

use crate::dnd::uri_list;

/// The XDND version we speak.
const VERSION: u32 = 5;
/// How often the pointer is checked while dragging.
const POLL: Duration = Duration::from_millis(15);
/// How long a target may take to fetch the files after the drop.
const FINISH_TIMEOUT: Duration = Duration::from_secs(30);

x11rb::atom_manager! {
    Atoms: AtomsCookie {
        XdndAware,
        XdndProxy,
        XdndSelection,
        XdndEnter,
        XdndPosition,
        XdndStatus,
        XdndLeave,
        XdndDrop,
        XdndFinished,
        XdndActionCopy,
        TARGETS,
        _NET_WM_PID,
        TEXT_URI_LIST: b"text/uri-list",
        EGUI_FILES_TIME: b"_EGUI_FILES_TIME",
    }
}

enum Command {
    Start(Vec<PathBuf>),
    Cancel,
}

pub struct X11Dnd {
    commands: Sender<Command>,
    ended: Receiver<()>,
}

impl X11Dnd {
    /// Connects to the X server, or returns `None` when not running on X11.
    pub fn new(cc: &eframe::CreationContext<'_>) -> Option<Self> {
        let display = cc.display_handle().ok()?;
        if !matches!(
            display.as_raw(),
            RawDisplayHandle::Xlib(_) | RawDisplayHandle::Xcb(_)
        ) {
            return None;
        }
        // winit connected to `$DISPLAY` too.
        let source = Source::connect()
            .inspect_err(|err| eprintln!("X11 drag and drop: {err}"))
            .ok()?;
        let (commands, receiver) = mpsc::channel();
        let (sender, ended) = mpsc::channel();
        let ctx = cc.egui_ctx.clone();
        std::thread::Builder::new()
            .name("x11-dnd".to_owned())
            .spawn(move || source.run(&receiver, &sender, &ctx))
            .ok()?;
        Some(Self { commands, ended })
    }

    /// Hands an in-app drag to other apps. Must be called while the mouse
    /// button that started the drag is held.
    pub fn start_drag(&self, paths: Vec<PathBuf>) -> bool {
        !paths.is_empty() && self.commands.send(Command::Start(paths)).is_ok()
    }

    /// Abandons the drag (Escape), telling the app under the pointer.
    pub fn cancel(&self) {
        let _ = self.commands.send(Command::Cancel);
    }

    /// Whether a drag started with [`Self::start_drag`] has ended since the
    /// last call.
    pub fn ended(&self) -> bool {
        self.ended.try_iter().count() > 0
    }
}

/// An XDND-aware window under the pointer.
#[derive(Clone, Copy, PartialEq, Debug)]
struct Target {
    /// The window the messages are about.
    window: Window,
    /// Where they are sent: `window`, or its `XdndProxy`.
    proxy: Window,
    version: u32,
}

#[derive(Default)]
enum State {
    #[default]
    Idle,
    Dragging(Drag),
    /// Dropped; serving the files until the target finishes.
    Dropped {
        target: Target,
        paths: Vec<PathBuf>,
        deadline: Instant,
    },
}

struct Drag {
    paths: Vec<PathBuf>,
    target: Option<Target>,
    /// The target accepts the drop where the pointer was last reported.
    accepted: bool,
    /// A position was sent and its status hasn't come back yet.
    awaiting_status: bool,
    /// The last position sent, and the one to send once the status arrives.
    sent: Option<(i16, i16)>,
    pointer: (i16, i16),
    time: u32,
}

struct Source {
    conn: RustConnection,
    atoms: Atoms,
    root: Window,
    window: Window,
    state: State,
}

impl Source {
    fn connect() -> Result<Self, Box<dyn std::error::Error>> {
        let (conn, screen) = x11rb::connect(None)?;
        let atoms = Atoms::new(&conn)?.reply()?;
        let root = conn.setup().roots[screen].root;
        // Never mapped: it owns the selection and receives the replies.
        let window = conn.generate_id()?;
        conn.create_window(
            0,
            window,
            root,
            -1,
            -1,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            0,
            &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )?;
        conn.flush()?;
        Ok(Self {
            conn,
            atoms,
            root,
            window,
            state: State::Idle,
        })
    }

    fn run(mut self, commands: &Receiver<Command>, ended: &Sender<()>, ctx: &egui::Context) {
        loop {
            let busy = !matches!(self.state, State::Idle);
            let command = if busy {
                match commands.recv_timeout(POLL) {
                    Ok(command) => Some(command),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            } else {
                // Requests that arrived while idle wait here, which is fine:
                // nothing is offered then.
                match commands.recv() {
                    Ok(command) => Some(command),
                    Err(_) => return,
                }
            };
            let was_dragging = matches!(self.state, State::Dragging(_));
            let result = self.step(command);
            if let Err(err) = &result {
                eprintln!("X11 drag and drop: {err}");
                self.state = State::Idle;
            }
            if was_dragging && !matches!(self.state, State::Dragging(_)) {
                let _ = ended.send(());
                ctx.request_repaint();
            }
            if result.is_err() && self.conn.flush().is_err() {
                // The X server is gone.
                return;
            }
        }
    }

    fn step(&mut self, command: Option<Command>) -> Result<(), Box<dyn std::error::Error>> {
        match command {
            Some(Command::Start(paths)) => {
                if let State::Dragging(drag) = &self.state
                    && let Some(target) = drag.target
                {
                    self.send(target, self.atoms.XdndLeave, [0; 4])?;
                }
                let time = self.server_time()?;
                self.conn
                    .set_selection_owner(self.window, self.atoms.XdndSelection, time)?;
                self.state = State::Dragging(Drag {
                    paths,
                    target: None,
                    accepted: false,
                    awaiting_status: false,
                    sent: None,
                    pointer: (0, 0),
                    time,
                });
            }
            Some(Command::Cancel) => {
                if let State::Dragging(drag) = std::mem::take(&mut self.state)
                    && let Some(target) = drag.target
                {
                    self.send(target, self.atoms.XdndLeave, [0; 4])?;
                }
            }
            None => {}
        }
        while let Some(event) = self.conn.poll_for_event()? {
            self.handle_event(event)?;
        }
        match &mut self.state {
            State::Dragging(_) => self.track_pointer()?,
            State::Dropped { deadline, .. } if Instant::now() > *deadline => {
                self.state = State::Idle;
            }
            _ => {}
        }
        self.conn.flush()?;
        Ok(())
    }

    /// Follows the pointer from one target to the next, and drops on release.
    fn track_pointer(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let pointer = self.conn.query_pointer(self.root)?.reply()?;
        let released = !pointer.mask.contains(KeyButMask::BUTTON1);
        let target = self.target_at()?;
        let State::Dragging(drag) = &mut self.state else {
            return Ok(());
        };
        drag.pointer = (pointer.root_x, pointer.root_y);
        let (old, accepted, time) = (drag.target, drag.accepted, drag.time);

        if old != target {
            if let Some(old) = old {
                self.send(old, self.atoms.XdndLeave, [0; 4])?;
            }
            if let Some(new) = target {
                let version = new.version.min(VERSION);
                self.send(
                    new,
                    self.atoms.XdndEnter,
                    [version << 24, self.atoms.TEXT_URI_LIST, 0, 0],
                )?;
            }
            let State::Dragging(drag) = &mut self.state else {
                unreachable!()
            };
            drag.target = target;
            drag.accepted = false;
            drag.awaiting_status = false;
            drag.sent = None;
        }

        if released {
            let state = std::mem::take(&mut self.state);
            let State::Dragging(drag) = state else {
                unreachable!()
            };
            match target {
                // Dropping where the target said no to is a leave.
                Some(target) if old == Some(target) && accepted => {
                    self.send(target, self.atoms.XdndDrop, [0, time, 0, 0])?;
                    self.state = State::Dropped {
                        target,
                        paths: drag.paths,
                        deadline: Instant::now() + FINISH_TIMEOUT,
                    };
                }
                Some(target) => self.send(target, self.atoms.XdndLeave, [0; 4])?,
                None => {}
            }
            return Ok(());
        }
        self.send_position()
    }

    /// Sends the pointer's position to the target, unless it hasn't answered
    /// the last one yet (the spec asks sources to wait).
    fn send_position(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let State::Dragging(drag) = &mut self.state else {
            return Ok(());
        };
        let Some(target) = drag.target else {
            return Ok(());
        };
        if drag.awaiting_status || drag.sent == Some(drag.pointer) {
            return Ok(());
        }
        drag.awaiting_status = true;
        drag.sent = Some(drag.pointer);
        let (x, y) = drag.pointer;
        let position = (u32::from(x as u16) << 16) | u32::from(y as u16);
        let time = drag.time;
        self.send(
            target,
            self.atoms.XdndPosition,
            [0, position, time, self.atoms.XdndActionCopy],
        )
    }

    fn handle_event(&mut self, event: Event) -> Result<(), Box<dyn std::error::Error>> {
        match event {
            Event::ClientMessage(message) if message.format == 32 => {
                let data = message.data.as_data32();
                if message.type_ == self.atoms.XdndStatus {
                    if let State::Dragging(drag) = &mut self.state
                        && drag.target.is_some_and(|t| t.window == data[0])
                    {
                        drag.accepted = data[1] & 1 != 0;
                        drag.awaiting_status = false;
                        // Catch up with where the pointer went meanwhile.
                        self.send_position()?;
                    }
                } else if message.type_ == self.atoms.XdndFinished
                    && let State::Dropped { target, .. } = &self.state
                    && target.window == data[0]
                {
                    self.state = State::Idle;
                }
            }
            Event::SelectionRequest(request) => self.answer(&request)?,
            _ => {}
        }
        Ok(())
    }

    /// Hands the dragged files to an app that asked for them.
    fn answer(&self, request: &SelectionRequestEvent) -> Result<(), Box<dyn std::error::Error>> {
        let paths = match &self.state {
            State::Dragging(drag) => Some(&drag.paths),
            State::Dropped { paths, .. } => Some(paths),
            State::Idle => None,
        };
        // Old clients leave the property empty and mean the target.
        let property = if request.property == x11rb::NONE {
            request.target
        } else {
            request.property
        };
        let mut reply = x11rb::NONE;
        if let Some(paths) = paths.filter(|_| request.selection == self.atoms.XdndSelection) {
            if request.target == self.atoms.TARGETS {
                self.conn.change_property32(
                    PropMode::REPLACE,
                    request.requestor,
                    property,
                    AtomEnum::ATOM,
                    &[self.atoms.TARGETS, self.atoms.TEXT_URI_LIST],
                )?;
                reply = property;
            } else if request.target == self.atoms.TEXT_URI_LIST {
                let list = uri_list(paths);
                // No INCR transfers: a list this long is thousands of files.
                if list.len() < self.conn.maximum_request_bytes() - 64 {
                    self.conn.change_property8(
                        PropMode::REPLACE,
                        request.requestor,
                        property,
                        self.atoms.TEXT_URI_LIST,
                        list.as_bytes(),
                    )?;
                    reply = property;
                }
            }
        }
        self.conn.send_event(
            false,
            request.requestor,
            EventMask::NO_EVENT,
            SelectionNotifyEvent {
                response_type: x11rb::protocol::xproto::SELECTION_NOTIFY_EVENT,
                sequence: 0,
                time: request.time,
                requestor: request.requestor,
                selection: request.selection,
                target: request.target,
                property: reply,
            },
        )?;
        Ok(())
    }

    /// Sends an XDND message about `target`, from our window.
    fn send(
        &self,
        target: Target,
        kind: Atom,
        rest: [u32; 4],
    ) -> Result<(), Box<dyn std::error::Error>> {
        let [a, b, c, d] = rest;
        let message = ClientMessageEvent::new(32, target.window, kind, [self.window, a, b, c, d]);
        self.conn
            .send_event(false, target.proxy, EventMask::NO_EVENT, message)?;
        Ok(())
    }

    /// The XDND-aware window under the pointer, if it belongs to another app.
    ///
    /// Window managers wrap apps' windows in frames, so this walks down from
    /// the root to the first window that says it takes drops.
    fn target_at(&self) -> Result<Option<Target>, Box<dyn std::error::Error>> {
        let mut window = self.root;
        loop {
            let child = self.conn.query_pointer(window)?.reply()?.child;
            if child == x11rb::NONE {
                return Ok(None);
            }
            if let Some(target) = self.aware(child)? {
                return Ok((!self.is_ours(child)?).then_some(target));
            }
            window = child;
        }
    }

    /// `window` as a target, if it (or the proxy it names) is XDND-aware.
    fn aware(&self, window: Window) -> Result<Option<Target>, Box<dyn std::error::Error>> {
        let proxy = self
            .atom_property(window, self.atoms.XdndProxy, AtomEnum::WINDOW.into())?
            // A valid proxy names itself as its proxy.
            .filter(|&proxy| {
                self.atom_property(proxy, self.atoms.XdndProxy, AtomEnum::WINDOW.into())
                    .ok()
                    .flatten()
                    == Some(proxy)
            });
        let version = self.atom_property(
            proxy.unwrap_or(window),
            self.atoms.XdndAware,
            AtomEnum::ATOM.into(),
        )?;
        Ok(version.filter(|&v| v >= 3).map(|version| Target {
            window,
            proxy: proxy.unwrap_or(window),
            version,
        }))
    }

    /// One of the app's own windows, where egui handles the drop itself.
    fn is_ours(&self, window: Window) -> Result<bool, Box<dyn std::error::Error>> {
        let pid = self.atom_property(window, self.atoms._NET_WM_PID, AtomEnum::CARDINAL.into())?;
        Ok(pid == Some(std::process::id()))
    }

    /// The first 32-bit value of a property, if it is set with that type.
    fn atom_property(
        &self,
        window: Window,
        property: Atom,
        kind: Atom,
    ) -> Result<Option<u32>, Box<dyn std::error::Error>> {
        // The window may be gone by now; that's no error for us.
        let Ok(reply) = self
            .conn
            .get_property(false, window, property, kind, 0, 1)?
            .reply()
        else {
            return Ok(None);
        };
        Ok(reply.value32().and_then(|mut values| values.next()))
    }

    /// The X server's current time, which owning a selection asks for: the
    /// timestamp of a change to a property on our own window.
    fn server_time(&mut self) -> Result<u32, Box<dyn std::error::Error>> {
        self.conn.change_property8(
            PropMode::APPEND,
            self.window,
            self.atoms.EGUI_FILES_TIME,
            AtomEnum::STRING,
            &[],
        )?;
        self.conn.flush()?;
        loop {
            match self.conn.wait_for_event()? {
                Event::PropertyNotify(event) if event.window == self.window => {
                    return Ok(event.time);
                }
                other => self.handle_event(other)?,
            }
        }
    }
}
