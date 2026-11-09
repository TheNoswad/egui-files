//! A parsed document at a width: everything to draw, with where it goes.
//!
//! The result is a flat list of items in document order, positioned from the
//! top of the document. Drawing it (see `view`) needs no further layout, so a
//! frame only costs what's on screen.

use std::{
    collections::{HashMap, HashSet},
    hash::{Hash, Hasher},
    ops::Range,
    sync::Arc,
};

use egui::{
    Align, Color32, Context, FontFamily, FontId, Galley, Rect, Stroke, Vec2,
    epaint::text::ByteIndex,
    text::{CCursor, LayoutJob, LayoutSection, TextFormat, TextWrapping},
};
use pulldown_cmark::Alignment;

use super::{
    diagram::Colors,
    drawing::{self, svg_color},
    html::Length,
    parse::{
        Block, BlockKind, Callout, CalloutFold, Document, IMAGE_CHAR, InlineImage, Inlines, Link,
        Run, Style, Task,
    },
    wysiwyg::{self, Editable, FieldKind},
};
use crate::editor::highlight::{self, LineSpans};

/// KaTeX's letters look small beside most fonts': its own CSS sets them larger
/// than the text around them too.
const MATH_SCALE: f32 = 1.15;

/// Colors and sizes of the rendered document.
#[derive(Clone, Debug, PartialEq)]
pub struct Theme {
    pub text: Color32,
    pub strong: Color32,
    pub weak: Color32,
    pub link: Color32,
    pub code_fg: Color32,
    pub code_bg: Color32,
    pub inline_code_bg: Color32,
    pub border: Color32,
    pub stripe: Color32,
    pub highlight_bg: Color32,
    pub tag_fg: Color32,
    pub tag_bg: Color32,
    pub dark: bool,
    pub size: f32,
    /// The line width past which text isn't worth reading.
    pub max_width: f32,
}

impl Theme {
    pub fn new(visuals: &egui::Visuals, code_fg: Color32, code_bg: Color32) -> Self {
        let dark = visuals.dark_mode;
        let tint = |color: Color32, alpha: f32| color.gamma_multiply(alpha);
        Self {
            text: visuals.text_color(),
            strong: visuals.strong_text_color(),
            weak: visuals.weak_text_color(),
            link: visuals.hyperlink_color,
            code_fg,
            code_bg,
            inline_code_bg: tint(visuals.text_color(), 0.1),
            border: tint(visuals.text_color(), 0.22),
            stripe: tint(visuals.text_color(), 0.04),
            highlight_bg: if dark {
                Color32::from_rgba_unmultiplied(214, 174, 36, 90)
            } else {
                Color32::from_rgba_unmultiplied(255, 221, 80, 170)
            },
            tag_fg: visuals.hyperlink_color,
            tag_bg: tint(visuals.hyperlink_color, 0.14),
            dark,
            size: 15.0,
            max_width: 800.0,
        }
    }

    fn line_height(&self, size: f32) -> f32 {
        (size * 1.5).round()
    }

    /// Space between blocks.
    fn gap(&self) -> f32 {
        (self.size * 0.8).round()
    }

    /// The color of a callout of this kind, and what it's a kind of.
    pub fn callout_color(&self, kind: &str) -> Color32 {
        let (dark, light) = match callout_family(kind) {
            CalloutFamily::Note => ((88, 166, 255), (9, 105, 218)),
            CalloutFamily::Tip => ((63, 185, 80), (26, 127, 55)),
            CalloutFamily::Important => ((171, 125, 248), (130, 80, 223)),
            CalloutFamily::Warning => ((210, 153, 34), (154, 103, 0)),
            CalloutFamily::Caution => ((248, 81, 73), (207, 34, 46)),
            CalloutFamily::Quote => return self.weak,
        };
        let (r, g, b) = if self.dark { dark } else { light };
        Color32::from_rgb(r, g, b)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CalloutFamily {
    Note,
    Tip,
    Important,
    Warning,
    Caution,
    Quote,
}

/// GitHub's five alerts, and Obsidian's kinds and their aliases.
pub fn callout_family(kind: &str) -> CalloutFamily {
    match kind {
        "tip" | "hint" | "success" | "check" | "done" => CalloutFamily::Tip,
        "important" | "example" | "abstract" | "summary" | "tldr" => CalloutFamily::Important,
        "warning" | "attention" | "question" | "help" | "faq" | "todo" => CalloutFamily::Warning,
        "caution" | "danger" | "error" | "failure" | "fail" | "missing" | "bug" => {
            CalloutFamily::Caution
        }
        "quote" | "cite" => CalloutFamily::Quote,
        _ => CalloutFamily::Note,
    }
}

pub fn callout_icon(kind: &str) -> &'static str {
    use egui_phosphor::regular as icon;
    match kind {
        "tip" | "hint" => icon::LIGHTBULB,
        "success" | "check" | "done" => icon::CHECK_CIRCLE,
        "important" => icon::CHAT_CENTERED_TEXT,
        "example" => icon::LIST_BULLETS,
        "abstract" | "summary" | "tldr" => icon::CLIPBOARD_TEXT,
        "warning" | "attention" => icon::WARNING,
        "question" | "help" | "faq" => icon::QUESTION,
        "todo" => icon::CHECK_SQUARE,
        "caution" | "danger" | "error" => icon::WARNING_OCTAGON,
        "failure" | "fail" | "missing" => icon::X_CIRCLE,
        "bug" => icon::BUG,
        "quote" | "cite" => icon::QUOTES,
        _ => icon::INFO,
    }
}

/// The font families of a rendered document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fonts {
    regular: FontFamily,
    bold: FontFamily,
    italic: FontFamily,
    bold_italic: FontFamily,
}

impl Fonts {
    /// The document's families, or the UI's if they aren't there (as in
    /// tests, which start with egui's own fonts).
    pub fn of(ctx: &Context) -> Self {
        let family = |name: &str| FontFamily::Name(name.into());
        let installed = ctx.fonts(|fonts| {
            fonts
                .definitions()
                .families
                .contains_key(&family(crate::theme::fonts::DOC_REGULAR))
        });
        if installed {
            Self {
                regular: family(crate::theme::fonts::DOC_REGULAR),
                bold: family(crate::theme::fonts::DOC_BOLD),
                italic: family(crate::theme::fonts::DOC_ITALIC),
                bold_italic: family(crate::theme::fonts::DOC_BOLD_ITALIC),
            }
        } else {
            Self {
                regular: FontFamily::Proportional,
                bold: FontFamily::Proportional,
                italic: FontFamily::Proportional,
                bold_italic: FontFamily::Proportional,
            }
        }
    }

    fn family(&self, bold: bool, italic: bool) -> FontFamily {
        match (bold, italic) {
            (false, false) => self.regular.clone(),
            (true, false) => self.bold.clone(),
            (false, true) => self.italic.clone(),
            (true, true) => self.bold_italic.clone(),
        }
    }
}

/// The size of an image, if it can be shown; asked of the image store.
pub trait ImageSizes {
    fn size(&mut self, url: &str) -> Option<Vec2>;
    /// Its size if it's here, without asking for it.
    fn ready_size(&self, _key: &str) -> Option<Vec2> {
        None
    }
    /// How far below the baseline it goes.
    fn depth(&self, _key: &str) -> f32 {
        0.0
    }
    /// Why it can't be shown.
    fn error(&self, _key: &str) -> Option<String> {
        None
    }
}

/// How a formula or diagram stands.
enum Drawn {
    Ready { key: String, size: Vec2, depth: f32 },
    Waiting,
    Failed(String),
}

/// A piece of text: laid out, placed, and able to answer what's under the
/// pointer.
pub struct TextItem {
    pub pos: egui::Pos2,
    pub galley: Arc<Galley>,
    /// By char in the galley.
    pub links: Vec<(Range<usize>, Link)>,
    /// Images over the text's placeholders.
    pub images: Vec<PlacedImage>,
    /// Where its chars came from, when the page is being edited.
    pub editable: Option<Editable>,
}

impl TextItem {
    pub fn rect(&self) -> Rect {
        self.galley.rect.translate(self.pos.to_vec2())
    }

    pub fn link_at(&self, pos: egui::Pos2) -> Option<&Link> {
        let local = pos - self.pos;
        if !self.galley.rect.contains(local.to_pos2()) {
            return None;
        }
        let cursor = self.galley.cursor_from_pos(local);
        // The cursor is between chars; the char it's on is the one whose
        // box has the pointer, so look at the side the pointer is on.
        let at = cursor.index.0;
        let before = at.checked_sub(1).filter(|&c| {
            self.galley.pos_from_cursor(CCursor::new(at)).min.x > local.x
                && self.galley.pos_from_cursor(CCursor::new(c)).min.x <= local.x
        });
        let index = before.unwrap_or(at);
        self.links
            .iter()
            .find(|(range, _)| range.contains(&index))
            .map(|(_, link)| link)
    }
}

pub struct PlacedImage {
    /// Relative to the text.
    pub rect: Rect,
    pub url: String,
    /// Its title, or else its description.
    pub tip: String,
}

/// What a clickable part of the document does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Widget {
    /// The source of a task's `[ ]`.
    Task(Task),
    /// Folds the callout that starts at this source offset.
    Fold(usize),
}

pub enum Item {
    Text(TextItem),
    /// Text that isn't part of the selectable document, like a list's numbers.
    Marker {
        pos: egui::Pos2,
        galley: Arc<Galley>,
    },
    Fill {
        rect: Rect,
        color: Color32,
        radius: f32,
    },
    Stroke {
        rect: Rect,
        stroke: Stroke,
        radius: f32,
    },
    Line {
        from: egui::Pos2,
        to: egui::Pos2,
        stroke: Stroke,
    },
    Dot {
        center: egui::Pos2,
        radius: f32,
        color: Color32,
        filled: bool,
    },
    Square {
        rect: Rect,
        color: Color32,
    },
    Widget {
        rect: Rect,
        widget: Widget,
        on: bool,
    },
    /// A formula or diagram.
    Picture {
        rect: Rect,
        key: String,
    },
    /// A button to see a diagram larger, which shows when it's hovered: a
    /// click on the diagram itself is for double-clicking, to edit it.
    Enlarge {
        rect: Rect,
        block: Rect,
        key: String,
    },
    /// Wider than the page: scrolls sideways on its own.
    Sideways {
        rect: Rect,
        content_width: f32,
        items: Vec<Item>,
    },
    /// The block being edited, as its markdown.
    Editor {
        rect: Rect,
    },
    /// A button to copy a code block, which shows when it's hovered.
    CopyCode {
        rect: Rect,
        block: Rect,
        code: String,
    },
}

impl Item {
    /// Where it's drawn, for skipping what's off screen.
    pub fn rect(&self) -> Rect {
        match self {
            Item::Text(text) => text.rect(),
            Item::Marker { pos, galley } => Rect::from_min_size(*pos, galley.size()),
            Item::Fill { rect, .. }
            | Item::Stroke { rect, .. }
            | Item::Square { rect, .. }
            | Item::Widget { rect, .. }
            | Item::Editor { rect }
            | Item::Picture { rect, .. }
            | Item::Sideways { rect, .. } => *rect,
            Item::CopyCode { block, .. } | Item::Enlarge { block, .. } => *block,
            Item::Line { from, to, .. } => Rect::from_two_pos(*from, *to).expand(1.0),
            Item::Dot { center, radius, .. } => {
                Rect::from_center_size(*center, Vec2::splat(*radius * 2.0))
            }
        }
    }
}

/// What code blocks look like so far: a line's colors, by language and text.
pub type CodeSpans = HashMap<u64, Vec<LineSpans>>;

pub fn code_key(lang: &str, code: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (lang, code).hash(&mut hasher);
    hasher.finish()
}

/// What a layout depends on besides the document. When any of it changes,
/// lay out again.
#[derive(Clone, PartialEq)]
pub struct Inputs {
    pub width: f32,
    pub theme: Theme,
    pub fonts: Fonts,
    pub pixels_per_point: f32,
    pub font_generation: u64,
    /// What's laid out holds places in the font atlas, which egui starts over
    /// when these change, or when it fills up (which this counts).
    pub text_options: egui::epaint::text::TextOptions,
    pub atlas_generation: u64,
    pub image_generation: u64,
    pub code_generation: u64,
    pub fold_generation: u64,
    pub allow_remote: bool,
    /// The source range being edited, and how tall its editor is.
    pub editing: Option<(Range<usize>, f32)>,
    /// Whether the page's text is being edited: chars then know where they
    /// came from.
    pub wysiwyg: bool,
    /// A cursor with no block to be in, on a line of its own there.
    pub gap: Option<usize>,
}

#[derive(Default)]
pub struct Layout {
    pub items: Vec<Item>,
    pub height: f32,
    /// Heading and footnote anchors, to where they are.
    pub anchors: HashMap<String, f32>,
    /// `(source offset, y)` of each block, both increasing.
    pub map: Vec<(usize, f32)>,
    /// Each block on the page as a whole: its source, and where it starts and
    /// ends going down. What double-clicking edits.
    pub blocks: Vec<(Range<usize>, f32, f32)>,
    /// The keys of the formulas and diagrams, in order.
    pub drawings: Vec<String>,
}

impl Layout {
    /// The block at this height, or the next one down from a gap. `None` is
    /// below them all.
    pub fn block_at(&self, y: f32) -> Option<&Range<usize>> {
        self.blocks
            .iter()
            .find(|(_, _, bottom)| y <= *bottom)
            .map(|(source, _, _)| source)
    }

