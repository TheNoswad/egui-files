//! A single browser tab: location, history, listing, selection and actions.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

use egui::{
    Align, Button, CentralPanel, Context, Id, Key, Layout, Modal, Modifiers, Panel, RichText,
    TextEdit, Ui,
};
use egui_phosphor::regular as icon;
use serde::{Deserialize, Serialize};

use crate::{
    archive::{self, CompressDialog, ExtractDialog, ExtractJob, ExtractMode},
    dnd::{self, DropTarget},
    fs::{self, Entry, Sort, SortKey},
    images::ImageCache,
    jobs::{JobKind, Jobs},
    open_with::{self, OpenWith},
    places,
    preview::TextCache,
    video::Videos,
    views::{self, ViewMode},
};

/// State shared by every tab, owned by the app.
pub struct Shared {
    pub show_hidden: bool,
    pub show_thumbnails: bool,
    /// Icon/thumbnail size in the grid view, in points.
    pub icon_size: f32,
    pub clipboard: Option<FileClipboard>,
    pub focused_tab: Option<u64>,
    /// The preview pane is open somewhere in the dock.
    pub preview_shown: bool,
    pub app_actions: Vec<AppAction>,
    pub images: ImageCache,
    pub videos: Videos,
    pub texts: TextCache,
    pub jobs: Jobs,
    pub archives: archive::Listings,
    pub archive_prefs: archive::Prefs,
    pub open_with: OpenWith,
}

pub struct FileClipboard {
    pub paths: Vec<PathBuf>,
    pub cut: bool,
}

/// Requests a tab cannot fulfil on its own.
pub enum AppAction {
    NewTab(PathBuf),
    /// Bring the tab with this id to the front.
    FocusTab(u64),
    TogglePreview,
    /// Show an archive's contents in a tab.
    OpenArchive(PathBuf),
    /// Open files with an app, or ask which when `None`.
    OpenWith(open_with::Request, Option<open_with::Target>),
}

/// Everything a view or toolbar can ask the tab to do.
pub enum Action {
    /// Index into the visible list.
    Select {
        index: usize,
        modifiers: Modifiers,
    },
    SelectAll,
    MoveCursor {
        delta: isize,
        extend: bool,
    },
    Activate(PathBuf),
    ActivateCursor,
    OpenInNewTab(PathBuf),
    Navigate(PathBuf),
    /// Navigate to `dir` and select `select` inside it (Miller-column clicks).
    NavigateSelect {
        dir: PathBuf,
        select: PathBuf,
    },
    /// Navigate into the selected folder and select its first item.
    EnterSelected,
    Back,
    Forward,
    Up,
    Refresh,
    SortBy(SortKey),
    SetView(ViewMode),
    EditPath,
    FocusFilter,
    StartRename,
    CommitRename,
    NewFolder,
    /// Creates an empty file, or a copy of the given template.
    NewFile(Option<PathBuf>),
    Trash,
    Copy,
    Cut,
    Paste,
    /// Pastes into a subfolder instead of the current folder.
    PasteInto(PathBuf),
    CopyPath,
    /// Copies the path of the folder itself, ignoring the selection.
    CopyFolderPath,
    ClearSelection,
    /// Start a file operation, e.g. from a drag-and-drop.
    StartJob(JobKind),
    ToggleHidden,
    TogglePreview,
    /// Open the selection with an app, or ask which when `None`.
    OpenWith(Option<open_with::Target>),
    /// Extract the selected archives next to themselves.
    Extract(ExtractMode),
    /// Ask where to extract the selected archives.
    ExtractTo,
    /// Ask how to compress the selection.
    Compress,
    /// Ask how to compress the folder itself.
    CompressFolder,
}

struct Rename {
    path: PathBuf,
    name: String,
    just_opened: bool,
}

#[derive(Serialize, Deserialize)]
pub struct BrowserTab {
    pub id: u64,
    pub path: PathBuf,
    #[serde(default)]
    pub view: ViewMode,
    /// Only read, to carry the preview over from layouts saved before it
    /// became a pane of its own.
    #[serde(default, skip_serializing)]
    pub show_preview: bool,
    #[serde(default)]
    pub sort: Sort,
    #[serde(default)]
    back: Vec<PathBuf>,
    #[serde(default)]
    forward: Vec<PathBuf>,

    #[serde(skip)]
    pub filter: String,
    #[serde(skip)]
    entries: Option<Vec<Entry>>,
    /// The listing is out of date and should be re-read, keeping the cursor.
    #[serde(skip)]
    stale: bool,
    #[serde(skip)]
    error: Option<String>,
    #[serde(skip)]
    pub selection: HashSet<PathBuf>,
    #[serde(skip)]
    anchor: Option<usize>,
    #[serde(skip)]
    pub cursor: Option<usize>,
    /// Row the view should scroll into view this frame.
    #[serde(skip)]
    pub scroll_to: Option<usize>,
    #[serde(skip)]
    path_edit: Option<String>,
    #[serde(skip)]
    focus_path_edit: bool,
    #[serde(skip)]
    rename: Option<Rename>,
    /// Listings of the extra folders shown by the columns view.
    #[serde(skip)]
    column_cache: HashMap<PathBuf, Vec<Entry>>,
    #[serde(skip)]
    compress: Option<CompressDialog>,
    #[serde(skip)]
    extract: Option<ExtractDialog>,
    /// Select this once the listing has it, e.g. a just-made archive.
    #[serde(skip)]
    reveal: Option<PathBuf>,
}

impl BrowserTab {
    pub fn new(id: u64, path: PathBuf) -> Self {
        Self {
            id,
            path,
            view: ViewMode::default(),
            show_preview: false,
            sort: Sort::default(),
            back: Vec::new(),
            forward: Vec::new(),
            filter: String::new(),
            entries: None,
            stale: false,
            error: None,
            selection: HashSet::new(),
            anchor: None,
            cursor: None,
            scroll_to: None,
            path_edit: None,
            focus_path_edit: false,
            rename: None,
            column_cache: HashMap::new(),
            compress: None,
            extract: None,
            reveal: None,
        }
    }

    /// Selects `path` once this tab shows its folder's new contents.
    pub fn reveal(&mut self, path: PathBuf) {
        if path.parent() == Some(self.path.as_path()) {
            self.reveal = Some(path);
        }
    }

    pub fn title(&self) -> String {
        format!("{} {}", icon::FOLDER, fs::display_name(&self.path))
    }

    pub fn entries(&self) -> &[Entry] {
        self.entries.as_deref().unwrap_or_default()
    }

    /// Indices into [`Self::entries`] that pass the hidden-file and name filters.
    pub fn visible(&self, show_hidden: bool) -> Vec<usize> {
        let filter = self.filter.to_lowercase();
        self.entries()
            .iter()
            .enumerate()
            .filter(|(_, e)| show_hidden || !e.hidden)
            .filter(|(_, e)| filter.is_empty() || e.name.to_lowercase().contains(&filter))
            .map(|(i, _)| i)
            .collect()
    }

    pub fn mark_stale(&mut self) {
        self.stale = true;
    }

    /// Reacts to the contents of `dirs` changing on disk.
    pub fn dirs_changed(&mut self, dirs: &HashSet<PathBuf>) {
        if dirs.contains(&self.path) {
            self.stale = true;
        }
        self.column_cache.retain(|dir, _| !dirs.contains(dir));
    }

    /// Folders this tab is showing, for the file watcher.
    pub fn shown_dirs(&self) -> impl Iterator<Item = &PathBuf> {
        std::iter::once(&self.path).chain(self.column_cache.keys())
    }

    fn reload(&mut self) {
        self.column_cache.clear();
        // If the folder was deleted or renamed away, fall back to the nearest parent.
        let mut gone = None;
        if !self.path.is_dir()
            && let Some(existing) = self.path.ancestors().skip(1).find(|p| p.is_dir())
        {
            gone = Some(self.path.clone());
            self.navigate(existing.to_path_buf());
        }
        match fs::read_dir(&self.path) {
            Ok(mut entries) => {
                fs::sort_entries(&mut entries, self.sort);
                self.selection
                    .retain(|p| entries.iter().any(|e| &e.path == p));
                self.entries = Some(entries);
                self.error = None;
            }
            Err(err) => {
                self.entries = Some(Vec::new());
                self.error = Some(format!("Cannot read {}: {err}", self.path.display()));
            }
        }
        if let Some(gone) = gone {
            self.error = Some(format!("{} no longer exists", gone.display()));
        }
    }

