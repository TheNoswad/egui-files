//! Raw HTML in a document: the tags we draw and what they do to the text.
//!
//! pulldown-cmark hands over HTML in pieces, one event for each inline tag,
//! with ordinary markdown events between them. So this reads tags, without
//! building a tree, and [`Open`] keeps track of what's open across events.
//! It's only ever drawn, never run, and what isn't known here just doesn't
//! do anything.

use std::collections::BTreeMap;

use html5gum::{DefaultEmitter, HtmlString, Token, Tokenizer};

use super::parse::{Link, Style};

/// The largest size asked of an image, so that a typo can't make a page
/// millions of points tall.
const MAX_LENGTH: f32 = 8192.0;

/// A piece of a stretch of HTML. Comments and doctypes aren't among them.
#[derive(Debug, PartialEq)]
pub enum Piece {
    Open(Tag),
    Close(String),
    Text(String),
}

#[derive(Debug, PartialEq)]
pub struct Tag {
    /// In lower case.
    pub name: String,
    attributes: BTreeMap<String, String>,
}

impl Tag {
    pub fn attribute(&self, name: &str) -> Option<&str> {
        self.attributes.get(name).map(String::as_str)
    }
}

fn text(bytes: &HtmlString) -> String {
    String::from_utf8_lossy(&bytes.0).into_owned()
}

/// What's in some HTML, however broken it is.
pub fn pieces(html: &str) -> Vec<Piece> {
    let mut emitter = DefaultEmitter::default();
    // So that what's in `<script>` and the like is text rather than tags.
    emitter.naively_switch_states(true);
    let mut pieces = Vec::new();
    for Ok(token) in Tokenizer::new_with_emitter(html, emitter) {
        match token {
            Token::StartTag(tag) => pieces.push(Piece::Open(Tag {
                name: text(&tag.name),
                attributes: tag
                    .attributes
                    .iter()
                    .map(|(name, value)| (text(name), text(&value.value)))
                    .collect(),
            })),
            Token::EndTag(tag) => pieces.push(Piece::Close(text(&tag.name))),
            Token::String(string) => pieces.push(Piece::Text(text(&string.value))),
            Token::Comment(_) | Token::Doctype(_) | Token::Error(_) => {}
        }
    }
    pieces
}

/// Whether a tag makes a block of its own, rather than being in a line of text.
pub fn is_block_tag(name: &str) -> bool {
    matches!(
        name,
        "address"
            | "article"
            | "aside"
            | "blockquote"
            | "body"
            | "caption"
            | "center"
            | "col"
            | "colgroup"
            | "dd"
            | "details"
            | "dialog"
            | "dir"
            | "div"
            | "dl"
            | "dt"
            | "fieldset"
            | "figcaption"
            | "figure"
            | "footer"
            | "form"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "head"
            | "header"
            | "hr"
            | "html"
            | "legend"
            | "li"
            | "main"
            | "menu"
            | "nav"
            | "ol"
            | "p"
            | "picture"
            | "pre"
            | "section"
            | "summary"
            | "table"
            | "tbody"
            | "td"
            | "tfoot"
            | "th"
            | "thead"
            | "tr"
            | "ul"
    )
}

/// An image's width or height as asked for in its tag.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Length {
    Points(f32),
    Percent(f32),
}

impl Length {
    /// What's written in a `width` or `height`: `200`, `200px` or `50%`.
    pub fn parse(value: &str) -> Option<Self> {
        let value = value.trim();
        let digits = value
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .unwrap_or(value.len());
        let number = value[..digits].parse::<f32>().ok()?;
        if !number.is_finite() || number <= 0.0 {
            return None;
        }
        Some(if value[digits..].trim_start().starts_with('%') {
            Self::Percent(number.min(MAX_LENGTH))
        } else {
            Self::Points(number.min(MAX_LENGTH))
        })
    }
}

/// What a tag does to the text inside it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Effect {
    Bold,
    Italic,
    Strike,
    Underline,
    Sup,
    Sub,
    Code,
    Mark,
    Link,
    Hidden,
}

const EFFECTS: usize = Effect::Hidden as usize + 1;

fn effect(name: &str) -> Option<Effect> {
    Some(match name {
        "b" | "strong" => Effect::Bold,
        "i" | "em" => Effect::Italic,
        "s" | "del" | "strike" => Effect::Strike,
        "u" | "ins" => Effect::Underline,
        "sup" => Effect::Sup,
        "sub" => Effect::Sub,
        "code" | "kbd" | "samp" | "tt" => Effect::Code,
        "mark" => Effect::Mark,
        "a" => Effect::Link,
        "script" | "style" | "title" | "template" | "iframe" | "object" | "embed" | "noembed"
        | "noframes" | "noscript" | "textarea" | "select" => Effect::Hidden,
        _ => return None,
    })
}

