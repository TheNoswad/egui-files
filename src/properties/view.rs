//! The properties UI, the same in a tab, a window and the preview pane.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime},
};

use chrono::{DateTime, Local, NaiveDateTime, TimeZone};
use egui::{
    Align, Button, Checkbox, CollapsingHeader, Color32, Grid, Layout, ProgressBar, RichText,
    ScrollArea, TextEdit, Ui,
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
    /// In the preview pane: fewer sections, no scroll area of its own.
    compact: bool,
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
            if !self.compact
                && !item.is_dir
                && media::applies(&item.path)
                && self.media.as_ref().is_none_or(|m| m.path != item.path)
            {
                self.media = Some(Probe::start(ctx, item.path.clone()));
            }
            if cfg!(windows) && self.acl.as_ref().is_none_or(|(p, _)| *p != item.path) {
                self.acl = Some((item.path.clone(), platform::acl(&item.path)));
            }
        }
        if let Some(first) = self.items.first() {
            // Of the folder an item's in for files, so it works for the
            // items that are mount points too.
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

    pub fn ui(&mut self, ui: &mut Ui, shared: &mut Shared, close: Option<&mut bool>) {
        let ctx = ui.ctx().clone();
        self.poll(&ctx);
        self.refresh(&ctx);
        ctx.request_repaint_after(REFRESH);
        let immediate = shared.properties.prefs.apply_immediately;
        if self.compact {
            self.body(ui, shared);
            self.footer(ui, &ctx, immediate, None);
        } else {
            ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
                ui.add_space(4.0);
                self.footer(ui, &ctx, immediate, close);
                ui.separator();
                ui.with_layout(Layout::top_down(Align::Min), |ui| {
                    ScrollArea::vertical()
                        .id_salt("properties-body")
                        .auto_shrink(false)
                        .show(ui, |ui| self.body(ui, shared));
                });
            });
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
    // Files on disk

    fn files_ui(&mut self, ui: &mut Ui, shared: &mut Shared) {
        if self.items.is_empty() {
            ui.spinner();
            return;
        }
        if self.items.iter().any(|i| !i.exists) {
            let missing = self.items.iter().filter(|i| !i.exists).count();
            let text = if self.items.len() == 1 {
                "This item no longer exists.".to_owned()
            } else {
                format!("{missing} of these items no longer exist.")
            };
            ui.colored_label(
                ui.visuals().warn_fg_color,
                format!("{} {text}", icon::WARNING),
            );
        }
        if !self.compact {
            self.header_ui(ui);
            ui.add_space(4.0);
        }
        self.general_ui(ui, shared);
        self.dates_ui(ui);
        self.permissions_ui(ui);
        self.owner_ui(ui);
        if !self.compact {
            self.media_ui(ui);
            self.checksums_ui(ui);
            self.filesystem_ui(ui);
        }
    }

    fn header_ui(&mut self, ui: &mut Ui) {
        let editable = self.editable();
        ui.horizontal(|ui| {
            let glyph = match self.items.as_slice() {
                [item] => crate::fs::icon_for(&item.path, item.is_dir, item.is_symlink),
                _ => icon::COPY,
            };
            ui.label(RichText::new(glyph).size(32.0));
            match self.items.as_slice() {
                [item] => {
                    let (name, path) = (item.name.clone(), item.path.clone());
                    let shown = self.edits.name.clone().unwrap_or_else(|| name.clone());
                    let draft = self.drafts.name.get_or_insert(shown.clone());
                    let has_parent = path.parent().is_some_and(|p| p != path);
                    let response = ui
                        .add_enabled(
                            editable && has_parent,
                            TextEdit::singleline(draft)
                                .font(egui::TextStyle::Heading)
                                .desired_width(f32::INFINITY),
                        )
                        .on_hover_text(path.display().to_string());
                    // While typing the text is only a draft; it becomes an
                    // edit once the field is left.
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
                }
                items => {
                    let (dirs, files) = (
                        items.iter().filter(|i| i.is_dir).count(),
                        items.iter().filter(|i| !i.is_dir).count(),
                    );
                    ui.vertical(|ui| {
                        ui.heading(format!("{} items", items.len()));
                        ui.weak(count_words(files as u64, dirs as u64));
                    });
                }
            }
        });
        if self.edits.name.is_some() && self.changed_on_disk(|i| i.name.clone()) {
            changed_marker(ui);
        }
    }

    fn general_ui(&mut self, ui: &mut Ui, shared: &mut Shared) {
        let ctx = ui.ctx().clone();
        let walked = self.walk.as_ref().map(|(_, w)| w.counts());
        let editable = self.editable();
        section(ui, "General", "properties-general", true, false, |ui| {
            Grid::new("properties-general-grid")
                .num_columns(2)
                .spacing([12.0, 6.0])
                .show(ui, |ui| {
                    let items = &self.items;
                    row(ui, "Type", |ui| {
                        ui.label(common(items, Item::kind).unwrap_or_else(|| "Various".to_owned()));
                    });
                    let parents: Vec<Option<&Path>> =
                        items.iter().map(|i| i.path.parent()).collect();
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
                    let file_bytes: u64 = items.iter().map(|i| i.size).sum();
                    let bytes = file_bytes + walked.map_or(0, |c| c.bytes);
                    row(ui, "Size", |ui| {
                        ui.horizontal(|ui| {
                            ui.label(format!(
                                "{} ({} bytes)",
                                human_size(bytes),
                                group_digits(bytes)
                            ));
                            if walked.is_some_and(|c| !c.done) {
                                ui.spinner();
                            }
                        });
                    });
                    let allocated: Option<u64> = items
                        .iter()
                        .map(|i| i.allocated)
                        .sum::<Option<u64>>()
                        .map(|a| a + walked.map_or(0, |c| c.allocated));
                    if let Some(allocated) = allocated {
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
                        row(ui, "Points to", |ui| {
                            let draft = self.drafts.link.get_or_insert_with(|| {
                                self.edits.link_target.clone().unwrap_or(target.clone())
                            });
                            let response = ui.add_enabled(
                                editable,
                                TextEdit::singleline(draft).desired_width(f32::INFINITY),
                            );
                            if item.broken {
                                ui.end_row();
                                ui.label("");
                                ui.colored_label(
                                    ui.visuals().warn_fg_color,
                                    "The target doesn't exist",
                                );
                            }
                            if !response.has_focus() {
                                let typed = self.drafts.link.take().unwrap_or_default();
                                let changed = !typed.is_empty() && typed != target;
                                if changed != self.edits.link_target.is_some()
                                    || (changed
                                        && self.edits.link_target.as_deref() != Some(&typed))
                                {
                                    self.edits.link_target = changed.then_some(typed);
                                    self.base.get_or_insert_with(|| self.items.clone());
                                }
                                if response.lost_focus() {
                                    self.commit = true;
                                }
                            }
                        });
                    }
                    self.default_app_ui(ui, &ctx, editable);
                });
        });
    }

    fn default_app_ui(&mut self, ui: &mut Ui, ctx: &egui::Context, editable: bool) {
        if self.items.iter().any(|i| i.is_dir || !i.exists) || self.items.is_empty() {
            return;
        }
        let subjects: Vec<open_with::Subject> = self
            .items
            .iter()
            .map(|i| open_with::Subject {
                path: i.path.clone(),
                on_disk: true,
            })
            .collect();
        let Some(handlers) = open_with::handlers(ctx, &subjects) else {
            row(ui, "Opens with", |ui| {
                ui.spinner();
            });
            return;
        };
        if cfg!(target_os = "linux") && !handlers.types.is_empty() {
            row(ui, "File type", |ui| {
                ui.label(handlers.types.join(", "));
            });
        }
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
                let one_type = handlers.types.len() == 1;
                ui.add_enabled_ui(editable && one_type, |ui| {
                    egui::ComboBox::from_id_salt("properties-default-app")
                        .selected_text(name)
                        .width(220.0)
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
                            "The default app for every “{}” file",
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
                        .add_enabled(editable && !busy, Button::new("Change…"))
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
                let _ = editable;
                ui.label(name);
            }
        });
    }

    fn dates_ui(&mut self, ui: &mut Ui) {
        let editable = self.editable();
        let changed = (self.edits.modified.is_some() || self.edits.accessed.is_some())
            && self.changed_on_disk(|i| (i.modified, i.accessed));
        section(
            ui,
            "Dates",
            "properties-dates",
            !self.compact,
            changed,
            |ui| {
                Grid::new("properties-dates-grid")
                    .num_columns(2)
                    .spacing([12.0, 6.0])
                    .show(ui, |ui| {
                        if let Some(created) = common(&self.items, |i| i.created) {
                            row(ui, "Created", |ui| {
                                ui.label(format_date(created));
                            });
                        }
                        let modified = common(&self.items, |i| i.modified);
                        row(ui, "Modified", |ui| {
                            if date_field(
                                ui,
                                "properties-modified",
                                modified.flatten(),
                                &mut self.edits.modified,
                                &mut self.drafts.modified,
                                editable,
                            ) {
                                self.base.get_or_insert_with(|| self.items.clone());
                                self.commit = true;
                            }
                        });
                        let accessed = common(&self.items, |i| i.accessed);
                        row(ui, "Accessed", |ui| {
                            if date_field(
                                ui,
                                "properties-accessed",
                                accessed.flatten(),
                                &mut self.edits.accessed,
                                &mut self.drafts.accessed,
                                editable,
                            ) {
                                self.base.get_or_insert_with(|| self.items.clone());
                                self.commit = true;
                            }
                        });
                        if let Some(Some(changed)) = common(&self.items, |i| i.changed) {
                            row(ui, "Changed", |ui| {
                                ui.label(format_date(Some(changed))).on_hover_text(
                                    "When the name, permissions or owner last changed",
                                );
                            });
                        }
                    });
            },
        );
    }

    fn any_dirs(&self) -> bool {
        self.items.iter().any(|i| i.is_dir && !i.is_symlink)
    }

    fn permissions_ui(&mut self, ui: &mut Ui) {
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
        section(ui, title, "properties-permissions", true, changed, |ui| {
            let value = |i: &Item| if unix { i.mode } else { i.attributes }.unwrap_or(0);
            let kinds: Vec<(&str, usize)> = if self.recursive {
                vec![("Files", 0), ("Folders", 1)]
            } else {
                vec![("", 2)]
            };
            let mut clicked: Option<(usize, u32, bool)> = None;
            let layout = |ui: &mut Ui, add: &mut dyn FnMut(&mut Ui)| {
                if self.compact || !self.recursive {
                    ui.vertical(|ui| add(ui));
                } else {
                    ui.horizontal_top(|ui| add(ui));
                }
            };
            layout(ui, &mut |ui| {
                for (label, kind) in &kinds {
                    // Which items the column is about: all of them, or the
                    // selected files or folders (their contents are unknown).
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
                    let bits = pending[if *kind == 2 { 0 } else { *kind }];
                    let state = |bit: u32| -> Option<bool> {
                        bits.get(bit).or_else(|| tri_state(&of_kind, bit))
                    };
                    ui.vertical(|ui| {
                        if !label.is_empty() {
                            ui.strong(*label);
                        }
                        ui.add_enabled_ui(editable, |ui| {
                            if unix {
                                let folders = *kind == 1;
                                Grid::new(("properties-mode-grid", *kind))
                                    .num_columns(4)
                                    .spacing([10.0, 4.0])
                                    .show(ui, |ui| {
                                        ui.label("");
                                        ui.weak("Read");
                                        ui.weak("Write");
                                        ui.weak(if folders { "Enter" } else { "Execute" })
                                            .on_hover_text(if folders {
                                                "Execute on a folder: open it and reach what's inside"
                                            } else {
                                                "Run the file as a program"
                                            });
                                        ui.end_row();
                                        for (who, row_bits) in PERMISSIONS {
                                            ui.label(who);
                                            for bit in row_bits {
                                                if let Some(on) = tri_box(ui, "", state(bit), "") {
                                                    clicked = Some((*kind, bit, on));
                                                }
                                            }
                                            ui.end_row();
                                        }
                                    });
                                ui.horizontal_wrapped(|ui| {
                                    for (name, bit, hint) in SPECIAL {
                                        if let Some(on) = tri_box(ui, name, state(bit), hint) {
                                            clicked = Some((*kind, bit, on));
                                        }
                                    }
                                });
                            } else {
                                ui.horizontal_wrapped(|ui| {
                                    for (name, bit) in ATTRIBUTES {
                                        if let Some(on) = tri_box(ui, name, state(bit), "") {
                                            clicked = Some((*kind, bit, on));
                                        }
                                    }
                                });
                            }
                        });
                    });
                }
            });
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
            if unix
                && !self.recursive
                && let [item] = self.items.as_slice()
            {
                let (current, is_dir) = (item.mode.unwrap_or(0), item.is_dir);
                let mode = self.edits.mode[0].apply(current);
                ui.horizontal(|ui| {
                    ui.label("As a number");
                    let draft = self
                        .drafts
                        .octal
                        .get_or_insert_with(|| format!("{mode:04o}"));
                    let valid = u32::from_str_radix(draft.trim(), 8).is_ok_and(|m| m <= 0o7777);
                    let response = ui.add_enabled(
                        editable,
                        TextEdit::singleline(draft)
                            .desired_width(48.0)
                            .font(egui::TextStyle::Monospace)
                            .text_color_opt((!valid).then_some(ui.visuals().error_fg_color)),
                    );
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
                    ui.weak(symbolic(mode, is_dir));
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
                    ui.weak(format!("Also {}", others.join(", ")));
                }
            }
            if self.any_dirs() {
                let mut recursive = self.recursive;
                if ui
                    .add_enabled(
                        editable,
                        Checkbox::new(&mut recursive, "Also change everything inside"),
                    )
                    .on_hover_text(
                        "Apply permission and owner changes to the folders' contents too, \
                         with separate settings for files and folders",
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
                CollapsingHeader::new("Access")
                    .id_salt("properties-acl")
                    .show(ui, |ui| {
                        for line in acl {
                            ui.label(line);
                        }
                        ui.weak("Edit access in Explorer's Security tab");
                    });
            }
        });
    }

    fn owner_ui(&mut self, ui: &mut Ui) {
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
        section(
            ui,
            "Owner",
            "properties-owner",
            !self.compact,
            changed,
            |ui| {
                Grid::new("properties-owner-grid")
                    .num_columns(2)
                    .spacing([12.0, 6.0])
                    .show(ui, |ui| {
                        #[cfg(target_os = "linux")]
                        {
                            let uid = self.edits.uid.or(common(&self.items, |i| i.uid).flatten());
                            let gid = self.edits.gid.or(common(&self.items, |i| i.gid).flatten());
                            row(ui, "User", |ui| {
                                if let Some(id) = id_combo(
                                    ui,
                                    "properties-user",
                                    platform::users(),
                                    uid,
                                    editable,
                                ) {
                                    let unchanged =
                                        common(&self.items, |i| i.uid) == Some(Some(id));
                                    self.set_edit(|e| e.uid = (!unchanged).then_some(id));
                                    self.commit = true;
                                }
                            });
                            row(ui, "Group", |ui| {
                                if let Some(id) = id_combo(
                                    ui,
                                    "properties-group",
                                    platform::groups(),
                                    gid,
                                    editable,
                                ) {
                                    let unchanged =
                                        common(&self.items, |i| i.gid) == Some(Some(id));
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
                                            .desired_width(220.0),
                                    )
                                    .on_hover_text(
                                        "An account name, like “Administrators” or “DOMAIN\\user”",
                                    );
                                if !response.has_focus() {
                                    let typed = self.drafts.account.take().unwrap_or_default();
                                    let typed = typed.trim().to_owned();
                                    let changed =
                                        !typed.is_empty() && Some(&typed) != owner.as_ref();
                                    if changed != self.edits.account.is_some()
                                        || (changed && self.edits.account.as_ref() != Some(&typed))
                                    {
                                        self.set_edit(|e| e.account = changed.then_some(typed));
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
            },
        );
    }

    fn media_ui(&mut self, ui: &mut Ui) {
        let Some(probe) = &self.media else { return };
        if self.items.len() != 1 {
            return;
        }
        section(ui, "Media", "properties-media", true, false, |ui| {
            let Some(media) = probe.get() else {
                ui.spinner();
                return;
            };
            Grid::new("properties-media-grid")
                .num_columns(2)
                .spacing([12.0, 6.0])
                .show(ui, |ui| {
                    if let Some([w, h]) = media.dimensions {
                        row(ui, "Dimensions", |ui| {
                            let megapixels = f64::from(w) * f64::from(h) / 1e6;
                            ui.label(format!("{w} × {h} px ({megapixels:.1} MP)"));
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
                            });
                            if let Some(item) = self.items.first()
                                && duration > 0.0
                            {
                                row(ui, "Bit rate", |ui| {
                                    let kbps = item.size as f64 * 8.0 / duration / 1000.0;
                                    ui.label(format!("{kbps:.0} kb/s"));
                                });
                            }
                        }
                        for track in &av.tracks {
                            row(ui, "Stream", |ui| {
                                ui.label(track);
                            });
                        }
                    }
                    for (tag, value) in &media.exif {
                        row(ui, tag, |ui| {
                            ui.label(value);
                        });
                    }
                    if let Some(error) = &media.error {
                        row(ui, "", |ui| {
                            ui.weak(error);
                        });
                    }
                });
            if !media.exif_rest.is_empty() {
                CollapsingHeader::new("All EXIF tags")
                    .id_salt("properties-exif")
                    .show(ui, |ui| {
                        Grid::new("properties-exif-grid")
                            .num_columns(2)
                            .spacing([12.0, 4.0])
                            .striped(true)
                            .show(ui, |ui| {
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
                                    .desired_width(200.0),
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
                        Grid::new("properties-sums")
                            .num_columns(3)
                            .spacing([8.0, 4.0])
                            .show(ui, |ui| {
                                for (name, sum) in [("MD5", &sums.md5), ("SHA-256", &sums.sha256)] {
                                    ui.label(name);
                                    ui.add(
                                        egui::Label::new(RichText::new(sum).monospace().size(11.0))
                                            .wrap(),
                                    );
                                    if ui.small_button(icon::COPY).on_hover_text("Copy").clicked() {
                                        ui.ctx().copy_text(sum.clone());
                                    }
                                    ui.end_row();
                                }
                            });
                        ui.horizontal(|ui| {
                            ui.add(
                                TextEdit::singleline(&mut self.compare)
                                    .hint_text("Paste a checksum to compare")
                                    .desired_width(260.0),
                            );
                            if !self.compare.trim().is_empty() {
                                match hash::matches(&sums, &self.compare) {
                                    Some(kind) => ui.colored_label(
                                        Color32::from_rgb(0x3f, 0xb9, 0x50),
                                        format!("{} Matches {kind}", icon::CHECK_CIRCLE),
                                    ),
                                    None => ui.colored_label(
                                        ui.visuals().error_fg_color,
                                        format!("{} Doesn't match", icon::X_CIRCLE),
                                    ),
                                };
                            }
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
                Grid::new("properties-filesystem-grid")
                    .num_columns(2)
                    .spacing([12.0, 6.0])
                    .show(ui, |ui| {
                        if let Some(volume) = volume {
                            row(ui, "Mounted at", |ui| {
                                ui.label(volume.mount.display().to_string());
                            });
                            if !volume.fs_type.is_empty() {
                                row(ui, "Filesystem", |ui| {
                                    ui.label(&volume.fs_type);
                                });
                            }
                            if !volume.device.is_empty() {
                                row(ui, if cfg!(windows) { "Label" } else { "Device" }, |ui| {
                                    ui.label(&volume.device);
                                });
                            }
                        }
                        if let Some(space) = space {
                            row(ui, "Free", |ui| {
                                ui.vertical(|ui| {
                                    ui.label(format!(
                                        "{} free of {}",
                                        human_size(space.available),
                                        human_size(space.total)
                                    ));
                                    ui.add(
                                        ProgressBar::new(space.used_share())
                                            .desired_width(200.0)
                                            .desired_height(6.0),
                                    );
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
            if e.mode.iter().any(|b| !b.is_empty()) {
                ops.push(Op::Mode {
                    path: path.clone(),
                    files: e.mode[0],
                    dirs: e.mode[1],
                    recursive: self.recursive && item.is_dir,
                });
            }
            if e.attributes.iter().any(|b| !b.is_empty()) {
                ops.push(Op::Attributes {
                    path: path.clone(),
                    files: e.attributes[0],
                    dirs: e.attributes[1],
                    recursive: self.recursive && item.is_dir,
                });
            }
            if e.uid.is_some() || e.gid.is_some() || e.account.is_some() {
                ops.push(Op::Owner {
                    path: path.clone(),
                    uid: e.uid,
                    gid: e.gid,
                    account: e.account.clone(),
                    recursive: self.recursive && item.is_dir,
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
            && let Some(mime) = self.items.first().and_then(|i| {
                let subject = open_with::Subject {
                    path: i.path.clone(),
                    on_disk: true,
                };
                open_with::handlers(ctx, &[subject]).and_then(|h| h.types.first().cloned())
            })
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

    fn footer(
        &mut self,
        ui: &mut Ui,
        ctx: &egui::Context,
        immediate: bool,
        close: Option<&mut bool>,
    ) {
        if !matches!(self.source, Source::Files(_)) {
            if let Some(close) = close
                && ui.button("Close").clicked()
            {
                *close = true;
            }
            return;
        }
        // In a bottom-up layout, so the buttons come first and the
        // messages above them.
        let pending = !self.edits.is_empty();
        ui.horizontal(|ui| {
            if let Some(applying) = &self.applying {
                ui.spinner();
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
            } else if self.elevated.is_some() {
                ui.spinner();
                ui.label("Waiting for authorization…");
            } else if !immediate || close.is_some() {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if let Some(close) = close {
                        let ok = if immediate || !pending { "Close" } else { "OK" };
                        if ui.button(ok).clicked() {
                            if pending {
                                self.apply(ctx);
                            }
                            *close = true;
                        }
                    }
                    if !immediate {
                        if ui
                            .add_enabled(pending, Button::new(format!("{} Apply", icon::CHECK)))
                            .clicked()
                        {
                            self.apply(ctx);
                        }
                        if ui
                            .add_enabled(pending, Button::new("Revert"))
                            .on_hover_text("Forget the changes not applied yet")
                            .clicked()
                        {
                            self.revert();
                        }
                        if pending {
                            ui.weak("Not applied yet");
                        }
                    }
                });
            }
        });
        if let Some(error) = &self.error {
            ui.colored_label(
                ui.visuals().error_fg_color,
                format!("{} {error}", icon::WARNING),
            );
        }
        if !self.failures.is_empty() {
            let denied = self.failures.iter().any(|f| f.denied);
            let mut retry = false;
            let mut dismiss = false;
            ui.horizontal_wrapped(|ui| {
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
            let n = self.failures.len();
            CollapsingHeader::new(
                RichText::new(format!(
                    "{} {n} change{} failed",
                    icon::WARNING,
                    if n == 1 { "" } else { "s" }
                ))
                .color(ui.visuals().error_fg_color),
            )
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
}

// ---------------------------------------------------------------------------
// Trash and archive entries (read-only)

fn trash_ui(ui: &mut Ui, items: &[TrashedItem]) {
    section(ui, "General", "properties-trash", true, false, |ui| {
        Grid::new("properties-trash-grid")
            .num_columns(2)
            .spacing([12.0, 6.0])
            .show(ui, |ui| {
                if let [item] = items {
                    row(ui, "Name", |ui| {
                        ui.strong(&item.name);
                    });
                    row(ui, "Type", |ui| {
                        ui.label(if item.is_dir {
                            "Folder".to_owned()
                        } else {
                            Item {
                                path: PathBuf::from(&item.name),
                                name: item.name.clone(),
                                exists: true,
                                ..Item::default()
                            }
                            .kind()
                        });
                    });
                    row(ui, "Deleted from", |ui| {
                        let parent = item.original.parent().unwrap_or(&item.original);
                        ui.label(parent.display().to_string());
                    });
                    row(ui, "Deleted", |ui| {
                        ui.label(format_date(item.deleted));
                    });
                } else {
                    row(ui, "Items", |ui| {
                        ui.strong(format!("{} items in the trash", items.len()));
                    });
                }
                let bytes: u64 = items.iter().filter_map(|i| i.size).sum();
                if items.iter().any(|i| i.size.is_some()) {
                    row(ui, "Size", |ui| {
                        ui.label(format!(
                            "{} ({} bytes)",
                            human_size(bytes),
                            group_digits(bytes)
                        ));
                    });
                }
                let entries: u64 = items.iter().filter_map(|i| i.entries).sum();
                if items.iter().any(|i| i.entries.is_some()) {
                    row(ui, "Contains", |ui| {
                        ui.label(format!("{entries} items"));
                    });
                }
            });
        ui.weak("Restore items to change them.");
    });
}

fn archive_ui(ui: &mut Ui, archive: &Path, entries: &[ArchivedItem]) {
    section(ui, "General", "properties-archive", true, false, |ui| {
        Grid::new("properties-archive-grid")
            .num_columns(2)
            .spacing([12.0, 6.0])
            .show(ui, |ui| {
                if let [entry] = entries {
                    let name = entry.path.rsplit('/').next().unwrap_or(&entry.path);
                    row(ui, "Name", |ui| {
                        ui.strong(name);
                    });
                    row(ui, "Inside", |ui| {
                        let folder = entry.path.rsplit_once('/').map_or("", |(dir, _)| dir);
                        ui.label(format!("{}/{folder}", display_name(archive)));
                    });
                    row(ui, "Type", |ui| {
                        ui.label(if entry.is_dir {
                            "Folder".to_owned()
                        } else {
                            Item {
                                path: PathBuf::from(name),
                                name: name.to_owned(),
                                exists: true,
                                ..Item::default()
                            }
                            .kind()
                        });
                    });
                    if let Some(modified) = entry.modified {
                        row(ui, "Modified", |ui| {
                            ui.label(format_date(Some(modified)));
                        });
                    }
                } else {
                    row(ui, "Items", |ui| {
                        ui.strong(format!(
                            "{} items in {}",
                            entries.len(),
                            display_name(archive)
                        ));
                    });
                }
                let size: u64 = entries.iter().map(|e| e.size).sum();
                row(ui, "Size", |ui| {
                    ui.label(format!(
                        "{} ({} bytes)",
                        human_size(size),
                        group_digits(size)
                    ));
                });
                if entries.iter().all(|e| e.packed.is_some()) {
                    let packed: u64 = entries.iter().filter_map(|e| e.packed).sum();
                    row(ui, "Compressed", |ui| {
                        let ratio = if size > 0 {
                            format!(" ({:.0}% of the size)", packed as f64 * 100.0 / size as f64)
                        } else {
                            String::new()
                        };
                        ui.label(format!("{}{ratio}", human_size(packed)));
                    });
                }
                if entries.iter().any(|e| e.encrypted) {
                    row(ui, "Encrypted", |ui| {
                        ui.label(format!("{} Yes", icon::LOCK));
                    });
                }
            });
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
    let title = if changed {
        RichText::new(format!("{title}  {}", icon::WARNING)).color(ui.visuals().warn_fg_color)
    } else {
        RichText::new(title)
    };
    let response = CollapsingHeader::new(title)
        .id_salt(id)
        .default_open(open)
        .show(ui, |ui| {
            if changed {
                changed_marker(ui);
            }
            add(ui);
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

fn row(ui: &mut Ui, label: &str, value: impl FnOnce(&mut Ui)) {
    ui.with_layout(Layout::left_to_right(Align::Min), |ui| ui.weak(label));
    value(ui);
    ui.end_row();
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
    let text = current.map_or_else(
        || "Various".to_owned(),
        |id| format!("{} ({id})", name_of(id)),
    );
    let mut chosen = None;
    ui.add_enabled_ui(editable, |ui| {
        egui::ComboBox::from_id_salt(id)
            .selected_text(text)
            .width(200.0)
            .height(320.0)
            .show_ui(ui, |ui| {
                for (i, name) in names {
                    if ui
                        .selectable_label(current == Some(*i), format!("{name} ({i})"))
                        .clicked()
                    {
                        chosen = Some(*i);
                    }
                }
            });
    });
    chosen
}

/// A date that can be typed in, with a "Now" button. Returns true when a
/// new date was committed.
fn date_field(
    ui: &mut Ui,
    id: &str,
    current: Option<SystemTime>,
    pending: &mut Option<SystemTime>,
    draft: &mut Option<String>,
    editable: bool,
) -> bool {
    let shown = pending.or(current);
    let original = shown.map(format_editable).unwrap_or_default();
    let mut committed = false;
    ui.horizontal(|ui| {
        let text = draft.get_or_insert_with(|| original.clone());
        let parsed = parse_date(text);
        let invalid = !text.trim().is_empty() && parsed.is_none();
        let response = ui.add_enabled(
            editable,
            TextEdit::singleline(text)
                .id_salt(id)
                .hint_text(if current.is_none() { "Various" } else { "" })
                .desired_width(150.0)
                .text_color_opt(invalid.then_some(ui.visuals().error_fg_color)),
        );
        if response.lost_focus() && !invalid && *text != original {
            *pending = parsed;
            committed = true;
        }
        if !response.has_focus() && !invalid {
            *draft = None;
        }
        if ui
            .add_enabled(editable, Button::new("Now").small())
            .on_hover_text("Set to the current time")
            .clicked()
        {
            *pending = Some(SystemTime::now());
            *draft = None;
            committed = true;
        }
        if pending.is_some() && current.is_some_and(|c| Some(c) != *pending) {
            ui.weak(format!("was {}", format_date(current)));
        }
    });
    committed
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

fn contains_label(ui: &mut Ui, counts: Counts) {
    ui.horizontal(|ui| {
        ui.label(count_words(counts.files, counts.dirs));
        if counts.unreadable > 0 {
            ui.colored_label(ui.visuals().warn_fg_color, icon::WARNING)
                .on_hover_text(format!(
                    "{} folder{} couldn't be read, so the totals may be low",
                    counts.unreadable,
                    if counts.unreadable == 1 { "" } else { "s" }
                ));
        }
    });
}

fn count_words(files: u64, dirs: u64) -> String {
    let plural = |n: u64, one: &str, many: &str| {
        format!("{} {}", group_digits(n), if n == 1 { one } else { many })
    };
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
}
