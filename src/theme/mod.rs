//! Themes: built-in presets and the user's own TOML files, one chosen for
//! dark mode and one for light mode, with appearance settings on top.
//!
//! A theme sets a few named colors that each set several of egui's own, the
//! app's colors (file lists, icons, terminal, media), fonts, and, under
//! `[style]`, anything else egui's style has. A theme may name a `base` and
//! only list what it changes.

mod editor;
pub mod file;
mod fonts;
pub mod presets;
mod value;

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use egui::{Color32, CornerRadius, Id, Margin, Style, Theme, Visuals};
use serde::{Deserialize, Serialize};

pub use self::editor::ThemeEditor;
use self::file::{Fonts, ThemeFile};

/// What the user chose, saved with the app's state.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    pub dark: String,
    pub light: String,
    pub appearance: Appearance,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            dark: presets::EGUI_DARK.to_owned(),
            light: presets::EGUI_LIGHT.to_owned(),
            appearance: Appearance::default(),
        }
    }
}

impl Prefs {
    pub fn name(&self, theme: Theme) -> &str {
        match theme {
            Theme::Dark => &self.dark,
            Theme::Light => &self.light,
        }
    }

    pub fn name_mut(&mut self, theme: Theme) -> &mut String {
        match theme {
            Theme::Dark => &mut self.dark,
            Theme::Light => &mut self.light,
        }
    }
}

/// The user's own settings, applied over whichever theme is showing.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Appearance {
    pub fonts: Fonts,
    pub density: Density,
    /// Corner radius for widgets, windows and menus.
    pub rounding: Option<u8>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Density {
    Compact,
    #[default]
    Normal,
    Comfortable,
}

impl Density {
    pub const ALL: [Density; 3] = [Density::Compact, Density::Normal, Density::Comfortable];

    pub fn label(self) -> &'static str {
        match self {
            Density::Compact => "Compact",
            Density::Normal => "Normal",
            Density::Comfortable => "Comfortable",
        }
    }

    fn scale(self) -> f32 {
        match self {
            Density::Compact => 0.7,
            Density::Normal => 1.0,
            Density::Comfortable => 1.4,
        }
    }
}

/// The kinds of file with their own icon color.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IconKind {
    Folder,
    Image,
    Audio,
    Video,
    Pdf,
    Archive,
    Code,
    Text,
    Document,
    Link,
    File,
}

