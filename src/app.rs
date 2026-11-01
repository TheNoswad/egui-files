//! Top-level application: menu bar and the dock of tabs and panes.

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
};

use egui::{
    Button, CentralPanel, Id, Key, Modifiers, Panel, Pos2, Rect, RichText, Ui, Vec2, WidgetText,
};
use egui_dock::{
    DockArea, DockState, Node, NodeIndex, NodePath, Split, Style, TabInsert, TabViewer, Tree,
    tab_viewer::OnCloseResponse,
};
use egui_phosphor::regular as icon;
use serde::{Deserialize, Serialize};

use crate::{
    about::About,
    archive::{self, ArchiveTab},
    dnd::{self, DropTarget},
    dock_tab::{DockTab, Pane, PaneTab},
    editor::{self, EditorTab},
    fs::Entry,
    images::ImageCache,
    jobs::{Jobs, Outcome},
    open_with::{self, OpenWith},
    places,
    preview::{self, TextCache},
    properties::{self, OpenIn, PropertiesTab, Source},
    sidebar::{Sidebar, SidebarResponse},
    system_theme::SystemTheme,
    tab::{self, AppAction, BrowserTab, Shared},
    terminal_panel::{PanelResponse, TerminalPanel},
    theme::{self, Themes},
    trash::{self, TrashCount, TrashTab},
    tree::FolderTree,
    usage::UsageTab,
    video::{Quality, Videos},
    watcher::DirWatcher,
};

/// What the sidebar showed before its parts became panes; read to migrate.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
enum SidebarMode {
    Places,
    Tree,
    #[default]
    Both,
    Hidden,
}

/// What survives a restart.
#[derive(Deserialize)]
struct Persisted {
    dock: DockState<DockTab>,
    show_hidden: bool,
    bookmarks: Vec<PathBuf>,
    /// Set once the panes live in the dock; older state is migrated.
    #[serde(default)]
    panes_docked: bool,
    #[serde(default)]
    sidebar_mode: SidebarMode,
    #[serde(default)]
    tree: FolderTree,
    #[serde(default = "default_true")]
    show_thumbnails: bool,
    #[serde(default = "default_icon_size")]
    icon_size: f32,
    #[serde(default)]
    show_terminal: bool,
    #[serde(default)]
    terminal: TerminalPanel,
    #[serde(default = "default_volume")]
    video_volume: f64,
    #[serde(default)]
    video_quality: Quality,
    #[serde(default)]
    archive_prefs: archive::Prefs,
    #[serde(default)]
    open_with: open_with::Prefs,
    #[serde(default)]
    editor: editor::Prefs,
    #[serde(default)]
    theme: theme::Prefs,
    #[serde(default = "default_true")]
    hide_empty_drives: bool,
    #[serde(default)]
    properties: properties::Prefs,
    /// Ids of the pinned tabs.
    #[serde(default)]
    pinned: HashSet<u64>,
    #[serde(default = "default_true")]
    folder_sizes: bool,
}

fn default_true() -> bool {
    true
}

fn default_icon_size() -> f32 {
    64.0
}

fn default_volume() -> f64 {
    crate::video::DEFAULT_VOLUME
}

#[derive(Serialize)]
struct PersistedRef<'a> {
    dock: &'a DockState<DockTab>,
    show_hidden: bool,
    bookmarks: &'a [PathBuf],
    panes_docked: bool,
    tree: &'a FolderTree,
    show_thumbnails: bool,
    icon_size: f32,
    terminal: &'a TerminalPanel,
    video_volume: f64,
    video_quality: Quality,
    archive_prefs: &'a archive::Prefs,
    open_with: &'a open_with::Prefs,
    editor: &'a editor::Prefs,
    theme: &'a theme::Prefs,
    hide_empty_drives: bool,
    properties: &'a properties::Prefs,
    pinned: &'a HashSet<u64>,
    folder_sizes: bool,
}

/// The panes a fresh layout starts with.
const DEFAULT_PANES: [Pane; 2] = [Pane::Places, Pane::Tree];

/// How big a pane starts out along each axis, in points, and the most of the
/// dock it may take; what the panels were before they became panes.
fn default_size(pane: Pane, horizontal: bool) -> Option<(f32, f32)> {
    match (pane, horizontal) {
        (Pane::Places | Pane::Tree, true) => Some((220.0, 0.3)),
        (Pane::Preview, true) => Some((300.0, 0.35)),
        (Pane::Terminal, false) => Some((240.0, 0.4)),
        _ => None,
    }
}

/// The panes in a subtree, or `None` if it also holds tabs.
fn panes_in(tree: &Tree<DockTab>, node: NodeIndex) -> Option<Vec<Pane>> {
    match &tree[node] {
        Node::Empty => Some(Vec::new()),
        Node::Leaf(leaf) => leaf.tabs.iter().map(DockTab::pane).collect(),
        Node::Horizontal(_) | Node::Vertical(_) => {
            let mut panes = panes_in(tree, node.left())?;
            panes.extend(panes_in(tree, node.right())?);
            Some(panes)
        }
    }
}

/// Sets the split fractions in `node`, which is laid out in `rect` of a dock
/// of size `area`, so each group of panes beside the tabs keeps the size it
/// was last drawn at, or its default size if it's new. Without this, docking
/// a pane along the edge of the whole dock would shrink the panes already there.
fn fit_panes(tree: &mut Tree<DockTab>, node: NodeIndex, rect: Rect, area: Vec2) {
    let horizontal = match &tree[node] {
        Node::Horizontal(_) => true,
        Node::Vertical(_) => false,
        Node::Empty | Node::Leaf(_) => return,
    };
    let length = if horizontal {
        rect.width()
    } else {
        rect.height()
    };
    let (first, second) = (node.left(), node.right());
    let pane_side = match (panes_in(tree, first), panes_in(tree, second)) {
        (Some(panes), None) => Some((first, panes)),
        (None, Some(panes)) => Some((second, panes)),
        _ => None,
    };
    if let Some((side, panes)) = pane_side
        && let Some((default, max)) = panes
            .iter()
            .filter_map(|&p| default_size(p, horizontal))
            .reduce(|a, b| (a.0.max(b.0), a.1.max(b.1)))
    {
        let drawn = tree[side]
            .rect()
            .filter(|r| r.is_finite() && r.is_positive())
            .map(|r| if horizontal { r.width() } else { r.height() });
        let whole = if horizontal { area.x } else { area.y };
        let size = drawn.unwrap_or_else(|| default.min(max * whole));
        let share = (size / length.max(1.0)).clamp(0.1, 0.9);
        let fraction = if side == first { share } else { 1.0 - share };
        if let Node::Horizontal(split) | Node::Vertical(split) = &mut tree[node] {
            split.fraction = fraction;
        }
    }
    let fraction = match &tree[node] {
        Node::Horizontal(split) | Node::Vertical(split) => split.fraction,
        Node::Empty | Node::Leaf(_) => return,
    };
    let (a, b) = if horizontal {
        rect.split_left_right_at_fraction(fraction)
    } else {
        rect.split_top_bottom_at_fraction(fraction)
    };
    fit_panes(tree, first, a, area);
    fit_panes(tree, second, b, area);
}

/// Docks `pane` where it goes by default: places and the folder tree on the
/// left, stacked if both are open, the preview on the right and the terminal
/// along the bottom. `area` is the dock's size.
fn dock_pane(dock: &mut DockState<DockTab>, pane: Pane, area: Vec2) {
    let tab = DockTab::Pane(PaneTab { pane });
    let partner = match pane {
        Pane::Places => Some((Pane::Tree, Split::Above)),
        Pane::Tree => Some((Pane::Places, Split::Below)),
        Pane::Preview | Pane::Terminal => None,
    };
    if let Some((partner, split)) = partner
        && let Some(path) = dock.find_tab_from(|t| t.pane() == Some(partner))
    {
        // Places gets enough height for the standard folders; the tree the rest.
        let places = (300.0 / area.y.max(1.0)).clamp(0.25, 0.6);
        let fraction = if split == Split::Above {
            places
        } else {
            1.0 - places
        };
        dock.split(path.node_path(), split, fraction, Node::leaf(tab));
    } else if dock.main_surface().root_node().is_none_or(|n| n.is_empty()) {
        dock.push_to_first_leaf(tab);
    } else {
        let split = match pane {
            Pane::Places | Pane::Tree => Split::Left,
            Pane::Preview => Split::Right,
            Pane::Terminal => Split::Below,
        };
        // `fit_panes` sizes it.
        dock.split(NodePath::MAIN_ROOT, split, 0.5, Node::leaf(tab));
    }
    let rect = Rect::from_min_size(egui::Pos2::ZERO, area);
    fit_panes(dock.main_surface_mut(), NodeIndex::root(), rect, area);
}

/// The panes to dock for state saved before panes were in the dock.
fn migrate(dock: &DockState<DockTab>, sidebar: SidebarMode, terminal: bool) -> Vec<Pane> {
    let sidebar: &[Pane] = match sidebar {
        SidebarMode::Places => &[Pane::Places],
        SidebarMode::Tree => &[Pane::Tree],
        SidebarMode::Both => &DEFAULT_PANES,
        SidebarMode::Hidden => &[],
    };
    let preview = dock
        .iter_all_tabs()
        .any(|(_, t)| t.browser().is_some_and(|b| b.show_preview));
    sidebar
        .iter()
        .copied()
        .chain(preview.then_some(Pane::Preview))
        .chain(terminal.then_some(Pane::Terminal))
        .collect()
}

/// Size of egui_dock's tab bar buttons, which it doesn't export.
const TAB_BUTTON_SIZE: f32 = 24.0;

/// Where egui_dock draws the add button of a leaf at `leaf_rect`, with the
/// close-all button to its right.
fn add_button_rect(leaf_rect: Rect, style: &Style) -> Rect {
    let bar = Rect::from_min_size(
        leaf_rect.min,
        egui::vec2(leaf_rect.width(), style.tab_bar.height),
    ) - style.tab_bar.inner_margin;
    let right = bar.right() - TAB_BUTTON_SIZE;
    Rect::from_min_max(
        egui::pos2(right - TAB_BUTTON_SIZE, bar.top()),
        egui::pos2(right, bar.bottom() - 2.0),
    )
}

