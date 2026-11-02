//! The settings tab: every preference in one place, grouped into sections.
//! The View menu keeps the quick toggles.

use egui::{Button, RichText, ScrollArea, Ui};
use egui_phosphor::regular as icon;
use serde::{Deserialize, Serialize};

use crate::{
    fs::{Sort, SortKey},
    properties::OpenIn,
    sidebar::Sidebar,
    tab::Shared,
    terminal_panel::TerminalPanel,
    theme::Themes,
    video::Quality,
    views::ViewMode,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Section {
    #[default]
    General,
    Appearance,
    Places,
    Files,
}

impl Section {
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
    pub settings: Section,
}

impl SettingsTab {
    pub fn new(id: u64) -> Self {
        Self {
            id,
            settings: Section::default(),
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
    let nav = |ui: &mut Ui, section: &mut Section| {
        for s in Section::ALL {
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
                Section::General => general(ui, app),
                Section::Appearance => appearance(ui, app),
                Section::Places => places(ui, app),
                Section::Files => files(ui, app),
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
    group(ui, "Drives");
    ui.checkbox(&mut app.sidebar.hide_empty_drives, "Hide empty drives")
        .on_hover_text("Card readers without a card and drives with no disc");
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
