//! Pictures made from a document's own text: formulas and diagrams.
//!
//! They go through the [images store](super::images) like any other picture,
//! under a key that says everything needed to draw them again. The key starts
//! with a NUL, which no markdown link can (CommonMark replaces them), so a
//! document can't name one.

use std::fmt::Write as _;

use egui::Color32;

use super::{diagram, diagram::Colors, math};

const MARK: char = '\0';
const MATH: &str = "\0math\0";
const DIAGRAM: &str = "\0diagram\0";

pub fn is_drawing(key: &str) -> bool {
    key.starts_with(MARK)
}

/// One at a time, as they can take seconds. See [`diagram`].
pub fn is_diagram(key: &str) -> bool {
    key.starts_with(DIAGRAM)
}

pub fn svg_color(color: Color32) -> String {
    let [r, g, b, _] = color.to_srgba_unmultiplied();
    format!("#{r:02x}{g:02x}{b:02x}")
}

/// The key of a formula. `size` is in points; `width`, also in points, is for
/// display math, where a tag goes against the right edge.
pub fn math_key(
    tex: &str,
    display: bool,
    size: f32,
    color: Color32,
    pixels_per_point: f32,
    width: Option<f32>,
) -> String {
    let mut key = String::with_capacity(tex.len() + 40);
    let _ = write!(
        key,
        "{MATH}{}\0{size}\0{pixels_per_point}\0{}\0",
        u8::from(display),
        svg_color(color),
    );
    // Only a tag cares, and any other formula needn't be drawn again for each
    // width of the page.
    if let Some(width) = width.filter(|_| tex.contains("\\tag")) {
        let _ = write!(key, "{width}");
    }
    key.push(MARK);
    key.push_str(tex);
    key
}

pub fn diagram_key(code: &str, colors: &Colors) -> String {
    format!(
        "{DIAGRAM}{}\0{}\0{}\0{}\0{code}",
        u8::from(colors.dark),
        colors.background,
        colors.text,
        colors.line,
    )
}

/// What a drawing is, before rasterizing.
pub struct Drawn {
    pub svg: String,
    /// How many pixels for each of the SVG's units: `None` for the usual.
    pub pixels_per_unit: Option<f32>,
    /// How far it goes below the baseline, in points.
    pub depth: f32,
}

/// Makes the SVG of a key from [`math_key`] or [`diagram_key`].
pub fn draw(key: &str) -> Result<Drawn, String> {
    let bad = || "not a drawing".to_owned();
    if let Some(rest) = key.strip_prefix(MATH) {
        let mut parts = rest.splitn(6, MARK);
        let mut next = || parts.next().ok_or_else(bad);
        let display = next()? == "1";
        let size: f32 = next()?.parse().map_err(|_| bad())?;
        let pixels_per_point: f32 = next()?.parse().map_err(|_| bad())?;
        let color = next()?;
        let width = next()?.parse().ok();
        let tex = next()?;
        let formula = math::svg(tex, display, size, color, width)?;
        Ok(Drawn {
            svg: formula.svg,
            pixels_per_unit: Some(pixels_per_point),
            depth: formula.depth,
        })
    } else if let Some(rest) = key.strip_prefix(DIAGRAM) {
        let mut parts = rest.splitn(5, MARK);
        let mut next = || parts.next().ok_or_else(bad);
        let colors = Colors {
            dark: next()? == "1",
            background: next()?.to_owned(),
            text: next()?.to_owned(),
            line: next()?.to_owned(),
        };
        let code = next()?;
        Ok(Drawn {
            svg: diagram::svg(code, &colors)?,
            pixels_per_unit: None,
            depth: 0.0,
        })
    } else {
        Err(bad())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_says_everything_needed_to_draw_it() {
        let key = math_key(r"\frac{a}{b}", true, 18.0, Color32::RED, 2.0, Some(600.0));
        assert!(is_drawing(&key));
        assert!(!is_diagram(&key));
        let drawn = draw(&key).unwrap();
        assert!(drawn.svg.contains("#ff0000"));
        assert_eq!(drawn.pixels_per_unit, Some(2.0));
        assert!(drawn.depth > 0.0);

        let colors = Colors {
            dark: false,
            background: "#ffffff".to_owned(),
            text: "#111111".to_owned(),
            line: "#777777".to_owned(),
        };
        let key = diagram_key("flowchart LR\n A --> B\n", &colors);
        assert!(is_diagram(&key));
        let drawn = draw(&key).unwrap();
        assert!(drawn.svg.contains("#ffffff"));
    }

    #[test]
    fn only_a_tag_makes_a_formula_depend_on_the_width() {
        let key = |tex, width| math_key(tex, true, 18.0, Color32::RED, 1.0, Some(width));
        assert_eq!(key("x", 400.0), key("x", 600.0));
        assert_ne!(key(r"\tag{1} x", 400.0), key(r"\tag{1} x", 600.0));
        assert!(draw(&key(r"\tag{1} x", 400.0)).is_ok());
    }

    #[test]
    fn what_is_not_a_drawing_is_not_drawn() {
        assert!(draw("\0math\0").is_err());
        assert!(draw("/some/path.png").is_err());
    }
}
