//! Help → About: the app's version, the commit and toolchain it was built
//! from, the system it runs on, and credits with the license texts of every
//! crate compiled into it.

use std::{
    path::PathBuf,
    process::Command,
    sync::{Arc, OnceLock},
};

use egui::{Context, Grid, Id, RichText, ScrollArea, TextEdit, Ui};
use egui_phosphor::regular as icon;

/// A crate in the dependency tree, from `cargo metadata` at build time.
pub struct Crate {
    pub name: &'static str,
    pub version: &'static str,
    /// An SPDX expression, or empty.
    pub license: &'static str,
    pub authors: &'static [&'static str],
    pub description: &'static str,
    pub repository: &'static str,
    pub homepage: &'static str,
    /// Named in Cargo.toml.
    pub direct: bool,
    /// Compiled into the app, rather than only run while building it
    /// (proc-macros, build scripts and what they use).
    pub in_app: bool,
    /// The crates depending on it.
    pub used_by: &'static [&'static str],
    /// License files as (file name, index into [`TEXTS`]).
    pub files: &'static [(&'static str, usize)],
    /// The package has no license file; `files` holds the standard texts.
    pub standard_text: bool,
}

pub struct Git {
    pub hash: &'static str,
    pub short_hash: &'static str,
    pub commit_time: i64,
    pub author_time: i64,
    pub author: &'static str,
    pub subject: &'static str,
    pub branch: &'static str,
    pub describe: &'static str,
    pub commit_count: u64,
    pub modified_files: usize,
    pub untracked_files: usize,
    pub remote: Option<&'static str>,
}

mod generated {
    use super::{Crate, Git};
    include!(concat!(env!("OUT_DIR"), "/build_info.rs"));
    include!(concat!(env!("OUT_DIR"), "/credits.rs"));
}
use generated::{CRATES, GIT, TEXTS};

const NAME: &str = env!("CARGO_PKG_NAME");
const VERSION: &str = env!("CARGO_PKG_VERSION");
const DESCRIPTION: &str = env!("CARGO_PKG_DESCRIPTION");
const LICENSE: &str = env!("CARGO_PKG_LICENSE");

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    About,
    Build,
    System,
    Credits,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Filter {
    All,
    InApp,
    Direct,
    BuildOnly,
}

