//! Touch gestures for the mobile layout, from a mouse as from a finger:
//! a drag scrolls, a tap opens, and holding still for a moment (a long
//! press) opens the context menu when let go, or with a drag after it,
//! selects what it passes over, or drags the selection if pressed on it.
//!
//! egui only tells long presses from real touches, and only for context
//! menus, so the press is followed here in the raw input, and a long press
//! let go without moving becomes a right click.

use std::{
    ops::RangeInclusive,
    time::{Duration, Instant},
};

use egui::{Event, Id, PointerButton, Pos2, RawInput, scroll_area::ScrollSource, style::Style};

use crate::haptics::Haptic;

/// How long to hold still for a long press.
pub const LONG_PRESS: Duration = Duration::from_millis(500);
/// How far the pointer may wander and still be held still; egui's own
/// click distance, so it hasn't started a drag either.
const SLOP: f32 = 6.0;

/// The least height of anything to tap.
pub const TOUCH_SIZE: f32 = 40.0;

/// Makes widgets, menus and windows big enough to tap.
pub fn touch_style(style: &mut Style) {
    let s = &mut style.spacing;
    s.interact_size.y = s.interact_size.y.max(TOUCH_SIZE * 0.8);
    s.button_padding = s.button_padding.max(egui::vec2(8.0, 6.0));
    s.item_spacing = s.item_spacing.max(egui::vec2(8.0, 6.0));
    let m = &mut s.menu_margin;
    for side in [&mut m.left, &mut m.right, &mut m.top, &mut m.bottom] {
        *side = (*side).max(8);
    }
    s.icon_width = s.icon_width.max(20.0);
    s.icon_width_inner = s.icon_width_inner.max(12.0);
    s.scroll.bar_width = s.scroll.bar_width.max(8.0);
}

struct Press {
    pos: Pos2,
    since: Instant,
    /// Moved before the long press: a drag (scrolling).
    moved: bool,
    /// Held long enough.
    long: bool,
    /// Moved after the long press: a drag selection.
    dragged: bool,
}

/// What views read of the press this frame.
#[derive(Clone, Copy, Default)]
struct State {
    /// The long press began this frame.
    started: bool,
    /// The button is down after a long press, or was let go this frame.
    holding: bool,
}

fn state_id() -> Id {
    Id::new("touch_long_press")
}

/// Follows the primary button for long presses.
#[derive(Default)]
pub struct LongPress {
    press: Option<Press>,
}

impl LongPress {
    /// Watches `raw` for presses; call from `raw_input_hook` every frame
    /// in the mobile layout.
    pub fn hook(&mut self, ctx: &egui::Context, raw: &mut RawInput) {
        let now = Instant::now();
        let mut state = State::default();
        let mut events = Vec::with_capacity(raw.events.len());
        for event in raw.events.drain(..) {
            let mut right_click = None;
            match &event {
                Event::PointerButton {
                    button: PointerButton::Primary,
                    pressed: true,
                    pos,
                    ..
                } => {
                    self.press = Some(Press {
                        pos: *pos,
                        since: now,
                        moved: false,
                        long: false,
                        dragged: false,
                    });
                }
                Event::PointerMoved(pos) => {
                    if let Some(press) = &mut self.press
                        && press.pos.distance(*pos) > SLOP
                    {
                        if press.long {
                            if !press.dragged {
                                // On a touchscreen egui opened the item's
                                // menu at the long press; dragging (to
                                // select, or to move the selection) isn't
                                // asking for it.
                                egui::Popup::close_all(ctx);
                            }
                            press.dragged = true;
                        } else {
                            press.moved = true;
                        }
                    }
                }
                Event::PointerButton {
                    button: PointerButton::Primary,
                    pressed: false,
                    modifiers,
                    ..
                } => {
                    if let Some(press) = self.press.take()
                        && press.long
                    {
                        // Through the frame it ends: egui gave the drag to
                        // the scroll area when it began, and would scroll it
                        // by the last move and fling it on release.
                        state.holding = true;
                        // egui opens context menus on real long touches itself.
                        if !press.dragged && !ctx.input(|i| i.has_touch_screen()) {
                            right_click = Some((press.pos, *modifiers));
                        }
                    }
                }
                Event::PointerGone => self.press = None,
                _ => {}
            }
            events.push(event);
            if let Some((pos, modifiers)) = right_click {
                for pressed in [true, false] {
                    events.push(Event::PointerButton {
                        pos,
                        button: PointerButton::Secondary,
                        pressed,
                        modifiers,
                    });
                }
            }
        }
        raw.events = events;

        if let Some(press) = &mut self.press {
            if !press.long && !press.moved && now - press.since >= LONG_PRESS {
                press.long = true;
                state.started = true;
                crate::haptics::play(Haptic::LongPress);
            }
            state.holding |= press.long;
        }
        ctx.data_mut(|d| d.insert_temp(state_id(), state));
    }