    /// Re-reads the folder, keeping the cursor and anchor on the same items.
    fn refresh(&mut self, show_hidden: bool) {
        self.stale = false;
        let path = self.path.clone();
        let old_visible = self.visible(show_hidden);
        let path_at = |tab: &Self, index: Option<usize>| {
            index
                .and_then(|i| old_visible.get(i))
                .map(|&i| tab.entries()[i].path.clone())
        };
        let (cursor, anchor) = (path_at(self, self.cursor), path_at(self, self.anchor));
        self.reload();
        if self.path != path {
            return;
        }
        let visible = self.visible(show_hidden);
        let index_of = |tab: &Self, path: Option<PathBuf>| {
            path.and_then(|p| visible.iter().position(|&i| tab.entries()[i].path == p))
        };
        self.cursor = index_of(self, cursor);
        self.anchor = index_of(self, anchor);
    }

    pub fn navigate(&mut self, path: PathBuf) {
        if path == self.path {
            return;
        }
        if !path.is_dir() {
            self.error = Some(format!("Not a folder: {}", path.display()));
            return;
        }
        let previous = std::mem::replace(&mut self.path, path);
        self.back.push(previous);
        self.forward.clear();
        self.reset_location_state();
    }

    fn reset_location_state(&mut self) {
        self.entries = None;
        self.column_cache.clear();
        self.error = None;
        self.selection.clear();
        self.anchor = None;
        self.cursor = None;
        self.filter.clear();
        self.path_edit = None;
    }

    pub fn selected_paths(&self) -> Vec<PathBuf> {
        self.entries()
            .iter()
            .filter(|e| self.selection.contains(&e.path))
            .map(|e| e.path.clone())
            .collect()
    }

    /// The single selected entry, which the columns view previews.
    pub fn preview_entry(&self) -> Option<&Entry> {
        if self.selection.len() != 1 {
            return None;
        }
        self.entries()
            .iter()
            .find(|e| self.selection.contains(&e.path))
    }

    /// Cached listing of a folder shown as an extra column (see [`Self::prepare_columns`]).
    pub fn column_listing(&self, dir: &Path) -> &[Entry] {
        self.column_cache.get(dir).map_or(&[], Vec::as_slice)
    }

    /// Loads the ancestor and preview listings the columns view needs and drops the rest.
    fn prepare_columns(&mut self) {
        let mut needed: Vec<PathBuf> = self.path.ancestors().skip(1).map(PathBuf::from).collect();
        if let Some(entry) = self.preview_entry().filter(|e| e.is_dir) {
            needed.push(entry.path.clone());
        }
        self.column_cache.retain(|dir, _| needed.contains(dir));
        for dir in needed {
            self.column_cache.entry(dir).or_insert_with_key(|dir| {
                let mut entries = fs::read_dir(dir).unwrap_or_default();
                fs::sort_entries(&mut entries, self.sort);
                entries
            });
        }
    }

    /// Selects `path` after a reload, e.g. the folder we just came up from.
    fn select_path(&mut self, path: &Path, show_hidden: bool) {
        if self.entries.is_none() {
            self.reload();
        }
        let visible = self.visible(show_hidden);
        if let Some(index) = visible.iter().position(|&i| self.entries()[i].path == path) {
            self.selection = HashSet::from([path.to_path_buf()]);
            self.anchor = Some(index);
            self.cursor = Some(index);
            self.scroll_to = Some(index);
        }
    }

    // -----------------------------------------------------------------------
    // UI

    pub fn ui(&mut self, ui: &mut Ui, shared: &mut Shared) {
        ui.style_mut().interaction.selectable_labels = false;
        if self.entries.is_none() {
            self.reload();
        } else if self.stale {
            self.refresh(shared.show_hidden);
        }
        if let Some(path) = self.reveal.take() {
            self.select_path(&path, shared.show_hidden);
        }

        let mut actions = Vec::new();
        if shared.focused_tab == Some(self.id) {
            self.keyboard(ui.ctx(), &mut actions);
        }

        Panel::top(Id::new(("toolbar", self.id)))
            .show(ui, |ui| self.toolbar(ui, shared, &mut actions));
        if self.view == ViewMode::Columns {
            self.prepare_columns();
        }
        let visible = self.visible(shared.show_hidden);
        Panel::bottom(Id::new(("status", self.id))).show(ui, |ui| self.status_bar(ui, &visible));
        CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ui, |ui| {
                // Registered before the view so items, drawn on top, take
                // precedence; this only receives clicks on empty space.
                let background = ui.interact(
                    ui.max_rect(),
                    Id::new(("background", self.id)),
                    egui::Sense::click(),
                );
                if visible.is_empty() && self.view != ViewMode::Columns {
                    ui.centered_and_justified(|ui| {
                        ui.weak(if self.filter.is_empty() {
                            "Empty folder"
                        } else {
                            "No matches"
                        });
                    });
                } else {
                    views::show(ui, self, &visible, shared, &mut actions);
                }
                if background.clicked() && !ui.input(|i| i.modifiers.command || i.modifiers.shift) {
                    actions.push(Action::ClearSelection);
                }
                // After the view, so folders under the pointer take the drop first.
                if let Some(job) = dnd::target(&background, DropTarget::Folder(self.path.clone())) {
                    actions.push(Action::StartJob(job));
                }
                background.context_menu(|ui| self.background_menu(ui, shared, &mut actions));
            });
        self.rename_modal(ui.ctx(), &mut actions);
        self.archive_dialogs(ui.ctx(), shared);
        self.scroll_to = None;

