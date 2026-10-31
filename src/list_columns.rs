//! Columns of the list tables (details view, trash, archive). The name column
//! always shows and takes whatever width is left; the others have widths and
//! visibility the user sets, remembered per kind of list. When the list is too
//! narrow for them, the other columns first shrink toward their minimums, then
//! the least important ones are hidden, so the table never overflows.
//!
//! egui_extras' own resizing keeps a column's width no matter how narrow the
//! table gets, so the table gets exact widths from here and the resize handles
//! are drawn by [`Layout::resize_handles`].

use egui::{Checkbox, CursorIcon, Id, Rect, Response, Sense, Ui, pos2};
use egui_extras::{Column, TableBuilder};
use serde::{Deserialize, Serialize};

pub struct Spec {
    pub label: &'static str,
    /// Default width; unused for the name column.
    pub width: f32,
    pub min: f32,
}

/// One kind of list. `columns[0]` is the name column.
pub struct Table {
    pub key: &'static str,
    pub columns: &'static [Spec],
    /// Indices of the columns to hide, in order, when even their minimum
    /// widths don't fit. Should list every column but the name.
    pub hide_order: &'static [usize],
    /// Columns hidden until the user shows them.
    pub hidden: &'static [usize],
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Prefs {
    /// Widths the user chose; index 0 (the name column) is unused.
    widths: Vec<f32>,
    hidden: Vec<bool>,
}

impl Prefs {
    fn new(table: &Table) -> Self {
        Self {
            widths: table.columns.iter().map(|c| c.width).collect(),
            hidden: (0..table.columns.len())
                .map(|i| table.hidden.contains(&i))
                .collect(),
        }
    }
}

/// This frame's column widths for one table.
pub struct Layout {
    table: &'static Table,
    prefs_id: Id,
    prefs: Prefs,
    /// Displayed width of each column, `None` when hidden.
    widths: Vec<Option<f32>>,
    spacing: f32,
    /// Width the table's columns share.
    available: f32,
    /// Top left of the table.
    origin: egui::Pos2,
}

impl Layout {
    /// Lays out the columns for the space left in `ui`. Call it right before
    /// building the table.
    pub fn new(ui: &Ui, table: &'static Table) -> Self {
        let prefs_id = Id::new(("list-columns", table.key));
        let n = table.columns.len();
        let mut prefs = ui
            .data_mut(|d| d.get_persisted::<Prefs>(prefs_id))
            .filter(|p| p.widths.len() == n && p.hidden.len() == n)
            .unwrap_or_else(|| Prefs::new(table));
        prefs.hidden[0] = false;
        for (width, spec) in prefs.widths.iter_mut().zip(table.columns) {
            *width = width.max(spec.min);
        }
        let rect = ui.available_rect_before_wrap();
        // The table leaves room for its scroll bar.
        let available = rect.width() - ui.spacing().scroll.allocated_width();
        let spacing = ui.spacing().item_spacing.x;
        Self {
            widths: fit(table, &prefs, available, spacing),
            table,
            prefs_id,
            prefs,
            spacing,
            available,
            origin: rect.min,
        }
    }

    pub fn shown(&self, column: usize) -> bool {
        self.widths[column].is_some()
    }

