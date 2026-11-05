//! A tab showing what's inside an archive, as a tree. Entries can be
//! extracted, dragged out to a folder, or opened.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::SystemTime,
};

use egui::{
    Align, Button, CentralPanel, Context, Id, Key, Layout, Modifiers, Panel, RichText, Sense,
    TextEdit, Ui,
};
use egui_extras::TableBuilder;
use egui_phosphor::regular as icon;
use serde::{Deserialize, Serialize};

use super::{
    ExtractJob, ExtractMode, ListState, Listing, Selection,
    dialogs::{DialogResult, ExtractDialog},
};
use crate::{
    dnd::{self, ArchiveDrag},
    fs::{self, display_name, human_size},
    jobs::JobKind,
    list_columns::{self, Spec},
    open_with,
    tab::{AppAction, Shared},
};

const SIZE: usize = 1;
const PACKED: usize = 2;
const MODIFIED: usize = 3;

pub static COLUMNS: list_columns::Table = list_columns::Table {
    key: "archive",
    columns: &[
        Spec {
            label: "Name",
            width: 0.0,
            min: 200.0,
        },
        Spec {
            label: "Size",
            width: 90.0,
            min: 60.0,
        },
        Spec {
            label: "Packed",
            width: 90.0,
            min: 60.0,
        },
        Spec {
            label: "Modified",
            width: 140.0,
            min: 80.0,
        },
    ],
    hide_order: &[PACKED, MODIFIED, SIZE],
    hidden: &[],
};

const INDENT: f32 = 14.0;

struct Node {
    name: String,
    path: String,
    is_dir: bool,
    /// For folders, the total of their contents.
    size: u64,
    packed: Option<u64>,
    modified: Option<SystemTime>,
    encrypted: bool,
    children: Vec<usize>,
}

/// The listing arranged as a tree, with folders the archive only implies added.
struct Tree {
    listing: Arc<Listing>,
    nodes: Vec<Node>,
    roots: Vec<usize>,
}

impl Tree {
    fn new(listing: Arc<Listing>) -> Self {
        let mut tree = Self {
            listing: listing.clone(),
            nodes: Vec::new(),
            roots: Vec::new(),
        };
        let mut index = HashMap::new();
        for entry in &listing.entries {
            if entry.path.is_empty() {
                continue;
            }
            let node = tree.node(&mut index, &entry.path, entry.is_dir);
            let node = &mut tree.nodes[node];
            node.is_dir = entry.is_dir;
            node.size = entry.size;
            node.packed = entry.packed;
            node.modified = entry.modified;
            node.encrypted = entry.encrypted;
        }
        let roots = std::mem::take(&mut tree.roots);
        for &root in &roots {
            tree.finish(root);
        }
        tree.roots = roots;
        tree.sort_children(None);
        tree
    }

    /// The node for `path`, creating it and its parents as needed.
    fn node(&mut self, index: &mut HashMap<String, usize>, path: &str, is_dir: bool) -> usize {
        if let Some(&node) = index.get(path) {
            return node;
        }
        let (parent, name) = match path.rsplit_once('/') {
            Some((parent, name)) => (Some(self.node(index, parent, true)), name),
            None => (None, path),
        };
        let node = self.nodes.len();
        self.nodes.push(Node {
            name: name.to_owned(),
            path: path.to_owned(),
            is_dir,
            size: 0,
            packed: None,
            modified: None,
            encrypted: false,
            children: Vec::new(),
        });
        match parent {
            Some(parent) => self.nodes[parent].children.push(node),
            None => self.roots.push(node),
        }
        index.insert(path.to_owned(), node);
        node
    }

    /// Adds up folder sizes.
    fn finish(&mut self, node: usize) -> u64 {
        if !self.nodes[node].is_dir {
            return self.nodes[node].size;
        }
        let children = self.nodes[node].children.clone();
        let size = children.into_iter().map(|c| self.finish(c)).sum();
        self.nodes[node].size = size;
        size
    }

