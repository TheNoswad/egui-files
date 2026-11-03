//! The trash, shown as a tab: lists trashed items and restores or
//! permanently deletes them (as background jobs).

// Android has no trash of its own; see android/trash.
#[cfg(any(target_os = "android", target_arch = "wasm32"))]
use trash_android as trash;

use std::{
    collections::HashSet,
    ffi::OsString,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime},
};

use egui::{
    Align, Button, CentralPanel, Context, Id, Layout, Modal, Modifiers, Panel, RichText, Sense,
    TextEdit, Ui,
};
use egui_extras::TableBuilder;
use egui_phosphor::regular as icon;
use serde::{Deserialize, Serialize};
use trash::{TrashItem, TrashItemSize, os_limited};

use crate::keymap::Command as C;
use crate::{
    dnd::{self, DropTarget},
    fs,
    jobs::JobKind,
    list_columns::{self, Spec},
    open_with,
    tab::{AppAction, Shared},
};

const LOCATION: usize = 1;
const DELETED: usize = 2;
const SIZE: usize = 3;

pub static COLUMNS: list_columns::Table = list_columns::Table {
    key: "trash",
    columns: &[
        Spec {
            label: "Name",
            width: 0.0,
            min: 160.0,
        },
        Spec {
            label: "Original location",
            width: 280.0,
            min: 80.0,
        },
        Spec {
            label: "Deleted",
            width: 140.0,
            min: 80.0,
        },
        Spec {
            label: "Size",
            width: 90.0,
            min: 60.0,
        },
    ],
    hide_order: &[SIZE, DELETED, LOCATION],
    hidden: &[],
};

struct Item {
    item: TrashItem,
    name: String,
    original: PathBuf,
    deleted: Option<SystemTime>,
    size: Option<TrashItemSize>,
    is_dir: bool,
}

