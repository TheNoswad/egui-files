//! Disk usage: a tab that scans a folder and shows where the space went, as
//! a treemap, a sunburst, an icicle, a breakdown by file type, or a list of
//! the largest files, next to the folder tree with share bars.

mod colors;
mod lists;
mod mft;
mod partition;
mod scan;
mod treemap;

use std::{
    collections::{HashMap, HashSet},
    ffi::OsString,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, Weak},
    time::Duration,
};

use web_time::Instant;

use egui::{
    Button, CentralPanel, CornerRadius, FontId, Id, Key, Modifiers, Panel, Rect, Response,
    RichText, Sense, Ui, text::LayoutJob, vec2,
};
use egui_phosphor::regular as icon;
use serde::{Deserialize, Serialize};

use crate::{
    jobs::JobKind,
    open_with,
    tab::{AppAction, Shared},
};
pub use colors::ColorMode;
use colors::{Painter, Palette};
#[cfg(not(target_arch = "wasm32"))]
pub use mft::run_helper;
use scan::{Category, Kind, Metric, NONE, ROOT, Scan, ScanHandle, Tree};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum View {
    #[default]
    Treemap,
    Sunburst,
    Icicle,
    Types,
    Largest,
}

impl View {
    const ALL: [Self; 5] = [
        Self::Treemap,
        Self::Sunburst,
        Self::Icicle,
        Self::Types,
        Self::Largest,
    ];

    fn icon(self) -> &'static str {
        match self {
            Self::Treemap => icon::SQUARE_SPLIT_HORIZONTAL,
            Self::Sunburst => icon::CHART_DONUT,
            Self::Icicle => icon::CHART_BAR_HORIZONTAL,
            Self::Types => icon::CHART_PIE_SLICE,
            Self::Largest => icon::SORT_DESCENDING,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Treemap => "Treemap",
            Self::Sunburst => "Sunburst",
            Self::Icicle => "Icicle",
            Self::Types => "File types",
            Self::Largest => "Largest files",
        }
    }
}

/// What a chart or list asked for, applied once drawing is done (and the
/// tree is no longer borrowed).
pub enum Act {
    /// Clicked in the folder tree.
    Select(u32),
    /// Clicked in a chart or list: also shown in the folder tree.
    Pick(u32),
    Hover(u32),
    /// Double-clicked: zoom into a folder, open a file.
    Activate(u32),
    Zoom(u32),
    Up,
    ToggleExpand(u32),
    Highlight(Category),
    Open(u32),
    ShowInFolder(u32),
    OpenInNewTab(u32),
    Rescan(u32),
    CopyPath(u32),
    Trash(u32),
    OpenWith(u32, Option<open_with::Target>),
    Properties(u32),
}

/// What a chart's layout depends on, to lay it out again only when needed.
#[derive(Clone, Debug)]
pub struct LayoutKey {
    generation: u64,
    view: (u32, Metric, Rect, u64),
    at: Instant,
}

/// How often a running scan's charts are laid out again.
pub(super) const RELAYOUT: Duration = Duration::from_millis(300);

impl LayoutKey {
    pub fn new(tree: &Tree, focus: u32, metric: Metric, rect: Rect) -> Self {
        Self {
            generation: tree.generation,
            view: (focus, metric, rect, 0),
            at: Instant::now(),
        }
    }

    pub fn with(mut self, extra: u64) -> Self {
        self.view.3 = extra;
        self
    }
}

/// Stores `new` and returns true when the layout must be redone: what's
/// shown changed, or the tree did (at most every `RELAYOUT` while scanning).
pub fn refresh(key: &mut Option<LayoutKey>, new: LayoutKey, ctx: &egui::Context) -> bool {
    refresh_every(key, new, ctx, RELAYOUT)
}

/// `refresh`, laying out again at most every `every` while scanning.
pub fn refresh_every(
    key: &mut Option<LayoutKey>,
    new: LayoutKey,
    ctx: &egui::Context,
    every: Duration,
) -> bool {
    let redo = match key {
        None => true,
        Some(old) if old.view != new.view => true,
        Some(old) if old.generation != new.generation => {
            let due = old.at.elapsed() >= every;
            if !due {
                ctx.request_repaint_after(every - old.at.elapsed());
            }
            due
        }
        Some(_) => false,
    };
    if redo {
        *key = Some(new);
    }
    redo
}

/// One line of text cut short with "…" to fit `width`.
pub fn elided(
    painter: &egui::Painter,
    text: String,
    font: FontId,
    color: egui::Color32,
    width: f32,
) -> Arc<egui::Galley> {
    let mut job = LayoutJob::simple_singleline(text, font, color);
    job.wrap = egui::text::TextWrapping {
        max_width: width.max(1.0),
        max_rows: 1,
        break_anywhere: true,
        overflow_character: Some('…'),
    };
    painter.layout_job(job)
}

/// What the pointer did on a chart.
pub struct ChartOutput {
    response: Response,
    hovered: Option<u32>,
    /// On the middle of the sunburst, or the icicle's first column: the
    /// focused folder, which goes up a level when clicked.
    on_focus: bool,
    back: bool,
}

impl ChartOutput {
    fn new(ui: &Ui, response: Response, hovered: Option<u32>, on_focus: bool) -> Self {
        let back = response.hovered()
            && ui.input(|i| i.pointer.button_clicked(egui::PointerButton::Extra1));
        Self {
            response,
            hovered,
            on_focus,
            back,
        }
    }
}

/// The right-click menu for anything in the tree.
pub fn node_menu(ui: &mut Ui, tree: &Tree, id: u32, acts: &mut Vec<Act>) {
    fn item(ui: &mut Ui, label: String, act: Act, acts: &mut Vec<Act>) {
        if ui.button(label).clicked() {
            acts.push(act);
            ui.close();
        }
    }
    let node = tree.node(id);
    if node.is_dir() {
        item(
            ui,
            format!("{} Zoom in", icon::MAGNIFYING_GLASS_PLUS),
            Act::Zoom(id),
            acts,
        );
        item(
            ui,
            format!("{} Open", icon::FOLDER_OPEN),
            Act::Open(id),
            acts,
        );
        let label = format!("{} Open in new tab", icon::PLUS);
        item(ui, label, Act::OpenInNewTab(id), acts);
    } else {
        item(
            ui,
            format!("{} Open", icon::ARROW_SQUARE_OUT),
            Act::Open(id),
            acts,
        );
        let path = tree.path(id);
        match open_with::submenu(ui, || vec![path]) {
            Some(open_with::Choice::Open(target)) => acts.push(Act::OpenWith(id, Some(target))),
            Some(open_with::Choice::Other) => acts.push(Act::OpenWith(id, None)),
            None => {}
        }
    }
    if id != ROOT {
        let label = format!("{} Show in folder", icon::FOLDER_SIMPLE);
        item(ui, label, Act::ShowInFolder(id), acts);
    }
    item(
        ui,
        format!("{} Copy path", icon::CLIPBOARD),
        Act::CopyPath(id),
        acts,
    );
    ui.separator();
    if node.is_dir() {
        item(
            ui,
            format!("{} Scan again", icon::ARROW_CLOCKWISE),
            Act::Rescan(id),
            acts,
        );
    }
    if id != ROOT {
        item(
            ui,
            format!("{} Move to trash", icon::TRASH),
            Act::Trash(id),
            acts,
        );
    }
    ui.separator();
    item(
        ui,
        format!("{} Properties", icon::INFO),
        Act::Properties(id),
        acts,
    );
}