    /// Adds the shown columns to `builder`.
    pub fn columns<'a>(&self, mut builder: TableBuilder<'a>) -> TableBuilder<'a> {
        let name = &self.table.columns[0];
        builder = builder.column(
            Column::remainder()
                .at_least(name.min)
                .clip(true)
                .resizable(false),
        );
        for width in self.widths.iter().skip(1).flatten() {
            builder = builder.column(Column::exact(*width));
        }
        builder
    }

    /// The header's right-click menu: which columns to show.
    pub fn context_menu(&self, response: &Response) {
        response.context_menu(|ui| {
            let mut prefs = self.prefs.clone();
            for (i, spec) in self.table.columns.iter().enumerate() {
                let mut shown = !prefs.hidden[i];
                let checkbox = Checkbox::new(&mut shown, spec.label);
                if ui.add_enabled(i > 0, checkbox).changed() {
                    prefs.hidden[i] = !shown;
                }
            }
            ui.separator();
            crate::dates::checkbox(ui);
            if ui.button("Reset columns").clicked() {
                prefs = Prefs::new(self.table);
                ui.close();
            }
            self.store(ui, prefs);
        });
    }

    /// Draggable column edges, for after the table is drawn down to `bottom`.
    /// The edge right of the name column resizes the column after it, and
    /// double-clicking an edge resets its column's width.
    pub fn resize_handles(&self, ui: &Ui, bottom: f32) {
        let shown: Vec<usize> = (1..self.widths.len()).filter(|&i| self.shown(i)).collect();
        let Some(&first) = shown.first() else {
            return;
        };
        let column_widths: f32 = shown.iter().filter_map(|&i| self.widths[i]).sum();
        let name_width =
            (self.available - column_widths - self.spacing * shown.len() as f32).floor();
        // How much any column can grow before the name column hits its minimum.
        let room = (name_width - self.table.columns[0].min).max(0.0);

        let mut x = self.origin.x + name_width + self.spacing * 0.5;
        let edges = std::iter::once((first, -1.0)).chain(shown.iter().map(|&i| (i, 1.0)));
        for (edge, (column, direction)) in edges.enumerate() {
            let width = self.widths[column].unwrap_or_default();
            if edge > 0 {
                x += width + self.spacing;
            }
            let line = Rect::from_min_max(pos2(x, self.origin.y), pos2(x, bottom));
            let response = ui.interact(
                line.expand2(egui::vec2(
                    ui.style().interaction.resize_grab_radius_side,
                    0.0,
                )),
                ui.id().with(("list-column-edge", self.table.key, edge)),
                Sense::click_and_drag(),
            );
            if response.double_clicked() {
                let mut prefs = self.prefs.clone();
                prefs.widths[column] = self.table.columns[column].width;
                self.store(ui, prefs);
            } else if response.dragged() {
                let delta = response.drag_delta().x * direction;
                let mut prefs = self.prefs.clone();
                // What's on screen becomes the chosen widths, so dragging one
                // column doesn't make the shrunk ones jump back.
                for &i in &shown {
                    prefs.widths[i] = self.widths[i].unwrap_or_default();
                }
                let min = self.table.columns[column].min;
                prefs.widths[column] = (width + delta).clamp(min, (width + room).max(min));
                self.store(ui, prefs);
            }
            let visuals = &ui.style().visuals.widgets;
            let stroke = if response.dragged() {
                visuals.active.bg_stroke
            } else if response.hovered() {
                visuals.hovered.bg_stroke
            } else {
                visuals.noninteractive.bg_stroke
            };
            if response.hovered() || response.dragged() {
                ui.ctx().set_cursor_icon(CursorIcon::ResizeColumn);
            }
            ui.painter().line_segment([line.min, line.max], stroke);
        }
    }

    fn store(&self, ui: &Ui, prefs: Prefs) {
        if prefs != self.prefs {
            ui.data_mut(|d| d.insert_persisted(self.prefs_id, prefs));
        }
    }
}