/// The app's own colors, worked out for one theme.
#[derive(Clone, Debug, PartialEq)]
pub struct AppColors {
    pub files: FileColors,
    icons: file::Icons,
    pub terminal: TerminalColors,
    pub media: MediaColors,
    /// The syntax highlighting theme the theme suggests.
    pub syntax: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FileColors {
    pub selection: Color32,
    pub selection_text: Color32,
    pub hover: Color32,
    pub stripe: Color32,
    pub hidden: Color32,
    pub symlink: Option<Color32>,
    pub executable: Option<Color32>,
    pub drop_target: Color32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TerminalColors {
    pub foreground: Color32,
    pub background: Color32,
    pub cursor: Color32,
    pub selection: Color32,
    pub selection_text: Color32,
    pub ansi: Option<[Color32; 16]>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MediaColors {
    pub backdrop: Color32,
    /// Behind images, when the theme sets a backdrop; transparent images
    /// otherwise show the panel through.
    pub image_backdrop: Option<Color32>,
    pub overlay: Color32,
    pub overlay_icon: Color32,
}

fn color(c: file::Color) -> Option<Color32> {
    c.map(|c| c.0)
}

impl AppColors {
    fn new(theme: &ThemeFile, visuals: &Visuals) -> Self {
        let f = &theme.files;
        let t = &theme.terminal;
        let m = &theme.media;
        let selection = color(f.selection).unwrap_or(visuals.selection.bg_fill);
        AppColors {
            files: FileColors {
                selection,
                selection_text: color(f.selection_text).unwrap_or(visuals.selection.stroke.color),
                hover: color(f.hover).unwrap_or(visuals.widgets.hovered.weak_bg_fill),
                stripe: color(f.stripe).unwrap_or(visuals.faint_bg_color),
                hidden: color(f.hidden).unwrap_or(visuals.weak_text_color()),
                symlink: color(f.symlink),
                executable: color(f.executable),
                drop_target: color(f.drop_target)
                    .or(color(theme.colors.accent))
                    .unwrap_or(visuals.selection.stroke.color),
            },
            icons: theme.icons.clone(),
            terminal: TerminalColors {
                foreground: color(t.foreground).unwrap_or(visuals.strong_text_color()),
                background: color(t.background).unwrap_or(visuals.extreme_bg_color),
                cursor: color(t.cursor)
                    .or(color(t.foreground))
                    .unwrap_or(visuals.strong_text_color()),
                selection: color(t.selection).unwrap_or(visuals.selection.bg_fill),
                selection_text: color(t.selection_text).unwrap_or(visuals.selection.stroke.color),
                ansi: t
                    .ansi
                    .as_ref()
                    .and_then(|ansi| ansi.iter().map(|c| c.0).collect::<Vec<_>>().try_into().ok()),
            },
            media: MediaColors {
                backdrop: color(m.backdrop).unwrap_or(Color32::BLACK),
                image_backdrop: color(m.backdrop),
                overlay: color(m.overlay).unwrap_or(Color32::from_black_alpha(150)),
                overlay_icon: color(m.overlay_icon).unwrap_or(Color32::WHITE),
            },
            syntax: theme.syntax.clone().unwrap_or_else(|| {
                if visuals.dark_mode {
                    crate::editor::highlight::DEFAULT_DARK_THEME
                } else {
                    crate::editor::highlight::DEFAULT_LIGHT_THEME
                }
                .to_owned()
            }),
        }
    }

    /// An icon's color, or `None` for its name's.
    pub fn icon(&self, kind: IconKind) -> Option<Color32> {
        let i = &self.icons;
        color(match kind {
            IconKind::Folder => i.folder,
            IconKind::Image => i.image,
            IconKind::Audio => i.audio,
            IconKind::Video => i.video,
            IconKind::Pdf => i.pdf,
            IconKind::Archive => i.archive,
            IconKind::Code => i.code,
            IconKind::Text => i.text,
            IconKind::Document => i.document,
            IconKind::Link => i.link,
            IconKind::File => i.file,
        })
    }

    /// The color for a file's name, or `None` for the normal text color.
    pub fn name(&self, hidden: bool, symlink: bool, executable: bool) -> Option<Color32> {
        if hidden {
            Some(self.files.hidden)
        } else if symlink && self.files.symlink.is_some() {
            self.files.symlink
        } else if executable {
            self.files.executable
        } else {
            None
        }
    }

    /// Makes egui's own selection, hover and stripe colors the file list's.
    pub fn apply_to_list(&self, visuals: &mut Visuals) {
        visuals.selection.bg_fill = self.files.selection;
        visuals.selection.stroke.color = self.files.selection_text;
        visuals.widgets.hovered.weak_bg_fill = self.files.hover;
        visuals.faint_bg_color = self.files.stripe;
    }
}

fn colors_id() -> Id {
    Id::new("theme-app-colors")
}

/// The app's colors for the theme showing now.
pub fn colors(ctx: &egui::Context) -> Arc<AppColors> {
    let theme = ctx.theme();
    ctx.data(|d| d.get_temp::<[Arc<AppColors>; 2]>(colors_id()))
        .map(|both| both[theme as usize].clone())
        .unwrap_or_else(|| {
            Arc::new(AppColors::new(
                &ThemeFile::default(),
                &ctx.style_of(theme).visuals,
            ))
        })
}

/// Where a theme came from.
#[derive(Clone, Debug, PartialEq)]
pub enum Source {
    Preset,
    File(PathBuf),
}

pub struct Entry {
    pub name: String,
    pub source: Source,
    pub file: ThemeFile,
    /// The file as written, which is what inheritance merges.
    table: toml::Table,
    /// Why the file couldn't be read; it then counts as its base theme.
    pub error: Option<String>,
}

impl Entry {
    pub fn dark(&self, themes: &Themes) -> bool {
        themes.merged(&self.name).0.dark.unwrap_or(true)
    }
}

/// A theme worked out down to egui's style and the app's colors.
pub struct Resolved {
    /// The theme as merged with its bases.
    pub file: ThemeFile,
    /// egui's style from the named colors, before `[style]`.
    pub from_colors: Style,
    /// With `[style]` applied, before the user's appearance settings.
    pub themed: Style,
    pub style: Style,
    pub app: AppColors,
    pub fonts: Fonts,
    pub warnings: Vec<String>,
}

pub struct Themes {
    entries: Vec<Entry>,
    dir: Option<PathBuf>,
    watcher: Option<notify::RecommendedWatcher>,
    changed: Arc<AtomicBool>,
    pub prefs: Prefs,
    /// The prefs, generation and touch sizing last installed into egui.
    installed: Option<(Prefs, u64, bool)>,
    /// Sizes everything for touch, for the mobile layout.
    pub touch: bool,
    /// A theme hovered in a theme menu, shown in place of the current mode's
    /// theme until the pointer leaves it.
    preview: Option<String>,
    /// What the menus found hovered this frame; becomes `preview` next frame.
    hovered: Option<String>,
    generation: u64,
    fonts: fonts::SystemFonts,
    /// Font choices for the dark and light themes.
    theme_fonts: [Fonts; 2],
    pub editor: ThemeEditor,
}

impl Themes {
    pub fn new(ctx: &egui::Context, prefs: Prefs) -> Self {
        let dir = directories::BaseDirs::new().map(|d| d.config_dir().join("egui-files/themes"));
        let mut themes = Self::with_dir(dir, prefs);
        themes.watch(ctx);
        themes
    }

    fn with_dir(dir: Option<PathBuf>, prefs: Prefs) -> Self {
        let mut themes = Self {
            entries: Vec::new(),
            dir,
            watcher: None,
            changed: Arc::default(),
            prefs,
            installed: None,
            touch: false,
            preview: None,
            hovered: None,
            generation: 0,
            fonts: fonts::SystemFonts::default(),
            theme_fonts: Default::default(),
            editor: ThemeEditor::default(),
        };
        themes.reload();
        themes
    }

    /// Notices theme files changing on disk.
    fn watch(&mut self, ctx: &egui::Context) {
        use notify::Watcher;
        let Some(dir) = self.dir.as_ref().filter(|d| d.is_dir()) else {
            return;
        };
        let changed = self.changed.clone();
        let ctx = ctx.clone();
        let watcher = crate::watcher::recommended(move |event: notify::Result<notify::Event>| {
            if event.is_ok_and(|e| !e.kind.is_access()) {
                changed.store(true, Ordering::Relaxed);
                ctx.request_repaint();
            }
        });
        match watcher {
            Ok(mut watcher) => match watcher.watch(dir, notify::RecursiveMode::NonRecursive) {
                Ok(()) => self.watcher = Some(watcher),
                Err(err) => eprintln!("watching {}: {err}", dir.display()),
            },
            Err(err) => eprintln!("watching themes: {err}"),
        }
    }

    /// Reads the presets and every theme file again.
    pub fn reload(&mut self) {
        let mut entries: Vec<Entry> = presets::all()
            .into_iter()
            .map(|file| Entry {
                name: file.name.clone().unwrap_or_default(),
                source: Source::Preset,
                table: toml::Table::try_from(&file).unwrap_or_default(),
                file,
                error: None,
            })
            .collect();
        let mut files: Vec<PathBuf> = self
            .dir
            .as_ref()
            .and_then(|dir| std::fs::read_dir(dir).ok())
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|e| e == "toml"))
            .collect();
        files.sort();
        for path in files {
            let entry = read_entry(&path);
            // A file named like a preset replaces it.
            entries.retain(|e| e.name != entry.name);
            entries.push(entry);
        }
        self.entries = entries;
        self.generation += 1;
    }

    pub fn get(&self, name: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.name == name)
    }