impl Filter {
    fn label(self) -> &'static str {
        match self {
            Self::All => "All crates",
            Self::InApp => "In the app",
            Self::Direct => "Direct dependencies",
            Self::BuildOnly => "Build tools only",
        }
    }

    fn matches(self, c: &Crate) -> bool {
        match self {
            Self::All => true,
            Self::InApp => c.in_app,
            Self::Direct => c.direct,
            Self::BuildOnly => !c.in_app,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Selected {
    /// The syntax definitions and color themes embedded through two-face.
    Syntaxes,
    Crate(usize),
}

/// Known when the window is created.
struct Startup {
    windowing: &'static str,
    gpu: Vec<(&'static str, String)>,
}

/// Found on a background thread the first time the window opens, since it
/// runs programs and loads libraries.
struct Probed {
    os: String,
    kernel: Option<String>,
    desktop: Option<String>,
    locale: Option<String>,
    libmpv: Result<crate::video::Libmpv, String>,
    /// (program, first line of its version output, or None if missing).
    tools: Vec<(&'static str, Option<String>)>,
    syntaxes: usize,
    themes: usize,
    settings_dir: Option<PathBuf>,
    thumbnail_dir: Option<PathBuf>,
}

pub struct About {
    pub open: bool,
    page: Page,
    search: String,
    filter: Filter,
    selected: Selected,
    /// Showing the selected crate's details in place of the list, when
    /// they don't fit side by side (on a phone).
    details_open: bool,
    startup: Startup,
    probed: Option<Arc<OnceLock<Probed>>>,
}

impl About {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        #[cfg(not(target_arch = "wasm32"))]
        let windowing = {
            use raw_window_handle::{HasWindowHandle, RawWindowHandle};
            match cc.window_handle().map(|h| h.as_raw()) {
                Ok(RawWindowHandle::Wayland(_)) => "Wayland",
                Ok(RawWindowHandle::Xlib(_) | RawWindowHandle::Xcb(_)) => "X11",
                Ok(RawWindowHandle::Win32(_)) => "Win32",
                Ok(RawWindowHandle::AppKit(_)) => "AppKit",
                _ => "unknown",
            }
        };
        #[cfg(target_arch = "wasm32")]
        let windowing = {
            let _ = cc;
            "browser"
        };
        Self {
            open: false,
            page: Page::About,
            search: String::new(),
            filter: Filter::All,
            selected: Selected::Crate(crate_index(NAME_FOR_START).unwrap_or(0)),
            details_open: false,
            startup: Startup {
                windowing,
                gpu: gpu_rows(cc),
            },
            probed: None,
        }
    }

    pub fn ui(&mut self, ctx: &Context) {
        if !self.open {
            return;
        }
        let probed = self
            .probed
            .get_or_insert_with(|| {
                let cell = Arc::new(OnceLock::new());
                let (cell2, ctx) = (cell.clone(), ctx.clone());
                crate::background::spawn("about-probe", move || {
                    let _ = cell2.set(probe());
                    ctx.request_repaint();
                });
                cell
            })
            .clone();
        let mut open = self.open;
        egui::Window::new(format!("About {NAME}"))
            .id(Id::new("about-window"))
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_size([780.0, 540.0])
            .min_size([420.0, 300.0])
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    for (page, label) in [
                        (Page::About, format!("{} About", icon::INFO)),
                        (Page::Build, format!("{} Build", icon::GIT_COMMIT)),
                        (Page::System, format!("{} System", icon::DESKTOP)),
                        (Page::Credits, format!("{} Credits", icon::HEART)),
                    ] {
                        ui.selectable_value(&mut self.page, page, label);
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .button(format!("{} Copy info", icon::COPY))
                            .on_hover_text(
                                "Version, build and system details as text, for bug reports",
                            )
                            .clicked()
                        {
                            ui.ctx().copy_text(self.report(ui.ctx(), probed.get()));
                        }
                    });
                });
                ui.separator();
                match self.page {
                    Page::About => about_page(ui),
                    Page::Build => {
                        ScrollArea::vertical()
                            .show(ui, |ui| {
                                sections_ui(ui, &build_sections());
                            })
                            .inner
                    }
                    Page::System => {
                        ScrollArea::vertical()
                            .show(ui, |ui| {
                                sections_ui(ui, &self.system_sections(ui.ctx(), probed.get()));
                                if probed.get().is_none() {
                                    ui.horizontal(|ui| {
                                        ui.spinner();
                                        ui.label("Looking for optional components…");
                                    });
                                }
                            })
                            .inner
                    }
                    Page::Credits => self.credits_page(ui),
                }
            });
        self.open = open;
    }

    /// Everything on the Build and System pages, as text.
    fn report(&self, ctx: &Context, probed: Option<&Probed>) -> String {
        let mut text = format!("{NAME} {VERSION}\n");
        for (title, rows) in build_sections()
            .into_iter()
            .chain(self.system_sections(ctx, probed))
        {
            text += &format!("\n[{title}]\n");
            for (key, value) in rows {
                text += &format!("{key}: {value}\n");
            }
        }
        text
    }

    fn system_sections(&self, ctx: &Context, probed: Option<&Probed>) -> Vec<Section> {
        let mut system = vec![
            ("OS", probed.map_or_else(String::new, |p| p.os.clone())),
            ("Architecture", std::env::consts::ARCH.into()),
        ];
        if let Some(p) = probed {
            system.extend(p.kernel.clone().map(|k| ("Kernel", k)));
            system.extend(p.desktop.clone().map(|d| ("Desktop", d)));
            system.extend(p.locale.clone().map(|l| ("Locale", l)));
        }
        system.push(("Windowing", self.startup.windowing.into()));
        let (ppp, monitor) = ctx.input(|i| (i.pixels_per_point, i.viewport().monitor_size));
        system.push(("UI scale", format!("{ppp}")));
        if let Some(size) = monitor {
            system.push((
                "Monitor",
                format!("{:.0}×{:.0} px", size.x * ppp, size.y * ppp),
            ));
        }

        let gpu = self.startup.gpu.clone();

        let mut sections = vec![("System", system), ("Graphics", gpu)];
        if let Some(p) = probed {
            let mut optional = Vec::new();
            match &p.libmpv {
                Ok(mpv) => {
                    optional.push((
                        "libmpv",
                        format!("{} (client API {})", mpv.library, mpv.api_version),
                    ));
                    optional.extend(mpv.mpv_version.clone().map(|v| ("mpv", v)));
                    optional.extend(mpv.ffmpeg_version.clone().map(|v| ("FFmpeg (mpv)", v)));
                }
                Err(e) => optional.push(("libmpv", format!("not available: {e}"))),
            }
            for (tool, version) in &p.tools {
                optional.push((tool, version.clone().unwrap_or_else(|| "not found".into())));
            }
            optional.push((
                "Syntax highlighting",
                format!("{} syntaxes, {} color themes", p.syntaxes, p.themes),
            ));
            sections.push(("Optional components", optional));

            let path = |p: &Option<PathBuf>| {
                p.as_ref()
                    .map_or_else(|| "none".into(), |p| p.display().to_string())
            };
            sections.push((
                "Paths",
                vec![
                    ("Settings", path(&p.settings_dir)),
                    ("Thumbnail cache", path(&p.thumbnail_dir)),
                ],
            ));
        }
        sections
    }

    fn credits_page(&mut self, ui: &mut Ui) {
        // Side by side when there's room, else one at a time.
        if ui.available_width() < 500.0 {
            if self.details_open {
                if ui.button(format!("{} Crates", icon::ARROW_LEFT)).clicked() {
                    self.details_open = false;
                }
                ui.separator();
                self.credits_details(ui);
            } else if self.credits_list(ui) {
                self.details_open = true;
            }
            return;
        }
        egui::Panel::left("about-credits-list")
            .resizable(true)
            .default_size(250.0)
            .min_size(160.0)
            .show(ui, |ui| self.credits_list(ui));
        self.credits_details(ui);
    }

    fn credits_details(&self, ui: &mut Ui) {
        ScrollArea::vertical()
            .scroll_source(crate::touch::scroll_source(ui.ctx()))
            .id_salt(("about-credits-details", self.selected_key()))
            .auto_shrink(false)
            .show(ui, |ui| match self.selected {
                Selected::Syntaxes => syntaxes_ui(ui),
                Selected::Crate(i) => crate_ui(ui, &CRATES[i]),
            });
    }

    /// The searchable list of crates. Returns whether one was picked.
    fn credits_list(&mut self, ui: &mut Ui) -> bool {
        let mut picked = false;
        ui.add(
            TextEdit::singleline(&mut self.search)
                .hint_text(format!(
                    "{} Search crates, licenses, authors",
                    icon::MAGNIFYING_GLASS
                ))
                .desired_width(f32::INFINITY),
        );
        egui::ComboBox::from_id_salt("about-credits-filter")
            .selected_text(self.filter.label())
            .width(ui.available_width())
            .show_ui(ui, |ui| {
                for filter in [
                    Filter::All,
                    Filter::InApp,
                    Filter::Direct,
                    Filter::BuildOnly,
                ] {
                    ui.selectable_value(&mut self.filter, filter, filter.label());
                }
            });
        let query = self.search.to_lowercase();
        let shown: Vec<usize> = CRATES
            .iter()
            .enumerate()
            .filter(|(_, c)| self.filter.matches(c) && matches_query(c, &query))
            .map(|(i, _)| i)
            .collect();
        ui.weak(format!("{} of {} crates", shown.len(), CRATES.len()));
        ui.separator();
        let row_height = ui.spacing().interact_size.y;
        let syntaxes = query.is_empty() || "syntaxes themes bat two-face".contains(&query);
        let rows = shown.len() + usize::from(syntaxes);
        ScrollArea::vertical()
            .scroll_source(crate::touch::scroll_source(ui.ctx()))
            .auto_shrink(false)
            .show_rows(ui, row_height, rows, |ui, range| {
                for row in range {
                    let (selected, text) = match row.checked_sub(usize::from(syntaxes)) {
                        None => (
                            Selected::Syntaxes,
                            RichText::new(format!("{} Syntaxes & themes", icon::PALETTE)),
                        ),
                        Some(i) => {
                            let c = &CRATES[shown[i]];
                            let text = RichText::new(format!("{} {}", c.name, c.version));
                            (
                                Selected::Crate(shown[i]),
                                if c.in_app { text } else { text.weak() },
                            )
                        }
                    };
                    ui.add_sized(
                        [ui.available_width(), row_height],
                        egui::Button::selectable(self.selected == selected, text).truncate(),
                    )
                    .clicked()
                    .then(|| {
                        self.selected = selected;
                        picked = true;
                    });
                }
            });
        picked
    }

    fn selected_key(&self) -> usize {
        match self.selected {
            Selected::Syntaxes => usize::MAX,
            Selected::Crate(i) => i,
        }
    }
}

