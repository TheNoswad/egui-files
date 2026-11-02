//! An optional title bar drawn by the app, in place of the system's: the
//! menus at its left, the window buttons at its right, and the space between
//! to drag the window by. Without the system frame the window also needs
//! edges to resize it by, which [`resize_edges`] adds.

use std::path::PathBuf;

use egui::{
    Align2, Button, Color32, CursorIcon, FontId, Id, Order, Panel, Rect, ResizeDirection, RichText,
    Sense, TextEdit, Ui, Vec2, ViewportCommand, pos2, vec2,
};
use egui_phosphor::regular as icon;
use serde::{Deserialize, Serialize};

use egui_dock::DockState;

use crate::{
    dock_tab::{DockTab, Pane},
    keymap::{self, Command},
    tab,
};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    /// Draw the title bar in the app; off keeps the system's.
    pub enabled: bool,
    /// The focused tab's title in the middle.
    pub title: bool,
    /// Back, forward and up for the focused tab.
    pub navigation: bool,
    /// The focused tab's folder, to type a path into.
    pub path_box: bool,
    /// The focused tab's filter.
    pub filter_box: bool,
    /// Buttons opening and closing the panes.
    pub pane_toggles: bool,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            enabled: false,
            title: true,
            navigation: true,
            path_box: false,
            filter_box: false,
            pane_toggles: false,
        }
    }
}

/// What the bar shows of the focused tab.
#[derive(Default)]
pub struct Focus {
    pub title: String,
    /// Shown on hovering the title.
    pub hover: Option<String>,
    /// The folder of the focused browser tab; without one, navigation and
    /// the boxes are disabled.
    pub path: Option<PathBuf>,
    pub can_go_back: bool,
    pub can_go_forward: bool,
    pub filter: String,
}

/// What the bar asks of the app this frame.
// A few a frame at most, so the size difference doesn't matter.
#[allow(clippy::large_enum_variant)]
pub enum Request {
    /// For the focused browser tab.
    Tab(tab::Action),
    /// Set the focused browser tab's filter.
    Filter(String),
    TogglePane(Pane),
}

/// Whether the bar was on when the app last saved its state. Read before
/// the window opens, so it opens without the system frame rather than
/// losing it a frame later.
pub fn saved_enabled(app_id: &str) -> bool {
    eframe::storage_dir(app_id)
        .and_then(|dir| std::fs::read_to_string(dir.join("app.ron")).ok())
        .is_some_and(|text| enabled_in(&text))
}

/// Whether the bar is on in eframe's saved state, `app_ron`: a map of keys
/// to the RON of each value, the app's under [`eframe::APP_KEY`].
fn enabled_in(app_ron: &str) -> bool {
    #[derive(Deserialize)]
    struct Saved {
        #[serde(default)]
        titlebar: Prefs,
    }
    ron::from_str::<std::collections::HashMap<String, String>>(app_ron)
        .ok()
        .and_then(|kv| ron::from_str::<Saved>(kv.get(eframe::APP_KEY)?).ok())
        .is_some_and(|saved| saved.titlebar.enabled)
}

/// How tall the bar is, in points.
const HEIGHT: f32 = 32.0;
/// How wide each window button is.
const BUTTON_WIDTH: f32 = 46.0;
/// How far in from the window's edges a drag resizes it.
const EDGE: f32 = 5.0;
/// How far along an edge from a corner a drag resizes from that corner.
const CORNER: f32 = 14.0;