#[derive(Serialize, Deserialize)]
pub struct UsageTab {
    pub id: u64,
    /// The folder scanned; also marks saved state as a disk usage tab.
    pub usage: PathBuf,
    #[serde(default)]
    view: View,
    #[serde(default)]
    color: ColorMode,
    #[serde(default)]
    metric: Metric,

    /// New tabs scan right away; restored ones wait to be asked, so starting
    /// the app doesn't set off a scan of a whole drive.
    #[serde(skip)]
    autostart: bool,
    #[serde(skip)]
    scan: Option<ScanHandle>,
    #[serde(skip)]
    focus: u32,
    #[serde(skip)]
    selected: Option<u32>,
    #[serde(skip)]
    hovered: Option<u32>,
    /// Right-clicked in a chart, for the menu that stays open.
    #[serde(skip)]
    menu: Option<u32>,
    #[serde(skip)]
    expanded: HashSet<u32>,
    /// Bumped when `expanded` changes.
    #[serde(skip)]
    expansion: u64,
    #[serde(skip)]
    scroll_to: Option<u32>,
    #[serde(skip)]
    highlight: Option<Category>,
    /// Sent to the trash; taken out of the tree once they're gone.
    #[serde(skip)]
    trashed: Vec<u32>,
    /// Folders that changed while the scan was running, looked at again
    /// once it's done.
    #[serde(skip)]
    changed: HashSet<PathBuf>,
    #[serde(skip)]
    table: lists::TreeTable,
    #[serde(skip)]
    breakdown: lists::Breakdown,
    #[serde(skip)]
    treemap: treemap::Cache,
    #[serde(skip)]
    partition: partition::Cache,
    #[serde(skip)]
    space: Option<(Instant, Option<crate::places::Space>)>,
    /// How the drive's master file table can be read, when the scanned
    /// folder is a whole NTFS drive.
    #[serde(skip)]
    mft: Option<mft::Access>,
    /// Asked for the fast scan as administrator: scanning again asks again.
    #[serde(skip)]
    fast: bool,
    /// Where a finished scan goes when the tab closes, to be kept a while.
    #[serde(skip)]
    closed: Option<Arc<Mutex<Vec<ScanHandle>>>>,
}

impl Drop for UsageTab {
    fn drop(&mut self) {
        let (Some(scan), Some(closed)) = (self.scan.take(), &self.closed) else {
            return;
        };
        // A running one stops: closing the tab is how to stop a scan.
        if !scan.running() && !scan.cancelled() {
            closed.lock().unwrap().push(scan);
        }
    }
}

impl UsageTab {
    pub fn new(id: u64, usage: PathBuf) -> Self {
        Self {
            id,
            usage,
            view: View::default(),
            color: ColorMode::default(),
            metric: Metric::default(),
            autostart: true,
            scan: None,
            focus: ROOT,
            selected: None,
            hovered: None,
            menu: None,
            expanded: HashSet::from([ROOT]),
            expansion: 0,
            scroll_to: None,
            highlight: None,
            trashed: Vec::new(),
            changed: HashSet::new(),
            table: Default::default(),
            breakdown: Default::default(),
            treemap: Default::default(),
            partition: Default::default(),
            space: None,
            mft: None,
            fast: false,
            closed: None,
        }
    }

    pub fn title(&self) -> String {
        format!(
            "{} {}",
            icon::CHART_PIE_SLICE,
            crate::fs::display_name(&self.usage)
        )
    }

    /// Scans the folder, or with `reuse`, takes over a scan of it that's
    /// kept: from a tab of it that closed, or the browser's.
    fn start(&mut self, ctx: &egui::Context, shared: &mut Shared, reuse: bool) {
        let root = self.usage.clone();
        self.mft = mft::access(&root);
        // The master file table is faster than a running listing is to finish.
        let kept = reuse
            .then(|| {
                let running = self.mft != Some(mft::Access::Direct);
                shared.usage.take(&root, running)
            })
            .flatten();
        let scan = match kept {
            Some(kept) => kept,
            None => ScanHandle(match self.mft {
                Some(mft::Access::Direct) => Scan::start_mft(ctx, root, mft::Access::Direct),
                Some(mft::Access::Elevate) if self.fast => {
                    Scan::start_mft(ctx, root, mft::Access::Elevate)
                }
                _ => Scan::start(ctx, root),
            }),
        };
        shared.usage.register(&scan);
        self.closed = Some(shared.usage.closed.clone());
        self.scan = Some(scan);
        self.focus = ROOT;
        self.selected = None;
        self.menu = None;
        self.expanded = HashSet::from([ROOT]);
        self.expansion += 1;
        self.trashed.clear();
        self.changed.clear();
    }

    /// The jobs finished: takes what was trashed out of the tree.
    pub fn mark_stale(&mut self) {
        let Some(scan) = &self.scan else { return };
        let gone: Vec<u32> = {
            let tree = scan.tree();
            self.trashed
                .iter()
                .copied()
                .filter(|&id| tree.is_attached(id) && !disk::path_exists(tree.path(id)))
                .collect()
        };
        if !gone.is_empty() {
            let mut tree = scan.tree.write().unwrap();
            for id in gone {
                tree.remove(id);
            }
        }
        self.trashed.clear();
    }

    pub fn dirs_changed(&mut self, dirs: &HashSet<PathBuf>) {
        if self.scan.is_some() {
            self.changed
                .extend(dirs.iter().filter(|d| d.starts_with(&self.usage)).cloned());
        }
    }

    /// The focused folder is watched, so deleting there shows up.
    pub fn shown_dirs(&self) -> Vec<PathBuf> {
        match &self.scan {
            Some(scan) => vec![scan.tree().path(self.focus)],
            None => Vec::new(),
        }
    }

    fn look_at_changes(&mut self) {
        let Some(scan) = &self.scan else { return };
        if self.changed.is_empty() || scan.running() {
            return;
        }
        let ids: Vec<u32> = {
            let tree = scan.tree();
            self.changed
                .drain()
                .filter_map(|dir| tree.find(&dir))
                .filter(|&id| tree.node(id).is_dir())
                .collect()
        };
        for id in ids {
            scan.refresh(id);
        }
    }