impl Item {
    fn size_text(&self) -> String {
        match &self.size {
            Some(TrashItemSize::Bytes(bytes)) => fs::human_size(*bytes),
            Some(TrashItemSize::Entries(1)) => "1 item".to_owned(),
            Some(TrashItemSize::Entries(n)) => format!("{n} items"),
            None => String::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SortKey {
    Name,
    Location,
    Deleted,
    Size,
}

enum Action {
    /// Index into the visible list.
    Select {
        index: usize,
        modifiers: Modifiers,
    },
    SelectAll,
    ClearSelection,
    Restore,
    /// Ask before permanently deleting the selection.
    AskPurge,
    /// Ask before permanently deleting everything.
    AskEmpty,
    Refresh,
    SortBy(SortKey),
    CopyOriginalPath,
    Properties,
    /// Moves whatever was copied or cut to the trash.
    TrashClipboard,
    /// Open the selection with an app, or ask which when `None`.
    OpenWith(Option<open_with::Target>),
    StartJob(JobKind),
}

#[derive(Serialize, Deserialize)]
pub struct TrashTab {
    pub id: u64,
    /// Marks saved state as a trash tab rather than a browser tab.
    is_trash: bool,

    #[serde(skip)]
    items: Option<Vec<Item>>,
    #[serde(skip)]
    stale: bool,
    #[serde(skip)]
    error: Option<String>,
    #[serde(skip)]
    selection: HashSet<OsString>,
    #[serde(skip)]
    anchor: Option<usize>,
    #[serde(skip, default = "default_sort")]
    sort: (SortKey, bool),
    #[serde(skip)]
    filter: String,
    /// Items awaiting confirmation of permanent deletion.
    #[serde(skip)]
    confirm_purge: Option<Vec<TrashItem>>,
}

fn default_sort() -> (SortKey, bool) {
    (SortKey::Deleted, false)
}

/// Counts trashed items on a background thread, since listing can be slow.
pub struct TrashCount {
    ctx: Context,
    count: Arc<Mutex<Option<usize>>>,
    running: Arc<AtomicBool>,
    /// Another count was requested while one was running.
    again: Arc<AtomicBool>,
}

impl TrashCount {
    pub fn new(ctx: &Context) -> Self {
        let counter = Self {
            ctx: ctx.clone(),
            count: Arc::default(),
            running: Arc::default(),
            again: Arc::default(),
        };
        counter.request();
        counter
    }

    pub fn get(&self) -> Option<usize> {
        *self.count.lock().expect("trash count poisoned")
    }

    /// Recounts in the background.
    pub fn request(&self) {
        if self.running.swap(true, Ordering::AcqRel) {
            self.again.store(true, Ordering::Release);
            return;
        }
        let (ctx, count, running, again) = (
            self.ctx.clone(),
            self.count.clone(),
            self.running.clone(),
            self.again.clone(),
        );
        crate::background::spawn("trash-count", move || {
            loop {
                again.store(false, Ordering::Release);
                let n = os_limited::list().map(|items| items.len()).ok();
                *count.lock().expect("trash count poisoned") = n;
                ctx.request_repaint();
                if !again.load(Ordering::Acquire) {
                    break;
                }
            }
            running.store(false, Ordering::Release);
        });
    }
}

/// Where a trashed item's contents are kept.
fn stored_path(item: &TrashItem) -> Option<PathBuf> {
    let id = Path::new(&item.id);
    let path = if cfg!(windows) {
        // The item's parsing name, a path inside the recycle bin.
        id.to_path_buf()
    } else {
        // `<trash>/info/<name>.trashinfo` describes `<trash>/files/<name>`.
        let name = id.file_stem()?;
        id.parent()?.parent()?.join("files").join(name)
    };
    disk::symlink_metadata(&path).is_ok().then_some(path)
}

/// The folder whose changes mean the trash changed, for the file watcher.
pub fn watched_dir() -> Option<PathBuf> {
    #[cfg(target_os = "android")]
    return Some(trash::watched_dir());
    #[allow(unreachable_code)]
    if cfg!(windows) {
        return None;
    }
    directories::BaseDirs::new().map(|d| d.data_dir().join("Trash/info"))
}

impl TrashTab {
    /// Clears the selection, for Android's back; whether there was one.
    pub fn clear_selection(&mut self) -> bool {
        self.anchor = None;
        let had = !self.selection.is_empty();
        self.selection.clear();
        had
    }

    pub fn new(id: u64) -> Self {
        Self {
            id,
            is_trash: true,
            items: None,
            stale: false,
            error: None,
            selection: HashSet::new(),
            anchor: None,
            sort: default_sort(),
            filter: String::new(),
            confirm_purge: None,
        }
    }

    pub fn title(&self) -> String {
        format!("{} Trash", icon::TRASH)
    }

    pub fn mark_stale(&mut self) {
        self.stale = true;
    }

    fn items(&self) -> &[Item] {
        self.items.as_deref().unwrap_or_default()
    }

    fn load(&mut self) {
        self.stale = false;
        match os_limited::list() {
            Ok(list) => {
                let mut items: Vec<Item> = list
                    .into_iter()
                    .map(|item| {
                        let size = os_limited::metadata(&item).ok().map(|m| m.size);
                        Item {
                            name: item.name.to_string_lossy().into_owned(),
                            original: item.original_path(),
                            deleted: u64::try_from(item.time_deleted)
                                .ok()
                                .map(|secs| SystemTime::UNIX_EPOCH + Duration::from_secs(secs)),
                            is_dir: matches!(size, Some(TrashItemSize::Entries(_))),
                            size,
                            item,
                        }
                    })
                    .collect();
                sort_items(&mut items, self.sort);
                self.selection
                    .retain(|id| items.iter().any(|i| &i.item.id == id));
                self.items = Some(items);
                self.error = None;
            }
            Err(err) => {
                self.items = Some(Vec::new());
                self.error = Some(format!("Cannot read the trash: {err}"));
            }
        }
    }

    fn visible(&self) -> Vec<usize> {
        let filter = self.filter.to_lowercase();
        self.items()
            .iter()
            .enumerate()
            .filter(|(_, i)| filter.is_empty() || i.name.to_lowercase().contains(&filter))
            .map(|(index, _)| index)
            .collect()
    }

    /// Where the selected items' contents are, inside the trash.
    fn selected_files(&self) -> Vec<PathBuf> {
        self.items()
            .iter()
            .filter(|i| self.selection.contains(&i.item.id))
            .filter_map(|i| stored_path(&i.item))
            .collect()
    }

    fn selected_items(&self) -> Vec<TrashItem> {
        self.items()
            .iter()
            .filter(|i| self.selection.contains(&i.item.id))
            .map(|i| i.item.clone())
            .collect()
    }

    pub fn ui(&mut self, ui: &mut Ui, shared: &mut Shared) {
        ui.style_mut().interaction.selectable_labels = false;
        if self.items.is_none() || self.stale {
            self.load();
        }

        let mut actions = Vec::new();
        if shared.focused_tab == Some(self.id) {
            self.keyboard(ui.ctx(), &mut actions);
        }
        Panel::top(Id::new(("trash-toolbar", self.id)))
            .show(ui, |ui| self.toolbar(ui, &mut actions));
        let visible = self.visible();
        Panel::bottom(Id::new(("trash-status", self.id))).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(format!("{} items", visible.len()));
                if !self.selection.is_empty() {
                    ui.separator();
                    ui.label(format!("{} selected", self.selection.len()));
                }
                if let Some(error) = &self.error {
                    ui.separator();
                    ui.colored_label(ui.visuals().error_fg_color, error);
                }
            });
        });
        CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ui, |ui| {
                // Registered first so rows take precedence; receives clicks on empty space.
                let background = ui.interact(
                    ui.max_rect(),
                    Id::new(("trash-background", self.id)),
                    Sense::click(),
                );
                if visible.is_empty() {
                    ui.centered_and_justified(|ui| {
                        ui.weak(if self.filter.is_empty() {
                            "The trash is empty"
                        } else {
                            "No matches"
                        });
                    });
                } else {
                    self.table(ui, &visible, &mut actions);
                }
                if background.clicked() {
                    actions.push(Action::ClearSelection);
                }
                // The whole tab is a drop zone for moving files to the trash.
                if let Some(job) = dnd::target(&background, DropTarget::Trash) {
                    actions.push(Action::StartJob(job));
                }
                let clipboard_len = shared.clipboard.as_ref().map(|c| c.paths.len());
                background.context_menu(|ui| {
                    let ctx = ui.ctx().clone();
                    let keys = |command| crate::keymap::text(&ctx, command);
                    let has_items = !self.items().is_empty();
                    let mut item =
                        |ui: &mut Ui, enabled: bool, label: String, shortcut: &str, action| {
                            if ui
                                .add_enabled(enabled, Button::new(label).shortcut_text(shortcut))
                                .clicked()
                            {
                                actions.push(action);
                                ui.close();
                            }
                        };
                    item(
                        ui,
                        clipboard_len.is_some(),
                        format!(
                            "{} Move clipboard items to trash{}",
                            icon::CLIPBOARD,
                            clipboard_len.map(|n| format!(" ({n})")).unwrap_or_default()
                        ),
                        "Ctrl+V",
                        Action::TrashClipboard,
                    );
                    ui.separator();
                    item(
                        ui,
                        has_items,
                        format!("{} Select all", icon::CHECK_SQUARE),
                        &keys(C::SelectAll),
                        Action::SelectAll,
                    );
                    item(
                        ui,
                        true,
                        format!("{} Refresh", icon::ARROW_CLOCKWISE),
                        &keys(C::Refresh),
                        Action::Refresh,
                    );
                    ui.separator();
                    item(
                        ui,
                        has_items,
                        format!("{} Empty trash…", icon::TRASH),
                        "",
                        Action::AskEmpty,
                    );
                });
            });
        self.confirm_modal(ui.ctx(), shared);

        if !actions.is_empty() {
            ui.ctx().request_repaint();
        }
        for action in actions {
            self.apply(action, &visible, ui.ctx(), shared);
        }
    }

    fn keyboard(&mut self, ctx: &Context, actions: &mut Vec<Action>) {
        if ctx.text_edit_focused() || self.confirm_purge.is_some() {
            return;
        }
        let keymap = crate::keymap::get(ctx);
        if keymap.is_capturing() {
            return;
        }
        ctx.input_mut(|i| {
            if keymap.pressed(i, C::Trash) {
                actions.push(Action::AskPurge);
            }
            if keymap.pressed(i, C::SelectAll) {
                actions.push(Action::SelectAll);
            }
            if keymap.pressed(i, C::Refresh) {
                actions.push(Action::Refresh);
            }
            if i.events.iter().any(|e| matches!(e, egui::Event::Paste(_))) {
                actions.push(Action::TrashClipboard);
            }
        });
    }

    fn toolbar(&mut self, ui: &mut Ui, actions: &mut Vec<Action>) {
        crate::widgets::split_row(ui, |ui, side| match side {
            crate::widgets::Side::Left => {
                let has_selection = !self.selection.is_empty();
                if ui
                    .button(RichText::new(icon::ARROW_CLOCKWISE).size(16.0))
                    .on_hover_text(crate::keymap::hint(ui.ctx(), "Refresh", C::Refresh))
                    .clicked()
                {
                    actions.push(Action::Refresh);
                }
                ui.separator();
                if ui
                    .add_enabled(
                        has_selection,
                        Button::new(format!("{} Restore", icon::ARROW_COUNTER_CLOCKWISE)),
                    )
                    .on_hover_text("Move back to the original location")
                    .clicked()
                {
                    actions.push(Action::Restore);
                }
                if ui
                    .add_enabled(
                        has_selection,
                        Button::new(format!("{} Delete permanently", icon::X)),
                    )
                    .on_hover_text(crate::keymap::hint(ui.ctx(), "Delete", C::Trash))
                    .clicked()
                {
                    actions.push(Action::AskPurge);
                }
            }
            crate::widgets::Side::Right => {
                if ui
                    .add_enabled(
                        !self.items().is_empty(),
                        Button::new(format!("{} Empty trash", icon::TRASH)),
                    )
                    .clicked()
                {
                    actions.push(Action::AskEmpty);
                }
                ui.add(
                    TextEdit::singleline(&mut self.filter)
                        .hint_text(format!("{} Filter", icon::MAGNIFYING_GLASS))
                        .desired_width(180.0),
                );
            }
        });
    }

    fn table(&self, ui: &mut Ui, visible: &[usize], actions: &mut Vec<Action>) {
        let row_height = crate::widgets::row_height(ui);
        let colors = crate::theme::colors(ui.ctx());
        colors.apply_to_list(ui.visuals_mut());
        let columns = list_columns::Layout::new(ui, &COLUMNS);
        let drag_scroll = crate::touch::drag_scroll(ui.ctx());
        let table = TableBuilder::new(ui)
            .drag_to_scroll(drag_scroll)
            .id_salt(("trash", self.id))
            .striped(true)
            .sense(Sense::click())
            .cell_layout(Layout::left_to_right(Align::Center));
        let output = columns
            .columns(table)
            .header(row_height, |mut header| {
                let keys = [
                    SortKey::Name,
                    SortKey::Location,
                    SortKey::Deleted,
                    SortKey::Size,
                ];
                for column in columns.order() {
                    let key = keys[column];
                    let mut button = None;
                    let mut area = None;
                    let (_, cell) = header.col(|ui| {
                        area = Some(columns.header_drag(ui, column));
                        let arrow = match (self.sort.0 == key, self.sort.1) {
                            (false, _) => "",
                            (true, true) => icon::CARET_UP,
                            (true, false) => icon::CARET_DOWN,
                        };
                        let label = COLUMNS.columns[column].label;
                        let text = RichText::new(format!("{label} {arrow}")).strong();
                        button = Some(ui.add(Button::new(text).frame(false)));
                    });
                    let response = [button, area]
                        .into_iter()
                        .flatten()
                        .fold(cell, |response, other| response.union(other));
                    if response.clicked() {
                        actions.push(Action::SortBy(key));
                    }
                    columns.context_menu(&response);
                }
            })
            .body(|body| {
                body.rows(row_height, visible.len(), |mut row| {
                    let index = row.index();
                    let item = &self.items()[visible[index]];
                    let selected = self.selection.contains(&item.item.id);
                    row.set_selected(selected);
                    row.col(|ui| {
                        let path = Path::new(&item.name);
                        let glyph = fs::icon_for(path, item.is_dir, false);
                        let color = (!selected)
                            .then(|| colors.icon(fs::icon_kind(path, item.is_dir, false)))
                            .flatten();
                        crate::widgets::icon_label(ui, glyph, color, &item.name);
                    });
                    for column in columns.order().into_iter().skip(1) {
                        row.col(|ui| match column {
                            LOCATION => {
                                let parent = item.original.parent().unwrap_or(&item.original);
                                ui.label(parent.display().to_string());
                            }
                            DELETED => _ = crate::dates::label(ui, item.deleted, false),
                            SIZE => _ = ui.label(item.size_text()),
                            _ => {}
                        });
                    }

                    let response = row.response();
                    if response.clicked() {
                        // A tap adds or removes the item, for the toolbar's
                        // restore and delete: trashed items don't open.
                        let modifiers = if crate::touch::is_touch(&response.ctx) {
                            Modifiers::COMMAND
                        } else {
                            response.ctx.input(|i| i.modifiers)
                        };
                        actions.push(Action::Select { index, modifiers });
                    }
                    if response.secondary_clicked() && !selected {
                        actions.push(Action::Select {
                            index,
                            modifiers: Modifiers::NONE,
                        });
                    }
                    response.context_menu(|ui| {
                        let open_with = open_with::submenu(ui, || {
                            if selected {
                                self.selected_files()
                            } else {
                                stored_path(&item.item).into_iter().collect()
                            }
                        });
                        match open_with {
                            Some(open_with::Choice::Open(target)) => {
                                actions.push(Action::OpenWith(Some(target)));
                            }
                            Some(open_with::Choice::Other) => actions.push(Action::OpenWith(None)),
                            None => {}
                        }
                        ui.separator();
                        let mut item = |ui: &mut Ui, label: String, action| {
                            if ui.button(label).clicked() {
                                actions.push(action);
                                ui.close();
                            }
                        };
                        item(
                            ui,
                            format!("{} Restore", icon::ARROW_COUNTER_CLOCKWISE),
                            Action::Restore,
                        );
                        item(
                            ui,
                            format!("{} Delete permanently…", icon::X),
                            Action::AskPurge,
                        );
                        ui.separator();
                        item(
                            ui,
                            format!("{} Copy original path", icon::CLIPBOARD),
                            Action::CopyOriginalPath,
                        );
                        ui.separator();
                        item(ui, format!("{} Properties", icon::INFO), Action::Properties);
                    });
                });
            });
        columns.resize_handles(ui, output.inner_rect.bottom());
    }

    fn confirm_modal(&mut self, ctx: &Context, shared: &mut Shared) {
        let Some(items) = &self.confirm_purge else {
            return;
        };
        let count = items.len();
        let mut confirmed = false;
        let mut cancelled = false;
        let modal = Modal::new(Id::new(("confirm-purge", self.id))).show(ctx, |ui| {
            ui.set_max_width(crate::widgets::modal_width(ui, f32::INFINITY));
            ui.heading("Delete permanently?");
            ui.label(if count == 1 {
                "1 item will be deleted. This can't be undone.".to_owned()
            } else {
                format!("{count} items will be deleted. This can't be undone.")
            });
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                confirmed = ui
                    .button(format!("{} Delete permanently", icon::X))
                    .clicked();
                cancelled = ui.button("Cancel").clicked();
            });
        });
        if confirmed {
            let items = self.confirm_purge.take().unwrap_or_default();
            self.selection.clear();
            shared.jobs.start(JobKind::Purge { items });
        } else if cancelled || modal.should_close() {
            self.confirm_purge = None;
        }
    }

    fn apply(&mut self, action: Action, visible: &[usize], ctx: &Context, shared: &mut Shared) {
        match action {
            Action::Select { index, modifiers } => self.select(index, modifiers, visible),
            Action::SelectAll => {
                self.selection = visible
                    .iter()
                    .map(|&i| self.items()[i].item.id.clone())
                    .collect();
            }
            Action::ClearSelection => {
                self.selection.clear();
                self.anchor = None;
            }
            Action::Restore => {
                let items = self.selected_items();
                if !items.is_empty() {
                    self.selection.clear();
                    shared.jobs.start(JobKind::Restore { items });
                }
            }
            Action::AskPurge => {
                let items = self.selected_items();
                if !items.is_empty() {
                    self.confirm_purge = Some(items);
                }
            }
            Action::AskEmpty => {
                let items: Vec<TrashItem> = self.items().iter().map(|i| i.item.clone()).collect();
                if !items.is_empty() {
                    self.confirm_purge = Some(items);
                }
            }
            Action::Refresh => self.load(),
            Action::SortBy(key) => {
                self.sort = if self.sort.0 == key {
                    (key, !self.sort.1)
                } else {
                    // Newest deletions first, like the default.
                    (key, key != SortKey::Deleted)
                };
                if let Some(items) = &mut self.items {
                    sort_items(items, self.sort);
                }
                self.anchor = None;
            }
            Action::TrashClipboard => {
                if let Some(clipboard) = shared.clipboard.take() {
                    shared.jobs.start(JobKind::Trash {
                        paths: clipboard.paths,
                    });
                }
            }
            Action::StartJob(job) => shared.jobs.start(job),
            Action::OpenWith(target) => {
                let paths = self.selected_files();
                if !paths.is_empty() {
                    let request = open_with::Request::Files(paths);
                    shared
                        .app_actions
                        .push(AppAction::OpenWith(request, target));
                }
            }
            Action::CopyOriginalPath => {
                let paths: Vec<String> = self
                    .items()
                    .iter()
                    .filter(|i| self.selection.contains(&i.item.id))
                    .map(|i| i.original.display().to_string())
                    .collect();
                ctx.copy_text(paths.join("\n"));
            }
            Action::Properties => {
                let items: Vec<_> = self
                    .items()
                    .iter()
                    .filter(|i| self.selection.contains(&i.item.id))
                    .map(|i| crate::properties::TrashedItem {
                        name: i.name.clone(),
                        original: i.original.clone(),
                        deleted: i.deleted,
                        size: match i.size {
                            Some(TrashItemSize::Bytes(bytes)) => Some(bytes),
                            _ => None,
                        },
                        entries: match i.size {
                            Some(TrashItemSize::Entries(n)) => Some(n as u64),
                            _ => None,
                        },
                        is_dir: i.is_dir,
                    })
                    .collect();
                shared
                    .app_actions
                    .push(AppAction::Properties(crate::properties::Source::Trash(
                        items,
                    )));
            }
        }
    }

    fn select(&mut self, index: usize, modifiers: Modifiers, visible: &[usize]) {
        let Some(&item_index) = visible.get(index) else {
            return;
        };
        let id = self.items()[item_index].item.id.clone();
        if modifiers.shift
            && let Some(anchor) = self.anchor
        {
            let range = anchor.min(index)..=anchor.max(index);
            let ids: Vec<OsString> = visible[range]
                .iter()
                .map(|&i| self.items()[i].item.id.clone())
                .collect();
            if !modifiers.command {
                self.selection.clear();
            }
            self.selection.extend(ids);
        } else if modifiers.command {
            if !self.selection.remove(&id) {
                self.selection.insert(id);
            }
            self.anchor = Some(index);
        } else {
            self.selection = HashSet::from([id]);
            self.anchor = Some(index);
        }
    }
}