/// Tooltips for the add buttons, which egui_dock doesn't offer. Only for the
/// main surface; windows are drawn on layers above it.
fn add_button_hints(ui: &mut Ui, dock: &DockState<DockTab>) {
    let style = Style::from_egui(ui.style());
    for (path, leaf) in dock.iter_leaves() {
        if path.surface.is_main() {
            let rect = add_button_rect(leaf.rect, &style);
            // Hover only, so the button underneath still gets the clicks.
            ui.interact(rect, Id::new(("add-tab-hint", path)), egui::Sense::hover())
                .on_hover_text("New file browser tab\nRight-click the tab bar for panes and more");
        }
    }
}

/// Moves pinned tabs to the front of their tab bar, keeping their order and
/// which tab is shown, and forgets pins of tabs that closed.
fn pinned_first(dock: &mut DockState<DockTab>, pinned: &mut HashSet<u64>) {
    let open: HashSet<u64> = dock.iter_all_tabs().map(|(_, t)| t.id()).collect();
    pinned.retain(|id| open.contains(id));
    for (_, node) in dock.iter_all_nodes_mut() {
        let Some(leaf) = node.get_leaf_mut() else {
            continue;
        };
        let unpinned = |t: &DockTab| !pinned.contains(&t.id());
        if leaf.tabs.is_sorted_by_key(unpinned) {
            continue;
        }
        let active = leaf.tabs.get(leaf.active.0).map(DockTab::id);
        leaf.tabs.sort_by_key(unpinned);
        if let Some(index) = active.and_then(|id| leaf.tabs.iter().position(|t| t.id() == id)) {
            leaf.active.0 = index;
        }
    }
}

/// Where a drop at `rel` (0 to 1 across a tab's content) splits it off: the
/// nearest edge, if within a quarter of the way across.
fn split_zone(rel: Vec2) -> Option<Split> {
    [
        (rel.x, Split::Left),
        (1.0 - rel.x, Split::Right),
        (rel.y, Split::Above),
        (1.0 - rel.y, Split::Below),
    ]
    .into_iter()
    .min_by(|a, b| a.0.total_cmp(&b.0))
    .filter(|(distance, _)| *distance < 0.25)
    .map(|(_, split)| split)
}

/// What the add button's menu can open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NewTab {
    Browser,
    Trash,
    /// Disk usage of the focused folder.
    Usage,
    Pane(Pane),
}

/// The menu opened by right-clicking a leaf's add button.
#[derive(Clone, Copy)]
struct AddMenu {
    leaf: NodePath,
    pos: Pos2,
}

pub struct FilesApp {
    dock: DockState<DockTab>,
    shared: Shared,
    sidebar: Sidebar,
    next_tab_id: u64,
    /// The browser tab the panes follow: the focused one, or the one focused
    /// last while a pane has focus.
    active_browser: Option<u64>,
    watcher: DirWatcher,
    /// The media last seen in each drive, to notice cards being swapped.
    drive_media: HashMap<PathBuf, Option<u32>>,
    trash_count: TrashCount,
    /// Showing the "operations still running" or "unsaved changes" dialog.
    confirm_quit: bool,
    /// Editor tabs with unsaved changes waiting for "Save changes?" answers.
    confirm_close: Vec<u64>,
    /// Why saving before quitting failed.
    quit_error: Option<String>,
    /// The user confirmed quitting with operations running.
    allow_close: bool,
    /// Drag and drop with other apps on Wayland, when running there.
    #[cfg(target_os = "linux")]
    wayland: Option<crate::wayland_dnd::WaylandDnd>,
    /// Ctrl+V that egui-winit dropped (see `file_clipboard::PasteKey`).
    #[cfg(windows)]
    paste_key: crate::file_clipboard::PasteKey,
    /// Last clipboard change seen, for picking up files copied in other apps.
    #[cfg(windows)]
    clipboard_seq: u32,
    /// An in-app drag was handed to the platform and is still in progress
    /// (Wayland), or just ended and egui still needs the button release (Windows).
    drag_out_active: bool,
    /// Files dropped onto the window via winit (X11, Windows).
    os_dropped: Vec<PathBuf>,
    /// Kept while the terminal pane is closed, so its shells live on.
    terminal: TerminalPanel,
    /// Panes to dock once the dock's size is known.
    pending_panes: Vec<Pane>,
    /// The dock's size last frame, which new panes are sized against.
    dock_size: Vec2,
    add_menu: Option<AddMenu>,
    /// Ids of the pinned tabs: they can't be closed and stay at the front of
    /// their tab bar.
    pinned: HashSet<u64>,
    /// The main surface's node kinds and the dock's size last frame, to
    /// notice them changing.
    dock_shape: (Vec<u8>, Vec2),
    about: About,
    system_theme: SystemTheme,
    themes: Themes,
}