/// The crate the Credits page starts on.
const NAME_FOR_START: &str = "egui";

fn crate_index(name: &str) -> Option<usize> {
    CRATES.iter().position(|c| c.name == name)
}

/// What the System page says of the GPU, known when the window is created.
#[cfg(not(target_arch = "wasm32"))]
fn gpu_rows(cc: &eframe::CreationContext<'_>) -> Vec<(&'static str, String)> {
    let Some(state) = &cc.wgpu_render_state else {
        return vec![("Adapter", "unknown".into())];
    };
    let info = state.adapter.get_info();
    let mut rows = vec![
        ("Adapter", info.name),
        ("Type", format!("{:?}", info.device_type)),
        ("Backend", format!("{:?}", info.backend)),
        (
            "Vendor / device",
            format!("{:04x}:{:04x}", info.vendor, info.device),
        ),
    ];
    if !info.driver.is_empty() {
        rows.push(("Driver", info.driver));
    }
    if !info.driver_info.is_empty() {
        rows.push(("Driver info", info.driver_info));
    }
    rows
}

/// What the System page says of the GPU: the browser only tells WebGL's name.
#[cfg(target_arch = "wasm32")]
fn gpu_rows(cc: &eframe::CreationContext<'_>) -> Vec<(&'static str, String)> {
    use eframe::glow::HasContext as _;
    let Some(gl) = &cc.gl else {
        return vec![("Adapter", "unknown".into())];
    };
    // SAFETY: only reads a string, on the thread with the context.
    let (renderer, version) = unsafe {
        (
            gl.get_parameter_string(eframe::glow::RENDERER),
            gl.get_parameter_string(eframe::glow::VERSION),
        )
    };
    vec![("Adapter", renderer), ("Backend", version)]
}

