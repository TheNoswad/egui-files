//! The settings tab: every preference in one place, grouped into sections.
//! The View menu keeps the quick toggles.

use egui::{Button, RichText, ScrollArea, Ui};
use egui_phosphor::regular as icon;
use serde::{Deserialize, Serialize};

use crate::{
    fs::{Sort, SortKey},
    properties::OpenIn,
    sidebar::{Layout, Section, Sidebar},
    tab::Shared,
    terminal_panel::TerminalPanel,
    theme::Themes,
    video::Quality,
    views::ViewMode,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Page {
    #[default]
    General,
    Appearance,
    Places,
    Files,
}

impl Page {
    pub const ALL: [Self; 4] = [Self::General, Self::Appearance, Self::Places, Self::Files];

    fn label(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Appearance => "Appearance",
            Self::Places => "Places",
            Self::Files => "Files and apps",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Self::General => icon::SLIDERS_HORIZONTAL,
            Self::Appearance => icon::PAINT_BRUSH,
            Self::Places => icon::COMPASS,
            Self::Files => icon::APP_WINDOW,
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct SettingsTab {
    pub id: u64,
    /// The section shown; also marks saved state as a settings tab.
    pub settings: Page,
}

impl SettingsTab {
    pub fn new(id: u64) -> Self {
        Self {
            id,
            settings: Page::default(),
        }
    }

    pub fn title(&self) -> String {
        format!("{} Settings", icon::GEAR)
    }
}

/// How new browser tabs start out.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NewTabs {
    pub sort: Sort,
    pub view: ViewMode,
}

/// The parts of the app the settings change.
pub struct Access<'a> {
    pub shared: &'a mut Shared,
    pub sidebar: &'a mut Sidebar,
    pub terminal: &'a mut TerminalPanel,
    pub themes: &'a mut Themes,
    pub new_tabs: &'a mut NewTabs,
}

pub fn ui(ui: &mut Ui, tab: &mut SettingsTab, app: &mut Access) {
    // Side by side when there's room, the sections above otherwise.
    let wide = ui.available_width() > 520.0;
    let nav = |ui: &mut Ui, section: &mut Page| {
        for s in Page::ALL {
            let text = format!("{}  {}", s.icon(), s.label());
            if ui.add(Button::selectable(*section == s, text)).clicked() {
                *section = s;
            }
        }
    };
    if wide {
        egui::Panel::left(egui::Id::new(("settings-nav", tab.id)))
            .resizable(false)
            .exact_size(170.0)
            .show(ui, |ui| {
                ui.add_space(6.0);
                ui.with_layout(egui::Layout::top_down_justified(egui::Align::Min), |ui| {
                    nav(ui, &mut tab.settings);
                });
            });
    } else {
        ui.horizontal_wrapped(|ui| nav(ui, &mut tab.settings));
        ui.separator();
    }
    ScrollArea::vertical()
        .auto_shrink(false)
        .id_salt(("settings", tab.id))
        .show(ui, |ui| {
            ui.add_space(6.0);
            ui.heading(tab.settings.label());
            ui.add_space(4.0);
            match tab.settings {
                Page::General => general(ui, app),
                Page::Appearance => appearance(ui, app),
                Page::Places => places(ui, app),
                Page::Files => files(ui, app),
            }
            ui.add_space(12.0);
        });
}

/// A group's title.
fn group(ui: &mut Ui, title: &str) {
    ui.add_space(10.0);
    ui.label(RichText::new(title).strong());
    ui.add_space(2.0);
}

fn general(ui: &mut Ui, app: &mut Access) {
    let shared = &mut *app.shared;
    group(ui, "Listing");
    ui.checkbox(&mut shared.show_hidden, "Show hidden files (Ctrl+H)");
    ui.checkbox(&mut shared.show_thumbnails, "Show thumbnails");
    ui.checkbox(&mut shared.usage.enabled, "Show folder sizes")
        .on_hover_text(
            "Measure the folders in the Details view, in the background; \
             not on network drives or FUSE mounts (a disk usage tab still can)",
        );
    crate::dates::checkbox(ui);
    ui.add(egui::Slider::new(&mut shared.icon_size, 32.0..=192.0).text("Icon size"));

    group(ui, "New tabs");
    new_tabs(ui, app.new_tabs);
}

fn new_tabs(ui: &mut Ui, new: &mut NewTabs) {
    egui::Grid::new("settings-new-tabs")
        .num_columns(2)
        .spacing([12.0, 6.0])
        .show(ui, |ui| {
            ui.label("View");
            ui.horizontal(|ui| {
                for (mode, glyph, label) in ViewMode::ALL {
                    ui.selectable_value(&mut new.view, mode, format!("{glyph} {label}"));
                }
            });
            ui.end_row();

            ui.label("Sort by");
            let sort = &mut new.sort;
            let current = SortKey::ALL
                .iter()
                .find(|(k, _)| *k == sort.key)
                .map_or("", |(_, l)| l);
            egui::ComboBox::from_id_salt("settings-sort-key")
                .selected_text(current)
                .show_ui(ui, |ui| {
                    for (key, label) in SortKey::ALL {
                        if ui.selectable_label(sort.key == key, label).clicked() {
                            *sort = Sort {
                                key,
                                ascending: key.starts_ascending(),
                                ..*sort
                            };
                        }
                    }
                });
            ui.end_row();

            ui.label("Order");
            ui.horizontal(|ui| {
                let [up, down] = sort.key.directions();
                ui.selectable_value(&mut sort.ascending, true, up);
                ui.selectable_value(&mut sort.ascending, false, down);
            });
            ui.end_row();

            ui.label("");
            ui.checkbox(&mut sort.folders_first, "Folders first");
            ui.end_row();
        });
}

fn appearance(ui: &mut Ui, app: &mut Access) {
    group(ui, "Theme");
    egui::widgets::global_theme_preference_buttons(ui);
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.menu_button(format!("{} Dark mode theme", icon::MOON), |ui| {
            app.themes.menu_ui(ui, egui::Theme::Dark);
        });
        ui.menu_button(format!("{} Light mode theme", icon::SUN), |ui| {
            app.themes.menu_ui(ui, egui::Theme::Light);
        });
        if ui
            .button(format!("{} Theme editor…", icon::PALETTE))
            .clicked()
        {
            app.themes.open_editor(ui.ctx());
        }
    });

    group(ui, "Editor");
    ui.menu_button(format!("{} Syntax colors", icon::PALETTE), |ui| {
        app.shared.editor.menu_ui(ui);
    });
}

