//! Native Wayland drag and drop with other apps.
//!
//! winit implements no Wayland data-device support, so this binds
//! `wl_data_device` on winit's own Wayland connection and dispatches it on a
//! background thread:
//!
//! - **Drops in:** enter/motion/leave/drop are forwarded to the UI as
//!   [`ExternalEvent`]s, and the dropped `text/uri-list` is read on drop.
//! - **Drags out:** when an in-app drag leaves the window, the UI calls
//!   [`WaylandDnd::start_drag`], which hands the drag to the compositor
//!   with a data source serving the paths as a `text/uri-list`.
//! - **Clipboard:** copied files are set as the selection here too, since
//!   egui's clipboard only offers text (other apps would paste the path).

use std::{
    io::{Read, Write},
    os::fd::{AsFd, FromRawFd},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU32, Ordering},
        mpsc::{self, Receiver, Sender},
    },
};

use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
use smithay_client_toolkit::{
    data_device_manager::{
        DataDeviceManagerState, WritePipe,
        data_device::{DataDevice, DataDeviceData, DataDeviceHandler},
        data_offer::{DataOfferHandler, DragOffer},
        data_source::{CopyPasteSource, DataSourceHandler, DragSource},
    },
    delegate_data_device, delegate_pointer, delegate_registry, delegate_seat,
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        Capability, SeatHandler, SeatState,
        pointer::{PointerEvent, PointerEventKind, PointerHandler},
    },
};
use wayland_backend::client::{Backend, ObjectId};
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle,
    globals::registry_queue_init,
    protocol::{
        wl_buffer::WlBuffer,
        wl_compositor::WlCompositor,
        wl_data_device::WlDataDevice,
        wl_data_device_manager::DndAction,
        wl_data_source::WlDataSource,
        wl_keyboard::{self, WlKeyboard},
        wl_pointer::WlPointer,
        wl_seat::WlSeat,
        wl_shm::{self, WlShm},
        wl_shm_pool::WlShmPool,
        wl_surface::WlSurface,
    },
};

use crate::drag_icon::DragIcon;

use crate::dnd::{ExternalEvent, parse_uri_list, uri_list};

const URI_LIST: &str = "text/uri-list";
/// GNOME and most GTK file managers: "copy" or "cut", then one URI per line.
const GNOME_FILES: &str = "x-special/gnome-copied-files";
/// KDE marks a cut with "1" in this extra format.
const KDE_CUT: &str = "application/x-kde-cutselection";
const TEXT: [&str; 5] = [
    "text/plain;charset=utf-8",
    "text/plain",
    "UTF8_STRING",
    "STRING",
    "TEXT",
];

/// What the UI receives, in surface-local logical coordinates.
pub enum DndEvent {
    External(ExternalEvent),
    /// A drag started with [`WaylandDnd::start_drag`] ended (dropped or cancelled).
    DragOutEnded,
}

/// Shared between the UI and the dispatch thread.
#[derive(Default)]
struct Shared {
    /// Serial of the last pointer button press, needed to start a drag.
    press_serial: AtomicU32,
    /// Serial of the last key or button event, needed to set the selection.
    input_serial: AtomicU32,
    /// Files on the clipboard, while our source is the selection.
    selection: Mutex<Option<Selection>>,
    /// Paths being dragged out, while a drag started here is in progress.
    outgoing: Mutex<Option<Vec<PathBuf>>>,
    source: Mutex<Option<DragSource>>,
    /// The drag icon's surface and buffer, kept alive while dragging out.
    icon: Mutex<Option<(WlSurface, WlBuffer)>>,
}

pub struct WaylandDnd {
    conn: Connection,
    qh: QueueHandle<State>,
    manager: DataDeviceManagerState,
    device: DataDevice,
    surface: WlSurface,
    /// For the drag icon; `None` if the compositor lacks them.
    compositor: Option<WlCompositor>,
    shm: Option<WlShm>,
    shared: Arc<Shared>,
    events: Receiver<DndEvent>,
}

