//! Small reusable widgets for virtualized, fixed-height row lists.

use egui::{
    Align2, FontId, Response, Sense, TextStyle, Ui, scroll_area::ScrollAreaOutput, text::LayoutJob,
    vec2,
};

pub fn row_height(ui: &Ui) -> f32 {
    ui.text_style_height(&TextStyle::Body) + 6.0
}

pub struct ListRow<'a> {
    pub icon: &'a str,
    pub name: &'a str,
    pub selected: bool,
    pub weak: bool,
    pub indent: f32,
    /// Glyph drawn before the icon, e.g. an expand caret.
    pub leading: Option<&'a str>,
    /// Glyph drawn at the right edge, e.g. a "has children" caret.
    pub trailing: Option<&'a str>,
}

/// Width reserved for [`ListRow::leading`].
pub const LEADING_WIDTH: f32 = 16.0;

impl ListRow<'_> {
    /// Allocates a full-width clickable row. Expects `item_spacing.y == 0`.
    pub fn show(self, ui: &mut Ui) -> Response {
        let size = vec2(ui.available_width(), row_height(ui));
        let (rect, response) = ui.allocate_exact_size(size, Sense::click());
        if !ui.is_rect_visible(rect) {
            return response;
        }

        let visuals = ui.visuals();
        let painter = ui.painter();
        if self.selected {
            painter.rect_filled(rect, 2.0, visuals.selection.bg_fill);
        } else if response.hovered() {
            painter.rect_filled(rect, 2.0, visuals.widgets.hovered.weak_bg_fill);
        }
        let color = if self.selected {
            visuals.selection.stroke.color
        } else if self.weak {
            visuals.weak_text_color()
        } else {
            visuals.text_color()
        };
        let font = FontId::proportional(ui.text_style_height(&TextStyle::Body) - 1.0);

        let mut x = rect.left() + 4.0 + self.indent;
        if let Some(leading) = self.leading {
            painter.text(
                egui::pos2(x + LEADING_WIDTH / 2.0, rect.center().y),
                Align2::CENTER_CENTER,
                leading,
                font.clone(),
                visuals.weak_text_color(),
            );
            x += LEADING_WIDTH;
        }
        painter.text(
            egui::pos2(x, rect.center().y),
            Align2::LEFT_CENTER,
            self.icon,
            font.clone(),
            color,
        );
        x += font.size + 6.0;

        let trailing_width = if self.trailing.is_some() { 18.0 } else { 4.0 };
        let mut job = LayoutJob::simple_singleline(self.name.to_owned(), font.clone(), color);
        job.wrap.max_width = (rect.right() - trailing_width - x).max(0.0);
        job.wrap.max_rows = 1;
        job.wrap.break_anywhere = true;
        let galley = painter.layout_job(job);
        painter.galley(
            egui::pos2(x, rect.center().y - galley.size().y / 2.0),
            galley,
            color,
        );

        if let Some(trailing) = self.trailing {
            painter.text(
                rect.right_center() - vec2(6.0, 0.0),
                Align2::RIGHT_CENTER,
                trailing,
                font,
                visuals.weak_text_color(),
            );
        }
        response
    }
}

/// After showing a `show_rows` scroll area, adjusts its offset so `row` is
/// visible next frame. Use with `item_spacing.y == 0`.
pub fn ensure_row_visible<R>(ui: &Ui, output: &ScrollAreaOutput<R>, row: usize, row_height: f32) {
    let top = row as f32 * row_height;
    let bottom = top + row_height;
    let view_height = output.inner_rect.height();
    let mut state = output.state;
    let offset = state.offset.y;
    let new_offset = if top < offset {
        top
    } else if bottom > offset + view_height {
        bottom - view_height
    } else {
        return;
    };
    state.offset.y = new_offset;
    state.store(ui.ctx(), output.id);
    ui.ctx().request_repaint();
}

/// Like [`ensure_row_visible`], but only the first time this `(area, row)` pair
/// is seen, so the user can still scroll away afterwards.
pub fn ensure_row_visible_once<R>(
    ui: &Ui,
    output: &ScrollAreaOutput<R>,
    row: usize,
    row_height: f32,
) {
    let key = output.id.with(("ensured", row));
    let first = ui.data_mut(|d| {
        let first = d.get_temp::<()>(key).is_none();
        d.insert_temp(key, ());
        first
    });
    if first {
        ensure_row_visible(ui, output, row, row_height);
    }
}