fn places(ui: &mut Ui, app: &mut Access) {
    let sidebar = &mut *app.sidebar;
    group(ui, "Sections");
    ui.weak("Drag to reorder; untick to hide.");
    let layout = &mut sidebar.layout;
    let standard = &sidebar.places;
    let mut places_shown: Vec<(String, bool)> = standard
        .iter()
        .map(|p| (p.label.clone(), layout.place_shown(&p.label)))
        .collect();
    let mut sections = layout.sections.clone();
    reorderable(ui, "settings-places", &mut sections, |ui, (section, on)| {
        ui.vertical(|ui| {
            ui.checkbox(on, section.label());
            if *section == Section::Places {
                ui.indent("places", |ui| {
                    ui.add_enabled_ui(*on, |ui| {
                        for (label, shown) in &mut places_shown {
                            ui.checkbox(shown, label.as_str());
                        }
                    });
                });
            }
        });
    });
    layout.sections = sections;
    for (label, shown) in places_shown {
        layout.set_place_shown(&label, shown);
    }

    group(ui, "Drives");
    ui.checkbox(&mut sidebar.hide_empty_drives, "Hide empty drives")
        .on_hover_text("Card readers without a card and drives with no disc");
    ui.checkbox(
        &mut sidebar.layout.space_bars,
        "Show how full each drive is",
    );
    if ui.button("Reset places pane").clicked() {
        sidebar.layout = Layout::default();
    }
}

