//! The properties UI. The same in a tab, a window and the preview pane:
//! a header naming the items, then the same sections in the same order,
//! with one label column width throughout so values line up.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime},
};

use chrono::{DateTime, Local, NaiveDateTime, TimeZone};
use egui::{
    Align, Button, Checkbox, CollapsingHeader, Color32, CornerRadius, Grid, Id, Key, Layout,
    ProgressBar, RichText, ScrollArea, Sense, TextEdit, Ui, vec2,
};
use egui_phosphor::regular as icon;

use super::{
    ArchivedItem, Source, TrashedItem,
    edit::{Applying, Bits, Failure, Op, Plan},
    elevate,
    hash::{self, Hashing},
    info::Item,
    media::{self, Probe},
    platform::{self, Volume},
    program::ProgramProbe,
    walk::{Counts, Walk},
};
use crate::{
    fs::{display_name, human_size},
    open_with,
    places::{self, Space},
    tab::{AppAction, Shared},
};

/// How often to look at the files again, for changes made elsewhere.
const REFRESH: Duration = Duration::from_secs(2);
/// Failures listed before "and N more".
const SHOWN_FAILURES: usize = 20;
/// Width of the label column in every section, so values line up.
const LABEL_WIDTH: f32 = 96.0;
const ROW_SPACING: [f32; 2] = [8.0, 6.0];

/// Unix permission bits, by row (owner, group, others) and column.
const PERMISSIONS: [(&str, [u32; 3]); 3] = [
    ("Owner", [0o400, 0o200, 0o100]),
    ("Group", [0o040, 0o020, 0o010]),
    ("Others", [0o004, 0o002, 0o001]),
];
const SPECIAL: [(&str, u32, &str); 3] = [
    ("Set user ID", 0o4000, "Runs as the file's owner"),
    (
        "Set group ID",
        0o2000,
        "Runs as the file's group; in folders, new files get the folder's group",
    ),
    (
        "Sticky",
        0o1000,
        "In folders, only owners can delete or rename their files",
    ),
];
/// Windows attributes that can be changed.
const ATTRIBUTES: [(&str, u32); 4] = [
    ("Read-only", 0x1),
    ("Hidden", 0x2),
    ("System", 0x4),
    ("Archive", 0x20),
];
/// Windows attributes that are only shown.
const OTHER_ATTRIBUTES: [(&str, u32); 6] = [
    ("compressed", 0x800),
    ("encrypted", 0x4000),
    ("offline", 0x1000),
    ("not indexed", 0x2000),
    ("sparse", 0x200),
    ("reparse point", 0x400),
];

const DATE_FORMAT: &str = "%Y-%m-%d %H:%M:%S";

/// Where the view is shown, and what that adds.
pub enum Host<'a> {
    Tab,
    /// The preview pane: the bulkier sections start closed, and the
    /// preview's own buttons go under the name.
    Preview {
        buttons: &'a mut dyn FnMut(&mut Ui),
    },
}

/// Changes not applied yet.
#[derive(Clone, Default, PartialEq)]
struct Edits {
    name: Option<String>,
    /// Permission bits for files, then folders. Without `recursive` both
    /// are the same, for every selected item.
    mode: [Bits; 2],
    attributes: [Bits; 2],
    uid: Option<u32>,
    gid: Option<u32>,
    account: Option<String>,
    modified: Option<SystemTime>,
    accessed: Option<SystemTime>,
    link_target: Option<String>,
    default_app: Option<open_with::App>,
}

impl Edits {
    fn is_empty(&self) -> bool {
        *self == Edits::default()
    }

    /// How many things are changed, for "3 changes not applied".
    fn count(&self) -> usize {
        [
            self.name.is_some(),
            self.mode.iter().any(|b| !b.is_empty()),
            self.attributes.iter().any(|b| !b.is_empty()),
            self.uid.is_some(),
            self.gid.is_some(),
            self.account.is_some(),
            self.modified.is_some(),
            self.accessed.is_some(),
            self.link_target.is_some(),
            self.default_app.is_some(),
        ]
        .into_iter()
        .filter(|&b| b)
        .count()
    }
}

/// Text being typed into a field, kept until it's committed.
#[derive(Default)]
struct Drafts {
    name: Option<String>,
    octal: Option<String>,
    modified: Option<String>,
    accessed: Option<String>,
    link: Option<String>,
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    account: Option<String>,
}

pub struct View {
    pub source: Source,
    /// In the preview pane.
    compact: bool,
    /// The preview's header button asked for the full view.
    pub open_full: bool,
    items: Vec<Item>,
    loaded: Option<Instant>,
    /// The items when editing started, to tell what changed on disk since.
    base: Option<Vec<Item>>,
    edits: Edits,
    drafts: Drafts,
    /// Permission and owner changes also go into folders.
    recursive: bool,
    walk: Option<(Vec<PathBuf>, Walk)>,
    hashing: Option<Hashing>,
    compare: String,
    media: Option<Probe>,
    program: Option<ProgramProbe>,
    volume: Option<(PathBuf, Option<Volume>, Option<Space>)>,
    acl: Option<(PathBuf, Vec<String>)>,
    applying: Option<Applying>,
    elevated: Option<elevate::Running>,
    /// Waiting for the system's "Open with" dialog (Windows).
    choosing_default: Option<Arc<Mutex<Option<String>>>>,
    failures: Vec<Failure>,
    error: Option<String>,
    /// The rename being applied, to follow it.
    renamed: Option<(PathBuf, PathBuf)>,
    /// A change was finished (a box ticked, Enter pressed): with immediate
    /// apply, apply it now.
    commit: bool,
}

impl View {
    pub fn new(source: Source, compact: bool) -> Self {
        Self {
            source,
            compact,
            open_full: false,
            items: Vec::new(),
            loaded: None,
            base: None,
            edits: Edits::default(),
            drafts: Drafts::default(),
            recursive: false,
            walk: None,
            hashing: None,
            compare: String::new(),
            media: None,
            program: None,
            volume: None,
            acl: None,
            applying: None,
            elevated: None,
            choosing_default: None,
            failures: Vec::new(),
            error: None,
            renamed: None,
            commit: false,
        }
    }

    fn paths(&self) -> &[PathBuf] {
        match &self.source {
            Source::Files(paths) => paths,
            _ => &[],
        }
    }

    fn busy(&self) -> bool {
        self.applying.is_some() || self.elevated.is_some()
    }

    fn editable(&self) -> bool {
        !self.busy() && !self.items.is_empty() && self.items.iter().all(|i| i.exists)
    }

    /// Looks at the files again, and starts what's read in the background.
    fn refresh(&mut self, ctx: &egui::Context) {
        let due = self.loaded.is_none_or(|t| t.elapsed() >= REFRESH);
        if !due || self.busy() {
            return;
        }
        self.loaded = Some(Instant::now());
        let items: Vec<Item> = self.paths().iter().map(|p| Item::load(p)).collect();
        let identity = |items: &[Item]| -> Vec<_> {
            items
                .iter()
                .map(|i| (i.path.clone(), i.size, i.modified))
                .collect()
        };
        if identity(&items) != identity(&self.items) {
            // Sums are of the contents as they were.
            self.hashing = None;
        }
        self.items = items;
        if self.edits.is_empty() {
            self.base = None;
        }
        let roots: Vec<PathBuf> = self
            .items
            .iter()
            .filter(|i| i.is_dir && !i.is_symlink)
            .map(|i| i.path.clone())
            .collect();
        if self.walk.as_ref().is_none_or(|(r, _)| *r != roots) {
            self.walk = (!roots.is_empty()).then(|| (roots.clone(), Walk::start(ctx, roots)));
        }
        if let [item] = self.items.as_slice() {
            if !item.is_dir
                && media::applies(&item.path)
                && self.media.as_ref().is_none_or(|m| m.path != item.path)
            {
                self.media = Some(Probe::start(ctx, item.path.clone()));
            }
            // Any file may be a program, whatever its name: the probe looks.
            if !item.is_dir && self.program.as_ref().is_none_or(|p| p.path != item.path) {
                self.program = Some(ProgramProbe::start(ctx, item.path.clone()));
            }
            if cfg!(windows) && self.acl.as_ref().is_none_or(|(p, _)| *p != item.path) {
                self.acl = Some((item.path.clone(), platform::acl(&item.path)));
            }
        }
        if let Some(first) = self.items.first() {
            let path = first.path.clone();
            if self.volume.as_ref().is_none_or(|(p, ..)| *p != path) {
                self.volume = Some((path.clone(), platform::volume(&path), places::space(&path)));
            } else if let Some((_, _, space)) = &mut self.volume {
                *space = places::space(&path);
            }
        }
    }

