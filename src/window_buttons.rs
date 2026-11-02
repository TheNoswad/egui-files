//! Minimize, maximize and close for the in-app title bar: which show, on
//! which side, and drawn like which platform's. By default they follow the
//! desktop: GNOME's `button-layout`, KDE's `kwinrc`, or the platform's own.

use std::sync::OnceLock;

use egui::{Color32, Pos2, Rect, Sense, Stroke, Ui, pos2, vec2};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Style {
    /// The desktop's.
    #[default]
    Auto,
    Windows,
    Adwaita,
    Breeze,
    MacOs,
}

impl Style {
    pub const ALL: [Self; 5] = [
        Self::Auto,
        Self::Windows,
        Self::Adwaita,
        Self::Breeze,
        Self::MacOs,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Like the desktop",
            Self::Windows => "Windows",
            Self::Adwaita => "Adwaita (GNOME)",
            Self::Breeze => "Breeze (KDE)",
            Self::MacOs => "macOS",
        }
    }

    /// Width of each button's slot, and the padding on the window's side
    /// of the group.
    fn slot(self) -> (f32, f32) {
        match self {
            // Windows' are 46 wide, but three of these must fit the dock's
            // tab bar margin (an i8) in tab windows.
            Self::Windows | Self::Auto => (42.0, 0.0),
            Self::Adwaita => (34.0, 6.0),
            Self::Breeze => (30.0, 6.0),
            Self::MacOs => (20.0, 10.0),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Button {
    Minimize,
    Maximize,
    Close,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Side {
    Left,
    #[default]
    Right,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    /// Show the buttons the desktop does, where it does; the fields below
    /// apply otherwise.
    pub follow_desktop: bool,
    pub side: Side,
    pub minimize: bool,
    pub maximize: bool,
    pub close: bool,
    pub style: Style,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            follow_desktop: true,
            side: Side::Right,
            minimize: true,
            maximize: true,
            close: true,
            style: Style::Auto,
        }
    }
}

impl Prefs {
    pub fn layout(&self) -> Layout {
        if self.follow_desktop {
            return desktop().layout.clone();
        }
        let style = self.style();
        // macOS and buttons on the left start with close; otherwise close
        // is at the window's edge.
        let mut order = vec![
            (Button::Close, self.close),
            (Button::Minimize, self.minimize),
            (Button::Maximize, self.maximize),
        ];
        if self.side == Side::Right && style != Style::MacOs {
            order.rotate_left(1);
        }
        let buttons: Vec<Button> = order
            .into_iter()
            .filter(|(_, on)| *on)
            .map(|(b, _)| b)
            .collect();
        match self.side {
            Side::Left => Layout {
                left: buttons,
                right: Vec::new(),
            },
            Side::Right => Layout {
                left: Vec::new(),
                right: buttons,
            },
        }
    }

    /// The style to draw in, never [`Style::Auto`].
    pub fn style(&self) -> Style {
        match self.style {
            Style::Auto => desktop().style,
            style => style,
        }
    }
}

/// The buttons at each end of the bar, in order from left to right.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Layout {
    pub left: Vec<Button>,
    pub right: Vec<Button>,
}

/// What the desktop does with its title bar buttons.
pub struct Desktop {
    pub name: &'static str,
    pub layout: Layout,
    pub style: Style,
}

/// Read once, the first time it's needed.
pub fn desktop() -> &'static Desktop {
    static DESKTOP: OnceLock<Desktop> = OnceLock::new();
    DESKTOP.get_or_init(detect)
}

/// Minimize, maximize and close on the right, as on Windows.
fn windows_layout() -> Layout {
    Layout {
        left: Vec::new(),
        right: vec![Button::Minimize, Button::Maximize, Button::Close],
    }
}