        if !actions.is_empty() {
            ui.ctx().request_repaint();
        }
        for action in actions {
            self.apply(action, ui.ctx(), shared);
        }
    }

    fn keyboard(&mut self, ctx: &Context, actions: &mut Vec<Action>) {
        if ctx.text_edit_focused()
            || self.rename.is_some()
            || self.compress.is_some()
            || self.extract.is_some()
        {
            return;
        }
        let cmd = Modifiers::COMMAND;
        let cmd_shift = Modifiers::COMMAND | Modifiers::SHIFT;
        ctx.input_mut(|i| {
            let mut key = |mods, key| i.consume_key(mods, key);
            if key(Modifiers::ALT, Key::ArrowLeft) {
                actions.push(Action::Back);
            }
            if key(Modifiers::ALT, Key::ArrowRight) {
                actions.push(Action::Forward);
            }
            if key(Modifiers::ALT, Key::ArrowUp) || key(Modifiers::NONE, Key::Backspace) {
                actions.push(Action::Up);
            }
            if self.view == ViewMode::Columns {
                if key(Modifiers::NONE, Key::ArrowLeft) {
                    actions.push(Action::Up);
                }
                if key(Modifiers::NONE, Key::ArrowRight) {
                    actions.push(Action::EnterSelected);
                }
            }
            if key(Modifiers::SHIFT, Key::ArrowDown) {
                actions.push(Action::MoveCursor {
                    delta: 1,
                    extend: true,
                });
            }
            if key(Modifiers::SHIFT, Key::ArrowUp) {
                actions.push(Action::MoveCursor {
                    delta: -1,
                    extend: true,
                });
            }
            if key(Modifiers::NONE, Key::ArrowDown) {
                actions.push(Action::MoveCursor {
                    delta: 1,
                    extend: false,
                });
            }
            if key(Modifiers::NONE, Key::ArrowUp) {
                actions.push(Action::MoveCursor {
                    delta: -1,
                    extend: false,
                });
            }
            if key(Modifiers::NONE, Key::Home) {
                actions.push(Action::MoveCursor {
                    delta: isize::MIN,
                    extend: false,
                });
            }
            if key(Modifiers::NONE, Key::End) {
                actions.push(Action::MoveCursor {
                    delta: isize::MAX,
                    extend: false,
                });
            }
            if key(Modifiers::NONE, Key::Enter) {
                actions.push(Action::ActivateCursor);
            }
            if key(Modifiers::NONE, Key::F2) {
                actions.push(Action::StartRename);
            }
            if key(Modifiers::NONE, Key::F3) {
                actions.push(Action::TogglePreview);
            }
            if key(Modifiers::NONE, Key::F5) {
                actions.push(Action::Refresh);
            }
            if key(Modifiers::NONE, Key::Delete) {
                actions.push(Action::Trash);
            }
            if key(Modifiers::COMMAND | Modifiers::ALT, Key::N) {
                actions.push(Action::NewFile(None));
            }
            if key(cmd_shift, Key::N) {
                actions.push(Action::NewFolder);
            }
            if key(cmd, Key::L) {
                actions.push(Action::EditPath);
            }
            if key(cmd, Key::F) {
                actions.push(Action::FocusFilter);
            }
            if key(cmd, Key::A) {
                actions.push(Action::SelectAll);
            }
            if key(cmd, Key::H) {
                actions.push(Action::ToggleHidden);
            }
            if key(cmd_shift, Key::E) {
                actions.push(Action::ExtractTo);
            }
            if key(cmd, Key::E) {
                actions.push(Action::Extract(ExtractMode::Smart));
            }
            // egui-winit turns Ctrl+C/X/V into dedicated events, also with Shift.
            let shift = i.modifiers.shift;
            for event in &i.events {
                match event {
                    egui::Event::Copy if shift => actions.push(Action::Compress),
                    egui::Event::Copy => actions.push(Action::Copy),
                    egui::Event::Cut => actions.push(Action::Cut),
                    egui::Event::Paste(_) => actions.push(Action::Paste),
                    _ => {}
                }
            }
        });
    }

    fn toolbar(&mut self, ui: &mut Ui, shared: &Shared, actions: &mut Vec<Action>) {
        ui.horizontal(|ui| {
            let nav = |ui: &mut Ui, enabled, glyph, tip| {
                ui.add_enabled(enabled, Button::new(RichText::new(glyph).size(16.0)))
                    .on_hover_text(tip)
                    .clicked()
            };
            if nav(
                ui,
                !self.back.is_empty(),
                icon::ARROW_LEFT,
                "Back (Alt+Left)",
            ) {
                actions.push(Action::Back);
            }
            if nav(
                ui,
                !self.forward.is_empty(),
                icon::ARROW_RIGHT,
                "Forward (Alt+Right)",
            ) {
                actions.push(Action::Forward);
            }
            if nav(
                ui,
                self.path.parent().is_some(),
                icon::ARROW_UP,
                "Up (Backspace)",
            ) {
                actions.push(Action::Up);
            }
            if nav(ui, true, icon::ARROW_CLOCKWISE, "Refresh (F5)") {
                actions.push(Action::Refresh);
            }
            ui.separator();

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let preview =
                    Button::selectable(shared.preview_shown, RichText::new(icon::EYE).size(16.0));
                if ui.add(preview).on_hover_text("Preview pane (F3)").clicked() {
                    actions.push(Action::TogglePreview);
                }
                ui.separator();
                for (mode, glyph, tip) in ViewMode::ALL.iter().rev() {
                    let button =
                        Button::selectable(self.view == *mode, RichText::new(*glyph).size(16.0));
                    if ui.add(button).on_hover_text(*tip).clicked() {
                        actions.push(Action::SetView(*mode));
                    }
                }
                let filter_id = self.filter_id();
                ui.add(
                    TextEdit::singleline(&mut self.filter)
                        .id(filter_id)
                        .hint_text(format!("{} Filter (Ctrl+F)", icon::MAGNIFYING_GLASS))
                        .desired_width(180.0),
                );
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    self.path_bar(ui, actions)
                });
            });
        });
    }

    /// Context menu for empty space in the listing; acts on the folder itself.
    fn background_menu(&self, ui: &mut Ui, shared: &Shared, actions: &mut Vec<Action>) {
        let mut item =
            |ui: &mut Ui, enabled: bool, label: String, shortcut: &str, action: Action| {
                let button = Button::new(label).shortcut_text(shortcut);
                if ui.add_enabled(enabled, button).clicked() {
                    actions.push(action);
                    ui.close();
                }
            };
        let paste_label = match &shared.clipboard {
            Some(clipboard) => format!(
                "{} Paste {} item{}",
                icon::CLIPBOARD,
                clipboard.paths.len(),
                if clipboard.paths.len() == 1 { "" } else { "s" }
            ),
            None => format!("{} Paste", icon::CLIPBOARD),
        };
        item(
            ui,
            shared.clipboard.is_some(),
            paste_label,
            "Ctrl+V",
            Action::Paste,
        );
        ui.separator();
        item(
            ui,
            true,
            format!("{} New folder", icon::FOLDER_PLUS),
            "Ctrl+Shift+N",
            Action::NewFolder,
        );
        item(
            ui,
            true,
            format!("{} New file", icon::FILE_PLUS),
            "Ctrl+Alt+N",
            Action::NewFile(None),
        );
        let templates = places::templates();
        if !templates.is_empty() {
            ui.menu_button(format!("{} New from template", icon::FILE_PLUS), |ui| {
                for template in templates {
                    let label = format!(
                        "{} {}",
                        fs::icon_for(&template, false, false),
                        fs::display_name(&template)
                    );
                    item(ui, true, label, "", Action::NewFile(Some(template)));
                }
            });
        }
        ui.separator();
        item(
            ui,
            true,
            format!("{} Select all", icon::CHECK_SQUARE),
            "Ctrl+A",
            Action::SelectAll,
        );
        item(
            ui,
            true,
            format!("{} Refresh", icon::ARROW_CLOCKWISE),
            "F5",
            Action::Refresh,
        );
        item(
            ui,
            true,
            format!("{} Copy folder path", icon::COPY),
            "",
            Action::CopyFolderPath,
        );
        item(
            ui,
            true,
            format!("{} Open in new tab", icon::PLUS),
            "",
            Action::OpenInNewTab(self.path.clone()),
        );
        item(
            ui,
            self.path.parent().is_some(),
            format!("{} Compress this folder…", icon::FILE_ZIP),
            "",
            Action::CompressFolder,
        );
        ui.separator();
        ui.menu_button(format!("{} View", icon::EYE), |ui| {
            for (mode, glyph, label) in ViewMode::ALL {
                if ui
                    .radio(self.view == mode, format!("{glyph} {label}"))
                    .clicked()
                {
                    actions.push(Action::SetView(mode));
                    ui.close();
                }
            }
        });
        ui.menu_button(format!("{} Sort by", icon::SORT_ASCENDING), |ui| {
            for (key, label) in [
                (SortKey::Name, "Name"),
                (SortKey::Size, "Size"),
                (SortKey::Modified, "Modified"),
                (SortKey::Kind, "Kind"),
            ] {
                let arrow = match (self.sort.key == key, self.sort.ascending) {
                    (false, _) => "",
                    (true, true) => icon::CARET_UP,
                    (true, false) => icon::CARET_DOWN,
                };
                if ui
                    .radio(self.sort.key == key, format!("{label} {arrow}"))
                    .clicked()
                {
                    actions.push(Action::SortBy(key));
                    ui.close();
                }
            }
        });
        let mut show_hidden = shared.show_hidden;
        if ui.checkbox(&mut show_hidden, "Show hidden files").clicked() {
            actions.push(Action::ToggleHidden);
            ui.close();
        }
        let mut show_preview = shared.preview_shown;
        if ui.checkbox(&mut show_preview, "Preview pane").clicked() {
            actions.push(Action::TogglePreview);
            ui.close();
        }
    }

    fn filter_id(&self) -> Id {
        Id::new(("filter", self.id))
    }

    fn path_bar(&mut self, ui: &mut Ui, actions: &mut Vec<Action>) {
        if let Some(text) = &mut self.path_edit {
            let response = ui.add(TextEdit::singleline(text).desired_width(f32::INFINITY));
            if std::mem::take(&mut self.focus_path_edit) {
                response.request_focus();
            }
            if response.lost_focus() {
                if ui.input(|i| i.key_pressed(Key::Enter)) {
                    actions.push(Action::Navigate(PathBuf::from(text.trim())));
                }
                self.path_edit = None;
            }
            return;
        }

        if ui
            .button(icon::PENCIL_SIMPLE)
            .on_hover_text("Edit path (Ctrl+L)")
            .clicked()
        {
            actions.push(Action::EditPath);
        }
        egui::ScrollArea::horizontal()
            .id_salt(("crumbs", self.id))
            .stick_to_right(true)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    let mut crumbs: Vec<_> = self.path.ancestors().collect();
                    crumbs.reverse();
                    for (i, crumb) in crumbs.iter().enumerate() {
                        if i > 0 {
                            ui.weak(icon::CARET_RIGHT);
                        }
                        let last = i + 1 == crumbs.len();
                        let label = RichText::new(fs::display_name(crumb));
                        let label = if last { label.strong() } else { label };
                        if ui.add(Button::new(label).frame(false)).clicked() && !last {
                            actions.push(Action::Navigate(crumb.to_path_buf()));
                        }
                    }
                });
            });
    }

    fn status_bar(&self, ui: &mut Ui, visible: &[usize]) {
        ui.horizontal(|ui| {
            ui.label(format!("{} items", visible.len()));
            if !self.selection.is_empty() {
                let size: u64 = self
                    .entries()
                    .iter()
                    .filter(|e| self.selection.contains(&e.path) && !e.is_dir)
                    .map(|e| e.size)
                    .sum();
                ui.separator();
                ui.label(format!(
                    "{} selected ({})",
                    self.selection.len(),
                    fs::human_size(size)
                ));
            }
            if let Some(error) = &self.error {
                ui.separator();
                ui.colored_label(ui.visuals().error_fg_color, error);
            }
        });
    }

    fn rename_modal(&mut self, ctx: &Context, actions: &mut Vec<Action>) {
        let Some(rename) = &mut self.rename else {
            return;
        };
        let mut cancel = false;
        let modal = Modal::new(Id::new(("rename", self.id))).show(ctx, |ui| {
            ui.heading("Rename");
            let mut output = TextEdit::singleline(&mut rename.name)
                .desired_width(320.0)
                .show(ui);
            if std::mem::take(&mut rename.just_opened) {
                output.response.request_focus();
                // Pre-select the stem so typing replaces the name but keeps the extension.
                let stem_len = match rename.name.rfind('.') {
                    Some(dot) if dot > 0 && !rename.path.is_dir() => {
                        rename.name[..dot].chars().count()
                    }
                    _ => rename.name.chars().count(),
                };
                output
                    .state
                    .cursor
                    .set_char_range(Some(egui::text::CCursorRange::two(
                        egui::text::CCursor::new(0),
                        egui::text::CCursor::new(stem_len),
                    )));
                output.state.store(ctx, output.response.id);
            }
            if output.response.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                actions.push(Action::CommitRename);
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Rename").clicked() {
                    actions.push(Action::CommitRename);
                }
                if ui.button("Cancel").clicked() {
                    cancel = true;
                }
            });
        });
        if cancel || modal.should_close() {
            self.rename = None;
        }
    }

    fn archive_dialogs(&mut self, ctx: &Context, shared: &mut Shared) {
        use archive::DialogResult;
        let result = if let Some(dialog) = &mut self.compress {
            dialog.show(ctx, &mut shared.archive_prefs, shared.show_hidden)
        } else if let Some(dialog) = &mut self.extract {
            dialog.show(ctx, &mut shared.archive_prefs, shared.show_hidden)
        } else {
            return;
        };
        match result {
            DialogResult::Open => {}
            DialogResult::Closed => {
                self.compress = None;
                self.extract = None;
            }
            DialogResult::Start(jobs) => {
                for job in jobs {
                    shared.jobs.start(job);
                }
                self.compress = None;
                self.extract = None;
            }
        }
    }

    /// The selected archives.
    fn selected_archives(&self) -> Vec<PathBuf> {
        self.selected_paths()
            .into_iter()
            .filter(|p| !p.is_dir() && archive::is_archive(p))
            .collect()
    }

    // -----------------------------------------------------------------------
    // Actions

    fn apply(&mut self, action: Action, ctx: &Context, shared: &mut Shared) {
        let visible = self.visible(shared.show_hidden);
        match action {
            Action::Select { index, modifiers } => self.select(index, modifiers, &visible),
            Action::SelectAll => {
                self.selection = visible
                    .iter()
                    .map(|&i| self.entries()[i].path.clone())
                    .collect();
            }
            Action::MoveCursor { delta, extend } => {
                if visible.is_empty() {
                    return;
                }
                let last = visible.len() - 1;
                let index = match self.cursor {
                    // End jumps to the last item even with no cursor yet.
                    None if delta == isize::MAX => last,
                    None => 0,
                    Some(c) => c.saturating_add_signed(delta).min(last),
                };
                let modifiers = if extend {
                    Modifiers::SHIFT
                } else {
                    Modifiers::NONE
                };
                self.select(index, modifiers, &visible);
                self.scroll_to = Some(index);
            }
            Action::Activate(path) => self.activate(path, &mut shared.app_actions),
            Action::ActivateCursor => {
                if let Some(&i) = self.cursor.and_then(|c| visible.get(c)) {
                    self.activate(self.entries()[i].path.clone(), &mut shared.app_actions);
                }
            }
            Action::OpenWith(target) => {
                let paths = self.selected_paths();
                if !paths.is_empty() {
                    let request = open_with::Request::Files(paths);
                    shared
                        .app_actions
                        .push(AppAction::OpenWith(request, target));
                }
            }
            Action::Extract(mode) => {
                for archive in self.selected_archives() {
                    let dest = archive
                        .parent()
                        .map_or_else(|| self.path.clone(), PathBuf::from);
                    let job = ExtractJob::new(archive, dest, mode, false);
                    shared.jobs.start(JobKind::Extract(job));
                }
            }
            Action::ExtractTo => {
                let archives = self.selected_archives();
                if !archives.is_empty() {
                    let id = Id::new(("extract-dialog", self.id));
                    self.extract = Some(ExtractDialog::new(id, archives, None, None));
                }
            }
            Action::Compress => {
                let paths = self.selected_paths();
                if !paths.is_empty() {
                    let id = Id::new(("compress-dialog", self.id));
                    self.compress = Some(CompressDialog::new(id, paths, &shared.archive_prefs));
                }
            }
            Action::CompressFolder => {
                if self.path.parent().is_some() {
                    let id = Id::new(("compress-dialog", self.id));
                    let paths = vec![self.path.clone()];
                    self.compress = Some(CompressDialog::new(id, paths, &shared.archive_prefs));
                }
            }
            Action::OpenInNewTab(path) => shared.app_actions.push(AppAction::NewTab(path)),
            Action::Navigate(path) => self.navigate(path),
            Action::NavigateSelect { dir, select } => {
                self.navigate(dir);
                self.select_path(&select, shared.show_hidden);
            }
            Action::EnterSelected => {
                let Some(dir) = self
                    .preview_entry()
                    .filter(|e| e.is_dir)
                    .map(|e| e.path.clone())
                else {
                    return;
                };
                self.navigate(dir);
                self.reload();
                let first = self
                    .visible(shared.show_hidden)
                    .first()
                    .map(|&i| self.entries()[i].path.clone());
                if let Some(first) = first {
                    self.select_path(&first, shared.show_hidden);
                }
            }
            Action::Back => {
                if let Some(path) = self.back.pop() {
                    let previous = std::mem::replace(&mut self.path, path);
                    self.forward.push(previous);
                    self.reset_location_state();
                }
            }
            Action::Forward => {
                if let Some(path) = self.forward.pop() {
                    let previous = std::mem::replace(&mut self.path, path);
                    self.back.push(previous);
                    self.reset_location_state();
                }
            }
            Action::Up => {
                if let Some(parent) = self.path.parent().map(PathBuf::from) {
                    let child = self.path.clone();
                    self.navigate(parent);
                    self.select_path(&child, shared.show_hidden);
                }
            }
            Action::Refresh => self.refresh(shared.show_hidden),
            Action::SortBy(key) => {
                if self.sort.key == key {
                    self.sort.ascending = !self.sort.ascending;
                } else {
                    self.sort = Sort {
                        key,
                        ascending: true,
                    };
                }
                if let Some(entries) = &mut self.entries {
                    fs::sort_entries(entries, self.sort);
                }
                self.anchor = None;
                self.cursor = None;
            }
            Action::SetView(view) => self.view = view,
            Action::EditPath => {
                self.path_edit = Some(self.path.display().to_string());
                self.focus_path_edit = true;
            }
            Action::FocusFilter => ctx.memory_mut(|m| m.request_focus(self.filter_id())),
            Action::StartRename => {
                let target = self
                    .cursor
                    .and_then(|c| visible.get(c))
                    .map(|&i| self.entries()[i].path.clone())
                    .filter(|p| self.selection.contains(p))
                    .or_else(|| self.selected_paths().into_iter().next());
                if let Some(path) = target {
                    self.rename = Some(Rename {
                        name: fs::display_name(&path),
                        path,
                        just_opened: true,
                    });
                }
            }
            Action::CommitRename => {
                let Some(rename) = self.rename.take() else {
                    return;
                };
                match fs::rename(&rename.path, &rename.name) {
                    Ok(new_path) => {
                        self.reload();
                        self.select_path(&new_path, shared.show_hidden);
                    }
                    Err(err) => self.error = Some(format!("Rename failed: {err}")),
                }
            }
            Action::NewFolder => match fs::create_dir(&self.path) {
                Ok(path) => self.start_renaming_new(path),
                Err(err) => self.error = Some(format!("Could not create folder: {err}")),
            },
            Action::NewFile(template) => match fs::create_file(&self.path, template.as_deref()) {
                Ok(path) => self.start_renaming_new(path),
                Err(err) => self.error = Some(format!("Could not create file: {err}")),
            },
            Action::Trash => {
                let paths = self.selected_paths();
                if paths.is_empty() {
                    return;
                }
                shared.jobs.start(JobKind::Trash { paths });
            }
            Action::Copy | Action::Cut => {
                let paths = self.selected_paths();
                if paths.is_empty() {
                    return;
                }
                // Also put the paths on the system clipboard as text. This keeps
                // Ctrl+V working, since egui only emits paste events for non-empty text.
                ctx.copy_text(join_paths(&paths));
                shared.clipboard = Some(FileClipboard {
                    paths,
                    cut: matches!(action, Action::Cut),
                });
            }
            Action::Paste => paste(shared, self.path.clone()),
            Action::PasteInto(dest) => paste(shared, dest),
            Action::CopyFolderPath => ctx.copy_text(self.path.display().to_string()),
            Action::StartJob(job) => shared.jobs.start(job),
            Action::ClearSelection => {
                self.selection.clear();
                self.anchor = None;
                self.cursor = None;
            }
            Action::CopyPath => {
                let paths = self.selected_paths();
                ctx.copy_text(if paths.is_empty() {
                    self.path.display().to_string()
                } else {
                    join_paths(&paths)
                });
            }
            Action::ToggleHidden => shared.show_hidden = !shared.show_hidden,
            Action::TogglePreview => shared.app_actions.push(AppAction::TogglePreview),
        }
    }

    /// Selects a just-created item and opens the rename dialog for it.
    fn start_renaming_new(&mut self, path: PathBuf) {
        self.filter.clear();
        self.reload();
        self.select_path(&path, true);
        self.rename = Some(Rename {
            name: fs::display_name(&path),
            path,
            just_opened: true,
        });
    }

    fn select(&mut self, index: usize, modifiers: Modifiers, visible: &[usize]) {
        let Some(&entry_index) = visible.get(index) else {
            return;
        };
        let path = self.entries()[entry_index].path.clone();
        if modifiers.shift
            && let Some(anchor) = self.anchor
        {
            let range = anchor.min(index)..=anchor.max(index);
            let paths: Vec<_> = visible[range]
                .iter()
                .map(|&i| self.entries()[i].path.clone())
                .collect();
            if modifiers.command {
                self.selection.extend(paths);
            } else {
                self.selection = paths.into_iter().collect();
            }
        } else if modifiers.command {
            if !self.selection.remove(&path) {
                self.selection.insert(path);
            }
            self.anchor = Some(index);
        } else {
            self.selection = HashSet::from([path]);
            self.anchor = Some(index);
        }
        self.cursor = Some(index);
    }

    /// Opens folders here, archives in an archive tab, and files in their app.
    pub fn activate(&mut self, path: PathBuf, app_actions: &mut Vec<AppAction>) {
        if path.is_dir() {
            self.navigate(path);
        } else if archive::is_archive(&path) {
            app_actions.push(AppAction::OpenArchive(path));
        } else if let Err(err) = fs::open(&path) {
            self.error = Some(format!("Could not open {}: {err}", path.display()));
        }
    }
}