    /// Picks up finished background work.
    fn poll(&mut self, ctx: &egui::Context) {
        if let Some(failures) = self.applying.as_ref().and_then(Applying::take) {
            self.applying = None;
            self.finish(failures);
        }
        if let Some(outcome) = self.elevated.as_ref().and_then(elevate::Running::take) {
            self.elevated = None;
            match outcome {
                Ok(failures) => self.finish(failures),
                Err(error) => self.error = Some(error),
            }
        }
        let chosen = self
            .choosing_default
            .as_ref()
            .and_then(|cell| cell.lock().unwrap().take());
        if let Some(result) = chosen {
            if !result.is_empty() {
                self.error = Some(result);
            }
            self.choosing_default = None;
        }
        if self.busy() || self.choosing_default.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }

    /// After applying: follow renames, keep what failed, look again.
    fn finish(&mut self, failures: Vec<Failure>) {
        if let Some((from, to)) = self.renamed.take()
            && !failures.iter().any(|f| matches!(f.op, Op::Rename { .. }))
            && let Source::Files(paths) = &mut self.source
            && let Some(path) = paths.iter_mut().find(|p| **p == from)
        {
            *path = to;
        }
        self.failures = failures;
        self.loaded = None;
        self.base = None;
    }

    pub fn ui(&mut self, ui: &mut Ui, shared: &mut Shared, host: Host) {
        let ctx = ui.ctx().clone();
        self.poll(&ctx);
        self.refresh(&ctx);
        ctx.request_repaint_after(REFRESH);
        let immediate = shared.properties.prefs.apply_immediately;
        match host {
            Host::Preview { buttons } => {
                self.header_ui(ui);
                ui.add_space(4.0);
                buttons(ui);
                ui.add_space(6.0);
                self.body(ui, shared);
                self.footer(ui, &ctx, immediate);
            }
            Host::Tab => {
                ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
                    if self.footer_needed() {
                        ui.add_space(4.0);
                        self.footer(ui, &ctx, immediate);
                        ui.separator();
                    }
                    ui.with_layout(Layout::top_down(Align::Min), |ui| {
                        ScrollArea::vertical()
                            .id_salt("properties-body")
                            .auto_shrink(false)
                            .show(ui, |ui| {
                                ui.add_space(4.0);
                                self.header_ui(ui);
                                ui.add_space(8.0);
                                self.body(ui, shared);
                            });
                    });
                });
            }
        }
        if std::mem::take(&mut self.commit) && immediate && !self.edits.is_empty() {
            self.apply(&ctx);
        }
    }

    fn body(&mut self, ui: &mut Ui, shared: &mut Shared) {
        match self.source.clone() {
            Source::Files(_) => self.files_ui(ui, shared),
            Source::Trash(items) => trash_ui(ui, &items),
            Source::Archive { archive, entries } => archive_ui(ui, &archive, &entries),
        }
    }

    // -----------------------------------------------------------------------
    // Header

    /// An icon, the name (editable for one item) and a line about it.
    fn header_ui(&mut self, ui: &mut Ui) {
        let (glyph, summary) = self.summary();
        let editable = self.editable();
        let compact = self.compact;
        ui.horizontal(|ui| {
            if !compact {
                ui.label(RichText::new(glyph).size(30.0));
            }
            ui.vertical(|ui| {
                match self.items.as_slice() {
                    [item] if matches!(self.source, Source::Files(_)) => {
                        let (name, path) = (item.name.clone(), item.path.clone());
                        let shown = self.edits.name.clone().unwrap_or_else(|| name.clone());
                        let can_rename = path.parent().is_some_and(|p| p != path);
                        let width = ui.available_width() - if compact { 28.0 } else { 0.0 };
                        let row = ui.horizontal(|ui| {
                            let draft = self.drafts.name.get_or_insert(shown.clone());
                            let response = ui
                                .add_enabled(
                                    editable && can_rename,
                                    TextEdit::singleline(draft)
                                        .font(egui::TextStyle::Heading)
                                        .frame(egui::Frame::NONE)
                                        .desired_width(width.max(60.0)),
                                )
                                .on_hover_text("Click to rename");
                            // While typing the text is only a draft; it
                            // becomes an edit once the field is left.
                            if !response.has_focus() {
                                let typed = self.drafts.name.take().unwrap_or_default();
                                let typed = typed.trim().to_owned();
                                if typed != shown {
                                    let changed = !typed.is_empty() && typed != name;
                                    self.set_edit(|e| e.name = changed.then_some(typed));
                                }
                                if response.lost_focus() {
                                    self.commit = true;
                                }
                            }
                            if compact && open_full_button(ui) {
                                self.open_full = true;
                            }
                        });
                        // An underline, so it reads as editable without a box.
                        let rect = row.response.rect;
                        let stroke = ui.visuals().widgets.noninteractive.bg_stroke;
                        ui.painter()
                            .hline(rect.x_range(), rect.bottom() + 1.0, stroke);
                    }
                    _ => {
                        ui.horizontal(|ui| {
                            ui.heading(self.source.title());
                            if compact && open_full_button(ui) {
                                self.open_full = true;
                            }
                        });
                    }
                }
                ui.add_space(2.0);
                ui.weak(summary);
            });
        });
        if self.edits.name.is_some() && self.changed_on_disk(|i| i.name.clone()) {
            changed_marker(ui);
        }
        if self.items.iter().any(|i| !i.exists) && matches!(self.source, Source::Files(_)) {
            let missing = self.items.iter().filter(|i| !i.exists).count();
            let text = if self.items.len() == 1 {
                "This item no longer exists".to_owned()
            } else {
                format!("{missing} of these items no longer exist")
            };
            ui.colored_label(
                ui.visuals().warn_fg_color,
                format!("{} {text}", icon::WARNING),
            );
        }
    }

    /// The header's icon and one-line description.
    fn summary(&self) -> (&'static str, String) {
        let walked = self.walk.as_ref().map(|(_, w)| w.counts());
        match &self.source {
            Source::Files(_) => match self.items.as_slice() {
                [] => (icon::FILE, String::new()),
                [item] => {
                    let glyph = crate::fs::icon_for(&item.path, item.is_dir, item.is_symlink);
                    let mut parts = vec![item.kind()];
                    if let Some(counts) = walked {
                        parts.push(plural(counts.files + counts.dirs, "item", "items"));
                        parts.push(human_size(counts.bytes));
                    } else if !item.is_dir {
                        parts.push(human_size(item.size));
                    }
                    (glyph, parts.join(" · "))
                }
                items => {
                    let dirs = items.iter().filter(|i| i.is_dir).count() as u64;
                    let files = items.len() as u64 - dirs;
                    let bytes =
                        items.iter().map(|i| i.size).sum::<u64>() + walked.map_or(0, |c| c.bytes);
                    (
                        icon::COPY,
                        format!("{} · {}", count_words(files, dirs), human_size(bytes)),
                    )
                }
            },
            Source::Trash(items) => (
                icon::TRASH,
                format!(
                    "In the trash · {}",
                    plural(items.len() as u64, "item", "items")
                ),
            ),
            Source::Archive { archive, entries } => (
                icon::FILE_ZIP,
                format!(
                    "In {} · {}",
                    display_name(archive),
                    plural(entries.len() as u64, "item", "items")
                ),
            ),
        }
    }

    // -----------------------------------------------------------------------
    // Files on disk

    fn files_ui(&mut self, ui: &mut Ui, shared: &mut Shared) {
        if self.items.is_empty() {
            ui.spinner();
            return;
        }
        // The same sections everywhere; the preview opens fewer at first.
        let full = !self.compact;
        self.general_ui(ui, shared);
        self.media_ui(ui, full);
        self.program_ui(ui, full);
        self.dates_ui(ui);
        self.permissions_ui(ui, full);
        self.owner_ui(ui, full);
        self.checksums_ui(ui);
        self.filesystem_ui(ui);
    }

    fn general_ui(&mut self, ui: &mut Ui, shared: &mut Shared) {
        let ctx = ui.ctx().clone();
        let walked = self.walk.as_ref().map(|(_, w)| w.counts());
        let editable = self.editable();
        let handlers = self.handlers(&ctx);
        section(ui, "General", "properties-general", true, false, |ui| {
            grid(ui, "properties-general-grid", |ui| {
                let items = &self.items;
                row(ui, "Type", |ui| {
                    ui.label(common(items, Item::kind).unwrap_or_else(|| "Various".to_owned()));
                    if cfg!(target_os = "linux")
                        && let Some(handlers) = &handlers
                        && let [mime] = handlers.types.as_slice()
                    {
                        ui.weak(mime);
                    }
                });
                let parents: Vec<Option<&Path>> = items.iter().map(|i| i.path.parent()).collect();
                if let Some(Some(parent)) = parents.first()
                    && parents.iter().all(|p| p == &Some(*parent))
                {
                    row(ui, "Location", |ui| {
                        let response = ui
                            .add(egui::Link::new(parent.display().to_string()))
                            .on_hover_text("Show in a new tab");
                        if response.clicked()
                            && let Some(item) = items.first()
                        {
                            shared
                                .app_actions
                                .push(AppAction::Reveal(item.path.clone()));
                        }
                    });
                }
                let bytes =
                    items.iter().map(|i| i.size).sum::<u64>() + walked.map_or(0, |c| c.bytes);
                row(ui, "Size", |ui| {
                    ui.label(human_size(bytes));
                    if bytes >= 1024 {
                        ui.weak(format!("{} bytes", group_digits(bytes)));
                    }
                    if walked.is_some_and(|c| !c.done) {
                        ui.spinner();
                    }
                });
                let allocated: Option<u64> = items
                    .iter()
                    .map(|i| i.allocated)
                    .sum::<Option<u64>>()
                    .map(|a| a + walked.map_or(0, |c| c.allocated));
                // Only worth a row when it says something Size doesn't.
                if let Some(allocated) = allocated
                    && human_size(allocated) != human_size(bytes)
                {
                    row(ui, "On disk", |ui| {
                        ui.label(human_size(allocated));
                    });
                }
                if let Some(counts) = walked {
                    row(ui, "Contains", |ui| contains_label(ui, counts));
                }
                if let [item] = items.as_slice()
                    && item.is_symlink
                {
                    let target = item
                        .link_target
                        .as_ref()
                        .map(|t| t.display().to_string())
                        .unwrap_or_default();
                    let broken = item.broken;
                    row(ui, "Points to", |ui| {
                        ui.vertical(|ui| {
                            let draft = self.drafts.link.get_or_insert_with(|| {
                                self.edits.link_target.clone().unwrap_or(target.clone())
                            });
                            let response = ui.add_enabled(
                                editable,
                                TextEdit::singleline(draft).desired_width(f32::INFINITY),
                            );
                            if broken {
                                ui.colored_label(
                                    ui.visuals().warn_fg_color,
                                    format!("{} The target doesn't exist", icon::WARNING),
                                );
                            }
                            if !response.has_focus() {
                                let typed = self.drafts.link.take().unwrap_or_default();
                                let changed = !typed.is_empty() && typed != target;
                                let wanted = changed.then_some(typed);
                                if wanted != self.edits.link_target {
                                    self.set_edit(|e| e.link_target = wanted);
                                }
                                if response.lost_focus() {
                                    self.commit = true;
                                }
                            }
                        });
                    });
                }
                if let Some(handlers) = &handlers {
                    self.default_app_ui(ui, &ctx, handlers, editable);
                }
            });
        });
    }

    /// The apps for the selected files; `None` for folders, or while the
    /// list of apps loads.
    fn handlers(&self, ctx: &egui::Context) -> Option<Arc<open_with::Handlers>> {
        if self.items.is_empty() || self.items.iter().any(|i| i.is_dir || !i.exists) {
            return None;
        }
        let subjects: Vec<open_with::Subject> = self
            .items
            .iter()
            .map(|i| open_with::Subject {
                path: i.path.clone(),
                on_disk: true,
            })
            .collect();
        open_with::handlers(ctx, &subjects)
    }

    fn default_app_ui(
        &mut self,
        ui: &mut Ui,
        ctx: &egui::Context,
        handlers: &open_with::Handlers,
        editable: bool,
    ) {
        row(ui, "Opens with", |ui| {
            let current = self
                .edits
                .default_app
                .clone()
                .or_else(|| handlers.default.clone());
            let name = current
                .as_ref()
                .map_or("No app".to_owned(), |a| a.name.clone());
            #[cfg(target_os = "linux")]
            {
                let _ = ctx;
                let one_type = handlers.types.len() == 1;
                ui.add_enabled_ui(editable && one_type, |ui| {
                    egui::ComboBox::from_id_salt("properties-default-app")
                        .selected_text(name)
                        .width(ui.available_width().min(220.0))
                        .show_ui(ui, |ui| {
                            for app in handlers.default.iter().chain(&handlers.others) {
                                let selected = current.as_ref().is_some_and(|c| c.id == app.id);
                                if ui.selectable_label(selected, &app.name).clicked() && !selected {
                                    let app = app.clone();
                                    let is_default = handlers.default.as_ref() == Some(&app);
                                    self.set_edit(|e| e.default_app = (!is_default).then_some(app));
                                    self.commit = true;
                                }
                            }
                        })
                        .response
                        .on_disabled_hover_text("The items are of different types")
                        .on_hover_text(format!(
                            "The default app for every {} file",
                            handlers.types.first().map_or("", String::as_str)
                        ));
                });
            }
            #[cfg(windows)]
            {
                ui.label(name);
                let busy = self.choosing_default.is_some();
                if let [item] = self.items.as_slice()
                    && ui
                        .add_enabled(editable && !busy, Button::new("Change…").small())
                        .on_hover_text("Choose the default app for this type of file")
                        .clicked()
                {
                    let cell = Arc::new(Mutex::new(None));
                    let (c, path, ctx) = (cell.clone(), item.path.clone(), ctx.clone());
                    std::thread::spawn(move || {
                        let result = open_with::choose_default(&path)
                            .err()
                            .map(|e| e.to_string())
                            .unwrap_or_default();
                        *c.lock().unwrap() = Some(result);
                        ctx.request_repaint();
                    });
                    self.choosing_default = Some(cell);
                }
            }
            #[cfg(not(any(target_os = "linux", windows)))]
            {
                let _ = (ctx, editable);
                ui.label(name);
            }
        });
    }

    fn media_ui(&mut self, ui: &mut Ui, open: bool) {
        let Some(probe) = &self.media else { return };
        if self.items.len() != 1 {
            return;
        }
        let title = if probe.get().is_some_and(|m| m.av.is_none()) {
            "Image"
        } else {
            "Media"
        };
        section(ui, title, "properties-media", open, false, |ui| {
            let Some(media) = probe.get() else {
                ui.spinner();
                return;
            };
            grid(ui, "properties-media-grid", |ui| {
                if let Some([w, h]) = media.dimensions {
                    row(ui, "Dimensions", |ui| {
                        ui.label(format!("{w} × {h}"));
                        let megapixels = f64::from(w) * f64::from(h) / 1e6;
                        if megapixels >= 0.1 {
                            ui.weak(format!("{megapixels:.1} MP"));
                        }
                    });
                }
                if let Some(av) = &media.av {
                    if let Some(format) = &av.format {
                        row(ui, "Format", |ui| {
                            ui.label(format);
                        });
                    }
                    if let Some(duration) = av.duration {
                        row(ui, "Duration", |ui| {
                            ui.label(crate::video::format_time(duration));
                            if let Some(item) = self.items.first()
                                && duration > 0.0
                            {
                                let kbps = item.size as f64 * 8.0 / duration / 1000.0;
                                ui.weak(format!("{kbps:.0} kb/s"));
                            }
                        });
                    }
                    for (i, track) in av.tracks.iter().enumerate() {
                        row(ui, if i == 0 { "Streams" } else { "" }, |ui| {
                            ui.label(track);
                        });
                    }
                }
                for (label, value) in &media.camera {
                    row(ui, label, |ui| {
                        ui.label(value);
                    });
                }
                if let Some((lat, lon)) = media.location {
                    row(ui, "Location", |ui| {
                        let text = format!("{lat:.5}, {lon:.5}");
                        if ui
                            .add(egui::Link::new(text))
                            .on_hover_text("Show on OpenStreetMap")
                            .clicked()
                        {
                            let url = format!(
                                "https://www.openstreetmap.org/?mlat={lat}&mlon={lon}#map=16/{lat}/{lon}"
                            );
                            let _ = open::that_detached(url);
                        }
                    });
                }
                if let Some(error) = &media.error {
                    row(ui, "", |ui| {
                        ui.weak(error);
                    });
                }
            });
            if !media.exif_rest.is_empty() {
                CollapsingHeader::new(
                    RichText::new(format!("All tags ({})", media.exif_rest.len())).weak(),
                )
                .id_salt("properties-exif")
                .show(ui, |ui| {
                    grid(ui, "properties-exif-grid", |ui| {
                        for (tag, value) in &media.exif_rest {
                            row(ui, tag, |ui| {
                                ui.label(value);
                            });
                        }
                    });
                });
            }
        });
    }

    /// What a program or library is, and what it loads.
    fn program_ui(&mut self, ui: &mut Ui, open: bool) {
        if self.items.len() != 1 {
            return;
        }
        // Shown once read, and only for programs: most files aren't.
        let Some(Ok(Some(program))) = self.program.as_ref().and_then(ProgramProbe::get) else {
            return;
        };
        section(ui, "Program", "properties-program", open, false, |ui| {
            grid(ui, "properties-program-grid", |ui| {
                for (label, value) in &program.rows {
                    row(ui, label, |ui| {
                        ui.label(value);
                    });
                }
            });
            if !program.libraries.is_empty() {
                CollapsingHeader::new(
                    RichText::new(format!("Libraries ({})", program.libraries.len())).weak(),
                )
                .id_salt("properties-program-libraries")
                .show(ui, |ui| {
                    for library in &program.libraries {
                        ui.label(library);
                    }
                });
            }
        });
    }

    fn dates_ui(&mut self, ui: &mut Ui) {
        let editable = self.editable();
        let changed = (self.edits.modified.is_some() || self.edits.accessed.is_some())
            && self.changed_on_disk(|i| (i.modified, i.accessed));
        section(ui, "Dates", "properties-dates", true, changed, |ui| {
            grid(ui, "properties-dates-grid", |ui| {
                if let Some(Some(created)) = common(&self.items, |i| i.created) {
                    row(ui, "Created", |ui| {
                        ui.label(format_date(Some(created)));
                    });
                }
                let modified = common(&self.items, |i| i.modified);
                let mut committed = row(ui, "Modified", |ui| {
                    date_field(
                        ui,
                        "properties-modified",
                        modified,
                        &mut self.edits.modified,
                        &mut self.drafts.modified,
                        editable,
                    )
                });
                let accessed = common(&self.items, |i| i.accessed);
                committed |= row(ui, "Accessed", |ui| {
                    date_field(
                        ui,
                        "properties-accessed",
                        accessed,
                        &mut self.edits.accessed,
                        &mut self.drafts.accessed,
                        editable,
                    )
                });
                if committed {
                    self.base.get_or_insert_with(|| self.items.clone());
                    self.commit = true;
                }
                if let Some(Some(changed)) = common(&self.items, |i| i.changed) {
                    row(ui, "Changed", |ui| {
                        ui.label(format_date(Some(changed)))
                            .on_hover_text("When the name, permissions or owner last changed");
                    });
                }
            });
        });
    }

    fn any_dirs(&self) -> bool {
        self.items.iter().any(|i| i.is_dir && !i.is_symlink)
    }

    fn permissions_ui(&mut self, ui: &mut Ui, open: bool) {
        let unix = self.items.iter().all(|i| i.mode.is_some());
        let windows = self.items.iter().all(|i| i.attributes.is_some());
        if !unix && !windows {
            return;
        }
        let editable = self.editable();
        let pending = if unix {
            self.edits.mode
        } else {
            self.edits.attributes
        };
        let changed = pending.iter().any(|b| !b.is_empty())
            && self.changed_on_disk(|i| (i.mode, i.attributes));
        let title = if unix { "Permissions" } else { "Attributes" };
        section(ui, title, "properties-permissions", open, changed, |ui| {
            let value = |i: &Item| if unix { i.mode } else { i.attributes }.unwrap_or(0);
            // Columns: everything, or the selected files and folders apart
            // (what's inside them is unknown until applied).
            let kinds: &[(&str, usize)] = if self.recursive {
                &[("Files", 0), ("Folders", 1)]
            } else {
                &[("", 2)]
            };
            let mut clicked: Option<(usize, u32, bool)> = None;
            for &(label, kind) in kinds {
                let of_kind: Vec<u32> = self
                    .items
                    .iter()
                    .filter(|i| match kind {
                        0 => !i.is_dir,
                        1 => i.is_dir,
                        _ => true,
                    })
                    .map(value)
                    .collect();
                let bits = pending[if kind == 2 { 0 } else { kind }];
                let state = |bit: u32| -> Option<bool> {
                    bits.get(bit).or_else(|| tri_state(&of_kind, bit))
                };
                if !label.is_empty() {
                    ui.add_space(2.0);
                    ui.strong(label);
                }
                ui.add_enabled_ui(editable, |ui| {
                    if unix {
                        let folders = kind == 1;
                        Grid::new(("properties-mode-grid", kind))
                            .spacing(ROW_SPACING)
                            .show(ui, |ui| {
                                label_cell(ui, "");
                                ui.weak("Read");
                                ui.weak("Write");
                                ui.weak(if folders { "Open" } else { "Run" }).on_hover_text(
                                    if folders {
                                        "Execute on a folder: open it and reach what's inside"
                                    } else {
                                        "Execute: run the file as a program"
                                    },
                                );
                                ui.end_row();
                                for (who, row_bits) in PERMISSIONS {
                                    label_cell(ui, who);
                                    for bit in row_bits {
                                        if let Some(on) = tri_box(ui, "", state(bit), "") {
                                            clicked = Some((kind, bit, on));
                                        }
                                    }
                                    ui.end_row();
                                }
                            });
                        let any_special =
                            SPECIAL.iter().any(|(_, bit, _)| state(*bit) != Some(false));
                        CollapsingHeader::new(RichText::new("Special").weak())
                            .id_salt(("properties-special", kind))
                            .default_open(any_special)
                            .show(ui, |ui| {
                                ui.horizontal_wrapped(|ui| {
                                    for (name, bit, hint) in SPECIAL {
                                        if let Some(on) = tri_box(ui, name, state(bit), hint) {
                                            clicked = Some((kind, bit, on));
                                        }
                                    }
                                });
                            });
                    } else {
                        grid(ui, ("properties-attr-grid", kind), |ui| {
                            row(ui, "Attributes", |ui| {
                                for (name, bit) in ATTRIBUTES {
                                    if let Some(on) = tri_box(ui, name, state(bit), "") {
                                        clicked = Some((kind, bit, on));
                                    }
                                }
                            });
                        });
                    }
                });
            }
            if let Some((kind, bit, on)) = clicked {
                self.set_edit(|e| {
                    let target = if unix { &mut e.mode } else { &mut e.attributes };
                    match kind {
                        2 => {
                            target[0].put(bit, on);
                            target[1].put(bit, on);
                        }
                        k => target[k].put(bit, on),
                    }
                });
                self.commit = true;
            }
            grid(ui, "properties-permissions-extra", |ui| {
                if unix
                    && !self.recursive
                    && let [item] = self.items.as_slice()
                {
                    let (current, is_dir) = (item.mode.unwrap_or(0), item.is_dir);
                    let mode = self.edits.mode[0].apply(current);
                    row(ui, "Mode", |ui| {
                        let draft = self
                            .drafts
                            .octal
                            .get_or_insert_with(|| format!("{mode:04o}"));
                        let valid = u32::from_str_radix(draft.trim(), 8).is_ok_and(|m| m <= 0o7777);
                        let response = ui
                            .add_enabled(
                                editable,
                                TextEdit::singleline(draft)
                                    .desired_width(44.0)
                                    .font(egui::TextStyle::Monospace)
                                    .text_color_opt(
                                        (!valid).then_some(ui.visuals().error_fg_color),
                                    ),
                            )
                            .on_hover_text("As a number, the way chmod takes it");
                        if !response.has_focus() {
                            let typed = self.drafts.octal.take().unwrap_or_default();
                            if let Ok(new) = u32::from_str_radix(typed.trim(), 8)
                                && new <= 0o7777
                                && new != mode
                            {
                                let bits = Bits {
                                    set: new & !current,
                                    clear: !new & current & 0o7777,
                                };
                                self.set_edit(|e| e.mode = [bits, bits]);
                                self.commit = true;
                            }
                        }
                        ui.label(RichText::new(symbolic(mode, is_dir)).monospace().weak());
                    });
                }
                if windows {
                    let others: Vec<&str> = OTHER_ATTRIBUTES
                        .iter()
                        .filter(|(_, bit)| {
                            self.items
                                .iter()
                                .all(|i| i.attributes.unwrap_or(0) & bit != 0)
                        })
                        .map(|(name, _)| *name)
                        .collect();
                    if !others.is_empty() {
                        row(ui, "Also", |ui| {
                            ui.label(others.join(", "));
                        });
                    }
                }
            });
            if self.any_dirs() {
                let mut recursive = self.recursive;
                if ui
                    .add_enabled(
                        editable,
                        Checkbox::new(&mut recursive, "Also change everything inside"),
                    )
                    .on_hover_text(
                        "Apply permission and owner changes to the folders' contents \
                         too, with separate settings for files and folders",
                    )
                    .changed()
                {
                    self.recursive = recursive;
                    if !recursive {
                        // Back to one setting for everything: the files' one.
                        self.edits.mode[1] = self.edits.mode[0];
                        self.edits.attributes[1] = self.edits.attributes[0];
                    }
                }
            }
            if cfg!(windows)
                && self.items.len() == 1
                && let Some((_, acl)) = &self.acl
                && !acl.is_empty()
            {
                CollapsingHeader::new(RichText::new("Access").weak())
                    .id_salt("properties-acl")
                    .show(ui, |ui| {
                        for line in acl {
                            ui.label(line);
                        }
                        ui.weak("Change access in Explorer's Security tab");
                    });
            }
        });
    }

    fn owner_ui(&mut self, ui: &mut Ui, open: bool) {
        if self
            .items
            .iter()
            .all(|i| i.owner.is_none() && i.uid.is_none())
        {
            return;
        }
        let editable = self.editable();
        let pending =
            self.edits.uid.is_some() || self.edits.gid.is_some() || self.edits.account.is_some();
        let changed = pending && self.changed_on_disk(|i| (i.uid, i.gid, i.owner.clone()));
        section(ui, "Owner", "properties-owner", open, changed, |ui| {
            grid(ui, "properties-owner-grid", |ui| {
                #[cfg(target_os = "linux")]
                {
                    let uid = self.edits.uid.or(common(&self.items, |i| i.uid).flatten());
                    let gid = self.edits.gid.or(common(&self.items, |i| i.gid).flatten());
                    row(ui, "User", |ui| {
                        if let Some(id) =
                            id_combo(ui, "properties-user", platform::users(), uid, editable)
                        {
                            let unchanged = common(&self.items, |i| i.uid) == Some(Some(id));
                            self.set_edit(|e| e.uid = (!unchanged).then_some(id));
                            self.commit = true;
                        }
                    });
                    row(ui, "Group", |ui| {
                        if let Some(id) =
                            id_combo(ui, "properties-group", platform::groups(), gid, editable)
                        {
                            let unchanged = common(&self.items, |i| i.gid) == Some(Some(id));
                            self.set_edit(|e| e.gid = (!unchanged).then_some(id));
                            self.commit = true;
                        }
                    });
                }
                #[cfg(not(target_os = "linux"))]
                {
                    let owner = common(&self.items, |i| i.owner.clone()).flatten();
                    row(ui, "Owner", |ui| {
                        let shown = self
                            .edits
                            .account
                            .clone()
                            .or(owner.clone())
                            .unwrap_or_default();
                        let draft = self.drafts.account.get_or_insert(shown);
                        let response = ui
                            .add_enabled(
                                editable,
                                TextEdit::singleline(draft)
                                    .hint_text("Various")
                                    .desired_width(ui.available_width().min(220.0)),
                            )
                            .on_hover_text(
                                "An account name, like “Administrators” or “DOMAIN\\user”",
                            );
                        if !response.has_focus() {
                            let typed = self.drafts.account.take().unwrap_or_default();
                            let typed = typed.trim().to_owned();
                            let changed = !typed.is_empty() && Some(&typed) != owner.as_ref();
                            let wanted = changed.then_some(typed);
                            if wanted != self.edits.account {
                                self.set_edit(|e| e.account = wanted);
                            }
                            if response.lost_focus() {
                                self.commit = true;
                            }
                        }
                    });
                    if let Some(Some(group)) = common(&self.items, |i| i.group.clone()) {
                        row(ui, "Group", |ui| {
                            ui.label(group);
                        });
                    }
                }
            });
        });
    }

    fn checksums_ui(&mut self, ui: &mut Ui) {
        let [item] = self.items.as_slice() else {
            return;
        };
        if item.is_dir || !item.exists {
            return;
        }
        let (path, size) = (item.path.clone(), item.size);
        section(
            ui,
            "Checksums",
            "properties-checksums",
            false,
            false,
            |ui| {
                let Some(hashing) = &self.hashing else {
                    if ui
                        .button(format!("{} Calculate MD5 and SHA-256", icon::HASH))
                        .clicked()
                    {
                        self.hashing = Some(Hashing::start(ui.ctx(), path, size));
                    }
                    return;
                };
                match hashing.result() {
                    None => {
                        let share = hashing.done_bytes() as f32 / hashing.total.max(1) as f32;
                        ui.horizontal(|ui| {
                            ui.add(
                                ProgressBar::new(share)
                                    .show_percentage()
                                    .desired_width((ui.available_width() - 70.0).max(60.0)),
                            );
                            if ui.button("Cancel").clicked() {
                                self.hashing = None;
                            }
                        });
                    }
                    Some(Err(error)) => {
                        ui.colored_label(ui.visuals().error_fg_color, error);
                    }
                    Some(Ok(sums)) => {
                        grid(ui, "properties-sums", |ui| {
                            for (name, sum) in [("MD5", &sums.md5), ("SHA-256", &sums.sha256)] {
                                row(ui, name, |ui| {
                                    let text = RichText::new(sum).monospace().size(11.0);
                                    let response = ui
                                        .add(egui::Label::new(text).wrap().sense(Sense::click()))
                                        .on_hover_text("Click to copy");
                                    if response.clicked() {
                                        ui.ctx().copy_text(sum.clone());
                                    }
                                });
                            }
                            row(ui, "Compare", |ui| {
                                ui.vertical(|ui| {
                                    ui.add(
                                        TextEdit::singleline(&mut self.compare)
                                            .hint_text("Paste a checksum")
                                            .desired_width(f32::INFINITY),
                                    );
                                    if !self.compare.trim().is_empty() {
                                        match hash::matches(&sums, &self.compare) {
                                            Some(kind) => ui.colored_label(
                                                Color32::from_rgb(0x3f, 0xb9, 0x50),
                                                format!(
                                                    "{} Matches the {kind}",
                                                    icon::CHECK_CIRCLE
                                                ),
                                            ),
                                            None => ui.colored_label(
                                                ui.visuals().error_fg_color,
                                                format!("{} Doesn't match", icon::X_CIRCLE),
                                            ),
                                        };
                                    }
                                });
                            });
                        });
                    }
                }
            },
        );
    }

    fn filesystem_ui(&mut self, ui: &mut Ui) {
        let Some((_, volume, space)) = &self.volume else {
            return;
        };
        let single = self.items.first().filter(|_| self.items.len() == 1);
        section(
            ui,
            "Filesystem",
            "properties-filesystem",
            false,
            false,
            |ui| {
                grid(ui, "properties-filesystem-grid", |ui| {
                    if let Some(volume) = volume {
                        row(ui, "Mounted at", |ui| {
                            ui.label(volume.mount.display().to_string());
                        });
                        if !volume.fs_type.is_empty() {
                            row(ui, "Type", |ui| {
                                ui.label(&volume.fs_type);
                                if !volume.device.is_empty() {
                                    ui.weak(if cfg!(windows) {
                                        format!("“{}”", volume.device)
                                    } else {
                                        format!("on {}", volume.device)
                                    });
                                }
                            });
                        }
                    }
                    if let Some(space) = space {
                        row(ui, "Free", |ui| {
                            ui.vertical(|ui| {
                                ui.label(format!(
                                    "{} of {}",
                                    human_size(space.available),
                                    human_size(space.total)
                                ));
                                space_bar(ui, *space);
                            });
                        });
                    }
                    if let Some(item) = single {
                        if let Some(inode) = item.inode {
                            row(ui, "Inode", |ui| {
                                ui.label(inode.to_string());
                            });
                        }
                        if let Some(links) = item.links.filter(|_| !item.is_dir) {
                            row(ui, "Hard links", |ui| {
                                ui.label(links.to_string()).on_hover_text(
                                    "Names this file has; it's deleted with the last",
                                );
                            });
                        }
                    }
                });
            },
        );
    }

    // -----------------------------------------------------------------------
    // Applying

    /// Changes `edits`, remembering the items as they were before the first
    /// change.
    fn set_edit(&mut self, change: impl FnOnce(&mut Edits)) {
        let before = self.edits.clone();
        change(&mut self.edits);
        if self.edits != before && self.base.is_none() {
            self.base = Some(self.items.clone());
        }
    }

    /// Whether `field` changed on disk since editing started.
    fn changed_on_disk<T: PartialEq>(&self, field: impl Fn(&Item) -> T) -> bool {
        self.base.as_ref().is_some_and(|base| {
            base.len() != self.items.len()
                || base
                    .iter()
                    .zip(&self.items)
                    .any(|(a, b)| field(a) != field(b))
        })
    }

    fn plan(&self) -> Plan {
        let e = &self.edits;
        let mut ops = Vec::new();
        for item in &self.items {
            let path = item.path.clone();
            let recursive = self.recursive && item.is_dir;
            if e.mode.iter().any(|b| !b.is_empty()) {
                ops.push(Op::Mode {
                    path: path.clone(),
                    files: e.mode[0],
                    dirs: e.mode[1],
                    recursive,
                });
            }
            if e.attributes.iter().any(|b| !b.is_empty()) {
                ops.push(Op::Attributes {
                    path: path.clone(),
                    files: e.attributes[0],
                    dirs: e.attributes[1],
                    recursive,
                });
            }
            if e.uid.is_some() || e.gid.is_some() || e.account.is_some() {
                ops.push(Op::Owner {
                    path: path.clone(),
                    uid: e.uid,
                    gid: e.gid,
                    account: e.account.clone(),
                    recursive,
                });
            }
            if e.modified.is_some() || e.accessed.is_some() {
                ops.push(Op::Times {
                    path: path.clone(),
                    modified: e.modified,
                    accessed: e.accessed,
                });
            }
            if let Some(target) = &e.link_target {
                ops.push(Op::Symlink {
                    path: path.clone(),
                    target: PathBuf::from(target),
                });
            }
        }
        // Last, as everything else names the old path.
        if let (Some(name), [item]) = (&e.name, self.items.as_slice())
            && let Some(parent) = item.path.parent()
        {
            ops.push(Op::Rename {
                from: item.path.clone(),
                to: parent.join(name),
            });
        }
        Plan { ops }
    }

    fn apply(&mut self, ctx: &egui::Context) {
        self.error = None;
        if let Some(name) = &self.edits.name
            && let Some(problem) = bad_name(name)
        {
            self.error = Some(problem.to_owned());
            return;
        }
        #[cfg(target_os = "linux")]
        if let Some(app) = self.edits.default_app.take()
            && let Some(mime) = self.handlers(ctx).and_then(|h| h.types.first().cloned())
            && let Err(error) = open_with::set_default(ctx, &app, &mime)
        {
            self.error = Some(format!("Couldn't change the default app: {error}"));
        }
        self.edits.default_app = None;
        let plan = self.plan();
        self.renamed = plan.ops.iter().find_map(|op| match op {
            Op::Rename { from, to } => Some((from.clone(), to.clone())),
            _ => None,
        });
        self.edits = Edits::default();
        self.drafts = Drafts::default();
        if plan.ops.is_empty() {
            self.finish(Vec::new());
        } else {
            self.applying = Some(Applying::start(ctx, plan));
        }
    }

    fn retry_elevated(&mut self, ctx: &egui::Context) {
        let ops: Vec<Op> = self
            .failures
            .iter()
            .filter(|f| f.denied)
            .map(|f| f.op.clone())
            .collect();
        self.renamed = ops.iter().find_map(|op| match op {
            Op::Rename { from, to } => Some((from.clone(), to.clone())),
            _ => None,
        });
        self.failures.retain(|f| !f.denied);
        self.error = None;
        self.elevated = Some(elevate::Running::start(ctx, Plan { ops }));
    }

    fn revert(&mut self) {
        self.edits = Edits::default();
        self.drafts = Drafts::default();
        self.base = None;
        self.error = None;
    }

    /// Whether there's anything for the footer to show.
    fn footer_needed(&self) -> bool {
        !self.edits.is_empty() || self.busy() || self.error.is_some() || !self.failures.is_empty()
    }

    fn footer(&mut self, ui: &mut Ui, ctx: &egui::Context, immediate: bool) {
        if !matches!(self.source, Source::Files(_)) {
            return;
        }
        // In a tab the footer is laid out bottom-up, so there the buttons go
        // first and the messages above them.
        if self.compact {
            self.messages_ui(ui, ctx);
            self.buttons_ui(ui, ctx, immediate);
        } else {
            self.buttons_ui(ui, ctx, immediate);
            self.messages_ui(ui, ctx);
        }
    }

    fn messages_ui(&mut self, ui: &mut Ui, ctx: &egui::Context) {
        if let Some(error) = &self.error {
            ui.colored_label(
                ui.visuals().error_fg_color,
                format!("{} {error}", icon::WARNING),
            );
        }
        if !self.failures.is_empty() {
            self.failures_ui(ui, ctx);
        }
    }

    fn buttons_ui(&mut self, ui: &mut Ui, ctx: &egui::Context, immediate: bool) {
        if self.busy() {
            ui.horizontal(|ui| {
                ui.spinner();
                if let Some(applying) = &self.applying {
                    let done = applying
                        .progress
                        .done
                        .load(std::sync::atomic::Ordering::Relaxed);
                    ui.label(format!("Applying… {done} done"));
                    if ui.button("Stop").clicked() {
                        applying
                            .progress
                            .cancel
                            .store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                } else {
                    ui.label("Waiting for authorization…");
                }
            });
            return;
        }
        let pending = if immediate { 0 } else { self.edits.count() };
        if pending == 0 {
            return;
        }
        ui.horizontal(|ui| {
            if pending > 0 {
                ui.weak(plural(pending as u64, "change", "changes") + " not applied");
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let apply = Button::new(format!("{} Apply", icon::CHECK))
                    .fill(ui.visuals().selection.bg_fill);
                if ui.add(apply).clicked() {
                    self.apply(ctx);
                }
                if ui
                    .button("Revert")
                    .on_hover_text("Forget the changes not applied yet")
                    .clicked()
                {
                    self.revert();
                }
            });
        });
    }

    fn failures_ui(&mut self, ui: &mut Ui, ctx: &egui::Context) {
        let n = self.failures.len();
        let denied = self.failures.iter().any(|f| f.denied);
        let mut retry = false;
        let mut dismiss = false;
        ui.horizontal_wrapped(|ui| {
            ui.colored_label(
                ui.visuals().error_fg_color,
                format!(
                    "{} {} failed",
                    icon::WARNING,
                    plural(n as u64, "change", "changes")
                ),
            );
            if denied
                && elevate::available()
                && !self.busy()
                && ui
                    .button(format!("{} {}", icon::SHIELD_CHECK, elevate::label()))
                    .on_hover_text("Ask for the password and try the failed changes again")
                    .clicked()
            {
                retry = true;
            }
            if ui.small_button("Dismiss").clicked() {
                dismiss = true;
            }
        });
        CollapsingHeader::new(RichText::new("Details").weak())
            .id_salt("properties-failures")
            .default_open(n <= 3)
            .show(ui, |ui| {
                for failure in self.failures.iter().take(SHOWN_FAILURES) {
                    ui.add(egui::Label::new(RichText::new(&failure.error).small()).wrap());
                }
                if n > SHOWN_FAILURES {
                    ui.weak(format!("and {} more", n - SHOWN_FAILURES));
                }
            });
        if retry {
            self.retry_elevated(ctx);
        }
        if dismiss {
            self.failures.clear();
        }
    }
}