impl FilesApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let mut fonts = egui::FontDefinitions::default();
        egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
        cc.egui_ctx.set_fonts(fonts);

        let persisted: Option<Persisted> = cc
            .storage
            .and_then(|s| eframe::get_value(s, eframe::APP_KEY));
        let persisted = persisted
            .filter(|p| p.dock.iter_all_tabs().next().is_some())
            .unwrap_or_else(|| Persisted {
                dock: DockState::new(vec![DockTab::Browser(BrowserTab::new(
                    1,
                    places::home_dir(),
                ))]),
                show_hidden: false,
                bookmarks: Vec::new(),
                panes_docked: false,
                sidebar_mode: SidebarMode::default(),
                tree: FolderTree::default(),
                show_thumbnails: default_true(),
                icon_size: default_icon_size(),
                show_terminal: false,
                terminal: TerminalPanel::default(),
                video_volume: default_volume(),
                video_quality: Quality::default(),
                archive_prefs: archive::Prefs::default(),
                open_with: open_with::Prefs::default(),
                editor: editor::Prefs::default(),
                theme: theme::Prefs::default(),
                hide_empty_drives: true,
                properties: properties::Prefs::default(),
                pinned: HashSet::new(),
                folder_sizes: true,
            });
        let Persisted {
            dock,
            show_hidden,
            bookmarks,
            panes_docked,
            sidebar_mode,
            tree,
            show_thumbnails,
            icon_size,
            show_terminal,
            mut terminal,
            video_volume,
            video_quality,
            archive_prefs,
            open_with,
            editor,
            theme,
            hide_empty_drives,
            properties,
            pinned,
            folder_sizes,
        } = persisted;
        editor::highlight::preload();
        let pending_panes = if panes_docked {
            Vec::new()
        } else {
            migrate(&dock, sidebar_mode, show_terminal)
        };
        if terminal.is_empty() {
            terminal = TerminalPanel::default();
        }
        let next_tab_id = dock
            .iter_all_tabs()
            .filter(|(_, t)| t.pane().is_none())
            .map(|(_, t)| t.id())
            .max()
            .unwrap_or(0)
            + 1;

        Self {
            dock,
            shared: Shared {
                show_hidden,
                show_thumbnails,
                icon_size,
                images: ImageCache::new(&cc.egui_ctx),
                videos: {
                    let mut videos = Videos::new(&cc.egui_ctx);
                    videos.set_volume(video_volume);
                    videos.set_quality(video_quality);
                    videos
                },
                texts: TextCache::default(),
                jobs: Jobs::new(&cc.egui_ctx),
                archives: archive::Listings::new(&cc.egui_ctx),
                archive_prefs,
                open_with: {
                    open_with::preload(&cc.egui_ctx);
                    OpenWith::new(open_with)
                },
                editor,
                clipboard: None,
                focused_tab: None,
                preview_shown: false,
                app_actions: Vec::new(),
                usage: {
                    let mut usage = crate::usage::Scans::default();
                    usage.enabled = folder_sizes;
                    usage
                },
                properties: properties::Properties::new(properties),
            },
            sidebar: Sidebar::new(&cc.egui_ctx, bookmarks, tree, hide_empty_drives),
            next_tab_id,
            active_browser: None,
            watcher: DirWatcher::new(&cc.egui_ctx),
            drive_media: HashMap::new(),
            trash_count: TrashCount::new(&cc.egui_ctx),
            confirm_quit: false,
            confirm_close: Vec::new(),
            quit_error: None,
            allow_close: false,
            #[cfg(target_os = "linux")]
            wayland: crate::wayland_dnd::WaylandDnd::new(cc),
            #[cfg(windows)]
            paste_key: Default::default(),
            #[cfg(windows)]
            clipboard_seq: 0,
            drag_out_active: false,
            os_dropped: Vec::new(),
            terminal,
            pending_panes,
            dock_size: Vec2::ZERO,
            add_menu: None,
            pinned,
            dock_shape: (Vec::new(), Vec2::ZERO),
            about: About::new(cc),
            system_theme: SystemTheme::watch(&cc.egui_ctx),
            themes: Themes::new(&cc.egui_ctx, theme),
        }
    }

    fn next_id(&mut self) -> u64 {
        self.next_tab_id += 1;
        self.next_tab_id - 1
    }

    fn make_tab(&mut self, path: PathBuf) -> DockTab {
        DockTab::Browser(BrowserTab::new(self.next_id(), path))
    }

    /// Keeps `active_browser` pointing at an open browser tab.
    fn update_active_browser(&mut self) {
        if let Some((_, tab)) = self.dock.find_active_focused()
            && tab.browser().is_some()
        {
            self.active_browser = Some(tab.id());
        }
        let open = |id| {
            self.dock
                .find_tab_from(|t| t.id() == id && t.browser().is_some())
                .is_some()
        };
        if !self.active_browser.is_some_and(open) {
            self.active_browser = self
                .dock
                .iter_all_tabs()
                .find(|(_, t)| t.browser().is_some())
                .map(|(_, t)| t.id());
        }
    }

    fn active_browser_mut(&mut self) -> Option<&mut BrowserTab> {
        let id = self.active_browser?;
        self.dock
            .iter_all_tabs_mut()
            .find(|(_, t)| t.id() == id)
            .and_then(|(_, t)| t.browser_mut())
    }

    fn trash_focused(&mut self) -> bool {
        matches!(
            self.dock.find_active_focused(),
            Some((_, DockTab::Trash(_)))
        )
    }

    /// The active browser tab's folder, unless a trash tab has focus.
    fn focused_folder(&mut self) -> Option<PathBuf> {
        if self.trash_focused() {
            return None;
        }
        self.active_browser_mut().map(|b| b.path.clone())
    }

    fn focused_path(&mut self) -> PathBuf {
        self.focused_folder().unwrap_or_else(places::home_dir)
    }

    /// Brings the tab with this id to the front and gives it focus.
    fn focus_tab(&mut self, id: u64) {
        if let Some(path) = self.dock.find_tab_from(|t| t.id() == id) {
            let _ = self.dock.set_active_tab(path);
            self.dock.set_focused_node_and_surface(path.node_path());
        }
    }

    /// Adds a tab next to the focused one, or next to the active browser tab
    /// if a pane has focus.
    fn push_tab(&mut self, tab: DockTab) {
        let pane_focused = self
            .dock
            .find_active_focused()
            .is_none_or(|(_, t)| t.pane().is_some());
        if pane_focused
            && let Some(path) = self
                .active_browser
                .and_then(|id| self.dock.find_tab_from(|t| t.id() == id))
        {
            self.dock.set_focused_node_and_surface(path.node_path());
        }
        self.dock.push_to_focused_leaf(tab);
    }

    /// Notices memory cards and discs being swapped: folders on a drive whose
    /// media changed are watched afresh and shown again. Returns the drives
    /// without media, whose folders mustn't be watched: the watch would keep
    /// the old card in use and the next one from mounting.
    fn follow_media(&mut self) -> Vec<PathBuf> {
        let drives = self.sidebar.drives.get();
        let mut changed = false;
        for drive in &drives {
            if self
                .drive_media
                .get(&drive.path)
                .is_some_and(|&media| media != drive.media)
            {
                self.watcher.forget(&drive.path);
                changed = true;
            }
        }
        if changed {
            self.sidebar.tree.invalidate();
            for (_, tab) in self.dock.iter_all_tabs_mut() {
                tab.mark_stale();
            }
        }
        self.drive_media = drives.iter().map(|d| (d.path.clone(), d.media)).collect();
        drives
            .into_iter()
            .filter(|d| d.media.is_none())
            .map(|d| d.path)
            .collect()
    }

    /// Moves tabs showing the drive to the home folder, since an open folder
    /// keeps a drive busy, then ejects it.
    fn eject(&mut self, drive: PathBuf, label: String) {
        let home = places::home_dir();
        for (_, tab) in self.dock.iter_all_tabs_mut() {
            if let Some(browser) = tab.browser_mut()
                && browser.path.starts_with(&drive)
            {
                browser.navigate(home.clone());
            }
        }
        self.shared
            .jobs
            .start(crate::jobs::JobKind::Eject { drive, label });
    }

    fn open_trash(&mut self) {
        let trash_tab = self
            .dock
            .iter_all_tabs()
            .find(|(_, t)| t.is_trash())
            .map(|(_, t)| t.id());
        if let Some(id) = trash_tab {
            self.focus_tab(id);
        } else {
            let tab = DockTab::Trash(TrashTab::new(self.next_id()));
            self.push_tab(tab);
        }
    }

    /// Puts files on the system clipboard so other apps paste the files
    /// themselves, falling back to their paths as text (all egui offers).
    fn copy_files(&self, ctx: &egui::Context, paths: &[PathBuf], cut: bool) {
        #[cfg(target_os = "linux")]
        if let Some(wayland) = &self.wayland
            && wayland.set_selection(paths, cut)
        {
            return;
        }
        #[cfg(windows)]
        if crate::file_clipboard::set(paths, cut).is_ok() {
            return;
        }
        let _ = cut;
        ctx.copy_text(tab::join_paths(paths));
    }

    /// Shows an archive's contents, in the tab already showing it if there is one.
    fn open_archive(&mut self, path: PathBuf) {
        let existing = self
            .dock
            .iter_all_tabs()
            .find(|(_, t)| t.archive() == Some(path.as_path()))
            .map(|(_, t)| t.id());
        if let Some(id) = existing {
            self.focus_tab(id);
        } else {
            let tab = DockTab::Archive(ArchiveTab::new(self.next_id(), path));
            self.push_tab(tab);
        }
    }

    /// Shows a folder's disk usage, in the tab already showing it if there
    /// is one.
    fn open_usage(&mut self, path: PathBuf) {
        let existing = self
            .dock
            .iter_all_tabs()
            .find(|(_, t)| t.usage().is_some_and(|u| u.usage == path))
            .map(|(_, t)| t.id());
        if let Some(id) = existing {
            self.focus_tab(id);
        } else {
            let tab = DockTab::Usage(UsageTab::new(self.next_id(), path));
            self.push_tab(tab);
        }
    }

    /// Shows properties where the user chose: in a tab (the one already
    /// showing them if there is one) or a window.
    fn open_properties(&mut self, source: Source) {
        if source.is_empty() {
            return;
        }
        match self.shared.properties.prefs.open_in {
            OpenIn::Tab => {
                let existing = self
                    .dock
                    .iter_all_tabs()
                    .find(|(_, t)| t.properties().is_some_and(|p| p.properties == source))
                    .map(|(_, t)| t.id());
                if let Some(id) = existing {
                    self.focus_tab(id);
                } else {
                    let tab = DockTab::Properties(PropertiesTab::new(self.next_id(), source));
                    self.push_tab(tab);
                }
            }
            OpenIn::Window => self.shared.properties.open_window(source, false),
            OpenIn::Native => self.shared.properties.open_window(source, true),
        }
    }

    /// The selection in the focused browser tab, or its folder if nothing
    /// is selected.
    fn focused_selection(&mut self) -> Option<Source> {
        let tab = self.active_browser_mut()?;
        let selected = tab.selected_paths();
        Some(Source::Files(if selected.is_empty() {
            vec![tab.path.clone()]
        } else {
            selected
        }))
    }

    /// Opens a new tab on the folder holding `path`, with it selected.
    fn reveal(&mut self, path: PathBuf) {
        let Some(parent) = path.parent().map(PathBuf::from) else {
            return self.open_tab(path);
        };
        let tab = self.make_tab(parent);
        let id = tab.id();
        self.push_tab(tab);
        if let Some(browser) = self
            .dock
            .iter_all_tabs_mut()
            .find(|(_, t)| t.id() == id)
            .and_then(|(_, t)| t.browser_mut())
        {
            browser.reveal(path);
        }
    }

    /// Edits a file, in the tab already editing it if there is one.
    fn open_editor(&mut self, path: PathBuf) {
        let existing = self
            .dock
            .iter_all_tabs()
            .find(|(_, t)| t.editor().is_some_and(|e| e.edit == path))
            .map(|(_, t)| t.id());
        if let Some(id) = existing {
            self.focus_tab(id);
        } else {
            let tab = DockTab::Editor(EditorTab::new(self.next_id(), path));
            self.push_tab(tab);
        }
    }

    fn editor_mut(&mut self, id: u64) -> Option<&mut EditorTab> {
        self.dock
            .iter_all_tabs_mut()
            .find(|(_, t)| t.id() == id)
            .and_then(|(_, t)| t.editor_mut())
    }

    fn remove_tab(&mut self, id: u64) {
        if let Some(path) = self.dock.find_tab_from(|t| t.id() == id) {
            self.dock.remove_tab(path);
        }
    }

    /// Closes a tab, first asking about unsaved changes in an editor.
    fn request_close(&mut self, id: u64) {
        if self.editor_mut(id).is_some_and(|e| e.is_dirty()) {
            if !self.confirm_close.contains(&id) {
                self.confirm_close.push(id);
            }
            self.focus_tab(id);
        } else {
            self.remove_tab(id);
        }
    }

    /// Asks whether to save an editor's changes before closing it.
    fn confirm_close_ui(&mut self, ctx: &egui::Context) {
        let Some(&id) = self.confirm_close.first() else {
            return;
        };
        let Some(editor) = self.editor_mut(id).filter(|e| e.is_dirty()) else {
            // Saved or closed meanwhile.
            self.confirm_close.remove(0);
            return;
        };
        let name = crate::fs::display_name(&editor.edit);
        let mut answer = None;
        let modal = egui::Modal::new(Id::new("confirm_close")).show(ctx, |ui| {
            ui.heading(format!("Save changes to “{name}”?"));
            ui.label("Your changes are lost if you don't save them.");
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button(format!("{} Save", icon::FLOPPY_DISK)).clicked() {
                    answer = Some(true);
                }
                if ui.button("Don't save").clicked() {
                    answer = Some(false);
                }
                if ui.button("Cancel").clicked() {
                    self.confirm_close.remove(0);
                }
            });
        });
        if modal.should_close() && answer.is_none() && self.confirm_close.first() == Some(&id) {
            self.confirm_close.remove(0);
        }
        match answer {
            Some(true) => {
                self.confirm_close.remove(0);
                // A failed save shows its error in the tab, which stays open.
                if self.editor_mut(id).is_some_and(|e| e.save().is_ok()) {
                    self.remove_tab(id);
                }
            }
            Some(false) => {
                self.confirm_close.remove(0);
                self.remove_tab(id);
            }
            None => {}
        }
    }

    fn open_tab(&mut self, path: PathBuf) {
        let tab = self.make_tab(path);
        self.push_tab(tab);
    }

    fn close_focused_tab(&mut self) {
        let last_browser = self.browser_count() == 1;
        let Some(id) = self
            .dock
            .find_active_focused()
            .filter(|(_, t)| !(last_browser && t.browser().is_some()))
            .filter(|(_, t)| !self.pinned.contains(&t.id()))
            .map(|(_, t)| t.id())
        else {
            return;
        };
        self.request_close(id);
    }

    fn toggle_pin(&mut self, id: u64) {
        if !self.pinned.remove(&id) {
            self.pinned.insert(id);
        }
        self.keep_pinned_first();
    }

    fn keep_pinned_first(&mut self) {
        pinned_first(&mut self.dock, &mut self.pinned);
    }

    /// Shows `path` in the active browser tab, or a new one, and focuses it.
    fn go_to(&mut self, path: PathBuf, new_tab: bool) {
        if !new_tab && let Some(tab) = self.active_browser_mut() {
            tab.navigate(path);
            let id = tab.id;
            self.focus_tab(id);
        } else {
            self.open_tab(path);
        }
    }

    fn browser_count(&self) -> usize {
        self.dock
            .iter_all_tabs()
            .filter(|(_, t)| t.browser().is_some())
            .count()
    }

    fn has_pane(&self, pane: Pane) -> bool {
        self.dock
            .find_tab_from(|t| t.pane() == Some(pane))
            .is_some()
    }

    /// Opens or closes a pane, as the View menu does.
    fn toggle_pane(&mut self, ctx: &egui::Context, pane: Pane) {
        if let Some(path) = self.dock.find_tab_from(|t| t.pane() == Some(pane)) {
            self.dock.remove_tab(path);
            if pane == Pane::Terminal {
                self.terminal.surrender_focus(ctx);
            }
        } else {
            dock_pane(&mut self.dock, pane, self.dock_size);
            self.focus_pane(pane);
        }
    }

    /// Like `toggle_pane`, but brings a pane hidden behind another tab to the
    /// front instead of closing it.
    fn show_or_toggle_pane(&mut self, ctx: &egui::Context, pane: Pane) {
        let hidden = self
            .dock
            .find_tab_from(|t| t.pane() == Some(pane))
            .is_some_and(|path| {
                self.dock
                    .leaf(path.node_path())
                    .is_ok_and(|leaf| leaf.active != path.tab)
            });
        if hidden {
            self.focus_pane(pane);
        } else {
            self.toggle_pane(ctx, pane);
        }
    }

    fn focus_pane(&mut self, pane: Pane) {
        if pane == Pane::Terminal {
            self.terminal.focus();
            self.focus_tab(pane.id());
        } else if let Some(path) = self.dock.find_tab_from(|t| t.pane() == Some(pane)) {
            // Shown without taking focus from the browser tab.
            let _ = self.dock.set_active_tab(path);
        }
    }

    /// Puts every pane back where it starts out, keeping the tabs.
    fn reset_layout(&mut self) {
        let panes: Vec<Pane> = Pane::ALL
            .into_iter()
            .filter(|&p| DEFAULT_PANES.contains(&p) || self.has_pane(p))
            .collect();
        self.dock.retain_tabs(|t| t.pane().is_none());
        let mut tabs = Vec::new();
        for (_, node) in self.dock.iter_all_nodes_mut() {
            if let Some(leaf) = node.get_leaf_mut() {
                tabs.append(&mut leaf.tabs);
            }
        }
        if tabs.is_empty() {
            tabs.push(self.make_tab(places::home_dir()));
        }
        self.dock = DockState::new(tabs);
        for pane in panes {
            dock_pane(&mut self.dock, pane, self.dock_size);
        }
    }

    /// Opens `kind` as a tab of `leaf`. Panes and the trash are only open
    /// once, so if one already is, it moves there instead.
    fn add_to_leaf(&mut self, leaf: NodePath, kind: NewTab) {
        if self.dock.leaf(leaf).is_err() {
            return;
        }
        let existing = self.dock.find_tab_from(|t| match kind {
            NewTab::Browser => false,
            NewTab::Trash => t.is_trash(),
            NewTab::Usage => false,
            NewTab::Pane(pane) => t.pane() == Some(pane),
        });
        let id = if let Some(path) = existing {
            let id = self.dock[path.node_path()].tabs().unwrap()[path.tab.0].id();
            self.dock.move_tab(path, (leaf, TabInsert::Append));
            id
        } else {
            let tab = match kind {
                NewTab::Browser => {
                    let path = self.focused_path();
                    self.make_tab(path)
                }
                NewTab::Trash => DockTab::Trash(TrashTab::new(self.next_id())),
                NewTab::Usage => {
                    let path = self.focused_path();
                    DockTab::Usage(UsageTab::new(self.next_id(), path))
                }
                NewTab::Pane(pane) => DockTab::Pane(PaneTab { pane }),
            };
            let id = tab.id();
            self.dock.set_focused_node_and_surface(leaf);
            self.dock.push_to_focused_leaf(tab);
            id
        };
        self.focus_tab(id);
        if kind == NewTab::Pane(Pane::Terminal) {
            self.terminal.focus();
        }
    }

    /// The empty part of each tab bar: right-clicking it opens the add
    /// button's menu, and folders dropped on it open in new tabs there.
    fn tab_bars_ui(&mut self, ctx: &egui::Context, tab_buttons: &[Rect]) {
        let style = Style::from_egui(&ctx.global_style());
        let bars: Vec<(NodePath, Rect)> = self
            .dock
            .iter_leaves()
            .map(|(path, leaf)| {
                let bar = Rect::from_min_size(
                    leaf.rect.min,
                    egui::vec2(leaf.rect.width(), style.tab_bar.height),
                );
                (path, bar)
            })
            .collect();
        let Some(pos) = ctx.pointer_latest_pos() else {
            return;
        };
        let Some(&(leaf, bar)) = bars.iter().find(|(_, bar)| bar.contains(pos)) else {
            return;
        };
        if tab_buttons.iter().any(|rect| rect.contains(pos)) {
            return;
        }

        if ctx.input(|i| i.pointer.button_clicked(egui::PointerButton::Secondary)) {
            self.add_menu = Some(AddMenu { leaf, pos });
        }
        let folders = dnd::new_tab_target(ctx, bar).unwrap_or_default();
        let mut last = None;
        for folder in folders {
            let tab = self.make_tab(folder);
            last = Some(tab.id());
            self.dock.set_focused_node_and_surface(leaf);
            self.dock.push_to_focused_leaf(tab);
        }
        if let Some(id) = last {
            self.focus_tab(id);
        }
    }

    /// Sidebar places dropped on a tab's content: near an edge they open
    /// in a new pane split off on that side, elsewhere in a new tab there.
    fn place_split_ui(&mut self, ctx: &egui::Context) {
        let style = Style::from_egui(&ctx.global_style());
        let Some(pos) = ctx.pointer_latest_pos() else {
            return;
        };
        // Not over the panes: the place came from one of them.
        let Some((leaf, body)) = self
            .dock
            .iter_leaves()
            .filter(|(path, leaf)| {
                path.surface.is_main()
                    && leaf
                        .tabs
                        .get(leaf.active.0)
                        .is_some_and(|t| t.pane().is_none())
            })
            .map(|(path, leaf)| {
                let mut body = leaf.rect;
                body.min.y += style.tab_bar.height;
                (path, body)
            })
            .find(|(_, body)| body.contains(pos))
        else {
            return;
        };
        let Some((folder, dropped)) = dnd::place_over(ctx, body) else {
            return;
        };

        let split = split_zone((pos - body.min) / body.size());

        let highlight = match split {
            Some(Split::Left) => body.split_left_right_at_fraction(0.5).0,
            Some(Split::Right) => body.split_left_right_at_fraction(0.5).1,
            Some(Split::Above) => body.split_top_bottom_at_fraction(0.5).0,
            Some(Split::Below) => body.split_top_bottom_at_fraction(0.5).1,
            None => body,
        };
        let color = crate::theme::colors(ctx).files.drop_target;
        let layer = egui::LayerId::new(egui::Order::Foreground, Id::new("place-split-target"));
        let painter = ctx.layer_painter(layer);
        painter.rect_filled(highlight, 3.0, color.gamma_multiply(0.15));
        painter.rect_stroke(
            highlight,
            3.0,
            egui::Stroke::new(2.0, color),
            egui::StrokeKind::Inside,
        );

        if dropped {
            let tab = self.make_tab(folder);
            let id = tab.id();
            match split {
                Some(split) => {
                    self.dock.split(leaf, split, 0.5, Node::leaf(tab));
                }
                None => {
                    self.dock.set_focused_node_and_surface(leaf);
                    self.dock.push_to_focused_leaf(tab);
                }
            }
            self.focus_tab(id);
        }
    }

    /// Shows the add button's menu.
    fn add_menu_ui(&mut self, ctx: &egui::Context) {
        if let Some(menu) = self.add_menu {
            let mut choice = None;
            let area = egui::Area::new(Id::new("add-tab-menu"))
                .order(egui::Order::Foreground)
                .fixed_pos(menu.pos)
                .show(ctx, |ui| {
                    egui::Frame::menu(ui.style()).show(ui, |ui| {
                        ui.set_min_width(170.0);
                        ui.with_layout(egui::Layout::top_down_justified(egui::Align::Min), |ui| {
                            let item = |ui: &mut Ui, glyph: &str, label: &str, open: bool| {
                                let hint = if open { "move here" } else { "" };
                                ui.add(
                                    Button::new(format!("{glyph}  {label}"))
                                        .right_text(RichText::new(hint).weak()),
                                )
                                .clicked()
                            };
                            if item(ui, icon::FOLDER, "File browser", false) {
                                choice = Some(NewTab::Browser);
                            }
                            if item(
                                ui,
                                icon::TRASH,
                                "Trash",
                                self.dock.iter_all_tabs().any(|(_, t)| t.is_trash()),
                            ) {
                                choice = Some(NewTab::Trash);
                            }
                            if item(ui, icon::CHART_PIE_SLICE, "Disk usage", false) {
                                choice = Some(NewTab::Usage);
                            }
                            ui.separator();
                            for pane in Pane::ALL {
                                if item(ui, pane.icon(), pane.label(), self.has_pane(pane)) {
                                    choice = Some(NewTab::Pane(pane));
                                }
                            }
                        });
                    });
                });
            let clicked_outside = ctx.input(|i| {
                i.pointer.any_click()
                    && i.pointer
                        .interact_pos()
                        .is_some_and(|pos| !area.response.rect.contains(pos))
            });
            if let Some(kind) = choice {
                self.add_to_leaf(menu.leaf, kind);
            }
            if choice.is_some() || clicked_outside || ctx.input(|i| i.key_pressed(Key::Escape)) {
                self.add_menu = None;
            }
        }
    }

    fn global_shortcuts(&mut self, ctx: &egui::Context) {
        // Shift+F4 edits the selection.
        if !ctx.input(|i| i.modifiers.shift)
            && ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::F4))
        {
            self.show_or_toggle_pane(ctx, Pane::Terminal);
        }
        // The shell gets everything else typed into it (Ctrl+W deletes a word).
        if self.terminal.has_focus(ctx) {
            return;
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::U)) {
            let path = self.focused_path();
            self.open_usage(path);
        }
        let (new_tab, close_tab) = ctx.input_mut(|i| {
            (
                i.consume_key(Modifiers::COMMAND, Key::T),
                i.consume_key(Modifiers::COMMAND, Key::W),
            )
        });
        if new_tab {
            let path = self.focused_path();
            self.open_tab(path);
        }
        if close_tab {
            self.close_focused_tab();
        }
    }

    fn menu_bar(&mut self, ui: &mut Ui) {
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("File", |ui| {
                if ui
                    .add(Button::new("New tab").shortcut_text("Ctrl+T"))
                    .clicked()
                {
                    let path = self.focused_path();
                    self.open_tab(path);
                }
                if ui
                    .add(Button::new("Close tab").shortcut_text("Ctrl+W"))
                    .clicked()
                {
                    self.close_focused_tab();
                }
                if ui
                    .button(format!("{} Open trash", egui_phosphor::regular::TRASH))
                    .clicked()
                {
                    self.open_trash();
                }
                if ui
                    .add(
                        Button::new(format!("{} Disk usage", icon::CHART_PIE_SLICE))
                            .shortcut_text("Ctrl+Shift+U"),
                    )
                    .on_hover_text("Where the space in this folder went")
                    .clicked()
                {
                    let path = self.focused_path();
                    self.open_usage(path);
                }
                if ui
                    .add(
                        Button::new(format!("{} Properties", icon::INFO))
                            .shortcut_text("Alt+Enter"),
                    )
                    .on_hover_text("Of the selected items, or of the folder")
                    .clicked()
                    && let Some(source) = self.focused_selection()
                {
                    self.open_properties(source);
                }
                ui.separator();
                if ui.button("Quit").clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
            ui.menu_button("View", |ui| {
                ui.checkbox(&mut self.shared.show_hidden, "Show hidden files (Ctrl+H)");
                ui.checkbox(&mut self.shared.show_thumbnails, "Show thumbnails");
                ui.checkbox(&mut self.shared.usage.enabled, "Show folder sizes")
                    .on_hover_text("Measure the folders in the Details view, in the background");
                crate::dates::checkbox(ui);
                ui.checkbox(&mut self.sidebar.hide_empty_drives, "Hide empty drives")
                    .on_hover_text("Card readers without a card and drives with no disc");
                ui.add(
                    egui::Slider::new(&mut self.shared.icon_size, 32.0..=192.0).text("Icon size"),
                );
                ui.checkbox(
                    &mut self.shared.open_with.prefs.custom_commands,
                    "Custom commands in “Open with”",
                )
                .on_hover_text("The “Other application…” dialog gets a box for typing any command");
                ui.menu_button(format!("{} Syntax colors", icon::PALETTE), |ui| {
                    self.shared.editor.menu_ui(ui);
                });
                ui.menu_button(format!("{} Properties", icon::INFO), |ui| {
                    ui.label("Open properties in");
                    let prefs = &mut self.shared.properties.prefs;
                    for open_in in OpenIn::ALL {
                        if ui
                            .radio(prefs.open_in == open_in, open_in.label())
                            .clicked()
                        {
                            prefs.open_in = open_in;
                        }
                    }
                    ui.separator();
                    ui.checkbox(&mut prefs.apply_immediately, "Apply changes immediately")
                        .on_hover_text(
                            "Without an Apply button: each change is made as soon as it's set",
                        );
                });
                ui.menu_button("Video quality", |ui| {
                    for quality in Quality::ALL {
                        let selected = self.shared.videos.quality() == quality;
                        if ui.radio(selected, quality.label()).clicked() {
                            self.shared.videos.set_quality(quality);
                        }
                    }
                });
                ui.separator();
                ui.menu_button(format!("{} Theme", icon::PAINT_BRUSH), |ui| {
                    egui::widgets::global_theme_preference_buttons(ui);
                    ui.separator();
                    ui.menu_button(format!("{} Dark mode theme", icon::MOON), |ui| {
                        self.themes.menu_ui(ui, egui::Theme::Dark);
                    });
                    ui.menu_button(format!("{} Light mode theme", icon::SUN), |ui| {
                        self.themes.menu_ui(ui, egui::Theme::Light);
                    });
                    ui.separator();
                    if ui
                        .button(format!("{} Theme editor…", icon::PALETTE))
                        .clicked()
                    {
                        self.themes.open_editor(ui.ctx());
                        ui.close();
                    }
                });
                ui.separator();
                ui.label("Panes");
                for pane in Pane::ALL {
                    let label = match pane {
                        Pane::Preview => "Preview (F3)",
                        Pane::Terminal => "Terminal (F4)",
                        _ => pane.label(),
                    };
                    let mut open = self.has_pane(pane);
                    if ui
                        .checkbox(&mut open, format!("{} {label}", pane.icon()))
                        .changed()
                    {
                        self.toggle_pane(ui.ctx(), pane);
                    }
                }
                ui.menu_button(format!("{} Terminal folder", icon::LINK_SIMPLE), |ui| {
                    self.terminal.follow_menu(ui);
                });
                if ui
                    .button("Reset layout")
                    .on_hover_text("Put the panes back where they started; tabs stay open")
                    .clicked()
                {
                    self.reset_layout();
                    ui.close();
                }
            });
            ui.menu_button("Help", |ui| {
                if ui
                    .button(format!("{} About egui-files", icon::INFO))
                    .clicked()
                {
                    self.about.open = true;
                    ui.close();
                }
            });
        });
    }
}