fn detect() -> Desktop {
    if cfg!(windows) {
        return Desktop {
            name: "Windows",
            layout: windows_layout(),
            style: Style::Windows,
        };
    }
    if cfg!(target_os = "macos") {
        return Desktop {
            name: "macOS",
            layout: Layout {
                left: vec![Button::Close, Button::Minimize, Button::Maximize],
                right: Vec::new(),
            },
            style: Style::MacOs,
        };
    }
    let current = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    let is = |names: &[&str]| {
        current
            .split(':')
            .any(|d| names.iter().any(|n| d.eq_ignore_ascii_case(n)))
    };
    if is(&["KDE"]) {
        return Desktop {
            name: "KDE",
            layout: kde_layout(),
            style: Style::Breeze,
        };
    }
    if is(&["GNOME", "Unity", "Budgie", "Pantheon"]) {
        let layout = gnome_setting()
            .and_then(|s| parse_gnome(&s))
            // GNOME's own default.
            .unwrap_or_else(|| Layout {
                left: Vec::new(),
                right: vec![Button::Close],
            });
        return Desktop {
            name: "GNOME",
            layout,
            style: Style::Adwaita,
        };
    }
    Desktop {
        name: "other",
        layout: windows_layout(),
        style: Style::Adwaita,
    }
}

/// `org.gnome.desktop.wm.preferences button-layout`, quoted as gsettings
/// prints it.
fn gnome_setting() -> Option<String> {
    let output = std::process::Command::new("gsettings")
        .args(["get", "org.gnome.desktop.wm.preferences", "button-layout"])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// A GNOME button layout like `'appmenu:minimize,maximize,close'`: the
/// left side's buttons, a colon, the right side's. Names other than the
/// three buttons (appmenu, icon, spacer) are left out.
fn parse_gnome(setting: &str) -> Option<Layout> {
    let setting = setting.trim().trim_matches('\'');
    let (left, right) = setting.split_once(':')?;
    let side = |names: &str| {
        names
            .split(',')
            .filter_map(|name| match name.trim() {
                "minimize" => Some(Button::Minimize),
                "maximize" => Some(Button::Maximize),
                "close" => Some(Button::Close),
                _ => None,
            })
            .collect()
    };
    Some(Layout {
        left: side(left),
        right: side(right),
    })
}

fn kde_layout() -> Layout {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::home_dir().map(|h| h.join(".config")));
    let text = config
        .and_then(|dir| std::fs::read_to_string(dir.join("kwinrc")).ok())
        .unwrap_or_default();
    parse_kwinrc(&text)
}

/// The buttons in `kwinrc`'s `[org.kde.kdecoration2]` section, a letter
/// each: X close, I minimize, A maximize (the others aren't ours). KDE's
/// defaults when unset: `MS` on the left, `HIAX` on the right.
fn parse_kwinrc(text: &str) -> Layout {
    let mut section = "";
    let (mut left, mut right) = ("MS".to_owned(), "HIAX".to_owned());
    for line in text.lines().map(str::trim) {
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = name;
        } else if section == "org.kde.kdecoration2"
            && let Some((key, value)) = line.split_once('=')
        {
            match key.trim() {
                "ButtonsOnLeft" => left = value.trim().to_owned(),
                "ButtonsOnRight" => right = value.trim().to_owned(),
                _ => {}
            }
        }
    }
    let side = |letters: &str| {
        letters
            .chars()
            .filter_map(|c| match c {
                'I' => Some(Button::Minimize),
                'A' => Some(Button::Maximize),
                'X' => Some(Button::Close),
                _ => None,
            })
            .collect()
    };
    Layout {
        left: side(&left),
        right: side(&right),
    }
}