/// What the bar shows of `dock`'s focused tab, and the id of the browser
/// tab its requests go to: the focused tab if it's one, or with a pane
/// focused, `active_browser` (the tab the panes follow).
pub fn focus(dock: &mut DockState<DockTab>, active_browser: Option<u64>) -> (Focus, Option<u64>) {
    let focused = dock.find_active_focused().map(|(_, t)| t.id());
    let shown = match focused {
        Some(id)
            if dock
                .iter_all_tabs()
                .any(|(_, t)| t.id() == id && t.pane().is_none()) =>
        {
            Some(id)
        }
        _ => active_browser.or_else(|| {
            dock.iter_all_tabs()
                .find(|(_, t)| t.browser().is_some())
                .map(|(_, t)| t.id())
        }),
    };
    let Some(tab) = shown.and_then(|id| {
        dock.iter_all_tabs()
            .find(|(_, t)| t.id() == id)
            .map(|(_, t)| t)
    }) else {
        return (Focus::default(), None);
    };
    let mut focus = Focus {
        title: crate::tab_windows::plain_title(&tab.title()),
        ..Focus::default()
    };
    let Some(browser) = tab.browser() else {
        return (focus, None);
    };
    focus.hover = Some(browser.path.display().to_string());
    focus.path = Some(browser.path.clone());
    focus.can_go_back = browser.can_go_back();
    focus.can_go_forward = browser.can_go_forward();
    focus.filter = browser.filter.clone();
    (focus, Some(browser.id))
}

/// Hands `requests` to the browser tab `browser` in `dock`; returns the
/// panes to open or close, which only the app can.
pub fn apply(
    dock: &mut DockState<DockTab>,
    browser: Option<u64>,
    requests: Vec<Request>,
) -> Vec<Pane> {
    let mut panes = Vec::new();
    let mut tab = browser.and_then(|id| {
        dock.iter_all_tabs_mut()
            .find(|(_, t)| t.id() == id)
            .and_then(|(_, t)| t.browser_mut())
    });
    for request in requests {
        match (request, &mut tab) {
            (Request::Tab(action), Some(tab)) => tab.queued.push(action),
            (Request::Filter(filter), Some(tab)) => tab.filter = filter,
            (Request::TogglePane(pane), _) => panes.push(pane),
            (Request::Tab(_) | Request::Filter(_), None) => {}
        }
    }
    panes
}

/// Least room left over to drag the window by, whatever else is on.
const MIN_DRAG: f32 = 80.0;
const GAP: f32 = 8.0;
const NAV_BUTTON: f32 = 28.0;
const PATH_WIDTH: (f32, f32) = (140.0, 360.0);
const FILTER_WIDTH: (f32, f32) = (100.0, 180.0);

/// Which optional parts left of the title fit in `room`, and how wide the
/// boxes are, keeping [`MIN_DRAG`] free. Navigation goes first, then the
/// path, then the filter; each shows at its least width before any grows.
#[derive(Debug, PartialEq)]
struct Fit {
    navigation: bool,
    path: Option<f32>,
    filter: Option<f32>,
}

fn fit(prefs: &Prefs, room: f32) -> Fit {
    let mut room = room - MIN_DRAG;
    let mut take = |on: bool, width: f32| {
        let fits = on && room >= width + GAP;
        if fits {
            room -= width + GAP;
        }
        fits
    };
    let navigation = take(prefs.navigation, 3.0 * NAV_BUTTON);
    let path = take(prefs.path_box, PATH_WIDTH.0);
    let filter = take(prefs.filter_box, FILTER_WIDTH.0);
    let mut grow = |on: bool, (least, most): (f32, f32)| {
        on.then(|| {
            let extra = room.min(most - least).max(0.0);
            room -= extra;
            least + extra
        })
    };
    let path = grow(path, PATH_WIDTH);
    let filter = grow(filter, FILTER_WIDTH);
    Fit {
        navigation,
        path,
        filter,
    }
}

