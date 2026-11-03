//! "Open with": the apps registered for a file's type in a submenu, and a
//! dialog listing every app (plus, if enabled, custom commands).
//!
//! The platform part (`linux.rs`, `windows.rs`, `android.rs`) finds the apps and starts
//! them; the app list is loaded once on a background thread and reloaded
//! when the system's associations change.

mod dialog;

#[cfg(target_os = "linux")]
#[path = "linux.rs"]
mod platform;
#[cfg(windows)]
#[path = "windows.rs"]
mod platform;
#[cfg(target_os = "android")]
#[path = "android.rs"]
mod platform;
#[cfg(not(any(target_os = "linux", windows, target_os = "android")))]
#[path = "other.rs"]
mod platform;

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, mpsc},
    time::Duration,
};

use web_time::Instant;

use egui::{Atom, AtomExt, Button, Context, Response, RichText, TextureHandle, Ui, vec2};
use egui_phosphor::regular as icon;
use serde::{Deserialize, Serialize};

use crate::archive::ExtractJob;
use platform::Catalog;

pub use dialog::Dialog;

/// An installed app.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct App {
    /// The desktop file id on Linux, the handler's name on Windows.
    pub id: String,
    pub name: String,
    /// What kind of app it is, e.g. "Image Viewer".
    pub comment: String,
    /// An icon theme name or path on Linux, `path,index` on Windows.
    pub icon: Option<String>,
    /// Runs in a terminal.
    pub terminal: bool,
    pub mime_types: Vec<String>,
}

/// What to open files with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    App(App),
    /// A command line typed into the dialog.
    Command {
        command: String,
        terminal: bool,
    },
}

/// The apps that open some files.
#[derive(Default, Debug)]
pub struct Handlers {
    /// The app double-click uses, if all the files share one.
    pub default: Option<App>,
    /// The other apps that open every one of the files.
    pub others: Vec<App>,
    /// The files' types, for showing.
    pub types: Vec<String>,
}

/// A file to find apps for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Subject {
    pub path: PathBuf,
    /// Whether `path` exists, so its contents can be sniffed; archive
    /// entries only have a name.
    pub on_disk: bool,
}

/// A program to run in the terminal pane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Launch {
    pub program: String,
    pub args: Vec<String>,
    pub dir: Option<PathBuf>,
}

/// What is being opened: files, or a file still inside an archive, which
/// is extracted to a temporary folder first.
#[derive(Clone, Debug)]
pub enum Request {
    Files(Vec<PathBuf>),
    FromArchive(Box<ExtractJob>),
}

impl Request {
    pub fn subjects(&self) -> Vec<Subject> {
        match self {
            Request::Files(paths) => paths
                .iter()
                .map(|path| Subject {
                    path: path.clone(),
                    on_disk: true,
                })
                .collect(),
            Request::FromArchive(job) => job
                .selection
                .iter()
                .flat_map(|s| &s.paths)
                .map(|path| Subject {
                    path: PathBuf::from(path),
                    on_disk: false,
                })
                .collect(),
        }
    }
}

/// A custom command used before.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Custom {
    pub command: String,
    pub terminal: bool,
}

/// Saved settings.
#[derive(Default, Serialize, Deserialize)]
pub struct Prefs {
    /// The dialog offers a box for typing any command.
    #[serde(default)]
    pub custom_commands: bool,
    /// Custom commands used, most recent first.
    #[serde(default)]
    pub recent: Vec<Custom>,
}

/// Recent custom commands remembered.
const MAX_RECENT: usize = 12;

/// The "Open with" state shared by every tab.
#[derive(Default)]
pub struct OpenWith {
    pub prefs: Prefs,
    dialog: Option<Dialog>,
    error: Option<String>,
}

impl OpenWith {
    pub fn new(prefs: Prefs) -> Self {
        Self {
            prefs,
            ..Self::default()
        }
    }

    /// Shows the dialog for picking any app.
    pub fn ask(&mut self, request: Request) {
        self.dialog = Some(Dialog::new(request));
    }