fn open_full_button(ui: &mut Ui) -> bool {
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        ui.add(Button::new(icon::ARROWS_OUT_SIMPLE).frame(false))
            .on_hover_text(crate::keymap::hint(
                ui.ctx(),
                "Open in its own tab or window",
                crate::keymap::Command::Properties,
            ))
            .clicked()
    })
    .inner
}

// ---------------------------------------------------------------------------
// Trash and archive entries (read-only)

fn kind_of_name(name: &str, is_dir: bool) -> String {
    if is_dir {
        return "Folder".to_owned();
    }
    Item {
        path: PathBuf::from(name),
        name: name.to_owned(),
        exists: true,
        ..Item::default()
    }
    .kind()
}

fn trash_ui(ui: &mut Ui, items: &[TrashedItem]) {
    section(ui, "General", "properties-trash", true, false, |ui| {
        grid(ui, "properties-trash-grid", |ui| {
            if let [item] = items {
                row(ui, "Type", |ui| {
                    ui.label(kind_of_name(&item.name, item.is_dir));
                });
                row(ui, "Was in", |ui| {
                    let parent = item.original.parent().unwrap_or(&item.original);
                    ui.label(parent.display().to_string());
                });
                row(ui, "Deleted", |ui| {
                    ui.label(format_date(item.deleted));
                });
            }
            if items.iter().any(|i| i.size.is_some()) {
                let bytes: u64 = items.iter().filter_map(|i| i.size).sum();
                row(ui, "Size", |ui| {
                    ui.label(human_size(bytes));
                    if bytes >= 1024 {
                        ui.weak(format!("{} bytes", group_digits(bytes)));
                    }
                });
            }
            if items.iter().any(|i| i.entries.is_some()) {
                let entries: u64 = items.iter().filter_map(|i| i.entries).sum();
                row(ui, "Contains", |ui| {
                    ui.label(plural(entries, "item", "items"));
                });
            }
        });
        ui.add_space(4.0);
        ui.weak("Restore items to change them.");
    });
}