impl WaylandDnd {
    /// Sets up drag and drop on the window's Wayland connection, or returns
    /// `None` when not running on Wayland or the compositor lacks support.
    pub fn new(cc: &eframe::CreationContext<'_>) -> Option<Self> {
        let RawDisplayHandle::Wayland(display) = cc.display_handle().ok()?.as_raw() else {
            return None;
        };
        let RawWindowHandle::Wayland(window) = cc.window_handle().ok()?.as_raw() else {
            return None;
        };
        // SAFETY: the display and surface pointers come from winit and stay
        // valid for the lifetime of the window, which outlives the app.
        let conn = Connection::from_backend(unsafe {
            Backend::from_foreign_display(display.display.as_ptr().cast())
        });
        let surface_id =
            unsafe { ObjectId::from_ptr(WlSurface::interface(), window.surface.as_ptr().cast()) }
                .ok()?;
        let surface = WlSurface::from_id(&conn, surface_id).ok()?;

        let (globals, mut queue): (_, EventQueue<State>) = registry_queue_init(&conn).ok()?;
        let qh = queue.handle();
        let manager = DataDeviceManagerState::bind(&globals, &qh).ok()?;
        // Version 5 forbids offsets in attach, which position the icon.
        let compositor = globals.bind(&qh, 1..=4, ()).ok();
        let shm = globals.bind(&qh, 1..=1, ()).ok();
        let (sender, events) = mpsc::channel();
        let shared = Arc::new(Shared::default());
        let mut state = State {
            registry: RegistryState::new(&globals),
            seats: SeatState::new(&globals, &qh),
            pointer: None,
            keyboard: None,
            sender,
            shared: shared.clone(),
            ctx: cc.egui_ctx.clone(),
            conn: conn.clone(),
            surface: surface.clone(),
        };
        // Learn about the seat (and get its pointer) before creating the device.
        queue.roundtrip(&mut state).ok()?;
        let seat = state.seats.seats().next()?;
        let device = manager.get_data_device(&qh, &seat);
        queue.roundtrip(&mut state).ok()?;

        std::thread::Builder::new()
            .name("wayland-dnd".to_owned())
            .spawn(move || while queue.blocking_dispatch(&mut state).is_ok() {})
            .ok()?;

        Some(Self {
            conn,
            qh,
            manager,
            device,
            surface,
            compositor,
            shm,
            shared,
            events,
        })
    }

    pub fn poll(&self) -> Vec<DndEvent> {
        self.events.try_iter().collect()
    }

    /// Hands an in-app drag to the compositor so other apps can receive it.
    /// Must be called while the mouse button that started the drag is held.
    /// `icon` follows the pointer, offset like the in-app drag label.
    pub fn start_drag(&self, paths: Vec<PathBuf>, icon: Option<DragIcon>) -> bool {
        let serial = self.shared.press_serial.load(Ordering::Acquire);
        if serial == 0 || paths.is_empty() {
            return false;
        }
        let source =
            self.manager
                .create_drag_and_drop_source(&self.qh, [URI_LIST], DndAction::Copy);
        let icon = icon.and_then(|icon| self.icon_surface(&icon));
        source.start_drag(
            &self.device,
            &self.surface,
            icon.as_ref().map(|i| &i.0),
            serial,
        );
        *self.shared.icon.lock().expect("icon poisoned") = icon;
        *self.shared.outgoing.lock().expect("outgoing poisoned") = Some(paths);
        *self.shared.source.lock().expect("source poisoned") = Some(source);
        self.conn.flush().is_ok()
    }

