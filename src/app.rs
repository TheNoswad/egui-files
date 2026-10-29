//! Top-level application: menu bar, places sidebar, and the tab dock.

use std::path::{Path, PathBuf};

use egui::{Button, CentralPanel, Id, Key, Modifiers, Panel, RichText, ScrollArea, Ui, WidgetText};
use egui_dock::{DockArea, DockState, NodePath, Style, TabViewer};
use egui_phosphor::regular as icon;
use serde::{Deserialize, Serialize};

use crate::{
    dnd::{self, DropTarget},
    dock_tab::DockTab,
    images::ImageCache,
    jobs::Jobs,
    places::{self, Place},
    preview::TextCache,
    tab::{AppAction, BrowserTab, Shared},
    trash::{self, TrashCount, TrashTab},
    tree::{FolderTree, TreeAction},
    watcher::DirWatcher,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
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
    #[serde(default)]
    sidebar_mode: SidebarMode,
    #[serde(default)]
    tree: FolderTree,
    #[serde(default = "default_true")]
    show_thumbnails: bool,
    #[serde(default = "default_icon_size")]
    icon_size: f32,
}

fn default_true() -> bool {
    true
}

fn default_icon_size() -> f32 {
    64.0
}

#[derive(Serialize)]
struct PersistedRef<'a> {
    dock: &'a DockState<DockTab>,
    show_hidden: bool,
    bookmarks: &'a [PathBuf],
    sidebar_mode: SidebarMode,
    tree: &'a FolderTree,
    show_thumbnails: bool,
    icon_size: f32,
}