fn archive_ui(ui: &mut Ui, archive: &Path, entries: &[ArchivedItem]) {
    section(ui, "General", "properties-archive", true, false, |ui| {
        grid(ui, "properties-archive-grid", |ui| {
            if let [entry] = entries {
                let name = entry.path.rsplit('/').next().unwrap_or(&entry.path);
                row(ui, "Type", |ui| {
                    ui.label(kind_of_name(name, entry.is_dir));
                });
                row(ui, "Location", |ui| {
                    let folder = entry.path.rsplit_once('/').map_or("", |(dir, _)| dir);
                    ui.label(format!("{}/{folder}", display_name(archive)));
                });
                if let Some(modified) = entry.modified {
                    row(ui, "Modified", |ui| {
                        ui.label(format_date(Some(modified)));
                    });
                }
            }
            let size: u64 = entries.iter().map(|e| e.size).sum();
            row(ui, "Size", |ui| {
                ui.label(human_size(size));
                if size >= 1024 {
                    ui.weak(format!("{} bytes", group_digits(size)));
                }
            });
            if entries.iter().all(|e| e.packed.is_some()) {
                let packed: u64 = entries.iter().filter_map(|e| e.packed).sum();
                row(ui, "Compressed", |ui| {
                    ui.label(human_size(packed));
                    if size > 0 {
                        ui.weak(format!(
                            "{:.0}% of the size",
                            packed as f64 * 100.0 / size as f64
                        ));
                    }
                });
            }
            if entries.iter().any(|e| e.encrypted) {
                row(ui, "Encrypted", |ui| {
                    ui.label(format!("{} Yes", icon::LOCK));
                });
            }
        });
        ui.add_space(4.0);
        ui.weak("Extract items to change them.");
    });
}

