//! Touch gestures for the mobile layout, from a mouse as from a finger:
//! a drag scrolls, a tap opens, and holding still for a moment (a long
//! press) opens the context menu when let go, or with a drag after it,
//! selects what it passes over, or drags the selection if pressed on it.
//!
//! egui only tells long presses from real touches, and only for context
//! menus, so the press is followed here in the raw input, and a long press
//! let go without moving becomes a right click.

use std::{ops::RangeInclusive, time::Duration};

use web_time::Instant;

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
    /// A swipe in from the screen's edge ended this frame: the system's back.
    back_swiped: bool,
}

fn state_id() -> Id {
    Id::new("touch_long_press")
}

/// How near the screen's sides a press may be the system's back gesture.
const EDGE: f32 = 24.0;
/// A finger still for this long at the edge isn't going to swipe.
const EDGE_STILL: Duration = Duration::from_millis(150);

/// A press at the screen's edge, not yet shown to egui.
struct EdgePress {
    pos: Pos2,
    modifiers: egui::Modifiers,
    since: Instant,
}

/// Follows the primary button for long presses.
#[derive(Default)]
pub struct LongPress {
    press: Option<Press>,
    /// Waiting to see which way it goes.
    edge: Option<EdgePress>,
    /// It went sideways: the rest of it is the system's.
    edge_swipe: bool,
    /// And it's over.
    edge_swipe_ended: bool,
}

impl LongPress {
    /// Android's back gesture starts as a touch like any other, that it then
    /// cancels, and that would be a tap at the edge of the page, or a grab of
    /// the scroll bar that's there. So a press at the edge isn't shown to egui
    /// until it's clear what it is: if it goes sideways it never is, and if it
    /// goes along the edge, is let go, or stays still, it's shown as it was.
    fn hold_back_edge_presses(&mut self, raw: &mut RawInput) {
        let Some(screen) = raw.screen_rect else {
            return;
        };
        let press_event = |edge: &EdgePress, pressed| Event::PointerButton {
            pos: edge.pos,
            button: PointerButton::Primary,
            pressed,
            modifiers: edge.modifiers,
        };
        let mut events = Vec::with_capacity(raw.events.len());
        for event in raw.events.drain(..) {
            match &event {
                Event::PointerButton {
                    button: PointerButton::Primary,
                    pressed: true,
                    pos,
                    modifiers,
                } if pos.x < screen.min.x + EDGE || pos.x > screen.max.x - EDGE => {
                    self.edge = Some(EdgePress {
                        pos: *pos,
                        modifiers: *modifiers,
                        since: Instant::now(),
                    });
                    self.edge_swipe = false;
                    // The move that says where it is goes with it, later.
                    events
                        .retain(|earlier| !matches!(earlier, Event::PointerMoved(at) if at == pos));
                }
                Event::PointerMoved(_) if self.edge_swipe => {}
                Event::PointerMoved(pos) => match &self.edge {
                    Some(edge) if edge.pos.distance(*pos) <= SLOP => {}
                    Some(edge) => {
                        let moved = *pos - edge.pos;
                        if moved.x.abs() > moved.y.abs() {
                            self.edge = None;
                            self.edge_swipe = true;
                        } else {
                            events.push(Event::PointerMoved(edge.pos));
                            events.push(press_event(edge, true));
                            events.push(event);
                            self.edge = None;
                        }
                    }
                    None => events.push(event),
                },
                Event::PointerButton {
                    button: PointerButton::Primary,
                    pressed: false,
                    ..
                } if self.edge_swipe => {
                    self.edge_swipe = false;
                    self.edge_swipe_ended = true;
                }
                Event::PointerButton {
                    button: PointerButton::Primary,
                    pressed: false,
                    ..
                } => {
                    // A tap: just as late as it had to be.
                    if let Some(edge) = self.edge.take() {
                        events.push(Event::PointerMoved(edge.pos));
                        events.push(press_event(&edge, true));
                    }
                    events.push(event);
                }
                Event::PointerGone => {
                    self.edge = None;
                    self.edge_swipe_ended |= std::mem::take(&mut self.edge_swipe);
                    events.push(event);
                }
                _ => events.push(event),
            }
        }
        if let Some(edge) = &self.edge
            && edge.since.elapsed() >= EDGE_STILL
        {
            events.push(Event::PointerMoved(edge.pos));
            events.push(press_event(edge, true));
            self.edge = None;
        }
        raw.events = events;
    }
}