    /// Whether this point is on something that does its own thing when clicked.
    pub fn is_control_at(&self, pos: egui::Pos2) -> bool {
        self.items.iter().any(|item| match item {
            Item::Widget { rect, .. } => rect.contains(pos),
            Item::Text(text) => text.link_at(pos).is_some(),
            _ => false,
        })
    }

    /// Whether this point is on a tick box or a fold.
    pub fn is_widget_at(&self, pos: egui::Pos2) -> bool {
        self.items
            .iter()
            .any(|item| matches!(item, Item::Widget { rect, .. } if rect.contains(pos)))
    }

    /// Whether this point is on a button that shows over a block.
    pub fn is_button_at(&self, pos: egui::Pos2) -> bool {
        self.items.iter().any(|item| match item {
            Item::CopyCode { rect, .. } | Item::Enlarge { rect, .. } => rect.contains(pos),
            _ => false,
        })
    }

    /// The word at this point, and how many times it shows before there in the
    /// same text, to find it again in the source.
    pub fn word_at(&self, pos: egui::Pos2) -> Option<(String, usize)> {
        self.items.iter().find_map(|item| {
            let Item::Text(text) = item else {
                return None;
            };
            if !text.rect().contains(pos) {
                return None;
            }
            let chars: Vec<char> = text.galley.text().chars().collect();
            let at = text.galley.cursor_from_pos(pos - text.pos).index.0;
            let is_word = |c: &char| c.is_alphanumeric() || *c == '_';
            let start = chars[..at.min(chars.len())]
                .iter()
                .rposition(|c| !is_word(c))
                .map_or(0, |i| i + 1);
            let end = chars[start.min(chars.len())..]
                .iter()
                .position(|c| !is_word(c))
                .map_or(chars.len(), |i| start + i);
            let word: String = chars[start..end].iter().collect();
            if word.is_empty() {
                return None;
            }
            let before: String = chars[..start].iter().collect();
            Some((word.clone(), before.matches(word.as_str()).count()))
        })
    }

    /// How far down the document this source offset shows.
    pub fn y_of_source(&self, offset: usize) -> f32 {
        let next = self.map.partition_point(|&(source, _)| source <= offset);
        let Some(&(from_source, from_y)) = next.checked_sub(1).and_then(|i| self.map.get(i)) else {
            return 0.0;
        };
        let (to_source, to_y) = self
            .map
            .get(next)
            .copied()
            .unwrap_or((offset.max(from_source) + 1, self.height));
        let along = (offset - from_source) as f32 / (to_source - from_source).max(1) as f32;
        from_y + (to_y - from_y) * along.clamp(0.0, 1.0)
    }

    /// The source offset showing this far down the document.
    pub fn source_of_y(&self, y: f32) -> usize {
        let next = self.map.partition_point(|&(_, block_y)| block_y <= y);
        let Some(&(from_source, from_y)) = next.checked_sub(1).and_then(|i| self.map.get(i)) else {
            return 0;
        };
        let Some(&(to_source, to_y)) = self.map.get(next) else {
            return from_source;
        };
        let along = (y - from_y) / (to_y - from_y).max(1.0);
        from_source + ((to_source - from_source) as f32 * along.clamp(0.0, 1.0)) as usize
    }
}

/// Text that can be edited, and where it is on the page.
pub struct PlacedField<'a> {
    pub text: &'a TextItem,
    pub field: &'a Editable,
    /// What its position is relative to: a sideways scroller's start.
    pub origin: Vec2,
}

impl PlacedField<'_> {
    /// Its text's rectangle on the page.
    pub fn rect(&self) -> Rect {
        self.text.rect().translate(self.origin)
    }

    /// The char nearest this point on the page.
    pub fn char_at_pos(&self, pos: egui::Pos2) -> usize {
        let local = pos - self.text.pos - self.origin;
        self.text.galley.cursor_from_pos(local).index.0
    }

    /// Where the cursor shows at this source offset, on the page. Spaces
    /// typed after the text don't show, but the cursor should move for them.
    pub fn cursor_rect(&self, source: &str, offset: usize) -> Rect {
        let mut cursor = CCursor::new(self.field.char_at(offset));
        cursor.prefer_next_row = true;
        let mut rect = self.text.galley.pos_from_cursor(cursor);
        if let Some(after) = source.get(self.field.high()..offset) {
            let spaces = after.chars().filter(|c| *c == ' ').count() as f32;
            rect = rect.translate(egui::vec2(spaces * rect.height() * 0.2, 0.0));
        }
        rect.translate(self.text.pos.to_vec2() + self.origin)
    }
}

impl Layout {
    /// What can be edited, in the order it's on the page.
    pub fn placed_fields(&self) -> Vec<PlacedField<'_>> {
        let mut out = Vec::new();
        for item in &self.items {
            match item {
                Item::Text(text) => {
                    if let Some(field) = &text.editable {
                        out.push(PlacedField {
                            text,
                            field,
                            origin: Vec2::ZERO,
                        });
                    }
                }
                Item::Sideways { rect, items, .. } => {
                    for inner in items {
                        if let Item::Text(text) = inner
                            && let Some(field) = &text.editable
                        {
                            out.push(PlacedField {
                                text,
                                field,
                                origin: rect.min.to_vec2(),
                            });
                        }
                    }
                }
                _ => {}
            }
        }
        out
    }

    /// The link the cursor at this offset is in, and where the cursor shows.
    pub fn link_at_offset(&self, source: &str, offset: usize) -> Option<(&Link, Rect)> {
        let fields = self.placed_fields();
        let refs: Vec<&Editable> = fields.iter().map(|placed| placed.field).collect();
        let (index, field) = wysiwyg::field_at(&refs, offset)?;
        let placed = &fields[index];
        let char = field.char_at(offset);
        let in_link = |range: &Range<usize>| {
            range.contains(&char)
                || char
                    .checked_sub(1)
                    .is_some_and(|last| range.contains(&last))
        };
        let (_, link) = placed.text.links.iter().find(|(range, _)| in_link(range))?;
        Some((link, placed.cursor_rect(source, offset)))
    }

    /// The source offset nearest this point on the page.
    pub fn offset_at_pos(&self, pos: egui::Pos2) -> Option<usize> {
        let fields = self.placed_fields();
        let field = nearest_field(&fields, pos)?;
        Some(field.field.offset_at(field.char_at_pos(pos)))
    }

    /// Where the cursor goes from this one, a row up or down, keeping to `x`.
    pub fn offset_vertically(
        &self,
        source: &str,
        offset: usize,
        x: f32,
        down: bool,
    ) -> Option<usize> {
        let fields = self.placed_fields();
        let refs: Vec<&Editable> = fields.iter().map(|placed| placed.field).collect();
        let (index, _) = wysiwyg::field_at(&refs, offset)?;
        let rect = fields[index].cursor_rect(source, offset);
        let target = if down {
            rect.max.y + rect.height() / 2.0
        } else {
            rect.min.y - rect.height() / 2.0
        };
        // Within its own text, or else the nearest in that direction.
        let this = fields[index].rect();
        let field = if this.y_range().contains(target) {
            &fields[index]
        } else {
            let distance = |placed: &PlacedField<'_>| {
                let rect = placed.rect();
                let across = (rect.min.x - x).max(x - rect.max.x).max(0.0);
                let along = if down {
                    rect.min.y - this.max.y
                } else {
                    this.min.y - rect.max.y
                };
                (along.max(0.0), across)
            };
            fields
                .iter()
                .filter(|placed| {
                    let rect = placed.rect();
                    if down {
                        rect.min.y >= this.max.y - 1.0
                    } else {
                        rect.max.y <= this.min.y + 1.0
                    }
                })
                .min_by(|a, b| {
                    distance(a)
                        .partial_cmp(&distance(b))
                        .unwrap_or(std::cmp::Ordering::Equal)
                })?
        };
        // Into the row of a tall text at the edge it was come to.
        let rect = field.rect();
        // A thin row has no room to keep off its edges.
        let y = target.min(rect.max.y - 1.0).max(rect.min.y + 1.0);
        let pos = egui::pos2(x, y);
        Some(field.field.offset_at(field.char_at_pos(pos)))
    }

    /// The start or end of the row this cursor is on.
    pub fn row_edge(&self, offset: usize, end: bool) -> Option<usize> {
        let fields = self.placed_fields();
        let refs: Vec<&Editable> = fields.iter().map(|placed| placed.field).collect();
        let (index, field) = wysiwyg::field_at(&refs, offset)?;
        let galley = &fields[index].text.galley;
        let mut cursor = CCursor::new(field.char_at(offset));
        cursor.prefer_next_row = true;
        let edge = if end {
            galley.cursor_end_of_row(&cursor)
        } else {
            galley.cursor_begin_of_row(&cursor)
        };
        Some(field.offset_at(edge.index.0))
    }

    /// The word around this cursor.
    pub fn word_around(&self, offset: usize) -> Option<Range<usize>> {
        let fields = self.placed_fields();
        let refs: Vec<&Editable> = fields.iter().map(|placed| placed.field).collect();
        let (index, field) = wysiwyg::field_at(&refs, offset)?;
        let chars: Vec<char> = fields[index].text.galley.text().chars().collect();
        let (start, end) = wysiwyg::word_chars(&chars, field.char_at(offset));
        let from = field
            .spans
            .get(start)
            .map_or(field.high(), |span| span.start);
        Some(from..field.offset_at(end))
    }

    /// The cursor one char or word along, going on to the next text at the end.
    pub fn offset_along(&self, offset: usize, back: bool, word: bool) -> Option<usize> {
        let fields = self.placed_fields();
        let refs: Vec<&Editable> = fields.iter().map(|placed| placed.field).collect();
        let (index, field) = wysiwyg::field_at(&refs, offset)?;
        let chars: Vec<char> = fields[index].text.galley.text().chars().collect();
        let at = field.char_at(offset);
        if back {
            if at == 0 {
                return index.checked_sub(1).map(|i| refs[i].high());
            }
            let to = if word {
                wysiwyg::word_back(&chars, at)
            } else {
                at - 1
            };
            return Some(field.spans.get(to).map_or(field.high(), |span| span.start));
        }
        if at >= field.spans.len() {
            return refs.get(index + 1).map(|next| next.low());
        }
        let to = if word {
            wysiwyg::word_forward(&chars, at)
        } else {
            at + 1
        };
        Some(field.offset_at(to))
    }
}

/// The text at this point, or the one nearest going down the page and then across.
fn nearest_field<'a, 'b>(
    fields: &'b [PlacedField<'a>],
    pos: egui::Pos2,
) -> Option<&'b PlacedField<'a>> {
    let distance = |field: &PlacedField<'_>| {
        let rect = field.rect();
        let down = (rect.min.y - pos.y).max(pos.y - rect.max.y).max(0.0);
        let across = (rect.min.x - pos.x).max(pos.x - rect.max.x).max(0.0);
        (down, across)
    };
    fields.iter().min_by(|a, b| {
        distance(a)
            .partial_cmp(&distance(b))
            .unwrap_or(std::cmp::Ordering::Equal)
    })
}

/// A find match in the rendered text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    /// The item it's in, and which item in that for something sideways.
    pub item: (usize, Option<usize>),
    /// By char in the galley.
    pub chars: Range<usize>,
}

impl Layout {
    /// Finds text as it reads on the page, not as it's written.
    pub fn find(&self, query: &str, match_case: bool, whole_word: bool) -> Vec<Hit> {
        let mut hits = Vec::new();
        let mut search = |text: &TextItem, item| {
            for chars in crate::editor::ops::find(text.galley.text(), query, match_case, whole_word)
            {
                hits.push(Hit { item, chars });
            }
        };
        for (index, item) in self.items.iter().enumerate() {
            match item {
                Item::Text(text) => search(text, (index, None)),
                Item::Sideways { items, .. } => {
                    for (inner, item) in items.iter().enumerate() {
                        if let Item::Text(text) = item {
                            search(text, (index, Some(inner)));
                        }
                    }
                }
                _ => {}
            }
        }
        hits
    }

    /// How far down the document a match is.
    pub fn y_of_hit(&self, hit: &Hit) -> f32 {
        let (index, inner) = hit.item;
        let Some(item) = self.items.get(index) else {
            return 0.0;
        };
        let top = item.rect().min.y;
        let text = match (item, inner) {
            (Item::Text(text), None) => Some((text, 0.0)),
            (Item::Sideways { items, .. }, Some(inner)) => match items.get(inner) {
                Some(Item::Text(text)) => Some((text, top)),
                _ => None,
            },
            _ => None,
        };
        text.map_or(top, |(text, base)| {
            let at = CCursor::new(hit.chars.start);
            base + text.pos.y + text.galley.pos_from_cursor(at).min.y
        })
    }
}

/// Each row's part of a range of chars, relative to the galley.
pub fn row_rects(galley: &Galley, chars: &Range<usize>) -> Vec<Rect> {
    let mut start = CCursor::new(chars.start);
    start.prefer_next_row = true;
    let end = CCursor::new(chars.end);
    let (from, to) = (galley.pos_from_cursor(start), galley.pos_from_cursor(end));
    let (first, last) = (
        galley.layout_from_cursor(start).row,
        galley.layout_from_cursor(end).row,
    );
    if first == last {
        return vec![Rect::from_min_max(from.min, to.max)];
    }
    (first..=last)
        .filter_map(|row| {
            let rect = galley.rows.get(row)?.rect();
            let left = if row == first { from.min.x } else { rect.min.x };
            let right = if row == last { to.max.x } else { rect.max.x };
            Some(Rect::from_x_y_ranges(left..=right, rect.y_range()))
        })
        .collect()
}

