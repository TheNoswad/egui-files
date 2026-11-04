//! Formulas: KaTeX's layout, as an SVG.
//!
//! Documents come from anywhere, and a formula is a small program. Two kinds
//! of them get past KaTeX's own limit on expanding macros, which counts
//! expansions and not what they produce: `\def\a#1{\a{#1#1}}\a{x}` doubles its
//! argument each time, reaching gigabytes within a second, and nesting
//! overflows the stack, which can't be caught and takes the whole app with it.
//! So [`refusal`] turns those away before KaTeX sees them.

use katex_native::katex;

/// Longer than this isn't a formula: 5000 fractions take over half a second.
const MAX_BYTES: usize = 16 << 10;
/// As deep as a formula may nest. Real ones stay under ten; everything KaTeX
/// has survives 64 on a 1 MiB stack (a browser's), and built-in macros can add
/// a few levels for each one written.
const MAX_DEPTH: usize = 24;

/// A formula ready to rasterize.
pub struct Formula {
    pub svg: String,
    /// How far it reaches below the baseline, in the SVG's units.
    pub depth: f32,
}

/// Why this formula isn't drawn, when it's one that could do harm.
pub fn refusal(tex: &str) -> Option<&'static str> {
    if tex.len() > MAX_BYTES {
        return Some("This formula is too long to draw");
    }
    // Where each open group started, and whether it's an environment, whose
    // cells end what's implicitly open in them.
    let mut groups: Vec<(usize, bool)> = Vec::new();
    let mut depth = 0;
    let mut chars = tex.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        let mut open = |depth: &mut usize, environment| {
            groups.push((*depth, environment));
            *depth += 1;
        };
        match c {
            '\\' => {
                let start = at + 1;
                let mut end = start;
                while let Some(&(next, c)) = chars.peek() {
                    if !(c.is_ascii_alphabetic() || c == '@') {
                        break;
                    }
                    end = next + c.len_utf8();
                    chars.next();
                }
                if end == start {
                    // A control symbol, which is any one character.
                    if let Some((_, symbol)) = chars.next() {
                        match symbol {
                            '(' => depth += 1,
                            '\\' => end_cell(&groups, &mut depth),
                            _ => {}
                        }
                    }
                    continue;
                }
                match &tex[start..end] {
                    "def" | "gdef" | "edef" | "xdef" | "let" | "futurelet" | "global" | "long"
                    | "newcommand" | "renewcommand" | "providecommand" | "expandafter"
                    | "csname" => return Some("Formulas can't define macros here"),
                    "bgroup" | "begingroup" | "left" => open(&mut depth, false),
                    "begin" => open(&mut depth, true),
                    "egroup" | "endgroup" | "right" | "end" => close(&mut groups, &mut depth),
                    // What applies up to the end of its group, by parsing the
                    // rest of it one level further down.
                    "color" | "rm" | "sf" | "tt" | "bf" | "it" | "cal" | "displaystyle"
                    | "textstyle" | "scriptstyle" | "scriptscriptstyle" | "tiny" | "sixptsize"
                    | "scriptsize" | "footnotesize" | "small" | "normalsize" | "large"
                    | "Large" | "LARGE" | "huge" | "Huge" => depth += 1,
                    _ => {}
                }
            }
            '{' => open(&mut depth, false),
            '}' => close(&mut groups, &mut depth),
            '$' => depth += 1,
            '&' => end_cell(&groups, &mut depth),
            _ => {}
        }
        if depth > MAX_DEPTH {
            return Some("This formula is too deeply nested to draw");
        }
    }
    None
}

fn close(groups: &mut Vec<(usize, bool)>, depth: &mut usize) {
    *depth = groups.pop().map_or(0, |(depth, _)| depth);
}

/// The next cell of an environment starts again from the environment's level.
fn end_cell(groups: &[(usize, bool)], depth: &mut usize) {
    if let Some(&(start, true)) = groups.last() {
        *depth = start + 1;
    }
}

