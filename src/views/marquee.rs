//! Drag-to-select: dragging from empty space draws a rectangle and selects
//! the items it touches. Ctrl toggles the touched items in the existing
//! selection and Shift adds them to it. Dragging past the top or bottom of
//! the list scrolls it.
//!
//! The rectangle's start is kept relative to the content, not the screen, so
//! it stays put while the list scrolls under it.
//!
//! In the mobile layout a drag scrolls instead, and a long press starts the
//! selection: it selects the item pressed, and with a drag after it, the
//! items passed over, adding to what's selected. No rectangle shows there,
//! and in a grid, the items selected are those between the two in reading
//! order, as text is selected, rather than those in a rectangle.
//! A long press on an item already selected drags the selection instead
//! (see `item_interaction`).

use std::{collections::HashSet, path::PathBuf, sync::Arc};

use egui::{
    DragAndDrop, Id, PointerButton, Pos2, Rect, Stroke, StrokeKind, Ui, Vec2,
    scroll_area::ScrollAreaOutput,
};

use crate::tab::{Action, BrowserTab};

#[derive(Clone)]
struct State {
    /// Where the drag began, relative to the content's top-left.
    origin: Vec2,
    /// The selection the touched items are added to (Shift) or toggled in (Ctrl).
    base: Arc<HashSet<PathBuf>>,
    toggle: bool,
    /// The items touched when the selection was last updated.
    hits: Option<Vec<usize>>,
}

/// Where a view's items are this frame.
pub(crate) struct Layout<'a> {
    /// The screen position of the content's top-left, scrolled.
    pub content_origin: Pos2,
    /// Which items (indices into the visible list) a rectangle touches,
    /// in content coordinates.
    pub hits: &'a dyn Fn(Rect) -> Vec<usize>,
    /// For a grid: the item at, or nearest in reading order to, a point in
    /// content coordinates. Touch selects the run of items between two.
    pub item_at: Option<&'a dyn Fn(Pos2) -> Option<usize>>,
}

/// Starts, continues or ends a drag selection in a view's scroll area.
/// `start` is whether a drag that may begin one started this frame; it only
/// does when it started inside the scroll area. `layout` is `None` when the
/// view has no items to select.
pub(crate) fn update<R>(
    ui: &Ui,
    tab: &BrowserTab,
    visible: &[usize],
    start: bool,
    scroll: &ScrollAreaOutput<R>,
    layout: Option<Layout<'_>>,
    actions: &mut Vec<Action>,
) {
    let ctx = ui.ctx();
    let id = Id::new(("marquee", tab.id));
    let Some(layout) = layout else {
        ctx.data_mut(|d| d.remove::<State>(id));
        return;
    };
    let (press, pointer, down, modifiers) = ctx.input(|i| {
        (
            i.pointer.press_origin(),
            i.pointer.latest_pos(),
            i.pointer.button_down(PointerButton::Primary),
            i.modifiers,
        )
    });
    let viewport = scroll.inner_rect;

    let touch = crate::touch::is_touch(ctx);
    let start = if touch {
        // Unless the long press picked the selection up to drag it.
        crate::touch::long_press_started(ctx) && !DragAndDrop::has_any_payload(ctx)
    } else {
        start
    };
    let mut state = ctx.data(|d| d.get_temp::<State>(id));
    if start
        && let Some(press) = press
        && viewport.contains(press)
    {
        if touch {
            actions.push(Action::StartSelecting);
        }
        // A long press adds to a touch selection under way, but otherwise
        // starts afresh: a file only tapped to preview it stays out.
        let keep = modifiers.command || modifiers.shift || touch && tab.selecting;
        state = Some(State {
            origin: press - layout.content_origin,
            base: Arc::new(if keep {
                tab.selection.clone()
            } else {
                HashSet::new()
            }),
            toggle: modifiers.command,
            hits: None,
        });
    }
    let Some(mut state) = state else {
        return;
    };
    let Some(pointer) = pointer.filter(|_| down) else {
        ctx.data_mut(|d| d.remove::<State>(id));
        return;
    };

    let rect = Rect::from_two_pos(layout.content_origin + state.origin, pointer);
    let hits = match layout.item_at {
        Some(item_at) if touch => {
            let content = |pos: Pos2| pos - layout.content_origin.to_vec2();
            match (item_at(state.origin.to_pos2()), item_at(content(pointer))) {
                (Some(a), Some(b)) => (a.min(b)..=a.max(b)).collect(),
                _ => Vec::new(),
            }
        }
        _ => (layout.hits)(rect.translate(-layout.content_origin.to_vec2())),
    };
    if state.hits.as_ref() != Some(&hits) {
        // A tick as the drag takes in, or gives back, an item; the long
        // press that began it has buzzed already.
        if touch
            && state
                .hits
                .as_ref()
                .is_some_and(|old| old.len() != hits.len())
        {
            crate::haptics::play(crate::haptics::Haptic::Tick);
        }
        let mut selection = (*state.base).clone();
        for &index in &hits {
            let Some(&entry) = visible.get(index) else {
                continue;
            };
            let path = tab.entries()[entry].path.clone();
            if !(state.toggle && selection.remove(&path)) {
                selection.insert(path);
            }
        }
        actions.push(Action::SetSelection(selection));
        state.hits = Some(hits);
    }

    if !touch {
        let color = crate::theme::colors(ctx).files.selection;
        let painter = ui.painter_at(viewport);
        painter.rect_filled(rect, 2.0, color.gamma_multiply(0.25));
        painter.rect_stroke(rect, 2.0, Stroke::new(1.0, color), StrokeKind::Inside);
    }

    // Scroll faster the further past the edge the pointer is.
    let past = if pointer.y < viewport.top() {
        pointer.y - viewport.top()
    } else if pointer.y > viewport.bottom() {
        pointer.y - viewport.bottom()
    } else {
        0.0
    };
    if past != 0.0 {
        let dt = ctx.input(|i| i.stable_dt).min(0.1);
        let speed = past.signum() * (past.abs() * 12.0).min(3000.0);
        let max = (scroll.content_size.y - viewport.height()).max(0.0);
        let mut scroll_state = scroll.state;
        scroll_state.offset.y = (scroll_state.offset.y + speed * dt).clamp(0.0, max);
        scroll_state.store(ctx, scroll.id);
    }
    ctx.request_repaint();
    ctx.data_mut(|d| d.insert_temp(id, state));
}