/// Displayed widths of the columns after the name column, which gets the rest.
fn fit(table: &Table, prefs: &Prefs, available: f32, spacing: f32) -> Vec<Option<f32>> {
    let mut shown: Vec<bool> = prefs.hidden.iter().map(|hidden| !hidden).collect();
    let mut hide_next = table.hide_order.iter();
    loop {
        let columns: Vec<usize> = (1..shown.len()).filter(|&i| shown[i]).collect();
        let room = available - table.columns[0].min - spacing * columns.len() as f32;
        let wanted: f32 = columns.iter().map(|&i| prefs.widths[i]).sum();
        let least: f32 = columns.iter().map(|&i| table.columns[i].min).sum();
        let hide = (least > room)
            .then(|| hide_next.find(|&&i| shown[i]))
            .flatten();
        if let Some(&i) = hide {
            shown[i] = false;
            continue;
        }
        // Take the shortfall from each column in proportion to how far it is
        // above its minimum.
        let shrink = if wanted > room {
            ((wanted - room) / (wanted - least)).clamp(0.0, 1.0)
        } else {
            0.0
        };
        return (0..shown.len())
            .map(|i| {
                let width = prefs.widths[i];
                let min = table.columns[i].min;
                shown[i].then_some(width - (width - min) * shrink)
            })
            .collect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static TABLE: Table = Table {
        key: "test",
        columns: &[
            Spec {
                label: "Name",
                width: 0.0,
                min: 160.0,
            },
            Spec {
                label: "Size",
                width: 90.0,
                min: 60.0,
            },
            Spec {
                label: "Modified",
                width: 140.0,
                min: 80.0,
            },
            Spec {
                label: "Kind",
                width: 100.0,
                min: 60.0,
            },
        ],
        hide_order: &[3, 2, 1],
        hidden: &[],
    };

    fn total(widths: &[Option<f32>], spacing: f32) -> f32 {
        let shown: Vec<f32> = widths.iter().skip(1).flatten().copied().collect();
        TABLE.columns[0].min + shown.iter().sum::<f32>() + spacing * shown.len() as f32
    }

    #[test]
    fn columns_keep_their_widths_when_there_is_room() {
        let widths = fit(&TABLE, &Prefs::new(&TABLE), 1000.0, 8.0);
        assert_eq!(widths, [Some(0.0), Some(90.0), Some(140.0), Some(100.0)]);
    }

    #[test]
    fn columns_shrink_before_hiding() {
        // Preferred needs 160 + 330 + 24 = 514; minimums need 160 + 200 + 24 = 384.
        let widths = fit(&TABLE, &Prefs::new(&TABLE), 450.0, 8.0);
        assert!(widths.iter().all(Option::is_some));
        assert!((total(&widths, 8.0) - 450.0).abs() < 0.01);
        for (width, spec) in widths.iter().zip(TABLE.columns).skip(1) {
            let width = width.unwrap();
            assert!(
                width >= spec.min && width < spec.width,
                "{}: {width}",
                spec.label
            );
        }
    }

    #[test]
    fn least_important_columns_hide_first() {
        let shown = |available| {
            fit(&TABLE, &Prefs::new(&TABLE), available, 8.0)
                .iter()
                .map(Option::is_some)
                .collect::<Vec<_>>()
        };
        // Without Kind the minimums need 160 + 140 + 16 = 316.
        assert_eq!(shown(350.0), [true, true, true, false]);
        assert_eq!(shown(300.0), [true, true, false, false]);
        assert_eq!(shown(200.0), [true, false, false, false]);
    }

    #[test]
    fn user_hidden_columns_stay_hidden() {
        let mut prefs = Prefs::new(&TABLE);
        prefs.hidden[2] = true;
        let widths = fit(&TABLE, &prefs, 1000.0, 8.0);
        assert_eq!(widths, [Some(0.0), Some(90.0), None, Some(100.0)]);
    }

    /// Draws a table like the details view in a window `width` points wide
    /// and returns the right edge of each shown column.
    fn draw(ctx: &egui::Context, width: f32, events: Vec<egui::Event>) -> Vec<f32> {
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(
                pos2(0.0, 0.0),
                egui::vec2(width, 400.0),
            )),
            events,
            ..Default::default()
        };
        let mut rights = Vec::new();
        let mut output = ctx.run_ui(input, |ui| {
            let columns = Layout::new(ui, &TABLE);
            let output = columns
                .columns(TableBuilder::new(ui).sense(Sense::click()))
                .header(20.0, |mut header| {
                    for column in 0..TABLE.columns.len() {
                        if columns.shown(column) {
                            rights.push(header.col(|ui| _ = ui.label("x")).1.rect.right());
                        }
                    }
                })
                .body(|_| {});
            columns.resize_handles(ui, output.inner_rect.bottom());
        });
        output.textures_delta.clear();
        rights
    }

    #[test]
    fn table_fits_the_window() {
        let ctx = egui::Context::default();
        for width in [1200.0, 450.0, 300.0, 1200.0] {
            // Cells report the rect they had last frame.
            draw(&ctx, width, Vec::new());
            let rights = draw(&ctx, width, Vec::new());
            let right = *rights.last().unwrap();
            assert!(
                right <= width && right > width - 30.0,
                "{width}: {rights:?}"
            );
            if width == 300.0 {
                assert_eq!(rights.len(), 2, "{rights:?}");
            } else {
                assert_eq!(rights.len(), 4, "{width}: {rights:?}");
            }
        }
    }

    #[test]
    fn dragging_an_edge_resizes_its_column() {
        let ctx = egui::Context::default();
        draw(&ctx, 1000.0, Vec::new());
        let before = draw(&ctx, 1000.0, Vec::new());
        // The edge between Size and Modified, 40 points to the right.
        let edge = before[1] + 4.0;
        let pointer = |x: f32, pressed: Option<bool>| {
            let pos = pos2(x, 100.0);
            let mut events = vec![egui::Event::PointerMoved(pos)];
            if let Some(pressed) = pressed {
                events.push(egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: Default::default(),
                });
            }
            events
        };
        draw(&ctx, 1000.0, pointer(edge, None));
        draw(&ctx, 1000.0, pointer(edge, Some(true)));
        for step in 1..=4 {
            draw(&ctx, 1000.0, pointer(edge + 10.0 * step as f32, None));
        }
        draw(&ctx, 1000.0, pointer(edge + 40.0, Some(false)));
        draw(&ctx, 1000.0, Vec::new());
        let after = draw(&ctx, 1000.0, Vec::new());
        let width = |rights: &[f32]| rights[1] - rights[0];
        assert!(
            (width(&after) - width(&before) - 40.0).abs() < 1.0,
            "{before:?} -> {after:?}"
        );
        assert!((after[3] - before[3]).abs() < 1.0, "table width changed");
    }
}