/// Seconds a drag must hover over a tab title before switching to it.
const TAB_SWITCH_DELAY: f64 = 0.6;

/// What the panes need from the app, and what they asked of it.
struct Panes<'a> {
    sidebar: &'a mut Sidebar,
    trash_count: &'a TrashCount,
    terminal: &'a mut TerminalPanel,
    folder: Option<PathBuf>,
    trash_focused: bool,
    /// What the preview pane shows.
    preview: Option<Entry>,
    sidebar_response: SidebarResponse,
    preview_actions: Vec<tab::Action>,
    terminal_response: PanelResponse,
}

struct Viewer<'a> {
    shared: &'a mut Shared,
    panes: Panes<'a>,
    add_requests: &'a mut Vec<NodePath>,
    /// Where the tab buttons were drawn, to tell them from the empty part
    /// of their tab bar.
    tab_buttons: &'a mut Vec<Rect>,
    /// The last browser tab can't be closed, so the panes always have one
    /// to follow and never spread into the space it leaves.
    browser_count: usize,
    pinned: &'a HashSet<u64>,
}

impl Viewer<'_> {
    fn pane_ui(&mut self, ui: &mut Ui, pane: Pane) {
        let panes = &mut self.panes;
        match pane {
            Pane::Places => panes.sidebar.places_ui(
                ui,
                panes.folder.as_ref(),
                panes.trash_count,
                panes.trash_focused,
                &mut self.shared.jobs,
                &mut panes.sidebar_response,
            ),
            Pane::Tree => panes.sidebar.tree_ui(
                ui,
                panes.folder.as_deref(),
                self.shared.show_hidden,
                &mut self.shared.jobs,
                &mut panes.sidebar_response,
            ),
            Pane::Preview => preview::pane(
                ui,
                panes.preview.as_ref(),
                self.shared,
                &mut panes.preview_actions,
            ),
            Pane::Terminal => {
                let response = panes.terminal.ui(ui, panes.folder.as_deref());
                let merged = &mut panes.terminal_response;
                merged.navigate = response.navigate.or(merged.navigate.take());
                merged.close |= response.close;
            }
        }
    }
}

