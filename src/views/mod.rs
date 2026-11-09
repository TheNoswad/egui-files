//! Listing views. Each view renders the visible entries of a tab and reports
//! user intent as [`Action`]s; it never mutates the tab directly.

mod columns;
mod details;
mod grid;
mod marquee;

pub(crate) use grid::ICON_SIZES;

/// How far the Details and Columns lists can scale from the theme's text size.
pub(crate) const LIST_SCALES: std::ops::RangeInclusive<f32> = 0.75..=2.0;

use egui::{Modifiers, Response, Ui};
use egui_phosphor::regular as icon;
use serde::{Deserialize, Serialize};

use crate::{
    archive,
    dnd::{self, DropTarget},
    fs::Entry,
    open_with,
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

pub use details::COLUMNS as DETAILS_COLUMNS;

pub fn show(
    ui: &mut Ui,
    tab: &BrowserTab,
    visible: &[usize],
    shared: &mut Shared,
    drag_select: bool,
    actions: &mut Vec<Action>,
) {
    // Ctrl and the wheel, or a pinch, over the items resizes them, as the
    // sliders in the menu do. Gentler for text, which a notch of the wheel
    // would otherwise change by more than a quarter.
    let area = ui.available_rect_before_wrap();
    if tab.view == ViewMode::Grid {
        crate::touch::zoom_gesture(ui, area, &mut shared.icon_size, ICON_SIZES, 1.0);
    } else {
        crate::touch::zoom_gesture(ui, area, &mut shared.list_scale, LIST_SCALES, 0.4);
    }
    match tab.view {
        ViewMode::Details => {
            scale_text(ui, shared.list_scale);
            let sizes = shared.usage.folder_sizes(ui.ctx(), &tab.path);
            let menu = MenuContext::new(shared);
            details::show(
                ui,
                tab,
                visible,
                menu,
                sizes.as_deref(),
                drag_select,
                actions,
            );
        }
        ViewMode::Grid => grid::show(ui, tab, visible, shared, drag_select, actions),
        ViewMode::Columns => columns::show(ui, tab, visible, shared, drag_select, actions),
    }
}

/// Scales the text in `ui`, and with it the rows sized to it. In half points,
/// so a wheel's or a pinch's many sizes don't each fill the font atlas.
fn scale_text(ui: &mut Ui, scale: f32) {
    for font in ui.style_mut().text_styles.values_mut() {
        font.size = (font.size * scale * 2.0).round() / 2.0;
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

/// Click, double-click, middle-click and context-menu handling shared by all
/// views. `drag_out` is whether dragging the item drags it (rather than
/// starting a drag selection).
pub(crate) fn item_interaction(
    response: &Response,
    index: usize,
    entry: &Entry,
    tab: &BrowserTab,
    menu: MenuContext,
    drag_out: bool,
    actions: &mut Vec<Action>,
) {
    let selected = tab.selection.contains(&entry.path);
    // Dragging a selected item drags the whole selection.
    dnd::source(response, || {
        if !drag_out {
            return Vec::new();
        }
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

    if crate::touch::is_touch(&response.ctx) {
        // A long press on a selected item, while selecting, picks the
        // selection up to drag onto a folder, rather than selecting more.
        if selected
            && tab.selecting
            && crate::touch::long_press_started(&response.ctx)
            && response.contains_pointer()
        {
            dnd::start(&response.ctx, tab.selected_paths());
        }
        // A tap opens a folder and previews a file (with buttons there to
        // open it), or while selecting, adds or removes the item.
        if response.clicked() {
            if tab.selecting {
                actions.push(Action::Select {
                    index,
                    modifiers: Modifiers::COMMAND,
                });
            } else if entry.is_dir {
                actions.push(Action::Activate(entry.path.clone()));
            } else {
                actions.push(Action::Select {
                    index,
                    modifiers: Modifiers::NONE,
                });
                actions.push(Action::ShowPreview);
            }
        }
    } else if response.double_clicked() {
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
    use crate::keymap::Command as C;
    let ctx = ui.ctx().clone();
    let keys = |command| crate::keymap::text(&ctx, command);
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
    if crate::editor::can_edit(&entry.path, entry.is_dir) {
        item(
            ui,
            format!("{} Edit", icon::NOTE_PENCIL),
            &keys(C::Edit),
            if tab.selection.contains(&entry.path) {
                Action::EditSelected
            } else {
                Action::Edit(entry.path.clone())
            },
        );
    }
    if !entry.is_dir && crate::clip::can_edit(&entry.path) {
        item(
            ui,
            format!("{} Edit video", icon::FILM_SLATE),
            "",
            Action::EditVideo(entry.path.clone()),
        );
    }
    let open_with = open_with::submenu(ui, || {
        if tab.selection.contains(&entry.path) {
            tab.selected_paths()
        } else {
            vec![entry.path.clone()]
        }
    });
    if entry.is_dir {
        item(
            ui,
            format!("{} Open in new tab", icon::PLUS),
            "",
            Action::OpenInNewTab(entry.path.clone()),
        );
        item(
            ui,
            format!("{} Disk usage", icon::CHART_PIE_SLICE),
            "",
            Action::DiskUsage(entry.path.clone()),
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
            &keys(C::Extract),
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
            &keys(C::ExtractTo),
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
        &keys(C::Rename),
        Action::StartRename,
    );
    let no_trash = crate::remote::is_remote(&entry.path) && !crate::remote::is_proton(&entry.path);
    let label = if no_trash {
        format!("{} Delete permanently…", icon::TRASH)
    } else {
        format!("{} Move to trash", icon::TRASH)
    };
    item(ui, label, &keys(C::Trash), Action::Trash);
    ui.separator();
    item(
        ui,
        format!("{} Properties", icon::INFO),
        &keys(C::Properties),
        Action::Properties(if tab.selection.contains(&entry.path) {
            tab.selected_paths()
        } else {
            vec![entry.path.clone()]
        }),
    );
    match open_with {
        Some(open_with::Choice::Open(target)) => actions.push(Action::OpenWith(Some(target))),
        Some(open_with::Choice::Other) => actions.push(Action::OpenWith(None)),
        None => {}
    }
}