fn crate_version(name: &str) -> &'static str {
    crate_index(name).map_or("?", |i| CRATES[i].version)
}

fn matches_query(c: &Crate, query: &str) -> bool {
    query.is_empty()
        || [c.name, c.license, c.description]
            .iter()
            .chain(c.authors)
            .any(|s| s.to_lowercase().contains(query))
}

type Section = (&'static str, Vec<(&'static str, String)>);

fn sections_ui(ui: &mut Ui, sections: &[Section]) {
    for (title, rows) in sections {
        ui.heading(*title);
        Grid::new(("about-section", *title))
            .num_columns(2)
            .spacing([16.0, 4.0])
            .striped(true)
            .show(ui, |ui| {
                for (key, value) in rows {
                    ui.label(RichText::new(*key).strong());
                    ui.add(egui::Label::new(value).selectable(true).wrap());
                    ui.end_row();
                }
            });
        ui.add_space(8.0);
    }
}

fn local_time(secs: i64) -> String {
    chrono::DateTime::from_timestamp(secs, 0).map_or_else(String::new, |t| {
        t.with_timezone(&chrono::Local)
            .format("%Y-%m-%d %H:%M:%S %:z")
            .to_string()
    })
}

fn build_sections() -> Vec<Section> {
    use generated::*;

    let mut sections = Vec::new();
    match &GIT {
        Some(git) => {
            let mut rows = vec![
                ("Commit", git.hash.to_owned()),
                ("Short hash", git.short_hash.to_owned()),
                ("Subject", git.subject.to_owned()),
                ("Author", git.author.to_owned()),
                ("Committed", local_time(git.commit_time)),
            ];
            if git.author_time != git.commit_time {
                rows.push(("Authored", local_time(git.author_time)));
            }
            rows.extend([
                ("Branch", git.branch.to_owned()),
                ("Describe", git.describe.to_owned()),
                ("Commits", git.commit_count.to_string()),
                (
                    "Working tree",
                    match (git.modified_files, git.untracked_files) {
                        (0, 0) => "clean".to_owned(),
                        (m, u) => format!(
                            "{m} changed, {u} untracked files (uncommitted changes built in)"
                        ),
                    },
                ),
            ]);
            rows.extend(git.remote.map(|r| ("Remote", r.to_owned())));
            sections.push(("Source", rows));
        }
        None => sections.push((
            "Source",
            vec![("Git", "not built from a git checkout".into())],
        )),
    }
    sections.push((
        "Toolchain",
        vec![
            ("Compiler", RUSTC.to_owned()),
            ("LLVM", LLVM.to_owned()),
            ("Cargo", CARGO.to_owned()),
            (
                "Profile",
                format!(
                    "{PROFILE}, opt-level {OPT_LEVEL}, debug assertions {}",
                    if cfg!(debug_assertions) { "on" } else { "off" }
                ),
            ),
            ("Target", TARGET.to_owned()),
            ("Host", HOST.to_owned()),
            ("Built", local_time(BUILT)),
        ],
    ));
    sections.push((
        "Libraries",
        [
            "egui",
            "eframe",
            "egui_dock",
            "wgpu",
            "winit",
            "alacritty_terminal",
            "syntect",
        ]
        .into_iter()
        .map(|name| (name, crate_version(name).to_owned()))
        .collect(),
    ));
    sections
}