impl TabViewer for Viewer<'_> {
    type Tab = DockTab;

    fn title(&mut self, tab: &mut DockTab) -> WidgetText {
        if self.pinned.contains(&tab.id()) {
            format!("{} {}", icon::PUSH_PIN, tab.title()).into()
        } else {
            tab.title().into()
        }
    }

    fn id(&mut self, tab: &mut DockTab) -> Id {
        Id::new(("tab", tab.id()))
    }

    fn ui(&mut self, ui: &mut Ui, tab: &mut DockTab) {
        match tab.pane() {
            Some(pane) => self.pane_ui(ui, pane),
            None => tab.ui(ui, self.shared),
        }
    }

    fn context_menu(&mut self, ui: &mut Ui, tab: &mut DockTab, _path: NodePath) {
        if tab.pane().is_none() {
            let label = if self.pinned.contains(&tab.id()) {
                format!("{} Unpin tab", icon::PUSH_PIN_SLASH)
            } else {
                format!("{} Pin tab", icon::PUSH_PIN)
            };
            if ui.button(label).clicked() {
                self.shared.app_actions.push(AppAction::TogglePin(tab.id()));
                ui.close();
            }
        }
        if let Some(browser) = tab.browser()
            && ui.button("Duplicate tab").clicked()
        {
            self.shared
                .app_actions
                .push(AppAction::NewTab(browser.path.clone()));
            ui.close();
        }
    }

    /// Tab titles are drop targets for their folder (or the trash).
    fn on_tab_button(&mut self, tab: &mut DockTab, response: &egui::Response) {
        self.tab_buttons.push(response.rect);
        let target = match tab {
            DockTab::Browser(browser) => Some(DropTarget::Folder(browser.path.clone())),
            DockTab::Trash(_) => Some(DropTarget::Trash),
            DockTab::Pane(_)
            | DockTab::Archive(_)
            | DockTab::Editor(_)
            | DockTab::Usage(_)
            | DockTab::Properties(_) => None,
        };
        if let Some(job) = target.and_then(|target| dnd::target(response, target)) {
            self.shared.jobs.start(job);
        }

        // Holding a drag over a tab's title switches to that tab.
        let ctx = &response.ctx;
        let key = Id::new(("dnd-tab-hover", tab.id()));
        if response.dnd_hover_payload::<dnd::FileDrag>().is_some() {
            let now = ctx.input(|i| i.time);
            let since = ctx.data_mut(|d| *d.get_temp_mut_or_insert_with(key, || now));
            if now - since > TAB_SWITCH_DELAY {
                self.shared.app_actions.push(AppAction::FocusTab(tab.id()));
            } else {
                ctx.request_repaint_after(std::time::Duration::from_millis(100));
            }
        } else {
            ctx.data_mut(|d| d.remove::<f64>(key));
        }
    }

    fn on_add(&mut self, path: NodePath) {
        self.add_requests.push(path);
    }

    /// Editors with unsaved changes ask first.
    fn on_close(&mut self, tab: &mut DockTab) -> OnCloseResponse {
        if tab.editor().is_some_and(EditorTab::is_dirty) {
            self.shared.app_actions.push(AppAction::CloseTab(tab.id()));
            OnCloseResponse::Ignore
        } else {
            OnCloseResponse::Close
        }
    }

    /// Pinned tabs can't be closed, nor can the leaf holding them.
    fn is_closeable(&self, tab: &DockTab) -> bool {
        !self.pinned.contains(&tab.id()) && (tab.browser().is_none() || self.browser_count > 1)
    }

    fn scroll_bars(&self, _tab: &DockTab) -> [bool; 2] {
        // Views and panes manage their own scrolling.
        [false, false]
    }
}