impl LongPress {
    /// Watches `raw` for presses; call from `raw_input_hook` every frame
    /// in the mobile layout.
    pub fn hook(&mut self, ctx: &egui::Context, raw: &mut RawInput) {
        self.hold_back_edge_presses(raw);
        let now = Instant::now();
        let mut state = State {
            back_swiped: std::mem::take(&mut self.edge_swipe_ended),
            ..State::default()
        };
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
        if let Some(edge) = &self.edge {
            ctx.request_repaint_after(EDGE_STILL.saturating_sub(edge.since.elapsed()));
        }
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

/// How long after a swipe hid the keyboard its back key is still that swipe's.
const BACK_KEY_GRACE: f64 = 1.0;

fn keyboard_dismissed_id() -> Id {
    Id::new("touch_keyboard_dismissed")
}

/// Hides the keyboard for a back swipe, which Android's own way of doing it
/// doesn't always: it starts an animation of the keyboard that needs the view
/// system to draw, which a native activity doesn't use, and it can take a second
/// swipe. Text fields let go of the keys, which takes the keyboard down, and a
/// tap on one brings it back. Call once a frame.
pub fn hide_keyboard_for_back_swipe(ctx: &egui::Context) {
    if !state(ctx).back_swiped || keyboard_up(ctx) != Some(true) {
        return;
    }
    ctx.memory_mut(|memory| {
        if let Some(focused) = memory.focused() {
            memory.surrender_focus(focused);
        }
    });
    let now = ctx.input(|i| i.time);
    ctx.data_mut(|d| d.insert_temp(keyboard_dismissed_id(), now));
}

/// Whether a back key now is from the swipe that just hid the keyboard, when
/// Android goes on to send one as well: it shouldn't also leave the document.
pub fn back_key_was_for_keyboard(ctx: &egui::Context) -> bool {
    let at: Option<f64> = ctx.data(|d| d.get_temp(keyboard_dismissed_id()));
    let now = ctx.input(|i| i.time);
    at.is_some_and(|at| now - at < BACK_KEY_GRACE)
}

fn keyboard_id() -> Id {
    Id::new("touch_keyboard_up")
}

/// Says whether the on-screen keyboard is up, where that can be told.
#[cfg(any(target_os = "android", test))]
pub fn set_keyboard_up(ctx: &egui::Context, up: bool) {
    ctx.data_mut(|d| d.insert_temp(keyboard_id(), up));
}

/// Whether the on-screen keyboard is up, where that can be told.
pub fn keyboard_up(ctx: &egui::Context) -> Option<bool> {
    ctx.data(|d| d.get_temp(keyboard_id()))
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

/// Scales `value` by a pinch or Ctrl and the scroll wheel over `rect`,
/// keeping it within `range`; a `strength` under 1 takes more of the gesture to
/// change it as much. A window or menu over `rect` keeps the gesture for
/// itself. Returns whether it changed.
pub fn zoom_gesture(
    ui: &egui::Ui,
    rect: egui::Rect,
    value: &mut f32,
    range: RangeInclusive<f32>,
    strength: f32,
) -> bool {
    let (zoom, at) = ui.input(|i| {
        let at = i
            .multi_touch()
            .map(|touch| touch.center_pos)
            .or(i.pointer.latest_pos());
        (i.zoom_delta(), at)
    });
    let over =
        at.is_some_and(|at| rect.contains(at) && ui.ctx().layer_id_at(at) == Some(ui.layer_id()));
    if zoom == 1.0 || !over {
        return false;
    }
    let scaled = (*value * zoom.powf(strength)).clamp(*range.start(), *range.end());
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

#[cfg(test)]
mod edge_tests {
    use egui::{Rect, pos2, vec2};

    use super::*;

    fn input(events: Vec<Event>) -> RawInput {
        RawInput {
            screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(400.0, 800.0))),
            events,
            ..Default::default()
        }
    }

    fn button(pos: Pos2, pressed: bool) -> Event {
        Event::PointerButton {
            pos,
            button: PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        }
    }

    /// Runs frames of events, and returns what egui would get of them.
    fn through(frames: Vec<Vec<Event>>) -> Vec<Event> {
        let ctx = egui::Context::default();
        let mut press = LongPress::default();
        let mut out = Vec::new();
        for events in frames {
            let mut raw = input(events);
            press.hook(&ctx, &mut raw);
            out.extend(raw.events);
        }
        out
    }

    fn presses(events: &[Event]) -> usize {
        events
            .iter()
            .filter(|event| matches!(event, Event::PointerButton { pressed: true, .. }))
            .count()
    }

    #[test]
    fn a_swipe_in_from_the_edge_is_the_systems() {
        let start = pos2(395.0, 300.0);
        let out = through(vec![
            vec![Event::PointerMoved(start), button(start, true)],
            vec![Event::PointerMoved(pos2(370.0, 302.0))],
            vec![Event::PointerMoved(pos2(340.0, 303.0))],
            // Android cancels it, which comes as letting go.
            vec![button(pos2(340.0, 303.0), false), Event::PointerGone],
        ]);
        assert_eq!(presses(&out), 0, "{out:?}");
        assert!(
            !out.iter()
                .any(|event| matches!(event, Event::PointerButton { .. }))
        );
    }

    #[test]
    fn a_drag_along_the_edge_reaches_the_scroll_bar() {
        let start = pos2(395.0, 300.0);
        let out = through(vec![
            vec![Event::PointerMoved(start), button(start, true)],
            vec![Event::PointerMoved(pos2(395.0, 340.0))],
            vec![Event::PointerMoved(pos2(395.0, 380.0))],
            vec![button(pos2(395.0, 380.0), false)],
        ]);
        assert_eq!(presses(&out), 1, "{out:?}");
        // From where it started, in order.
        let first = out
            .iter()
            .position(|e| matches!(e, Event::PointerButton { .. }))
            .unwrap();
        assert!(matches!(out[first], Event::PointerButton { pos, .. } if pos == start));
        assert!(
            out[first + 1..]
                .iter()
                .any(|e| matches!(e, Event::PointerMoved(p) if p.y == 380.0))
        );
    }

    fn swipe_in(ctx: &egui::Context, press: &mut LongPress) {
        let start = pos2(395.0, 300.0);
        for events in [
            vec![Event::PointerMoved(start), button(start, true)],
            vec![Event::PointerMoved(pos2(360.0, 301.0))],
            vec![button(pos2(360.0, 301.0), false), Event::PointerGone],
            vec![],
        ] {
            let mut raw = input(events);
            press.hook(ctx, &mut raw);
        }
    }

    #[test]
    fn a_back_swipe_is_known_for_the_frame_it_ends() {
        let ctx = egui::Context::default();
        let mut press = LongPress::default();
        let start = pos2(395.0, 300.0);
        let mut hook = |events| {
            let mut raw = input(events);
            press.hook(&ctx, &mut raw);
            state(&ctx).back_swiped
        };
        assert!(!hook(vec![Event::PointerMoved(start), button(start, true)]));
        assert!(!hook(vec![Event::PointerMoved(pos2(360.0, 301.0))]));
        assert!(hook(vec![
            button(pos2(360.0, 301.0), false),
            Event::PointerGone
        ]));
        assert!(!hook(vec![]), "once");
    }

    #[test]
    fn a_swipe_with_the_keyboard_down_is_just_back() {
        let ctx = egui::Context::default();
        let mut press = LongPress::default();
        set_keyboard_up(&ctx, false);
        swipe_in(&ctx, &mut press);
        hide_keyboard_for_back_swipe(&ctx);
        assert!(!back_key_was_for_keyboard(&ctx));
    }

    #[test]
    fn a_tap_at_the_edge_is_a_tap() {
        let start = pos2(5.0, 300.0);
        let out = through(vec![
            vec![Event::PointerMoved(start), button(start, true)],
            vec![button(start, false), Event::PointerGone],
        ]);
        let kinds: Vec<bool> = out
            .iter()
            .filter_map(|event| match event {
                Event::PointerButton { pressed, .. } => Some(*pressed),
                _ => None,
            })
            .collect();
        assert_eq!(kinds, [true, false], "{out:?}");
    }

    #[test]
    fn a_finger_held_at_the_edge_is_let_through_in_time() {
        let start = pos2(395.0, 300.0);
        let ctx = egui::Context::default();
        let mut press = LongPress::default();
        let mut raw = input(vec![Event::PointerMoved(start), button(start, true)]);
        press.hook(&ctx, &mut raw);
        assert_eq!(presses(&raw.events), 0, "not yet");
        std::thread::sleep(EDGE_STILL + Duration::from_millis(10));
        let mut raw = input(vec![]);
        press.hook(&ctx, &mut raw);
        assert_eq!(presses(&raw.events), 1);
    }

    #[test]
    fn what_starts_away_from_the_edge_is_as_it_was() {
        let start = pos2(200.0, 300.0);
        let out = through(vec![
            vec![Event::PointerMoved(start), button(start, true)],
            vec![Event::PointerMoved(pos2(150.0, 300.0))],
            vec![button(pos2(150.0, 300.0), false)],
        ]);
        assert_eq!(out.len(), 4, "{out:?}");
    }
}
