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
    fs::{self, Entry, Sort, SortKey},
    images::{self, ImageCache},
    preview::{self, TextCache},
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
    pub app_actions: Vec<AppAction>,
    pub images: ImageCache,
    pub texts: TextCache,
}

pub struct FileClipboard {
    pub paths: Vec<PathBuf>,
    pub cut: bool,
}

/// Requests a tab cannot fulfil on its own.
pub enum AppAction {
    NewTab(PathBuf),
    RefreshAll,
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
    Trash,
    Copy,
    Cut,
    Paste,
    CopyPath,
    ToggleHidden,
    TogglePreview,
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
    #[serde(default)]
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
            error: None,
            selection: HashSet::new(),
            anchor: None,
            cursor: None,
            scroll_to: None,
            path_edit: None,
            focus_path_edit: false,
            rename: None,
            column_cache: HashMap::new(),
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

    pub fn invalidate(&mut self) {
        self.entries = None;
    }

    fn reload(&mut self) {
        self.column_cache.clear();
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

    fn selected_paths(&self) -> Vec<PathBuf> {
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
        }

        let mut actions = Vec::new();
        if shared.focused_tab == Some(self.id) {
            self.keyboard(ui.ctx(), &mut actions);
        }

        Panel::top(Id::new(("toolbar", self.id))).show(ui, |ui| self.toolbar(ui, &mut actions));
        if self.view == ViewMode::Columns {
            self.prepare_columns();
        }
        let visible = self.visible(shared.show_hidden);
        Panel::bottom(Id::new(("status", self.id))).show(ui, |ui| self.status_bar(ui, &visible));
        if self.show_preview {
            Panel::right(Id::new(("preview", self.id)))
                .resizable(true)
                .default_size(280.0)
                .show(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .id_salt(("preview", self.id))
                        .show(ui, |ui| match self.preview_entry() {
                            Some(entry) => preview::show(
                                ui,
                                entry,
                                &mut shared.images,
                                &mut shared.texts,
                                images::PREVIEW_SIZE,
                                &mut actions,
                            ),
                            None => {
                                ui.add_space(16.0);
                                ui.vertical_centered(|ui| ui.weak("Select one item to preview"));
                            }
                        });
                });
        }
        CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ui, |ui| {
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
            });
        self.rename_modal(ui.ctx(), &mut actions);
        self.scroll_to = None;

        if !actions.is_empty() {
            ui.ctx().request_repaint();
        }
        for action in actions {
            self.apply(action, ui.ctx(), shared);
        }
    }

    fn keyboard(&mut self, ctx: &Context, actions: &mut Vec<Action>) {
        if ctx.text_edit_focused() || self.rename.is_some() {
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
            // egui-winit turns Ctrl+C/X/V into dedicated events.
            for event in &i.events {
                match event {
                    egui::Event::Copy => actions.push(Action::Copy),
                    egui::Event::Cut => actions.push(Action::Cut),
                    egui::Event::Paste(_) => actions.push(Action::Paste),
                    _ => {}
                }
            }
        });
    }

    fn toolbar(&mut self, ui: &mut Ui, actions: &mut Vec<Action>) {
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
                    Button::selectable(self.show_preview, RichText::new(icon::EYE).size(16.0));
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
            Action::Activate(path) => self.activate(path),
            Action::ActivateCursor => {
                if let Some(&i) = self.cursor.and_then(|c| visible.get(c)) {
                    self.activate(self.entries()[i].path.clone());
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
            Action::Refresh => self.reload(),
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
                Ok(path) => {
                    self.filter.clear();
                    self.reload();
                    self.select_path(&path, true);
                    self.rename = Some(Rename {
                        name: fs::display_name(&path),
                        path,
                        just_opened: true,
                    });
                }
                Err(err) => self.error = Some(format!("Could not create folder: {err}")),
            },
            Action::Trash => {
                let paths = self.selected_paths();
                if paths.is_empty() {
                    return;
                }
                if let Err(err) = fs::trash(&paths) {
                    self.error = Some(format!("Move to trash failed: {err}"));
                }
                shared.app_actions.push(AppAction::RefreshAll);
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
            Action::Paste => {
                let Some(clipboard) = &shared.clipboard else {
                    return;
                };
                if let Err(err) = fs::paste(&clipboard.paths, &self.path, clipboard.cut) {
                    self.error = Some(format!("Paste failed: {err}"));
                }
                if clipboard.cut {
                    shared.clipboard = None;
                }
                shared.app_actions.push(AppAction::RefreshAll);
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
            Action::TogglePreview => self.show_preview = !self.show_preview,
        }
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

    fn activate(&mut self, path: PathBuf) {
        if path.is_dir() {
            self.navigate(path);
        } else if let Err(err) = fs::open(&path) {
            self.error = Some(format!("Could not open {}: {err}", path.display()));
        }
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
            app_actions: Vec::new(),
            images: ImageCache::new(ctx),
            texts: TextCache::default(),
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
    fn preview_pane_and_grid_render_images_and_text() {
        let root = fixture("preview");
        let b = root.join("a/b");
        image::RgbaImage::from_pixel(40, 20, image::Rgba([0, 0, 255, 255]))
            .save(b.join("pic.png"))
            .unwrap();
        let ctx = Context::default();
        let mut tab = BrowserTab::new(1, b.clone());
        tab.view = ViewMode::Grid;
        tab.show_preview = true;
        let mut shared = shared(&ctx, &tab);

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
}