impl FilesApp {
    /// Hands an in-app drag to the platform once it leaves the window, so it
    /// can be dropped into other apps.
    fn drag_out(&mut self, ctx: &egui::Context) {
        #[cfg(target_os = "linux")]
        if let Some(wayland) = &self.wayland
            && !self.drag_out_active
            && let Some(payload) = egui::DragAndDrop::payload::<dnd::FileDrag>(ctx)
            && !payload.external
            // Archive entries only exist once extracted.
            && payload.archive.is_none()
            && !payload.link_only
            && ctx.input(|i| i.pointer.primary_down())
            && let Some(pos) = ctx.input(|i| i.pointer.latest_pos())
            && !ctx.content_rect().contains(pos)
            && wayland.start_drag(
                payload.paths.clone(),
                crate::drag_icon::render(ctx, dnd::label(&payload)),
            )
        {
            self.drag_out_active = true;
            egui::DragAndDrop::clear_payload(ctx);
        }
        #[cfg(windows)]
        if !self.drag_out_active
            && let Some(payload) = egui::DragAndDrop::payload::<dnd::FileDrag>(ctx)
            && !payload.external
            && payload.archive.is_none()
            && !payload.link_only
            && ctx.input(|i| i.pointer.primary_down())
            && let Some(pos) = ctx.input(|i| i.pointer.latest_pos())
            && !ctx.content_rect().contains(pos)
        {
            egui::DragAndDrop::clear_payload(ctx);
            // Blocks until the drop; the window doesn't repaint meanwhile.
            let _ = crate::windows_drag::run(&payload.paths);
            // OLE swallowed the button release; raw_input_hook sends it.
            self.drag_out_active = true;
            ctx.request_repaint();
        }
        #[cfg(not(any(target_os = "linux", windows)))]
        let _ = ctx;
    }

    /// Applies file-watcher changes, and refreshes everything when a job finishes
    /// (it may have touched folders that aren't watched).
    fn sync_with_disk(&mut self, ctx: &egui::Context) {
        let changed = self.watcher.poll();
        if trash::watched_dir().is_some_and(|dir| changed.contains(&dir)) {
            self.trash_count.request();
        }
        if !changed.is_empty() {
            self.shared.usage.dirs_changed(&changed);
            self.sidebar.tree.dirs_changed(&changed);
            for (_, tab) in self.dock.iter_all_tabs_mut() {
                tab.dirs_changed(&changed);
            }
        }
        if self.shared.jobs.poll() {
            self.shared.usage.invalidate();
            self.trash_count.request();
            self.sidebar.tree.invalidate();
            for (_, tab) in self.dock.iter_all_tabs_mut() {
                tab.mark_stale();
            }
        }
        for outcome in self.shared.jobs.take_outcomes() {
            match outcome {
                Outcome::Reveal(path) => {
                    for (_, tab) in self.dock.iter_all_tabs_mut() {
                        if let Some(browser) = tab.browser_mut() {
                            browser.reveal(path.clone());
                        }
                    }
                }
                Outcome::Open(path) => {
                    if let Err(err) = crate::fs::open(&path) {
                        eprintln!("Could not open {}: {err}", path.display());
                    }
                }
                Outcome::OpenWith(path, target) => {
                    self.open_with(ctx, open_with::Request::Files(vec![path]), target);
                }
            }
        }
    }

    fn terminal_actions(&mut self, ctx: &egui::Context, response: PanelResponse) {
        if let Some(path) = response.navigate {
            // Following the shell shouldn't take the keys away from it.
            if let Some(tab) = self.active_browser_mut() {
                tab.navigate(path);
            }
        }
        if response.close {
            if let Some(path) = self
                .dock
                .find_tab_from(|t| t.pane() == Some(Pane::Terminal))
            {
                self.dock.remove_tab(path);
            }
            self.terminal.reset();
        } else if !self.has_pane(Pane::Terminal) {
            // Closed with its tab's button; the shells keep running.
            self.terminal.surrender_focus(ctx);
        }
    }

    /// Keeps the panes at the size they were drawn when the dock's shape or
    /// size changes. egui_dock keeps split fractions, so without this, closing
    /// a pane, moving a tab or resizing the window would resize the panes.
    fn refit_on_change(&mut self) {
        let shape: Vec<u8> = self
            .dock
            .main_surface()
            .iter()
            .map(|node| match node {
                Node::Empty => 0,
                Node::Leaf(_) => 1,
                Node::Horizontal(_) => 2,
                Node::Vertical(_) => 3,
            })
            .collect();
        let changed = (shape, self.dock_size);
        if changed != self.dock_shape {
            // Not on the first frame, before anything was drawn at its size.
            if !self.dock_shape.0.is_empty() && self.dock_size.min_elem() > 0.0 {
                let rect = Rect::from_min_size(Pos2::ZERO, self.dock_size);
                fit_panes(
                    self.dock.main_surface_mut(),
                    NodeIndex::root(),
                    rect,
                    self.dock_size,
                );
            }
            self.dock_shape = changed;
        }
    }

    /// There's always a browser tab, so panes have something to follow.
    fn ensure_browser_tab(&mut self) {
        if self
            .dock
            .iter_all_tabs()
            .any(|(_, t)| t.browser().is_some())
        {
            return;
        }
        let tab = self.make_tab(places::home_dir());
        if self
            .dock
            .main_surface()
            .root_node()
            .is_none_or(|n| n.is_empty())
        {
            self.dock.push_to_first_leaf(tab);
        } else {
            // Beside whatever panes are left.
            self.dock
                .split(NodePath::MAIN_ROOT, Split::Right, 0.25, Node::leaf(tab));
        }
    }

    /// Opens files with an app; a file in an archive is extracted first.
    fn open_with(
        &mut self,
        ctx: &egui::Context,
        request: open_with::Request,
        target: open_with::Target,
    ) {
        match request {
            open_with::Request::Files(paths) => {
                for launch in self.shared.open_with.launch(ctx, &target, &paths) {
                    self.terminal.run(launch);
                    if !self.has_pane(Pane::Terminal) {
                        dock_pane(&mut self.dock, Pane::Terminal, self.dock_size);
                    }
                    self.focus_pane(Pane::Terminal);
                }
            }
            open_with::Request::FromArchive(mut job) => {
                job.open_with = Some(target);
                self.shared.jobs.start(crate::jobs::JobKind::Extract(*job));
            }
        }
    }

    /// Asks before quitting while file operations are running or editors
    /// have unsaved changes.
    fn quit_guard(&mut self, ctx: &egui::Context) {
        let running = self.shared.jobs.running();
        let unsaved: Vec<String> = self
            .dock
            .iter_all_tabs()
            .filter_map(|(_, t)| t.editor())
            .filter(|e| e.is_dirty())
            .map(|e| crate::fs::display_name(&e.edit))
            .collect();
        let blocked = running > 0 || !unsaved.is_empty();
        if ctx.input(|i| i.viewport().close_requested()) && blocked && !self.allow_close {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.confirm_quit = true;
        }
        if !self.confirm_quit {
            return;
        }
        if !blocked {
            self.confirm_quit = false;
            return;
        }
        let mut quit = false;
        let mut save_all = false;
        let modal = egui::Modal::new(Id::new("confirm_quit")).show(ctx, |ui| {
            ui.heading("Quit egui-files?");
            if !unsaved.is_empty() {
                ui.label(format!("{} file(s) have unsaved changes:", unsaved.len()));
                for name in unsaved.iter().take(10) {
                    ui.label(format!("  {} {name}", icon::NOTE_PENCIL));
                }
                if unsaved.len() > 10 {
                    ui.weak(format!("  … and {} more", unsaved.len() - 10));
                }
            }
            if running > 0 {
                ui.label(format!(
                    "{running} file operation(s) still running. Quitting cancels them."
                ));
            }
            if let Some(error) = &self.quit_error {
                ui.colored_label(ui.visuals().error_fg_color, error);
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if !unsaved.is_empty() {
                    if ui
                        .button(format!("{} Save all and quit", icon::FLOPPY_DISK))
                        .clicked()
                    {
                        save_all = true;
                    }
                    if ui.button("Quit without saving").clicked() {
                        quit = true;
                    }
                } else if ui.button("Cancel operations and quit").clicked() {
                    quit = true;
                }
                if ui.button("Don't quit").clicked() {
                    self.confirm_quit = false;
                }
            });
        });
        if save_all {
            let errors: Vec<String> = self
                .dock
                .iter_all_tabs_mut()
                .filter_map(|(_, t)| t.editor_mut())
                .filter(|e| e.is_dirty())
                .filter_map(|e| {
                    e.save()
                        .err()
                        .map(|err| format!("{}: {err}", crate::fs::display_name(&e.edit)))
                })
                .collect();
            if errors.is_empty() {
                quit = true;
            } else {
                self.quit_error = Some(errors.join("\n"));
            }
        }
        if quit {
            self.shared.jobs.cancel_all_and_wait();
            self.allow_close = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if modal.should_close() || !self.confirm_quit {
            self.confirm_quit = false;
            self.quit_error = None;
        }
    }
}

