//! The disk usage tab's tables: the folder tree with share bars (WizTree's
//! left side), the file type breakdown, and the largest files.

use std::{
    collections::{BinaryHeap, HashMap, HashSet},
    ffi::OsStr,
    path::Path,
    time::{Duration, SystemTime},
};

use egui::{
    Align, Button, Color32, CornerRadius, Layout, Rect, RichText, Sense, TextStyle, Ui, vec2,
};
use egui_extras::TableBuilder;
use egui_phosphor::regular as icon;

use super::{
    Act, LayoutKey,
    colors::Painter,
    scan::{CATEGORIES, Category, Kind, Metric, NONE, ROOT, Tree},
};
use crate::list_columns::{self, Spec};

fn row_height(ui: &Ui) -> f32 {
    ui.text_style_height(&TextStyle::Body) + 6.0
}

fn time(seconds: i64) -> Option<SystemTime> {
    (seconds > 0).then(|| SystemTime::UNIX_EPOCH + Duration::from_secs(seconds as u64))
}

fn node_icon(tree: &Tree, id: u32) -> &'static str {
    let node = tree.node(id);
    match node.kind {
        Kind::Dir if node.unreadable => icon::FOLDER_LOCK,
        Kind::Dir => icon::FOLDER,
        Kind::Mount => icon::HARD_DRIVES,
        Kind::Symlink => icon::LINK,
        Kind::File(_) => crate::fs::icon_for(Path::new(&*node.name), false, false),
    }
}

/// A share of something as a thin bar with its percentage.
fn share_bar(ui: &mut Ui, share: f64, color: Color32) {
    let height = 8.0;
    let width = (ui.available_width() - 44.0).max(10.0);
    let (rect, _) = ui.allocate_exact_size(vec2(width, height), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(4), ui.visuals().faint_bg_color);
    let filled = Rect::from_min_size(rect.min, vec2(rect.width() * share as f32, height));
    if filled.width() >= 1.0 {
        p.rect_filled(filled, CornerRadius::same(4), color);
    }
    ui.label(RichText::new(format!("{:.1} %", share * 100.0)).small());
}

static TREE_COLUMNS: list_columns::Table = list_columns::Table {
    key: "usage-tree",
    columns: &[
        Spec {
            label: "Name",
            width: 0.0,
            min: 140.0,
        },
        Spec {
            label: "Share",
            width: 110.0,
            min: 70.0,
        },
        Spec {
            label: "Size",
            width: 76.0,
            min: 60.0,
        },
        Spec {
            label: "Files",
            width: 64.0,
            min: 44.0,
        },
        Spec {
            label: "Newest",
            width: 120.0,
            min: 70.0,
        },
    ],
    hide_order: &[4, 3, 1, 2],
    hidden: &[],
};

#[derive(Default)]
pub struct TreeTable {
    key: Option<LayoutKey>,
    rows: Vec<(u32, u16)>,
}