    /// The theme merged with its bases, and what went wrong on the way.
    fn merged(&self, name: &str) -> (ThemeFile, Vec<String>) {
        let mut warnings = Vec::new();
        let mut chain = Vec::new();
        let mut next = Some(name.to_owned());
        while let Some(name) = next.take() {
            let Some(entry) = self.get(&name) else {
                warnings.push(format!("There's no theme named “{name}”."));
                break;
            };
            if chain.iter().any(|e: &&Entry| e.name == name) {
                warnings.push(format!("“{name}” is its own base."));
                break;
            }
            if let Some(error) = &entry.error {
                warnings.push(format!("{}: {error}", entry.name));
            }
            next = entry.file.base.clone();
            chain.push(entry);
        }
        let mut table = toml::Table::new();
        for entry in chain.iter().rev() {
            value::merge(&mut table, &entry.table);
        }
        table.remove("base");
        let file = toml::Value::Table(table).try_into().unwrap_or_else(|err| {
            warnings.push(format!("{err}"));
            ThemeFile::default()
        });
        (file, warnings)
    }

    /// Works out a theme from its name, or from `edited` in place of that
    /// theme's own file.
    pub fn resolve(&self, name: &str, edited: Option<&ThemeFile>) -> Resolved {
        let (mut file, mut warnings) = match edited {
            Some(edited) => {
                let (mut base, warnings) = match &edited.base {
                    Some(base) => self.merged(base),
                    None => (ThemeFile::default(), Vec::new()),
                };
                let mut table = toml::Table::try_from(&base).unwrap_or_default();
                value::merge(
                    &mut table,
                    &toml::Table::try_from(edited).unwrap_or_default(),
                );
                if let Ok(file) = toml::Value::Table(table).try_into() {
                    base = file;
                }
                (base, warnings)
            }
            None => self.merged(name),
        };
        file.name = Some(name.to_owned());
        let dark = file.dark.unwrap_or(true);
        let mut style = Style {
            visuals: if dark {
                Visuals::dark()
            } else {
                Visuals::light()
            },
            ..Style::default()
        };
        apply_colors(&file.colors, &mut style.visuals);
        apply_text_sizes(&file.fonts, &mut style);
        let from_colors = style.clone();
        if let Err(err) = value::apply_style(&mut style, &file.style) {
            warnings.push(format!("[style]: {err}"));
        }
        let themed = style.clone();
        let appearance = &self.prefs.appearance;
        apply_text_sizes(&appearance.fonts, &mut style);
        apply_density(appearance.density, &mut style);
        if let Some(radius) = appearance.rounding {
            apply_rounding(radius, &mut style.visuals);
        }
        let app = AppColors::new(&file, &style.visuals);
        Resolved {
            fonts: appearance.fonts.over(&file.fonts),
            file,
            from_colors,
            themed,
            style,
            app,
            warnings,
        }
    }

