//! Miller columns: one column per ancestor folder, then the current folder,
//! then a preview of the selected item.

use std::{hash::Hash, path::Path};

use egui::{Align, Layout, ScrollArea, Ui, vec2};
use egui_phosphor::regular as icon;

use crate::{
    dnd::{self, DropTarget},
    fs::Entry,
    images, preview,
    tab::{Action, BrowserTab, Shared},
    widgets::{self, ListRow},
};

const COLUMN_WIDTH: f32 = 220.0;

enum ScrollTo {
    None,
    /// Keep this row in view (keyboard navigation).
    Always(usize),
    /// Bring this row into view the first time it is shown.
    Once(usize),
}

pub fn show(
    ui: &mut Ui,
    tab: &BrowserTab,
    visible: &[usize],
    shared: &mut Shared,
    actions: &mut Vec<Action>,
) {
    let show_hidden = shared.show_hidden;
    let height = ui.available_height();
    ScrollArea::horizontal()
        .id_salt(("columns", tab.id))
        .auto_shrink(false)
        .stick_to_right(true)
        .show(ui, |ui| {
            ui.horizontal_top(|ui| {
                let mut chain: Vec<&Path> = tab.path.ancestors().collect();
                chain.reverse();
                for pair in chain.windows(2) {
                    column(ui, height, |ui| {
                        ancestor_column(ui, tab, pair[0], pair[1], show_hidden, actions);
                    });
                }
                let menu = super::MenuContext::new(shared);
                column(ui, height, |ui| {
                    current_column(ui, tab, visible, menu, actions)
                });
                if let Some(entry) = tab.preview_entry() {
                    column(ui, height, |ui| {
                        if entry.is_dir {
                            preview_folder(ui, tab, entry, show_hidden, actions);
                        } else {
                            preview_file(ui, tab, entry, shared, actions);
                        }
                    });
                }
            });
        });
}

fn column(ui: &mut Ui, height: f32, add_contents: impl FnOnce(&mut Ui)) {
    ui.allocate_ui_with_layout(
        vec2(COLUMN_WIDTH, height),
        Layout::top_down_justified(Align::Min),
        |ui| {
            ui.set_min_size(vec2(COLUMN_WIDTH, height));
            ui.set_max_width(COLUMN_WIDTH);
            add_contents(ui);
        },
    );
    ui.separator();
}

fn entry_list(
    ui: &mut Ui,
    id_salt: impl Hash + std::fmt::Debug,
    count: usize,
    scroll_to: ScrollTo,
    mut add_row: impl FnMut(&mut Ui, usize),
) {
    if count == 0 {
        ui.add_space(8.0);
        ui.vertical_centered(|ui| ui.weak("Empty folder"));
        return;
    }
    let row_height = widgets::row_height(ui);
    ui.spacing_mut().item_spacing.y = 0.0;
    let output = ScrollArea::vertical()
        .id_salt(id_salt)
        .auto_shrink(false)
        .show_rows(ui, row_height, count, |ui, range| {
            for i in range {
                add_row(ui, i);
            }
        });
    match scroll_to {
        ScrollTo::None => {}
        ScrollTo::Always(row) => widgets::ensure_row_visible(ui, &output, row, row_height),
        ScrollTo::Once(row) => widgets::ensure_row_visible_once(ui, &output, row, row_height),
    }
}

fn entry_row(ui: &mut Ui, entry: &Entry, selected: bool) -> egui::Response {
    let colors = crate::theme::colors(ui.ctx());
    ListRow {
        icon: entry.icon(),
        name: &entry.name,
        selected,
        weak: entry.hidden,
        icon_color: entry.icon_color(&colors),
        name_color: entry.name_color(&colors),
        indent: 0.0,
        leading: None,
        trailing: entry.is_dir.then_some(icon::CARET_RIGHT),
    }
    .show(ui)
}

/// Entries of a non-current folder, keeping `keep` even if it is hidden.
fn listing<'a>(
    tab: &'a BrowserTab,
    dir: &Path,
    show_hidden: bool,
    keep: Option<&Path>,
) -> Vec<&'a Entry> {
    tab.column_listing(dir)
        .iter()
        .filter(|e| show_hidden || !e.hidden || Some(e.path.as_path()) == keep)
        .collect()
}

/// Clicking selects the item in its own folder, which becomes the current one.
fn other_folder_interaction(
    response: &egui::Response,
    dir: &Path,
    entry: &Entry,
    actions: &mut Vec<Action>,
) {
    dnd::source(response, || vec![entry.path.clone()]);
    if entry.is_dir
        && let Some(job) = dnd::target(response, DropTarget::Folder(entry.path.clone()))
    {
        actions.push(Action::StartJob(job));
    }
    if response.double_clicked() {
        actions.push(Action::Activate(entry.path.clone()));
    } else if response.clicked() {
        actions.push(Action::NavigateSelect {
            dir: dir.to_path_buf(),
            select: entry.path.clone(),
        });
    } else if response.middle_clicked() && entry.is_dir {
        actions.push(Action::OpenInNewTab(entry.path.clone()));
    }
}

fn ancestor_column(
    ui: &mut Ui,
    tab: &BrowserTab,
    dir: &Path,
    child: &Path,
    show_hidden: bool,
    actions: &mut Vec<Action>,
) {
    let entries = listing(tab, dir, show_hidden, Some(child));
    let highlighted = entries.iter().position(|e| e.path == child);
    let scroll_to = highlighted.map_or(ScrollTo::None, ScrollTo::Once);
    entry_list(
        ui,
        ("column", tab.id, dir),
        entries.len(),
        scroll_to,
        |ui, i| {
            let entry = entries[i];
            let response = entry_row(ui, entry, Some(i) == highlighted);
            other_folder_interaction(&response, dir, entry, actions);
        },
    );
}

fn current_column(
    ui: &mut Ui,
    tab: &BrowserTab,
    visible: &[usize],
    menu: super::MenuContext,
    actions: &mut Vec<Action>,
) {
    let scroll_to = match (tab.scroll_to, tab.cursor) {
        (Some(row), _) => ScrollTo::Always(row),
        (None, Some(row)) => ScrollTo::Once(row),
        (None, None) => ScrollTo::None,
    };
    entry_list(
        ui,
        ("column", tab.id, &tab.path),
        visible.len(),
        scroll_to,
        |ui, i| {
            let entry = &tab.entries()[visible[i]];
            let selected = tab.selection.contains(&entry.path);
            let response = entry_row(ui, entry, selected);
            super::item_interaction(&response, i, entry, tab, menu, actions);
        },
    );
}

fn preview_folder(
    ui: &mut Ui,
    tab: &BrowserTab,
    folder: &Entry,
    show_hidden: bool,
    actions: &mut Vec<Action>,
) {
    let entries = listing(tab, &folder.path, show_hidden, None);
    entry_list(
        ui,
        ("column", tab.id, &folder.path),
        entries.len(),
        ScrollTo::None,
        |ui, i| {
            let entry = entries[i];
            let response = entry_row(ui, entry, false);
            other_folder_interaction(&response, &folder.path, entry, actions);
        },
    );
}

fn preview_file(
    ui: &mut Ui,
    tab: &BrowserTab,
    entry: &Entry,
    shared: &mut Shared,
    actions: &mut Vec<Action>,
) {
    ScrollArea::vertical()
        .id_salt(("column-preview", tab.id))
        .auto_shrink(false)
        .show(ui, |ui| {
            preview::show(ui, entry, shared, images::THUMBNAIL_SIZE, actions, true);
        });
}