    fn keyboard(&self, ctx: &egui::Context, acts: &mut Vec<Act>) {
        if ctx.memory(|m| m.focused().is_some()) {
            return;
        }
        let keymap = crate::keymap::get(ctx);
        if keymap.is_capturing() {
            return;
        }
        ctx.input_mut(|i| {
            if keymap.pressed(i, crate::keymap::Command::Up) {
                acts.push(Act::Up);
            }
            if let Some(id) = self.selected {
                if keymap.pressed(i, crate::keymap::Command::Trash) {
                    acts.push(Act::Trash(id));
                }
                if i.consume_key(Modifiers::NONE, Key::Enter) {
                    acts.push(Act::Activate(id));
                }
            }
        });
    }

    pub fn ui(&mut self, ui: &mut Ui, shared: &mut Shared) {
        // A minimized window on Windows still runs frames, with no room at
        // all; laying out then would squeeze the tree panel to its minimum
        // for good.
        if ui.available_width() < 1.0 || ui.available_height() < 1.0 {
            return;
        }
        ui.style_mut().interaction.selectable_labels = false;
        if self.scan.is_none() && self.autostart {
            self.autostart = false;
            self.start(ui.ctx(), shared, true);
        }
        self.look_at_changes();
        let mut acts = Vec::new();
        if shared.focused_tab == Some(self.id) {
            self.keyboard(ui.ctx(), &mut acts);
        }
        let mut rescan = false;
        let Some(scan) = self.scan.as_ref().map(|s| s.0.clone()) else {
            CentralPanel::default().show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(ui.available_height() / 3.0);
                    ui.label(RichText::new(icon::CHART_PIE_SLICE).size(40.0).weak());
                    ui.label(self.usage.display().to_string());
                    ui.add_space(8.0);
                    if ui
                        .button(format!("{} Scan disk usage", icon::PLAY))
                        .clicked()
                    {
                        rescan = true;
                    }
                });
            });
            if rescan {
                self.start(ui.ctx(), shared, true);
            }
            return;
        };

        {
            let tree = scan.tree();
            if !tree.is_attached(self.focus) || !tree.node(self.focus).is_dir() {
                self.focus = ROOT;
            }
            if self.selected.is_some_and(|id| !tree.is_attached(id)) {
                self.selected = None;
            }
            let palette = Palette::new(ui.ctx().theme());
            let painter = Painter::new(
                &tree,
                self.focus,
                self.metric,
                self.color,
                &palette,
                self.highlight,
            );
            let linked_hover = self.hovered.take();

            Panel::top(Id::new(("usage-toolbar", self.id))).show(ui, |ui| {
                self.toolbar(ui, &tree, &scan, &mut acts, &mut rescan)
            });
            let fast = Panel::top(Id::new(("usage-status", self.id)))
                .show(ui, |ui| self.status(ui, &tree, &scan))
                .inner;
            if fast {
                self.fast = true;
                rescan = true;
            }
            Panel::left(Id::new(("usage-tree", self.id)))
                .resizable(true)
                .default_size(440.0)
                .min_size(200.0)
                .show(ui, |ui| {
                    lists::tree_table(
                        ui,
                        self.id,
                        &mut self.table,
                        &painter,
                        &self.expanded,
                        self.expansion,
                        self.selected,
                        self.scroll_to.take(),
                        &mut acts,
                    );
                });
            CentralPanel::default()
                .frame(egui::Frame::NONE)
                .show(ui, |ui| match self.view {
                    View::Treemap | View::Sunburst | View::Icicle => {
                        // The legend goes under the chart, measured every
                        // frame rather than kept in a panel: a panel remembers
                        // its height, and on Windows it kept one far taller
                        // than its row, squashing the chart above it.
                        let ui = &mut self.legend_below(ui, &painter, &mut acts);
                        let output = match self.view {
                            View::Treemap => treemap::show(
                                ui,
                                &mut self.treemap,
                                &painter,
                                self.focus,
                                self.selected,
                                linked_hover,
                            ),
                            View::Sunburst => partition::sunburst(
                                ui,
                                &mut self.partition,
                                &painter,
                                self.focus,
                                self.selected,
                                linked_hover,
                            ),
                            _ => partition::icicle(
                                ui,
                                &mut self.partition,
                                &painter,
                                self.focus,
                                self.selected,
                                linked_hover,
                            ),
                        };
                        self.chart_input(&tree, output, &painter, &mut acts);
                    }
                    View::Types => {
                        egui::Frame::NONE.inner_margin(8.0).show(ui, |ui| {
                            self.breakdown.update(ui, &tree, self.focus, self.metric);
                            lists::types(ui, &mut self.breakdown, &painter, self.focus, &mut acts);
                        });
                    }
                    View::Largest => {
                        self.breakdown.update(ui, &tree, self.focus, self.metric);
                        lists::largest(
                            ui,
                            &self.breakdown,
                            &painter,
                            self.focus,
                            self.selected,
                            &mut acts,
                        );
                    }
                });
        }

        if rescan {
            self.start(ui.ctx(), shared, false);
            return;
        }
        for act in acts {
            self.apply(ui.ctx(), &scan, act, shared);
        }
    }

    fn chart_input(
        &mut self,
        tree: &Tree,
        output: ChartOutput,
        painter: &Painter,
        acts: &mut Vec<Act>,
    ) {
        let ChartOutput {
            response,
            hovered,
            on_focus,
            back,
        } = output;
        if let Some(id) = hovered {
            acts.push(Act::Hover(id));
        }
        if back || (on_focus && response.clicked()) {
            acts.push(Act::Up);
        } else if response.double_clicked() {
            if let Some(id) = hovered {
                acts.push(Act::Activate(id));
            }
        } else if response.clicked() || response.secondary_clicked() {
            match hovered {
                Some(id) => acts.push(Act::Pick(id)),
                None if on_focus => acts.push(Act::Pick(self.focus)),
                None => {}
            }
        }
        if response.secondary_clicked() {
            self.menu = hovered.or(on_focus.then_some(self.focus));
        }
        if let Some(id) = hovered {
            let response = response.clone();
            response.on_hover_ui_at_pointer(|ui| tooltip(ui, tree, id, self.focus, painter.metric));
        }
        if let Some(id) = self.menu.filter(|&id| tree.is_attached(id)) {
            let open = response.context_menu(|ui| node_menu(ui, tree, id, acts));
            if open.is_none() {
                self.menu = None;
            }
        }
    }

    fn toolbar(
        &mut self,
        ui: &mut Ui,
        tree: &Tree,
        scan: &Scan,
        acts: &mut Vec<Act>,
        rescan: &mut bool,
    ) {
        crate::widgets::split_row(ui, |ui, side| match side {
            crate::widgets::Side::Left => {
                ui.spacing_mut().item_spacing.x = 2.0;
                if scan.running() {
                    if ui
                        .button(icon::STOP)
                        .on_hover_text("Stop scanning")
                        .clicked()
                    {
                        scan.cancel();
                    }
                } else if ui
                    .button(icon::ARROW_CLOCKWISE)
                    .on_hover_text("Scan again")
                    .clicked()
                {
                    *rescan = true;
                }
                if ui
                    .add_enabled(self.focus != ROOT, Button::new(icon::ARROW_UP))
                    .on_hover_text("Up a level (Backspace)")
                    .clicked()
                {
                    acts.push(Act::Up);
                }
                ui.add_space(6.0);
                // The path from the scanned folder to the focused one.
                let chain = tree.ancestry(self.focus);
                for (i, &id) in chain.iter().enumerate() {
                    if i > 0 {
                        ui.label(RichText::new(icon::CARET_RIGHT).small().weak());
                    }
                    let name = if id == ROOT {
                        self.usage.display().to_string()
                    } else {
                        tree.name(id)
                    };
                    if ui
                        .add(Button::selectable(id == self.focus, name).frame_when_inactive(false))
                        .clicked()
                    {
                        acts.push(Act::Zoom(id));
                    }
                }
            }
            crate::widgets::Side::Right => {
                ui.spacing_mut().item_spacing.x = 2.0;
                egui::ComboBox::from_id_salt(("usage-metric", self.id))
                    .selected_text(self.metric.label())
                    .show_ui(ui, |ui| {
                        for metric in [Metric::Disk, Metric::Apparent] {
                            ui.selectable_value(&mut self.metric, metric, metric.label());
                        }
                    })
                    .response
                    .on_hover_text(
                        "Size on disk counts whole blocks and leaves out sparse files' holes; \
                         apparent size adds up the files' lengths",
                    );
                ui.add_space(4.0);
                egui::ComboBox::from_id_salt(("usage-color", self.id))
                    .selected_text(format!("{} {}", icon::PALETTE, self.color.label()))
                    .show_ui(ui, |ui| {
                        for mode in ColorMode::ALL {
                            ui.selectable_value(&mut self.color, mode, mode.label());
                        }
                    })
                    .response
                    .on_hover_text("Color by");
                ui.add_space(8.0);
                for view in View::ALL.into_iter().rev() {
                    if ui
                        .add(Button::selectable(self.view == view, view.icon()))
                        .on_hover_text(view.label())
                        .clicked()
                    {
                        self.view = view;
                    }
                }
            }
        });
    }

    /// True when the fast scan was asked for.
    fn status(&mut self, ui: &mut Ui, tree: &Tree, scan: &Scan) -> bool {
        let mut fast = false;
        let root = tree.node(ROOT);
        let running = scan.running();
        let space = match self.space {
            Some((at, space)) if at.elapsed() < Duration::from_secs(2) => space,
            _ => {
                let space = crate::places::space(&self.usage);
                self.space = Some((Instant::now(), space));
                space
            }
        };
        crate::widgets::split_row(ui, |ui, side| match side {
            crate::widgets::Side::Left => {
                if running {
                    ui.spinner();
                }
                if scan.reading_mft() {
                    ui.label(format!(
                        "Reading the drive's master file table… {:.1} s",
                        scan.elapsed().as_secs_f32()
                    ));
                    return;
                }
                let mut text = format!(
                    "{} in {} files, {} folders",
                    crate::fs::human_size(root.size(self.metric)),
                    lists::count(root.totals.files),
                    lists::count(tree.dirs),
                );
                if scan.cancelled() && !running {
                    text.push_str(" — stopped early");
                }
                text.push_str(&format!(" · {:.1} s", scan.elapsed().as_secs_f32()));
                ui.label(text);
                if tree.unreadable > 0 {
                    ui.label(
                        RichText::new(format!("{} {} unreadable", icon::WARNING, tree.unreadable))
                            .color(ui.visuals().warn_fg_color),
                    )
                    .on_hover_text(
                        "Folders that couldn't be listed, usually for lack of permission",
                    );
                }
                if let Some(error) = scan.mft_error() {
                    // Declined or failed: scanning again lists folders.
                    self.fast = false;
                    ui.label(
                        RichText::new(format!("{} Fast scan didn't work", icon::WARNING))
                            .color(ui.visuals().warn_fg_color),
                    )
                    .on_hover_text(format!(
                        "Couldn't read the drive's master file table ({error}), so its folders \
                     were listed instead"
                    ));
                } else if self.mft == Some(mft::Access::Elevate)
                    && !scan.used_mft()
                    && ui
                        .button(format!("{} Fast scan", icon::LIGHTNING))
                        .on_hover_text(
                            "Scan the whole drive again from its master file table: seconds \
                         instead of minutes on a hard drive. Needs an administrator, so \
                         Windows asks first.",
                        )
                        .clicked()
                {
                    fast = true;
                }
            }
            crate::widgets::Side::Right => {
                if let Some(space) = space {
                    ui.label(
                        RichText::new(format!(
                            "{} free of {}",
                            crate::fs::human_size(space.available),
                            crate::fs::human_size(space.total)
                        ))
                        .weak(),
                    );
                    volume_bar(ui, space, root.totals.disk);
                }
            }
        });
        fast
    }

    /// Shows the legend along the bottom of `ui`, and returns a `Ui` for the
    /// chart filling the space above it.
    fn legend_below(&self, ui: &mut Ui, painter: &Painter, acts: &mut Vec<Act>) -> Ui {
        let full = ui.available_rect_before_wrap();
        let frame = egui::Frame::side_top_panel(ui.style());
        let show = |ui: &mut Ui, acts: &mut Vec<Act>| {
            frame
                .show(ui, |ui| {
                    ui.set_min_width(ui.available_width());
                    self.legend(ui, painter, acts);
                })
                .response
                .rect
                .height()
        };
        // Measured first, unseen, at the width it will have.
        let height = ui
            .scope_builder(
                egui::UiBuilder::new()
                    .id_salt("legend-measure")
                    .max_rect(full)
                    .sizing_pass()
                    .invisible(),
                |ui| show(ui, &mut Vec::new()),
            )
            .inner
            .min(full.height());
        let legend = Rect::from_min_max(egui::pos2(full.min.x, full.max.y - height), full.max);
        ui.scope_builder(
            egui::UiBuilder::new().id_salt("legend").max_rect(legend),
            |ui| show(ui, acts),
        );
        let line = ui.visuals().widgets.noninteractive.bg_stroke;
        ui.painter().hline(full.x_range(), legend.top(), line);
        let chart = Rect::from_min_max(full.min, egui::pos2(full.max.x, legend.top() - line.width));
        ui.new_child(egui::UiBuilder::new().id_salt("chart").max_rect(chart))
    }

    fn legend(&self, ui: &mut Ui, painter: &Painter, acts: &mut Vec<Act>) {
        let swatch = |ui: &mut Ui, color: egui::Color32| {
            let (rect, _) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
            ui.painter().rect_filled(rect, CornerRadius::same(2), color);
        };
        let tree = painter.tree;
        ui.horizontal_wrapped(|ui| match self.color {
            ColorMode::Type => {
                let totals = tree.node(self.focus).totals;
                for category in Category::ALL {
                    let size = totals.category(self.metric, category);
                    if size == 0 {
                        continue;
                    }
                    let on = self.highlight.is_none_or(|h| h == category);
                    let color = painter.palette.category(category);
                    let response = ui
                        .scope(|ui| {
                            swatch(
                                ui,
                                if on {
                                    color
                                } else {
                                    color.gamma_multiply(0.25)
                                },
                            );
                            let text = RichText::new(format!(
                                "{} {}",
                                category.label(),
                                crate::fs::human_size(size)
                            ));
                            ui.add(
                                egui::Label::new(if on { text } else { text.weak() })
                                    .sense(Sense::click()),
                            )
                        })
                        .inner;
                    if response
                        .on_hover_text("Click to highlight; again to show everything")
                        .clicked()
                    {
                        acts.push(Act::Highlight(category));
                    }
                    ui.add_space(8.0);
                }
            }
            ColorMode::Folder => {
                let children = tree.sorted_children(self.focus, self.metric);
                for (slot, &id) in children.iter().take(8).enumerate() {
                    swatch(ui, painter.palette.series[slot]);
                    let response = ui.add(egui::Label::new(tree.name(id)).sense(Sense::click()));
                    if response.clicked() {
                        acts.push(Act::Pick(id));
                    }
                    ui.add_space(8.0);
                }
                if children.len() > 8 {
                    swatch(ui, painter.palette.gray);
                    ui.label("Everything else");
                }
            }
            ColorMode::Age => {
                ui.label(RichText::new("Last changed:").weak());
                for (bucket, label) in colors::age_labels().enumerate() {
                    swatch(ui, painter.palette.ages[bucket]);
                    ui.label(label);
                    ui.add_space(6.0);
                }
            }
        });
    }

    fn expand_to(&mut self, tree: &Tree, id: u32) {
        let chain = tree.ancestry(id);
        let parents = &chain[..chain.len().saturating_sub(1)];
        if parents.iter().any(|p| !self.expanded.contains(p)) {
            self.expanded.extend(parents);
            self.expansion += 1;
        }
        self.scroll_to = Some(id);
    }

    fn apply(&mut self, ctx: &egui::Context, scan: &Arc<Scan>, act: Act, shared: &mut Shared) {
        let path = |id: u32| scan.tree().path(id);
        match act {
            Act::Select(id) => self.selected = Some(id),
            Act::Pick(id) => {
                self.selected = Some(id);
                self.expand_to(&scan.tree(), id);
            }
            Act::Hover(id) => self.hovered = Some(id),
            Act::Activate(id) => {
                let act = if scan.tree().node(id).is_dir() {
                    Act::Zoom(id)
                } else {
                    Act::Open(id)
                };
                self.apply(ctx, scan, act, shared);
            }
            Act::Zoom(id) => {
                let tree = scan.tree();
                if tree.node(id).is_dir() {
                    self.focus = id;
                    self.selected = Some(id);
                    self.expanded.insert(id);
                    self.expand_to(&tree, id);
                }
            }
            Act::Up => {
                let parent = scan.tree().node(self.focus).parent;
                if parent != NONE {
                    self.selected = Some(self.focus);
                    self.focus = parent;
                    self.expand_to(&scan.tree(), self.selected.unwrap());
                }
            }
            Act::ToggleExpand(id) => {
                if !self.expanded.remove(&id) {
                    self.expanded.insert(id);
                }
                self.expansion += 1;
            }
            Act::Highlight(category) => {
                self.highlight = (self.highlight != Some(category)).then_some(category);
            }
            Act::Open(id) => {
                let kind = scan.tree().node(id).kind;
                let path = path(id);
                if matches!(kind, Kind::Dir | Kind::Mount) {
                    shared.app_actions.push(AppAction::NewTab(path));
                } else if let Err(err) = crate::fs::open(&path) {
                    eprintln!("Could not open {}: {err}", path.display());
                }
            }
            Act::ShowInFolder(id) => shared.app_actions.push(AppAction::Reveal(path(id))),
            Act::OpenInNewTab(id) => shared.app_actions.push(AppAction::NewTab(path(id))),
            Act::Rescan(id) => scan.rescan(id),
            Act::CopyPath(id) => ctx.copy_text(path(id).display().to_string()),
            Act::Trash(id) if id != ROOT => {
                shared.jobs.start(JobKind::Trash {
                    paths: vec![path(id)],
                });
                self.trashed.push(id);
            }
            Act::Trash(_) => {}
            Act::OpenWith(id, target) => shared.app_actions.push(AppAction::OpenWith(
                open_with::Request::Files(vec![path(id)]),
                target,
            )),
            Act::Properties(id) => {
                shared
                    .app_actions
                    .push(AppAction::Properties(crate::properties::Source::Files(
                        vec![path(id)],
                    )))
            }
        }
    }
}