    /// Wakes the app when a press held still becomes long; call during the
    /// frame (repaints asked for in `raw_input_hook` are lost).
    pub fn wake(&self, ctx: &egui::Context) {
        if let Some(press) = &self.press
            && !press.long
            && !press.moved
        {
            ctx.request_repaint_after(LONG_PRESS.saturating_sub(press.since.elapsed()));
        }
    }
}

fn state(ctx: &egui::Context) -> State {
    ctx.data(|d| d.get_temp(state_id())).unwrap_or_default()
}

/// Whether a long press began this frame.
pub fn long_press_started(ctx: &egui::Context) -> bool {
    state(ctx).started
}

/// Whether the button is still down after a long press.
pub fn holding(ctx: &egui::Context) -> bool {
    state(ctx).holding
}

fn touch_id() -> Id {
    Id::new("touch_layout")
}

/// Says whether the layout is for touch.
pub fn set_touch(ctx: &egui::Context, touch: bool) {
    ctx.data_mut(|d| d.insert_temp(touch_id(), touch));
    // A long press isn't a click.
    let max_click = if touch {
        LONG_PRESS.as_secs_f64()
    } else {
        egui::Options::default().input_options.max_click_duration
    };
    ctx.options_mut(|o| o.input_options.max_click_duration = max_click);
}

pub fn is_touch(ctx: &egui::Context) -> bool {
    ctx.data(|d| d.get_temp(touch_id())).unwrap_or(false)
}

/// Whether dragging a scroll area's contents scrolls it: always in the
/// mobile layout, except while selecting after a long press.
pub fn drag_scroll(ctx: &egui::Context) -> egui::containers::scroll_area::DragScroll {
    use egui::containers::scroll_area::DragScroll;
    if !is_touch(ctx) {
        DragScroll::OnTouch
    } else if holding(ctx) {
        DragScroll::Never
    } else {
        DragScroll::Always
    }
}

/// The ways a scroll area scrolls; see [`drag_scroll`].
pub fn scroll_source(ctx: &egui::Context) -> ScrollSource {
    ScrollSource {
        drag: drag_scroll(ctx),
        ..ScrollSource::default()
    }
}

/// What list items sense: in the mobile layout only taps, so a drag
/// scrolls the list instead of dragging the item.
pub fn item_sense(ctx: &egui::Context) -> egui::Sense {
    if is_touch(ctx) {
        egui::Sense::click()
    } else {
        egui::Sense::click_and_drag()
    }
}

