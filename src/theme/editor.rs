//! The theme editor window: pick, duplicate, edit, import and export themes,
//! and the appearance settings that apply over every theme.

// Android has no trash of its own; see android/trash.
#[cfg(any(target_os = "android", target_arch = "wasm32"))]
use trash_android as trash;

use std::path::PathBuf;

use egui::{
    Color32, ComboBox, Context, DragValue, Panel, RichText, ScrollArea, TextEdit, Theme, Ui,
    color_picker::{Alpha, color_edit_button_srgba},
};
use egui_phosphor::regular as icon;

use super::{
    Density, Resolved, Source, Themes,
    file::{Color, Fonts, Hex, ThemeFile},
};

/// Seconds after the last edit before a theme is written to its file.
const SAVE_DELAY: f64 = 0.6;

#[derive(Default)]
pub struct ThemeEditor {
    pub open: bool,
    /// The theme being looked at.
    selected: Option<String>,
    draft: Option<Draft>,
    /// Bumped on every edit, so the app re-applies the themes.
    generation: u64,
    page: Page,
    /// What went wrong last, or what was done.
    message: Option<(String, bool)>,
    import: String,
    font_filter: String,
    /// Showing the selected theme in place of the list, when they don't
    /// fit side by side (on a phone).
    details_open: bool,
}

/// A theme file being edited, shown live before it's saved.
struct Draft {
    path: PathBuf,
    file: ThemeFile,
    /// When the last unsaved edit happened.
    edited_at: Option<f64>,
}

#[derive(Default, PartialEq)]
enum Page {
    #[default]
    Theme,
    Appearance,
}

impl ThemeEditor {
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The unsaved version of the theme `name`, while it's being edited.
    pub fn edited(&self, name: &str) -> Option<&ThemeFile> {
        let draft = self.draft.as_ref()?;
        (draft.file.name.as_deref() == Some(name)).then_some(&draft.file)
    }

    fn changed(&mut self, now: f64) {
        self.generation += 1;
        if let Some(draft) = &mut self.draft {
            draft.edited_at = Some(now);
        }
    }
}

impl Themes {
    /// Opens the editor on the theme showing now.
    pub fn open_editor(&mut self, ctx: &Context) {
        self.editor.open = true;
        if self.editor.selected.is_none() {
            self.select(self.prefs.name(ctx.theme()).to_owned());
        }
    }

    fn select(&mut self, name: String) {
        self.flush();
        let draft = self.get(&name).and_then(|entry| match &entry.source {
            Source::File(path) if entry.error.is_none() => Some(Draft {
                path: path.clone(),
                file: ThemeFile {
                    name: Some(entry.name.clone()),
                    ..entry.file.clone()
                },
                edited_at: None,
            }),
            _ => None,
        });
        self.editor.draft = draft;
        self.editor.selected = Some(name);
        self.editor.generation += 1;
    }

    /// Saves the draft if it has unsaved edits.
    fn flush(&mut self) {
        let Some(draft) = &mut self.editor.draft else {
            return;
        };
        if draft.edited_at.take().is_none() {
            return;
        }
        let (path, file) = (draft.path.clone(), draft.file.clone());
        if let Err(err) = self.save(&path, &file) {
            self.editor.message = Some((format!("Couldn't save {}: {err}", path.display()), true));
        }
    }