    /// A surface showing `icon`, to hand to the compositor as the drag icon.
    fn icon_surface(&self, icon: &DragIcon) -> Option<(WlSurface, WlBuffer)> {
        let (compositor, shm) = (self.compositor.as_ref()?, self.shm.as_ref()?);
        let stride = icon.width as i32 * 4;
        let len = icon.bgra.len();
        // SAFETY: memfd_create takes a NUL-terminated name and flags.
        let fd = unsafe { libc::memfd_create(c"egui-files-drag-icon".as_ptr(), libc::MFD_CLOEXEC) };
        if fd < 0 {
            return None;
        }
        // SAFETY: memfd_create returned a new descriptor we own.
        let mut file = disk::File::from(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) });
        file.write_all(&icon.bgra).ok()?;
        let pool = shm.create_pool(file.as_fd(), len as i32, &self.qh, ());
        let buffer = pool.create_buffer(
            0,
            icon.width as i32,
            icon.height as i32,
            stride,
            wl_shm::Format::Argb8888,
            &self.qh,
            (),
        );
        pool.destroy();
        let surface = compositor.create_surface(&self.qh, ());
        if surface.version() >= 3 {
            surface.set_buffer_scale(icon.scale);
        }
        // Down and right of the pointer, where the in-app label sits.
        surface.attach(Some(&buffer), 16, 16);
        surface.damage(0, 0, i32::MAX, i32::MAX);
        surface.commit();
        Some((surface, buffer))
    }

    /// Puts files on the clipboard. Returns false if there was no input event
    /// yet to attribute it to.
    pub fn set_selection(&self, paths: &[PathBuf], cut: bool) -> bool {
        let serial = self.shared.input_serial.load(Ordering::Acquire);
        if serial == 0 || paths.is_empty() {
            return false;
        }
        let mut mimes = vec![URI_LIST, GNOME_FILES];
        if cut {
            mimes.push(KDE_CUT);
        }
        mimes.extend(TEXT);
        let source = self.manager.create_copy_paste_source(&self.qh, mimes);
        let mut selection = self.shared.selection.lock().expect("selection poisoned");
        source.set_selection(&self.device, serial);
        // Replaces (and destroys) the previous source, if any.
        *selection = Some(Selection {
            source,
            paths: paths.to_vec(),
            cut,
        });
        self.conn.flush().is_ok()
    }
}

struct Selection {
    source: CopyPasteSource,
    paths: Vec<PathBuf>,
    cut: bool,
}