/// Hits for a list of rows `stride` apart, each `height` tall.
pub(crate) fn rows_hit(rect: Rect, stride: f32, height: f32, count: usize) -> Vec<usize> {
    if count == 0 || rect.bottom() < 0.0 || stride <= 0.0 {
        return Vec::new();
    }
    let top = rect.top().max(0.0);
    let mut first = (top / stride).floor() as usize;
    if top - first as f32 * stride > height {
        // In the gap below a row.
        first += 1;
    }
    let last = ((rect.bottom() / stride).floor() as usize).min(count - 1);
    (first..=last).collect()
}

/// The item at `pos` in a grid of `items` (in reading order, at content
/// rectangles): in the row `pos` is level with, or the nearest row, the item
/// it's over or nearest to across. Past the end of a short last row, that's
/// its last item.
pub(crate) fn grid_item_at(pos: Pos2, items: &[(usize, Rect)]) -> Option<usize> {
    let row_distance = |rect: &Rect| {
        if pos.y < rect.top() {
            rect.top() - pos.y
        } else {
            (pos.y - rect.bottom()).max(0.0)
        }
    };
    let row = items
        .iter()
        .map(|(_, rect)| row_distance(rect))
        .min_by(f32::total_cmp)?;
    items
        .iter()
        .filter(|(_, rect)| row_distance(rect) == row)
        .min_by(|(_, a), (_, b)| {
            let across = |rect: &Rect| (pos.x - pos.x.clamp(rect.left(), rect.right())).abs();
            across(a).total_cmp(&across(b))
        })
        .map(|&(index, _)| index)
}

/// Hits for items laid out at the given content rectangles.
pub(crate) fn rects_hit(rect: Rect, items: &[(usize, Rect)]) -> Vec<usize> {
    items
        .iter()
        .filter(|(_, item)| item.intersects(rect))
        .map(|&(index, _)| index)
        .collect()
}

#[cfg(test)]
mod tests {
    use egui::{Rect, pos2};

    use super::{grid_item_at, rows_hit};

    fn span(top: f32, bottom: f32) -> Rect {
        Rect::from_min_max(pos2(0.0, top), pos2(10.0, bottom))
    }

    #[test]
    fn grid_items() {
        // Three to a row, 10 wide and tall with 2 between; a last row of two.
        let items: Vec<_> = (0..5)
            .map(|i| {
                let (x, y) = ((i % 3) as f32 * 12.0, (i / 3) as f32 * 12.0);
                (i, Rect::from_min_size(pos2(x, y), egui::vec2(10.0, 10.0)))
            })
            .collect();
        assert_eq!(grid_item_at(pos2(5.0, 5.0), &items), Some(0));
        assert_eq!(grid_item_at(pos2(17.0, 5.0), &items), Some(1));
        // In the gap between two: either neighbour.
        assert!(matches!(grid_item_at(pos2(11.0, 5.0), &items), Some(0 | 1)));
        // Past a row's end, and past the short last row's end.
        assert_eq!(grid_item_at(pos2(80.0, 5.0), &items), Some(2));
        assert_eq!(grid_item_at(pos2(80.0, 17.0), &items), Some(4));
        // Above and below the content.
        assert_eq!(grid_item_at(pos2(30.0, -40.0), &items), Some(2));
        assert_eq!(grid_item_at(pos2(1.0, 200.0), &items), Some(3));
        assert_eq!(grid_item_at(pos2(1.0, 1.0), &[]), None);
    }

    #[test]
    fn rows() {
        // Rows 20 tall, 24 apart, 5 of them.
        assert_eq!(rows_hit(span(0.0, 1.0), 24.0, 20.0, 5), [0]);
        assert_eq!(rows_hit(span(10.0, 30.0), 24.0, 20.0, 5), [0, 1]);
        // Starting in the gap below row 0.
        assert_eq!(rows_hit(span(21.0, 30.0), 24.0, 20.0, 5), [1]);
        // Above the content, and past the end.
        assert_eq!(rows_hit(span(-50.0, 500.0), 24.0, 20.0, 5), [0, 1, 2, 3, 4]);
        assert!(rows_hit(span(-50.0, -1.0), 24.0, 20.0, 5).is_empty());
        assert!(rows_hit(span(200.0, 300.0), 24.0, 20.0, 5).is_empty());
    }
}