pub struct FilesApp {
    dock: DockState<DockTab>,
    shared: Shared,
    bookmarks: Vec<PathBuf>,
    sidebar_mode: SidebarMode,
    tree: FolderTree,
    next_tab_id: u64,
    places: Vec<Place>,
    drives: Vec<Place>,
    watcher: DirWatcher,
    trash_count: TrashCount,
    /// Showing the "operations still running" dialog.
    confirm_quit: bool,
    /// The user confirmed quitting with operations running.
    allow_close: bool,
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
                sidebar_mode: SidebarMode::default(),
                tree: FolderTree::default(),
                show_thumbnails: default_true(),
                icon_size: default_icon_size(),
            });
        let Persisted {
            dock,
            show_hidden,
            bookmarks,
            sidebar_mode,
            tree,
            show_thumbnails,
            icon_size,
        } = persisted;
        let next_tab_id = dock.iter_all_tabs().map(|(_, t)| t.id()).max().unwrap_or(0) + 1;

        Self {
            dock,
            shared: Shared {
                show_hidden,
                show_thumbnails,
                icon_size,
                images: ImageCache::new(&cc.egui_ctx),
                texts: TextCache::default(),
                jobs: Jobs::new(&cc.egui_ctx),
                clipboard: None,
                focused_tab: None,
                app_actions: Vec::new(),
            },
            bookmarks,
            sidebar_mode,
            tree,
            next_tab_id,
            places: places::standard_places(),
            drives: places::drives(),
            watcher: DirWatcher::new(&cc.egui_ctx),
            trash_count: TrashCount::new(&cc.egui_ctx),
            confirm_quit: false,
            allow_close: false,
        }
    }

    fn next_id(&mut self) -> u64 {
        self.next_tab_id += 1;
        self.next_tab_id - 1
    }

    fn make_tab(&mut self, path: PathBuf) -> DockTab {
        DockTab::Browser(BrowserTab::new(self.next_id(), path))
    }

    /// The focused browser tab's folder, if a browser tab is focused.
    fn focused_folder(&mut self) -> Option<PathBuf> {
        self.dock
            .find_active_focused()
            .and_then(|(_, tab)| tab.browser().map(|b| b.path.clone()))
    }

    fn focused_path(&mut self) -> PathBuf {
        self.focused_folder().unwrap_or_else(places::home_dir)
    }

    /// Focuses the trash tab, opening one if needed.
    fn open_trash(&mut self) {
        if let Some(path) = self.dock.find_tab_from(DockTab::is_trash) {
            let _ = self.dock.set_active_tab(path);
            self.dock.set_focused_node_and_surface(path.node_path());
        } else {
            let tab = DockTab::Trash(TrashTab::new(self.next_id()));
            self.dock.push_to_focused_leaf(tab);
        }
    }

    fn open_tab(&mut self, path: PathBuf) {
        let tab = self.make_tab(path);
        self.dock.push_to_focused_leaf(tab);
    }

    fn close_focused_tab(&mut self) {
        let Some(id) = self.shared.focused_tab else {
            return;
        };
        if let Some(path) = self.dock.find_tab_from(|t| t.id() == id) {
            self.dock.remove_tab(path);
        }
    }

    /// Navigates the focused tab, or opens a new one if none is focused.
    fn go_to(&mut self, path: PathBuf, new_tab: bool) {
        if !new_tab && let Some((_, DockTab::Browser(tab))) = self.dock.find_active_focused() {
            tab.navigate(path);
        } else {
            self.open_tab(path);
        }
    }

    fn global_shortcuts(&mut self, ctx: &egui::Context) {
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
                if ui.button(format!("{} Open trash", icon::TRASH)).clicked() {
                    self.open_trash();
                }
                ui.separator();
                if ui.button("Quit").clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
            ui.menu_button("View", |ui| {
                ui.checkbox(&mut self.shared.show_hidden, "Show hidden files (Ctrl+H)");
                ui.checkbox(&mut self.shared.show_thumbnails, "Show thumbnails");
                ui.add(
                    egui::Slider::new(&mut self.shared.icon_size, 32.0..=192.0).text("Icon size"),
                );
                ui.separator();
                ui.label("Theme");
                egui::widgets::global_theme_preference_buttons(ui);
                ui.separator();
                ui.label("Sidebar");
                for (mode, label) in [
                    (SidebarMode::Places, "Places"),
                    (SidebarMode::Tree, "Folder tree"),
                    (SidebarMode::Both, "Places and folder tree"),
                    (SidebarMode::Hidden, "Hidden"),
                ] {
                    ui.radio_value(&mut self.sidebar_mode, mode, label);
                }
            });
        });
    }

    fn sidebar(&mut self, ui: &mut Ui) {
        let current = self.focused_folder();
        // (path, open in new tab)
        let mut target: Option<(PathBuf, bool)> = None;
        match self.sidebar_mode {
            SidebarMode::Places => self.places_ui(ui, current.as_ref(), &mut target),
            SidebarMode::Tree => self.tree_ui(ui, current.as_deref(), &mut target),
            SidebarMode::Both => {
                Panel::top("sidebar_places")
                    .resizable(true)
                    .default_size(260.0)
                    .show(ui, |ui| self.places_ui(ui, current.as_ref(), &mut target));
                CentralPanel::default()
                    .frame(egui::Frame::NONE)
                    .show(ui, |ui| self.tree_ui(ui, current.as_deref(), &mut target));
            }
            SidebarMode::Hidden => {}
        }
        if let Some((path, new_tab)) = target {
            self.go_to(path, new_tab);
        }
    }

    fn tree_ui(
        &mut self,
        ui: &mut Ui,
        current: Option<&Path>,
        target: &mut Option<(PathBuf, bool)>,
    ) {
        ui.add_space(4.0);
        ui.label(RichText::new("Folders").small().weak());
        let roots: Vec<&Place> = self.places.iter().take(1).chain(&self.drives).collect();
        match self.tree.ui(ui, &roots, current, self.shared.show_hidden) {
            Some(TreeAction::Navigate(path)) => *target = Some((path, false)),
            Some(TreeAction::OpenInNewTab(path)) => *target = Some((path, true)),
            Some(TreeAction::Drop(job)) => self.shared.jobs.start(job),
            None => {}
        }
    }

    fn places_ui(
        &mut self,
        ui: &mut Ui,
        current: Option<&PathBuf>,
        target: &mut Option<(PathBuf, bool)>,
    ) {
        let mut remove_bookmark = None;
        let mut trash_clicked = false;
        let mut dropped = None;
        let mut trash_dropped = None;
        let trash_focused = matches!(
            self.dock.find_active_focused(),
            Some((_, DockTab::Trash(_)))
        );

        let mut place_button = |ui: &mut Ui, glyph: &str, label: &str, path: &PathBuf| {
            let selected = current == Some(path);
            let response = ui
                .add(Button::selectable(selected, format!("{glyph}  {label}")))
                .on_hover_text(path.display().to_string());
            if response.clicked() {
                *target = Some((path.clone(), false));
            } else if response.middle_clicked() {
                *target = Some((path.clone(), true));
            }
            if let Some(job) = dnd::target(&response, DropTarget::Folder(path.clone())) {
                dropped = Some(job);
            }
            response
        };

        ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            ui.with_layout(egui::Layout::top_down_justified(egui::Align::Min), |ui| {
                ui.label(RichText::new("Places").small().weak());
                for place in &self.places {
                    place_button(ui, place.icon, &place.label, &place.path);
                }
                let count = self
                    .trash_count
                    .get()
                    .filter(|&n| n > 0)
                    .map(|n| n.to_string())
                    .unwrap_or_default();
                let trash = ui.add(
                    Button::selectable(trash_focused, format!("{}  Trash", icon::TRASH))
                        .right_text(RichText::new(count).weak()),
                );
                trash_clicked = trash.clicked();
                trash_dropped = dnd::target(&trash, DropTarget::Trash);

                ui.add_space(8.0);
                ui.label(RichText::new("Bookmarks").small().weak());
                for (i, path) in self.bookmarks.iter().enumerate() {
                    let label = crate::fs::display_name(path);
                    place_button(ui, icon::BOOKMARK_SIMPLE, &label, path).context_menu(|ui| {
                        if ui.button("Remove bookmark").clicked() {
                            remove_bookmark = Some(i);
                            ui.close();
                        }
                    });
                }
                if let Some(path) = current
                    && !self.bookmarks.contains(path)
                    && ui
                        .button(format!("{}  Bookmark this folder", icon::PLUS))
                        .clicked()
                {
                    self.bookmarks.push(path.clone());
                }

                ui.add_space(8.0);
                ui.label(RichText::new("Drives").small().weak());
                for drive in &self.drives {
                    place_button(ui, drive.icon, &drive.label, &drive.path);
                }
            });
        });

        if let Some(i) = remove_bookmark {
            self.bookmarks.remove(i);
        }
        if trash_clicked {
            self.open_trash();
        }
        if let Some(job) = dropped.or(trash_dropped) {
            self.shared.jobs.start(job);
        }
    }
}