// ---------------------------------------------------------------------------
// Pieces

/// A collapsible section; `changed` marks edits that changed on disk since.
fn section(
    ui: &mut Ui,
    title: &str,
    id: &str,
    open: bool,
    changed: bool,
    add: impl FnOnce(&mut Ui),
) {
    let text = if changed {
        RichText::new(format!("{title}  {}", icon::WARNING))
            .strong()
            .color(ui.visuals().warn_fg_color)
    } else {
        RichText::new(title).strong()
    };
    let response = CollapsingHeader::new(text)
        .id_salt(id)
        .default_open(open)
        .show(ui, |ui| {
            if changed {
                changed_marker(ui);
            }
            add(ui);
            ui.add_space(4.0);
        });
    if changed {
        response.header_response.on_hover_text(
            "Changed on disk since you started editing; applying keeps your changes",
        );
    }
}

fn changed_marker(ui: &mut Ui) {
    ui.colored_label(
        ui.visuals().warn_fg_color,
        format!(
            "{} Changed on disk since you started editing",
            icon::WARNING
        ),
    );
}

/// A two-column grid of label and value rows.
fn grid(ui: &mut Ui, id: impl egui::AsIdSalt, add: impl FnOnce(&mut Ui)) {
    Grid::new(id)
        .num_columns(2)
        .spacing(ROW_SPACING)
        .show(ui, |ui| add(ui));
}