    /// Makes egui show the chosen themes; call every frame.
    pub fn update(&mut self, ctx: &egui::Context) {
        if self.changed.swap(false, Ordering::Relaxed) {
            self.reload();
        }
        if self.watcher.is_none() && self.dir.as_ref().is_some_and(|d| d.is_dir()) {
            self.watch(ctx);
        }
        self.preview = self.hovered.take();
        if self.preview.is_some() {
            // Another frame to see if it's still hovered, since a menu
            // closing doesn't say so.
            ctx.request_repaint();
        }
        let mut prefs = self.prefs.clone();
        if let Some(name) = &self.preview {
            *prefs.name_mut(ctx.theme()) = name.clone();
        }
        let current = (
            prefs,
            self.generation + self.editor.generation(),
            self.touch,
        );
        if self.installed.as_ref() != Some(&current) {
            let mut colors = Vec::new();
            for theme in [Theme::Dark, Theme::Light] {
                let name = current.0.name(theme).to_owned();
                let mut resolved = self.resolve(&name, self.editor.edited(&name));
                if self.touch {
                    crate::touch::touch_style(&mut resolved.style);
                }
                ctx.set_style_of(theme, resolved.style);
                colors.push(Arc::new(resolved.app));
                self.theme_fonts[theme as usize] = resolved.fonts;
            }
            let colors: [Arc<AppColors>; 2] = colors.try_into().expect("two themes");
            ctx.data_mut(|d| d.insert_temp(colors_id(), colors));
            self.installed = Some(current);
        }
        let fonts = &self.theme_fonts[ctx.theme() as usize];
        let (ui, mono) = (fonts.ui.clone(), fonts.mono.clone());
        self.fonts.install(ctx, ui.as_deref(), mono.as_deref());
    }