/// Starts copying or moving the clipboard contents into `dest`.
fn paste(shared: &mut Shared, dest: PathBuf) {
    let Some(clipboard) = &shared.clipboard else {
        return;
    };
    let sources = clipboard.paths.clone();
    if clipboard.cut {
        shared.clipboard = None;
        shared.jobs.start(JobKind::Move { sources, dest });
    } else {
        shared.jobs.start(JobKind::Copy { sources, dest });
    }
}

fn join_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::images;

    /// `root/a/b/file.txt`, `root/a/c/`, `root/a/.hidden/`
    fn fixture(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("egui-files-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        std::fs::create_dir_all(root.join("a/c")).unwrap();
        std::fs::create_dir_all(root.join("a/.hidden")).unwrap();
        std::fs::write(root.join("a/b/file.txt"), "hi").unwrap();
        root
    }

    fn shared(ctx: &Context, tab: &BrowserTab) -> Shared {
        Shared {
            show_hidden: false,
            show_thumbnails: true,
            icon_size: 64.0,
            clipboard: None,
            focused_tab: Some(tab.id),
            preview_shown: false,
            app_actions: Vec::new(),
            images: ImageCache::new(ctx),
            videos: Videos::new(ctx),
            texts: TextCache::default(),
            jobs: Jobs::new(ctx),
            archives: archive::Listings::new(ctx),
            archive_prefs: archive::Prefs::default(),
            open_with: Default::default(),
        }
    }

    fn run_frame(ctx: &Context, tab: &mut BrowserTab, shared: &mut Shared, keys: &[Key]) {
        let events = keys
            .iter()
            .map(|&key| egui::Event::Key {
                key,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::NONE,
            })
            .collect();
        run_events(ctx, tab, shared, events);
    }

    fn run_events(
        ctx: &Context,
        tab: &mut BrowserTab,
        shared: &mut Shared,
        events: Vec<egui::Event>,
    ) {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1000.0, 600.0),
            )),
            events,
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ui| {
            // Where the app docks the preview pane by default.
            if shared.preview_shown {
                let entry = tab.preview_entry().cloned();
                Panel::right("preview").default_size(280.0).show(ui, |ui| {
                    crate::preview::pane(ui, entry.as_ref(), shared, &mut Vec::new())
                });
            }
            tab.ui(ui, shared);
        });
        output.textures_delta.clear();
    }

    /// Moves the pointer to `pos`, then presses and releases `button` over three frames.
    fn click(
        ctx: &Context,
        tab: &mut BrowserTab,
        shared: &mut Shared,
        pos: egui::Pos2,
        button: egui::PointerButton,
    ) {
        let press = |pressed| egui::Event::PointerButton {
            pos,
            button,
            pressed,
            modifiers: Modifiers::NONE,
        };
        run_events(ctx, tab, shared, vec![egui::Event::PointerMoved(pos)]);
        run_events(ctx, tab, shared, vec![press(true)]);
        run_events(ctx, tab, shared, vec![press(false)]);
        run_events(ctx, tab, shared, vec![]);
    }

    #[test]
    fn empty_space_has_context_menu_and_clears_selection() {
        let root = fixture("background");
        let a = root.join("a");
        for view in [ViewMode::Details, ViewMode::Grid, ViewMode::Columns] {
            let ctx = Context::default();
            let mut tab = BrowserTab::new(1, a.clone());
            tab.view = view;
            let mut shared = shared(&ctx, &tab);
            run_frame(&ctx, &mut tab, &mut shared, &[]);

            // Well below the two entries, inside the listing area.
            let empty = egui::pos2(900.0, 450.0);
            click(
                &ctx,
                &mut tab,
                &mut shared,
                empty,
                egui::PointerButton::Secondary,
            );
            assert!(
                ctx.any_popup_open(),
                "{view:?}: context menu on empty space"
            );
            run_events(
                &ctx,
                &mut tab,
                &mut shared,
                vec![egui::Event::Key {
                    key: Key::Escape,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: Modifiers::NONE,
                }],
            );
            // Popup bookkeeping lags a couple of frames behind.
            for _ in 0..3 {
                run_frame(&ctx, &mut tab, &mut shared, &[]);
            }
            assert!(!ctx.any_popup_open(), "{view:?}: Escape closes the menu");

            tab.selection = HashSet::from([a.join("b")]);
            click(
                &ctx,
                &mut tab,
                &mut shared,
                empty,
                egui::PointerButton::Primary,
            );
            assert!(
                tab.selection.is_empty(),
                "{view:?}: clicking empty space clears selection"
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn columns_view_navigates_with_arrow_keys() {
        let root = fixture("columns");
        let a = root.join("a");
        let b = a.join("b");
        let ctx = Context::default();
        let mut tab = BrowserTab::new(1, b.clone());
        tab.view = ViewMode::Columns;
        let mut shared = shared(&ctx, &tab);

        for _ in 0..3 {
            run_frame(&ctx, &mut tab, &mut shared, &[]);
        }
        assert!(!tab.column_listing(&a).is_empty(), "ancestor column loaded");

        // Left goes up and selects the folder we came from; the preview column then lists it.
        run_frame(&ctx, &mut tab, &mut shared, &[Key::ArrowLeft]);
        assert_eq!(tab.path, a);
        assert_eq!(tab.preview_entry().map(|e| e.path.clone()), Some(b.clone()));
        run_frame(&ctx, &mut tab, &mut shared, &[]);
        assert_eq!(tab.column_listing(&b).len(), 1, "preview column loaded");

        // Hidden folders are filtered from the current column.
        assert_eq!(tab.visible(false).len(), 2);

        // Right enters the selected folder and selects its first item.
        run_frame(&ctx, &mut tab, &mut shared, &[Key::ArrowRight]);
        assert_eq!(tab.path, b);
        assert_eq!(
            tab.preview_entry().map(|e| e.name.as_str()),
            Some("file.txt")
        );
        run_frame(&ctx, &mut tab, &mut shared, &[]);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn disk_changes_keep_cursor_and_survive_deletion() {
        let root = fixture("refresh");
        let a = root.join("a");
        let ctx = Context::default();
        let mut tab = BrowserTab::new(1, a.clone());
        let mut shared = shared(&ctx, &tab);
        run_frame(&ctx, &mut tab, &mut shared, &[]);
        // Visible: b, c. Put the cursor on c.
        run_frame(&ctx, &mut tab, &mut shared, &[Key::End]);
        assert_eq!(tab.cursor, Some(1));

        // A new folder sorting before c shifts it down; the cursor follows c.
        std::fs::create_dir(a.join("bb")).unwrap();
        tab.dirs_changed(&HashSet::from([a.clone()]));
        run_frame(&ctx, &mut tab, &mut shared, &[]);
        assert_eq!(tab.visible(false).len(), 3);
        assert_eq!(tab.cursor, Some(2));
        assert!(tab.selection.contains(&a.join("c")));

        // Deleting the open folder falls back to its parent.
        std::fs::remove_dir_all(&a).unwrap();
        tab.dirs_changed(&HashSet::from([a.clone()]));
        run_frame(&ctx, &mut tab, &mut shared, &[]);
        assert_eq!(tab.path, root);
        assert!(tab.error.as_deref().unwrap().contains("no longer exists"));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn new_file_from_template_and_paste_into_folder() {
        let root = fixture("newfile");
        let a = root.join("a");
        let ctx = Context::default();
        let mut tab = BrowserTab::new(1, a.clone());
        let mut shared = shared(&ctx, &tab);
        run_frame(&ctx, &mut tab, &mut shared, &[]);

        // Empty file: created, selected, and the rename dialog opens.
        tab.apply(Action::NewFile(None), &ctx, &mut shared);
        let created = a.join("New File");
        assert!(created.is_file());
        assert!(tab.selection.contains(&created));
        assert_eq!(
            tab.rename.as_ref().map(|r| r.path.clone()),
            Some(created.clone())
        );
        tab.rename = None;

        // Template: copied with its name, and a second one gets a unique name.
        let template = root.join("Report.md");
        std::fs::write(&template, "# Title").unwrap();
        tab.apply(Action::NewFile(Some(template.clone())), &ctx, &mut shared);
        tab.apply(Action::NewFile(Some(template.clone())), &ctx, &mut shared);
        assert_eq!(
            std::fs::read_to_string(a.join("Report.md")).unwrap(),
            "# Title"
        );
        assert!(a.join("Report (1).md").is_file());

        // Paste into a subfolder rather than the current folder.
        shared.clipboard = Some(FileClipboard {
            paths: vec![created.clone()],
            cut: false,
        });
        tab.apply(Action::PasteInto(a.join("c")), &ctx, &mut shared);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while shared.jobs.running() > 0 {
            assert!(std::time::Instant::now() < deadline);
            shared.jobs.poll();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(a.join("c/New File").is_file());
        assert!(created.is_file(), "copy keeps the original");

        std::fs::remove_dir_all(root).unwrap();
    }

    /// Drags with the primary button from `from` to `to`, holding `modifiers` on release.
    fn drag(
        ctx: &Context,
        tab: &mut BrowserTab,
        shared: &mut Shared,
        from: egui::Pos2,
        to: egui::Pos2,
        modifiers: Modifiers,
    ) {
        let frame =
            |tab: &mut BrowserTab, shared: &mut Shared, mut events: Vec<egui::Event>, modifiers| {
                events.insert(0, egui::Event::ModifiersChanged(modifiers));
                let input = egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1000.0, 600.0),
                    )),
                    events,
                    ..Default::default()
                };
                let mut output = ctx.run_ui(input, |ui| tab.ui(ui, shared));
                output.textures_delta.clear();
            };
        let button = |pos, pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers,
        };
        frame(
            tab,
            shared,
            vec![egui::Event::PointerMoved(from)],
            Modifiers::NONE,
        );
        frame(tab, shared, vec![button(from, true)], Modifiers::NONE);
        for step in 1..=10 {
            let pos = from + (to - from) * (step as f32 / 10.0);
            frame(tab, shared, vec![egui::Event::PointerMoved(pos)], modifiers);
        }
        frame(tab, shared, vec![button(to, false)], modifiers);
        frame(tab, shared, vec![], Modifiers::NONE);
    }

    fn wait_for_jobs(shared: &mut Shared) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while shared.jobs.running() > 0 {
            assert!(std::time::Instant::now() < deadline, "job timed out");
            shared.jobs.poll();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    /// Runs a frame with `modifiers` held.
    fn run_held(
        ctx: &Context,
        tab: &mut BrowserTab,
        shared: &mut Shared,
        events: Vec<egui::Event>,
        modifiers: Modifiers,
    ) {
        let mut held = vec![egui::Event::ModifiersChanged(modifiers)];
        held.extend(events);
        run_events(ctx, tab, shared, held);
        run_events(
            ctx,
            tab,
            shared,
            vec![egui::Event::ModifiersChanged(Modifiers::NONE)],
        );
    }

    /// What the app does when jobs finish.
    fn finish_jobs(ctx: &Context, tab: &mut BrowserTab, shared: &mut Shared) {
        wait_for_jobs(shared);
        for outcome in shared.jobs.take_outcomes() {
            if let crate::jobs::Outcome::Reveal(path) = outcome {
                tab.reveal(path);
            }
        }
        tab.mark_stale();
        run_frame(ctx, tab, shared, &[]);
    }

    #[test]
    fn compress_dialog_and_extract_shortcut() {
        let root = fixture("archive-ui");
        let a = root.join("a");
        let ctx = Context::default();
        let mut tab = BrowserTab::new(1, a.clone());
        let mut shared = shared(&ctx, &tab);
        run_frame(&ctx, &mut tab, &mut shared, &[]);

        // Ctrl+Shift+C reaches us as a copy event with Shift held.
        tab.selection = HashSet::from([a.join("b")]);
        run_held(
            &ctx,
            &mut tab,
            &mut shared,
            vec![egui::Event::Copy],
            Modifiers::COMMAND | Modifiers::SHIFT,
        );
        assert!(tab.compress.is_some(), "compress dialog opens");
        assert!(shared.clipboard.is_none(), "and nothing was copied");
        // The name field has focus; Enter compresses to `b.zip`.
        run_frame(&ctx, &mut tab, &mut shared, &[]);
        run_frame(&ctx, &mut tab, &mut shared, &[Key::Enter]);
        assert!(tab.compress.is_none(), "dialog closes");
        finish_jobs(&ctx, &mut tab, &mut shared);
        let archive = a.join("b.zip");
        assert!(archive.is_file());
        assert_eq!(
            tab.selection,
            HashSet::from([archive.clone()]),
            "new archive is selected"
        );

        // Ctrl+E: one top-level folder, so it comes out as `b/` again.
        std::fs::remove_dir_all(a.join("b")).unwrap();
        let ctrl_e = egui::Event::Key {
            key: Key::E,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::COMMAND,
        };
        run_held(
            &ctx,
            &mut tab,
            &mut shared,
            vec![ctrl_e],
            Modifiers::COMMAND,
        );
        finish_jobs(&ctx, &mut tab, &mut shared);
        assert_eq!(std::fs::read_to_string(a.join("b/file.txt")).unwrap(), "hi");
        assert_eq!(
            tab.selection,
            HashSet::from([a.join("b")]),
            "extracted folder is selected"
        );

        // Opening an archive asks the app for an archive tab.
        tab.activate(archive.clone(), &mut shared.app_actions);
        assert!(
            matches!(shared.app_actions.as_slice(), [AppAction::OpenArchive(p)] if *p == archive)
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn drag_and_drop_moves_or_copies_into_folders() {
        let root = fixture("dnd");
        let a = root.join("a");
        std::fs::write(a.join("x.txt"), "x").unwrap();
        let ctx = Context::default();
        let mut tab = BrowserTab::new(1, a.clone());
        tab.view = ViewMode::Grid;
        let mut shared = shared(&ctx, &tab);
        run_frame(&ctx, &mut tab, &mut shared, &[]);

        // Grid cells, left to right: b/, c/, x.txt (folders first).
        let cell = |i: f32| egui::pos2(48.0 + i * 102.0, 90.0);

        // Plain drop on a folder on the same drive moves.
        drag(
            &ctx,
            &mut tab,
            &mut shared,
            cell(2.0),
            cell(0.0),
            Modifiers::NONE,
        );
        wait_for_jobs(&mut shared);
        assert!(a.join("b/x.txt").is_file(), "moved into b");
        assert!(!a.join("x.txt").exists());

        // Ctrl+drop copies: drag the folder b (now holding x.txt) onto c.
        tab.mark_stale();
        run_frame(&ctx, &mut tab, &mut shared, &[]);
        drag(
            &ctx,
            &mut tab,
            &mut shared,
            cell(0.0),
            cell(1.0),
            Modifiers::COMMAND,
        );
        wait_for_jobs(&mut shared);
        assert!(a.join("c/b/x.txt").is_file(), "copied into c");
        assert!(a.join("b/x.txt").is_file(), "original kept");

        // Dropping onto empty space in its own folder is a no-op.
        drag(
            &ctx,
            &mut tab,
            &mut shared,
            cell(0.0),
            egui::pos2(900.0, 450.0),
            Modifiers::NONE,
        );
        assert_eq!(shared.jobs.running(), 0);
        assert!(
            egui::DragAndDrop::payload::<crate::dnd::FileDrag>(&ctx).is_none(),
            "drag ended"
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    fn external(
        ctx: &Context,
        tab: &mut BrowserTab,
        shared: &mut Shared,
        event: crate::dnd::ExternalEvent,
    ) {
        let mut input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1000.0, 600.0),
            )),
            ..Default::default()
        };
        crate::dnd::inject_external(ctx, &mut input, event);
        let mut output = ctx.run_ui(input, |ui| tab.ui(ui, shared));
        output.textures_delta.clear();
    }

    #[test]
    fn drops_from_other_apps_copy_into_the_folder_under_the_pointer() {
        use crate::dnd::{ExternalEvent, FileDrag};

        let root = fixture("external");
        let a = root.join("a");
        let elsewhere = root.join("elsewhere.txt");
        std::fs::write(&elsewhere, "from another app").unwrap();
        let ctx = Context::default();
        let mut tab = BrowserTab::new(1, a.clone());
        tab.view = ViewMode::Grid;
        let mut shared = shared(&ctx, &tab);
        run_frame(&ctx, &mut tab, &mut shared, &[]);
        let folder_b = egui::pos2(48.0, 90.0);
        let empty = egui::pos2(900.0, 450.0);

        // Hovering marks an external drag (paths unknown yet); leaving cancels it.
        external(&ctx, &mut tab, &mut shared, ExternalEvent::Hover(folder_b));
        let payload = egui::DragAndDrop::payload::<FileDrag>(&ctx).expect("hovering");
        assert!(payload.external && payload.paths.is_empty());
        external(&ctx, &mut tab, &mut shared, ExternalEvent::Leave);
        assert!(egui::DragAndDrop::payload::<FileDrag>(&ctx).is_none());

        // Dropping on a folder copies into it; the original stays put.
        external(&ctx, &mut tab, &mut shared, ExternalEvent::Hover(folder_b));
        external(
            &ctx,
            &mut tab,
            &mut shared,
            ExternalEvent::Drop {
                pos: folder_b,
                paths: vec![elsewhere.clone()],
                internal: false,
            },
        );
        wait_for_jobs(&mut shared);
        assert!(a.join("b/elsewhere.txt").is_file());
        assert!(elsewhere.is_file(), "external drops copy");

        // Dropping on empty space copies into the tab's folder, and the
        // injected release isn't mistaken for a click that clears the selection.
        tab.selection = HashSet::from([a.join("b")]);
        external(
            &ctx,
            &mut tab,
            &mut shared,
            ExternalEvent::Drop {
                pos: empty,
                paths: vec![elsewhere.clone()],
                internal: false,
            },
        );
        wait_for_jobs(&mut shared);
        assert!(a.join("elsewhere.txt").is_file());
        assert!(tab.selection.contains(&a.join("b")), "selection kept");

        // A drag that left this app and came back is an in-app drag: same drive, so it moves.
        tab.mark_stale();
        run_frame(&ctx, &mut tab, &mut shared, &[]);
        let moved = a.join("elsewhere.txt");
        external(
            &ctx,
            &mut tab,
            &mut shared,
            ExternalEvent::Drop {
                pos: egui::pos2(150.0, 90.0),
                paths: vec![moved.clone()],
                internal: true,
            },
        );
        wait_for_jobs(&mut shared);
        assert!(a.join("c/elsewhere.txt").is_file());
        assert!(!moved.exists(), "returning in-app drags move");

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn preview_pane_and_grid_render_images_and_text() {
        let root = fixture("preview");
        let b = root.join("a/b");
        image::RgbaImage::from_pixel(40, 20, image::Rgba([0, 0, 255, 255]))
            .save(b.join("pic.png"))
            .unwrap();
        let ctx = Context::default();
        let mut tab = BrowserTab::new(1, b.clone());
        tab.view = ViewMode::Grid;
        let mut shared = shared(&ctx, &tab);
        shared.preview_shown = true;

        for name in ["pic.png", "file.txt"] {
            run_frame(&ctx, &mut tab, &mut shared, &[]);
            tab.selection = HashSet::from([b.join(name)]);
            for _ in 0..50 {
                shared.images.begin_frame();
                run_frame(&ctx, &mut tab, &mut shared, &[]);
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        // Both the grid thumbnail and the larger preview were decoded.
        let pic = b.join("pic.png");
        let modified = tab
            .entries()
            .iter()
            .find(|e| e.path == pic)
            .unwrap()
            .modified;
        assert!(
            shared
                .images
                .is_ready(&pic, modified, images::THUMBNAIL_SIZE)
        );
        assert!(shared.images.is_ready(&pic, modified, images::PREVIEW_SIZE));

        std::fs::remove_dir_all(root).unwrap();
    }

    /// Needs libmpv, ffmpeg and a GPU.
    #[test]
    #[ignore]
    fn videos_play_in_preview_pane_and_on_hover() {
        let root = fixture("video-preview");
        let b = root.join("a/b");
        let clip = b.join("clip.mp4");
        let status = std::process::Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=s=320x180:d=2",
            ])
            .args(["-pix_fmt", "yuv420p"])
            .arg(&clip)
            .status()
            .unwrap();
        assert!(status.success());
        let ctx = Context::default();
        let mut tab = BrowserTab::new(1, b.clone());
        tab.view = ViewMode::Grid;
        let mut shared = shared(&ctx, &tab);
        shared.preview_shown = true;
        run_frame(&ctx, &mut tab, &mut shared, &[]);
        tab.selection = HashSet::from([clip.clone()]);

        let preview_ready = |shared: &Shared| {
            shared.videos.players().any(|(id, player)| {
                let status = player.status();
                id != Id::new("hover-video")
                    && player.frame().is_some()
                    && status.paused
                    && status.video_size == Some([320, 180])
                    && (status.duration - 2.0).abs() < 0.1
                    // Rendered at the preview pane's width, in 16:9.
                    && player.size()[0] > 200
                    && player.size()[0].abs_diff(player.size()[1] * 16 / 9) <= 2
            })
        };
        let hover_ready = |shared: &Shared| {
            shared.videos.players().any(|(id, player)| {
                id == Id::new("hover-video") && player.frame().is_some() && !player.status().paused
            })
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        // The first grid cell is at the top left, below the toolbar.
        let cell = egui::pos2(50.0, 90.0);
        while !(preview_ready(&shared) && hover_ready(&shared)) {
            assert!(std::time::Instant::now() < deadline, "videos did not play");
            shared.images.begin_frame();
            shared.videos.begin_frame();
            run_events(
                &ctx,
                &mut tab,
                &mut shared,
                vec![egui::Event::PointerMoved(cell)],
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        // The grid also got a regular thumbnail for the video.
        let modified = tab.entries()[0].modified;
        assert!(
            shared
                .images
                .is_ready(&clip, modified, images::THUMBNAIL_SIZE)
        );

        // Moving away stops the hover player.
        for _ in 0..3 {
            shared.videos.begin_frame();
            run_events(&ctx, &mut tab, &mut shared, vec![egui::Event::PointerGone]);
        }
        assert!(
            !shared
                .videos
                .players()
                .any(|(id, _)| id == Id::new("hover-video"))
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    /// Where `needle` was drawn, from the text shapes of a frame.
    fn text_pos(output: &egui::FullOutput, needle: &str) -> Option<egui::Pos2> {
        output
            .shapes
            .iter()
            .find_map(|clipped| match &clipped.shape {
                egui::Shape::Text(text) if text.galley.text().contains(needle) => {
                    Some(text.pos + text.galley.rect.center().to_vec2())
                }
                _ => None,
            })
    }

    #[test]
    fn open_with_menu_and_dialog() {
        let root = fixture("open-with");
        let b = root.join("a/b");
        let file = b.join("file.txt");
        let ctx = Context::default();
        let mut tab = BrowserTab::new(1, b.clone());
        let mut shared = shared(&ctx, &tab);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while crate::open_with::catalog(&ctx).is_none() {
            assert!(std::time::Instant::now() < deadline, "app catalog loads");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let frame = |tab: &mut BrowserTab, shared: &mut Shared, events: Vec<egui::Event>| {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1000.0, 600.0),
                )),
                events,
                ..Default::default()
            };
            let mut output = ctx.run_ui(input, |ui| {
                tab.ui(ui, shared);
                if let Some(chosen) = shared.open_with.ui(ui.ctx()) {
                    shared
                        .app_actions
                        .push(AppAction::OpenWith(chosen.0, Some(chosen.1)));
                }
            });
            output.textures_delta.clear();
            output
        };
        let press = |pos, button, pressed| egui::Event::PointerButton {
            pos,
            button,
            pressed,
            modifiers: Modifiers::NONE,
        };
        let click = |tab: &mut BrowserTab, shared: &mut Shared, pos, button| {
            frame(tab, shared, vec![egui::Event::PointerMoved(pos)]);
            frame(tab, shared, vec![press(pos, button, true)]);
            frame(tab, shared, vec![press(pos, button, false)]);
            frame(tab, shared, vec![])
        };

        let output = frame(&mut tab, &mut shared, vec![]);
        let row = text_pos(&output, "file.txt").expect("file row drawn");
        let output = click(&mut tab, &mut shared, row, egui::PointerButton::Secondary);
        let item = text_pos(&output, "Open with").expect("Open with in the menu");
        frame(&mut tab, &mut shared, vec![egui::Event::PointerMoved(item)]);
        let mut output = frame(&mut tab, &mut shared, vec![]);
        for _ in 0..5 {
            if text_pos(&output, "Other application").is_some() {
                break;
            }
            output = frame(&mut tab, &mut shared, vec![]);
        }
        let other = text_pos(&output, "Other application").expect("submenu opens");
        // Along the submenu first, so the pointer doesn't cross other items.
        frame(
            &mut tab,
            &mut shared,
            vec![egui::Event::PointerMoved(egui::pos2(other.x, item.y))],
        );
        click(&mut tab, &mut shared, other, egui::PointerButton::Primary);
        let request = shared.app_actions.iter().find_map(|a| match a {
            AppAction::OpenWith(open_with::Request::Files(paths), None) => Some(paths.clone()),
            _ => None,
        });
        assert_eq!(
            request,
            Some(vec![file.clone()]),
            "Other… asks for the file"
        );

        shared.app_actions.clear();
        shared
            .open_with
            .ask(open_with::Request::Files(vec![file.clone()]));
        let mut output = frame(&mut tab, &mut shared, vec![]);
        for _ in 0..5 {
            output = frame(&mut tab, &mut shared, vec![]);
        }
        assert!(text_pos(&output, "Open “file.txt” with").is_some());
        assert!(
            text_pos(&output, "text/plain").is_some(),
            "shows the file's type"
        );
        // Keys go to the dialog's search box, not the tab.
        frame(
            &mut tab,
            &mut shared,
            vec![egui::Event::Text("no-such-app-zzz".into())],
        );
        let output = frame(&mut tab, &mut shared, vec![]);
        assert!(text_pos(&output, "No matching applications").is_some());
        let key = |key| egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        };
        frame(&mut tab, &mut shared, vec![key(Key::Enter)]);
        assert!(shared.app_actions.is_empty(), "nothing to open");
        frame(&mut tab, &mut shared, vec![key(Key::Escape)]);
        frame(&mut tab, &mut shared, vec![]);
        assert!(!shared.open_with.is_asking(), "Escape closes the dialog");
        std::fs::remove_dir_all(root).unwrap();
    }
}