    pub fn is_asking(&self) -> bool {
        self.dialog.is_some()
    }

    /// Draws the dialog and errors; returns what the dialog chose.
    pub fn ui(&mut self, ctx: &Context) -> Option<(Request, Target)> {
        let mut chosen = None;
        if let Some(dialog) = &mut self.dialog {
            match dialog.show(ctx, &mut self.prefs) {
                dialog::Outcome::Open => {}
                dialog::Outcome::Closed => self.dialog = None,
                dialog::Outcome::Chosen(target) => {
                    let request = self.dialog.take().map(Dialog::into_request);
                    chosen = request.map(|r| (r, target));
                }
            }
        }
        if let Some(error) = &self.error {
            let modal = egui::Modal::new(egui::Id::new("open-with-error")).show(ctx, |ui| {
                ui.set_max_width(crate::widgets::modal_width(ui, f32::INFINITY));
                ui.heading("Couldn't open");
                ui.label(error);
                ui.add_space(8.0);
                ui.button("OK").clicked()
            });
            if modal.inner || modal.should_close() {
                self.error = None;
            }
        }
        chosen
    }

    /// Opens `paths` with `target`, returning programs for the terminal
    /// pane. Errors are shown in a dialog.
    pub fn launch(&mut self, ctx: &Context, target: &Target, paths: &[PathBuf]) -> Vec<Launch> {
        if let Target::Command { command, terminal } = target {
            let custom = Custom {
                command: command.clone(),
                terminal: *terminal,
            };
            self.prefs.recent.retain(|c| c.command != custom.command);
            self.prefs.recent.insert(0, custom);
            self.prefs.recent.truncate(MAX_RECENT);
        }
        let result = match catalog(ctx) {
            Some(catalog) => catalog.launch(target, paths),
            None => Err(std::io::Error::other("the list of apps is still loading")),
        };
        result.unwrap_or_else(|err| {
            let name = match target {
                Target::App(app) => app.name.clone(),
                Target::Command { command, .. } => command.clone(),
            };
            self.error = Some(format!("Couldn't start {name}: {err}"));
            Vec::new()
        })
    }
}

/// Runs a custom command with the files appended.
#[cfg(not(target_os = "linux"))]
fn launch_command(target: &Target, paths: &[PathBuf]) -> std::io::Result<Vec<Launch>> {
    let Target::Command { command, terminal } = target else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "not supported here",
        ));
    };
    let mut words = command.split_whitespace().map(str::to_owned);
    let program = words
        .next()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "empty command"))?;
    let launch = Launch {
        program,
        args: words
            .chain(paths.iter().map(|p| p.to_string_lossy().into_owned()))
            .collect(),
        dir: paths.first().and_then(|p| p.parent()).map(PathBuf::from),
    };
    if *terminal {
        return Ok(vec![launch]);
    }
    let mut command = std::process::Command::new(&launch.program);
    command
        .args(&launch.args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if let Some(dir) = &launch.dir {
        command.current_dir(dir);
    }
    let mut child = command.spawn()?;
    std::thread::spawn(move || child.wait());
    Ok(Vec::new())
}

// ---------------------------------------------------------------------------
// The app catalog, loaded in the background

struct Global {
    catalog: Option<Arc<Catalog>>,
    loading: bool,
    checked: Option<Instant>,
    /// The handlers last asked for, which menus ask for every frame.
    cached: Option<(Vec<Subject>, Arc<Handlers>)>,
}

static GLOBAL: Mutex<Global> = Mutex::new(Global {
    catalog: None,
    loading: false,
    checked: None,
    cached: None,
});

/// How often to check whether the system's associations changed.
const STALE_CHECK: Duration = Duration::from_secs(2);

/// Selections bigger than this are typed by name only, without reading them.
const MAX_SNIFFED: usize = 200;