    fn sort_children(&mut self, node: Option<usize>) {
        let mut children = match node {
            Some(node) => std::mem::take(&mut self.nodes[node].children),
            None => std::mem::take(&mut self.roots),
        };
        children.sort_by(|&a, &b| {
            let (a, b) = (&self.nodes[a], &self.nodes[b]);
            b.is_dir
                .cmp(&a.is_dir)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        for &child in &children {
            self.sort_children(Some(child));
        }
        match node {
            Some(node) => self.nodes[node].children = children,
            None => self.roots = children,
        }
    }

    fn folders(&self) -> usize {
        self.nodes.iter().filter(|n| n.is_dir).count()
    }
}

/// A visible row: a node and how deep it is drawn.
#[derive(Clone, Copy)]
struct Row {
    node: usize,
    depth: usize,
}

enum Action {
    Select {
        index: usize,
        modifiers: Modifiers,
    },
    SelectAll,
    MoveCursor(isize),
    Toggle(usize),
    /// Collapse the cursor's folder, or go to its parent.
    Left,
    /// Expand the cursor's folder.
    Right,
    Activate(usize),
    ActivateCursor,
    /// Open a file with an app, or ask which when `None`.
    OpenWith(usize, Option<open_with::Target>),
    ExtractHere {
        selected: bool,
    },
    ExtractTo {
        selected: bool,
    },
    ClearSelection,
    ExpandAll(bool),
    Unlock,
    Reload,
    Properties,
}

#[derive(Serialize, Deserialize)]
pub struct ArchiveTab {
    pub id: u64,
    pub archive: PathBuf,

    #[serde(skip)]
    tree: Option<Tree>,
    #[serde(skip)]
    expanded: HashSet<String>,
    #[serde(skip)]
    selection: HashSet<String>,
    #[serde(skip)]
    anchor: Option<usize>,
    #[serde(skip)]
    cursor: Option<usize>,
    #[serde(skip)]
    scroll_to: Option<usize>,
    #[serde(skip)]
    filter: String,
    #[serde(skip)]
    password: String,
    #[serde(skip)]
    show_password: bool,
    #[serde(skip)]
    dialog: Option<ExtractDialog>,
}

impl ArchiveTab {
    /// Clears the selection, for Android's back; whether there was one.
    pub fn clear_selection(&mut self) -> bool {
        self.anchor = None;
        let had = !self.selection.is_empty();
        self.selection.clear();
        had
    }

    pub fn new(id: u64, archive: PathBuf) -> Self {
        Self {
            id,
            archive,
            tree: None,
            expanded: HashSet::new(),
            selection: HashSet::new(),
            anchor: None,
            cursor: None,
            scroll_to: None,
            filter: String::new(),
            password: String::new(),
            show_password: false,
            dialog: None,
        }
    }

    pub fn title(&self) -> String {
        format!("{} {}", icon::FILE_ZIP, display_name(&self.archive))
    }

    /// The folder the archive is in, which the tab watches.
    pub fn dir(&self) -> Option<PathBuf> {
        self.archive.parent().map(Path::to_path_buf)
    }

    fn rows(&self) -> Vec<Row> {
        let Some(tree) = &self.tree else {
            return Vec::new();
        };
        let mut rows = Vec::new();
        if self.filter.is_empty() {
            let mut stack: Vec<Row> = tree
                .roots
                .iter()
                .rev()
                .map(|&node| Row { node, depth: 0 })
                .collect();
            while let Some(row) = stack.pop() {
                rows.push(row);
                let node = &tree.nodes[row.node];
                if node.is_dir && self.expanded.contains(&node.path) {
                    stack.extend(node.children.iter().rev().map(|&child| Row {
                        node: child,
                        depth: row.depth + 1,
                    }));
                }
            }
        } else {
            // Every match, flat, with its full path.
            let filter = self.filter.to_lowercase();
            let mut stack: Vec<usize> = tree.roots.iter().rev().copied().collect();
            while let Some(node) = stack.pop() {
                let n = &tree.nodes[node];
                if n.name.to_lowercase().contains(&filter) {
                    rows.push(Row { node, depth: 0 });
                }
                stack.extend(n.children.iter().rev());
            }
        }
        rows
    }

    fn selected_paths(&self, rows: &[Row]) -> Vec<String> {
        let Some(tree) = &self.tree else {
            return Vec::new();
        };
        // In display order, without entries already covered by a selected folder.
        let mut paths: Vec<String> = Vec::new();
        for row in rows {
            let path = &tree.nodes[row.node].path;
            if self.selection.contains(path)
                && !paths.iter().any(|p| {
                    path.starts_with(p.as_str()) && path.as_bytes().get(p.len()) == Some(&b'/')
                })
            {
                paths.push(path.clone());
            }
        }
        paths
    }

    pub fn ui(&mut self, ui: &mut Ui, shared: &mut Shared) {
        ui.style_mut().interaction.selectable_labels = false;
        let state = shared.archives.get(&self.archive);
        if let ListState::Ready(listing) = &state
            && !self
                .tree
                .as_ref()
                .is_some_and(|t| Arc::ptr_eq(&t.listing, listing))
        {
            self.tree = Some(Tree::new(listing.clone()));
            let tree = self.tree.as_ref().expect("just built");
            let paths: HashSet<&String> = tree.nodes.iter().map(|n| &n.path).collect();
            self.selection.retain(|p| paths.contains(p));
            // A lone top-level folder starts out open.
            if let [root] = tree.roots.as_slice()
                && tree.nodes[*root].is_dir
            {
                self.expanded.insert(tree.nodes[*root].path.clone());
            }
        }

        let mut actions = Vec::new();
        if shared.focused_tab == Some(self.id) {
            self.keyboard(ui.ctx(), &mut actions);
        }
        let rows = self.rows();
        let ready = matches!(state, ListState::Ready(_));
        Panel::top(Id::new(("archive-toolbar", self.id)))
            .show(ui, |ui| self.toolbar(ui, ready, &mut actions));
        Panel::bottom(Id::new(("archive-status", self.id))).show(ui, |ui| self.status_bar(ui));
        CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ui, |ui| match &state {
                ListState::Loading => {
                    ui.centered_and_justified(|ui| {
                        ui.horizontal_centered(|ui| {
                            ui.spinner();
                            ui.weak("Reading archive…");
                        });
                    });
                }
                ListState::Failed(error) => {
                    ui.centered_and_justified(|ui| {
                        ui.colored_label(
                            ui.visuals().error_fg_color,
                            format!("Cannot read {}: {error}", display_name(&self.archive)),
                        );
                    });
                }
                ListState::NeedsPassword { wrong } => self.password_form(ui, *wrong, &mut actions),
                ListState::Ready(_) => {
                    let background = ui.interact(
                        ui.max_rect(),
                        Id::new(("archive-background", self.id)),
                        Sense::click(),
                    );
                    if rows.is_empty() {
                        ui.centered_and_justified(|ui| {
                            ui.weak(if self.filter.is_empty() {
                                "Empty archive"
                            } else {
                                "No matches"
                            });
                        });
                    } else {
                        self.table(ui, &rows, shared, &mut actions);
                    }
                    if background.clicked() {
                        actions.push(Action::ClearSelection);
                    }
                }
            });
        self.dialog_ui(ui.ctx(), shared);
        self.scroll_to = None;

        if !actions.is_empty() {
            ui.ctx().request_repaint();
        }
        for action in actions {
            self.apply(action, &rows, shared);
        }
    }

    fn keyboard(&mut self, ctx: &Context, actions: &mut Vec<Action>) {
        if ctx.text_edit_focused() || self.dialog.is_some() {
            return;
        }
        use crate::keymap::Command as C;
        let keymap = crate::keymap::get(ctx);
        if keymap.is_capturing() {
            return;
        }
        ctx.input_mut(|i| {
            if keymap.pressed(i, C::ExtractTo) {
                actions.push(Action::ExtractTo { selected: false });
            }
            if keymap.pressed(i, C::Extract) {
                actions.push(Action::ExtractHere { selected: false });
            }
            if keymap.pressed(i, C::SelectAll) {
                actions.push(Action::SelectAll);
            }
            if keymap.pressed(i, C::Refresh) {
                actions.push(Action::Reload);
            }
            let mut key = |mods, key| i.consume_key(mods, key);
            if key(Modifiers::NONE, Key::ArrowDown) {
                actions.push(Action::MoveCursor(1));
            }
            if key(Modifiers::NONE, Key::ArrowUp) {
                actions.push(Action::MoveCursor(-1));
            }
            if key(Modifiers::NONE, Key::Home) {
                actions.push(Action::MoveCursor(isize::MIN));
            }
            if key(Modifiers::NONE, Key::End) {
                actions.push(Action::MoveCursor(isize::MAX));
            }
            if key(Modifiers::NONE, Key::ArrowLeft) {
                actions.push(Action::Left);
            }
            if key(Modifiers::NONE, Key::ArrowRight) {
                actions.push(Action::Right);
            }
            if key(Modifiers::NONE, Key::Enter) {
                actions.push(Action::ActivateCursor);
            }
        });
    }

    fn toolbar(&mut self, ui: &mut Ui, ready: bool, actions: &mut Vec<Action>) {
        crate::widgets::split_row(ui, |ui, side| match side {
            crate::widgets::Side::Left => {
                let has_selection = !self.selection.is_empty();
                if ui
                    .add_enabled(
                        ready,
                        Button::new(format!("{} Extract here", icon::ARCHIVE)),
                    )
                    .on_hover_text(crate::keymap::hint(
                        ui.ctx(),
                        "Extract everything next to the archive",
                        crate::keymap::Command::Extract,
                    ))
                    .clicked()
                {
                    actions.push(Action::ExtractHere { selected: false });
                }
                if ui
                    .add_enabled(
                        ready,
                        Button::new(format!("{} Extract to…", icon::FOLDER_OPEN)),
                    )
                    .on_hover_text(crate::keymap::hint(
                        ui.ctx(),
                        "Choose where to extract everything",
                        crate::keymap::Command::ExtractTo,
                    ))
                    .clicked()
                {
                    actions.push(Action::ExtractTo { selected: false });
                }
                ui.separator();
                if ui
                    .add_enabled(
                        has_selection,
                        Button::new(format!("{} Extract selected…", icon::FILE_ARROW_DOWN)),
                    )
                    .on_hover_text("Or drag them to a folder")
                    .clicked()
                {
                    actions.push(Action::ExtractTo { selected: true });
                }
            }
            crate::widgets::Side::Right => {
                if ui
                    .add_enabled(ready, Button::new(icon::ARROWS_IN_SIMPLE))
                    .on_hover_text("Collapse all")
                    .clicked()
                {
                    actions.push(Action::ExpandAll(false));
                }
                if ui
                    .add_enabled(ready, Button::new(icon::ARROWS_OUT_SIMPLE))
                    .on_hover_text("Expand all")
                    .clicked()
                {
                    actions.push(Action::ExpandAll(true));
                }
                ui.add(
                    TextEdit::singleline(&mut self.filter)
                        .hint_text(format!("{} Filter", icon::MAGNIFYING_GLASS))
                        .desired_width(180.0),
                );
            }
        });
    }

    fn status_bar(&self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            let Some(tree) = &self.tree else {
                ui.weak(display_name(&self.archive));
                return;
            };
            let listing = &tree.listing;
            let total = listing.total_size();
            ui.label(format!(
                "{} files, {} folders · {}",
                listing.files(),
                tree.folders(),
                human_size(total)
            ));
            ui.separator();
            let ratio = if total > 0 {
                format!(
                    " ({:.0}%)",
                    listing.archive_size as f64 * 100.0 / total as f64
                )
            } else {
                String::new()
            };
            ui.label(format!(
                "{} packed{ratio}",
                human_size(listing.archive_size)
            ));
            ui.separator();
            ui.label(listing.format.label());
            if listing.encrypted() {
                ui.separator();
                ui.label(format!("{} Encrypted", icon::LOCK));
            }
            if !self.selection.is_empty() {
                ui.separator();
                ui.label(format!("{} selected", self.selection.len()));
            }
        });
    }

    fn password_form(&mut self, ui: &mut Ui, wrong: bool, actions: &mut Vec<Action>) {
        ui.vertical_centered(|ui| {
            ui.add_space(48.0);
            ui.label(RichText::new(icon::LOCK).size(48.0));
            ui.heading("This archive is encrypted");
            ui.label("Enter its password to see what's inside.");
            if wrong {
                ui.colored_label(ui.visuals().error_fg_color, "Wrong password, try again.");
            }
            ui.add_space(8.0);
            let edit = ui.add(
                TextEdit::singleline(&mut self.password)
                    .password(!self.show_password)
                    .hint_text("Password")
                    .desired_width(260.0),
            );
            if edit.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                actions.push(Action::Unlock);
            }
            ui.checkbox(&mut self.show_password, "Show password");
            if ui.button(format!("{} Unlock", icon::LOCK_OPEN)).clicked() {
                actions.push(Action::Unlock);
            }
        });
    }

    fn table(&self, ui: &mut Ui, rows: &[Row], shared: &Shared, actions: &mut Vec<Action>) {
        let Some(tree) = &self.tree else {
            return;
        };
        let row_height = crate::widgets::row_height(ui);
        let password = shared.archives.password(&self.archive);
        let columns = list_columns::Layout::new(ui, &COLUMNS);
        let drag_scroll = crate::touch::drag_scroll(ui.ctx());
        let sense = crate::touch::item_sense(ui.ctx());
        let mut table = TableBuilder::new(ui)
            .drag_to_scroll(drag_scroll)
            .id_salt(("archive", self.id))
            .striped(true)
            .sense(sense)
            .cell_layout(Layout::left_to_right(Align::Center));
        table = columns.columns(table);
        if let Some(index) = self.scroll_to {
            table = table.scroll_to_row(index, None);
        }
        let output = table
            .header(row_height, |mut header| {
                for column in columns.order() {
                    let mut area = None;
                    let (_, cell) = header.col(|ui| {
                        area = Some(columns.header_drag(ui, column));
                        ui.strong(COLUMNS.columns[column].label);
                    });
                    columns.context_menu(&area.map_or(cell.clone(), |area| cell.union(area)));
                }
            })
            .body(|body| {
                body.rows(row_height, rows.len(), |mut row| {
                    let index = row.index();
                    let Row { node, depth } = rows[index];
                    let n = &tree.nodes[node];
                    let selected = self.selection.contains(&n.path);
                    row.set_selected(
                        selected || self.cursor == Some(index) && self.selection.is_empty(),
                    );
                    let mut caret_rect = None;
                    row.col(|ui| {
                        ui.add_space(depth as f32 * INDENT);
                        let caret = match (
                            n.is_dir && !n.children.is_empty(),
                            self.expanded.contains(&n.path),
                        ) {
                            (false, _) => " ",
                            (true, true) => icon::CARET_DOWN,
                            (true, false) => icon::CARET_RIGHT,
                        };
                        let caret = ui.add(
                            egui::Label::new(RichText::new(caret).weak()).sense(Sense::hover()),
                        );
                        caret_rect = Some(caret.rect.expand(3.0));
                        let glyph = fs::icon_for(Path::new(&n.name), n.is_dir, false);
                        let name = if self.filter.is_empty() {
                            &n.name
                        } else {
                            &n.path
                        };
                        let color = (!selected)
                            .then(|| {
                                crate::theme::colors(ui.ctx()).icon(fs::icon_kind(
                                    Path::new(&n.name),
                                    n.is_dir,
                                    false,
                                ))
                            })
                            .flatten();
                        crate::widgets::icon_label(ui, glyph, color, name);
                        if n.encrypted {
                            ui.weak(icon::LOCK).on_hover_text("Encrypted");
                        }
                    });
                    for column in columns.order().into_iter().skip(1) {
                        row.col(|ui| match column {
                            SIZE => _ = ui.label(human_size(n.size)),
                            PACKED => _ = ui.label(n.packed.map(human_size).unwrap_or_default()),
                            MODIFIED => _ = crate::dates::label(ui, n.modified, false),
                            _ => {}
                        });
                    }

                    let response = row.response();
                    // Dragging a selected row drags the whole selection.
                    dnd::archive_source(&response, || {
                        let paths = if selected {
                            self.selected_paths(rows)
                        } else {
                            vec![n.path.clone()]
                        };
                        Some(ArchiveDrag {
                            archive: self.archive.clone(),
                            selection: Selection::new(paths),
                            password: password.clone(),
                        })
                    });
                    let on_caret = response
                        .interact_pointer_pos()
                        .zip(caret_rect)
                        .is_some_and(|(pos, rect)| rect.contains(pos));
                    if crate::touch::is_touch(&response.ctx) {
                        // A tap opens a file and opens or closes a folder,
                        // or while items are selected (a long press selects
                        // the item it opens the menu for), adds or removes it.
                        if response.clicked() {
                            if self.selection.is_empty() {
                                actions.push(Action::Activate(index));
                            } else {
                                actions.push(Action::Select {
                                    index,
                                    modifiers: Modifiers::COMMAND,
                                });
                            }
                        }
                    } else if response.double_clicked() {
                        actions.push(Action::Activate(index));
                    } else if response.clicked() && on_caret && n.is_dir {
                        actions.push(Action::Toggle(node));
                    } else if response.clicked() {
                        let modifiers = response.ctx.input(|i| i.modifiers);
                        actions.push(Action::Select { index, modifiers });
                    }
                    if response.secondary_clicked() && !selected {
                        actions.push(Action::Select {
                            index,
                            modifiers: Modifiers::NONE,
                        });
                    }
                    response.context_menu(|ui| {
                        let mut open_with = None;
                        let mut item = |ui: &mut Ui, label: String, action| {
                            if ui.button(label).clicked() {
                                actions.push(action);
                                ui.close();
                            }
                        };
                        if !n.is_dir {
                            item(
                                ui,
                                format!("{} Open", icon::ARROW_SQUARE_OUT),
                                Action::Activate(index),
                            );
                            let subject = open_with::Subject {
                                path: PathBuf::from(&n.path),
                                on_disk: false,
                            };
                            open_with = match open_with::submenu_for(ui, &[subject]) {
                                Some(open_with::Choice::Open(target)) => Some(Some(target)),
                                Some(open_with::Choice::Other) => Some(None),
                                None => None,
                            };
                            ui.separator();
                        }
                        let here = self.archive.parent().map(display_name).unwrap_or_default();
                        item(
                            ui,
                            format!("{} Extract to “{here}”", icon::FILE_ARROW_DOWN),
                            Action::ExtractHere { selected: true },
                        );
                        item(
                            ui,
                            format!("{} Extract to…", icon::FOLDER_OPEN),
                            Action::ExtractTo { selected: true },
                        );
                        if n.is_dir {
                            ui.separator();
                            let label = if self.expanded.contains(&n.path) {
                                format!("{} Collapse", icon::CARET_DOWN)
                            } else {
                                format!("{} Expand", icon::CARET_RIGHT)
                            };
                            item(ui, label, Action::Toggle(node));
                        }
                        ui.separator();
                        item(ui, format!("{} Properties", icon::INFO), Action::Properties);
                        if let Some(target) = open_with {
                            actions.push(Action::OpenWith(index, target));
                        }
                    });
                });
            });
        columns.resize_handles(ui, output.inner_rect.bottom());
    }

    fn dialog_ui(&mut self, ctx: &Context, shared: &mut Shared) {
        let Some(dialog) = &mut self.dialog else {
            return;
        };
        match dialog.show(ctx, &mut shared.archive_prefs, shared.show_hidden) {
            DialogResult::Open => {}
            DialogResult::Closed => self.dialog = None,
            DialogResult::Start(jobs) => {
                for job in jobs {
                    shared.jobs.start(job);
                }
                self.dialog = None;
            }
        }
    }

    fn apply(&mut self, action: Action, rows: &[Row], shared: &mut Shared) {
        fn node_at<'t>(tab: &'t ArchiveTab, rows: &[Row], index: usize) -> Option<&'t Node> {
            let tree = tab.tree.as_ref()?;
            rows.get(index).map(|row| &tree.nodes[row.node])
        }
        match action {
            Action::Select { index, modifiers } => self.select(index, modifiers, rows),
            Action::SelectAll => {
                if let Some(tree) = &self.tree {
                    self.selection = rows
                        .iter()
                        .map(|r| tree.nodes[r.node].path.clone())
                        .collect();
                }
            }
            Action::MoveCursor(delta) => {
                if rows.is_empty() {
                    return;
                }
                let last = rows.len() - 1;
                let index = match self.cursor {
                    None if delta == isize::MAX => last,
                    None => 0,
                    Some(c) => c.saturating_add_signed(delta).min(last),
                };
                self.select(index, Modifiers::NONE, rows);
                self.scroll_to = Some(index);
            }
            Action::Toggle(node) => {
                if let Some(tree) = &self.tree {
                    let path = &tree.nodes[node].path;
                    if !self.expanded.remove(path) {
                        self.expanded.insert(path.clone());
                    }
                }
            }
            Action::Left | Action::Right => {
                let Some(index) = self.cursor else { return };
                let Some(n) = node_at(self, rows, index) else {
                    return;
                };
                let (path, is_dir) = (n.path.clone(), n.is_dir);
                let open = self.expanded.contains(&path);
                match action {
                    Action::Right if is_dir => {
                        self.expanded.insert(path);
                    }
                    Action::Left if is_dir && open => {
                        self.expanded.remove(&path);
                    }
                    Action::Left => {
                        // To the parent folder's row.
                        if let Some((parent, _)) = path.rsplit_once('/')
                            && let Some(tree) = &self.tree
                            && let Some(i) =
                                rows.iter().position(|r| tree.nodes[r.node].path == parent)
                        {
                            self.select(i, Modifiers::NONE, rows);
                            self.scroll_to = Some(i);
                        }
                    }
                    _ => {}
                }
            }
            Action::Activate(index) => self.activate(index, rows, shared),
            Action::Properties => {
                let Some(tree) = &self.tree else { return };
                let entries = self
                    .selected_paths(rows)
                    .iter()
                    .filter_map(|path| tree.nodes.iter().find(|n| &n.path == path))
                    .map(|n| crate::properties::ArchivedItem {
                        path: n.path.clone(),
                        is_dir: n.is_dir,
                        size: n.size,
                        packed: n.packed,
                        modified: n.modified,
                        encrypted: n.encrypted,
                    })
                    .collect();
                shared.app_actions.push(AppAction::Properties(
                    crate::properties::Source::Archive {
                        archive: self.archive.clone(),
                        entries,
                    },
                ));
            }
            Action::OpenWith(index, target) => {
                if let Some(job) = self.open_job(index, rows, shared) {
                    let request = open_with::Request::FromArchive(Box::new(job));
                    shared
                        .app_actions
                        .push(AppAction::OpenWith(request, target));
                }
            }
            Action::ActivateCursor => {
                if let Some(index) = self.cursor {
                    self.activate(index, rows, shared);
                }
            }
            Action::ExtractHere { selected } => {
                let Some(dest) = self.dir() else { return };
                let (mode, selection) = if selected {
                    (
                        ExtractMode::Into,
                        Some(Selection::new(self.selected_paths(rows))),
                    )
                } else {
                    (ExtractMode::Smart, None)
                };
                if selection.as_ref().is_some_and(|s| s.paths.is_empty()) {
                    return;
                }
                let mut job = ExtractJob::new(self.archive.clone(), dest, mode, false);
                job.selection = selection;
                job.password = shared.archives.password(&self.archive);
                shared.jobs.start(JobKind::Extract(job));
            }
            Action::ExtractTo { selected } => {
                let selection = selected.then(|| Selection::new(self.selected_paths(rows)));
                if selection.as_ref().is_some_and(|s| s.paths.is_empty()) {
                    return;
                }
                self.dialog = Some(ExtractDialog::new(
                    Id::new(("archive-extract", self.id)),
                    vec![self.archive.clone()],
                    selection,
                    shared.archives.password(&self.archive),
                ));
            }
            Action::ClearSelection => {
                self.selection.clear();
                self.anchor = None;
                self.cursor = None;
            }
            Action::ExpandAll(expand) => {
                self.expanded.clear();
                if expand && let Some(tree) = &self.tree {
                    self.expanded = tree
                        .nodes
                        .iter()
                        .filter(|n| n.is_dir)
                        .map(|n| n.path.clone())
                        .collect();
                }
            }
            Action::Unlock => {
                let password = std::mem::take(&mut self.password);
                shared.archives.unlock(&self.archive, password);
            }
            Action::Reload => {
                let password = shared.archives.password(&self.archive);
                match password {
                    Some(password) => shared.archives.unlock(&self.archive, password),
                    None => shared.archives.forget(&self.archive),
                }
            }
        }
    }

    /// Folders open or close; files are extracted to a temporary folder and opened.
    fn activate(&mut self, index: usize, rows: &[Row], shared: &mut Shared) {
        let Some(tree) = &self.tree else { return };
        let Some(row) = rows.get(index) else { return };
        let n = &tree.nodes[row.node];
        if n.is_dir {
            let node = row.node;
            self.apply(Action::Toggle(node), rows, shared);
            return;
        }
        if let Some(job) = self.open_job(index, rows, shared) {
            shared.jobs.start(JobKind::Extract(job));
        }
    }

    /// A job extracting a file to a temporary folder and opening it.
    fn open_job(&self, index: usize, rows: &[Row], shared: &Shared) -> Option<ExtractJob> {
        let n = &self.tree.as_ref()?.nodes[rows.get(index)?.node];
        if n.is_dir {
            return None;
        }
        let mut job = ExtractJob::new(
            self.archive.clone(),
            PathBuf::new(),
            ExtractMode::Open,
            false,
        );
        job.selection = Some(Selection::new(vec![n.path.clone()]));
        job.password = shared.archives.password(&self.archive);
        Some(job)
    }

    fn select(&mut self, index: usize, modifiers: Modifiers, rows: &[Row]) {
        let Some(tree) = &self.tree else { return };
        let path_at = |i: usize| tree.nodes[rows[i].node].path.clone();
        if index >= rows.len() {
            return;
        }
        let path = path_at(index);
        if modifiers.shift
            && let Some(anchor) = self.anchor.filter(|&a| a < rows.len())
        {
            let range: Vec<String> = (anchor.min(index)..=anchor.max(index))
                .map(path_at)
                .collect();
            if modifiers.command {
                self.selection.extend(range);
            } else {
                self.selection = range.into_iter().collect();
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        archive::{CompressJob, Format, Listings, Prefs, compress},
        images::ImageCache,
        jobs::{Jobs, Progress},
        preview::TextCache,
        video::Videos,
    };

    fn shared(ctx: &Context) -> Shared {
        Shared {
            show_hidden: false,
            show_thumbnails: false,
            icon_size: 64.0,
            list_scale: 1.0,
            clipboard: None,
            focused_tab: Some(1),
            preview_shown: false,
            app_actions: Vec::new(),
            usage: Default::default(),
            properties: Default::default(),
            images: ImageCache::new(ctx),
            videos: Videos::new(ctx),
            texts: TextCache::default(),
            markdown_images: Default::default(),
            jobs: Jobs::new(ctx),
            archives: Listings::new(ctx),
            archive_prefs: Prefs::default(),
            open_with: Default::default(),
            editor: Default::default(),
        }
    }

    fn frame(ctx: &Context, tab: &mut ArchiveTab, shared: &mut Shared, events: Vec<egui::Event>) {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(900.0, 600.0),
            )),
            events,
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ui| tab.ui(ui, shared));
        output.textures_delta.clear();
    }

    /// Runs frames until the listing has been read.
    fn load(ctx: &Context, tab: &mut ArchiveTab, shared: &mut Shared) -> ListState {
        let deadline = web_time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            frame(ctx, tab, shared, Vec::new());
            match shared.archives.get(&tab.archive) {
                ListState::Loading => {
                    assert!(web_time::Instant::now() < deadline, "listing timed out");
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                state => {
                    frame(ctx, tab, shared, Vec::new());
                    return state;
                }
            }
        }
    }

    #[test]
    fn browses_unlocks_and_extracts() {
        let root =
            std::env::temp_dir().join(format!("egui-files-archive-tab-{}", std::process::id()));
        let _ = disk::remove_dir_all(&root);
        disk::create_dir_all(root.join("music/album")).unwrap();
        disk::write(root.join("music/album/song.flac"), "la".repeat(1000)).unwrap();
        disk::write(root.join("music/notes.txt"), "notes").unwrap();
        let archive = root.join("music.7z");
        let p = Progress::default();
        crate::jobs::block_on(compress(
            CompressJob {
                sources: vec![root.join("music")],
                dest: archive.clone(),
                format: Format::SevenZ,
                level: 1,
                password: Some("pw".into()),
                encrypt_names: true,
            },
            &p,
        ));
        assert!(archive.is_file());

        let ctx = Context::default();
        let mut shared = shared(&ctx);
        let mut tab = ArchiveTab::new(1, archive.clone());
        assert!(matches!(
            load(&ctx, &mut tab, &mut shared),
            ListState::NeedsPassword { wrong: false }
        ));
        tab.password = "nope".into();
        tab.apply(Action::Unlock, &[], &mut shared);
        assert!(matches!(
            load(&ctx, &mut tab, &mut shared),
            ListState::NeedsPassword { wrong: true }
        ));
        tab.password = "pw".into();
        tab.apply(Action::Unlock, &[], &mut shared);
        assert!(matches!(
            load(&ctx, &mut tab, &mut shared),
            ListState::Ready(_)
        ));

        // The lone top folder starts open: music, album, notes.txt.
        let rows = tab.rows();
        let names: Vec<_> = rows
            .iter()
            .map(|r| tab.tree.as_ref().unwrap().nodes[r.node].path.as_str())
            .collect();
        assert_eq!(names, ["music", "music/album", "music/notes.txt"]);
        let album = &tab.tree.as_ref().unwrap().nodes[rows[1].node];
        assert_eq!(album.size, 2000, "folders add up their contents");

        // Expanding the album shows the song; filtering lists matches flat.
        tab.apply(
            Action::Select {
                index: 1,
                modifiers: Modifiers::NONE,
            },
            &rows,
            &mut shared,
        );
        tab.apply(Action::Right, &rows, &mut shared);
        assert_eq!(tab.rows().len(), 4);
        tab.filter = "song".into();
        assert_eq!(tab.rows().len(), 1);
        tab.filter.clear();

        // Extract the album next to the archive, using the password typed in.
        let rows = tab.rows();
        tab.apply(
            Action::Select {
                index: 1,
                modifiers: Modifiers::NONE,
            },
            &rows,
            &mut shared,
        );
        tab.apply(Action::ExtractHere { selected: true }, &rows, &mut shared);
        let deadline = web_time::Instant::now() + std::time::Duration::from_secs(5);
        while shared.jobs.running() > 0 {
            assert!(web_time::Instant::now() < deadline, "extract timed out");
            shared.jobs.poll();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(
            disk::read_to_string(root.join("album/song.flac"))
                .unwrap()
                .len(),
            2000
        );
        disk::remove_dir_all(root).unwrap();
    }
}
