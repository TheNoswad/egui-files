//! Sortable table with name, size, modified, created and kind columns.

use egui::{Align, Button, Layout, RichText, Sense, TextStyle, Ui};
use egui_extras::TableBuilder;
use egui_phosphor::regular as icon;

use crate::{
    fs::{self, SortKey},
    list_columns::{self, Spec},
    tab::{Action, BrowserTab},
    usage::FolderSizes,
};

const SIZE: usize = 1;
const MODIFIED: usize = 2;
const CREATED: usize = 3;
const KIND: usize = 4;

static COLUMNS: list_columns::Table = list_columns::Table {
    key: "details",
    columns: &[
        Spec {
            label: "Name",
            width: 0.0,
            min: 160.0,
        },
        Spec {
            label: "Size",
            width: 90.0,
            min: 60.0,
        },
        Spec {
            label: "Modified",
            width: 140.0,
            min: 80.0,
        },
        Spec {
            label: "Created",
            width: 140.0,
            min: 80.0,
        },
        Spec {
            label: "Kind",
            width: 100.0,
            min: 60.0,
        },
    ],
    hide_order: &[KIND, CREATED, MODIFIED, SIZE],
    hidden: &[CREATED],
};

pub fn show(
    ui: &mut Ui,
    tab: &BrowserTab,
    visible: &[usize],
    menu: super::MenuContext,
    folder_sizes: Option<&FolderSizes>,
    actions: &mut Vec<Action>,
) {
    let row_height = ui.text_style_height(&TextStyle::Body) + 6.0;
    let colors = crate::theme::colors(ui.ctx());
    colors.apply_to_list(ui.visuals_mut());
    let columns = list_columns::Layout::new(ui, &COLUMNS);
    let mut table = TableBuilder::new(ui)
        .id_salt(("details", tab.id))
        .striped(true)
        .sense(Sense::click_and_drag())
        .cell_layout(Layout::left_to_right(Align::Center));
    table = columns.columns(table);
    if let Some(row) = tab.scroll_to {
        table = table.scroll_to_row(row, None);
    }

    let output = table
        .header(row_height, |mut header| {
            for (column, key) in [
                SortKey::Name,
                SortKey::Size,
                SortKey::Modified,
                SortKey::Created,
                SortKey::Kind,
            ]
            .into_iter()
            .enumerate()
            {
                if !columns.shown(column) {
                    continue;
                }
                let mut button = None;
                let (_, cell) = header.col(|ui| {
                    let arrow = match (tab.sort.key == key, tab.sort.ascending) {
                        (false, _) => "",
                        (true, true) => icon::CARET_UP,
                        (true, false) => icon::CARET_DOWN,
                    };
                    let label = COLUMNS.columns[column].label;
                    let text = RichText::new(format!("{label} {arrow}")).strong();
                    button = Some(ui.add(Button::new(text).frame(false)));
                });
                let response = button.map_or(cell.clone(), |button| cell.union(button));
                if response.clicked() {
                    actions.push(Action::SortBy(key));
                }
                columns.context_menu(&response);
            }
        })
        .body(|body| {
            body.rows(row_height, visible.len(), |mut row| {
                let index = row.index();
                let entry = &tab.entries()[visible[index]];
                let selected = tab.selection.contains(&entry.path);
                row.set_selected(selected);
                row.col(|ui| {
                    let mut name = RichText::new(&entry.name);
                    let mut icon = RichText::new(entry.icon());
                    if !selected {
                        if let Some(color) = entry.name_color(&colors) {
                            name = name.color(color);
                        }
                        if let Some(color) = entry.icon_color(&colors) {
                            icon = icon.color(color);
                        }
                    }
                    ui.spacing_mut().item_spacing.x = 4.0;
                    ui.label(icon);
                    ui.label(name);
                });
                if columns.shown(SIZE) {
                    row.col(|ui| {
                        if !entry.is_dir {
                            ui.label(fs::human_size(entry.size));
                        } else if let Some(sizes) = folder_sizes
                            && let Some(size) = sizes.get(&entry.path)
                        {
                            // From a disk usage tab; still growing while it scans.
                            let text = RichText::new(fs::human_size(size)).weak();
                            let text = if sizes.partial { text.italics() } else { text };
                            ui.label(text);
                        }
                    });
                }
                if columns.shown(MODIFIED) {
                    row.col(|ui| {
                        crate::dates::label(ui, entry.modified, false);
                    });
                }
                if columns.shown(CREATED) {
                    row.col(|ui| {
                        crate::dates::label(ui, entry.created, false);
                    });
                }
                if columns.shown(KIND) {
                    row.col(|ui| {
                        ui.label(entry.kind());
                    });
                }
                super::item_interaction(&row.response(), index, entry, tab, menu, actions);
            });
        });
    columns.resize_handles(ui, output.inner_rect.bottom());
}