#[cfg(any(windows, test))]
/// Where maximize is, if shown: the side of the window its group is at,
/// and how far its near and far edges are from that side, in points.
pub fn maximize_span(layout: &Layout, style: Style) -> Option<(Side, f32, f32)> {
    let (slot, pad) = style.slot();
    if let Some(i) = layout.left.iter().position(|&b| b == Button::Maximize) {
        let near = pad + i as f32 * slot;
        return Some((Side::Left, near, near + slot));
    }
    let i = layout.right.iter().position(|&b| b == Button::Maximize)?;
    // The right group starts with its slots and ends with the padding.
    let group = width(&layout.right, style);
    let far = group - i as f32 * slot;
    Some((Side::Right, far - slot, far))
}

/// How wide a group of `buttons` is in `style`.
pub fn width(buttons: &[Button], style: Style) -> f32 {
    if buttons.is_empty() {
        return 0.0;
    }
    let (slot, pad) = style.slot();
    buttons.len() as f32 * slot + pad
}

/// Draws `buttons` across `rect`, which is [`width`] wide, at the window's
/// `edge`. Returns the one clicked.
pub fn show(
    ui: &mut Ui,
    rect: Rect,
    buttons: &[Button],
    style: Style,
    edge: Side,
    maximized: bool,
) -> Option<Button> {
    let (slot, pad) = style.slot();
    let start = match edge {
        Side::Left => rect.left() + pad,
        Side::Right => rect.left(),
    };
    let slots: Vec<Rect> = (0..buttons.len())
        .map(|i| {
            let x = start + i as f32 * slot;
            Rect::from_x_y_ranges(x..=x + slot, rect.y_range())
        })
        .collect();
    // Over maximize, Windows tells the subclass rather than egui (for the
    // snap layouts flyout).
    #[cfg(windows)]
    let native = crate::windows_frame::maximize_state(ui.ctx());
    #[cfg(not(windows))]
    let native = (false, false);
    let group_hovered = ui.rect_contains_pointer(rect) || native.0;
    let focused = ui.ctx().input(|i| i.viewport().focused) != Some(false);
    let mut clicked = None;
    for (&button, &slot) in buttons.iter().zip(&slots) {
        let id = ui.id().with(("window-button", button as u8));
        let response = ui.interact(slot, id, Sense::click());
        let tip = match button {
            Button::Minimize => "Minimize",
            Button::Maximize if maximized => "Restore",
            Button::Maximize => "Maximize",
            Button::Close => "Close",
        };
        let maximize = button == Button::Maximize;
        let state = State {
            hovered: response.hovered() || maximize && native.0,
            pressed: response.is_pointer_button_down_on() || maximize && native.1,
            group_hovered,
            focused,
            maximized,
        };
        paint(ui, slot, button, style, state);
        if response.on_hover_text(tip).clicked() {
            clicked = Some(button);
        }
    }
    clicked
}

#[derive(Clone, Copy)]
struct State {
    hovered: bool,
    pressed: bool,
    /// The pointer is over any of the group (macOS shows its glyphs then).
    group_hovered: bool,
    focused: bool,
    maximized: bool,
}