struct Viewer<'a> {
    shared: &'a mut Shared,
    add_requests: &'a mut Vec<NodePath>,
}

impl TabViewer for Viewer<'_> {
    type Tab = DockTab;

    fn title(&mut self, tab: &mut DockTab) -> WidgetText {
        tab.title().into()
    }

    fn id(&mut self, tab: &mut DockTab) -> Id {
        Id::new(("tab", tab.id()))
    }

    fn ui(&mut self, ui: &mut Ui, tab: &mut DockTab) {
        tab.ui(ui, self.shared);
    }

    fn context_menu(&mut self, ui: &mut Ui, tab: &mut DockTab, _path: NodePath) {
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
        let target = match tab {
            DockTab::Browser(browser) => DropTarget::Folder(browser.path.clone()),
            DockTab::Trash(_) => DropTarget::Trash,
        };
        if let Some(job) = dnd::target(response, target) {
            self.shared.jobs.start(job);
        }
    }

    fn on_add(&mut self, path: NodePath) {
        self.add_requests.push(path);
    }

    fn scroll_bars(&self, _tab: &DockTab) -> [bool; 2] {
        // Views manage their own scrolling.
        [false, false]
    }
}

impl FilesApp {
    /// Applies file-watcher changes, and refreshes everything when a job finishes
    /// (it may have touched folders that aren't watched).
    fn sync_with_disk(&mut self) {
        let changed = self.watcher.poll();
        if trash::watched_dir().is_some_and(|dir| changed.contains(&dir)) {
            self.trash_count.request();
        }
        if !changed.is_empty() {
            self.tree.dirs_changed(&changed);
            for (_, tab) in self.dock.iter_all_tabs_mut() {
                tab.dirs_changed(&changed);
            }
        }
        if self.shared.jobs.poll() {
            self.trash_count.request();
            self.tree.invalidate();
            for (_, tab) in self.dock.iter_all_tabs_mut() {
                tab.mark_stale();
            }
        }
    }