fn about_page(ui: &mut Ui) {
    ScrollArea::vertical().show(ui, |ui| {
        ui.vertical_centered(|ui| {
            ui.add_space(8.0);
            ui.label(RichText::new(icon::FOLDERS).size(56.0));
            ui.label(RichText::new(NAME).size(26.0).strong());
            let commit = GIT.as_ref().map_or(String::new(), |g| format!(" ({})", g.describe));
            ui.label(format!("Version {VERSION}{commit}"));
            ui.add_space(4.0);
            ui.label(DESCRIPTION);
            ui.weak(format!("Licensed under {LICENSE}"));
        });
        ui.add_space(12.0);
        ui.heading("Built on");
        let links: &[(&str, &str, &str)] = &[
            ("egui and eframe", "https://github.com/emilk/egui", "the GUI, by Emil Ernerfeldt and contributors"),
            ("egui_dock", "https://github.com/Adanos020/egui_dock", "tabs and docking"),
            ("Phosphor Icons", "https://phosphoricons.com", "icons, through egui-phosphor"),
            ("wgpu", "https://wgpu.rs", "GPU rendering"),
            ("alacritty_terminal", "https://github.com/alacritty/alacritty", "the terminal pane"),
            ("syntect", "https://github.com/trishume/syntect", "syntax highlighting, with bat's syntaxes and themes through two-face"),
            ("UnRAR", "https://www.rarlab.com", "RAR extraction, by Alexander Roshal, through the unrar crate"),
            ("mpv", "https://mpv.io", "video playback, loaded at runtime if installed (not bundled)"),
            ("FFmpeg", "https://ffmpeg.org", "video decoding and thumbnails, used through mpv and ffmpegthumbnailer if installed (not bundled)"),
        ];
        for (name, url, what) in links {
            ui.horizontal_wrapped(|ui| {
                ui.hyperlink_to(*name, *url);
                ui.label(format!("— {what}"));
            });
        }
        ui.add_space(8.0);
        let in_app = CRATES.iter().filter(|c| c.in_app).count();
        ui.label(format!(
            "…and {in_app} crates compiled into the app, plus {} used only to build it. \
             See Credits for each one's authors and license.",
            CRATES.len() - in_app
        ));
    });
}