fn tooltip(ui: &mut Ui, tree: &Tree, id: u32, focus: u32, metric: Metric) {
    let node = tree.node(id);
    let path = tree.path(id);
    let base = tree.path(focus);
    let shown = path.strip_prefix(&base).unwrap_or(&path);
    ui.label(RichText::new(shown.display().to_string()).strong());
    let whole = tree.node(focus).size(metric).max(1);
    ui.label(format!(
        "{} · {:.1} % of {}",
        crate::fs::human_size(node.size(metric)),
        node.size(metric) as f64 / whole as f64 * 100.0,
        tree.name(focus),
    ));
    if node.is_dir() {
        ui.label(RichText::new(format!("{} files", lists::count(node.totals.files))).weak());
    }
}

/// The drive's used space, with the part the scan found picked out.
fn volume_bar(ui: &mut Ui, space: crate::places::Space, scanned: u64) {
    let (rect, response) = ui.allocate_exact_size(vec2(160.0, 8.0), Sense::hover());
    let total = space.total.max(1) as f32;
    let used = space.total.saturating_sub(space.available) as f32 / total;
    let found = (scanned as f32 / total).min(used);
    let p = ui.painter();
    let visuals = ui.visuals();
    p.rect_filled(rect, CornerRadius::same(4), visuals.extreme_bg_color);
    let part =
        |share: f32| Rect::from_min_size(rect.min, vec2(rect.width() * share, rect.height()));
    p.rect_filled(
        part(used),
        CornerRadius::same(4),
        visuals.widgets.inactive.bg_fill,
    );
    p.rect_filled(
        part(found),
        CornerRadius::same(4),
        visuals.selection.bg_fill,
    );
    response.on_hover_text(format!(
        "This folder: {} · drive used: {} of {}",
        crate::fs::human_size(scanned),
        crate::fs::human_size(space.total.saturating_sub(space.available)),
        crate::fs::human_size(space.total),
    ));
}

