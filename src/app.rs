//! Top-level application: menu bar, places sidebar, and the tab dock.

use std::path::{Path, PathBuf};

use egui::{Button, CentralPanel, Id, Key, Modifiers, Panel, RichText, ScrollArea, Ui, WidgetText};
use egui_dock::{DockArea, DockState, NodePath, Style, TabViewer};
use egui_phosphor::regular as icon;
use serde::{Deserialize, Serialize};

use crate::{
    images::ImageCache,
    places::{self, Place},
    preview::TextCache,
    tab::{AppAction, BrowserTab, Shared},
    tree::{FolderTree, TreeAction},
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
    dock: DockState<BrowserTab>,
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
    dock: &'a DockState<BrowserTab>,
    show_hidden: bool,
    bookmarks: &'a [PathBuf],
    sidebar_mode: SidebarMode,
    tree: &'a FolderTree,
    show_thumbnails: bool,
    icon_size: f32,
}

pub struct FilesApp {
    dock: DockState<BrowserTab>,
    shared: Shared,
    bookmarks: Vec<PathBuf>,
    sidebar_mode: SidebarMode,
    tree: FolderTree,
    next_tab_id: u64,
    places: Vec<Place>,
    drives: Vec<Place>,
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
                dock: DockState::new(vec![BrowserTab::new(1, places::home_dir())]),
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
        let next_tab_id = dock.iter_all_tabs().map(|(_, t)| t.id).max().unwrap_or(0) + 1;

        Self {
            dock,
            shared: Shared {
                show_hidden,
                show_thumbnails,
                icon_size,
                images: ImageCache::new(&cc.egui_ctx),
                texts: TextCache::default(),
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
        }
    }

    fn make_tab(&mut self, path: PathBuf) -> BrowserTab {
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        BrowserTab::new(id, path)
    }

    fn focused_path(&mut self) -> PathBuf {
        self.dock
            .find_active_focused()
            .map(|(_, tab)| tab.path.clone())
            .unwrap_or_else(places::home_dir)
    }

    fn open_tab(&mut self, path: PathBuf) {
        let tab = self.make_tab(path);
        self.dock.push_to_focused_leaf(tab);
    }

    fn close_focused_tab(&mut self) {
        let Some(id) = self.shared.focused_tab else {
            return;
        };
        if let Some(path) = self.dock.find_tab_from(|t| t.id == id) {
            self.dock.remove_tab(path);
        }
    }

    /// Navigates the focused tab, or opens a new one if none is focused.
    fn go_to(&mut self, path: PathBuf, new_tab: bool) {
        if !new_tab && let Some((_, tab)) = self.dock.find_active_focused() {
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
        let current = self
            .dock
            .find_active_focused()
            .map(|(_, tab)| tab.path.clone());
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
            response
        };

        ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            ui.with_layout(egui::Layout::top_down_justified(egui::Align::Min), |ui| {
                ui.label(RichText::new("Places").small().weak());
                for place in &self.places {
                    place_button(ui, place.icon, &place.label, &place.path);
                }

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
    }
}

struct Viewer<'a> {
    shared: &'a mut Shared,
    add_requests: &'a mut Vec<NodePath>,
}

impl TabViewer for Viewer<'_> {
    type Tab = BrowserTab;

    fn title(&mut self, tab: &mut BrowserTab) -> WidgetText {
        tab.title().into()
    }

    fn id(&mut self, tab: &mut BrowserTab) -> Id {
        Id::new(("tab", tab.id))
    }

    fn ui(&mut self, ui: &mut Ui, tab: &mut BrowserTab) {
        tab.ui(ui, self.shared);
    }

    fn context_menu(&mut self, ui: &mut Ui, tab: &mut BrowserTab, _path: NodePath) {
        if ui.button("Duplicate tab").clicked() {
            self.shared
                .app_actions
                .push(AppAction::NewTab(tab.path.clone()));
            ui.close();
        }
    }

    fn on_add(&mut self, path: NodePath) {
        self.add_requests.push(path);
    }

    fn scroll_bars(&self, _tab: &BrowserTab) -> [bool; 2] {
        // Views manage their own scrolling.
        [false, false]
    }
}

impl eframe::App for FilesApp {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.shared.images.begin_frame();
        self.shared.focused_tab = self.dock.find_active_focused().map(|(_, tab)| tab.id);
        self.global_shortcuts(&ctx);

        Panel::top("menu_bar").show(ui, |ui| self.menu_bar(ui));
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
                AppAction::RefreshAll => {
                    self.tree.invalidate();
                    for (_, tab) in self.dock.iter_all_tabs_mut() {
                        tab.invalidate();
                    }
                }
            }
        }
        if self.dock.iter_all_tabs().next().is_none() {
            let tab = self.make_tab(places::home_dir());
            self.dock.push_to_first_leaf(tab);
        }
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