    /// Asks before quitting while file operations are running.
    fn quit_guard(&mut self, ctx: &egui::Context) {
        let running = self.shared.jobs.running();
        if ctx.input(|i| i.viewport().close_requested()) && running > 0 && !self.allow_close {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.confirm_quit = true;
        }
        if !self.confirm_quit {
            return;
        }
        if running == 0 {
            self.confirm_quit = false;
            return;
        }
        let modal = egui::Modal::new(Id::new("confirm_quit")).show(ctx, |ui| {
            ui.heading("Quit egui-files?");
            ui.label(format!(
                "{running} file operation(s) still running. Quitting cancels them."
            ));
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Cancel operations and quit").clicked() {
                    self.shared.jobs.cancel_all_and_wait();
                    self.allow_close = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                if ui.button("Keep running").clicked() {
                    self.confirm_quit = false;
                }
            });
        });
        if modal.should_close() {
            self.confirm_quit = false;
        }
    }
}

impl eframe::App for FilesApp {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.shared.images.begin_frame();
        self.sync_with_disk();
        self.quit_guard(&ctx);
        self.shared.focused_tab = self.dock.find_active_focused().map(|(_, tab)| tab.id());
        self.global_shortcuts(&ctx);

        Panel::top("menu_bar").show(ui, |ui| self.menu_bar(ui));
        if !self.shared.jobs.is_empty() {
            Panel::bottom("jobs").show(ui, |ui| self.shared.jobs.ui(ui));
        }
        if self.sidebar_mode != SidebarMode::Hidden {
            Panel::left("sidebar")
                .resizable(true)
                .default_size(220.0)
                .show(ui, |ui| self.sidebar(ui));
        }

        let mut add_requests = Vec::new();
        CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ui, |ui| {
                DockArea::new(&mut self.dock)
                    .id(Id::new("dock"))
                    .style(Style::from_egui(ui.style()))
                    .show_add_buttons(true)
                    .show_leaf_collapse_buttons(false)
                    .show_inside(
                        ui,
                        &mut Viewer {
                            shared: &mut self.shared,
                            add_requests: &mut add_requests,
                        },
                    );
            });

        for node in add_requests {
            let path = self.focused_path();
            let tab = self.make_tab(path);
            self.dock.set_focused_node_and_surface(node);
            self.dock.push_to_focused_leaf(tab);
        }
        for action in std::mem::take(&mut self.shared.app_actions) {
            match action {
                AppAction::NewTab(path) => self.open_tab(path),
            }
        }
        let shown = self
            .dock
            .iter_all_tabs()
            .flat_map(|(_, tab)| tab.shown_dirs())
            // Always watched, to keep the sidebar's trash count current.
            .chain(trash::watched_dir())
            .collect();
        self.watcher.set_watched(shown);
        if self.dock.iter_all_tabs().next().is_none() {
            let tab = self.make_tab(places::home_dir());
            self.dock.push_to_first_leaf(tab);
        }
        dnd::paint_preview(&ctx);
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        let state = PersistedRef {
            dock: &self.dock,
            show_hidden: self.shared.show_hidden,
            bookmarks: &self.bookmarks,
            sidebar_mode: self.sidebar_mode,
            tree: &self.tree,
            show_thumbnails: self.shared.show_thumbnails,
            icon_size: self.shared.icon_size,
        };
        eframe::set_value(storage, eframe::APP_KEY, &state);
    }
}
