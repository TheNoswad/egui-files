//! The places and folder tree panes.

use std::path::{Path, PathBuf};

use egui::{Button, RichText, ScrollArea, Ui};
use egui_phosphor::regular as icon;

use crate::{
    devices::Drives,
    dnd::{self, DropTarget},
    jobs::Jobs,
    places::{self, Place},
    trash::TrashCount,
    tree::{FolderTree, TreeAction},
};

/// What the user asked for in a sidebar pane.
#[derive(Default)]
pub struct SidebarResponse {
    /// A folder to show, and whether in a new tab.
    pub target: Option<(PathBuf, bool)>,
    pub open_trash: bool,
    /// A drive to eject, and its label.
    pub eject: Option<(PathBuf, String)>,
    /// A folder to show the disk usage of.
    pub analyze: Option<PathBuf>,
    /// A place or folder to show the properties of.
    pub properties: Option<PathBuf>,
}

pub struct Sidebar {
    pub bookmarks: Vec<PathBuf>,
    pub tree: FolderTree,
    pub places: Vec<Place>,
    pub drives: Drives,
    /// Like Explorer's option: a card reader without a card (or an ejected
    /// one) isn't listed.
    pub hide_empty_drives: bool,
}

impl Sidebar {
    pub fn new(
        ctx: &egui::Context,
        bookmarks: Vec<PathBuf>,
        tree: FolderTree,
        hide_empty_drives: bool,
    ) -> Self {
        Self {
            bookmarks,
            tree,
            places: places::standard_places(),
            drives: Drives::new(ctx),
            hide_empty_drives,
        }
    }

    pub fn tree_ui(
        &mut self,
        ui: &mut Ui,
        current: Option<&Path>,
        show_hidden: bool,
        jobs: &mut Jobs,
        response: &mut SidebarResponse,
    ) {
        let drives = self.drives.shown(self.hide_empty_drives);
        let roots: Vec<&Place> = self.places.iter().take(1).chain(&drives).collect();
        match self.tree.ui(ui, &roots, current, show_hidden) {
            Some(TreeAction::Navigate(path)) => response.target = Some((path, false)),
            Some(TreeAction::OpenInNewTab(path)) => response.target = Some((path, true)),
            Some(TreeAction::Drop(job)) => jobs.start(*job),
            Some(TreeAction::Properties(path)) => response.properties = Some(path),
            None => {}
        }
    }