/// Keeps the laid out paragraphs of the last layout: changing one word of a
/// long document then lays out one paragraph.
#[derive(Default)]
pub struct GalleyCache {
    galleys: HashMap<u64, Arc<Galley>>,
    used: HashSet<u64>,
}

impl GalleyCache {
    fn get(&mut self, ctx: &Context, job: LayoutJob) -> Arc<Galley> {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        job.hash(&mut hasher);
        let key = hasher.finish();
        self.used.insert(key);
        self.galleys
            .entry(key)
            .or_insert_with(|| ctx.fonts_mut(|fonts| fonts.layout_job(job)))
            .clone()
    }

    fn sweep(&mut self) {
        let used = std::mem::take(&mut self.used);
        self.galleys.retain(|key, _| used.contains(key));
    }

    pub fn clear(&mut self) {
        self.galleys.clear();
        self.used.clear();
    }
}

pub struct Params<'a> {
    pub ctx: &'a Context,
    pub inputs: &'a Inputs,
    pub images: &'a mut dyn ImageSizes,
    pub cache: &'a mut GalleyCache,
    pub code_spans: &'a CodeSpans,
    /// Callouts folded the other way than they start out.
    pub toggled: &'a HashSet<usize>,
    /// The last layout's drawings: while the one in its place is being drawn,
    /// as it will be for each key typed, that's what shows.
    pub previous_drawings: &'a [String],
    /// The source, for finding where each char came from when editing.
    pub source: Option<&'a str>,
}

pub fn layout(document: &Document, params: Params<'_>) -> Layout {
    let Params {
        ctx,
        inputs,
        images,
        cache,
        code_spans,
        toggled,
        previous_drawings,
        source,
    } = params;
    let mut builder = Builder {
        ctx,
        theme: &inputs.theme,
        fonts: &inputs.fonts,
        images,
        cache,
        code_spans,
        toggled,
        previous_drawings,
        source: source.filter(|_| inputs.wysiwyg),
        block_source: 0..0,
        field_kind: FieldKind::Inline,
        gap: inputs.gap,
        out: Layout::default(),
        y: 0.0,
        list_depth: 0,
        editing: inputs.editing.clone(),
    };
    let width = inputs.width;
    builder.document_blocks(document, width);
    if !document.footnotes.is_empty() {
        builder.footnotes(document, width);
    }
    let height = builder.y;
    builder.cache.sweep();
    let mut out = builder.out;
    out.height = height;
    // Blocks in order of their source only: nothing here reorders them but
    // footnotes, which go at the end wherever they're written.
    let mut last = 0;
    out.map.retain(|&(source, _)| {
        let keep = source >= last;
        if keep {
            last = source;
        }
        keep
    });
    out
}

struct Builder<'a> {
    ctx: &'a Context,
    theme: &'a Theme,
    fonts: &'a Fonts,
    images: &'a mut dyn ImageSizes,
    cache: &'a mut GalleyCache,
    code_spans: &'a CodeSpans,
    toggled: &'a HashSet<usize>,
    previous_drawings: &'a [String],
    source: Option<&'a str>,
    /// Where what's being laid out is in the source.
    block_source: Range<usize>,
    field_kind: FieldKind,
    gap: Option<usize>,
    out: Layout,
    y: f32,
    /// How many lists this is inside of, which varies their bullets.
    list_depth: usize,
    editing: Option<(Range<usize>, f32)>,
}

fn overlaps(a: &Range<usize>, b: &Range<usize>) -> bool {
    a.start < b.end && b.start < a.end
}

/// Inline text laid out: its galley, its links by char, its images, and when
/// the page is being edited, where its chars came from.
type LaidOut = (
    Arc<Galley>,
    Vec<(Range<usize>, Link)>,
    Vec<PlacedImage>,
    Option<Editable>,
);

/// How to set a piece of inline text.
#[derive(Clone, Copy)]
struct TextStyle {
    size: f32,
    color: Color32,
    bold: bool,
    italic: bool,
    halign: Align,
}