/// Draws the bar along the top of the viewport `ui` is in: `menus` at its
/// left, then the parts `prefs` turns on for `focus`, and `panes` (each
/// with whether it's open) as toggles beside the window buttons.
pub fn show(
    ui: &mut Ui,
    id: &str,
    prefs: &Prefs,
    focus: &Focus,
    panes: &[(Pane, bool)],
    menus: impl FnOnce(&mut Ui),
) -> Vec<Request> {
    let frame = egui::Frame::side_top_panel(ui.style()).inner_margin(egui::Margin::ZERO);
    Panel::top(Id::new(("titlebar", id)))
        .exact_size(HEIGHT)
        .resizable(false)
        .frame(frame)
        .show(ui, |ui| bar(ui, prefs, focus, panes, menus))
        .inner
}

fn bar(
    ui: &mut Ui,
    prefs: &Prefs,
    focus: &Focus,
    panes: &[(Pane, bool)],
    menus: impl FnOnce(&mut Ui),
) -> Vec<Request> {
    let rect = ui.max_rect();
    let ctx = ui.ctx().clone();
    let maximized = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
    let mut requests = Vec::new();

    // Under everything else in the bar: what's left over moves the window.
    let background = ui.interact(rect, ui.id().with("drag"), Sense::click_and_drag());
    if background.double_clicked() {
        ctx.send_viewport_cmd(ViewportCommand::Maximized(!maximized));
    } else if background.drag_started_by(egui::PointerButton::Primary) {
        ctx.send_viewport_cmd(ViewportCommand::StartDrag);
    }

    let toggles = if prefs.pane_toggles { panes } else { &[] };
    let right_width = 3.0 * BUTTON_WIDTH + toggles.len() as f32 * NAV_BUTTON + GAP;
    let mut left_end = rect.left();
    let mut buttons_left = rect.right();
    ui.horizontal_centered(|ui| {
        ui.add_space(6.0);
        ui.scope(menus);
        let room = rect.right() - right_width - ui.cursor().left();
        let fit = fit(prefs, room);
        let browser = focus.path.as_ref();
        if fit.navigation {
            ui.add_space(GAP);
            ui.spacing_mut().item_spacing.x = 0.0;
            let nav = [
                (
                    icon::ARROW_LEFT,
                    "Back",
                    Command::Back,
                    focus.can_go_back,
                    tab::Action::Back,
                ),
                (
                    icon::ARROW_RIGHT,
                    "Forward",
                    Command::Forward,
                    focus.can_go_forward,
                    tab::Action::Forward,
                ),
                (
                    icon::ARROW_UP,
                    "Up",
                    Command::Up,
                    browser.is_some_and(|p| p.parent().is_some()),
                    tab::Action::Up,
                ),
            ];
            for (glyph, label, command, enabled, action) in nav {
                let button = Button::new(RichText::new(glyph).size(15.0))
                    .frame(false)
                    .min_size(vec2(NAV_BUTTON, NAV_BUTTON - 4.0));
                if ui
                    .add_enabled(enabled, button)
                    .on_hover_text(keymap::hint(&ctx, label, command))
                    .clicked()
                {
                    requests.push(Request::Tab(action));
                }
            }
            ui.spacing_mut().item_spacing.x = ui.style().spacing.item_spacing.x;
        }
        if let Some(width) = fit.path {
            ui.add_space(GAP);
            if let Some(path) = path_box(ui, browser, width) {
                requests.push(Request::Tab(tab::Action::Navigate(path)));
            }
        }
        if let Some(width) = fit.filter {
            ui.add_space(GAP);
            let mut filter = focus.filter.clone();
            let hint = format!(
                "{} {}",
                icon::MAGNIFYING_GLASS,
                keymap::hint(&ctx, "Filter", Command::Filter)
            );
            let edit = TextEdit::singleline(&mut filter)
                .hint_text(hint)
                .desired_width(width);
            if ui.add_enabled(browser.is_some(), edit).changed() {
                requests.push(Request::Filter(filter));
            }
        }
        left_end = ui.cursor().left();

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            if window_button(ui, icon::X, "Close", true).clicked() {
                ctx.send_viewport_cmd(ViewportCommand::Close);
            }
            let (glyph, tip) = if maximized {
                (icon::CORNERS_IN, "Restore")
            } else {
                (icon::SQUARE, "Maximize")
            };
            if window_button(ui, glyph, tip, false).clicked() {
                ctx.send_viewport_cmd(ViewportCommand::Maximized(!maximized));
            }
            let minimize = window_button(ui, icon::MINUS, "Minimize", false);
            if minimize.clicked() {
                ctx.send_viewport_cmd(ViewportCommand::Minimized(true));
            }
            buttons_left = minimize.rect.left();
            if !toggles.is_empty() {
                ui.add_space(GAP);
                for &(pane, open) in toggles.iter().rev() {
                    let button = Button::selectable(open, RichText::new(pane.icon()).size(15.0))
                        .min_size(vec2(NAV_BUTTON, NAV_BUTTON - 4.0));
                    let tip = match pane {
                        Pane::Preview => keymap::hint(&ctx, pane.label(), Command::TogglePreview),
                        Pane::Terminal => keymap::hint(&ctx, pane.label(), Command::ToggleTerminal),
                        _ => pane.label().to_owned(),
                    };
                    let response = ui.add(button).on_hover_text(tip);
                    if response.clicked() {
                        requests.push(Request::TogglePane(pane));
                    }
                    buttons_left = response.rect.left();
                }
            }
        });
    });

    if prefs.title {
        title(ui, &background, focus, rect, left_end, buttons_left);
    }
    requests
}