fn crate_ui(ui: &mut Ui, c: &Crate) {
    ui.heading(format!("{} {}", c.name, c.version));
    if !c.description.is_empty() {
        ui.label(c.description);
    }
    ui.add_space(4.0);
    Grid::new("about-crate")
        .num_columns(2)
        .spacing([16.0, 4.0])
        .show(ui, |ui| {
            let row = |ui: &mut Ui, key: &str, add: &mut dyn FnMut(&mut Ui)| {
                ui.label(RichText::new(key).strong());
                add(ui);
                ui.end_row();
            };
            row(ui, "License", &mut |ui| {
                ui.label(if c.license.is_empty() {
                    "see below"
                } else {
                    c.license
                });
            });
            if !c.authors.is_empty() {
                row(ui, "Authors", &mut |ui| {
                    ui.add(egui::Label::new(c.authors.join("\n")).selectable(true));
                });
            }
            for (key, url) in [("Repository", c.repository), ("Homepage", c.homepage)] {
                if !url.is_empty() && (key == "Repository" || url != c.repository) {
                    row(ui, key, &mut |ui| {
                        ui.hyperlink(url);
                    });
                }
            }
            row(ui, "crates.io", &mut |ui| {
                ui.hyperlink(format!("https://crates.io/crates/{}", c.name));
            });
            row(ui, "Used", &mut |ui| {
                ui.label(match (c.in_app, c.direct) {
                    (true, true) => "compiled into the app; a direct dependency",
                    (true, false) => "compiled into the app",
                    (false, true) => "only while building the app; a direct dependency",
                    (false, false) => "only while building the app (macros, build scripts)",
                });
            });
            if !c.used_by.is_empty() {
                row(ui, "Needed by", &mut |ui| {
                    ui.add(egui::Label::new(c.used_by.join(", ")).wrap());
                });
            }
        });
    ui.add_space(8.0);
    if c.standard_text {
        ui.weak("The published crate has no license file of its own; these are the standard texts of its licenses.");
    }
    if c.files.is_empty() {
        ui.weak("No license text found.");
    }
    for (i, (name, text)) in c.files.iter().enumerate() {
        egui::CollapsingHeader::new(RichText::new(*name).strong())
            .id_salt(("about-license", c.name, c.version, i))
            .default_open(i == 0)
            .show(ui, |ui| license_text(ui, TEXTS[*text]));
    }
}

