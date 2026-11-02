//! An optional title bar drawn by the app, in place of the system's: the
//! menus at its left, the window buttons at its right, and the space between
//! to drag the window by. Without the system frame the window also needs
//! edges to resize it by, which [`resize_edges`] adds.

use egui::{
    Align2, Color32, CursorIcon, FontId, Id, Order, Panel, Rect, ResizeDirection, Sense, Ui, Vec2,
    ViewportCommand, pos2, vec2,
};
use egui_phosphor::regular as icon;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    /// Draw the title bar in the app; off keeps the system's.
    pub enabled: bool,
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

/// Draws the bar along the top of the viewport `ui` is in, with `menus` at
/// its left and `title` in the middle when there's room.
pub fn show(ui: &mut Ui, id: &str, title: &str, menus: impl FnOnce(&mut Ui)) {
    let frame = egui::Frame::side_top_panel(ui.style()).inner_margin(egui::Margin::ZERO);
    Panel::top(Id::new(("titlebar", id)))
        .exact_size(HEIGHT)
        .resizable(false)
        .frame(frame)
        .show(ui, |ui| bar(ui, title, menus));
}

fn bar(ui: &mut Ui, title: &str, menus: impl FnOnce(&mut Ui)) {
    let rect = ui.max_rect();
    let ctx = ui.ctx().clone();
    let maximized = ctx.input(|i| i.viewport().maximized.unwrap_or(false));

    // Under everything else in the bar: what's left over moves the window.
    let background = ui.interact(rect, ui.id().with("drag"), Sense::click_and_drag());
    if background.double_clicked() {
        ctx.send_viewport_cmd(ViewportCommand::Maximized(!maximized));
    } else if background.drag_started_by(egui::PointerButton::Primary) {
        ctx.send_viewport_cmd(ViewportCommand::StartDrag);
    }

    let mut menus_right = rect.left();
    let mut buttons_left = rect.right();
    ui.horizontal_centered(|ui| {
        ui.add_space(6.0);
        let response = ui.scope(menus).response;
        menus_right = response.rect.right();
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
        });
    });

    // Centred on the window, as system title bars are, if that leaves it
    // clear of the menus and buttons.
    let font = FontId::proportional(13.0);
    let color = if ctx.input(|i| i.viewport().focused) == Some(false) {
        ui.visuals().weak_text_color()
    } else {
        ui.visuals().text_color()
    };
    let galley = ui.painter().layout_no_wrap(title.to_owned(), font, color);
    let width = galley.size().x;
    let centred = rect.center().x - width / 2.0;
    let x = centred.max(menus_right + 12.0);
    if x + width + 12.0 <= buttons_left {
        let pos = pos2(x, rect.center().y - galley.size().y / 2.0);
        ui.painter().galley(pos, galley, color);
    }
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