/// The folder tree from the scanned folder down, with the expanded folders'
/// contents biggest first.
#[allow(clippy::too_many_arguments)]
pub fn tree_table(
    ui: &mut Ui,
    salt: u64,
    state: &mut TreeTable,
    painter: &Painter,
    expanded: &HashSet<u32>,
    expansion: u64,
    selected: Option<u32>,
    scroll_to: Option<u32>,
    acts: &mut Vec<Act>,
) {
    let tree = painter.tree;
    let metric = painter.metric;
    let key = LayoutKey::new(tree, ROOT, metric, Rect::NOTHING).with(expansion);
    if super::refresh(&mut state.key, key, ui.ctx()) {
        state.rows.clear();
        let mut stack = vec![(ROOT, 0)];
        while let Some((id, depth)) = stack.pop() {
            state.rows.push((id, depth));
            if expanded.contains(&id) {
                let children = tree.sorted_children(id, metric);
                stack.extend(children.into_iter().rev().map(|c| (c, depth + 1)));
            }
        }
    }
    let height = row_height(ui);
    let columns = list_columns::Layout::new(ui, &TREE_COLUMNS);
    let colors = crate::theme::colors(ui.ctx());
    colors.apply_to_list(ui.visuals_mut());
    let mut table = TableBuilder::new(ui)
        .id_salt(("usage-tree", salt))
        .striped(true)
        .sense(Sense::click())
        .cell_layout(Layout::left_to_right(Align::Center));
    table = columns.columns(table);
    if let Some(row) = scroll_to.and_then(|id| state.rows.iter().position(|r| r.0 == id)) {
        table = table.scroll_to_row(row, Some(Align::Center));
    }
    let output = table
        .header(height, |mut header| {
            for (column, spec) in TREE_COLUMNS.columns.iter().enumerate() {
                if columns.shown(column) {
                    let (_, response) = header.col(|ui| {
                        ui.label(RichText::new(spec.label).strong());
                    });
                    columns.context_menu(&response);
                }
            }
        })
        .body(|body| {
            body.rows(height, state.rows.len(), |mut row| {
                let (id, depth) = state.rows[row.index()];
                let node = tree.node(id);
                row.set_selected(selected == Some(id));
                let color = painter.color(id);
                row.col(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    ui.add_space(depth as f32 * 12.0);
                    let has_children = node.is_dir() && node.first_child != NONE;
                    let caret = if expanded.contains(&id) {
                        icon::CARET_DOWN
                    } else {
                        icon::CARET_RIGHT
                    };
                    if has_children {
                        if ui
                            .add(Button::new(RichText::new(caret).small()).frame(false))
                            .clicked()
                        {
                            acts.push(Act::ToggleExpand(id));
                        }
                    } else {
                        ui.add_space(14.0);
                    }
                    ui.label(RichText::new(node_icon(tree, id)).color(color));
                    let mut name = RichText::new(tree.name(id));
                    if node.unreadable || node.kind == Kind::Mount {
                        name = name.weak().italics();
                    }
                    let label = ui.add(egui::Label::new(name).truncate());
                    if node.unreadable {
                        label.on_hover_text("Couldn't be read");
                    } else if node.kind == Kind::Mount {
                        label.on_hover_text("Another drive, mounted here; not scanned");
                    }
                });
                if columns.shown(1) {
                    row.col(|ui| {
                        let parent = node.parent;
                        let whole = if parent == NONE {
                            node.size(metric)
                        } else {
                            tree.node(parent).size(metric)
                        };
                        let share = node.size(metric) as f64 / whole.max(1) as f64;
                        share_bar(ui, share, color);
                    });
                }
                if columns.shown(2) {
                    row.col(|ui| {
                        ui.label(crate::fs::human_size(node.size(metric)));
                    });
                }
                if columns.shown(3) {
                    row.col(|ui| {
                        if node.is_dir() {
                            ui.label(RichText::new(count(node.totals.files)).weak());
                        }
                    });
                }
                if columns.shown(4) {
                    row.col(|ui| {
                        crate::dates::label(ui, time(node.newest), true);
                    });
                }
                row_interaction(&row.response(), tree, id, acts);
            });
        });
    columns.resize_handles(ui, output.inner_rect.bottom());
}

fn row_interaction(response: &egui::Response, tree: &Tree, id: u32, acts: &mut Vec<Act>) {
    if response.hovered() {
        acts.push(Act::Hover(id));
    }
    if response.double_clicked() {
        acts.push(Act::Activate(id));
    } else if response.clicked() || response.secondary_clicked() {
        acts.push(Act::Select(id));
    }
    response.context_menu(|ui| super::node_menu(ui, tree, id, acts));
}

/// 1 234 567 → "1.2 M".
pub fn count(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{:.1} k", n as f64 / 1e3),
        _ => format!("{:.1} M", n as f64 / 1e6),
    }
}

/// What's inside the focused folder, by type and by size, found by walking it.
#[derive(Default)]
pub struct Breakdown {
    key: Option<LayoutKey>,
    pub files: [u64; CATEGORIES],
    /// Extensions per category, biggest first.
    pub extensions: Vec<Vec<(String, u64, u64)>>,
    /// The biggest files, biggest first.
    pub largest: Vec<u32>,
    /// Categories showing their extensions.
    pub open: HashSet<Category>,
    /// How long the last walk took: walks of huge trees are redone less
    /// often while the scan runs.
    took: Duration,
}

const LARGEST: usize = 500;