impl Builder<'_> {
    fn body(&self) -> TextStyle {
        TextStyle {
            size: self.theme.size,
            color: self.theme.text,
            bold: false,
            italic: false,
            halign: Align::LEFT,
        }
    }

    /// The document's own blocks, with the one being edited as its editor:
    /// in its place, or for a new one after what comes before it.
    fn document_blocks(&mut self, document: &Document, width: f32) {
        let editing = self.editing.clone();
        let in_footnote = editing.as_ref().is_some_and(|(range, _)| {
            document
                .footnotes
                .iter()
                .any(|f| overlaps(&f.source, range))
        });
        let mut placed = in_footnote || editing.is_none();
        let mut first = true;
        for block in &document.blocks {
            if self.gap.is_some_and(|gap| block.source.start >= gap) {
                self.gap_line(&mut first, width);
            }
            let replaced = editing
                .as_ref()
                .is_some_and(|(range, _)| overlaps(&block.source, range));
            let comes_after = editing
                .as_ref()
                .is_some_and(|(range, _)| block.source.start >= range.start);
            if !placed
                && (replaced || comes_after)
                && let Some((range, height)) = &editing
            {
                if !first {
                    self.y += self.theme.gap();
                }
                first = false;
                placed = true;
                self.editor(range, *height, width);
            }
            if replaced {
                continue;
            }
            if !first {
                self.y += self.theme.gap();
            }
            first = false;
            let top = self.y;
            self.block(block, 0.0, width);
            self.out.blocks.push((block.source.clone(), top, self.y));
        }
        if !placed && let Some((range, height)) = &editing {
            if !first {
                self.y += self.theme.gap();
            }
            self.editor(range, *height, width);
        }
        self.gap_line(&mut first, width);
    }

    /// A quote or list item with nothing in it, while editing, has a line for
    /// the cursor: it would be nowhere, and the keys would go to its neighbor.
    fn empty_line(&mut self, children: &[Block], x: f32, width: f32) {
        if children.is_empty() && self.source.is_some() {
            let style = self.body();
            self.text(&Inlines::default(), x, width, style);
        }
    }

    /// An empty line for a cursor that has no block to be in.
    fn gap_line(&mut self, first: &mut bool, width: f32) {
        let Some(gap) = self.gap.take() else {
            return;
        };
        if !*first {
            self.y += self.theme.gap();
        }
        *first = false;
        self.block_source = gap..gap;
        let style = self.body();
        let top = self.y;
        self.text(&Inlines::default(), 0.0, width, style);
        self.out.map.push((gap, top));
    }

    fn editor(&mut self, range: &Range<usize>, height: f32, width: f32) {
        let top = self.y;
        self.out.map.push((range.start, top));
        self.out.items.push(Item::Editor {
            rect: Rect::from_min_size(egui::pos2(0.0, top), egui::vec2(width, height)),
        });
        self.y += height;
        self.out.blocks.push((range.clone(), top, self.y));
    }

    fn blocks(&mut self, blocks: &[Block], x: f32, width: f32) {
        for (index, block) in blocks.iter().enumerate() {
            if index > 0 {
                self.y += self.theme.gap();
            }
            self.block(block, x, width);
        }
    }

    fn block(&mut self, block: &Block, x: f32, width: f32) {
        self.block_source = block.source.clone();
        self.out.map.push((block.source.start, self.y));
        match &block.kind {
            BlockKind::Paragraph(inlines) => {
                let style = self.body();
                self.text(inlines, x, width, style);
            }
            BlockKind::HtmlText(inlines) => {
                // One unit on the page, without a cursor in it.
                let source = self.source.take();
                let style = self.body();
                self.text(inlines, x, width, style);
                self.source = source;
            }
            BlockKind::Heading { level, slug, text } => self.heading(*level, slug, text, x, width),
            BlockKind::Code { lang, code } if lang.as_deref() == Some("mermaid") => {
                self.diagram(code, x, width);
            }
            BlockKind::Code { lang, code } => self.code(lang.as_deref(), code, x, width),
            BlockKind::Quote { callout, children } => match callout {
                Some(callout) => self.callout(callout, children, block.source.start, x, width),
                None => self.quote(children, x, width),
            },
            BlockKind::List { start, items } => self.list(*start, items, x, width),
            BlockKind::Definitions(definitions) => {
                let indent = self.theme.size * 1.6;
                for (index, definition) in definitions.iter().enumerate() {
                    if index > 0 {
                        self.y += self.theme.gap();
                    }
                    let style = TextStyle {
                        bold: true,
                        color: self.theme.strong,
                        ..self.body()
                    };
                    self.text(&definition.term, x, width, style);
                    for detail in &definition.details {
                        self.y += self.theme.gap() * 0.3;
                        self.blocks(detail, x + indent, width - indent);
                    }
                }
            }
            BlockKind::Table { aligns, head, rows } => self.table(aligns, head, rows, x, width),
            BlockKind::Rule => {
                let y = self.y + self.theme.gap() * 0.5;
                self.out.items.push(Item::Line {
                    from: egui::pos2(x, y),
                    to: egui::pos2(x + width, y),
                    stroke: Stroke::new(1.5, self.theme.border),
                });
                self.y = y + self.theme.gap() * 0.5;
            }
            BlockKind::Frontmatter(fields) => self.frontmatter(fields, x, width),
            BlockKind::Html(html) => self.raw(html.trim_end(), x, width),
            BlockKind::Math(math) => self.display_math(math.trim(), x, width),
        }
    }

    fn heading(&mut self, level: u8, slug: &str, text: &Inlines, x: f32, width: f32) {
        let scale = match level {
            1 => 2.0,
            2 => 1.55,
            3 => 1.3,
            4 => 1.12,
            5 => 1.0,
            _ => 0.92,
        };
        let size = (self.theme.size * scale).round();
        // A heading belongs with what follows it, not what's before it.
        if self.y > 0.0 {
            self.y += self.theme.gap() * if level <= 2 { 1.1 } else { 0.7 };
        }
        self.out.anchors.insert(slug.to_owned(), self.y);
        let style = TextStyle {
            size,
            bold: true,
            color: if level >= 6 {
                self.theme.weak
            } else {
                self.theme.strong
            },
            ..self.body()
        };
        self.text(text, x, width, style);
        if level <= 2 {
            self.y += 4.0;
            self.out.items.push(Item::Line {
                from: egui::pos2(x, self.y),
                to: egui::pos2(x + width, self.y),
                stroke: Stroke::new(1.0, self.theme.border),
            });
            self.y += 2.0;
        }
    }

    /// Where a drawing stands, asking for it if it's new.
    fn drawn(&mut self, key: String) -> Drawn {
        let index = self.out.drawings.len();
        self.out.drawings.push(key.clone());
        if let Some(size) = self.images.size(&key) {
            let depth = self.images.depth(&key);
            return Drawn::Ready { key, size, depth };
        }
        if let Some(err) = self.images.error(&key) {
            return Drawn::Failed(err);
        }
        let previous = self.previous_drawings.get(index);
        match previous.and_then(|key| Some((key, self.images.ready_size(key)?))) {
            Some((key, size)) => Drawn::Ready {
                key: key.clone(),
                size,
                depth: self.images.depth(key),
            },
            None => Drawn::Waiting,
        }
    }

    /// Why something couldn't be drawn, under its source.
    fn drawing_error(&mut self, err: &str, x: f32, width: f32) {
        let size = (self.theme.size * 0.85).round();
        self.y += self.theme.gap() * 0.3;
        let job = plain_job(
            self.ctx,
            err,
            FontId::proportional(size),
            self.theme.callout_color("caution"),
            width,
            self.theme.line_height(size),
        );
        let galley = self.cache.get(self.ctx, job);
        let height = galley.size().y;
        self.out.items.push(Item::Marker {
            pos: egui::pos2(x, self.y),
            galley,
        });
        self.y += height;
    }

    fn display_math(&mut self, tex: &str, x: f32, width: f32) {
        let key = drawing::math_key(
            tex,
            true,
            (self.theme.size * MATH_SCALE).round(),
            self.theme.text,
            self.ctx.pixels_per_point(),
            Some(width),
        );
        match self.drawn(key) {
            Drawn::Ready { key, size, .. } => {
                let top = self.y;
                if size.x > width {
                    let rect = Rect::from_min_size(egui::pos2(x, top), egui::vec2(width, size.y));
                    let picture = Rect::from_min_size(egui::Pos2::ZERO, size);
                    self.out.items.push(Item::Sideways {
                        rect,
                        content_width: size.x,
                        items: vec![Item::Picture { rect: picture, key }],
                    });
                } else {
                    let left = x + (width - size.x) / 2.0;
                    self.out.items.push(Item::Picture {
                        rect: Rect::from_min_size(egui::pos2(left, top), size),
                        key,
                    });
                }
                self.y += size.y;
            }
            Drawn::Waiting => self.raw(tex, x, width),
            Drawn::Failed(err) => {
                self.raw(tex, x, width);
                self.drawing_error(&err, x, width);
            }
        }
    }

    fn diagram(&mut self, code: &str, x: f32, width: f32) {
        let colors = Colors {
            dark: self.theme.dark,
            background: svg_color(self.theme.code_bg),
            text: svg_color(self.theme.code_fg),
            line: svg_color(self.theme.weak),
        };
        match self.drawn(drawing::diagram_key(code, &colors)) {
            Drawn::Ready { key, size, .. } => {
                let padding = self.theme.size * 0.75;
                let size = fit(size, width - padding * 2.0);
                let block = Rect::from_min_size(
                    egui::pos2(x, self.y),
                    egui::vec2(width, size.y + padding * 2.0),
                );
                self.out.items.push(Item::Fill {
                    rect: block,
                    color: self.theme.code_bg,
                    radius: 4.0,
                });
                let left = x + (width - size.x) / 2.0;
                self.out.items.push(Item::Picture {
                    rect: Rect::from_min_size(egui::pos2(left, self.y + padding), size),
                    key: key.clone(),
                });
                let button = (self.theme.size * 0.88).round() * 1.6;
                let inset = self.theme.size * 0.35;
                let corner = egui::pos2(block.max.x - button - inset, block.min.y + inset);
                self.out.items.push(Item::Enlarge {
                    rect: Rect::from_min_size(corner, Vec2::splat(button)),
                    block,
                    key,
                });
                // To the left of the first, as code blocks have it alone.
                let copy = corner - egui::vec2(button + inset * 0.5, 0.0);
                self.out.items.push(Item::CopyCode {
                    rect: Rect::from_min_size(copy, Vec2::splat(button)),
                    block,
                    code: code.strip_suffix('\n').unwrap_or(code).to_owned(),
                });
                self.y += block.height();
            }
            Drawn::Waiting => self.code(Some("mermaid"), code, x, width),
            Drawn::Failed(err) => {
                self.code(Some("mermaid"), code, x, width);
                self.drawing_error(&err, x, width);
            }
        }
    }

    fn raw(&mut self, text: &str, x: f32, width: f32) {
        let size = (self.theme.size * 0.88).round();
        let job = plain_job(
            self.ctx,
            text,
            FontId::monospace(size),
            self.theme.weak,
            width,
            self.theme.line_height(size),
        );
        let galley = self.cache.get(self.ctx, job);
        let height = galley.size().y;
        self.out.items.push(Item::Text(TextItem {
            pos: egui::pos2(x, self.y),
            galley,
            links: Vec::new(),
            images: Vec::new(),
            editable: None,
        }));
        self.y += height;
    }

    fn quote(&mut self, children: &[Block], x: f32, width: f32) {
        let top = self.y;
        let bar = self.out.items.len();
        self.out.items.push(Item::Fill {
            rect: Rect::NOTHING,
            color: self.theme.border,
            radius: 1.5,
        });
        let indent = self.theme.size * 0.95;
        self.empty_line(children, x + indent, width - indent);
        self.blocks(children, x + indent, width - indent);
        self.out.items[bar] = Item::Fill {
            rect: Rect::from_min_max(egui::pos2(x, top), egui::pos2(x + 3.0, self.y)),
            color: self.theme.border,
            radius: 1.5,
        };
    }

    fn callout(
        &mut self,
        callout: &Callout,
        children: &[Block],
        source: usize,
        x: f32,
        width: f32,
    ) {
        let top = self.y;
        let color = self.theme.callout_color(&callout.kind);
        let background = self.out.items.len();
        // Filled in below, once the height is known.
        self.out.items.push(Item::Fill {
            rect: Rect::NOTHING,
            color: Color32::TRANSPARENT,
            radius: 4.0,
        });
        self.out.items.push(Item::Fill {
            rect: Rect::NOTHING,
            color,
            radius: 1.5,
        });
        let padding = self.theme.size * 0.7;
        let inner_x = x + padding + 3.0;
        let inner_width = width - padding * 2.0 - 3.0;
        self.y += padding * 0.6;

        let collapsed = match callout.fold {
            CalloutFold::None => false,
            CalloutFold::Open => self.toggled.contains(&source),
            CalloutFold::Closed => !self.toggled.contains(&source),
        };
        let size = self.theme.size;
        let line = self.theme.line_height(size);
        let foldable = callout.fold != CalloutFold::None;
        let icon = self.cache.get(
            self.ctx,
            plain_job(
                self.ctx,
                callout_icon(&callout.kind),
                FontId::proportional(size + 1.0),
                color,
                f32::INFINITY,
                line,
            ),
        );
        let icon_width = icon.size().x + size * 0.4;
        self.out.items.push(Item::Marker {
            pos: egui::pos2(inner_x, self.y),
            galley: icon,
        });
        let title_style = TextStyle {
            bold: true,
            color,
            ..self.body()
        };
        let title_job = self.simple_job(&callout.title, title_style, inner_width - icon_width);
        let title = self.cache.get(self.ctx, title_job);
        let title_height = title.size().y.max(line);
        self.out.items.push(Item::Text(TextItem {
            pos: egui::pos2(inner_x + icon_width, self.y),
            galley: title,
            links: Vec::new(),
            images: Vec::new(),
            editable: None,
        }));
        if foldable {
            self.out.items.push(Item::Widget {
                rect: Rect::from_min_size(
                    egui::pos2(x, self.y - padding * 0.3),
                    egui::vec2(width, title_height + padding * 0.6),
                ),
                widget: Widget::Fold(source),
                on: !collapsed,
            });
        }
        self.y += title_height;
        if !collapsed && !children.is_empty() {
            self.y += self.theme.gap() * 0.4;
            self.blocks(children, inner_x, inner_width);
        }
        self.y += padding * 0.6;
        self.out.items[background] = Item::Fill {
            rect: Rect::from_min_max(egui::pos2(x, top), egui::pos2(x + width, self.y)),
            color: color.gamma_multiply(0.1),
            radius: 4.0,
        };
        self.out.items[background + 1] = Item::Fill {
            rect: Rect::from_min_max(egui::pos2(x, top), egui::pos2(x + 3.0, self.y)),
            color,
            radius: 1.5,
        };
    }

    fn list(&mut self, start: Option<u64>, items: &[super::parse::ListItem], x: f32, width: f32) {
        let depth = self.list_depth;
        self.list_depth += 1;
        let size = self.theme.size;
        let line = self.theme.line_height(size);
        // Wide enough for the longest number.
        let gutter = match start {
            Some(start) => {
                let last = start.saturating_add(items.len() as u64);
                let digits = last.to_string().len() as f32;
                (size * (0.65 * digits + 1.0)).max(size * 1.6)
            }
            None => size * 1.4,
        };
        for (index, item) in items.iter().enumerate() {
            if index > 0 {
                self.y += self.theme.gap() * 0.35;
            }
            self.out.map.push((item.source.start, self.y));
            let top = self.y;
            match (&item.task, start) {
                (Some(task), _) => {
                    let side = size;
                    let rect = Rect::from_min_size(
                        egui::pos2(x + gutter - side - size * 0.35, top + (line - side) / 2.0),
                        Vec2::splat(side),
                    );
                    self.out.items.push(Item::Widget {
                        rect,
                        widget: Widget::Task(task.clone()),
                        on: task.checked,
                    });
                }
                (None, Some(start)) => {
                    let number = start.saturating_add(index as u64);
                    let job = plain_job(
                        self.ctx,
                        &format!("{number}."),
                        FontId::new(size, self.fonts.regular.clone()),
                        self.theme.weak,
                        f32::INFINITY,
                        line,
                    );
                    let galley = self.cache.get(self.ctx, job);
                    let pos = egui::pos2(x + gutter - galley.size().x - size * 0.35, top);
                    self.out.items.push(Item::Marker { pos, galley });
                }
                (None, None) => {
                    let center = egui::pos2(x + gutter - size * 0.75, top + line / 2.0);
                    let color = self.theme.weak;
                    self.out.items.push(match depth % 3 {
                        0 => Item::Dot {
                            center,
                            radius: 2.6,
                            color,
                            filled: true,
                        },
                        1 => Item::Dot {
                            center,
                            radius: 2.6,
                            color,
                            filled: false,
                        },
                        _ => Item::Square {
                            rect: Rect::from_center_size(center, Vec2::splat(4.6)),
                            color,
                        },
                    });
                }
            }
            self.block_source = item.source.clone();
            self.empty_line(&item.children, x + gutter, width - gutter);
            for (child, block) in item.children.iter().enumerate() {
                if child > 0 {
                    // A list in an item is part of it, not a new thought.
                    let nested = matches!(block.kind, BlockKind::List { .. });
                    self.y += self.theme.gap() * if nested { 0.35 } else { 1.0 };
                }
                self.block(block, x + gutter, width - gutter);
            }
            // An empty item still takes a line.
            if self.y < top + line {
                self.y = top + line;
            }
        }
        self.list_depth = depth;
    }

    fn code(&mut self, lang: Option<&str>, code: &str, x: f32, width: f32) {
        let size = (self.theme.size * 0.88).round();
        let line = self.theme.line_height(size);
        let padding = self.theme.size * 0.75;
        let code = code.strip_suffix('\n').unwrap_or(code);
        let spans = lang
            .and_then(|lang| self.code_spans.get(&code_key(lang, code)))
            .map_or(&[][..], Vec::as_slice);
        let font = FontId::monospace(size);
        let mut job = highlight::layout_job(code, spans, font, self.theme.code_fg, f32::INFINITY);
        set_line_height(self.ctx, &mut job, line);
        let galley = self.cache.get(self.ctx, job);
        let text_size = galley.size();
        let height = text_size.y + padding * 2.0;
        let rect = Rect::from_min_size(egui::pos2(x, self.y), egui::vec2(width, height));
        self.out.items.push(Item::Fill {
            rect,
            color: self.theme.code_bg,
            radius: 4.0,
        });
        let content_width = text_size.x + padding * 2.0;
        let editable = self
            .source
            .map(|source| code_field(source, &self.block_source, code));
        let text = Item::Text(TextItem {
            pos: egui::pos2(padding, padding),
            galley,
            links: Vec::new(),
            images: Vec::new(),
            editable,
        });
        if content_width > width {
            self.out.items.push(Item::Sideways {
                rect,
                content_width,
                items: vec![text],
            });
        } else if let Item::Text(mut text) = text {
            text.pos += rect.min.to_vec2();
            self.out.items.push(Item::Text(text));
        }
        let button = size * 1.6;
        let inset = size * 0.4;
        self.out.items.push(Item::CopyCode {
            rect: Rect::from_min_size(
                egui::pos2(rect.max.x - button - inset, rect.min.y + inset),
                Vec2::splat(button),
            ),
            block: rect,
            code: code.to_owned(),
        });
        self.y += height;
    }

    fn table(
        &mut self,
        aligns: &[Alignment],
        head: &[Inlines],
        rows: &[Vec<Inlines>],
        x: f32,
        width: f32,
    ) {
        let columns = head
            .len()
            .max(rows.iter().map(Vec::len).max().unwrap_or(0))
            .max(1);
        let padding = self.theme.size * 0.6;
        let head_style = TextStyle {
            bold: true,
            color: self.theme.strong,
            ..self.body()
        };
        let body_style = self.body();

        // Natural widths, then shrink what's wide until it fits or nothing
        // more can give. A cell is never narrower than a few characters.
        let mut natural = vec![0.0f32; columns];
        for (index, cell) in head.iter().enumerate() {
            natural[index] = natural[index].max(self.text_width(cell, head_style));
        }
        for row in rows {
            for (index, cell) in row.iter().enumerate() {
                natural[index] = natural[index].max(self.text_width(cell, body_style));
            }
        }
        let widths = column_widths(
            &natural,
            width - padding * 2.0 * columns as f32,
            self.theme.size * 5.0,
        );
        let content_width: f32 = widths.iter().sum::<f32>() + padding * 2.0 * columns as f32;

        let origin_y = self.y;
        let saved_y = std::mem::replace(&mut self.y, 0.0);
        let mut items = Vec::new();
        let mut row_y = 0.0;
        let mut row_index = 0usize;
        let head_row = (!head.is_empty()).then_some(head);
        let all_rows = head_row
            .into_iter()
            .map(|row| (row, true))
            .chain(rows.iter().map(|row| (row.as_slice(), false)));
        for (row, is_head) in all_rows {
            let style = if is_head { head_style } else { body_style };
            let mut cells = Vec::new();
            let mut height = self.theme.line_height(self.theme.size);
            let mut cell_x = padding;
            for (index, width) in widths.iter().enumerate() {
                if let Some(cell) = row.get(index) {
                    let halign = match aligns.get(index) {
                        Some(Alignment::Center) => Align::Center,
                        Some(Alignment::Right) => Align::RIGHT,
                        _ => Align::LEFT,
                    };
                    let style = TextStyle { halign, ..style };
                    self.field_kind = FieldKind::Cell;
                    let (galley, links, images, editable) = self.inline_job(cell, *width, style);
                    self.field_kind = FieldKind::Inline;
                    // An empty cell has no place in the source to go by.
                    let editable = editable.filter(|field| !field.spans.is_empty());
                    height = height.max(galley.size().y);
                    // A galley aligned to the center or right is drawn around
                    // its position, which is why it's put in the cell's middle.
                    let cell_pos = match halign {
                        Align::Center => cell_x + width / 2.0,
                        Align::Max => cell_x + width,
                        Align::Min => cell_x,
                    };
                    cells.push(TextItem {
                        pos: egui::pos2(cell_pos, 0.0),
                        galley,
                        links,
                        images,
                        editable,
                    });
                }
                cell_x += width + padding * 2.0;
            }
            let row_height = height + padding;
            if is_head {
                items.push(Item::Fill {
                    rect: Rect::from_min_size(
                        egui::pos2(0.0, row_y),
                        egui::vec2(content_width, row_height),
                    ),
                    color: self.theme.stripe,
                    radius: 0.0,
                });
            } else {
                if row_index % 2 == 1 {
                    items.push(Item::Fill {
                        rect: Rect::from_min_size(
                            egui::pos2(0.0, row_y),
                            egui::vec2(content_width, row_height),
                        ),
                        color: self.theme.stripe,
                        radius: 0.0,
                    });
                }
                row_index += 1;
            }
            for mut cell in cells {
                cell.pos.y = row_y + padding / 2.0;
                items.push(Item::Text(cell));
            }
            row_y += row_height;
            items.push(Item::Line {
                from: egui::pos2(0.0, row_y),
                to: egui::pos2(content_width, row_y),
                stroke: Stroke::new(1.0, self.theme.border),
            });
        }
        // Column rules and the outline.
        let mut rule_x = 0.0;
        for width in &widths[..columns - 1] {
            rule_x += width + padding * 2.0;
            items.push(Item::Line {
                from: egui::pos2(rule_x, 0.0),
                to: egui::pos2(rule_x, row_y),
                stroke: Stroke::new(1.0, self.theme.border),
            });
        }
        items.push(Item::Stroke {
            rect: Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(content_width, row_y)),
            stroke: Stroke::new(1.0, self.theme.border),
            radius: 2.0,
        });
        self.y = saved_y;
        let rect = Rect::from_min_size(
            egui::pos2(x, origin_y),
            egui::vec2(width.min(content_width).max(1.0), row_y),
        );
        if content_width > width {
            self.out.items.push(Item::Sideways {
                rect,
                content_width,
                items,
            });
        } else {
            for mut item in items {
                offset(&mut item, rect.min.to_vec2());
                self.out.items.push(item);
            }
        }
        self.y += row_y;
    }

    fn frontmatter(&mut self, fields: &[(String, String)], x: f32, width: f32) {
        if fields.is_empty() {
            return;
        }
        let padding = self.theme.size * 0.6;
        let size = (self.theme.size * 0.88).round();
        let top = self.y;
        let background = self.out.items.len();
        self.out.items.push(Item::Fill {
            rect: Rect::NOTHING,
            color: self.theme.stripe,
            radius: 4.0,
        });
        self.y += padding;
        let key_style = TextStyle {
            size,
            bold: true,
            color: self.theme.weak,
            ..self.body()
        };
        let value_style = TextStyle {
            size,
            ..self.body()
        };
        let key_width = fields
            .iter()
            .map(|(key, _)| {
                let job = self.simple_job(key, key_style, f32::INFINITY);
                self.cache.get(self.ctx, job).size().x
            })
            .fold(0.0, f32::max)
            .min(width * 0.4);
        let gap = size;
        for (key, value) in fields {
            let job = self.simple_job(key, key_style, key_width);
            let key = self.cache.get(self.ctx, job);
            let job = self.simple_job(value, value_style, width - key_width - gap - padding * 2.0);
            let value = self.cache.get(self.ctx, job);
            let height = key.size().y.max(value.size().y);
            self.out.items.push(Item::Text(TextItem {
                pos: egui::pos2(x + padding, self.y),
                galley: key,
                links: Vec::new(),
                images: Vec::new(),
                editable: None,
            }));
            self.out.items.push(Item::Text(TextItem {
                pos: egui::pos2(x + padding + key_width + gap, self.y),
                galley: value,
                links: Vec::new(),
                images: Vec::new(),
                editable: None,
            }));
            self.y += height;
        }
        self.y += padding;
        self.out.items[background] = Item::Fill {
            rect: Rect::from_min_max(egui::pos2(x, top), egui::pos2(x + width, self.y)),
            color: self.theme.stripe,
            radius: 4.0,
        };
    }

    fn footnotes(&mut self, document: &Document, width: f32) {
        self.y += self.theme.gap() * 1.5;
        self.out.items.push(Item::Line {
            from: egui::pos2(0.0, self.y),
            to: egui::pos2(width, self.y),
            stroke: Stroke::new(1.0, self.theme.border),
        });
        self.y += self.theme.gap();
        let size = self.theme.size;
        let gutter = size * 1.8;
        for (index, footnote) in document.footnotes.iter().enumerate() {
            if index > 0 {
                self.y += self.theme.gap() * 0.5;
            }
            if let Some((range, height)) = self.editing.clone()
                && overlaps(&footnote.source, &range)
            {
                self.editor(&range, height, width);
                continue;
            }
            let top = self.y;
            self.out
                .anchors
                .insert(format!("fn:{}", footnote.label), self.y);
            let job = plain_job(
                self.ctx,
                &format!("{}.", footnote.number),
                FontId::new(size, self.fonts.regular.clone()),
                self.theme.weak,
                f32::INFINITY,
                self.theme.line_height(size),
            );
            let galley = self.cache.get(self.ctx, job);
            let pos = egui::pos2(gutter - galley.size().x - size * 0.35, self.y);
            self.out.items.push(Item::Marker { pos, galley });
            let back = format!("fnref:{}", footnote.label);
            if self.out.anchors.contains_key(&back) {
                let job = plain_job(
                    self.ctx,
                    "\u{21A9}",
                    FontId::new(size, self.fonts.regular.clone()),
                    self.theme.link,
                    f32::INFINITY,
                    self.theme.line_height(size),
                );
                let galley = self.cache.get(self.ctx, job);
                let chars = galley.text().chars().count();
                let pos = egui::pos2(width - galley.size().x, self.y);
                self.out.items.push(Item::Text(TextItem {
                    pos,
                    galley,
                    links: vec![(
                        0..chars,
                        Link {
                            url: format!("#{back}"),
                            title: "Back to where it's referred to".to_owned(),
                            wiki: false,
                        },
                    )],
                    images: Vec::new(),
                    editable: None,
                }));
            }
            self.blocks(&footnote.blocks, gutter, width - gutter - size * 1.5);
            self.out.blocks.push((footnote.source.clone(), top, self.y));
        }
    }

    fn text(&mut self, inlines: &Inlines, x: f32, width: f32, style: TextStyle) {
        let (galley, links, images, editable) = self.inline_job(inlines, width, style);
        // A formula can reach below its row, as it does below its baseline.
        let height = images.iter().fold(galley.size().y, |height, image| {
            height.max(image.rect.max.y)
        });
        for name in &inlines.anchors {
            self.out.anchors.entry(name.clone()).or_insert(self.y);
        }
        for label in &inlines.footnote_refs {
            self.out
                .anchors
                .entry(format!("fnref:{label}"))
                .or_insert(self.y);
        }
        self.out.items.push(Item::Text(TextItem {
            pos: egui::pos2(x, self.y),
            galley,
            links,
            images,
            editable,
        }));
        self.y += height;
    }

    /// A font's distance from top to baseline, and its height.
    fn metrics(&self, font: &FontId) -> (f32, f32) {
        let pixels_per_point = self.ctx.pixels_per_point();
        self.ctx.fonts_mut(|fonts| {
            let metrics = fonts.fonts.font(&font.family).styled_metrics(
                pixels_per_point,
                font.size,
                &egui::epaint::text::VariationCoords::default(),
            );
            (metrics.ascent, metrics.row_height)
        })
    }

    /// How far below a line's baseline its row goes.
    fn below_baseline(&self, style: TextStyle) -> f32 {
        let line = self.theme.line_height(style.size);
        let base = FontId::new(style.size, self.fonts.regular.clone());
        let (ascent, height) = self.metrics(&base);
        line - (ascent + (line - height) / 2.0)
    }

    /// The height that puts this font's baseline where the line's is, or
    /// `shift` below it. egui puts a font's baseline at its ascent plus the
    /// row's height less this one, so fonts and sizes can share a baseline.
    fn aligned_height(&self, font: &FontId, style: TextStyle, shift: f32) -> f32 {
        let line = self.theme.line_height(style.size);
        let base = FontId::new(style.size, self.fonts.regular.clone());
        let (base_ascent, base_height) = self.metrics(&base);
        let baseline = base_ascent + (line - base_height) / 2.0;
        let (ascent, _) = self.metrics(font);
        (line + ascent - baseline - shift).clamp(1.0, line)
    }

    fn format(&self, style: TextStyle, run_style: Style) -> TextFormat {
        let mut size = style.size;
        let bold = style.bold || run_style.bold;
        let italic = style.italic || run_style.italic;
        let script = run_style.sup || run_style.sub || run_style.footnote_ref;
        if script {
            size = (size * 0.72).round();
        }
        let mut font = FontId::new(size, self.fonts.family(bold, italic));
        let mut color = style.color;
        let (mut background, mut expand_bg) = (Color32::TRANSPARENT, 0.0);
        if run_style.code || run_style.math {
            font = FontId::monospace((size * 0.9).round());
            background = self.theme.inline_code_bg;
            expand_bg = 1.0;
        }
        if run_style.highlight {
            background = self.theme.highlight_bg;
        }
        if run_style.tag {
            color = self.theme.tag_fg;
            background = self.theme.tag_bg;
            expand_bg = 1.0;
        }
        let shift = if run_style.sup || run_style.footnote_ref {
            -style.size * 0.32
        } else if run_style.sub {
            style.size * 0.16
        } else {
            0.0
        };
        TextFormat {
            line_height: Some(self.aligned_height(&font, style, shift)),
            valign: Align::BOTTOM,
            font_id: font,
            color,
            background,
            expand_bg,
            strikethrough: if run_style.strike {
                Stroke::new(1.0, color)
            } else {
                Stroke::NONE
            },
            underline: if run_style.underline {
                Stroke::new(1.0, color)
            } else {
                Stroke::NONE
            },
            ..TextFormat::default()
        }
    }

    /// Text without inline formatting.
    fn simple_job(&self, text: &str, style: TextStyle, width: f32) -> LayoutJob {
        let mut job = LayoutJob::simple(
            text.to_owned(),
            FontId::new(style.size, self.fonts.family(style.bold, style.italic)),
            style.color,
            width,
        );
        set_line_height(self.ctx, &mut job, self.theme.line_height(style.size));
        job
    }

    /// How wide the text would like to be, for sizing table columns.
    fn text_width(&mut self, inlines: &Inlines, style: TextStyle) -> f32 {
        self.inline_job(inlines, f32::INFINITY, style).0.size().x
    }

    fn inline_job(&mut self, inlines: &Inlines, width: f32, style: TextStyle) -> LaidOut {
        let mut job = LayoutJob {
            wrap: TextWrapping {
                max_width: width,
                ..TextWrapping::default()
            },
            halign: style.halign,
            break_on_newline: true,
            // A row of just code is as tall as the others.
            first_row_min_height: self.theme.line_height(style.size),
            ..LayoutJob::default()
        };
        let mut links: Vec<(Range<usize>, Link)> = Vec::new();
        // Which images to place, by char in the galley, and how big.
        let mut placed: Vec<Placing> = Vec::new();
        let mut chars = 0;
        let mut images = inlines.images.iter().enumerate().peekable();
        let source = self.source;
        let mut spans: Vec<Range<usize>> = Vec::new();
        let mut source_cursor = 0;
        for run in &inlines.runs {
            let start_chars = chars;
            let text = &inlines.text[run.range.clone()];
            // Where each of the run's chars came from.
            let map = source.map_or_else(Vec::new, |source| {
                run_spans(inlines, run, text, source, &mut source_cursor)
            });
            let mut map_at = 0;
            let mut format = self.format(style, run.style);
            if run.link.is_some() {
                format.color = self.theme.link;
            }
            if run.style.math {
                let key = drawing::math_key(
                    text,
                    false,
                    (style.size * MATH_SCALE).round(),
                    format.color,
                    self.ctx.pixels_per_point(),
                    None,
                );
                let drawn = self.drawn(key);
                if let Drawn::Ready { key, size, depth } = drawn {
                    let scaled = fit(size, width);
                    let depth = depth * scaled.y / size.y.max(1.0);
                    let below = self.below_baseline(style);
                    let mut space = format.clone();
                    space.font_id = FontId::new(1.0, self.fonts.regular.clone());
                    space.background = Color32::TRANSPARENT;
                    let above = scaled.y - depth + below;
                    space.line_height = Some(above.max(self.theme.line_height(style.size)));
                    placed.push(Placing {
                        char: chars,
                        url: key,
                        tip: String::new(),
                        size: scaled,
                        lower: depth - below,
                    });
                    push(&mut job, "\u{A0}", space.clone());
                    space.extra_letter_spacing = scaled.x;
                    push(&mut job, "\u{A0}", space);
                    chars += 2;
                    if let Some(span) = map.first() {
                        spans.extend([span.clone(), span.clone()]);
                    }
                    if let Some(link) = &run.link {
                        links.push((start_chars..chars, link.clone()));
                    }
                    continue;
                }
                // Its source, until it's here or if it can't be.
                if matches!(drawn, Drawn::Failed(_)) {
                    format.color = self.theme.callout_color("caution");
                }
            }
            let mut plain_start = 0;
            for (offset, _) in text.match_indices(IMAGE_CHAR) {
                let before = &text[plain_start..offset];
                push(&mut job, before, format.clone());
                let count = before.chars().count();
                chars += count;
                extend_spans(&mut spans, &map, &mut map_at, count);
                plain_start = offset + IMAGE_CHAR.len_utf8();
                let chars_before_image = chars;
                let absolute = run.range.start + offset;
                let image = images.next_if(|(_, image)| image.at == absolute);
                let Some((_, image)) = image else {
                    continue;
                };
                match self.images.size(&image.url) {
                    Some(size) => {
                        let size = fit(asked_size(size, image, width), width);
                        // Takes up the image's room: tiny spaces, the second
                        // with room before it. egui drops the room before the
                        // first glyph of a row, hence the first.
                        let mut space = format.clone();
                        space.font_id = FontId::new(1.0, self.fonts.regular.clone());
                        space.background = Color32::TRANSPARENT;
                        space.line_height = Some(size.y.max(self.theme.line_height(style.size)));
                        placed.push(Placing {
                            char: chars,
                            url: image.url.clone(),
                            tip: if image.title.is_empty() {
                                image.alt.clone()
                            } else {
                                image.title.clone()
                            },
                            size,
                            lower: 0.0,
                        });
                        push(&mut job, "\u{A0}", space.clone());
                        space.extra_letter_spacing = size.x;
                        push(&mut job, "\u{A0}", space);
                        chars += 2;
                    }
                    None => {
                        // Its description, until it's here or if it can't be.
                        use egui_phosphor::regular as icon;
                        let mut alt_format = format.clone();
                        if run.link.is_none() {
                            alt_format.color = self.theme.weak;
                        }
                        let mut glyph = alt_format.clone();
                        glyph.font_id = FontId::proportional(glyph.font_id.size);
                        push(&mut job, icon::IMAGE, glyph);
                        chars += icon::IMAGE.chars().count();
                        if !image.alt.is_empty() {
                            let alt = format!(" {}", image.alt);
                            push(&mut job, &alt, alt_format);
                            chars += alt.chars().count();
                        }
                    }
                }
                // However it shows, all of it is the image.
                if let Some(span) = map.get(map_at) {
                    spans.extend((chars_before_image..chars).map(|_| span.clone()));
                    map_at += 1;
                }
            }
            let rest = &text[plain_start..];
            push(&mut job, rest, format);
            let count = rest.chars().count();
            chars += count;
            extend_spans(&mut spans, &map, &mut map_at, count);
            if let Some(link) = &run.link {
                links.push((start_chars..chars, link.clone()));
            }
        }
        if job.text.is_empty() {
            job.append(
                "",
                0.0,
                TextFormat {
                    line_height: Some(self.theme.line_height(style.size)),
                    ..self.format(style, Style::default())
                },
            );
        }
        let galley = self.cache.get(self.ctx, job);
        let placed = placed
            .into_iter()
            .map(|placing| {
                // An image starting a row belongs to that row, not the end of
                // the one before.
                let mut cursor = CCursor::new(placing.char);
                cursor.prefer_next_row = true;
                let at = galley.pos_from_cursor(cursor);
                let bottom = at.max.y + placing.lower;
                PlacedImage {
                    rect: Rect::from_min_size(
                        egui::pos2(at.min.x, bottom - placing.size.y),
                        placing.size,
                    ),
                    url: placing.url,
                    tip: placing.tip,
                }
            })
            .collect();
        // With no text, the cursor goes after the block's marker, on its line.
        let editable = source.map(|source| {
            let end = self.block_source.end.min(source.len());
            let line = source[..end].trim_end_matches(['\r', '\n']);
            let start = line.len().max(self.block_source.start.min(end));
            Editable::new(spans, start, self.field_kind, source)
        });
        (galley, links, placed, editable)
    }
}

