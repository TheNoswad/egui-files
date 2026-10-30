//! Listing views. Each view renders the visible entries of a tab and reports
//! user intent as [`Action`]s; it never mutates the tab directly.

mod columns;
mod details;
mod grid;

use egui::{Modifiers, Response, Ui};
use egui_phosphor::regular as icon;
use serde::{Deserialize, Serialize};

use crate::{
    archive,
    dnd::{self, DropTarget},
    fs::Entry,
    tab::{Action, BrowserTab, Shared},
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
    shared: &mut Shared,
    actions: &mut Vec<Action>,
) {
    match tab.view {
        ViewMode::Details => details::show(ui, tab, visible, MenuContext::new(shared), actions),
        ViewMode::Grid => grid::show(ui, tab, visible, shared, actions),
        ViewMode::Columns => columns::show(ui, tab, visible, shared, actions),
    }
}

/// What the item context menu needs to know beyond the item.
#[derive(Clone, Copy)]
pub(crate) struct MenuContext {
    can_paste: bool,
}

impl MenuContext {
    pub(crate) fn new(shared: &Shared) -> Self {
        Self {
            can_paste: shared.clipboard.is_some(),
        }
    }
}

/// Click, double-click, middle-click and context-menu handling shared by all views.
pub(crate) fn item_interaction(
    response: &Response,
    index: usize,
    entry: &Entry,
    tab: &BrowserTab,
    menu: MenuContext,
    actions: &mut Vec<Action>,
) {
    let selected = tab.selection.contains(&entry.path);
    // Dragging a selected item drags the whole selection.
    dnd::source(response, || {
        if selected {
            tab.selected_paths()
        } else {
            vec![entry.path.clone()]
        }
    });
    if entry.is_dir
        && let Some(job) = dnd::target(response, DropTarget::Folder(entry.path.clone()))
    {
        actions.push(Action::StartJob(job));
    }

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
    response.context_menu(|ui| entry_menu(ui, entry, tab, menu, actions));
}

fn entry_menu(
    ui: &mut Ui,
    entry: &Entry,
    tab: &BrowserTab,
    menu: MenuContext,
    actions: &mut Vec<Action>,
) {
    let mut item = |ui: &mut Ui, label: String, shortcut: &str, action: Action| {
        if ui
            .add(egui::Button::new(label).shortcut_text(shortcut))
            .clicked()
        {
            actions.push(action);
            ui.close();
        }
    };
    let is_archive = !entry.is_dir && archive::is_archive(&entry.path);
    item(
        ui,
        format!("{} Open", icon::ARROW_SQUARE_OUT),
        "",
        Action::Activate(entry.path.clone()),
    );
    if is_archive {
        item(
            ui,
            format!("{} Open with default app", icon::APP_WINDOW),
            "",
            Action::OpenExternally(entry.path.clone()),
        );
    }
    if entry.is_dir {
        item(
            ui,
            format!("{} Open in new tab", icon::PLUS),
            "",
            Action::OpenInNewTab(entry.path.clone()),
        );
    }
    ui.separator();
    item(ui, format!("{} Cut", icon::SCISSORS), "", Action::Cut);
    item(ui, format!("{} Copy", icon::COPY), "", Action::Copy);
    if entry.is_dir && menu.can_paste {
        item(
            ui,
            format!("{} Paste into folder", icon::CLIPBOARD),
            "",
            Action::PasteInto(entry.path.clone()),
        );
    }
    item(
        ui,
        format!("{} Copy path", icon::CLIPBOARD),
        "",
        Action::CopyPath,
    );
    ui.separator();
    if is_archive {
        let archives: Vec<_> = tab
            .selected_paths()
            .into_iter()
            .filter(|p| archive::is_archive(p))
            .collect();
        item(
            ui,
            format!("{} Extract here", icon::ARCHIVE),
            "Ctrl+E",
            Action::Extract(archive::ExtractMode::Smart),
        );
        item(
            ui,
            format!(
                "{} {}",
                icon::FOLDER_SIMPLE,
                archive::subfolder_label(&archives)
            ),
            "",
            Action::Extract(archive::ExtractMode::Subfolder),
        );
        item(
            ui,
            format!("{} Extract to…", icon::FOLDER_OPEN),
            "Ctrl+Shift+E",
            Action::ExtractTo,
        );
    }
    item(
        ui,
        format!("{} Compress…", icon::FILE_ZIP),
        "Ctrl+Shift+C",
        Action::Compress,
    );
    ui.separator();
    item(
        ui,
        format!("{} Rename", icon::PENCIL_SIMPLE),
        "",
        Action::StartRename,
    );
    item(
        ui,
        format!("{} Move to trash", icon::TRASH),
        "",
        Action::Trash,
    );
}