fn sort_items(items: &mut [Item], (key, ascending): (SortKey, bool)) {
    items.sort_by(|a, b| {
        let ord = match key {
            SortKey::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            SortKey::Location => a.original.cmp(&b.original),
            SortKey::Deleted => a.deleted.cmp(&b.deleted),
            SortKey::Size => size_key(a).cmp(&size_key(b)),
        };
        if ascending { ord } else { ord.reverse() }
    });
}

/// Folders (by entry count) sort before files (by bytes).
fn size_key(item: &Item) -> (bool, u64) {
    match &item.size {
        Some(TrashItemSize::Entries(n)) => (false, *n as u64),
        Some(TrashItemSize::Bytes(bytes)) => (true, *bytes),
        None => (true, 0),
    }
}

#[cfg(test)]
mod tests {
    use web_time::Instant;

    use egui::Key;

    use super::*;
    use crate::{images::ImageCache, jobs::Jobs, preview::TextCache};

    fn wait(shared: &mut Shared) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while shared.jobs.running() > 0 {
            assert!(Instant::now() < deadline, "job timed out");
            shared.jobs.poll();
            std::thread::sleep(Duration::from_millis(10));
        }
        shared.jobs.poll();
    }

    fn frame(ctx: &Context, tab: &mut TrashTab, shared: &mut Shared, keys: &[(Modifiers, Key)]) {
        let events = keys
            .iter()
            .map(|&(modifiers, key)| egui::Event::Key {
                key,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers,
            })
            .collect();
        let input = egui::RawInput {
            events,
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ui| tab.ui(ui, shared));
        output.textures_delta.clear();
    }

    /// Uses the real system trash, so it is opt-in: it only touches two files
    /// with a unique name and permanently deletes them at the end.
    ///
    /// `cargo test trash_round_trip -- --ignored`
    #[test]
    #[ignore]
    fn trash_round_trip() {
        let dir = std::env::current_dir()
            .unwrap()
            .join("target")
            .join("trash-test");
        disk::create_dir_all(&dir).unwrap();
        let tag = format!("egui-files-trash-test-{}", std::process::id());
        let paths: Vec<PathBuf> = ["a", "b"]
            .iter()
            .map(|n| dir.join(format!("{tag}-{n}.txt")))
            .collect();
        for path in &paths {
            disk::write(path, "trash me").unwrap();
        }

        let ctx = Context::default();
        let mut tab = TrashTab::new(1);
        let mut shared = Shared {
            show_hidden: false,
            show_thumbnails: false,
            icon_size: 64.0,
            clipboard: None,
            focused_tab: Some(1),
            preview_shown: false,
            app_actions: Vec::new(),
            usage: Default::default(),
            properties: Default::default(),
            images: ImageCache::new(&ctx),
            videos: crate::video::Videos::new(&ctx),
            texts: TextCache::default(),
            jobs: Jobs::new(&ctx),
            archives: crate::archive::Listings::new(&ctx),
            archive_prefs: Default::default(),
            open_with: Default::default(),
            editor: Default::default(),
        };
        let trash_both = |shared: &mut Shared| {
            shared.jobs.start(JobKind::Trash {
                paths: paths.clone(),
            });
            wait(shared);
            assert!(paths.iter().all(|p| !p.exists()), "moved to trash");
        };

        // Trashed files show up with their original location.
        trash_both(&mut shared);
        tab.filter = tag.clone();
        tab.mark_stale();
        frame(&ctx, &mut tab, &mut shared, &[]);
        let visible = tab.visible();
        assert_eq!(visible.len(), 2);
        assert!(tab.items()[visible[0]].original.starts_with(&dir));

        // Ctrl+A then Restore puts them back.
        frame(&ctx, &mut tab, &mut shared, &[(Modifiers::COMMAND, Key::A)]);
        assert_eq!(tab.selection.len(), 2);
        tab.apply(Action::Restore, &visible, &ctx, &mut shared);
        wait(&mut shared);
        assert!(paths.iter().all(|p| p.exists()), "restored");

        // Ctrl+V in the trash tab trashes what was copied, and the count follows.
        let count = TrashCount::new(&ctx);
        let deadline = Instant::now() + Duration::from_secs(10);
        let wait_count = |count: &TrashCount, until: &dyn Fn(usize) -> bool| {
            while !count.get().is_some_and(until) {
                assert!(Instant::now() < deadline, "trash count timed out");
                std::thread::sleep(Duration::from_millis(10));
            }
            count.get().unwrap()
        };
        let before = wait_count(&count, &|_| true);
        shared.clipboard = Some(crate::tab::FileClipboard {
            paths: paths.clone(),
            cut: false,
        });
        let paste = egui::Event::Paste(String::new());
        let input = egui::RawInput {
            events: vec![paste],
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ui| tab.ui(ui, &mut shared));
        output.textures_delta.clear();
        wait(&mut shared);
        assert!(paths.iter().all(|p| !p.exists()), "pasted into trash");
        assert!(shared.clipboard.is_none(), "clipboard consumed");
        count.request();
        assert_eq!(wait_count(&count, &|n| n != before), before + 2);

        // Delete asks first; confirming purges them for good.
        tab.mark_stale();
        frame(&ctx, &mut tab, &mut shared, &[(Modifiers::COMMAND, Key::A)]);
        frame(
            &ctx,
            &mut tab,
            &mut shared,
            &[(Modifiers::NONE, Key::Delete)],
        );
        let pending = tab.confirm_purge.take().expect("asks for confirmation");
        assert_eq!(pending.len(), 2);
        shared.jobs.start(JobKind::Purge { items: pending });
        wait(&mut shared);
        tab.mark_stale();
        frame(&ctx, &mut tab, &mut shared, &[]);
        assert!(tab.visible().is_empty(), "purged from trash");
        assert!(paths.iter().all(|p| !p.exists()));

        disk::remove_dir(&dir).unwrap();
    }
}