    /// The themes for one mode, for picking from a menu.
    pub fn menu_ui(&mut self, ui: &mut egui::Ui, theme: Theme) {
        let dark = theme == Theme::Dark;
        let names: Vec<(String, bool)> = self
            .entries
            .iter()
            .map(|e| (e.name.clone(), e.dark(self) == dark))
            .collect();
        let mut hovered = None;
        egui::ScrollArea::vertical()
            .max_height(420.0)
            .show(ui, |ui| {
                let current = self.prefs.name_mut(theme);
                let mut item = |ui: &mut egui::Ui, name: &String| {
                    let response = ui.radio(current == name, name);
                    if response.clicked() {
                        *current = name.clone();
                    }
                    if response.hovered() {
                        hovered = Some(name.clone());
                    }
                };
                for (name, _) in names.iter().filter(|(_, matches)| *matches) {
                    item(ui, name);
                }
                let others: Vec<_> = names.iter().filter(|(_, matches)| !matches).collect();
                if !others.is_empty() {
                    ui.menu_button(if dark { "Light themes" } else { "Dark themes" }, |ui| {
                        for (name, _) in others {
                            item(ui, name);
                        }
                    });
                }
            });
        // Shown from the next frame; a light theme hovered while dark mode
        // shows is previewed in dark mode's place.
        if let Some(name) = hovered {
            if self.preview.as_ref() != Some(&name) {
                ui.ctx().request_repaint();
            }
            self.hovered = Some(name);
        }
    }

    /// Writes a theme file and reads the themes again.
    fn save(&mut self, path: &Path, file: &ThemeFile) -> Result<(), String> {
        let text = toml::to_string_pretty(file).map_err(|e| e.to_string())?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let temp = path.with_extension("toml.tmp");
        std::fs::write(&temp, text)
            .and_then(|()| std::fs::rename(&temp, path))
            .map_err(|e| e.to_string())?;
        self.reload();
        Ok(())
    }

    /// A file name in the themes folder for a new theme.
    fn new_path(&self, name: &str) -> Option<PathBuf> {
        let dir = self.dir.as_ref()?;
        let stem = name
            .split(|c: char| !c.is_alphanumeric())
            .filter(|word| !word.is_empty())
            .collect::<Vec<_>>()
            .join("-")
            .to_lowercase();
        let stem = if stem.is_empty() { "theme" } else { &stem };
        (1..)
            .map(|n| {
                dir.join(if n == 1 {
                    format!("{stem}.toml")
                } else {
                    format!("{stem}-{n}.toml")
                })
            })
            .find(|p| !p.exists())
    }

    /// A theme name nobody uses yet.
    fn unique_name(&self, name: &str) -> String {
        if self.get(name).is_none() {
            return name.to_owned();
        }
        (2..)
            .map(|n| format!("{name} {n}"))
            .find(|n| self.get(n).is_none())
            .expect("some number is free")
    }
}