/// The catalog, or `None` while it's first loaded; starts loading it (and
/// reloads it once the system's apps or associations change).
pub fn catalog(ctx: &Context) -> Option<Arc<Catalog>> {
    let mut global = GLOBAL.lock().unwrap();
    let check = global.checked.is_none_or(|t| t.elapsed() > STALE_CHECK);
    if check {
        global.checked = Some(Instant::now());
    }
    let stale = check && global.catalog.as_ref().is_some_and(|c| c.is_stale());
    if (global.catalog.is_none() || stale) && !global.loading {
        global.loading = true;
        let ctx = ctx.clone();
        crate::background::spawn("open-with catalog", move || {
            let catalog = Arc::new(Catalog::load());
            let mut global = GLOBAL.lock().unwrap();
            global.catalog = Some(catalog);
            global.cached = None;
            global.loading = false;
            ctx.request_repaint();
        });
    }
    global.catalog.clone()
}

/// The apps for `subjects`, or `None` while the catalog loads.
pub fn handlers(ctx: &Context, subjects: &[Subject]) -> Option<Arc<Handlers>> {
    let catalog = catalog(ctx)?;
    if let Some((cached, handlers)) = &GLOBAL.lock().unwrap().cached
        && cached == subjects
    {
        return Some(handlers.clone());
    }
    let handlers = if subjects.len() > MAX_SNIFFED {
        let by_name: Vec<Subject> = subjects
            .iter()
            .map(|s| Subject {
                on_disk: false,
                ..s.clone()
            })
            .collect();
        catalog.handlers(&by_name)
    } else {
        catalog.handlers(subjects)
    };
    let handlers = Arc::new(handlers);
    GLOBAL.lock().unwrap().cached = Some((subjects.to_vec(), handlers.clone()));
    Some(handlers)
}

/// Makes `app` the default for files of `mime_type` (Linux).
#[cfg(target_os = "linux")]
pub fn set_default(ctx: &Context, app: &App, mime_type: &str) -> std::io::Result<()> {
    platform::set_default(app, mime_type)?;
    // Pick the change up right away instead of at the next stale check.
    GLOBAL.lock().unwrap().checked = None;
    catalog(ctx);
    Ok(())
}

/// Asks the system to change the default app for `path`'s type (Windows,
/// where only its own dialog may). Returns once the dialog closes.
#[cfg(windows)]
pub fn choose_default(path: &Path) -> std::io::Result<()> {
    platform::choose_default(path)?;
    GLOBAL.lock().unwrap().cached = None;
    Ok(())
}

/// Starts loading the catalog ahead of the first menu.
pub fn preload(ctx: &Context) {
    catalog(ctx);
}

// ---------------------------------------------------------------------------
// App icons, loaded in the background

/// Pixels per side icons are rendered at: crisp at 32 points on 2× screens.
const ICON_PIXELS: u32 = 64;

enum IconSlot {
    Loading,
    Ready(TextureHandle),
    Missing,
}

static ICONS: Mutex<Option<HashMap<String, IconSlot>>> = Mutex::new(None);
static ICON_QUEUE: OnceLock<Mutex<mpsc::Sender<(Context, String)>>> = OnceLock::new();

fn icon_texture(ctx: &Context, name: &str) -> Option<TextureHandle> {
    let mut icons = ICONS.lock().unwrap();
    let icons = icons.get_or_insert_with(HashMap::new);
    match icons.get(name) {
        Some(IconSlot::Ready(texture)) => return Some(texture.clone()),
        Some(IconSlot::Loading | IconSlot::Missing) => return None,
        None => {}
    }
    icons.insert(name.to_owned(), IconSlot::Loading);
    let queue = ICON_QUEUE.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<(Context, String)>();
        std::thread::Builder::new()
            .name("app icons".into())
            .spawn(move || {
                for (ctx, name) in rx {
                    let slot = match platform::icon_image(&name, ICON_PIXELS) {
                        Some(image) => IconSlot::Ready(ctx.load_texture(
                            format!("app-icon:{name}"),
                            image,
                            egui::TextureOptions::LINEAR,
                        )),
                        None => IconSlot::Missing,
                    };
                    if let Some(icons) = ICONS.lock().unwrap().as_mut() {
                        icons.insert(name, slot);
                    }
                    ctx.request_repaint();
                }
            })
            .expect("spawn thread");
        Mutex::new(tx)
    });
    let _ = queue.lock().unwrap().send((ctx.clone(), name.to_owned()));
    None
}

