//! Properties: everything about some files or folders, and changing it:
//! name, permissions or attributes, owner, times, link target, default app.
//! Shown in a tab, a floating window or a native window (a View menu
//! choice), and for the previewed item in the preview pane.

mod edit;
mod elevate;
mod hash;
mod info;
mod media;
mod platform;
mod view;
mod walk;

use std::{
    path::{Path, PathBuf},
    time::SystemTime,
};

use egui::{Id, Ui, ViewportBuilder, ViewportId};
use egui_phosphor::regular as icon;
use serde::{Deserialize, Serialize};

pub use elevate::run_helper;
use view::{Host, View};

use crate::tab::Shared;

/// Where Properties opens.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum OpenIn {
    #[default]
    Tab,
    Window,
    Native,
}

impl OpenIn {
    pub const ALL: [Self; 3] = [Self::Tab, Self::Window, Self::Native];

    pub fn label(self) -> &'static str {
        match self {
            Self::Tab => "A tab",
            Self::Window => "A floating window",
            Self::Native => "A separate window",
        }
    }
}

/// Saved settings.
#[derive(Default, Serialize, Deserialize)]
pub struct Prefs {
    #[serde(default)]
    pub open_in: OpenIn,
    /// Changes apply as they're made, without an Apply button.
    #[serde(default)]
    pub apply_immediately: bool,
}

/// Something in the trash, as listed there.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TrashedItem {
    pub name: String,
    pub original: PathBuf,
    pub deleted: Option<SystemTime>,
    /// Bytes, for files.
    pub size: Option<u64>,
    /// Items inside, for folders where the trash counts them.
    pub entries: Option<u64>,
    pub is_dir: bool,
}

/// An entry of an archive, as listed there.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ArchivedItem {
    /// `/`-separated path inside the archive.
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    pub packed: Option<u64>,
    pub modified: Option<SystemTime>,
    pub encrypted: bool,
}

/// What properties are shown for.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Source {
    Files(Vec<PathBuf>),
    Trash(Vec<TrashedItem>),
    Archive {
        archive: PathBuf,
        entries: Vec<ArchivedItem>,
    },
}

impl Source {
    pub fn len(&self) -> usize {
        match self {
            Source::Files(paths) => paths.len(),
            Source::Trash(items) => items.len(),
            Source::Archive { entries, .. } => entries.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// A short name: the item's, or how many there are.
    pub fn title(&self) -> String {
        let one = match self {
            Source::Files(paths) => paths.first().map(|p| crate::fs::display_name(p)),
            Source::Trash(items) => items.first().map(|i| i.name.clone()),
            Source::Archive { entries, .. } => entries
                .first()
                .map(|e| e.path.rsplit('/').next().unwrap_or(&e.path).to_owned()),
        };
        match (self.len(), one) {
            (1, Some(name)) => name,
            (n, _) => format!("{n} items"),
        }
    }
}

/// Properties in a dock tab.
#[derive(Serialize, Deserialize)]
pub struct PropertiesTab {
    pub id: u64,
    /// What's shown; also marks saved state as a properties tab.
    pub properties: Source,
    #[serde(skip)]
    view: Option<View>,
}

impl PropertiesTab {
    pub fn new(id: u64, source: Source) -> Self {
        Self {
            id,
            properties: source,
            view: None,
        }
    }

    pub fn title(&self) -> String {
        let source = self.view.as_ref().map_or(&self.properties, |v| &v.source);
        format!("{} {}", icon::INFO, source.title())
    }

    pub fn ui(&mut self, ui: &mut Ui, shared: &mut Shared) {
        let view = self
            .view
            .get_or_insert_with(|| View::new(self.properties.clone(), false));
        view.ui(ui, shared, Host::Tab);
        // A rename changes the paths; keep them for the next restart.
        self.properties.clone_from(&view.source);
    }
}

struct Window {
    id: u64,
    native: bool,
    view: View,
    /// Bring it to the front this frame.
    raise: bool,
}

/// Properties windows, and the preview pane's properties.
#[derive(Default)]
pub struct Properties {
    pub prefs: Prefs,
    windows: Vec<Window>,
    next_window: u64,
    preview: Option<View>,
}

impl Properties {
    pub fn new(prefs: Prefs) -> Self {
        Self {
            prefs,
            ..Self::default()
        }
    }

    /// Opens a window for `source`, or raises the one already showing it.
    pub fn open_window(&mut self, source: Source, native: bool) {
        if let Some(window) = self
            .windows
            .iter_mut()
            .find(|w| w.view.source == source && w.native == native)
        {
            window.raise = true;
            return;
        }
        self.next_window += 1;
        self.windows.push(Window {
            id: self.next_window,
            native,
            view: View::new(source, false),
            raise: true,
        });
    }

    /// Draws the open windows.
    pub fn windows_ui(ctx: &egui::Context, shared: &mut Shared) {
        let mut windows = std::mem::take(&mut shared.properties.windows);
        windows.retain_mut(|window| window_ui(ctx, window, shared));
        // Windows opened while drawing these go after them.
        windows.append(&mut shared.properties.windows);
        shared.properties.windows = windows;
    }

    /// The preview pane's properties for `path`, the same as elsewhere but
    /// with the bulkier sections closed. `buttons` go under the name.
    pub fn preview_ui(
        ui: &mut Ui,
        path: &Path,
        shared: &mut Shared,
        buttons: &mut dyn FnMut(&mut Ui),
    ) {
        let source = Source::Files(vec![path.to_path_buf()]);
        let mut view = shared
            .properties
            .preview
            .take()
            .filter(|v| v.source == source)
            .unwrap_or_else(|| View::new(source.clone(), true));
        view.ui(ui, shared, Host::Preview { buttons });
        if std::mem::take(&mut view.open_full) {
            shared
                .app_actions
                .push(crate::tab::AppAction::Properties(view.source.clone()));
        }
        shared.properties.preview = Some(view);
    }
}

/// Draws one window; false once it's closed.
fn window_ui(ctx: &egui::Context, window: &mut Window, shared: &mut Shared) -> bool {
    let title = format!("{} Properties", window.view.source.title());
    let raise = std::mem::take(&mut window.raise);
    if window.native {
        let id = ViewportId::from_hash_of(("properties", window.id));
        if raise {
            ctx.send_viewport_cmd_to(id, egui::ViewportCommand::Focus);
        }
        let builder = ViewportBuilder::default()
            .with_title(title)
            .with_app_id("egui-files")
            .with_inner_size([460.0, 600.0])
            .with_min_inner_size([320.0, 240.0]);
        ctx.show_viewport_immediate(id, builder, |ui, _class| {
            let close = ui.input(|i| i.viewport().close_requested());
            let mut closed = false;
            egui::CentralPanel::default().show(ui, |ui| {
                window.view.ui(
                    ui,
                    shared,
                    Host::Window {
                        closed: &mut closed,
                    },
                );
            });
            !(close || closed)
        })
    } else {
        let id = Id::new(("properties-window", window.id));
        if raise {
            ctx.move_to_top(egui::LayerId::new(egui::Order::Middle, id));
        }
        let mut open = true;
        let mut closed = false;
        egui::Window::new(title)
            .id(id)
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_size([440.0, 560.0])
            .min_size([300.0, 200.0])
            .show(ctx, |ui| {
                window.view.ui(
                    ui,
                    shared,
                    Host::Window {
                        closed: &mut closed,
                    },
                )
            });
        open && !closed
    }
}