fn syntaxes_ui(ui: &mut Ui) {
    use two_face::acknowledgement::{Acknowledgements, License};

    static LISTING: OnceLock<Acknowledgements> = OnceLock::new();
    let listing = LISTING.get_or_init(two_face::acknowledgement::listing);
    ui.heading("Syntaxes & themes");
    ui.label(
        "Syntax definitions and color themes collected by bat, embedded through the two-face crate. \
         These are the ones whose licenses ask to be acknowledged.",
    );
    ui.horizontal(|ui| {
        ui.hyperlink_to("bat", "https://github.com/sharkdp/bat");
        ui.hyperlink_to("two-face", "https://codeberg.org/CosmicHarper/two-face");
        ui.hyperlink_to("Full list", two_face::acknowledgement::url());
    });
    let section = |ui: &mut Ui, title: &str, licenses: &[License]| {
        ui.add_space(8.0);
        ui.heading(format!("{title} ({})", licenses.len()));
        for license in licenses {
            // "syntaxes/02_Extra/HTML_(Twig)/LICENSE" → "HTML (Twig)".
            let path = license.rel_path.display().to_string();
            let dirs: Vec<String> = license
                .rel_path
                .parent()
                .into_iter()
                .flat_map(|p| p.iter())
                .map(|c| c.to_string_lossy().into_owned())
                .collect();
            let skip = if dirs.first().is_some_and(|d| d == "syntaxes") {
                2
            } else {
                1
            };
            let name = dirs
                .get(skip..)
                .unwrap_or_default()
                .join("/")
                .replace('_', " ");
            egui::CollapsingHeader::new(RichText::new(&name).strong())
                .id_salt(("about-syntax-license", &path))
                .show(ui, |ui| {
                    ui.weak(format!("{} · {path}", license_name(license.ty)));
                    license_text(ui, &license.text);
                });
        }
    };
    section(ui, "Syntaxes", listing.for_syntaxes());
    section(ui, "Themes", listing.for_themes());
}

fn license_name(ty: two_face::acknowledgement::LicenseType) -> String {
    use two_face::acknowledgement::LicenseType::*;
    match ty {
        Sublime => "Sublime license".into(),
        Mit => "MIT".into(),
        Bsd2Clause => "BSD-2-Clause".into(),
        Bsd2ClauseFreeBsd => "BSD-2-Clause (FreeBSD)".into(),
        Unlicense => "Unlicense".into(),
        Bsd3Clause => "BSD-3-Clause".into(),
        Apache2 => "Apache-2.0".into(),
        Wtfpl => "WTFPL".into(),
        other => format!("{other:?}"),
    }
}

fn license_text(ui: &mut Ui, text: &str) {
    ui.add(
        egui::Label::new(RichText::new(text).monospace().size(12.0))
            .selectable(true)
            .wrap(),
    );
}

fn probe() -> Probed {
    let tools = ["ffmpegthumbnailer", "ffmpeg"]
        .into_iter()
        .map(|tool| (tool, tool_version(tool)))
        .collect();
    Probed {
        os: os_name(),
        kernel: kernel(),
        desktop: desktop(),
        locale: ["LC_ALL", "LC_MESSAGES", "LANG"]
            .iter()
            .find_map(|v| std::env::var(v).ok().filter(|s| !s.is_empty())),
        libmpv: crate::video::libmpv(),
        tools,
        syntaxes: crate::editor::highlight::syntax_names().len(),
        themes: crate::editor::highlight::theme_names().count(),
        #[cfg(not(target_arch = "wasm32"))]
        settings_dir: eframe::storage_dir(NAME),
        // The browser keeps them in its local storage.
        #[cfg(target_arch = "wasm32")]
        settings_dir: None,
        thumbnail_dir: crate::thumbnails::cache_dir(),
    }
}