/// The focused tab's title, centred on the window as system title bars
/// have it, if that leaves it clear of what's on either side.
fn title(
    ui: &Ui,
    background: &egui::Response,
    focus: &Focus,
    rect: Rect,
    left_end: f32,
    right_start: f32,
) {
    let ctx = ui.ctx();
    let font = FontId::proportional(13.0);
    let color = if ctx.input(|i| i.viewport().focused) == Some(false) {
        ui.visuals().weak_text_color()
    } else {
        ui.visuals().text_color()
    };
    let galley = ui
        .painter()
        .layout_no_wrap(focus.title.clone(), font, color);
    let size = galley.size();
    let x = (rect.center().x - size.x / 2.0).max(left_end + 12.0);
    if x + size.x + 12.0 > right_start {
        return;
    }
    let pos = pos2(x, rect.center().y - size.y / 2.0);
    let text_rect = Rect::from_min_size(pos, size);
    ui.painter().galley(pos, galley, color);
    if let Some(hover) = &focus.hover
        && background
            .hover_pos()
            .is_some_and(|p| text_rect.contains(p))
    {
        background.clone().on_hover_text(hover);
    }
}

/// The focused tab's folder; click to type another and Enter to go there.
/// Returns where to go.
fn path_box(ui: &mut Ui, path: Option<&PathBuf>, width: f32) -> Option<PathBuf> {
    let id = ui.id().with("path-box");
    let editing = ui.memory(|m| m.has_focus(id));
    let shown = path.map(|p| p.display().to_string()).unwrap_or_default();
    let mut text = if editing {
        ui.data(|d| d.get_temp::<String>(id)).unwrap_or(shown)
    } else {
        shown
    };
    let response = ui.add_enabled(
        path.is_some(),
        TextEdit::singleline(&mut text).id(id).desired_width(width),
    );
    if response.has_focus() {
        ui.data_mut(|d| d.insert_temp(id, text.clone()));
    }
    let entered = response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
    if response.lost_focus() {
        ui.data_mut(|d| d.remove::<String>(id));
    }
    (entered && !text.trim().is_empty()).then(|| PathBuf::from(text.trim()))
}