fn extend_spans(
    spans: &mut Vec<Range<usize>>,
    map: &[Range<usize>],
    map_at: &mut usize,
    count: usize,
) {
    let end = (*map_at + count).min(map.len());
    if let Some(part) = map.get(*map_at..end) {
        spans.extend_from_slice(part);
    }
    *map_at = end;
}

/// Where a run's chars came from. A formula and an image are one thing.
fn run_spans(
    inlines: &Inlines,
    run: &Run,
    text: &str,
    source: &str,
    cursor: &mut usize,
) -> Vec<Range<usize>> {
    if run.style.math {
        *cursor = (*cursor).max(run.source.end);
        return text.chars().map(|_| run.source.clone()).collect();
    }
    let mut map = Vec::with_capacity(text.len());
    let mut plain_start = 0;
    for (offset, _) in text.match_indices(IMAGE_CHAR) {
        let before = &text[plain_start..offset];
        let image = inlines
            .images
            .iter()
            .find(|image| image.at == run.range.start + offset);
        let until = image.map_or(run.source.end, |image| image.source.start);
        map.extend(wysiwyg::spans(
            before,
            source,
            run.source.start..until,
            cursor,
        ));
        if let Some(image) = image {
            map.push(image.source.clone());
            *cursor = (*cursor).max(image.source.end);
        }
        plain_start = offset + IMAGE_CHAR.len_utf8();
    }
    map.extend(wysiwyg::spans(
        &text[plain_start..],
        source,
        run.source.clone(),
        cursor,
    ));
    map
}