/// The tags open in a line of text, up to the end of its paragraph.
#[derive(Default)]
pub struct Open {
    stack: Vec<Effect>,
    counts: [usize; EFFECTS],
    links: Vec<Link>,
}

impl Open {
    fn on(&self, effect: Effect) -> bool {
        self.counts[effect as usize] > 0
    }

    /// Whether what's here isn't to be shown.
    pub fn hidden(&self) -> bool {
        self.on(Effect::Hidden)
    }

    pub fn link(&self) -> Option<&Link> {
        self.links.last()
    }

    /// The style of text here, given what markdown has made it.
    pub fn apply(&self, style: Style) -> Style {
        Style {
            bold: style.bold || self.on(Effect::Bold),
            italic: style.italic || self.on(Effect::Italic),
            strike: style.strike || self.on(Effect::Strike),
            underline: self.on(Effect::Underline),
            sup: style.sup || self.on(Effect::Sup),
            sub: style.sub || self.on(Effect::Sub),
            code: style.code || self.on(Effect::Code),
            highlight: style.highlight || self.on(Effect::Mark),
            ..style
        }
    }

    pub fn open(&mut self, tag: &Tag) {
        let Some(effect) = effect(&tag.name) else {
            return;
        };
        if effect == Effect::Link {
            // Only a place to link to, without an address.
            let Some(url) = tag.attribute("href") else {
                return;
            };
            self.links.push(Link {
                url: url.to_owned(),
                title: tag.attribute("title").unwrap_or_default().to_owned(),
                wiki: false,
            });
        }
        self.stack.push(effect);
        self.counts[effect as usize] += 1;
    }

    /// Closes the nearest tag of this kind and any left open inside it. One
    /// that nothing opened is ignored, as it is by browsers.
    pub fn close(&mut self, name: &str) {
        let Some(effect) = effect(name) else {
            return;
        };
        if self.counts[effect as usize] == 0 || (self.hidden() && effect != Effect::Hidden) {
            return;
        }
        while let Some(last) = self.stack.pop() {
            self.counts[last as usize] -= 1;
            if last == Effect::Link {
                self.links.pop();
            }
            if last == effect {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tag(html: &str) -> Tag {
        match pieces(html).remove(0) {
            Piece::Open(tag) => tag,
            other => panic!("expected a tag: {other:?}"),
        }
    }

    #[test]
    fn names_are_lower_case_and_values_decoded() {
        let tag = tag("<IMG SRC=a.png alt='x &amp; y'>");
        assert_eq!(tag.name, "img");
        assert_eq!(tag.attribute("src"), Some("a.png"));
        assert_eq!(tag.attribute("alt"), Some("x & y"));
    }

    #[test]
    fn comments_are_nothing_even_unfinished() {
        assert_eq!(pieces("<!-- a <b> -->"), []);
        assert_eq!(pieces("<!-- a"), []);
        assert_eq!(pieces("<!doctype html>"), []);
    }

    #[test]
    fn what_is_in_a_script_is_not_tags() {
        assert_eq!(pieces("<script>a<b</script>").len(), 3);
    }

    #[test]
    fn lengths() {
        assert_eq!(Length::parse("200"), Some(Length::Points(200.0)));
        assert_eq!(Length::parse(" 200px "), Some(Length::Points(200.0)));
        assert_eq!(Length::parse("50%"), Some(Length::Percent(50.0)));
        assert_eq!(Length::parse("1e9"), Some(Length::Points(1.0)));
        assert_eq!(Length::parse("99999999"), Some(Length::Points(MAX_LENGTH)));
        for bad in ["", "auto", "0", "-5", "."] {
            assert_eq!(Length::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn closing_takes_what_was_left_open_inside() {
        let mut open = Open::default();
        open.open(&tag("<b>"));
        open.open(&tag("<i>"));
        assert!(open.apply(Style::default()).italic);
        open.close("b");
        let style = open.apply(Style::default());
        assert!(!style.bold && !style.italic);
    }

    #[test]
    fn stray_closing_tags_do_nothing() {
        let mut open = Open::default();
        open.open(&tag("<b>"));
        open.close("i");
        open.close("span");
        assert!(open.apply(Style::default()).bold);
    }

    #[test]
    fn nothing_closes_a_hidden_stretch_but_its_own_tag() {
        let mut open = Open::default();
        open.open(&tag("<b>"));
        open.open(&tag("<style>"));
        open.close("b");
        assert!(open.hidden());
        open.close("style");
        assert!(!open.hidden());
        assert!(open.apply(Style::default()).bold);
    }

    #[test]
    fn a_link_needs_an_address() {
        let mut open = Open::default();
        open.open(&tag("<a name=here>"));
        assert!(open.link().is_none());
        open.open(&tag("<a href='x.md' title=T>"));
        assert_eq!(open.link().map(|l| l.url.as_str()), Some("x.md"));
        open.close("a");
        assert!(open.link().is_none());
    }
}