    pub fn editor_ui(&mut self, ctx: &Context) {
        let now = ctx.input(|i| i.time);
        if let Some(edited_at) = self.editor.draft.as_ref().and_then(|d| d.edited_at) {
            if now - edited_at >= SAVE_DELAY {
                self.flush();
            } else {
                ctx.request_repaint_after_secs((SAVE_DELAY - (now - edited_at)) as f32);
            }
        }
        // A file changed on disk while not being edited here: show the new one.
        if let Some(draft) = &mut self.editor.draft
            && draft.edited_at.is_none()
            && let Some(entry) = self
                .entries
                .iter()
                .find(|e| e.source == Source::File(draft.path.clone()))
            && entry.error.is_none()
            && entry.file.name.is_some()
            && entry.file != draft.file
        {
            draft.file = entry.file.clone();
            self.editor.generation += 1;
        }

        let mut open = self.editor.open;
        egui::Window::new(format!("{} Themes", icon::PALETTE))
            .open(&mut open)
            .default_size([760.0, 560.0])
            .resizable(true)
            .collapsible(false)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.editor.page, Page::Theme, "Themes");
                    ui.selectable_value(&mut self.editor.page, Page::Appearance, "Appearance");
                });
                ui.separator();
                if let Some((message, error)) = &self.editor.message {
                    let text = RichText::new(message);
                    let mut close = false;
                    ui.horizontal(|ui| {
                        ui.label(if *error {
                            text.color(ui.visuals().error_fg_color)
                        } else {
                            text
                        });
                        close = ui.small_button(icon::X).clicked();
                    });
                    if close {
                        self.editor.message = None;
                    }
                }
                match self.editor.page {
                    Page::Theme => self.themes_page(ui, now),
                    Page::Appearance => self.appearance_page(ui),
                }
            });
        if !open {
            self.flush();
        }
        self.editor.open = open;
    }

    fn themes_page(&mut self, ui: &mut Ui, now: f64) {
        // Side by side when there's room, else one at a time.
        if ui.available_width() < 500.0 {
            if !self.editor.details_open || self.editor.selected.is_none() {
                // Picking a theme, even the one already picked, shows it.
                self.editor.details_open = self.theme_list(ui);
                return;
            }
            if ui.button(format!("{} Themes", icon::ARROW_LEFT)).clicked() {
                self.editor.details_open = false;
                return;
            }
            ui.separator();
        } else {
            Panel::left("theme-list")
                .resizable(true)
                .default_size(200.0)
                .min_size(150.0)
                .show(ui, |ui| {
                    self.theme_list(ui);
                });
        }
        let Some(name) = self.editor.selected.clone() else {
            ui.weak("Pick a theme on the left.");
            return;
        };
        let resolved = self.resolve(&name, self.editor.edited(&name));
        ScrollArea::vertical()
            .id_salt("theme-fields")
            .auto_shrink(false)
            .show(ui, |ui| self.theme_details(ui, &name, &resolved, now));
    }

    /// The themes to pick from. Returns whether one was picked.
    fn theme_list(&mut self, ui: &mut Ui) -> bool {
        let mut select = None;
        let mut picked = false;
        ScrollArea::vertical()
            .id_salt("theme-list")
            .max_height(ui.available_height() - 90.0)
            .show(ui, |ui| {
                for (heading, presets) in [("Built in", true), ("Yours", false)] {
                    ui.weak(heading);
                    let mut any = false;
                    for entry in &self.entries {
                        if (entry.source == Source::Preset) != presets {
                            continue;
                        }
                        any = true;
                        let mut label = entry.name.clone();
                        if self.prefs.dark == entry.name {
                            label = format!("{label}  {}", icon::MOON);
                        }
                        if self.prefs.light == entry.name {
                            label = format!("{label}  {}", icon::SUN);
                        }
                        let mut text = RichText::new(label);
                        if entry.error.is_some() {
                            text = text.color(ui.visuals().error_fg_color);
                        }
                        let selected = self.editor.selected.as_deref() == Some(&entry.name);
                        let clicked = ui.selectable_label(selected, text).clicked();
                        picked |= clicked;
                        if clicked && !selected {
                            select = Some(entry.name.clone());
                        }
                    }
                    if !any {
                        ui.weak("None yet: duplicate a theme to make one.");
                    }
                    ui.add_space(6.0);
                }
            });
        if let Some(name) = select {
            self.select(name);
        }
        ui.separator();
        if ui
            .button(format!("{} Open themes folder", icon::FOLDER_OPEN))
            .clicked()
            && let Some(dir) = self.dir.clone()
        {
            let opened = std::fs::create_dir_all(&dir)
                .map_err(|e| e.to_string())
                .and_then(|()| crate::fs::open_detached(&dir).map_err(|e| e.to_string()));
            if let Err(err) = opened {
                self.editor.message =
                    Some((format!("Couldn't open {}: {err}", dir.display()), true));
            }
        }
        ui.collapsing("Import", |ui| {
            ui.add(
                TextEdit::multiline(&mut self.editor.import)
                    .hint_text("Paste a theme file here")
                    .desired_rows(4)
                    .code_editor(),
            );
            if ui.button("Import").clicked() {
                self.import();
            }
        });
        picked
    }

    fn import(&mut self) {
        let file = match toml::from_str::<ThemeFile>(&self.editor.import) {
            Ok(file) => file,
            Err(err) => {
                self.editor.message =
                    Some((format!("That isn't a theme: {}", err.message()), true));
                return;
            }
        };
        let name = self.unique_name(file.name.as_deref().unwrap_or("Imported theme"));
        self.create(ThemeFile {
            name: Some(name),
            ..file
        });
        self.editor.import.clear();
    }

    /// Saves a new theme file and selects it.
    fn create(&mut self, file: ThemeFile) {
        let name = file.name.clone().unwrap_or_default();
        let Some(path) = self.new_path(&name) else {
            self.editor.message = Some(("There's no folder to keep themes in.".into(), true));
            return;
        };
        match self.save(&path, &file) {
            Ok(()) => {
                self.select(name.clone());
                self.editor.message =
                    Some((format!("Saved “{name}” to {}", path.display()), false));
            }
            Err(err) => {
                self.editor.message =
                    Some((format!("Couldn't save {}: {err}", path.display()), true));
            }
        }
    }

    fn theme_details(&mut self, ui: &mut Ui, name: &str, resolved: &Resolved, now: f64) {
        let Some(entry) = self.get(name) else {
            ui.colored_label(
                ui.visuals().error_fg_color,
                format!("There's no theme named “{name}”."),
            );
            return;
        };
        let source = entry.source.clone();
        let error = entry.error.clone();
        let written = entry.file.clone();
        let dark = resolved.style.visuals.dark_mode;

        ui.heading(name);
        let mut deleted = None;
        ui.horizontal_wrapped(|ui| {
            for (theme, label) in [
                (Theme::Dark, "Use in dark mode"),
                (Theme::Light, "Use in light mode"),
            ] {
                let used = self.prefs.name(theme) == name;
                if ui.selectable_label(used, label).clicked() {
                    *self.prefs.name_mut(theme) = name.to_owned();
                }
            }
            if ui
                .button(format!("{} Duplicate", icon::COPY))
                .on_hover_text("Make a theme of your own that starts from this one")
                .clicked()
            {
                let copy = self.unique_name(&format!("{name} (custom)"));
                self.create(ThemeFile {
                    name: Some(copy),
                    base: Some(name.to_owned()),
                    dark: None,
                    ..ThemeFile::default()
                });
            }
            ui.menu_button(format!("{} Export", icon::EXPORT), |ui| {
                if ui
                    .button("Copy theme file")
                    .on_hover_text("Just what this theme changes, with its base named")
                    .clicked()
                {
                    let file = self.editor.edited(name).unwrap_or(&written);
                    ui.ctx()
                        .copy_text(toml::to_string_pretty(file).unwrap_or_default());
                    ui.close();
                }
                if ui
                    .button("Copy complete theme")
                    .on_hover_text(
                        "Everything, including what comes from its base, without needing it",
                    )
                    .clicked()
                {
                    let mut file = resolved.file.clone();
                    file.base = None;
                    ui.ctx()
                        .copy_text(toml::to_string_pretty(&file).unwrap_or_default());
                    ui.close();
                }
            });
            if let Source::File(path) = &source {
                if ui
                    .button(format!("{} Open file", icon::FILE_TEXT))
                    .clicked()
                    && let Err(err) = crate::fs::open_detached(path)
                {
                    self.editor.message = Some((err.to_string(), true));
                }
                if ui.button(format!("{} Delete", icon::TRASH)).clicked() {
                    deleted = Some(path.clone());
                }
            }
        });
        if let Some(path) = deleted {
            self.delete(name, path);
            return;
        }
        if let Some(error) = &error {
            ui.colored_label(
                ui.visuals().error_fg_color,
                format!("This file can't be read, so it looks like its base for now:\n{error}"),
            );
        }
        for warning in &resolved.warnings {
            ui.colored_label(ui.visuals().warn_fg_color, warning);
        }
        let other_names: Vec<String> = self
            .entries
            .iter()
            .map(|e| e.name.clone())
            .filter(|n| n != name)
            .collect();
        let families = self
            .fonts
            .families(ui.ctx())
            .map(|f| (f.all.clone(), f.mono.clone()));

        let editable = self.editor.draft.is_some();
        if !editable {
            ui.weak(if source == Source::Preset {
                "Built-in themes can't be changed. Duplicate one to make your own."
            } else {
                "Fix the file, or delete it."
            });
        }
        let mut readonly = ThemeFile {
            dark: Some(dark),
            ..resolved.file.clone()
        };
        let editor = &mut self.editor;
        let mut changed = false;
        let mut renamed = None;
        let file = match &mut editor.draft {
            Some(draft) => &mut draft.file,
            None => &mut readonly,
        };
        ui.add_enabled_ui(editable, |ui| {
            egui::Grid::new("theme-general")
                .num_columns(2)
                .show(ui, |ui| {
                    ui.label("Name");
                    let mut new_name = file.name.clone().unwrap_or_default();
                    if ui.text_edit_singleline(&mut new_name).changed()
                        && !new_name.trim().is_empty()
                    {
                        renamed = Some((file.name.clone().unwrap_or_default(), new_name.clone()));
                        file.name = Some(new_name);
                        changed = true;
                    }
                    ui.end_row();

                    ui.label("Based on");
                    ComboBox::from_id_salt("theme-base")
                        .selected_text(file.base.as_deref().unwrap_or("egui"))
                        .height(320.0)
                        .show_ui(ui, |ui| {
                            changed |= ui.selectable_value(&mut file.base, None, "egui").changed();
                            for other in &other_names {
                                changed |= ui
                                    .selectable_value(&mut file.base, Some(other.clone()), other)
                                    .changed();
                            }
                        });
                    ui.end_row();

                    ui.label("Dark");
                    let mut is_dark = file.dark.unwrap_or(dark);
                    ui.horizontal(|ui| {
                        if ui.checkbox(&mut is_dark, "").changed() {
                            file.dark = Some(is_dark);
                            changed = true;
                        }
                        changed |= reset_button(ui, &mut file.dark);
                    });
                    ui.end_row();

                    ui.label("Syntax colors");
                    ui.horizontal(|ui| {
                        ComboBox::from_id_salt("theme-syntax")
                            .selected_text(file.syntax.as_deref().unwrap_or(&resolved.app.syntax))
                            .height(320.0)
                            .show_ui(ui, |ui| {
                                for theme in crate::editor::highlight::theme_names() {
                                    changed |= ui
                                        .selectable_value(
                                            &mut file.syntax,
                                            Some(theme.to_owned()),
                                            theme,
                                        )
                                        .changed();
                                }
                            });
                        changed |= reset_button(ui, &mut file.syntax);
                    })
                    .response
                    .on_hover_text(
                        "Suggested for the editor and previews; the View menu can pick others",
                    );
                    ui.end_row();
                });

            let v = &resolved.style.visuals;
            let app = &resolved.app;
            ui.collapsing("Colors", |ui| {
                let shown = [
                    v.panel_fill,
                    v.window_fill,
                    v.extreme_bg_color,
                    v.window_stroke.color,
                    v.widgets.noninteractive.fg_stroke.color,
                    v.widgets.active.fg_stroke.color,
                    v.weak_text_color(),
                    v.widgets.inactive.bg_fill,
                    v.widgets.hovered.bg_fill,
                    v.widgets.active.bg_fill,
                    v.text_cursor.stroke.color,
                    v.selection.bg_fill,
                    v.selection.stroke.color,
                    v.hyperlink_color,
                    v.warn_fg_color,
                    v.error_fg_color,
                ];
                changed |= color_grid(ui, "colors", file.colors.fields_mut(), &shown);
            });
            ui.collapsing("File lists", |ui| {
                let f = &app.files;
                let text = v.text_color();
                let shown = [
                    f.selection,
                    f.selection_text,
                    f.hover,
                    f.stripe,
                    f.hidden,
                    f.symlink.unwrap_or(text),
                    f.executable.unwrap_or(text),
                    f.drop_target,
                ];
                changed |= color_grid(ui, "files", file.files.fields_mut(), &shown);
            });
            ui.collapsing("Icons", |ui| {
                use super::IconKind::*;
                let shown = [
                    Folder, Image, Audio, Video, Pdf, Archive, Code, Text, Document, Link, File,
                ]
                .map(|kind| app.icon(kind).unwrap_or(v.text_color()));
                changed |= color_grid(ui, "icons", file.icons.fields_mut(), &shown);
            });
            ui.collapsing("Terminal", |ui| {
                let t = &app.terminal;
                let shown = [
                    t.foreground,
                    t.background,
                    t.cursor,
                    t.selection,
                    t.selection_text,
                ];
                changed |= color_grid(ui, "terminal", file.terminal.fields_mut(), &shown);
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label("ANSI colors");
                    changed |= reset_button(ui, &mut file.terminal.ansi);
                });
                let mut ansi = t
                    .ansi
                    .unwrap_or_else(|| crate::terminal::default_ansi(dark));
                egui::Grid::new("ansi").show(ui, |ui| {
                    for (row, label) in [(0, "Normal"), (8, "Bright")] {
                        ui.label(label);
                        for i in row..row + 8 {
                            if color_edit_button_srgba(ui, &mut ansi[i], Alpha::Opaque).changed() {
                                changed = true;
                                file.terminal.ansi = Some(ansi.iter().map(|&c| Hex(c)).collect());
                            }
                        }
                        ui.end_row();
                    }
                });
            });
            ui.collapsing("Previews", |ui| {
                let m = &app.media;
                let shown = [m.backdrop, m.overlay, m.overlay_icon];
                changed |= color_grid(ui, "media", file.media.fields_mut(), &shown);
            });
            ui.collapsing("Fonts", |ui| {
                ui.weak("Your appearance settings take precedence over these.");
                changed |= fonts_ui(
                    ui,
                    &mut file.fonts,
                    families.as_ref(),
                    &mut editor.font_filter,
                );
            });
            ui.collapsing("Advanced", |ui| {
                ui.weak(
                    "Every one of egui's style settings. What you change here is kept under \
                     [style] in the theme file, over the colors above.",
                );
                if !file.style.is_empty() && ui.button("Reset advanced settings").clicked() {
                    file.style.clear();
                    changed = true;
                }
                let mut style = resolved.themed.clone();
                style.ui(ui);
                if style != resolved.themed {
                    file.style = super::value::style_diff(&resolved.from_colors, &style);
                    changed = true;
                }
            });
        });
        if editable && changed {
            if let Some((old, new)) = renamed {
                for theme in [Theme::Dark, Theme::Light] {
                    if self.prefs.name(theme) == old {
                        *self.prefs.name_mut(theme) = new.clone();
                    }
                }
                self.editor.selected = Some(new);
            }
            self.editor.changed(now);
        }
    }

    fn delete(&mut self, name: &str, path: PathBuf) {
        if let Err(err) = trash::delete(&path) {
            self.editor.message =
                Some((format!("Couldn't delete {}: {err}", path.display()), true));
            return;
        }
        self.editor.draft = None;
        self.editor.selected = None;
        for (theme, fallback) in [
            (Theme::Dark, super::presets::EGUI_DARK),
            (Theme::Light, super::presets::EGUI_LIGHT),
        ] {
            if self.prefs.name(theme) == name {
                *self.prefs.name_mut(theme) = fallback.to_owned();
            }
        }
        self.reload();
        self.editor.message = Some((format!("Moved “{name}” to the trash."), false));
    }

    fn appearance_page(&mut self, ui: &mut Ui) {
        ui.weak("These apply over whichever theme is showing.");
        ui.add_space(4.0);
        let families = self
            .fonts
            .families(ui.ctx())
            .map(|f| (f.all.clone(), f.mono.clone()));
        let appearance = &mut self.prefs.appearance;
        egui::Grid::new("appearance").num_columns(2).show(ui, |ui| {
            ui.label("Zoom");
            let mut zoom = ui.ctx().zoom_factor();
            ui.horizontal(|ui| {
                if ui
                    .add(egui::Slider::new(&mut zoom, 0.5..=3.0).fixed_decimals(2))
                    .on_hover_text("Ctrl and +, - or 0 also zoom")
                    .changed()
                {
                    ui.ctx().set_zoom_factor(zoom);
                }
                if ui.button("Reset").clicked() {
                    ui.ctx().set_zoom_factor(1.0);
                }
            });
            ui.end_row();

            ui.label("Spacing");
            ui.horizontal(|ui| {
                for density in Density::ALL {
                    ui.selectable_value(&mut appearance.density, density, density.label());
                }
            });
            ui.end_row();

            ui.label("Corner radius");
            ui.horizontal(|ui| {
                let mut set = appearance.rounding.is_some();
                if ui.checkbox(&mut set, "").changed() {
                    appearance.rounding = set.then_some(4);
                }
                match &mut appearance.rounding {
                    Some(radius) => {
                        ui.add(egui::Slider::new(radius, 0..=16));
                    }
                    None => {
                        ui.weak("The theme's");
                    }
                }
            });
            ui.end_row();
        });
        ui.add_space(6.0);
        ui.strong("Fonts");
        fonts_ui(
            ui,
            &mut appearance.fonts,
            families.as_ref(),
            &mut self.editor.font_filter,
        );
        ui.add_space(6.0);
        if ui.button("Reset appearance").clicked() {
            *appearance = Default::default();
            ui.ctx().set_zoom_factor(1.0);
        }
    }
}