fn read_entry(path: &Path) -> Entry {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let parsed = std::fs::read_to_string(path)
        .map_err(|e| e.to_string())
        .and_then(|text| toml::from_str::<toml::Table>(&text).map_err(|e| e.to_string()))
        .and_then(|table| {
            let file = ThemeFile::deserialize(toml::Value::Table(table.clone()))
                .map_err(|e| e.message().to_owned())?;
            Ok((table, file))
        });
    match parsed {
        Ok((table, file)) => Entry {
            name: file.name.clone().unwrap_or(stem),
            source: Source::File(path.to_owned()),
            table,
            file,
            error: None,
        },
        Err(error) => {
            // Keep its base, if that much can be read, so it still looks close.
            let base = std::fs::read_to_string(path)
                .ok()
                .and_then(|text| toml::from_str::<toml::Table>(&text).ok())
                .and_then(|t| t.get("base")?.as_str().map(str::to_owned));
            let mut table = toml::Table::new();
            if let Some(base) = &base {
                table.insert("base".into(), base.clone().into());
            }
            Entry {
                name: stem,
                source: Source::File(path.to_owned()),
                table,
                file: ThemeFile {
                    base,
                    ..ThemeFile::default()
                },
                error: Some(error.trim().to_owned()),
            }
        }
    }
}

/// Sets egui's colors from the theme's named ones.
fn apply_colors(c: &file::Colors, v: &mut Visuals) {
    let w = &mut v.widgets;
    if let Some(bg) = color(c.background) {
        v.panel_fill = bg;
    }
    if let Some(surface) = color(c.surface) {
        v.window_fill = surface;
        w.noninteractive.bg_fill = surface;
        w.noninteractive.weak_bg_fill = surface;
    }
    if let Some(input) = color(c.input) {
        v.extreme_bg_color = input;
        v.text_edit_bg_color = Some(input);
        // Stripes: a touch of the text color over the list's background.
        v.faint_bg_color = color(c.text)
            .unwrap_or(w.noninteractive.fg_stroke.color)
            .gamma_multiply(0.04);
    }
    if let Some(border) = color(c.border) {
        w.noninteractive.bg_stroke.color = border;
        v.window_stroke.color = border;
    }
    if let Some(text) = color(c.text) {
        w.noninteractive.fg_stroke.color = text;
        w.inactive.fg_stroke.color = text;
    }
    if let Some(strong) = color(c.strong_text) {
        w.hovered.fg_stroke.color = strong;
        w.active.fg_stroke.color = strong;
        w.open.fg_stroke.color = strong;
    }
    if let Some(weak) = color(c.weak_text) {
        v.weak_text_color = Some(weak);
        w.hovered.bg_stroke.color = weak;
    }
    if let Some(widget) = color(c.widget) {
        w.inactive.bg_fill = widget;
        w.inactive.weak_bg_fill = widget;
        w.open.bg_fill = widget;
        w.open.weak_bg_fill = widget;
        v.code_bg_color = widget;
    }
    if let Some(hover) = color(c.widget_hover) {
        w.hovered.bg_fill = hover;
        w.hovered.weak_bg_fill = hover;
    }
    if let Some(active) = color(c.widget_active) {
        w.active.bg_fill = active;
        w.active.weak_bg_fill = active;
        if let Some(strong) = color(c.strong_text) {
            w.active.bg_stroke.color = strong;
        }
    }
    if let Some(accent) = color(c.accent) {
        v.text_cursor.stroke.color = accent;
        v.hyperlink_color = accent;
    }
    if let Some(selection) = color(c.selection) {
        v.selection.bg_fill = selection;
    }
    if let Some(text) = color(c.selection_text) {
        v.selection.stroke.color = text;
    }
    if let Some(link) = color(c.link) {
        v.hyperlink_color = link;
    }
    if let Some(warning) = color(c.warning) {
        v.warn_fg_color = warning;
    }
    if let Some(error) = color(c.error) {
        v.error_fg_color = error;
    }
}