/// A label in the label column, which is the same width everywhere.
fn label_cell(ui: &mut Ui, text: &str) {
    let height = ui.spacing().interact_size.y;
    ui.allocate_ui_with_layout(
        vec2(LABEL_WIDTH, height),
        Layout::left_to_right(Align::Center),
        |ui| {
            ui.set_min_width(LABEL_WIDTH);
            ui.add(egui::Label::new(RichText::new(text).weak()).truncate());
        },
    );
}

/// A row of a [`grid`]: the label, then whatever `value` adds side by side,
/// wrapping when the column is narrow.
fn row<R>(ui: &mut Ui, label: &str, value: impl FnOnce(&mut Ui) -> R) -> R {
    label_cell(ui, label);
    let inner = ui
        .horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            value(ui)
        })
        .inner;
    ui.end_row();
    inner
}

/// The value all items share, or `None` if they differ.
fn common<T: PartialEq>(items: &[Item], field: impl Fn(&Item) -> T) -> Option<T> {
    let mut values = items.iter().map(&field);
    let first = values.next()?;
    values.all(|v| v == first).then_some(first)
}

/// Whether `bit` is set in all values, none, or some (`None`).
fn tri_state(values: &[u32], bit: u32) -> Option<bool> {
    let set = values.iter().filter(|v| *v & bit != 0).count();
    match set {
        _ if values.is_empty() => None,
        0 => Some(false),
        n if n == values.len() => Some(true),
        _ => None,
    }
}