    pub fn places_ui(
        &mut self,
        ui: &mut Ui,
        current: Option<&PathBuf>,
        trash_count: &TrashCount,
        trash_focused: bool,
        jobs: &mut Jobs,
        response: &mut SidebarResponse,
    ) {
        let mut remove_bookmark = None;
        let mut dropped = None;
        let mut trash_dropped = None;
        let target = &mut response.target;
        let analyze = &mut response.analyze;
        let properties = &mut response.properties;

        // Places open in a new tab on a middle click, from the right-click
        // menu, or dragged onto a tab bar; dragged near the edge of a tab's
        // content they open in a new pane. `more` adds to that menu.
        let mut place_button = |ui: &mut Ui,
                                glyph: &str,
                                label: &str,
                                path: &PathBuf,
                                more: &mut dyn FnMut(&mut Ui)| {
            let selected = current == Some(path);
            let response = ui
                .add(Button::selectable(selected, format!("{glyph}  {label}")))
                .interact(egui::Sense::click_and_drag())
                .on_hover_text(path.display().to_string());
            if response.clicked() {
                *target = Some((path.clone(), false));
            } else if response.middle_clicked() {
                *target = Some((path.clone(), true));
            }
            dnd::place_source(&response, path);
            if let Some(job) = dnd::target(&response, DropTarget::Folder(path.clone())) {
                dropped = Some(job);
            }
            response.context_menu(|ui| {
                if ui
                    .button(format!("{}  Open in new tab", icon::PLUS))
                    .clicked()
                {
                    *target = Some((path.clone(), true));
                    ui.close();
                }
                if ui
                    .button(format!("{}  Disk usage", icon::CHART_PIE_SLICE))
                    .clicked()
                {
                    *analyze = Some(path.clone());
                    ui.close();
                }
                more(ui);
                ui.separator();
                if ui.button(format!("{}  Properties", icon::INFO)).clicked() {
                    *properties = Some(path.clone());
                    ui.close();
                }
            });
            response
        };

        ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            ui.with_layout(egui::Layout::top_down_justified(egui::Align::Min), |ui| {
                ui.add_space(4.0);
                for place in &self.places {
                    place_button(ui, place.icon, &place.label, &place.path, &mut |_| {});
                }
                let count = trash_count
                    .get()
                    .filter(|&n| n > 0)
                    .map(|n| n.to_string())
                    .unwrap_or_default();
                let trash = ui.add(
                    Button::selectable(trash_focused, format!("{}  Trash", icon::TRASH))
                        .right_text(RichText::new(count).weak()),
                );
                response.open_trash = trash.clicked();
                trash_dropped = dnd::target(&trash, DropTarget::Trash);

                ui.add_space(8.0);
                ui.label(RichText::new("Bookmarks").small().weak());
                for (i, path) in self.bookmarks.iter().enumerate() {
                    let label = crate::fs::display_name(path);
                    place_button(ui, icon::BOOKMARK_SIMPLE, &label, path, &mut |ui| {
                        if ui.button(format!("{}  Remove bookmark", icon::X)).clicked() {
                            remove_bookmark = Some(i);
                            ui.close();
                        }
                    });
                }
                if let Some(path) = current
                    && !self.bookmarks.contains(path)
                    && ui
                        .button(format!("{}  Bookmark this folder", icon::PLUS))
                        .clicked()
                {
                    self.bookmarks.push(path.clone());
                }

                ui.add_space(8.0);
                ui.label(RichText::new("Drives").small().weak());
                let mut eject = None;
                for drive in self.drives.shown(self.hide_empty_drives) {
                    let mut menu = |ui: &mut Ui| {
                        if drive.ejectable
                            && ui
                                .button(format!("{}  Eject", icon::EJECT_SIMPLE))
                                .clicked()
                        {
                            eject = Some((drive.path.clone(), drive.label.clone()));
                            ui.close();
                        }
                    };
                    let row = place_button(ui, drive.icon, &drive.label, &drive.path, &mut menu);
                    if let Some(space) = drive.space {
                        space_bar(ui, space);
                    }
                    if drive.ejectable {
                        let size = row.rect.height();
                        let rect = egui::Rect::from_min_size(
                            egui::pos2(row.rect.right() - size, row.rect.top()),
                            egui::vec2(size, size),
                        );
                        if ui
                            .put(rect, Button::new(icon::EJECT_SIMPLE).frame(false))
                            .on_hover_text("Eject")
                            .clicked()
                        {
                            eject = Some((drive.path.clone(), drive.label.clone()));
                        }
                    }
                }
                response.eject = eject;
            });
        });

        if let Some(i) = remove_bookmark {
            self.bookmarks.remove(i);
        }
        if let Some(job) = dropped.or(trash_dropped) {
            jobs.start(job);
        }
    }
}

/// A thin bar under a drive showing how full it is.
fn space_bar(ui: &mut Ui, space: places::Space) {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 4.0), egui::Sense::hover());
    let bar = rect
        .shrink2(egui::vec2(8.0, 0.0))
        .translate(egui::vec2(0.0, -2.0));
    let visuals = ui.visuals();
    let used = space.used_share();
    let fill = if used > 0.9 {
        visuals.warn_fg_color
    } else {
        visuals.selection.bg_fill
    };
    let p = ui.painter();
    p.rect_filled(bar, 2.0, visuals.extreme_bg_color);
    let filled = egui::Rect::from_min_size(bar.min, egui::vec2(bar.width() * used, bar.height()));
    p.rect_filled(filled, 2.0, fill);
    response.on_hover_text(format!(
        "{} free of {}",
        crate::fs::human_size(space.available),
        crate::fs::human_size(space.total)
    ));
}