/// Lays a formula out at `size` units per em. With `width`, a tag goes against
/// the right edge of that many units, as it does in a page's display math.
pub fn svg(
    tex: &str,
    display: bool,
    size: f32,
    color: &str,
    width: Option<f32>,
) -> Result<Formula, String> {
    if let Some(refusal) = refusal(tex) {
        return Err(refusal.to_owned());
    }
    let context = katex::KatexContext::default();
    let layout = katex_native::layout(&context, tex, display).map_err(|err| err.to_string())?;
    let em = f64::from(size);
    let svg = match (width, &layout.tag) {
        (Some(width), Some(_)) => {
            katex_native::svg::to_svg_display(&layout, em, color, f64::from(width) / em)
        }
        _ => katex_native::svg::to_svg(&layout, em, color),
    };
    let depth = layout
        .tag
        .as_ref()
        .map_or(layout.depth, |tag| layout.depth.max(tag.depth));
    Ok(Formula {
        svg,
        depth: (depth * em) as f32,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_could_do_harm_is_refused() {
        for bomb in [
            // Doubles an argument each time: gigabytes within a second.
            r"\def\a#1{\a{#1#1}}\a{x}",
            r"\newcommand{\a}[1]{\a{#1#1}}\a{x}",
            // Nests by expanding, with nothing to see in the text.
            r"\def\a{{\a}}\a",
            r"\gdef\c{\color{red}}\c\c\c\c",
            r"\let\a\def",
        ] {
            assert!(refusal(bomb).is_some(), "{bomb}");
        }
        let nested = |open: &str, close: &str, depth: usize| {
            format!("{}x{}", open.repeat(depth), close.repeat(depth))
        };
        for (open, close) in [
            ("{", "}"),
            (r"\frac{", "}{y}"),
            (r"\left(", r"\right)"),
            (r"\begin{matrix}", r"\end{matrix}"),
            (r"\text{$", "$}"),
            (r"\bgroup", r"\egroup"),
            // Nothing closes these but the end of the group they're in.
            (r"\color{red}", ""),
            (r"\displaystyle ", ""),
            (r"\Huge ", ""),
            (r"\bf ", ""),
        ] {
            assert!(
                refusal(&nested(open, close, 200)).is_some(),
                "{open} {close}"
            );
        }
        assert!(refusal(&"x".repeat(MAX_BYTES + 1)).is_some());
    }

    #[test]
    fn real_formulas_are_not_refused() {
        for tex in [
            r"e^{i\pi}+1=0",
            r"\int_0^1 x^2 \, dx = \frac{1}{3}",
            r"\text{costs \$5 \# items}",
            r"\left(\frac{\partial f}{\partial x}\right)^{2}",
            r"\begin{cases} x & x>0 \\ -x & \text{otherwise} \end{cases}",
            r"\color{red}{x}+\textcolor{blue}{y}",
        ] {
            assert_eq!(refusal(tex), None, "{tex}");
        }
        // Each cell and each group starts over.
        let rows = r"\displaystyle a & \displaystyle b \\".repeat(40);
        let matrix = format!(r"\begin{{aligned}}{rows}\end{{aligned}}");
        assert_eq!(refusal(&matrix), None);
        assert_eq!(refusal(&r"{\color{red} x}".repeat(100)), None);
        assert_eq!(refusal(&r"\frac{a}{b}+".repeat(100)), None);
    }

    #[test]
    fn a_formula_says_how_far_below_the_line_it_goes() {
        let flat = svg("x", false, 20.0, "#000", None).unwrap();
        let fraction = svg(r"\frac{a}{b}", false, 20.0, "#000", None).unwrap();
        assert!(fraction.depth > flat.depth);
        assert!(fraction.svg.contains("<svg"));
    }

    #[test]
    fn bad_and_refused_formulas_say_why() {
        assert!(svg(r"\frac{1", false, 20.0, "#000", None).is_err());
        assert!(svg(r"\def\a{x}\a", false, 20.0, "#000", None).is_err());
    }
}