/// egui's text sizes, scaled from its default body size of 13.
fn apply_text_sizes(fonts: &Fonts, style: &mut Style) {
    use egui::TextStyle;
    let defaults = Style::default().text_styles;
    if let Some(size) = fonts.size.filter(|s| *s > 0.0) {
        for text_style in [
            TextStyle::Body,
            TextStyle::Button,
            TextStyle::Small,
            TextStyle::Heading,
        ] {
            if let (Some(font), Some(default)) = (
                style.text_styles.get_mut(&text_style),
                defaults.get(&text_style),
            ) {
                font.size = default.size * size / 13.0;
            }
        }
    }
    if let Some(size) = fonts.mono_size.filter(|s| *s > 0.0)
        && let Some(font) = style.text_styles.get_mut(&TextStyle::Monospace)
    {
        font.size = size;
    }
}

fn apply_density(density: Density, style: &mut Style) {
    let scale = density.scale();
    if scale == 1.0 {
        return;
    }
    let margin = |m: Margin| Margin {
        left: (m.left as f32 * scale).round() as i8,
        right: (m.right as f32 * scale).round() as i8,
        top: (m.top as f32 * scale).round() as i8,
        bottom: (m.bottom as f32 * scale).round() as i8,
    };
    let s = &mut style.spacing;
    s.item_spacing *= scale;
    s.button_padding *= scale;
    s.window_margin = margin(s.window_margin);
    s.menu_margin = margin(s.menu_margin);
    s.indent *= scale;
    // Widgets stay tall enough for their text.
    s.interact_size.y = (s.interact_size.y * scale).max(14.0);
    s.icon_spacing *= scale;
}