/// Scans for folder sizes in the browser's Details view: those open in disk
/// usage tabs, and ones started for folders the browser shows.
pub struct Scans {
    scans: Vec<Weak<Scan>>,
    /// Started for the browser; dropped (and stopped) once nothing shows them.
    own: Vec<Owned>,
    /// The last sizes worked out for each folder shown.
    cache: Vec<Cached>,
    /// Scan folders the browser shows; off, only disk usage tabs give sizes.
    pub enabled: bool,
    /// Folders looked at for being on a network or FUSE mount, which the
    /// browser doesn't measure on its own.
    remote: HashMap<PathBuf, bool>,
    /// Finished scans of disk usage tabs that closed, handed over to be kept
    /// for a while (see [`UNUSED_CLOSED`]).
    closed: Arc<Mutex<Vec<ScanHandle>>>,
}

struct Owned {
    handle: ScanHandle,
    used: Instant,
    /// How long it's kept, finished, once nothing shows it.
    keep: Duration,
    /// Folders that changed while it was scanning, looked at once it's done.
    changed: HashSet<PathBuf>,
}

struct Cached {
    dir: PathBuf,
    scan: usize,
    generation: u64,
    sizes: Arc<FolderSizes>,
    used: Instant,
}

/// How long a browser scan nothing shows keeps going, so switching tabs and
/// back doesn't start over.
const UNUSED_RUNNING: Duration = Duration::from_secs(5);
/// How long a finished browser scan nothing shows is kept, so going back to
/// a folder shows its sizes right away.
const UNUSED_FINISHED: Duration = Duration::from_secs(120);
/// How long the scan of a disk usage tab that closed is kept, so opening
/// the tab again shows it right away (and the browser its folder sizes).
const UNUSED_CLOSED: Duration = Duration::from_secs(15 * 60);
/// Finished browser scans kept at most, besides those in use.
const KEPT_FINISHED: usize = 4;