/// What the clipboard serves for `mime`, if it offers that type.
fn selection_data(paths: &[PathBuf], cut: bool, mime: &str) -> Option<String> {
    match mime {
        URI_LIST => Some(uri_list(paths)),
        GNOME_FILES => {
            let action = if cut { "cut" } else { "copy" };
            let uris = uri_list(paths);
            Some(
                std::iter::once(action)
                    .chain(uris.lines())
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
        }
        KDE_CUT => Some(if cut { "1" } else { "0" }.to_owned()),
        _ if TEXT.contains(&mime) => Some(crate::tab::join_paths(paths)),
        _ => None,
    }
}

struct State {
    registry: RegistryState,
    seats: SeatState,
    pointer: Option<WlPointer>,
    keyboard: Option<WlKeyboard>,
    sender: Sender<DndEvent>,
    shared: Arc<Shared>,
    ctx: egui::Context,
    conn: Connection,
    surface: WlSurface,
}

impl State {
    fn send(&self, event: ExternalEvent) {
        let _ = self.sender.send(DndEvent::External(event));
        self.ctx.request_repaint();
    }

    fn drag_offer(device: &WlDataDevice) -> Option<DragOffer> {
        device.data::<DataDeviceData>()?.drag_offer()
    }

    fn end_drag_out(&mut self) {
        self.shared
            .outgoing
            .lock()
            .expect("outgoing poisoned")
            .take();
        self.shared.source.lock().expect("source poisoned").take();
        if let Some((surface, buffer)) = self.shared.icon.lock().expect("icon poisoned").take() {
            surface.destroy();
            buffer.destroy();
        }
        let _ = self.sender.send(DndEvent::DragOutEnded);
        self.ctx.request_repaint();
    }
}

fn pos(x: f64, y: f64) -> egui::Pos2 {
    egui::pos2(x as f32, y as f32)
}

impl DataDeviceHandler for State {
    fn enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        device: &WlDataDevice,
        x: f64,
        y: f64,
        surface: &WlSurface,
    ) {
        let Some(offer) = Self::drag_offer(device) else {
            return;
        };
        if surface != &self.surface || !offer.with_mime_types(|m| m.iter().any(|m| m == URI_LIST)) {
            // Not files: refuse, so the source shows a "can't drop" cursor.
            offer.accept_mime_type(offer.serial, None);
            return;
        }
        offer.accept_mime_type(offer.serial, Some(URI_LIST.to_owned()));
        offer.set_actions(DndAction::Copy | DndAction::Move, DndAction::Copy);
        self.send(ExternalEvent::Hover(pos(x, y)));
    }

    fn leave(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, device: &WlDataDevice) {
        // After a drop, leave also arrives; the drop was already reported.
        if Self::drag_offer(device).is_none_or(|offer| !offer.dropped) {
            self.send(ExternalEvent::Leave);
        }
    }

    fn motion(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _device: &WlDataDevice,
        x: f64,
        y: f64,
    ) {
        self.send(ExternalEvent::Hover(pos(x, y)));
    }

    fn selection(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _device: &WlDataDevice) {}

    fn drop_performed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        device: &WlDataDevice,
    ) {
        let Some(offer) = Self::drag_offer(device) else {
            return;
        };
        let at = pos(offer.x, offer.y);

        // A drag that started here and came back: use the paths directly.
        let outgoing = self
            .shared
            .outgoing
            .lock()
            .expect("outgoing poisoned")
            .clone();
        if let Some(paths) = outgoing {
            offer.finish();
            offer.destroy();
            let _ = self.conn.flush();
            self.send(ExternalEvent::Drop {
                pos: at,
                paths,
                internal: true,
            });
            return;
        }

        let Ok(mut pipe) = offer.receive(URI_LIST.to_owned()) else {
            offer.destroy();
            return;
        };
        let _ = self.conn.flush();
        // Reading blocks until the other app has written everything, so do it
        // off the dispatch thread (which that app may be waiting on).
        let (sender, ctx, conn) = (self.sender.clone(), self.ctx.clone(), self.conn.clone());
        std::thread::spawn(move || {
            let mut text = String::new();
            let ok = pipe.read_to_string(&mut text).is_ok();
            if ok {
                offer.finish();
            }
            offer.destroy();
            let _ = conn.flush();
            let paths = parse_uri_list(&text);
            if ok && !paths.is_empty() {
                let _ = sender.send(DndEvent::External(ExternalEvent::Drop {
                    pos: at,
                    paths,
                    internal: false,
                }));
                ctx.request_repaint();
            }
        });
    }
}

impl DataOfferHandler for State {
    fn source_actions(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _offer: &mut DragOffer,
        _actions: DndAction,
    ) {
    }

    fn selected_action(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _offer: &mut DragOffer,
        _actions: DndAction,
    ) {
    }
}

impl DataSourceHandler for State {
    fn accept_mime(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _source: &WlDataSource,
        _mime: Option<String>,
    ) {
    }

    fn send_request(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        source: &WlDataSource,
        mime: String,
        mut fd: WritePipe,
    ) {
        let selection = self.shared.selection.lock().expect("selection poisoned");
        if let Some(selection) = selection.as_ref().filter(|s| s.source.inner() == source) {
            if let Some(data) = selection_data(&selection.paths, selection.cut, &mime) {
                // A slow reader would stall the dispatch thread on a full pipe.
                std::thread::spawn(move || {
                    let _ = fd.write_all(data.as_bytes());
                });
            }
            return;
        }
        drop(selection);
        if mime != URI_LIST {
            return;
        }
        let outgoing = self
            .shared
            .outgoing
            .lock()
            .expect("outgoing poisoned")
            .clone();
        if let Some(paths) = outgoing {
            let _ = fd.write_all(uri_list(&paths).as_bytes());
        }
    }