/// A button that clears an override, shown only while there is one.
fn reset_button<T>(ui: &mut Ui, value: &mut Option<T>) -> bool {
    if value.is_some()
        && ui
            .small_button(icon::ARROW_COUNTER_CLOCKWISE)
            .on_hover_text("Back to the base theme's")
            .clicked()
    {
        *value = None;
        return true;
    }
    false
}

/// A color picker per field; fields the theme doesn't set show what they get
/// from its base, and editing one sets it.
fn color_grid(
    ui: &mut Ui,
    id: &str,
    fields: Vec<(&'static str, &'static str, &mut Color)>,
    shown: &[Color32],
) -> bool {
    let mut changed = false;
    egui::Grid::new(("theme-colors", id))
        .num_columns(3)
        .show(ui, |ui| {
            for ((label, hover, value), &shown) in fields.into_iter().zip(shown) {
                let text = RichText::new(label);
                let response = ui.label(if value.is_some() { text } else { text.weak() });
                if !hover.is_empty() {
                    response.on_hover_text(hover.trim());
                }
                let mut color = value.map_or(shown, |c| c.0);
                if color_edit_button_srgba(ui, &mut color, Alpha::OnlyBlend).changed() {
                    *value = Some(Hex(color));
                    changed = true;
                }
                changed |= reset_button(ui, value);
                ui.end_row();
            }
        });
    changed
}

