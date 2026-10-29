//! Sortable table with name, size, modified and kind columns.

use egui::{Align, Button, Layout, RichText, Sense, TextStyle, Ui};
use egui_extras::{Column, TableBuilder};
use egui_phosphor::regular as icon;

use crate::{
    fs::{self, SortKey},
    tab::{Action, BrowserTab},
};

pub fn show(
    ui: &mut Ui,
    tab: &BrowserTab,
    visible: &[usize],
    can_paste: bool,
    actions: &mut Vec<Action>,
) {
    let row_height = ui.text_style_height(&TextStyle::Body) + 6.0;
    let mut table = TableBuilder::new(ui)
        .id_salt(("details", tab.id))
        .striped(true)
        .resizable(true)
        .sense(Sense::click_and_drag())
        .cell_layout(Layout::left_to_right(Align::Center))
        .column(Column::remainder().at_least(160.0).clip(true))
        .column(Column::initial(90.0).at_least(60.0))
        .column(Column::initial(140.0).at_least(80.0))
        .column(Column::initial(100.0).at_least(60.0).clip(true));
    if let Some(row) = tab.scroll_to {
        table = table.scroll_to_row(row, None);
    }

    table
        .header(row_height, |mut header| {
            for (key, label) in [
                (SortKey::Name, "Name"),
                (SortKey::Size, "Size"),
                (SortKey::Modified, "Modified"),
                (SortKey::Kind, "Kind"),
            ] {
                header.col(|ui| {
                    let arrow = match (tab.sort.key == key, tab.sort.ascending) {
                        (false, _) => "",
                        (true, true) => icon::CARET_UP,
                        (true, false) => icon::CARET_DOWN,
                    };
                    let text = RichText::new(format!("{label} {arrow}")).strong();
                    if ui.add(Button::new(text).frame(false)).clicked() {
                        actions.push(Action::SortBy(key));
                    }
                });
            }
        })
        .body(|body| {
            body.rows(row_height, visible.len(), |mut row| {
                let index = row.index();
                let entry = &tab.entries()[visible[index]];
                let selected = tab.selection.contains(&entry.path);
                row.set_selected(selected);
                row.col(|ui| {
                    let name = RichText::new(format!("{} {}", entry.icon(), entry.name));
                    ui.label(if entry.hidden { name.weak() } else { name });
                });
                row.col(|ui| {
                    if !entry.is_dir {
                        ui.label(fs::human_size(entry.size));
                    }
                });
                row.col(|ui| {
                    ui.label(fs::format_time(entry.modified));
                });
                row.col(|ui| {
                    ui.label(entry.kind());
                });
                super::item_interaction(&row.response(), index, entry, tab, can_paste, actions);
            });
        });
}