    fn cancelled(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, source: &WlDataSource) {
        let mut selection = self.shared.selection.lock().expect("selection poisoned");
        if selection
            .as_ref()
            .is_some_and(|s| s.source.inner() == source)
        {
            // Another selection replaced ours.
            selection.take();
            return;
        }
        drop(selection);
        self.end_drag_out();
    }

    fn dnd_dropped(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _source: &WlDataSource) {
    }

    fn dnd_finished(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _source: &WlDataSource,
    ) {
        self.end_drag_out();
    }

    fn action(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _source: &WlDataSource,
        _action: DndAction,
    ) {
    }
}

impl SeatHandler for State {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seats
    }

    fn new_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _seat: WlSeat) {}

    fn new_capability(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        seat: WlSeat,
        capability: Capability,
    ) {
        // Our own pointer object on the same seat, only to learn press serials.
        if capability == Capability::Pointer && self.pointer.is_none() {
            self.pointer = self.seats.get_pointer(qh, &seat).ok();
        }
        // And a keyboard, only to learn key serials for the selection.
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            self.keyboard = Some(seat.get_keyboard(qh, ()));
        }
    }

    fn remove_capability(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _seat: WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer
            && let Some(pointer) = self.pointer.take()
        {
            pointer.release();
        }
        if capability == Capability::Keyboard
            && let Some(keyboard) = self.keyboard.take()
        {
            keyboard.release();
        }
    }

    fn remove_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _seat: WlSeat) {}
}

impl PointerHandler for State {
    fn pointer_frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _pointer: &WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            if let PointerEventKind::Press { serial, .. } = event.kind {
                self.shared.press_serial.store(serial, Ordering::Release);
                self.shared.input_serial.store(serial, Ordering::Release);
            }
        }
    }
}

impl Dispatch<WlKeyboard, ()> for State {
    fn event(
        state: &mut Self,
        _keyboard: &WlKeyboard,
        event: wl_keyboard::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wl_keyboard::Event::Enter { serial, .. } | wl_keyboard::Event::Key { serial, .. } =
            event
        {
            state.shared.input_serial.store(serial, Ordering::Release);
        }
    }
}

/// The drag icon's objects send nothing that needs handling.
macro_rules! ignore_events {
    ($($proxy:ty),*) => {$(
        impl Dispatch<$proxy, ()> for State {
            fn event(
                _state: &mut Self,
                _proxy: &$proxy,
                _event: <$proxy as Proxy>::Event,
                _data: &(),
                _conn: &Connection,
                _qh: &QueueHandle<Self>,
            ) {
            }
        }
    )*};
}

ignore_events!(WlCompositor, WlShm, WlShmPool, WlBuffer, WlSurface);

impl ProvidesRegistryState for State {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }

    registry_handlers![SeatState];
}

delegate_registry!(State);
delegate_seat!(State);
delegate_pointer!(State);
delegate_data_device!(State);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipboard_formats() {
        let paths = [PathBuf::from("/tmp/a b"), PathBuf::from("/tmp/c")];
        let data = |cut, mime| selection_data(&paths, cut, mime);
        assert_eq!(
            data(false, URI_LIST).as_deref(),
            Some("file:///tmp/a%20b\r\nfile:///tmp/c\r\n")
        );
        assert_eq!(
            data(false, GNOME_FILES).as_deref(),
            Some("copy\nfile:///tmp/a%20b\nfile:///tmp/c")
        );
        assert_eq!(data(true, GNOME_FILES).unwrap().lines().next(), Some("cut"));
        assert_eq!(data(true, KDE_CUT).as_deref(), Some("1"));
        assert_eq!(
            data(false, "UTF8_STRING").as_deref(),
            Some("/tmp/a b\n/tmp/c")
        );
        assert_eq!(data(false, "image/png"), None);
    }
}