fn paint(ui: &Ui, slot: Rect, button: Button, style: Style, state: State) {
    let painter = ui.painter();
    let visuals = ui.visuals();
    let text = if state.focused {
        visuals.text_color()
    } else {
        visuals.weak_text_color()
    };
    let centre = slot.center();
    match style {
        Style::Windows | Style::Auto => {
            let close = button == Button::Close;
            let (fill, ink) = match (state.hovered || state.pressed, close) {
                (true, true) => (Color32::from_rgb(0xc4, 0x2b, 0x1c), Color32::WHITE),
                (true, false) => (visuals.widgets.hovered.weak_bg_fill, text),
                (false, _) => (Color32::TRANSPARENT, text),
            };
            painter.rect_filled(slot, 0.0, fill);
            glyph(
                ui,
                centre,
                button,
                Shape::Windows,
                state.maximized,
                10.0,
                Stroke::new(1.0, ink),
            );
        }
        Style::Adwaita => {
            let alpha = match (state.pressed, state.hovered) {
                (true, _) => 0.28,
                (_, true) => 0.18,
                _ => 0.1,
            };
            painter.circle_filled(centre, 12.0, text.gamma_multiply(alpha));
            glyph(
                ui,
                centre,
                button,
                Shape::Adwaita,
                state.maximized,
                8.0,
                Stroke::new(1.3, text),
            );
        }
        Style::Breeze => {
            let hot = state.hovered || state.pressed;
            let ink = if hot {
                let fill = if button == Button::Close {
                    Color32::from_rgb(0xda, 0x44, 0x53)
                } else {
                    text
                };
                painter.circle_filled(centre, 9.0, fill);
                if button == Button::Close {
                    Color32::WHITE
                } else {
                    visuals.panel_fill
                }
            } else {
                text
            };
            glyph(
                ui,
                centre,
                button,
                Shape::Breeze,
                state.maximized,
                8.0,
                Stroke::new(1.3, ink),
            );
        }
        Style::MacOs => {
            let colour = if state.focused || state.group_hovered {
                match button {
                    Button::Close => Color32::from_rgb(0xff, 0x5f, 0x57),
                    Button::Minimize => Color32::from_rgb(0xfe, 0xbc, 0x2e),
                    Button::Maximize => Color32::from_rgb(0x28, 0xc8, 0x40),
                }
            } else {
                visuals.widgets.inactive.bg_fill
            };
            let colour = if state.pressed {
                colour.gamma_multiply(0.8)
            } else {
                colour
            };
            painter.circle_filled(centre, 6.0, colour);
            if state.group_hovered {
                let ink = Color32::from_black_alpha(150);
                glyph(
                    ui,
                    centre,
                    button,
                    Shape::MacOs,
                    state.maximized,
                    6.0,
                    Stroke::new(1.2, ink),
                );
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Shape {
    Windows,
    Adwaita,
    Breeze,
    MacOs,
}

/// A button's symbol, `size` across, centred on `c`.
fn glyph(
    ui: &Ui,
    c: Pos2,
    button: Button,
    shape: Shape,
    maximized: bool,
    size: f32,
    stroke: Stroke,
) {
    let painter = ui.painter();
    let h = size / 2.0;
    // Lines land on pixel centres, so 1-point strokes stay sharp.
    let c = pos2(c.x.round() + 0.5, c.y.round() + 0.5);
    let line = |a: Pos2, b: Pos2| {
        painter.line_segment([a, b], stroke);
    };
    let square = |r: Rect| {
        painter.rect_stroke(r, 0.0, stroke, egui::StrokeKind::Middle);
    };
    match (button, shape) {
        (Button::Close, _) => {
            line(pos2(c.x - h, c.y - h), pos2(c.x + h, c.y + h));
            line(pos2(c.x - h, c.y + h), pos2(c.x + h, c.y - h));
        }
        (Button::Minimize, Shape::Breeze) => {
            line(pos2(c.x - h, c.y - h / 2.0), pos2(c.x, c.y + h / 2.0));
            line(pos2(c.x, c.y + h / 2.0), pos2(c.x + h, c.y - h / 2.0));
        }
        (Button::Minimize, Shape::Adwaita) => {
            line(pos2(c.x - h, c.y + h / 2.0), pos2(c.x + h, c.y + h / 2.0));
        }
        (Button::Minimize, _) => line(pos2(c.x - h, c.y), pos2(c.x + h, c.y)),
        (Button::Maximize, Shape::Breeze) if maximized => {
            let pts = [
                pos2(c.x, c.y - h),
                pos2(c.x + h, c.y),
                pos2(c.x, c.y + h),
                pos2(c.x - h, c.y),
            ];
            for i in 0..4 {
                line(pts[i], pts[(i + 1) % 4]);
            }
        }
        (Button::Maximize, Shape::Breeze) => {
            line(pos2(c.x - h, c.y + h / 2.0), pos2(c.x, c.y - h / 2.0));
            line(pos2(c.x, c.y - h / 2.0), pos2(c.x + h, c.y + h / 2.0));
        }
        (Button::Maximize, Shape::MacOs) => {
            line(pos2(c.x - h, c.y), pos2(c.x + h, c.y));
            line(pos2(c.x, c.y - h), pos2(c.x, c.y + h));
        }
        (Button::Maximize, _) if maximized => {
            // Restore: a window in front of another.
            let d = (size * 0.2).round();
            let front = Rect::from_min_max(pos2(c.x - h, c.y - h + d), pos2(c.x + h - d, c.y + h));
            square(front);
            line(pos2(c.x - h + d, c.y - h + d), pos2(c.x - h + d, c.y - h));
            line(pos2(c.x - h + d, c.y - h), pos2(c.x + h, c.y - h));
            line(pos2(c.x + h, c.y - h), pos2(c.x + h, c.y + h - d));
            line(pos2(c.x + h, c.y + h - d), pos2(c.x + h - d, c.y + h - d));
        }
        (Button::Maximize, _) => square(Rect::from_center_size(c, vec2(size, size))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Button::*;

    #[test]
    fn reads_gnome_layouts() {
        let layout = |s| parse_gnome(s).unwrap();
        assert_eq!(
            layout("'appmenu:close'\n"),
            Layout {
                left: vec![],
                right: vec![Close]
            }
        );
        assert_eq!(
            layout("'close,minimize,maximize:appmenu'"),
            Layout {
                left: vec![Close, Minimize, Maximize],
                right: vec![]
            }
        );
        assert_eq!(
            layout("'icon:minimize,spacer,maximize,close'"),
            Layout {
                left: vec![],
                right: vec![Minimize, Maximize, Close]
            }
        );
        assert_eq!(parse_gnome("garbage"), None);
    }

    #[test]
    fn reads_kde_layouts() {
        // Nothing set: KDE's defaults.
        assert_eq!(parse_kwinrc(""), windows_layout());
        let text = "[Other]\nButtonsOnRight=X\n\n[org.kde.kdecoration2]\nButtonsOnLeft=XAI\nButtonsOnRight=\n";
        assert_eq!(
            parse_kwinrc(text),
            Layout {
                left: vec![Close, Maximize, Minimize],
                right: vec![]
            }
        );
    }

    #[test]
    fn chosen_layouts_keep_platform_order() {
        let prefs = |side, style, minimize| Prefs {
            follow_desktop: false,
            side,
            minimize,
            maximize: true,
            close: true,
            style,
        };
        assert_eq!(
            prefs(Side::Right, Style::Windows, true).layout().right,
            [Minimize, Maximize, Close]
        );
        assert_eq!(
            prefs(Side::Left, Style::Adwaita, true).layout().left,
            [Close, Minimize, Maximize]
        );
        assert_eq!(
            prefs(Side::Right, Style::MacOs, false).layout().right,
            [Close, Maximize]
        );
    }

    #[test]
    fn finds_maximize() {
        let right = Layout {
            left: vec![],
            right: vec![Minimize, Maximize, Close],
        };
        // Close is the last 42 points, maximize the 42 before.
        assert_eq!(
            maximize_span(&right, Style::Windows),
            Some((Side::Right, 42.0, 84.0))
        );
        let left = Layout {
            left: vec![Close, Minimize, Maximize],
            right: vec![],
        };
        assert_eq!(
            maximize_span(&left, Style::MacOs),
            Some((Side::Left, 50.0, 70.0))
        );
        let none = Layout {
            left: vec![],
            right: vec![Close],
        };
        assert_eq!(maximize_span(&none, Style::Adwaita), None);
    }

    #[test]
    fn widths_add_up() {
        assert_eq!(width(&[], Style::MacOs), 0.0);
        assert_eq!(width(&[Close], Style::Windows), 42.0);
        assert_eq!(width(&[Close, Minimize], Style::Adwaita), 74.0);
    }
}