impl Breakdown {
    pub fn update(&mut self, ui: &Ui, tree: &Tree, focus: u32, metric: Metric) {
        let key = LayoutKey::new(tree, focus, metric, Rect::NOTHING);
        let every = super::RELAYOUT.max(self.took * 10);
        if !super::refresh_every(&mut self.key, key, ui.ctx(), every) {
            return;
        }
        let start = std::time::Instant::now();
        let mut files = [0; CATEGORIES];
        let mut by_extension: HashMap<(Category, String), (u64, u64)> = HashMap::new();
        let mut largest = BinaryHeap::new();
        let mut stack = vec![focus];
        while let Some(id) = stack.pop() {
            let node = tree.node(id);
            match node.kind {
                Kind::Dir => stack.extend(tree.children(id)),
                Kind::File(category) => {
                    let size = node.size(metric);
                    files[category.index()] += 1;
                    let extension = Path::new(&*node.name)
                        .extension()
                        .and_then(OsStr::to_str)
                        .map(str::to_ascii_lowercase)
                        .unwrap_or_default();
                    let entry = by_extension.entry((category, extension)).or_default();
                    entry.0 += size;
                    entry.1 += 1;
                    largest.push(std::cmp::Reverse((size, id)));
                    if largest.len() > LARGEST {
                        largest.pop();
                    }
                }
                Kind::Symlink | Kind::Mount => {}
            }
        }
        let mut extensions = vec![Vec::new(); CATEGORIES];
        for ((category, extension), (size, n)) in by_extension {
            extensions[category.index()].push((extension, size, n));
        }
        for list in &mut extensions {
            list.sort_by_key(|e| std::cmp::Reverse(e.1));
        }
        self.files = files;
        self.extensions = extensions;
        self.largest = largest
            .into_sorted_vec()
            .into_iter()
            .map(|r| r.0.1)
            .collect();
        self.took = start.elapsed();
    }
}

/// A stacked bar of the categories, then a table of them with their
/// extensions; clicking one highlights it in the charts.
pub fn types(
    ui: &mut Ui,
    breakdown: &mut Breakdown,
    painter: &Painter,
    focus: u32,
    acts: &mut Vec<Act>,
) {
    let tree = painter.tree;
    let metric = painter.metric;
    let totals = tree.node(focus).totals;
    let total = totals.size(metric).max(1);
    let mut categories: Vec<Category> = Category::ALL
        .into_iter()
        .filter(|&c| totals.category(metric, c) > 0)
        .collect();
    categories.sort_by_key(|&c| std::cmp::Reverse(totals.category(metric, c)));

    ui.add_space(8.0);
    let (bar, _) = ui.allocate_exact_size(vec2(ui.available_width(), 22.0), Sense::hover());
    let gap = 2.0;
    let usable = bar.width() - gap * categories.len().saturating_sub(1) as f32;
    let mut x = bar.left();
    for &category in &categories {
        let share = totals.category(metric, category) as f64 / total as f64;
        let width = (usable * share as f32).max(1.0);
        let rect = Rect::from_min_size(egui::pos2(x, bar.top()), vec2(width, bar.height()));
        let dim = painter.highlight.is_some_and(|h| h != category);
        let color = painter.palette.category(category);
        let color = if dim {
            color.gamma_multiply(0.25)
        } else {
            color
        };
        ui.painter().rect_filled(rect, CornerRadius::same(4), color);
        let response = ui.interact(rect, ui.id().with(("type-bar", category)), Sense::click());
        response.clone().on_hover_text(format!(
            "{} — {} ({:.1} %)",
            category.label(),
            crate::fs::human_size(totals.category(metric, category)),
            share * 100.0
        ));
        if response.clicked() {
            acts.push(Act::Highlight(category));
        }
        x += width + gap;
    }
    ui.add_space(8.0);

    let height = row_height(ui);
    let mut rows: Vec<(Category, Option<usize>)> = Vec::new();
    for &category in &categories {
        rows.push((category, None));
        if breakdown.open.contains(&category) {
            let n = breakdown.extensions[category.index()].len().min(30);
            rows.extend((0..n).map(|i| (category, Some(i))));
        }
    }
    let mut toggle = None;
    TableBuilder::new(ui)
        .id_salt("usage-types")
        .striped(true)
        .sense(Sense::click())
        .cell_layout(Layout::left_to_right(Align::Center))
        .column(egui_extras::Column::remainder().at_least(160.0))
        .column(egui_extras::Column::exact(160.0))
        .column(egui_extras::Column::exact(80.0))
        .column(egui_extras::Column::exact(70.0))
        .header(height, |mut header| {
            for label in ["Type", "Share", "Size", "Files"] {
                header.col(|ui| {
                    ui.label(RichText::new(label).strong());
                });
            }
        })
        .body(|body| {
            body.rows(height, rows.len(), |mut row| {
                let (category, extension) = rows[row.index()];
                let color = painter.palette.category(category);
                let (size, files, label) = match extension {
                    None => (
                        totals.category(metric, category),
                        breakdown.files[category.index()],
                        format!("{}  {}", category.icon(), category.label()),
                    ),
                    Some(i) => {
                        let (ext, size, n) = &breakdown.extensions[category.index()][i];
                        let name = if ext.is_empty() {
                            "(no extension)".to_owned()
                        } else {
                            format!(".{ext}")
                        };
                        (*size, *n, name)
                    }
                };
                row.set_selected(extension.is_none() && painter.highlight == Some(category));
                row.col(|ui| match extension {
                    None => {
                        let open = breakdown.open.contains(&category);
                        let caret = if open {
                            icon::CARET_DOWN
                        } else {
                            icon::CARET_RIGHT
                        };
                        if ui
                            .add(Button::new(RichText::new(caret).small()).frame(false))
                            .clicked()
                        {
                            toggle = Some(category);
                        }
                        let (swatch, _) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
                        ui.painter()
                            .rect_filled(swatch, CornerRadius::same(2), color);
                        ui.label(label);
                    }
                    Some(_) => {
                        ui.add_space(40.0);
                        ui.label(RichText::new(label).weak());
                    }
                });
                row.col(|ui| share_bar(ui, size as f64 / total as f64, color));
                row.col(|ui| {
                    ui.label(crate::fs::human_size(size));
                });
                row.col(|ui| {
                    ui.label(RichText::new(count(files)).weak());
                });
                let response = row.response();
                if response.clicked() {
                    acts.push(Act::Highlight(category));
                }
                if response.double_clicked() && extension.is_none() {
                    toggle = Some(category);
                }
            });
        });
    if let Some(category) = toggle
        && !breakdown.open.remove(&category)
    {
        breakdown.open.insert(category);
    }
}

