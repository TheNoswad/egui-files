//! Mermaid diagrams, as an SVG.
//!
//! Laying one out takes time that grows quickly with its size, with no way to
//! stop it (a flowchart of 200 nodes and 400 edges takes 15 s), so there's a
//! limit to what's tried. The images store draws one at a time, and in the
//! browser that's on the page's only thread, hence its lower limit.

use std::sync::Once;

use mermaid_rs_renderer::{RenderOptions, Theme};

const MAX_BYTES: usize = 64 << 10;
#[cfg(not(target_arch = "wasm32"))]
const MAX_LINES: usize = 400;
#[cfg(target_arch = "wasm32")]
const MAX_LINES: usize = 100;

/// The family SVGs get it under, in the images store.
const FONT_FAMILY: &str = "Inter";

/// Pie slices, which the renderer's own colors make too dark on a dark page.
const PIE: [&str; 12] = [
    "#4e79a7", "#e15759", "#59a14f", "#b07aa1", "#f28e2b", "#76b7b2", "#edc948", "#9c755f",
    "#ff9da7", "#bab0ac", "#86bc86", "#a0cbe8",
];

/// A diagram's colors, as SVG colors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Colors {
    pub dark: bool,
    pub background: String,
    pub text: String,
    pub line: String,
}

/// Why this diagram isn't drawn, when it's too big to try.
pub fn refusal(code: &str) -> Option<&'static str> {
    (code.len() > MAX_BYTES || code.lines().count() > MAX_LINES)
        .then_some("This diagram is too big to draw")
}

pub fn svg(code: &str, colors: &Colors) -> Result<String, String> {
    if let Some(refusal) = refusal(code) {
        return Err(refusal.to_owned());
    }
    // Measured with the font it's drawn with, on every platform.
    static FONT: Once = Once::new();
    FONT.call_once(|| {
        mermaid_rs_renderer::register_font(crate::theme::fonts::INTER_REGULAR.to_vec());
    });
    let options = RenderOptions {
        theme: theme(colors),
        ..RenderOptions::default()
    };
    mermaid_rs_renderer::render_strict(code, options).map_err(|err| err.to_string())
}

fn theme(colors: &Colors) -> Theme {
    let mut theme = if colors.dark {
        Theme::dark()
    } else {
        Theme::modern()
    };
    theme.font_family = FONT_FAMILY.to_owned();
    theme.background.clone_from(&colors.background);
    theme.edge_label_background.clone_from(&colors.background);
    theme.text_color.clone_from(&colors.text);
    theme.primary_text_color.clone_from(&colors.text);
    theme.pie_title_text_color.clone_from(&colors.text);
    theme.pie_legend_text_color.clone_from(&colors.text);
    theme.line_color.clone_from(&colors.line);
    theme.pie_colors = PIE.map(str::to_owned);
    theme
}

#[cfg(test)]
mod tests {
    use super::*;

    fn colors() -> Colors {
        Colors {
            dark: true,
            background: "#202020".to_owned(),
            text: "#e0e0e0".to_owned(),
            line: "#909090".to_owned(),
        }
    }

    #[test]
    fn diagrams_draw_in_the_colors_given() {
        let svg = svg("flowchart LR\n A --> B\n", &colors()).unwrap();
        assert!(svg.contains("<svg"));
        assert!(svg.contains("#202020"));
    }

    #[test]
    fn what_is_not_a_diagram_says_so() {
        assert!(svg("this is not a diagram", &colors()).is_err());
    }

    #[test]
    fn huge_diagrams_are_not_tried() {
        let huge = format!("flowchart TD\n{}", " a --> b\n".repeat(MAX_LINES));
        assert!(refusal(&huge).is_some());
        assert!(svg(&huge, &colors()).is_err());
        assert_eq!(refusal("flowchart TD\n a --> b\n"), None);
    }

    /// A label that became markup could have the SVG load a file.
    #[test]
    fn labels_are_escaped() {
        let svg = svg(
            "flowchart LR\n A[\"<image href='/x.png'/>\"] --> B\n",
            &colors(),
        )
        .unwrap();
        assert!(!svg.contains("<image"));
    }
}