/// A minimize, maximize or close button: flat, filling the bar's height,
/// with close turning red under the pointer as on Windows.
fn window_button(ui: &mut Ui, glyph: &str, tip: &str, close: bool) -> egui::Response {
    let (rect, response) =
        ui.allocate_exact_size(vec2(BUTTON_WIDTH, ui.available_height()), Sense::click());
    let visuals = ui.visuals();
    let hot = response.hovered() || response.is_pointer_button_down_on();
    let (fill, text) = match (hot, close) {
        (true, true) => (Color32::from_rgb(0xc4, 0x2b, 0x1c), Color32::WHITE),
        (true, false) => (visuals.widgets.hovered.weak_bg_fill, visuals.text_color()),
        (false, _) => (Color32::TRANSPARENT, visuals.text_color()),
    };
    let painter = ui.painter();
    painter.rect_filled(rect, 0.0, fill);
    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        glyph,
        FontId::proportional(14.0),
        text,
    );
    response.on_hover_text(tip)
}

/// Lets the edges and corners of the viewport `ctx` is running resize it,
/// which an undecorated window can't otherwise be. Not while maximized or
/// fullscreen. Call it once a frame, after the rest of the UI.
pub fn resize_edges(ctx: &egui::Context) {
    let (maximized, fullscreen) = ctx.input(|i| {
        let v = i.viewport();
        (v.maximized.unwrap_or(false), v.fullscreen.unwrap_or(false))
    });
    if maximized || fullscreen {
        return;
    }
    for (direction, rect) in zones(ctx.content_rect()) {
        let id = Id::new(("resize-edge", format!("{direction:?}")));
        egui::Area::new(id)
            .order(Order::Foreground)
            .fixed_pos(rect.min)
            .show(ctx, |ui| {
                let response = ui.allocate_response(rect.size(), Sense::drag());
                if response.hovered() || response.dragged() {
                    ctx.set_cursor_icon(cursor(direction));
                }
                if response.drag_started_by(egui::PointerButton::Primary) {
                    ctx.send_viewport_cmd(ViewportCommand::BeginResize(direction));
                }
            });
    }
}

/// The strips along the edges of `screen` that resize the window, corners
/// first so they win where they overlap the edges.
fn zones(screen: Rect) -> [(ResizeDirection, Rect); 8] {
    use ResizeDirection as D;
    let (l, r, t, b) = (screen.left(), screen.right(), screen.top(), screen.bottom());
    let corner = |x: f32, y: f32| Rect::from_center_size(pos2(x, y), Vec2::splat(CORNER));
    [
        (D::NorthWest, corner(l, t)),
        (D::NorthEast, corner(r, t)),
        (D::SouthWest, corner(l, b)),
        (D::SouthEast, corner(r, b)),
        (D::North, Rect::from_x_y_ranges(l..=r, t..=t + EDGE)),
        (D::South, Rect::from_x_y_ranges(l..=r, b - EDGE..=b)),
        (D::West, Rect::from_x_y_ranges(l..=l + EDGE, t..=b)),
        (D::East, Rect::from_x_y_ranges(r - EDGE..=r, t..=b)),
    ]
    .map(|(direction, rect)| (direction, rect.intersect(screen)))
}