/// Scales `value` by a pinch (or Ctrl and the scroll wheel, on the
/// desktop) over `rect` in the mobile layout, keeping it within `range`.
/// Returns whether it changed.
pub fn pinch(ui: &egui::Ui, rect: egui::Rect, value: &mut f32, range: RangeInclusive<f32>) -> bool {
    if !is_touch(ui.ctx()) {
        return false;
    }
    let (zoom, at) = ui.input(|i| {
        let at = i
            .multi_touch()
            .map(|touch| touch.center_pos)
            .or(i.pointer.latest_pos());
        (i.zoom_delta(), at)
    });
    if zoom == 1.0 || !at.is_some_and(|at| rect.contains(at)) {
        return false;
    }
    let scaled = (*value * zoom).clamp(*range.start(), *range.end());
    let changed = scaled != *value;
    if changed && (scaled == *range.start() || scaled == *range.end()) {
        crate::haptics::play(Haptic::Limit);
    }
    *value = scaled;
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn button(pos: Pos2, button: PointerButton, pressed: bool) -> Event {
        Event::PointerButton {
            pos,
            button,
            pressed,
            modifiers: egui::Modifiers::NONE,
        }
    }

    fn run(press: &mut LongPress, ctx: &egui::Context, events: Vec<Event>) -> Vec<Event> {
        let mut raw = RawInput {
            events,
            ..RawInput::default()
        };
        press.hook(ctx, &mut raw);
        raw.events
    }

    fn right_clicks(events: &[Event]) -> usize {
        events
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    Event::PointerButton {
                        button: PointerButton::Secondary,
                        pressed: false,
                        ..
                    }
                )
            })
            .count()
    }

    /// Starts a press at `at` and holds it until it's long.
    fn hold(press: &mut LongPress, ctx: &egui::Context, at: Pos2) {
        run(press, ctx, vec![button(at, PointerButton::Primary, true)]);
        assert!(!long_press_started(ctx));
        press.press.as_mut().unwrap().since -= LONG_PRESS;
        run(press, ctx, Vec::new());
        assert!(long_press_started(ctx));
        assert!(holding(ctx));
        run(press, ctx, Vec::new());
        assert!(!long_press_started(ctx), "only the frame it began");
    }

    #[test]
    fn a_long_press_let_go_is_a_right_click() {
        let ctx = egui::Context::default();
        let mut press = LongPress::default();
        let at = egui::pos2(10.0, 10.0);
        hold(&mut press, &ctx, at);
        let events = run(
            &mut press,
            &ctx,
            vec![button(at, PointerButton::Primary, false)],
        );
        assert_eq!(right_clicks(&events), 1);
        assert!(holding(&ctx), "through the frame it ends");
        run(&mut press, &ctx, Vec::new());
        assert!(!holding(&ctx));
    }

    #[test]
    fn a_long_press_then_a_drag_is_not() {
        let ctx = egui::Context::default();
        let mut press = LongPress::default();
        let at = egui::pos2(10.0, 10.0);
        hold(&mut press, &ctx, at);
        let to = egui::pos2(10.0, 80.0);
        let events = run(
            &mut press,
            &ctx,
            vec![
                Event::PointerMoved(to),
                button(to, PointerButton::Primary, false),
            ],
        );
        assert_eq!(right_clicks(&events), 0);
    }

    #[test]
    fn moving_first_is_a_drag_not_a_long_press() {
        let ctx = egui::Context::default();
        let mut press = LongPress::default();
        let at = egui::pos2(10.0, 10.0);
        run(
            &mut press,
            &ctx,
            vec![
                button(at, PointerButton::Primary, true),
                Event::PointerMoved(egui::pos2(10.0, 40.0)),
            ],
        );
        press.press.as_mut().unwrap().since -= LONG_PRESS;
        run(&mut press, &ctx, Vec::new());
        assert!(!long_press_started(&ctx));
        assert!(!holding(&ctx));
        let events = run(
            &mut press,
            &ctx,
            vec![button(at, PointerButton::Primary, false)],
        );
        assert_eq!(right_clicks(&events), 0);
    }

    #[test]
    fn a_short_tap_is_not_a_long_press() {
        let ctx = egui::Context::default();
        let mut press = LongPress::default();
        let at = egui::pos2(10.0, 10.0);
        let events = run(
            &mut press,
            &ctx,
            vec![
                button(at, PointerButton::Primary, true),
                button(at, PointerButton::Primary, false),
            ],
        );
        assert_eq!(right_clicks(&events), 0);
        assert!(!long_press_started(&ctx));
    }
}