/// Rows dragged by their handle into a different order.
fn reorderable<T>(
    ui: &mut Ui,
    salt: &str,
    items: &mut Vec<T>,
    mut row: impl FnMut(&mut Ui, &mut T),
) {
    let list = egui::Id::new(salt);
    let mut moved = None;
    for (i, item) in items.iter_mut().enumerate() {
        let response = ui
            .horizontal(|ui| {
                ui.dnd_drag_source(list.with(i), DragRow { list, index: i }, |ui| {
                    ui.label(RichText::new(icon::DOTS_SIX_VERTICAL).weak());
                })
                .response
                .on_hover_cursor(egui::CursorIcon::Grab);
                row(ui, item);
            })
            .response;
        let ours = |row: Option<std::sync::Arc<DragRow>>| row.filter(|r| r.list == list);
        if let Some(pointer) = ui.ctx().pointer_interact_pos()
            && ours(response.dnd_hover_payload()).is_some()
        {
            // Dropped onto the top half of a row goes before it, the
            // bottom half after.
            let rect = response.rect;
            let below = pointer.y > rect.center().y;
            let y = if below { rect.bottom() } else { rect.top() };
            ui.painter()
                .hline(rect.x_range(), y, ui.visuals().selection.stroke);
            if let Some(from) = ours(response.dnd_release_payload()) {
                moved = Some((from.index, i + usize::from(below)));
            }
        }
    }
    if let Some((from, to)) = moved {
        move_item(items, from, to);
    }
}

/// What a row of a [`reorderable`] list carries while dragged.
struct DragRow {
    list: egui::Id,
    index: usize,
}

/// Moves `items[from]` to just before what was `items[to]` (or to the end
/// for `items.len()`).
fn move_item<T>(items: &mut Vec<T>, from: usize, to: usize) {
    if from >= items.len() || to > items.len() {
        return;
    }
    let item = items.remove(from);
    let to = if to > from { to - 1 } else { to };
    items.insert(to, item);
}

fn files(ui: &mut Ui, app: &mut Access) {
    let shared = &mut *app.shared;
    group(ui, "Open with");
    ui.checkbox(
        &mut shared.open_with.prefs.custom_commands,
        "Custom commands in “Open with”",
    )
    .on_hover_text("The “Other application…” dialog gets a box for typing any command");

    group(ui, "Properties");
    let prefs = &mut shared.properties.prefs;
    ui.horizontal(|ui| {
        ui.label("Open in");
        for open_in in OpenIn::ALL {
            if ui
                .selectable_label(prefs.open_in == open_in, open_in.label())
                .clicked()
            {
                prefs.open_in = open_in;
            }
        }
    });
    ui.checkbox(&mut prefs.apply_immediately, "Apply changes immediately")
        .on_hover_text("Without an Apply button: each change is made as soon as it's set");

    group(ui, "Video");
    ui.horizontal(|ui| {
        ui.label("Quality");
        for quality in Quality::ALL {
            let selected = shared.videos.quality() == quality;
            if ui.selectable_label(selected, quality.label()).clicked() {
                shared.videos.set_quality(quality);
            }
        }
    });

    group(ui, "Terminal");
    ui.menu_button(format!("{} Terminal folder", icon::LINK_SIMPLE), |ui| {
        app.terminal.follow_menu(ui);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moves_items() {
        let moved = |from, to| {
            let mut v = vec!['a', 'b', 'c', 'd'];
            move_item(&mut v, from, to);
            v.into_iter().collect::<String>()
        };
        assert_eq!(moved(0, 2), "bacd");
        assert_eq!(moved(0, 4), "bcda");
        assert_eq!(moved(3, 0), "dabc");
        assert_eq!(moved(2, 2), "abcd");
        assert_eq!(moved(2, 3), "abcd");
        assert_eq!(moved(1, 9), "abcd");
    }
}