/// The field of a code block, whose source starts with a fence if it's fenced.
fn code_field(source: &str, block: &Range<usize>, code: &str) -> Editable {
    let end = block.end.min(source.len());
    let start = block.start.min(end);
    let head = source[start..end].trim_start();
    let content = if head.starts_with("```") || head.starts_with("~~~") {
        source[start..end]
            .find('\n')
            .map_or(end, |at| start + at + 1)
    } else {
        start
    };
    let mut cursor = content;
    let spans = wysiwyg::spans(code, source, content..end, &mut cursor);
    Editable::new(spans, content, FieldKind::Code, source)
}

/// A picture in text, once its room is made and before the text is laid out.
struct Placing {
    /// In the galley.
    char: usize,
    url: String,
    tip: String,
    size: Vec2,
    /// How far below the bottom of its row its bottom goes.
    lower: f32,
}

fn push(job: &mut LayoutJob, text: &str, format: TextFormat) {
    if text.is_empty() {
        return;
    }
    let start = job.text.len();
    job.text.push_str(text);
    job.sections.push(LayoutSection {
        leading_space: 0.0,
        byte_range: ByteIndex(start)..ByteIndex(job.text.len()),
        format,
    });
}

/// Scales an image down to fit a width; never up.
/// The size an image's HTML asks for, keeping its shape if only one side is
/// given. A percentage is of the width of the page; of the height, as in a
/// browser with a page of no set height, it means nothing.
fn asked_size(natural: Vec2, image: &InlineImage, width: f32) -> Vec2 {
    let asked_width = image.width.and_then(|length| match length {
        Length::Points(points) => Some(points),
        Length::Percent(percent) => width.is_finite().then_some(width * percent / 100.0),
    });
    let asked_height = image.height.and_then(|length| match length {
        Length::Points(points) => Some(points),
        Length::Percent(_) => None,
    });
    let (wide, tall) = (natural.x.max(1.0), natural.y.max(1.0));
    match (asked_width, asked_height) {
        (Some(x), Some(y)) => egui::vec2(x, y),
        (Some(x), None) => egui::vec2(x, x * tall / wide),
        (None, Some(y)) => egui::vec2(y * wide / tall, y),
        (None, None) => natural,
    }
}

fn fit(size: Vec2, width: f32) -> Vec2 {
    if width.is_finite() && size.x > width && size.x > 0.0 {
        size * (width / size.x)
    } else {
        size
    }
}

fn plain_job(
    ctx: &Context,
    text: &str,
    font: FontId,
    color: Color32,
    width: f32,
    line_height: f32,
) -> LayoutJob {
    let mut job = LayoutJob::simple(text.to_owned(), font, color, width);
    set_line_height(ctx, &mut job, line_height);
    job
}

/// Sets the lines of text in one font apart, with the text in their middle.
fn set_line_height(ctx: &Context, job: &mut LayoutJob, line_height: f32) {
    for section in &mut job.sections {
        let height = ctx.fonts_mut(|fonts| fonts.row_height(&section.format.font_id));
        section.format.line_height = Some((line_height + height) / 2.0);
        section.format.valign = Align::BOTTOM;
    }
}

/// Column widths for a total: the natural ones if they fit. Otherwise narrow
/// columns keep theirs, and the wide ones share what's left in proportion,
/// down to `least`. May come out wider than `total`: the table then scrolls.
fn column_widths(natural: &[f32], total: f32, least: f32) -> Vec<f32> {
    // Cells fit in the width they measured at, rounding aside.
    let natural: Vec<f32> = natural.iter().map(|w| w.ceil() + 1.0).collect();
    if natural.iter().sum::<f32>() <= total {
        return natural;
    }
    let mut widths = natural.clone();
    // What isn't narrow takes part, until those that are left are all wide.
    let mut shrinking: Vec<usize> = (0..natural.len()).collect();
    loop {
        let fixed: f32 = (0..natural.len())
            .filter(|i| !shrinking.contains(i))
            .map(|i| widths[i])
            .sum();
        let room = (total - fixed).max(0.0);
        let wanted: f32 = shrinking.iter().map(|&i| natural[i]).sum();
        let scale = if wanted > 0.0 { room / wanted } else { 1.0 };
        let (at_least, rest): (Vec<usize>, Vec<usize>) = shrinking
            .iter()
            .partition(|&&i| natural[i] * scale < least.min(natural[i]));
        if at_least.is_empty() {
            for &i in &rest {
                widths[i] = natural[i] * scale;
            }
            return widths;
        }
        for &i in &at_least {
            widths[i] = least.min(natural[i]);
        }
        shrinking = rest;
        if shrinking.is_empty() {
            return widths;
        }
    }
}