/// The app's icon `size` points square, or a generic one.
pub fn app_icon<'a>(ctx: &Context, app: Option<&App>, size: f32) -> Atom<'a> {
    let texture = app
        .and_then(|a| a.icon.as_deref())
        .and_then(|name| icon_texture(ctx, name));
    match texture {
        Some(texture) => egui::Image::from_texture((texture.id(), vec2(size, size))).into(),
        None => {
            let glyph = if app.is_some() {
                icon::APP_WINDOW
            } else {
                icon::TERMINAL_WINDOW
            };
            RichText::new(glyph)
                .size(size * 0.9)
                .atom_size(vec2(size, size))
        }
    }
}

/// A menu entry for an app.
fn app_button(ui: &mut Ui, app: &App, hint: &str) -> Response {
    let ctx = ui.ctx().clone();
    ui.add(
        Button::new((app_icon(&ctx, Some(app), 16.0), app.name.as_str()))
            .right_text(RichText::new(hint).weak()),
    )
    .on_hover_text(&app.comment)
}

/// What the "Open with" submenu chose.
pub enum Choice {
    Open(Target),
    /// Show the dialog.
    Other,
}

/// The "Open with" submenu for `paths` (called lazily, when it opens).
pub fn submenu(ui: &mut Ui, paths: impl FnOnce() -> Vec<PathBuf>) -> Option<Choice> {
    let mut choice = None;
    ui.menu_button(format!("{} Open with", icon::APP_WINDOW), |ui| {
        let subjects: Vec<Subject> = paths()
            .into_iter()
            .map(|path| Subject {
                path,
                on_disk: true,
            })
            .collect();
        choice = submenu_contents(ui, &subjects);
        if choice.is_some() {
            ui.close();
        }
    });
    choice
}

/// The submenu's items, for files that may not exist yet (archive entries).
pub fn submenu_for(ui: &mut Ui, subjects: &[Subject]) -> Option<Choice> {
    let mut choice = None;
    ui.menu_button(format!("{} Open with", icon::APP_WINDOW), |ui| {
        choice = submenu_contents(ui, subjects);
        if choice.is_some() {
            ui.close();
        }
    });
    choice
}

fn submenu_contents(ui: &mut Ui, subjects: &[Subject]) -> Option<Choice> {
    let Some(handlers) = handlers(ui.ctx(), subjects) else {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("Finding apps…");
        });
        return None;
    };
    let mut choice = None;
    if let Some(app) = &handlers.default
        && app_button(
            ui,
            app,
            if cfg!(target_os = "android") {
                ""
            } else {
                "default"
            },
        )
        .clicked()
    {
        choice = Some(Choice::Open(Target::App(app.clone())));
    }
    if handlers.default.is_some() && !handlers.others.is_empty() {
        ui.separator();
    }
    egui::ScrollArea::vertical()
        .scroll_source(crate::touch::scroll_source(ui.ctx()))
        .max_height(ui.ctx().content_rect().height() * 0.6)
        .show(ui, |ui| {
            for app in &handlers.others {
                if app_button(ui, app, "").clicked() {
                    choice = Some(Choice::Open(Target::App(app.clone())));
                }
            }
        });
    if handlers.default.is_none() && handlers.others.is_empty() {
        ui.label(RichText::new("No apps for this type").weak());
    }
    // Android lists its apps itself, and has no commands to type.
    if cfg!(target_os = "android") {
        return choice;
    }
    ui.separator();
    if ui
        .button(format!("{} Other application…", icon::DOTS_THREE))
        .clicked()
    {
        choice = Some(Choice::Other);
    }
    choice
}

/// A short description of what's being opened, e.g. "notes.txt" or "3 items".
fn describe(subjects: &[Subject]) -> String {
    match subjects {
        [one] => crate::fs::display_name(Path::new(&one.path)),
        many => format!("{} items", many.len()),
    }
}