impl Default for Scans {
    fn default() -> Self {
        Self {
            scans: Vec::new(),
            own: Vec::new(),
            cache: Vec::new(),
            enabled: true,
            remote: HashMap::new(),
            closed: Default::default(),
        }
    }
}

/// The sizes of a folder's subfolders, from a disk usage scan.
pub struct FolderSizes {
    sizes: HashMap<OsString, u64>,
    /// The scan is still running, so the sizes may grow.
    pub partial: bool,
}

impl FolderSizes {
    pub fn get(&self, path: &Path) -> Option<u64> {
        self.sizes.get(path.file_name()?).copied()
    }
}

impl Scans {
    fn register(&mut self, scan: &Arc<Scan>) {
        self.scans.retain(|s| s.strong_count() > 0);
        self.scans.push(Arc::downgrade(scan));
    }

    /// A kept scan of exactly `root` for a disk usage tab to take over: one
    /// whose tab closed, or the browser's. A running one only when `running`
    /// is allowed, and never one stopped early.
    fn take(&mut self, root: &Path, running: bool) -> Option<ScanHandle> {
        self.take_closed();
        let at = self.own.iter().position(|o| {
            !o.handle.cancelled()
                && (running || !o.handle.running())
                && o.handle.tree().root_path == root
        })?;
        let own = self.own.remove(at);
        let handle = own.handle;
        if !handle.running() {
            let ids: Vec<u32> = {
                let tree = handle.tree();
                own.changed
                    .iter()
                    .filter_map(|dir| tree.find(dir))
                    .collect()
            };
            for id in ids {
                handle.refresh(id);
            }
        }
        Some(handle)
    }

    /// Takes in the scans of disk usage tabs that closed.
    fn take_closed(&mut self) {
        let closed = std::mem::take(&mut *self.closed.lock().unwrap());
        let now = Instant::now();
        for handle in closed {
            let root = handle.tree().root_path.clone();
            // The newer scan of a folder replaces an older one.
            self.own
                .retain(|o| o.handle.running() || o.handle.tree().root_path != root);
            self.own.push(Owned {
                handle,
                used: now,
                keep: UNUSED_CLOSED,
                changed: HashSet::new(),
            });
        }
    }

    /// Sizes for the folders inside `dir`, from the innermost scan that
    /// covers it; with none, starts one when [`Self::enabled`].
    pub fn folder_sizes(&mut self, ctx: &egui::Context, dir: &Path) -> Option<Arc<FolderSizes>> {
        let now = Instant::now();
        let tabs: Vec<Arc<Scan>> = self.scans.iter().filter_map(Weak::upgrade).collect();
        let covering = |scan: &Scan| {
            let tree = scan.tree();
            dir.starts_with(&tree.root_path)
                .then(|| tree.root_path.components().count())
        };
        let from_tab = tabs
            .iter()
            .filter_map(|s| Some((covering(s)?, s.clone())))
            .max_by_key(|(depth, _)| *depth);
        let own = self
            .own
            .iter_mut()
            .filter_map(|o| Some((covering(&o.handle)?, o)))
            .max_by_key(|(depth, _)| *depth);
        let scan = match (from_tab, own) {
            (Some((tab_depth, tab)), Some((own_depth, own))) => {
                own.used = now;
                if own_depth > tab_depth {
                    own.handle.0.clone()
                } else {
                    tab
                }
            }
            (Some((_, tab)), None) => tab,
            (None, Some((_, own))) => {
                own.used = now;
                own.handle.0.clone()
            }
            (None, None) => {
                if !self.enabled || self.is_remote(dir) {
                    return None;
                }
                let scan = Scan::start(ctx, dir.to_path_buf());
                self.own.push(Owned {
                    handle: ScanHandle(scan.clone()),
                    used: now,
                    keep: UNUSED_FINISHED,
                    changed: HashSet::new(),
                });
                scan
            }
        };
        let identity = Arc::as_ptr(&scan) as usize;
        let tree = scan.tree();
        let running = scan.running();
        if let Some(cached) = self.cache.iter_mut().find(|c| c.dir == dir) {
            cached.used = now;
            if cached.scan == identity
                && cached.generation == tree.generation
                && cached.sizes.partial == running
            {
                return Some(cached.sizes.clone());
            }
        }
        let id = tree.find(dir)?;
        let sizes = tree
            .children(id)
            .filter(|&c| tree.node(c).kind == Kind::Dir)
            .map(|c| {
                let node = tree.node(c);
                (node.name.to_os_string(), node.size(Metric::Apparent))
            })
            .collect();
        let sizes = Arc::new(FolderSizes {
            sizes,
            partial: running,
        });
        let cached = Cached {
            dir: dir.to_path_buf(),
            scan: identity,
            generation: tree.generation,
            sizes: sizes.clone(),
            used: now,
        };
        match self.cache.iter_mut().find(|c| c.dir == dir) {
            Some(old) => *old = cached,
            None => self.cache.push(cached),
        }
        Some(sizes)
    }