impl eframe::App for FilesApp {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.themes.update(&ctx);
        self.shared.images.begin_frame();
        self.shared.videos.begin_frame();
        self.sync_with_disk(&ctx);
        self.shared.usage.tick();
        // Files copied or cut in another app (or by us) paste here too.
        #[cfg(windows)]
        if let Some((paths, cut)) = crate::file_clipboard::files_if_changed(&mut self.clipboard_seq)
        {
            self.shared.clipboard = Some(tab::FileClipboard { paths, cut });
        }
        self.quit_guard(&ctx);
        self.update_active_browser();
        self.global_shortcuts(&ctx);
        // Keys go to the focused tab; with a pane focused, to the browser tab
        // it follows; and with a shell focused, to nobody else.
        self.shared.focused_tab = match self.dock.find_active_focused() {
            _ if self.terminal.has_focus(&ctx) => None,
            _ if self.shared.open_with.is_asking() => None,
            _ if !self.confirm_close.is_empty() || self.confirm_quit => None,
            Some((_, tab)) if tab.pane().is_none() => Some(tab.id()),
            _ => self.active_browser,
        };
        self.shared.preview_shown = self.has_pane(Pane::Preview);

        Panel::top("menu_bar").show(ui, |ui| self.menu_bar(ui));
        self.about.ui(&ctx);
        properties::Properties::windows_ui(&ctx, &mut self.shared);
        if self.themes.editor.open {
            self.themes.editor_ui(&ctx);
        }
        if !self.shared.jobs.is_empty() {
            Panel::bottom("jobs").show(ui, |ui| self.shared.jobs.ui(ui));
        }