fn cursor(direction: ResizeDirection) -> CursorIcon {
    use ResizeDirection as D;
    match direction {
        D::North => CursorIcon::ResizeNorth,
        D::South => CursorIcon::ResizeSouth,
        D::East => CursorIcon::ResizeEast,
        D::West => CursorIcon::ResizeWest,
        D::NorthEast => CursorIcon::ResizeNorthEast,
        D::NorthWest => CursorIcon::ResizeNorthWest,
        D::SouthEast => CursorIcon::ResizeSouthEast,
        D::SouthWest => CursorIcon::ResizeSouthWest,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_setting_from_saved_state() {
        let save = |app: &str| {
            let kv =
                std::collections::HashMap::from([(eframe::APP_KEY.to_owned(), app.to_owned())]);
            ron::to_string(&kv).unwrap()
        };
        assert!(enabled_in(&save(
            "(show_hidden: true, titlebar: (enabled: true))"
        )));
        assert!(!enabled_in(&save(
            "(show_hidden: true, titlebar: (enabled: false))"
        )));
        // Saved before the setting existed, or nothing saved.
        assert!(!enabled_in(&save("(show_hidden: true)")));
        assert!(!enabled_in(""));
    }

    #[test]
    fn requests_reach_the_focused_browser_tab() {
        use crate::{dock_tab::PaneTab, tab::BrowserTab};
        let browser = |id, path: &str| DockTab::Browser(BrowserTab::new(id, PathBuf::from(path)));
        let mut dock = DockState::new(vec![browser(1, "/tmp"), browser(2, "/usr/lib")]);
        dock.push_to_first_leaf(DockTab::Pane(PaneTab {
            pane: Pane::Preview,
        }));

        // A pane is focused (pushed last): the bar follows the active browser.
        let (focus, target) = focus(&mut dock, Some(2));
        assert_eq!(target, Some(2));
        assert_eq!(focus.path, Some(PathBuf::from("/usr/lib")));
        assert_eq!(focus.title, "lib");
        assert!(!focus.can_go_back);

        let panes = apply(
            &mut dock,
            target,
            vec![
                Request::Tab(tab::Action::Up),
                Request::Filter("so".into()),
                Request::TogglePane(Pane::Terminal),
            ],
        );
        assert_eq!(panes, [Pane::Terminal]);
        let tab = dock
            .iter_all_tabs()
            .find_map(|(_, t)| t.browser().filter(|b| b.id == 2))
            .unwrap();
        assert_eq!(tab.filter, "so");
        assert!(matches!(tab.queued[..], [tab::Action::Up]));
    }

    #[test]
    fn parts_fit_in_order_and_leave_room_to_drag() {
        let all = Prefs {
            enabled: true,
            title: true,
            navigation: true,
            path_box: true,
            filter_box: true,
            pane_toggles: true,
        };
        let nav = 3.0 * NAV_BUTTON + GAP;
        // Plenty of room: everything at its widest.
        assert_eq!(
            fit(&all, 2000.0),
            Fit {
                navigation: true,
                path: Some(PATH_WIDTH.1),
                filter: Some(FILTER_WIDTH.1),
            }
        );
        // Room for navigation and the path at its least only.
        let room = MIN_DRAG + nav + PATH_WIDTH.0 + GAP + 10.0;
        assert_eq!(
            fit(&all, room),
            Fit {
                navigation: true,
                path: Some(PATH_WIDTH.0 + 10.0),
                filter: None,
            }
        );
        // Nothing fits but the room to drag.
        assert_eq!(
            fit(&all, MIN_DRAG + 10.0),
            Fit {
                navigation: false,
                path: None,
                filter: None,
            }
        );
        // Turned off stays off.
        let defaults = Prefs::default();
        assert_eq!(
            fit(&defaults, 2000.0),
            Fit {
                navigation: true,
                path: None,
                filter: None,
            }
        );
    }

    #[test]
    fn resize_zones_cover_the_edges_and_stay_inside() {
        let screen = Rect::from_min_size(pos2(0.0, 0.0), vec2(800.0, 600.0));
        let zones = zones(screen);
        for (direction, rect) in zones {
            assert!(screen.contains_rect(rect), "{direction:?}: {rect:?}");
            assert!(rect.area() > 0.0, "{direction:?}");
        }
        // The first zone holding a point is the one that takes the drag.
        let at = |x, y| {
            zones
                .iter()
                .find(|(_, r)| r.contains(pos2(x, y)))
                .map(|(d, _)| *d)
        };
        assert_eq!(at(1.0, 1.0), Some(ResizeDirection::NorthWest));
        assert_eq!(at(400.0, 2.0), Some(ResizeDirection::North));
        assert_eq!(at(799.0, 300.0), Some(ResizeDirection::East));
        assert_eq!(at(798.0, 598.0), Some(ResizeDirection::SouthEast));
        assert_eq!(at(400.0, 300.0), None);
    }
}