    /// Whether `dir` is on a network or FUSE mount, remembered.
    fn is_remote(&mut self, dir: &Path) -> bool {
        if let Some(&remote) = self.remote.get(dir) {
            return remote;
        }
        if self.remote.len() >= 256 {
            self.remote.clear();
        }
        // Over SFTP, or a network or FUSE mount.
        let remote = crate::remote::is_remote(dir) || crate::properties::platform::is_remote(dir);
        self.remote.insert(dir.to_path_buf(), remote);
        remote
    }

    /// Once a frame: stops browser scans nothing shows any more, and looks
    /// at folders that changed once their scan is done.
    pub fn tick(&mut self) {
        self.cache
            .retain(|c| c.used.elapsed() < Duration::from_secs(1));
        self.take_closed();
        if !self.enabled {
            self.own.retain(|o| o.keep == UNUSED_CLOSED);
        }
        self.own.retain(|o| {
            let limit = if o.handle.running() {
                UNUSED_RUNNING
            } else {
                o.keep
            };
            o.used.elapsed() < limit
        });
        // Most recently shown first; the oldest finished ones go.
        self.own.sort_by_key(|o| std::cmp::Reverse(o.used));
        let mut finished = 0;
        self.own.retain(|o| {
            let in_use = o.used.elapsed() < Duration::from_secs(1);
            if in_use || o.handle.running() {
                return true;
            }
            finished += 1;
            finished <= KEPT_FINISHED
        });
        for own in &mut self.own {
            if own.changed.is_empty() || own.handle.running() {
                continue;
            }
            let ids: Vec<u32> = {
                let tree = own.handle.tree();
                own.changed
                    .drain()
                    .filter_map(|dir| tree.find(&dir))
                    .filter(|&id| tree.node(id).is_dir())
                    .collect()
            };
            for id in ids {
                own.handle.refresh(id);
            }
        }
    }

    /// Folders the file watcher saw change: browser scans list them again.
    pub fn dirs_changed(&mut self, dirs: &HashSet<PathBuf>) {
        for own in &mut self.own {
            let root = own.handle.tree().root_path.clone();
            own.changed
                .extend(dirs.iter().filter(|d| d.starts_with(&root)).cloned());
        }
    }

    /// Something finished that may have changed folders anywhere (a copy,
    /// a move, emptying the trash): browser scans start over when next shown.
    pub fn invalidate(&mut self) {
        self.own.clear();
    }
}

#[cfg(test)]
mod tests {
    use egui::{Context, Event, PointerButton, Pos2, pos2};

    use super::*;
    use crate::{archive, images::ImageCache, jobs::Jobs, preview::TextCache, video::Videos};

    fn shared(ctx: &Context) -> Shared {
        Shared {
            show_hidden: false,
            show_thumbnails: false,
            icon_size: 64.0,
            clipboard: None,
            focused_tab: Some(1),
            preview_shown: false,
            app_actions: Vec::new(),
            usage: Default::default(),
            properties: Default::default(),
            images: ImageCache::new(ctx),
            videos: Videos::new(ctx),
            texts: TextCache::default(),
            jobs: Jobs::new(ctx),
            archives: archive::Listings::new(ctx),
            archive_prefs: archive::Prefs::default(),
            open_with: Default::default(),
            editor: Default::default(),
        }
    }