/// A checkbox that may be neither ticked nor not; returns the new value
/// when clicked.
fn tri_box(ui: &mut Ui, label: &str, state: Option<bool>, hint: &str) -> Option<bool> {
    let mut checked = state.unwrap_or(false);
    let mut response = ui.add(Checkbox::new(&mut checked, label).indeterminate(state.is_none()));
    if state.is_none() {
        response = response.on_hover_text("Some are, some aren't");
    }
    if !hint.is_empty() {
        response = response.on_hover_text(hint);
    }
    response
        .changed()
        .then(|| if state.is_none() { true } else { checked })
}

#[cfg(target_os = "linux")]
fn id_combo(
    ui: &mut Ui,
    id: &str,
    names: &[(u32, String)],
    current: Option<u32>,
    editable: bool,
) -> Option<u32> {
    let name_of = |id: u32| {
        names
            .iter()
            .find(|(i, _)| *i == id)
            .map_or_else(|| id.to_string(), |(_, n)| n.clone())
    };
    let text = current.map_or_else(|| "Various".to_owned(), name_of);
    let mut chosen = None;
    ui.add_enabled_ui(editable, |ui| {
        egui::ComboBox::from_id_salt(id)
            .selected_text(text)
            .width(ui.available_width().min(180.0))
            .height(320.0)
            .show_ui(ui, |ui| {
                for (i, name) in names {
                    let label = format!("{name}  ({i})");
                    if ui.selectable_label(current == Some(*i), label).clicked() {
                        chosen = Some(*i);
                    }
                }
            })
            .response
            .on_hover_text(current.map_or_else(String::new, |id| format!("ID {id}")));
    });
    chosen
}