/// The biggest files anywhere inside the focused folder.
pub fn largest(
    ui: &mut Ui,
    breakdown: &Breakdown,
    painter: &Painter,
    focus: u32,
    selected: Option<u32>,
    acts: &mut Vec<Act>,
) {
    let tree = painter.tree;
    let metric = painter.metric;
    let height = row_height(ui);
    let base = tree.path(focus);
    let colors = crate::theme::colors(ui.ctx());
    colors.apply_to_list(ui.visuals_mut());
    TableBuilder::new(ui)
        .id_salt("usage-largest")
        .striped(true)
        .sense(Sense::click())
        .cell_layout(Layout::left_to_right(Align::Center))
        .column(
            egui_extras::Column::initial(260.0)
                .at_least(120.0)
                .clip(true),
        )
        .column(egui_extras::Column::exact(80.0))
        .column(egui_extras::Column::remainder().at_least(120.0).clip(true))
        .column(egui_extras::Column::exact(130.0))
        .header(height, |mut header| {
            for label in ["Name", "Size", "Folder", "Modified"] {
                header.col(|ui| {
                    ui.label(RichText::new(label).strong());
                });
            }
        })
        .body(|body| {
            body.rows(height, breakdown.largest.len(), |mut row| {
                let id = breakdown.largest[row.index()];
                if !tree.is_within(id, focus) {
                    return;
                }
                let node = tree.node(id);
                row.set_selected(selected == Some(id));
                row.col(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    ui.label(RichText::new(node_icon(tree, id)).color(painter.color(id)));
                    ui.add(egui::Label::new(tree.name(id)).truncate());
                });
                row.col(|ui| {
                    ui.label(crate::fs::human_size(node.size(metric)));
                });
                row.col(|ui| {
                    let folder = tree.path(node.parent);
                    let shown = folder.strip_prefix(&base).unwrap_or(&folder);
                    let text = if shown.as_os_str().is_empty() {
                        ".".to_owned()
                    } else {
                        shown.display().to_string()
                    };
                    ui.add(egui::Label::new(RichText::new(text).weak()).truncate());
                });
                row.col(|ui| {
                    crate::dates::label(ui, time(node.newest), true);
                });
                row_interaction(&row.response(), tree, id, acts);
            });
        });
}