/// Font family and size pickers; `None` everywhere means "not set here".
fn fonts_ui(
    ui: &mut Ui,
    fonts: &mut Fonts,
    families: Option<&(Vec<String>, Vec<String>)>,
    filter: &mut String,
) -> bool {
    let mut changed = false;
    egui::Grid::new(ui.id().with("fonts"))
        .num_columns(2)
        .show(ui, |ui| {
            for (label, family, size, mono) in [
                ("Interface", &mut fonts.ui, &mut fonts.size, false),
                ("Monospace", &mut fonts.mono, &mut fonts.mono_size, true),
            ] {
                ui.label(label);
                ui.horizontal(|ui| {
                    ComboBox::from_id_salt((label, "family"))
                        .selected_text(family.as_deref().unwrap_or("Default"))
                        .width(200.0)
                        .height(360.0)
                        .show_ui(ui, |ui| {
                            let Some((all, monos)) = families else {
                                ui.spinner();
                                return;
                            };
                            ui.add(TextEdit::singleline(filter).hint_text("Search"));
                            changed |= ui.selectable_value(family, None, "Default").changed();
                            let needle = filter.to_lowercase();
                            for name in if mono { monos } else { all } {
                                if needle.is_empty() || name.to_lowercase().contains(&needle) {
                                    changed |= ui
                                        .selectable_value(family, Some(name.clone()), name)
                                        .changed();
                                }
                            }
                        });
                    let mut set = size.is_some();
                    if ui.checkbox(&mut set, "Size").changed() {
                        *size = set.then_some(13.0);
                        changed = true;
                    }
                    if let Some(size) = size {
                        changed |= ui
                            .add(
                                DragValue::new(size)
                                    .range(6.0..=48.0)
                                    .speed(0.25)
                                    .suffix(" pt"),
                            )
                            .changed();
                    }
                });
                ui.end_row();
            }
        });
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_pos(output: &egui::FullOutput, needle: &str) -> Option<egui::Pos2> {
        output
            .shapes
            .iter()
            .find_map(|clipped| match &clipped.shape {
                egui::Shape::Text(text) if text.galley.text() == needle => {
                    Some(text.pos + text.galley.rect.center().to_vec2())
                }
                _ => None,
            })
    }

    #[test]
    fn duplicate_edit_and_save_a_theme() {
        let dir =
            std::env::temp_dir().join(format!("egui-files-theme-editor-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut themes = Themes::with_dir(Some(dir.clone()), Default::default());
        let ctx = Context::default();
        let time = std::cell::Cell::new(0.0);
        let mut frame = |themes: &mut Themes, events: Vec<egui::Event>| {
            time.set(time.get() + 0.1);
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1200.0, 2000.0),
                )),
                time: Some(time.get()),
                events,
                ..Default::default()
            };
            let mut output = ctx.run_ui(input, |ui| {
                themes.update(ui.ctx());
                themes.editor_ui(ui.ctx());
            });
            output.textures_delta.clear();
            output
        };
        let click = |themes: &mut Themes,
                     frame: &mut dyn FnMut(&mut Themes, Vec<egui::Event>) -> egui::FullOutput,
                     pos| {
            let press = |pressed| egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            };
            frame(themes, vec![egui::Event::PointerMoved(pos)]);
            frame(themes, vec![press(true)]);
            frame(themes, vec![press(false)]);
            frame(themes, vec![])
        };

        themes.open_editor(&ctx);
        themes.select("Catppuccin Mocha".into());
        frame(&mut themes, vec![]);
        let output = frame(&mut themes, vec![]);
        let duplicate = text_pos(&output, &format!("{} Duplicate", icon::COPY)).expect("Duplicate");
        let mut output = click(&mut themes, &mut frame, duplicate);
        let name = "Catppuccin Mocha (custom)";
        assert_eq!(themes.editor.selected.as_deref(), Some(name));
        let path = dir.join("catppuccin-mocha-custom.toml");
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("base = \"Catppuccin Mocha\""), "{written}");

        // Every section opens, Advanced included; from the bottom, so opening
        // one doesn't push the next out of view.
        for section in [
            "Advanced",
            "Fonts",
            "Previews",
            "Terminal",
            "Icons",
            "File lists",
            "Colors",
        ] {
            let pos = text_pos(&output, section).unwrap_or_else(|| panic!("{section}"));
            output = click(&mut themes, &mut frame, pos);
        }
        let used = text_pos(&output, "Use in dark mode").unwrap();
        click(&mut themes, &mut frame, used);
        assert_eq!(themes.prefs.dark, name);

        // An edit shows at once and is saved shortly after.
        let red = Color32::from_rgb(200, 0, 0);
        themes.editor.draft.as_mut().unwrap().file.colors.background = Some(Hex(red));
        themes.editor.changed(time.get());
        frame(&mut themes, vec![]);
        assert_eq!(ctx.style_of(Theme::Dark).visuals.panel_fill, red);
        for _ in 0..10 {
            frame(&mut themes, vec![]);
        }
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("background = \"#c80000\""), "{written}");
        // Just showing egui's style settings doesn't change any.
        assert!(!written.contains("[style"), "{written}");
        assert_eq!(
            themes.get(name).unwrap().file.colors.background,
            Some(Hex(red))
        );

        let appearance = text_pos(&output, "Appearance").unwrap();
        click(&mut themes, &mut frame, appearance);
        assert!(themes.editor.page == Page::Appearance);
    }
}