/// A date, shown like every other date, with a button to change it.
/// Returns true when a new date was committed.
fn date_field(
    ui: &mut Ui,
    id: &str,
    current: Option<Option<SystemTime>>,
    pending: &mut Option<SystemTime>,
    draft: &mut Option<String>,
    editable: bool,
) -> bool {
    let id = Id::new(id);
    match draft {
        None => {
            let shown = pending.or(current.flatten());
            let text = match (shown, current) {
                (Some(time), _) => format_date(Some(time)),
                (None, None) => "Various".to_owned(),
                (None, Some(_)) => "Unknown".to_owned(),
            };
            if pending.is_some() {
                ui.label(RichText::new(text).strong())
                    .on_hover_text(format!("Was {}", format_date(current.flatten())));
            } else {
                ui.label(text);
            }
            if editable
                && ui
                    .add(Button::new(icon::PENCIL_SIMPLE).frame(false).small())
                    .on_hover_text("Change")
                    .clicked()
            {
                *draft = Some(shown.map(format_editable).unwrap_or_default());
                ui.memory_mut(|m| m.request_focus(id));
            }
            false
        }
        Some(text) => {
            let parsed = parse_date(text);
            let response = ui.add(
                TextEdit::singleline(text)
                    .id(id)
                    .hint_text("YYYY-MM-DD HH:MM:SS")
                    .desired_width(150.0)
                    .text_color_opt(parsed.is_none().then_some(ui.visuals().error_fg_color)),
            );
            let escape = ui.input(|i| i.key_pressed(Key::Escape));
            if ui
                .add(Button::new("Now").small())
                .on_hover_text("The current time")
                .clicked()
            {
                *pending = Some(SystemTime::now());
                *draft = None;
                return true;
            }
            if response.lost_focus() {
                if escape {
                    *draft = None;
                } else if let Some(time) = parsed {
                    *draft = None;
                    if Some(time) != current.flatten() {
                        *pending = Some(time);
                        return true;
                    }
                }
            }
            false
        }
    }
}

fn format_date(time: Option<SystemTime>) -> String {
    match time {
        Some(time) => crate::fs::format_time(Some(time)),
        None => "Various".to_owned(),
    }
}

fn format_editable(time: SystemTime) -> String {
    DateTime::<Local>::from(time)
        .format(DATE_FORMAT)
        .to_string()
}

fn parse_date(text: &str) -> Option<SystemTime> {
    let text = text.trim();
    let naive = NaiveDateTime::parse_from_str(text, DATE_FORMAT)
        .or_else(|_| NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M"))
        .or_else(|_| {
            chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d")
                .map(|d| d.and_hms_opt(0, 0, 0).expect("midnight exists"))
        })
        .ok()?;
    Local
        .from_local_datetime(&naive)
        .earliest()
        .map(SystemTime::from)
}

/// How full a drive is, as a bar.
fn space_bar(ui: &mut Ui, space: Space) {
    let width = ui.available_width().clamp(80.0, 240.0);
    let (rect, _) = ui.allocate_exact_size(vec2(width, 6.0), Sense::hover());
    let visuals = ui.visuals();
    let used = space.used_share();
    let fill = if used > 0.9 {
        visuals.warn_fg_color
    } else {
        visuals.selection.bg_fill
    };
    let radius = CornerRadius::same(3);
    ui.painter()
        .rect_filled(rect, radius, visuals.extreme_bg_color);
    let mut filled = rect;
    filled.set_width(rect.width() * used);
    ui.painter().rect_filled(filled, radius, fill);
}

fn contains_label(ui: &mut Ui, counts: Counts) {
    ui.label(count_words(counts.files, counts.dirs));
    if counts.unreadable > 0 {
        ui.colored_label(ui.visuals().warn_fg_color, icon::WARNING)
            .on_hover_text(format!(
                "{} couldn't be read, so the totals may be low",
                plural(counts.unreadable, "folder", "folders")
            ));
    }
}

fn plural(n: u64, one: &str, many: &str) -> String {
    format!("{} {}", group_digits(n), if n == 1 { one } else { many })
}

fn count_words(files: u64, dirs: u64) -> String {
    format!(
        "{}, {}",
        plural(files, "file", "files"),
        plural(dirs, "folder", "folders")
    )
}

/// 1234567 as "1,234,567".
fn group_digits(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `rwxr-xr-x`, like `ls -l`.
fn symbolic(mode: u32, is_dir: bool) -> String {
    let mut out = String::from(if is_dir { "d" } else { "-" });
    for (shift, special, special_char) in [(6, 0o4000, 's'), (3, 0o2000, 's'), (0, 0o1000, 't')] {
        let bits = (mode >> shift) & 0o7;
        out.push(if bits & 4 != 0 { 'r' } else { '-' });
        out.push(if bits & 2 != 0 { 'w' } else { '-' });
        let exec = bits & 1 != 0;
        out.push(match (mode & special != 0, exec) {
            (true, true) => special_char,
            (true, false) => special_char.to_ascii_uppercase(),
            (false, true) => 'x',
            (false, false) => '-',
        });
    }
    out
}

/// Why `name` can't be a file name, if it can't.
fn bad_name(name: &str) -> Option<&'static str> {
    if name.is_empty() || name == "." || name == ".." {
        Some("That name isn't allowed")
    } else if name.contains('/')
        || (cfg!(windows) && name.contains(['\\', ':', '*', '?', '"', '<', '>', '|']))
    {
        Some("Names can't contain / or (on Windows) \\ : * ? \" < > |")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_modes_like_ls() {
        assert_eq!(symbolic(0o755, true), "drwxr-xr-x");
        assert_eq!(symbolic(0o4755, false), "-rwsr-xr-x");
        assert_eq!(symbolic(0o1777, true), "drwxrwxrwt");
        assert_eq!(symbolic(0o2640, false), "-rw-r-S---");
    }

    #[test]
    fn groups_digits() {
        assert_eq!(group_digits(0), "0");
        assert_eq!(group_digits(999), "999");
        assert_eq!(group_digits(1000), "1,000");
        assert_eq!(group_digits(1234567), "1,234,567");
    }

    #[test]
    fn tri_states() {
        assert_eq!(tri_state(&[0o644, 0o600], 0o400), Some(true));
        assert_eq!(tri_state(&[0o644, 0o600], 0o004), None);
        assert_eq!(tri_state(&[0o600], 0o004), Some(false));
        assert_eq!(tri_state(&[], 0o004), None);
    }

    #[test]
    fn parses_dates_back() {
        let time = parse_date("2024-02-29 13:45:10").unwrap();
        assert_eq!(format_editable(time), "2024-02-29 13:45:10");
        assert!(parse_date("2024-02-30 00:00:00").is_none());
        assert!(parse_date("2024-03-01").is_some());
    }

    #[test]
    fn rejects_bad_names() {
        assert!(bad_name("a/b").is_some());
        assert!(bad_name("..").is_some());
        assert!(bad_name("fine.txt").is_none());
    }

    #[test]
    fn counts_edits() {
        let mut edits = Edits::default();
        assert_eq!(edits.count(), 0);
        edits.name = Some("x".into());
        edits.mode[0].put(0o100, true);
        assert_eq!(edits.count(), 2);
    }
}