fn offset(item: &mut Item, by: Vec2) {
    match item {
        Item::Text(text) => text.pos += by,
        Item::Marker { pos, .. } => *pos += by,
        Item::Fill { rect, .. }
        | Item::Stroke { rect, .. }
        | Item::Square { rect, .. }
        | Item::Editor { rect }
        | Item::Picture { rect, .. }
        | Item::Widget { rect, .. } => *rect = rect.translate(by),
        Item::Line { from, to, .. } => {
            *from += by;
            *to += by;
        }
        Item::Dot { center, .. } => *center += by,
        Item::Sideways { rect, .. } => *rect = rect.translate(by),
        Item::CopyCode { rect, block, .. } | Item::Enlarge { rect, block, .. } => {
            *rect = rect.translate(by);
            *block = block.translate(by);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown::parse::parse;

    struct NoImages;
    impl ImageSizes for NoImages {
        fn size(&mut self, _url: &str) -> Option<Vec2> {
            None
        }
    }

    struct Pictures(Vec2);
    impl ImageSizes for Pictures {
        fn size(&mut self, _url: &str) -> Option<Vec2> {
            Some(self.0)
        }
    }

    fn inputs(ctx: &Context, width: f32) -> Inputs {
        Inputs {
            width,
            theme: Theme::new(&egui::Visuals::dark(), Color32::WHITE, Color32::BLACK),
            fonts: Fonts::of(ctx),
            pixels_per_point: 1.0,
            font_generation: 0,
            text_options: ctx.fonts(|fonts| *fonts.options()),
            atlas_generation: 0,
            image_generation: 0,
            code_generation: 0,
            fold_generation: 0,
            allow_remote: false,
            editing: None,
            wysiwyg: false,
            gap: None,
        }
    }

    /// Lays out in a frame, as egui's fonts need.
    fn run(source: &str, width: f32, images: &mut dyn ImageSizes) -> Layout {
        let ctx = Context::default();
        let document = parse(source);
        let mut cache = GalleyCache::default();
        let mut result = None;
        let mut slot = Some(images);
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            if let Some(images) = slot.take() {
                result = Some(layout(
                    &document,
                    Params {
                        ctx: ui.ctx(),
                        inputs: &inputs(ui.ctx(), width),
                        images,
                        cache: &mut cache,
                        code_spans: &CodeSpans::new(),
                        toggled: &HashSet::new(),
                        previous_drawings: &[],
                        source: None,
                    },
                ));
            }
        });
        output.textures_delta.clear();
        result.expect("ran a frame")
    }

    /// Every drawing here, as this big, reaching this far below the baseline.
    struct Drawings {
        size: Vec2,
        depth: f32,
    }
    impl ImageSizes for Drawings {
        fn size(&mut self, _url: &str) -> Option<Vec2> {
            Some(self.size)
        }
        fn depth(&self, _key: &str) -> f32 {
            self.depth
        }
    }

    struct Failing;
    impl ImageSizes for Failing {
        fn size(&mut self, _url: &str) -> Option<Vec2> {
            None
        }
        fn error(&self, _key: &str) -> Option<String> {
            Some("Bad formula".to_owned())
        }
    }

    fn pictures(layout: &Layout) -> Vec<Rect> {
        layout
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Picture { rect, .. } => Some(*rect),
                _ => None,
            })
            .collect()
    }

    fn texts(layout: &Layout) -> Vec<&TextItem> {
        layout
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Text(text) => Some(text),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn text_flows_downward_and_within_the_width() {
        let layout = run(
            "# Title\n\nSome text that goes on for long enough to wrap around at this width, and then some.\n\n- a\n- b\n",
            200.0,
            &mut NoImages,
        );
        let items = texts(&layout);
        assert_eq!(items.len(), 4);
        let mut y = -1.0;
        for item in &items {
            assert!(item.rect().min.y > y, "goes down");
            assert!(item.rect().max.x <= 200.5, "{:?}", item.rect());
            y = item.rect().min.y;
        }
        assert!(items[1].rect().height() > items[2].rect().height(), "wraps");
        assert!(layout.height >= items[3].rect().max.y);
    }

    #[test]
    fn source_positions_map_to_y_and_back() {
        let source = "# Title\n\nFirst paragraph.\n\nSecond paragraph.\n\nThird paragraph.\n";
        let layout = run(source, 400.0, &mut NoImages);
        let second = source.find("Second").unwrap();
        let third = source.find("Third").unwrap();
        let y = layout.y_of_source(second);
        assert!(y > 0.0);
        assert!(layout.y_of_source(third) > y);
        assert_eq!(layout.source_of_y(y), second);
        assert_eq!(layout.source_of_y(0.0), 0);
        assert_eq!(layout.y_of_source(0), 0.0);
        // Past the end stays in the document.
        assert!(layout.y_of_source(source.len() + 100) <= layout.height);
    }

    #[test]
    fn anchors_are_found_for_headings_and_footnotes() {
        let layout = run(
            "# One\n\nText[^n]\n\n## Two\n\n[^n]: Note\n",
            400.0,
            &mut NoImages,
        );
        assert!(layout.anchors["two"] > layout.anchors["one"]);
        assert!(layout.anchors["fn:n"] > layout.anchors["two"]);
        assert!(layout.anchors["fnref:n"] < layout.anchors["two"]);
    }

    #[test]
    fn known_images_get_room_and_are_scaled_to_fit() {
        let mut big = Pictures(egui::vec2(800.0, 400.0));
        let layout = run("before\n\n![pic](a.png)\n\nafter\n", 400.0, &mut big);
        let items = texts(&layout);
        let image = &items[1].images[0];
        assert_eq!(image.rect.size(), egui::vec2(400.0, 200.0));
        assert!(items[1].rect().height() >= 200.0);
        assert!(items[2].rect().min.y >= items[1].rect().min.y + 200.0);

        let mut small = Pictures(egui::vec2(50.0, 20.0));
        let layout = run("![a](a.png) ![b](b.png) text", 400.0, &mut small);
        let items = texts(&layout);
        let images = &items[0].images;
        assert_eq!(images.len(), 2);
        assert!(images[1].rect.min.x >= images[0].rect.max.x, "side by side");
        assert_eq!(images[0].rect.min.y, images[1].rect.min.y);
    }

    #[test]
    fn images_that_wrap_start_their_row_and_stay_on_the_page() {
        let mut badge = Pictures(egui::vec2(90.0, 20.0));
        let source = "![a](a.svg) ".repeat(10);
        let layout = run(&source, 400.0, &mut badge);
        let item = texts(&layout)[0];
        assert_eq!(item.images.len(), 10);
        let rows: Vec<f32> = item.images.iter().map(|image| image.rect.min.x).collect();
        assert!(rows.iter().filter(|&&x| x < 1.0).count() >= 3, "{rows:?}");
        for image in &item.images {
            assert!(image.rect.max.x <= 400.5, "{:?}", image.rect);
        }
        for pair in item.images.windows(2) {
            let (a, b) = (pair[0].rect, pair[1].rect);
            assert!(b.min.x >= a.max.x || b.min.y >= a.max.y, "{a:?} {b:?}");
        }
    }

    #[test]
    fn the_sample_document_lays_out_at_any_width() {
        let source = include_str!("../../samples/markdown/showcase.md");
        for width in [60.0, 320.0, 800.0] {
            let layout = run(source, width, &mut Pictures(egui::vec2(90.0, 20.0)));
            assert!(layout.height > 0.0);
            assert!(layout.anchors.contains_key("stress"));
        }
    }

    #[test]
    fn find_looks_at_the_text_on_the_page() {
        let source = "# One word\n\nA **word** in `word` and\n\n| word |\n|---|\n\n[word](hidden-address)\n\n```\nword\n```\n";
        let layout = run(source, 500.0, &mut NoImages);
        assert_eq!(layout.find("word", false, false).len(), 6);
        assert_eq!(layout.find("WORD", true, false).len(), 0);
        assert_eq!(
            layout.find("hidden", false, false).len(),
            0,
            "not the address"
        );
        assert_eq!(layout.find("**", false, false).len(), 0, "not the markup");
        let hits = layout.find("word", false, false);
        let ys: Vec<f32> = hits.iter().map(|hit| layout.y_of_hit(hit)).collect();
        assert!(ys.windows(2).all(|pair| pair[0] <= pair[1]), "{ys:?}");
        assert!(ys[5] > ys[0]);
    }

    #[test]
    fn find_reaches_what_scrolls_sideways_and_rows_split_matches() {
        let long = format!("```\n{} needle\n```\n", "x".repeat(400));
        let layout = run(&long, 300.0, &mut NoImages);
        let hits = layout.find("needle", false, false);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].item.1.is_some());

        let layout = run(
            "one two three four five six seven eight nine ten",
            80.0,
            &mut NoImages,
        );
        let item = texts(&layout)[0];
        let all = 0..item.galley.text().chars().count();
        let rects = row_rects(&item.galley, &all);
        assert_eq!(rects.len(), item.galley.rows.len());
        assert!(
            rects
                .windows(2)
                .all(|pair| pair[1].min.y >= pair[0].max.y - 0.5)
        );
        assert_eq!(row_rects(&item.galley, &(0..3)).len(), 1);
    }

    fn run_editing(source: &str, range: Range<usize>, height: f32) -> Layout {
        let ctx = Context::default();
        let document = parse(source);
        let mut cache = GalleyCache::default();
        let mut result = None;
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            let mut inputs = inputs(ui.ctx(), 500.0);
            inputs.editing = Some((range.clone(), height));
            result = Some(layout(
                &document,
                Params {
                    ctx: ui.ctx(),
                    inputs: &inputs,
                    images: &mut NoImages,
                    cache: &mut cache,
                    code_spans: &CodeSpans::new(),
                    toggled: &HashSet::new(),
                    previous_drawings: &[],
                    source: None,
                },
            ));
        });
        output.textures_delta.clear();
        result.expect("ran a frame")
    }

    fn editors(layout: &Layout) -> Vec<Rect> {
        layout
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Editor { rect } => Some(*rect),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn the_block_being_edited_gives_way_to_its_editor() {
        let source = "first\n\nsecond\n\nthird\n";
        let at = source.find("second").unwrap();
        let layout = run_editing(source, at..at + 6, 100.0);
        let items = texts(&layout);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].galley.text(), "first");
        assert_eq!(items[1].galley.text(), "third");
        let editor = editors(&layout)[0];
        assert_eq!(editor.height(), 100.0);
        assert!(editor.min.y > items[0].rect().max.y);
        assert!(
            items[1].rect().min.y > editor.max.y,
            "what's after moves down"
        );
    }

    #[test]
    fn a_new_block_is_edited_after_the_others() {
        let source = "first\n";
        let layout = run_editing(source, source.len()..source.len(), 50.0);
        assert_eq!(texts(&layout).len(), 1);
        assert!(editors(&layout)[0].min.y > texts(&layout)[0].rect().max.y);
        let layout = run_editing("", 0..0, 50.0);
        assert_eq!(editors(&layout).len(), 1, "even with nothing there");
    }

    #[test]
    fn a_footnote_being_edited_is_edited_with_the_footnotes() {
        let source = "text[^n]\n\n[^n]: note\n\nmore\n";
        let at = source.find("[^n]:").unwrap();
        let layout = run_editing(source, at..at + 10, 40.0);
        let found = editors(&layout);
        assert_eq!(found.len(), 1);
        let items = texts(&layout);
        assert!(items.iter().all(|t| t.galley.text() != "note"));
        assert!(found[0].min.y > items[1].rect().max.y, "below the document");
    }

    #[test]
    fn a_click_finds_its_block_and_word() {
        let source = "first word\n\nsecond word here\n\n- list\n";
        let layout = run(source, 500.0, &mut NoImages);
        let items = texts(&layout);
        let second = items[1];
        let at = source.find("second").unwrap();
        assert!(
            layout
                .block_at(second.rect().center().y)
                .unwrap()
                .contains(&at)
        );
        assert!(layout.block_at(layout.height + 50.0).is_none());
        // In the gap above it: still that one, the next down.
        assert!(
            layout
                .block_at(second.rect().min.y - 1.0)
                .unwrap()
                .contains(&at)
        );
        let word = second.galley.pos_from_cursor(CCursor::new(9)).center();
        assert_eq!(
            layout.word_at(second.pos + word.to_vec2()),
            Some(("word".to_owned(), 0))
        );
    }

    #[test]
    fn links_and_tasks_are_controls_not_text_to_edit() {
        let layout = run("[link](x) plain\n\n- [ ] task\n", 500.0, &mut NoImages);
        let item = texts(&layout)[0];
        let on_link = item.galley.pos_from_cursor(CCursor::new(1)).center();
        assert!(layout.is_control_at(item.pos + on_link.to_vec2()));
        let on_plain = item.galley.pos_from_cursor(CCursor::new(8)).center();
        assert!(!layout.is_control_at(item.pos + on_plain.to_vec2()));
        let task = layout.items.iter().find_map(|item| match item {
            Item::Widget { rect, .. } => Some(*rect),
            _ => None,
        });
        assert!(layout.is_control_at(task.unwrap().center()));
    }

    #[test]
    fn missing_images_show_their_description() {
        let layout = run("![a picture](a.png)", 400.0, &mut NoImages);
        let items = texts(&layout);
        assert!(items[0].galley.text().contains("a picture"));
        assert!(items[0].images.is_empty());
    }

    #[test]
    fn links_are_found_under_the_pointer() {
        let layout = run("plain [linked](u) plain", 600.0, &mut NoImages);
        let item = texts(&layout)[0];
        let (range, link) = &item.links[0];
        assert_eq!(link.url, "u");
        let middle = item
            .galley
            .pos_from_cursor(CCursor::new((range.start + range.end) / 2))
            .center();
        assert_eq!(item.link_at(item.pos + middle.to_vec2()), Some(link));
        let start = item.galley.pos_from_cursor(CCursor::new(1)).center();
        assert_eq!(item.link_at(item.pos + start.to_vec2()), None);
    }

    #[test]
    fn wide_tables_and_code_scroll_sideways_and_narrow_ones_do_not() {
        let scrolls = |layout: &Layout| {
            layout
                .items
                .iter()
                .any(|item| matches!(item, Item::Sideways { .. }))
        };
        assert!(!scrolls(&run(
            "| a | b |\n|---|---|\n| 1 | 2 |\n",
            400.0,
            &mut NoImages
        )));
        let wide = format!(
            "| {0} | {0} | {0} | {0} | {0} | {0} |\n|---|---|---|---|---|---|\n",
            "column"
        );
        assert!(scrolls(&run(&wide, 200.0, &mut NoImages)));
        assert!(!scrolls(&run("```\nshort\n```\n", 400.0, &mut NoImages)));
        let long = format!("```\n{}\n```\n", "x".repeat(400));
        assert!(scrolls(&run(&long, 400.0, &mut NoImages)));
    }

    #[test]
    fn column_widths_keep_narrow_columns_and_shrink_wide_ones() {
        let widths = column_widths(&[50.0, 50.0], 400.0, 40.0);
        assert_eq!(widths, [51.0, 51.0]);
        let widths = column_widths(&[20.0, 1000.0], 400.0, 40.0);
        assert_eq!(widths[0], 21.0);
        assert!((widths[0] + widths[1] - 400.0).abs() < 1.0, "{widths:?}");
        let widths = column_widths(&[100.0, 100.0, 100.0], 60.0, 40.0);
        assert_eq!(widths, [40.0, 40.0, 40.0]);
    }

    #[test]
    fn folded_callouts_hide_their_body_and_toggle() {
        let source = "> [!note]- Title\n> Hidden\n";
        let document = parse(source);
        let ctx = Context::default();
        let mut heights = Vec::new();
        for toggled in [HashSet::new(), HashSet::from([0])] {
            let mut cache = GalleyCache::default();
            let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
                let layout = layout(
                    &document,
                    Params {
                        ctx: ui.ctx(),
                        inputs: &inputs(ui.ctx(), 400.0),
                        images: &mut NoImages,
                        cache: &mut cache,
                        code_spans: &CodeSpans::new(),
                        toggled: &toggled,
                        previous_drawings: &[],
                        source: None,
                    },
                );
                heights.push(layout.height);
            });
            output.textures_delta.clear();
        }
        assert!(heights[1] > heights[0]);
    }

    #[test]
    fn deep_lists_at_narrow_widths_do_not_panic() {
        let mut source = String::new();
        for depth in 0..40 {
            source.push_str(&"  ".repeat(depth));
            source.push_str("- item\n");
        }
        let layout = run(&source, 30.0, &mut NoImages);
        assert!(layout.height > 0.0);
    }

    #[test]
    fn every_kind_of_block_lays_out() {
        let source = "---\ntitle: x\n---\n\n# H\n\n> quote\n\n> [!tip] T\n> body\n\n1. a\n2. [ ] b\n\nTerm\n: def\n\n| a |\n|---|\n| b |\n\n---\n\n<div>\n\n$$x$$\n\n`c` ==d== #e :rocket: [[f]] ~~g~~ H~2~ x^2^[^n]\n\n[^n]: note\n";
        let layout = run(source, 500.0, &mut NoImages);
        assert!(texts(&layout).len() >= 10);
    }

    #[test]
    fn inline_math_sits_on_the_baseline_of_its_line() {
        let size = egui::vec2(30.0, 30.0);
        let shallow = Drawings { size, depth: 0.0 };
        let (mut shallow, mut deep) = (shallow, Drawings { size, depth: 10.0 });
        let flat = run("a $x$ b", 400.0, &mut shallow);
        let low = run("a $x$ b", 400.0, &mut deep);
        // How far its baseline is from the bottom of the row: that of the text.
        let above_bottom = |layout: &Layout, depth: f32| {
            let text = texts(layout)[0];
            text.galley.size().y - (text.images[0].rect.max.y - depth)
        };
        assert_eq!(above_bottom(&flat, 0.0), above_bottom(&low, 10.0));
        assert!(above_bottom(&flat, 0.0) > 0.0);
        let image = &texts(&flat)[0].images[0];
        assert_eq!(image.rect.size(), size);
        assert!(image.url.starts_with('\0'), "not asked for as a file");
    }

    #[test]
    fn a_formula_reaching_below_its_row_makes_room_in_the_block() {
        let mut images = Drawings {
            size: egui::vec2(30.0, 40.0),
            depth: 30.0,
        };
        let layout = run("$x$", 400.0, &mut images);
        let text = texts(&layout)[0];
        assert!(text.images[0].rect.max.y > text.galley.size().y);
        assert!(layout.height >= text.images[0].rect.max.y);
    }

    #[test]
    fn display_math_is_centered_or_scrolls_when_too_wide() {
        let mut images = Pictures(egui::vec2(100.0, 40.0));
        let layout = run("$$\nx\n$$\n", 400.0, &mut images);
        let rects = pictures(&layout);
        assert_eq!(rects.len(), 1);
        assert_eq!(rects[0].center().x, 200.0);
        assert_eq!(layout.height, 40.0);

        let mut images = Pictures(egui::vec2(900.0, 40.0));
        let layout = run("$$\nx\n$$\n", 400.0, &mut images);
        assert!(layout.items.iter().any(|item| matches!(
            item,
            Item::Sideways { content_width, .. } if *content_width == 900.0
        )));
    }

    #[test]
    fn a_diagram_is_a_picture_scaled_to_fit_that_can_be_enlarged() {
        let mut images = Pictures(egui::vec2(2000.0, 1000.0));
        let layout = run(
            "```mermaid\nflowchart LR\n A --> B\n```\n",
            400.0,
            &mut images,
        );
        let rects = pictures(&layout);
        assert_eq!(rects.len(), 1);
        assert!(rects[0].width() <= 400.0);
        assert!((rects[0].width() / rects[0].height() - 2.0).abs() < 0.01);
        assert!(
            layout
                .items
                .iter()
                .any(|item| matches!(item, Item::Enlarge { .. }))
        );
        assert!(
            layout
                .items
                .iter()
                .any(|item| matches!(item, Item::CopyCode { .. }))
        );
    }

    #[test]
    fn what_is_not_drawn_yet_shows_its_source() {
        let layout = run(
            "$$\nx^2\n$$\n\n```mermaid\npie\n```\n",
            400.0,
            &mut NoImages,
        );
        let shown: Vec<&str> = texts(&layout).iter().map(|t| t.galley.text()).collect();
        assert_eq!(shown, ["x^2", "pie"]);
        assert!(pictures(&layout).is_empty());
    }

    #[test]
    fn what_cannot_be_drawn_shows_its_source_and_why() {
        let layout = run("$$\n\\bad\n$$\n", 400.0, &mut Failing);
        assert_eq!(texts(&layout)[0].galley.text(), "\\bad");
        assert!(layout.items.iter().any(|item| matches!(
            item,
            Item::Marker { galley, .. } if galley.text() == "Bad formula"
        )));
    }

    #[test]
    fn layout_lists_its_drawings_in_order() {
        let source = "$a$ and $b$\n\n$$\nc\n$$\n\n```mermaid\npie\n```\n";
        let layout = run(source, 400.0, &mut NoImages);
        assert_eq!(layout.drawings.len(), 4);
        assert!(layout.drawings[0].ends_with('a'));
        assert!(layout.drawings[1].ends_with('b'));
        assert!(layout.drawings[2].ends_with('c'));
        assert!(drawing::is_diagram(&layout.drawings[3]));
    }

    fn fields_of(source: &str) -> Layout {
        let ctx = Context::default();
        let document = parse(source);
        let mut cache = GalleyCache::default();
        let mut result = None;
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            let mut wide = inputs(ui.ctx(), 700.0);
            wide.wysiwyg = true;
            result = Some(layout(
                &document,
                Params {
                    ctx: ui.ctx(),
                    inputs: &wide,
                    images: &mut NoImages,
                    cache: &mut cache,
                    code_spans: &CodeSpans::new(),
                    toggled: &HashSet::new(),
                    previous_drawings: &[],
                    source: Some(source),
                },
            ));
        });
        output.textures_delta.clear();
        result.expect("ran a frame")
    }

    /// Everything on the page goes back to the bytes it came from: what the
    /// cursor and every edit depend on.
    #[test]
    fn every_char_of_the_showcase_maps_to_its_source() {
        let source = include_str!("../../samples/markdown/showcase.md");
        let laid_out = fields_of(source);
        let fields = laid_out.placed_fields();
        assert!(fields.len() > 100, "{}", fields.len());
        let mut chars_seen = 0;
        let mut exact = 0;
        for placed in &fields {
            let field = placed.field;
            let text: Vec<char> = placed.text.galley.text().chars().collect();
            assert_eq!(
                field.spans.len(),
                text.len(),
                "{:?}",
                placed.text.galley.text()
            );
            for (index, span) in field.spans.iter().enumerate() {
                assert!(source.is_char_boundary(span.start) && source.is_char_boundary(span.end));
                assert!(span.start <= span.end && span.end <= source.len());
                if let Some(next) = field.spans.get(index + 1) {
                    assert!(
                        span.end <= next.start || span == next,
                        "in order: {span:?} {next:?} in {:?}",
                        placed.text.galley.text()
                    );
                }
                chars_seen += 1;
                let mine = text[index];
                if source[span.clone()].contains(mine) {
                    exact += 1;
                }
            }
        }
        // Only what isn't written as itself differs: emoji, entities, images,
        // formulas waiting to be drawn, soft breaks.
        let share = f64::from(exact) / f64::from(chars_seen);
        assert!(share > 0.97, "{exact} of {chars_seen}");
    }

    #[test]
    fn hidden_markup_is_not_part_of_any_char() {
        let source = "A **bold** [link](http://x) `code` *it* ~~no~~ \\* &amp; end\n";
        let laid_out = fields_of(source);
        let fields = laid_out.placed_fields();
        assert_eq!(fields.len(), 1);
        let shown: String = fields[0]
            .field
            .spans
            .iter()
            .map(|span| &source[span.clone()])
            .collect();
        assert_eq!(shown, "A bold link code it no \\* &amp; end");
    }

    #[test]
    fn html_tags_are_not_part_of_any_char() {
        let source = "A <span>a</span> <kbd>s</kbd>x<br>y\nz\n";
        let laid_out = fields_of(source);
        let fields = laid_out.placed_fields();
        assert_eq!(fields.len(), 1);
        let shown: Vec<&str> = fields[0]
            .field
            .spans
            .iter()
            .map(|span| &source[span.clone()])
            .collect();
        assert_eq!(
            shown,
            ["A", " ", "a", " ", "s", "x", "<br>", "y", "\n", "z"]
        );
    }

    #[test]
    fn a_line_of_html_is_one_unit() {
        let laid_out = fields_of("<span>\none\n</span>\n");
        assert!(laid_out.placed_fields().is_empty());
    }

    #[test]
    fn images_take_the_size_their_html_asks_for() {
        let image = |width, height| InlineImage {
            url: String::new(),
            alt: String::new(),
            title: String::new(),
            at: 0,
            source: 0..0,
            width,
            height,
        };
        let natural = egui::vec2(100.0, 50.0);
        let asked = |width, height, page| asked_size(natural, &image(width, height), page);
        let (points, percent) = (Length::Points(200.0), Length::Percent(50.0));
        assert_eq!(asked(None, None, 400.0), natural);
        assert_eq!(asked(Some(points), None, 400.0), egui::vec2(200.0, 100.0));
        assert_eq!(asked(None, Some(points), 400.0), egui::vec2(400.0, 200.0));
        assert_eq!(asked(Some(percent), None, 400.0), egui::vec2(200.0, 100.0));
        assert_eq!(
            asked(Some(points), Some(Length::Points(10.0)), 400.0),
            egui::vec2(200.0, 10.0)
        );
        // A percentage of nothing, as when measuring a table's columns.
        assert_eq!(asked(Some(percent), None, f32::INFINITY), natural);
        assert_eq!(asked(None, Some(percent), 400.0), natural);
    }

    #[test]
    fn an_anchor_in_html_can_be_gone_to() {
        let laid_out = fields_of("first\n\n<a id=\"here\"></a>second\n");
        assert!(laid_out.anchors["here"] > 0.0);
    }

    #[test]
    fn a_blocks_marker_is_not_part_of_its_text() {
        for (source, text) in [
            ("## Head\n", "Head"),
            ("- item\n", "item"),
            ("1. item\n", "item"),
            ("> quoted\n", "quoted"),
            ("- [ ] task\n", "task"),
            ("```\ncode\n```\n", "code"),
            ("| a |\n|---|\n| b |\n", "a"),
        ] {
            let laid_out = fields_of(source);
            let fields = laid_out.placed_fields();
            let span = fields[0].field.low()..fields[0].field.high();
            assert_eq!(&source[span], text, "{source:?}");
        }
    }

    #[test]
    fn a_quoted_paragraphs_line_break_takes_the_next_lines_marker() {
        let source = "> one\n> two\n";
        let laid_out = fields_of(source);
        let fields = laid_out.placed_fields();
        let shown: Vec<&str> = fields[0]
            .field
            .spans
            .iter()
            .map(|span| &source[span.clone()])
            .collect();
        assert_eq!(shown, ["o", "n", "e", "\n> ", "t", "w", "o"]);
    }

    #[test]
    fn the_cursor_goes_along_the_page_and_between_blocks() {
        let source = "ab\n\n## cd\n";
        let laid_out = fields_of(source);
        assert_eq!(laid_out.offset_along(0, false, false), Some(1));
        assert_eq!(
            laid_out.offset_along(2, false, false),
            Some(7),
            "past the marker"
        );
        assert_eq!(laid_out.offset_along(7, true, false), Some(2));
        assert_eq!(laid_out.offset_along(0, true, false), None);
        assert_eq!(laid_out.offset_along(9, false, false), None);
    }

    #[test]
    fn the_cursor_goes_up_and_down_between_blocks_at_its_place_across() {
        let source = "abcdef\n\nabcdef\n";
        let laid_out = fields_of(source);
        let fields = laid_out.placed_fields();
        let x = fields[0].cursor_rect(source, 3).min.x;
        let down = laid_out.offset_vertically(source, 3, x, true).unwrap();
        assert_eq!(down, 11);
        assert_eq!(laid_out.offset_vertically(source, down, x, false), Some(3));
        assert_eq!(laid_out.offset_vertically(source, 3, x, false), None);
    }

    #[test]
    fn a_click_finds_the_source_past_hidden_markup() {
        let source = "**bold**\n";
        let laid_out = fields_of(source);
        let fields = laid_out.placed_fields();
        let at = fields[0].cursor_rect(source, 4).center();
        assert_eq!(laid_out.offset_at_pos(at), Some(4));
        // Well to the right of the text: its end.
        let far = egui::pos2(600.0, at.y);
        assert_eq!(laid_out.offset_at_pos(far), Some(6));
    }

    #[test]
    fn a_formula_is_one_thing_to_the_cursor() {
        let source = "a $x^2$ b\n";
        let laid_out = fields_of(source);
        let fields = laid_out.placed_fields();
        let field = fields[0].field;
        let formula = field.spans.iter().find(|span| span.len() > 1).unwrap();
        assert_eq!(&source[formula.clone()], "$x^2$");
    }

    /// Whatever's done wherever in the showcase: never a panic or a cut through
    /// a character, the cursor stays in the text, and what comes before and
    /// after the edit isn't touched.
    #[test]
    fn every_edit_everywhere_keeps_the_rest_of_the_source() {
        use crate::markdown::wysiwyg::{self, Change};
        let source = include_str!("../../samples/markdown/showcase.md");
        let laid_out = fields_of(source);
        let placed = laid_out.placed_fields();
        let fields: Vec<&Editable> = placed.iter().map(|placed| placed.field).collect();
        let check = |what: &str, at: usize, change: &Change| {
            assert!(
                change.text.is_char_boundary(change.cursor),
                "{what} at {at}: cursor {}",
                change.cursor
            );
            let common_start = source
                .bytes()
                .zip(change.text.bytes())
                .take_while(|(a, b)| a == b)
                .count();
            let common_end = source
                .bytes()
                .rev()
                .zip(change.text.bytes().rev())
                .take_while(|(a, b)| a == b)
                .count();
            // Only one place changes, and only by a little: a char or two,
            // its marks, and the blank lines of a join.
            let changed = source.len().saturating_sub(common_start + common_end);
            assert!(changed < 400, "{what} at {at} changed {changed} bytes");
        };
        let mut offsets: Vec<usize> = fields
            .iter()
            .flat_map(|field| {
                let mut all: Vec<usize> = field.spans.iter().map(|span| span.start).collect();
                all.push(field.high());
                all
            })
            .collect();
        offsets.sort_unstable();
        offsets.dedup();
        // Every tenth, as each is a copy of the document: it's still thousands.
        let mut edits = 0;
        for &at in offsets.iter().step_by(10) {
            if let Some(change) = wysiwyg::backspace(source, &fields, at) {
                check("backspace", at, &change);
                edits += 1;
            }
            if let Some(change) = wysiwyg::delete_forward(source, &fields, at) {
                check("delete", at, &change);
                edits += 1;
            }
            if let Some(change) = wysiwyg::enter(source, &fields, at..at) {
                check("enter", at, &change);
                edits += 1;
            }
            if let Some(change) = wysiwyg::line_break(source, &fields, at..at) {
                check("line break", at, &change);
            }
            let end = (at + 7).min(source.len());
            let end = (end..source.len())
                .find(|&end| source.is_char_boundary(end))
                .unwrap_or(source.len());
            if end - at < 60
                && let Some(change) = wysiwyg::delete_selection(source, &fields, at..end)
            {
                check("selection", at, &change);
            }
            let _ = laid_out.offset_along(at, false, true);
            let _ = laid_out.offset_along(at, true, true);
            let _ = laid_out.row_edge(at, true);
            let _ = laid_out.word_around(at);
            let _ = laid_out.offset_vertically(source, at, 100.0, true);
        }
        assert!(edits > 500, "{edits}");
    }
}