        let trash_focused = self.trash_focused();
        let folder = self.focused_folder();
        let preview = self
            .active_browser_mut()
            .and_then(|b| b.preview_entry().cloned());
        let mut add_requests = Vec::new();
        let mut tab_buttons = Vec::new();
        let browser_count = self.browser_count();
        let mut viewer = Viewer {
            shared: &mut self.shared,
            panes: Panes {
                sidebar: &mut self.sidebar,
                trash_count: &self.trash_count,
                terminal: &mut self.terminal,
                folder,
                trash_focused,
                preview,
                sidebar_response: SidebarResponse::default(),
                preview_actions: Vec::new(),
                terminal_response: PanelResponse::default(),
            },
            add_requests: &mut add_requests,
            tab_buttons: &mut tab_buttons,
            browser_count,
            pinned: &self.pinned,
        };
        CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ui, |ui| {
                self.dock_size = ui.available_size();
                for pane in std::mem::take(&mut self.pending_panes) {
                    dock_pane(&mut self.dock, pane, self.dock_size);
                }
                DockArea::new(&mut self.dock)
                    .id(Id::new("dock"))
                    .style(Style::from_egui(ui.style()))
                    .show_add_buttons(true)
                    // `add_button_rect` assumes these.
                    .show_leaf_collapse_buttons(false)
                    .show_leaf_close_all_buttons(true)
                    .show_inside(ui, &mut viewer);
                if self.add_menu.is_none() {
                    add_button_hints(ui, &self.dock);
                }
            });
        let Panes {
            sidebar_response,
            preview_actions,
            terminal_response,
            ..
        } = viewer.panes;

        if let Some((path, new_tab)) = sidebar_response.target {
            self.go_to(path, new_tab);
        }
        if sidebar_response.open_trash {
            self.open_trash();
        }
        if let Some((drive, label)) = sidebar_response.eject {
            self.eject(drive, label);
        }
        if let Some(path) = sidebar_response.analyze {
            self.open_usage(path);
        }
        if let Some(path) = sidebar_response.properties {
            self.open_properties(Source::Files(vec![path]));
        }
        let mut app_actions = Vec::new();
        for action in preview_actions {
            match action {
                tab::Action::Activate(path) => {
                    if let Some(tab) = self.active_browser_mut() {
                        tab.activate(path, &mut app_actions);
                    }
                }
                tab::Action::Edit(path) => app_actions.push(AppAction::Edit(path)),
                _ => {}
            }
        }
        self.shared.app_actions.extend(app_actions);
        self.terminal_actions(&ctx, terminal_response);
        for leaf in add_requests {
            self.add_to_leaf(leaf, NewTab::Browser);
        }
        self.add_menu_ui(&ctx);
        self.tab_bars_ui(&ctx, &tab_buttons);
        self.place_split_ui(&ctx);
        for action in std::mem::take(&mut self.shared.app_actions) {
            match action {
                AppAction::NewTab(path) => self.open_tab(path),
                AppAction::FocusTab(id) => self.focus_tab(id),
                AppAction::TogglePreview => self.show_or_toggle_pane(&ctx, Pane::Preview),
                AppAction::OpenArchive(path) => self.open_archive(path),
                AppAction::OpenWith(request, Some(target)) => {
                    self.open_with(&ctx, request, target);
                }
                AppAction::OpenWith(request, None) => self.shared.open_with.ask(request),
                AppAction::Edit(path) => self.open_editor(path),
                AppAction::CloseTab(id) => self.request_close(id),
                AppAction::TogglePin(id) => self.toggle_pin(id),
                AppAction::CopyFiles { paths, cut } => self.copy_files(&ctx, &paths, cut),
                AppAction::Reveal(path) => self.reveal(path),
                AppAction::DiskUsage(path) => self.open_usage(path),
                AppAction::Properties(source) => self.open_properties(source),
            }
        }
        self.confirm_close_ui(&ctx);
        if let Some((request, target)) = self.shared.open_with.ui(&ctx) {
            self.open_with(&ctx, request, target);
        }
        let empty_drives = self.follow_media();
        let shown = self
            .dock
            .iter_all_tabs()
            .flat_map(|(_, tab)| tab.shown_dirs())
            .filter(|dir| !empty_drives.iter().any(|root| dir.starts_with(root)))
            // Always watched, to keep the sidebar's trash count current.
            .chain(trash::watched_dir())
            .collect();
        self.watcher.set_watched(shown);
        self.ensure_browser_tab();
        // Also after tabs are dragged around or closed.
        self.keep_pinned_first();
        self.refit_on_change();
        self.drag_out(&ctx);
        if !self.os_dropped.is_empty() {
            // winit reports no drop position, so use the focused folder.
            let sources = std::mem::take(&mut self.os_dropped);
            let dest = self.focused_path();
            self.shared
                .jobs
                .start(crate::jobs::JobKind::Copy { sources, dest });
        }
        dnd::paint_preview(&ctx);
    }

    fn raw_input_hook(&mut self, ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        if let Some(theme) = self.system_theme.get() {
            raw_input.system_theme = Some(theme);
        }
        #[cfg(windows)]
        {
            self.paste_key.poll(ctx, raw_input);
            if std::mem::take(&mut self.drag_out_active) {
                // Our own drag dropped back on our window arrives as a winit
                // drop without a position; don't copy it into the focused folder.
                raw_input.dropped_files.clear();
                // egui never saw the button go up (OLE had the mouse).
                raw_input.events.push(egui::Event::PointerButton {
                    pos: egui::pos2(-1.0, -1.0),
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                });
            }
        }
        #[cfg(target_os = "linux")]
        if let Some(wayland) = &self.wayland {
            // Wayland reports logical pixels; egui points also include the zoom.
            let to_points = |p: egui::Pos2| (p.to_vec2() / ctx.zoom_factor()).to_pos2();
            for event in wayland.poll() {
                use crate::{dnd::ExternalEvent, wayland_dnd::DndEvent};
                let event = match event {
                    DndEvent::DragOutEnded => {
                        self.drag_out_active = false;
                        // egui never saw the button go up (the compositor had the
                        // pointer); release it outside the window, over nothing.
                        raw_input.events.push(egui::Event::PointerButton {
                            pos: egui::pos2(-1.0, -1.0),
                            button: egui::PointerButton::Primary,
                            pressed: false,
                            modifiers: egui::Modifiers::NONE,
                        });
                        continue;
                    }
                    DndEvent::External(ExternalEvent::Hover(pos)) => {
                        ExternalEvent::Hover(to_points(pos))
                    }
                    DndEvent::External(ExternalEvent::Drop {
                        pos,
                        paths,
                        internal,
                    }) => ExternalEvent::Drop {
                        pos: to_points(pos),
                        paths,
                        internal,
                    },
                    DndEvent::External(event) => event,
                };
                dnd::inject_external(ctx, raw_input, event);
            }
        }
        self.os_dropped.extend(
            raw_input
                .dropped_files
                .drain(..)
                .map(|file| file.path().to_path_buf())
                .filter(|path| !path.as_os_str().is_empty()),
        );
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        self.terminal.remember_dirs();
        let state = PersistedRef {
            dock: &self.dock,
            show_hidden: self.shared.show_hidden,
            bookmarks: &self.sidebar.bookmarks,
            hide_empty_drives: self.sidebar.hide_empty_drives,
            panes_docked: true,
            tree: &self.sidebar.tree,
            show_thumbnails: self.shared.show_thumbnails,
            icon_size: self.shared.icon_size,
            terminal: &self.terminal,
            video_volume: self.shared.videos.volume(),
            video_quality: self.shared.videos.quality(),
            archive_prefs: &self.shared.archive_prefs,
            open_with: &self.shared.open_with.prefs,
            editor: &self.shared.editor,
            theme: &self.themes.prefs,
            properties: &self.shared.properties.prefs,
            pinned: &self.pinned,
            folder_sizes: self.shared.usage.enabled,
        };
        eframe::set_value(storage, eframe::APP_KEY, &state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn places_split_near_edges() {
        let zone = |x, y| split_zone(egui::vec2(x, y));
        assert_eq!(zone(0.1, 0.5), Some(Split::Left));
        assert_eq!(zone(0.9, 0.5), Some(Split::Right));
        assert_eq!(zone(0.5, 0.05), Some(Split::Above));
        assert_eq!(zone(0.4, 0.95), Some(Split::Below));
        // The nearer edge wins in a corner.
        assert_eq!(zone(0.2, 0.05), Some(Split::Above));
        assert_eq!(zone(0.5, 0.5), None);
        assert_eq!(zone(0.3, 0.7), None);
    }

    #[test]
    fn pinned_tabs_stay_first() {
        let mut dock = DockState::new(vec![browser(1), browser(2), browser(3), browser(4)]);
        let path = dock.find_tab_from(|t| t.id() == 2).unwrap();
        let _ = dock.set_active_tab(path);
        // 9 isn't open any more.
        let mut pinned = HashSet::from([3, 4, 9]);
        pinned_first(&mut dock, &mut pinned);
        let leaf = dock.leaf(NodePath::MAIN_ROOT).unwrap();
        let ids: Vec<u64> = leaf.tabs.iter().map(DockTab::id).collect();
        assert_eq!(ids, [3, 4, 1, 2]);
        assert_eq!(leaf.tabs[leaf.active.0].id(), 2);
        assert_eq!(pinned, HashSet::from([3, 4]));
    }

    fn browser(id: u64) -> DockTab {
        DockTab::Browser(BrowserTab::new(id, PathBuf::from("/")))
    }

    /// Lays the dock out in a `size` window and returns each leaf's first
    /// tab's title and rect.
    fn layout(dock: &mut DockState<DockTab>, size: egui::Vec2) -> Vec<(String, egui::Rect)> {
        struct Blank;
        impl TabViewer for Blank {
            type Tab = DockTab;
            fn title(&mut self, tab: &mut DockTab) -> WidgetText {
                tab.title().into()
            }
            fn ui(&mut self, _ui: &mut Ui, _tab: &mut DockTab) {}
            fn id(&mut self, tab: &mut DockTab) -> Id {
                Id::new(tab.id())
            }
        }
        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ui| {
            DockArea::new(dock).show_inside(ui, &mut Blank);
        });
        output.textures_delta.clear();
        dock.iter_leaves()
            .map(|(_, leaf)| (leaf.tabs[0].title(), leaf.rect))
            .collect()
    }

    fn rect_of(leaves: &[(String, egui::Rect)], pane: Pane) -> egui::Rect {
        leaves
            .iter()
            .find(|(title, _)| title.contains(pane.label()))
            .map(|(_, rect)| *rect)
            .unwrap()
    }

    #[test]
    fn panes_get_their_old_panel_sizes() {
        let size = egui::vec2(1400.0, 900.0);
        let mut dock = DockState::new(vec![browser(1), browser(2)]);
        for pane in Pane::ALL {
            dock_pane(&mut dock, pane, size);
        }
        let leaves = layout(&mut dock, size);
        assert_eq!(leaves.len(), 5, "each pane gets a leaf, the tabs share one");
        let near = |got: f32, want: f32| (got - want).abs() < 12.0;
        let places = rect_of(&leaves, Pane::Places);
        let tree = rect_of(&leaves, Pane::Tree);
        assert!(near(places.width(), 220.0), "{places:?}");
        assert!(near(rect_of(&leaves, Pane::Preview).width(), 300.0));
        assert!(near(rect_of(&leaves, Pane::Terminal).height(), 240.0));
        assert!(places.max.y <= tree.min.y + 1.0, "places above the tree");
        assert!(near(places.width(), tree.width()));

        // A small window caps the panes so the files keep most of it.
        let small = egui::vec2(600.0, 400.0);
        let mut dock = DockState::new(vec![browser(1)]);
        for pane in Pane::ALL {
            dock_pane(&mut dock, pane, small);
        }
        let leaves = layout(&mut dock, small);
        assert!(rect_of(&leaves, Pane::Places).width() <= 0.3 * 600.0 + 1.0);
        assert!(rect_of(&leaves, Pane::Preview).width() <= 0.35 * 600.0 + 1.0);
        assert!(rect_of(&leaves, Pane::Terminal).height() <= 0.4 * 400.0 + 1.0);
    }

    #[test]
    fn opening_a_pane_keeps_the_others_sizes() {
        let size = egui::vec2(1400.0, 900.0);
        let mut dock = DockState::new(vec![browser(1)]);
        dock_pane(&mut dock, Pane::Places, size);
        layout(&mut dock, size);
        // The user drags the sidebar wider.
        if let Node::Horizontal(split) = &mut dock.main_surface_mut()[NodeIndex::root()] {
            split.fraction = 350.0 / 1400.0;
        }
        layout(&mut dock, size);
        for pane in [Pane::Preview, Pane::Terminal] {
            dock_pane(&mut dock, pane, size);
            let leaves = layout(&mut dock, size);
            let places = rect_of(&leaves, Pane::Places).width();
            assert!((places - 350.0).abs() < 12.0, "{pane:?}: {places}");
            let preview = rect_of(&leaves, Pane::Preview).width();
            assert!((preview - 300.0).abs() < 12.0, "{pane:?}: {preview}");
        }
    }

    #[test]
    fn add_button_rect_matches_egui_dock() {
        struct Adds(Vec<NodePath>);
        impl TabViewer for Adds {
            type Tab = DockTab;
            fn title(&mut self, tab: &mut DockTab) -> WidgetText {
                tab.title().into()
            }
            fn ui(&mut self, _ui: &mut Ui, _tab: &mut DockTab) {}
            fn id(&mut self, tab: &mut DockTab) -> Id {
                Id::new(tab.id())
            }
            fn on_add(&mut self, path: NodePath) {
                self.0.push(path);
            }
        }
        let size = egui::vec2(1400.0, 900.0);
        let mut dock = DockState::new(vec![browser(1)]);
        dock_pane(&mut dock, Pane::Places, size);
        dock_pane(&mut dock, Pane::Terminal, size);
        let ctx = egui::Context::default();
        let mut viewer = Adds(Vec::new());
        let mut frame = |dock: &mut DockState<DockTab>, events| {
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, size)),
                events,
                ..Default::default()
            };
            let mut output = ctx.run_ui(input, |ui| {
                DockArea::new(dock)
                    .style(Style::from_egui(ui.style()))
                    .show_add_buttons(true)
                    .show_leaf_collapse_buttons(false)
                    .show_leaf_close_all_buttons(true)
                    .show_inside(ui, &mut viewer);
                // The hints mustn't take the clicks.
                add_button_hints(ui, dock);
            });
            output.textures_delta.clear();
        };
        frame(&mut dock, Vec::new());
        let style = Style::from_egui(&ctx.global_style());
        let leaves: Vec<_> = dock
            .iter_leaves()
            .map(|(path, leaf)| (path, add_button_rect(leaf.rect, &style)))
            .collect();
        assert_eq!(leaves.len(), 3);
        for (path, rect) in &leaves {
            let press = |pressed| egui::Event::PointerButton {
                pos: rect.center(),
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: Modifiers::NONE,
            };
            frame(&mut dock, vec![egui::Event::PointerMoved(rect.center())]);
            let hint = ctx.read_response(Id::new(("add-tab-hint", *path)));
            assert!(hint.is_some_and(|r| r.hovered()), "tooltip hint is hovered");
            frame(&mut dock, vec![press(true)]);
            frame(&mut dock, vec![press(false)]);
        }
        let expected: Vec<_> = leaves.iter().map(|(path, _)| *path).collect();
        assert_eq!(viewer.0, expected);
    }

    #[test]
    fn closing_a_pane_or_resizing_keeps_the_others_sizes() {
        let size = egui::vec2(1400.0, 900.0);
        let mut dock = DockState::new(vec![browser(1)]);
        dock_pane(&mut dock, Pane::Places, size);
        dock_pane(&mut dock, Pane::Preview, size);
        layout(&mut dock, size);
        let preview = dock.find_tab_from(|t| t.pane() == Some(Pane::Preview));
        dock.remove_tab(preview.unwrap());
        // What `refit_on_change` does.
        let refit = |dock: &mut DockState<DockTab>, size| {
            let rect = Rect::from_min_size(Pos2::ZERO, size);
            fit_panes(dock.main_surface_mut(), NodeIndex::root(), rect, size);
        };
        refit(&mut dock, size);
        let leaves = layout(&mut dock, size);
        let places = rect_of(&leaves, Pane::Places).width();
        assert!((places - 220.0).abs() < 12.0, "{places}");

        // A bigger window goes to the files, not the sidebar.
        let bigger = egui::vec2(1900.0, 1000.0);
        refit(&mut dock, bigger);
        let leaves = layout(&mut dock, bigger);
        let places = rect_of(&leaves, Pane::Places).width();
        assert!((places - 220.0).abs() < 12.0, "{places}");
    }

    #[test]
    fn old_layouts_get_the_panes_they_showed() {
        let mut old = BrowserTab::new(1, PathBuf::from("/"));
        old.show_preview = true;
        let dock = DockState::new(vec![DockTab::Browser(old)]);
        assert_eq!(
            migrate(&dock, SidebarMode::Tree, true),
            [Pane::Tree, Pane::Preview, Pane::Terminal]
        );
        let dock = DockState::new(vec![browser(1)]);
        assert!(migrate(&dock, SidebarMode::Hidden, false).is_empty());
        assert_eq!(migrate(&dock, SidebarMode::Both, false), DEFAULT_PANES);
    }
}