fn apply_rounding(radius: u8, v: &mut Visuals) {
    let radius = CornerRadius::same(radius);
    for w in [
        &mut v.widgets.noninteractive,
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        w.corner_radius = radius;
    }
    v.window_corner_radius = radius;
    v.menu_corner_radius = radius;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn themes_in(dir: &Path) -> Themes {
        Themes::with_dir(Some(dir.to_owned()), Prefs::default())
    }

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("egui-files-themes-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn egui_presets_look_like_egui() {
        let themes = themes_in(&scratch("egui"));
        let dark = themes.resolve(presets::EGUI_DARK, None);
        assert_eq!(dark.style.visuals, Visuals::dark());
        let light = themes.resolve(presets::EGUI_LIGHT, None);
        assert_eq!(light.style.visuals, Visuals::light());
        assert!(dark.warnings.is_empty() && light.warnings.is_empty());
    }

    #[test]
    fn every_preset_resolves_cleanly() {
        let themes = themes_in(&scratch("presets"));
        for theme in presets::all() {
            let name = theme.name.unwrap();
            let resolved = themes.resolve(&name, None);
            assert!(
                resolved.warnings.is_empty(),
                "{name}: {:?}",
                resolved.warnings
            );
            assert_eq!(
                resolved.style.visuals.dark_mode,
                theme.dark.unwrap(),
                "{name}"
            );
            let egui = name.starts_with("egui");
            assert_eq!(resolved.app.terminal.ansi.is_some(), !egui, "{name}");
        }
    }

    #[test]
    fn files_inherit_from_their_base() {
        let dir = scratch("inherit");
        std::fs::write(
            dir.join("red.toml"),
            r##"
            name = "Red Mocha"
            base = "Catppuccin Mocha"
            [colors]
            accent = "#ff0000"
            [style.visuals]
            window_corner_radius = { nw = 0, ne = 0, sw = 0, se = 0 }
            "##,
        )
        .unwrap();
        let themes = themes_in(&dir);
        let mocha = themes.resolve("Catppuccin Mocha", None);
        let red = themes.resolve("Red Mocha", None);
        assert!(red.warnings.is_empty(), "{:?}", red.warnings);
        assert_eq!(red.style.visuals.hyperlink_color, Color32::RED);
        assert_eq!(red.style.visuals.panel_fill, mocha.style.visuals.panel_fill);
        assert_eq!(red.style.visuals.window_corner_radius, CornerRadius::ZERO);
        assert_eq!(red.app.syntax, "Catppuccin Mocha");
        assert!(themes.get("Red Mocha").unwrap().dark(&themes));
    }

    #[test]
    fn broken_files_fall_back_to_their_base_with_a_warning() {
        let dir = scratch("broken");
        std::fs::write(
            dir.join("broken.toml"),
            "base = \"Nord\"\n[colors]\ntext = 12",
        )
        .unwrap();
        std::fs::write(dir.join("loop.toml"), "base = \"loop\"").unwrap();
        let themes = themes_in(&dir);
        let broken = themes.resolve("broken", None);
        assert!(!broken.warnings.is_empty());
        let nord = themes.resolve("Nord", None);
        assert_eq!(
            broken.style.visuals.panel_fill,
            nord.style.visuals.panel_fill
        );
        assert!(!themes.resolve("loop", None).warnings.is_empty());
        assert!(!themes.resolve("missing", None).warnings.is_empty());
    }

    #[test]
    fn appearance_applies_over_any_theme() {
        let mut themes = themes_in(&scratch("appearance"));
        themes.prefs.appearance = Appearance {
            fonts: Fonts {
                size: Some(26.0),
                ..Fonts::default()
            },
            density: Density::Compact,
            rounding: Some(0),
        };
        let resolved = themes.resolve("Nord", None);
        let body = &resolved.style.text_styles[&egui::TextStyle::Body];
        assert_eq!(body.size, 26.0);
        assert!(resolved.style.spacing.item_spacing.x < Style::default().spacing.item_spacing.x);
        assert_eq!(
            resolved.style.visuals.window_corner_radius,
            CornerRadius::ZERO
        );
    }

    #[test]
    fn saved_themes_are_read_back() {
        let dir = scratch("save");
        let mut themes = themes_in(&dir);
        let path = themes.new_path("My Theme!").unwrap();
        assert_eq!(path.file_name().unwrap(), "my-theme.toml");
        let file = ThemeFile {
            name: Some("My Theme!".into()),
            base: Some("Dracula".into()),
            ..ThemeFile::default()
        };
        themes.save(&path, &file).unwrap();
        assert_eq!(themes.get("My Theme!").unwrap().file, file);
        assert_eq!(themes.unique_name("My Theme!"), "My Theme! 2");
    }

    #[test]
    fn hovering_a_theme_previews_it_until_the_pointer_leaves() {
        let mut themes = themes_in(&scratch("preview"));
        let ctx = egui::Context::default();
        ctx.set_theme(egui::ThemePreference::Dark);
        let mut frame = |pointer: egui::Event| {
            let input = egui::RawInput {
                events: vec![pointer],
                ..Default::default()
            };
            for _ in 0..2 {
                ctx.run_ui(input.clone(), |ui| {
                    themes.update(ui.ctx());
                    themes.menu_ui(ui, Theme::Dark);
                })
                .textures_delta
                .clear();
            }
            ctx.style_of(Theme::Dark).visuals.panel_fill
        };
        let default = frame(egui::Event::PointerGone);
        assert_eq!(default, Visuals::dark().panel_fill);
        let previewed = (0..80)
            .map(|step| {
                frame(egui::Event::PointerMoved(egui::pos2(
                    20.0,
                    step as f32 * 5.0,
                )))
            })
            .find(|&fill| fill != default);
        assert!(previewed.is_some(), "hovering some theme changes the style");
        // Hover lags a frame behind the pointer.
        frame(egui::Event::PointerGone);
        assert_eq!(frame(egui::Event::PointerGone), default);
        assert_eq!(themes.prefs.dark, presets::EGUI_DARK);
    }

    #[test]
    fn oled_theme_is_black() {
        let themes = themes_in(&scratch("oled"));
        let oled = themes.resolve("OLED Black", None);
        let visuals = &oled.style.visuals;
        for (what, color) in [
            ("panels", visuals.panel_fill),
            ("windows", visuals.window_fill),
            ("text fields", visuals.extreme_bg_color),
            (
                "the editor",
                crate::editor::highlight::ThemeColors::of(&oled.app.syntax, visuals).background,
            ),
        ] {
            assert_eq!(color, egui::Color32::BLACK, "{what}");
        }
    }
}
