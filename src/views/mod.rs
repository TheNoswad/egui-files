//! Listing views. Each view renders the visible entries of a tab and reports
//! user intent as [`Action`]s; it never mutates the tab directly.

mod columns;
mod details;
mod grid;

use egui::{Modifiers, Response, Ui};
use egui_phosphor::regular as icon;
use serde::{Deserialize, Serialize};

use crate::{
    fs::Entry,
    tab::{Action, BrowserTab},
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ViewMode {
    #[default]
    Details,
    Grid,
    Columns,
}

impl ViewMode {
    pub const ALL: [(Self, &'static str, &'static str); 3] = [
        (Self::Details, icon::LIST, "Details"),
        (Self::Grid, icon::SQUARES_FOUR, "Icons"),
        (Self::Columns, icon::COLUMNS, "Columns"),
    ];
}

pub fn show(
    ui: &mut Ui,
    tab: &BrowserTab,
    visible: &[usize],
    show_hidden: bool,
    actions: &mut Vec<Action>,
) {
    match tab.view {
        ViewMode::Details => details::show(ui, tab, visible, actions),
        ViewMode::Grid => grid::show(ui, tab, visible, actions),
        ViewMode::Columns => columns::show(ui, tab, visible, show_hidden, actions),
    }
}

/// Click, double-click, middle-click and context-menu handling shared by all views.
pub(crate) fn item_interaction(
    response: &Response,
    index: usize,
    entry: &Entry,
    selected: bool,
    actions: &mut Vec<Action>,
) {
    if response.double_clicked() {
        actions.push(Action::Activate(entry.path.clone()));
    } else if response.clicked() {
        let modifiers = response.ctx.input(|i| i.modifiers);
        actions.push(Action::Select { index, modifiers });
    } else if response.middle_clicked() && entry.is_dir {
        actions.push(Action::OpenInNewTab(entry.path.clone()));
    }
    if response.secondary_clicked() && !selected {
        actions.push(Action::Select {
            index,
            modifiers: Modifiers::NONE,
        });
    }
    response.context_menu(|ui| entry_menu(ui, entry, actions));
}

fn entry_menu(ui: &mut Ui, entry: &Entry, actions: &mut Vec<Action>) {
    let mut item = |ui: &mut Ui, label: String, action: Action| {
        if ui.button(label).clicked() {
            actions.push(action);
            ui.close();
        }
    };
    item(
        ui,
        format!("{} Open", icon::ARROW_SQUARE_OUT),
        Action::Activate(entry.path.clone()),
    );
    if entry.is_dir {
        item(
            ui,
            format!("{} Open in new tab", icon::PLUS),
            Action::OpenInNewTab(entry.path.clone()),
        );
    }
    ui.separator();
    item(ui, format!("{} Cut", icon::SCISSORS), Action::Cut);
    item(ui, format!("{} Copy", icon::COPY), Action::Copy);
    item(
        ui,
        format!("{} Copy path", icon::CLIPBOARD),
        Action::CopyPath,
    );
    ui.separator();
    item(
        ui,
        format!("{} Rename", icon::PENCIL_SIMPLE),
        Action::StartRename,
    );
    item(ui, format!("{} Move to trash", icon::TRASH), Action::Trash);
}