    /// `big/` holds most of the space, in a few files and a subfolder.
    fn fixture(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("egui-files-usage-{name}-{}", std::process::id()));
        let _ = disk::remove_dir_all(&root);
        disk::create_dir_all(root.join("big/inner")).unwrap();
        disk::create_dir_all(root.join("small")).unwrap();
        for i in 0..6 {
            disk::write(root.join(format!("big/movie{i}.mkv")), vec![0; 200_000]).unwrap();
        }
        disk::write(root.join("big/inner/photo.jpg"), vec![0; 300_000]).unwrap();
        for i in 0..20 {
            disk::write(root.join(format!("small/note{i}.txt")), vec![0; 5_000]).unwrap();
        }
        root
    }

    struct Harness {
        ctx: Context,
        shared: Shared,
        tab: UsageTab,
        time: f64,
    }

    impl Harness {
        fn frame(&mut self, events: Vec<Event>) {
            self.time += 0.05;
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1200.0, 700.0))),
                time: Some(self.time),
                events,
                ..Default::default()
            };
            let (tab, shared) = (&mut self.tab, &mut self.shared);
            let mut output = self.ctx.run_ui(input, |ui| tab.ui(ui, shared));
            output.textures_delta.clear();
        }

        fn wait_for_scan(&mut self) {
            let start = Instant::now();
            while self.tab.scan.as_ref().is_some_and(|s| s.running()) {
                assert!(start.elapsed() < Duration::from_secs(10));
                std::thread::sleep(Duration::from_millis(5));
            }
            // Past the relayout throttle, so the charts show the whole scan.
            std::thread::sleep(RELAYOUT);
            self.frame(Vec::new());
            self.frame(Vec::new());
        }

        fn click(&mut self, pos: Pos2) {
            self.time += 1.0;
            self.frame(vec![Event::PointerMoved(pos)]);
            for pressed in [true, false] {
                self.frame(vec![Event::PointerButton {
                    pos,
                    button: PointerButton::Primary,
                    pressed,
                    modifiers: Modifiers::NONE,
                }]);
            }
        }

        fn double_click(&mut self, pos: Pos2) {
            // Long after earlier clicks, or this would count as a triple click.
            self.time += 1.0;
            self.frame(vec![Event::PointerMoved(pos)]);
            let mut events = Vec::new();
            for _ in 0..2 {
                for pressed in [true, false] {
                    events.push(Event::PointerButton {
                        pos,
                        button: PointerButton::Primary,
                        pressed,
                        modifiers: Modifiers::NONE,
                    });
                }
            }
            self.frame(events);
            self.frame(Vec::new());
        }

        fn focus_path(&self) -> PathBuf {
            self.tab.scan.as_ref().unwrap().tree().path(self.tab.focus)
        }
    }

    #[test]
    fn closed_tabs_scan_is_kept() {
        let root = fixture("kept").join("big");
        let ctx = Context::default();
        let mut h = Harness {
            shared: shared(&ctx),
            ctx,
            tab: UsageTab::new(1, root.clone()),
            time: 0.0,
        };
        h.frame(Vec::new());
        h.wait_for_scan();
        let first = Arc::as_ptr(&h.tab.scan.as_ref().unwrap().0);

        // Closing hands it over; a new tab of the folder takes it, unstopped.
        h.tab = UsageTab::new(2, root.clone());
        h.shared.usage.tick();
        h.frame(Vec::new());
        let scan = h.tab.scan.as_ref().unwrap();
        assert_eq!(Arc::as_ptr(&scan.0), first);
        assert!(!scan.cancelled());
        // While the browser shows sizes from it.
        assert!(h.shared.usage.folder_sizes(&h.ctx, &root).is_some());

        // Scanning again doesn't.
        h.tab.start(&h.ctx.clone(), &mut h.shared, false);
        assert_ne!(Arc::as_ptr(&h.tab.scan.as_ref().unwrap().0), first);
    }

    #[test]
    fn chart_fills_down_to_the_legend() {
        let root = fixture("legend").join("big");
        let ctx = Context::default();
        let mut h = Harness {
            shared: shared(&ctx),
            ctx,
            tab: UsageTab::new(1, root),
            time: 0.0,
        };
        h.frame(Vec::new());
        h.wait_for_scan();
        let chart = |h: &Harness| h.tab.treemap.items[0].rect;
        let before = chart(&h);
        // Only a row of legend under it.
        assert!(700.0 - before.bottom() < 60.0, "{before:?}");

        // A minimized window's empty frames change nothing.
        let input = egui::RawInput {
            screen_rect: Some(Rect::ZERO),
            time: Some(h.time + 0.05),
            ..Default::default()
        };
        let (tab, shared) = (&mut h.tab, &mut h.shared);
        let mut output = h.ctx.run_ui(input, |ui| tab.ui(ui, shared));
        output.textures_delta.clear();
        h.time += 0.05;
        h.frame(Vec::new());
        h.frame(Vec::new());
        assert_eq!(chart(&h), before);
    }

    #[test]
    fn every_view_draws_and_zooms() {
        let root = fixture("views");
        let ctx = Context::default();
        let mut h = Harness {
            shared: shared(&ctx),
            ctx,
            tab: UsageTab::new(1, root.clone()),
            time: 0.0,
        };
        h.frame(Vec::new());
        h.wait_for_scan();
        {
            let tree = h.tab.scan.as_ref().unwrap().tree();
            assert_eq!(tree.node(ROOT).totals.apparent, 1_600_000);
        }

        // Folder sizes reach the Details view.
        let sizes = h
            .shared
            .usage
            .folder_sizes(&h.ctx, &root)
            .expect("sizes for the scanned folder");
        assert_eq!(sizes.get(&root.join("big")), Some(1_500_000));
        assert!(!sizes.partial);

        for view in View::ALL {
            for color in ColorMode::ALL {
                h.tab.view = view;
                h.tab.color = color;
                h.frame(Vec::new());
                h.frame(vec![Event::PointerMoved(pos2(800.0, 350.0))]);
            }
        }

        // Somewhere in the middle of each chart is inside `big`, which takes
        // most of the space; double-clicking zooms into it (or deeper).
        for view in [View::Treemap, View::Sunburst, View::Icicle] {
            h.tab.view = view;
            h.tab.focus = ROOT;
            h.frame(Vec::new());
            std::thread::sleep(RELAYOUT);
            h.frame(Vec::new());
            let point = match view {
                // The header of the biggest folder.
                View::Treemap => {
                    let item = h.tab.treemap.items.iter().find(|i| i.header).unwrap();
                    item.rect.left_top() + vec2(10.0, 4.0)
                }
                // The first ring, just past the hole, clockwise from the top.
                View::Sunburst => {
                    let spans = &h.tab.partition.spans;
                    assert!(spans.iter().any(|s| s.depth == 1));
                    let key = h.tab.partition.key.clone().unwrap();
                    let rect = key.view.2;
                    let radius = rect.width().min(rect.height()) / 2.0 - 6.0;
                    let hole = (radius * 0.2).max(36.0).min(radius * 0.5);
                    rect.center() + vec2(8.0, -(hole + 10.0))
                }
                _ => {
                    let rect = h.tab.partition.key.clone().unwrap().view.2;
                    pos2(rect.left() + 26.0 + 20.0, rect.top() + 20.0)
                }
            };
            h.double_click(point);
            assert!(
                h.focus_path().starts_with(root.join("big")),
                "{view:?}: focus is {}",
                h.focus_path().display()
            );
            // Backspace goes back up.
            while h.tab.focus != ROOT {
                h.frame(vec![Event::Key {
                    key: Key::Backspace,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: Modifiers::NONE,
                }]);
            }
        }

        // A click in the treemap selects and expands the tree to it.
        h.tab.view = View::Treemap;
        h.frame(Vec::new());
        let point = h
            .tab
            .treemap
            .items
            .iter()
            .find(|i| i.header)
            .unwrap()
            .rect
            .center();
        h.click(point);
        let selected = h.tab.selected.expect("something selected");
        let tree = h.tab.scan.as_ref().unwrap().tree();
        assert!(tree.path(selected).starts_with(root.join("big")));
        let parent = tree.node(selected).parent;
        assert!(h.tab.expanded.contains(&parent));
        drop(tree);
        disk::remove_dir_all(root).unwrap();
    }

    /// `EGUI_FILES_USAGE_DIR=/usr cargo test --release usage_timing -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn usage_timing() {
        let root = PathBuf::from(std::env::var("EGUI_FILES_USAGE_DIR").unwrap_or("/usr".into()));
        let ctx = Context::default();
        let mut h = Harness {
            shared: shared(&ctx),
            ctx,
            tab: UsageTab::new(1, root),
            time: 0.0,
        };
        let start = Instant::now();
        h.frame(Vec::new());
        h.wait_for_scan();
        let tree = h.tab.scan.as_ref().unwrap().tree();
        eprintln!(
            "scanned {} nodes, {} in {:?}",
            tree.node(ROOT).totals.files,
            crate::fs::human_size(tree.node(ROOT).totals.disk),
            start.elapsed()
        );
        drop(tree);
        for view in View::ALL {
            h.tab.view = view;
            // First frame lays out; later ones only draw.
            let start = Instant::now();
            h.frame(vec![Event::PointerMoved(pos2(800.0, 350.0))]);
            let first = start.elapsed();
            let start = Instant::now();
            for _ in 0..10 {
                h.frame(vec![Event::PointerMoved(pos2(800.0, 350.0))]);
            }
            eprintln!(
                "{view:?}: first frame {first:?}, then {:?} each",
                start.elapsed() / 10
            );
        }
    }
}