/// The version a program prints, from the first line minus its name and
/// copyright.
fn tool_version(program: &str) -> Option<String> {
    let flag = if program == "ffmpegthumbnailer" {
        "-v"
    } else {
        "-version"
    };
    let mut command = Command::new(program);
    command.arg(flag).stdin(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let output = command.output().ok()?;
    let text = String::from_utf8_lossy(if output.stdout.is_empty() {
        &output.stderr
    } else {
        &output.stdout
    })
    .into_owned();
    let line = text.lines().next().unwrap_or_default();
    let line = line.split(" Copyright").next().unwrap_or(line);
    let line = line
        .strip_prefix(&format!("{program} version"))
        .unwrap_or(line)
        .trim_start_matches(':')
        .trim();
    Some(if line.is_empty() {
        "installed".into()
    } else {
        line.to_owned()
    })
}

#[cfg(target_os = "linux")]
fn os_name() -> String {
    let release = disk::read_to_string("/etc/os-release")
        .or_else(|_| disk::read_to_string("/usr/lib/os-release"))
        .unwrap_or_default();
    let field = |name: &str| {
        release.lines().find_map(|l| {
            Some(
                l.strip_prefix(name)?
                    .strip_prefix('=')?
                    .trim_matches('"')
                    .to_owned(),
            )
        })
    };
    let name = field("PRETTY_NAME")
        .or_else(|| field("NAME"))
        .unwrap_or_else(|| "Linux".into());
    match field("VERSION_ID").or_else(|| field("BUILD_ID")) {
        Some(version) if !name.contains(&version) => format!("{name} ({version})"),
        _ => name,
    }
}

#[cfg(not(target_os = "linux"))]
fn os_name() -> String {
    std::env::consts::OS.into()
}

#[cfg(target_os = "linux")]
fn kernel() -> Option<String> {
    // SAFETY: uname fills the zeroed struct with NUL-terminated strings.
    let name = unsafe {
        let mut name: libc::utsname = std::mem::zeroed();
        (libc::uname(&mut name) == 0).then_some(name)?
    };
    let field = |f: &[libc::c_char]| {
        // SAFETY: NUL-terminated within the array.
        unsafe { std::ffi::CStr::from_ptr(f.as_ptr()) }
            .to_string_lossy()
            .into_owned()
    };
    Some(format!("{} {}", field(&name.sysname), field(&name.release)))
}

#[cfg(not(target_os = "linux"))]
fn kernel() -> Option<String> {
    None
}

fn desktop() -> Option<String> {
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").ok()?;
    Some(match std::env::var("XDG_SESSION_TYPE") {
        Ok(session) if !session.is_empty() => format!("{desktop} ({session} session)"),
        _ => desktop,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credits_cover_the_dependency_tree() {
        let egui = &CRATES[crate_index("egui").unwrap()];
        assert!(egui.direct && egui.in_app && !egui.files.is_empty());
        // Proc-macros run only while building.
        assert!(!CRATES[crate_index("serde_derive").unwrap()].in_app);
        // Dev-dependencies aren't credited... unless something else needs them.
        assert!(CRATES.iter().all(|c| c.name != NAME));
        let unrar = &CRATES[crate_index("unrar_sys").unwrap()];
        assert!(
            unrar
                .files
                .iter()
                .any(|(name, _)| name.contains("unrar/license"))
        );
        assert!(unrar.files.iter().any(|(name, _)| name.starts_with("MIT")));
        let in_app_without_text = CRATES
            .iter()
            .filter(|c| c.in_app && c.files.is_empty())
            .map(|c| c.name)
            .collect::<Vec<_>>();
        assert!(in_app_without_text.is_empty(), "{in_app_without_text:?}");
    }

    #[test]
    fn every_page_draws() {
        let mut about = About {
            open: true,
            page: Page::About,
            search: String::new(),
            filter: Filter::All,
            selected: Selected::Crate(0),
            details_open: false,
            startup: Startup {
                windowing: "test",
                gpu: Vec::new(),
            },
            probed: Some(Arc::new(OnceLock::from(probe()))),
        };
        let ctx = Context::default();
        let pages = [Page::About, Page::Build, Page::System, Page::Credits];
        let selections = [
            Selected::Syntaxes,
            Selected::Crate(crate_index("egui").unwrap()),
        ];
        for page in pages {
            for selected in selections {
                about.page = page;
                about.selected = selected;
                for _ in 0..2 {
                    let mut output = ctx.run_ui(Default::default(), |ui| about.ui(ui.ctx()));
                    output.textures_delta.clear();
                }
            }
        }
        let report = about.report(&ctx, about.probed.as_ref().unwrap().get());
        assert!(report.contains("Commit") || report.contains("not built from a git checkout"));
        assert!(report.contains("[Graphics]"));
        println!("{report}");
    }
}
