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

use super::parse::{
    Block, BlockKind, Callout, CalloutFold, Document, IMAGE_CHAR, Inlines, Link, Style, Task,
};
use crate::editor::highlight::{self, LineSpans};

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
    /// Wider than the page: scrolls sideways on its own.
    Sideways {
        rect: Rect,
        content_width: f32,
        items: Vec<Item>,
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
            | Item::Sideways { rect, .. } => *rect,
            Item::CopyCode { block, .. } => *block,
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
    pub image_generation: u64,
    pub code_generation: u64,
    pub fold_generation: u64,
    pub allow_remote: bool,
}

#[derive(Default)]
pub struct Layout {
    pub items: Vec<Item>,
    pub height: f32,
    /// Heading and footnote anchors, to where they are.
    pub anchors: HashMap<String, f32>,
    /// `(source offset, y)` of each block, both increasing.
    pub map: Vec<(usize, f32)>,
}

impl Layout {
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
}

pub fn layout(document: &Document, params: Params<'_>) -> Layout {
    let Params {
        ctx,
        inputs,
        images,
        cache,
        code_spans,
        toggled,
    } = params;
    let mut builder = Builder {
        ctx,
        theme: &inputs.theme,
        fonts: &inputs.fonts,
        images,
        cache,
        code_spans,
        toggled,
        out: Layout::default(),
        y: 0.0,
        list_depth: 0,
    };
    let width = inputs.width;
    builder.blocks(&document.blocks, 0.0, width);
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
    out: Layout,
    y: f32,
    /// How many lists this is inside of, which varies their bullets.
    list_depth: usize,
}

/// Inline text laid out: its galley, its links by char, and its images.
type LaidOut = (Arc<Galley>, Vec<(Range<usize>, Link)>, Vec<PlacedImage>);

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

    fn blocks(&mut self, blocks: &[Block], x: f32, width: f32) {
        for (index, block) in blocks.iter().enumerate() {
            if index > 0 {
                self.y += self.theme.gap();
            }
            self.block(block, x, width);
        }
    }

    fn block(&mut self, block: &Block, x: f32, width: f32) {
        self.out.map.push((block.source.start, self.y));
        match &block.kind {
            BlockKind::Paragraph(inlines) => {
                let style = self.body();
                self.text(inlines, x, width, style);
            }
            BlockKind::Heading { level, slug, text } => self.heading(*level, slug, text, x, width),
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
            // Drawn properly by a later change; its source until then.
            BlockKind::Math(math) => self.raw(math.trim(), x, width),
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
        let text = Item::Text(TextItem {
            pos: egui::pos2(padding, padding),
            galley,
            links: Vec::new(),
            images: Vec::new(),
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
                    let (galley, links, images) = self.inline_job(cell, *width, style);
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
            }));
            self.out.items.push(Item::Text(TextItem {
                pos: egui::pos2(x + padding + key_width + gap, self.y),
                galley: value,
                links: Vec::new(),
                images: Vec::new(),
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
                }));
            }
            self.blocks(&footnote.blocks, gutter, width - gutter - size * 1.5);
        }
    }

    fn text(&mut self, inlines: &Inlines, x: f32, width: f32, style: TextStyle) {
        let (galley, links, images) = self.inline_job(inlines, width, style);
        let height = galley.size().y;
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
        if run_style.html {
            font = FontId::monospace((size * 0.9).round());
            color = self.theme.weak;
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
        let mut placed: Vec<(usize, usize, Vec2)> = Vec::new();
        let mut chars = 0;
        let mut images = inlines.images.iter().enumerate().peekable();
        for run in &inlines.runs {
            let start_chars = chars;
            let text = &inlines.text[run.range.clone()];
            let mut format = self.format(style, run.style);
            if run.link.is_some() {
                format.color = self.theme.link;
            }
            let mut plain_start = 0;
            for (offset, _) in text.match_indices(IMAGE_CHAR) {
                let before = &text[plain_start..offset];
                push(&mut job, before, format.clone());
                chars += before.chars().count();
                plain_start = offset + IMAGE_CHAR.len_utf8();
                let absolute = run.range.start + offset;
                let image = images.next_if(|(_, image)| image.at == absolute);
                let Some((index, image)) = image else {
                    continue;
                };
                match self.images.size(&image.url) {
                    Some(size) => {
                        let size = fit(size, width);
                        // Takes up the image's room: tiny spaces, the second
                        // with room before it. egui drops the room before the
                        // first glyph of a row, hence the first.
                        let mut space = format.clone();
                        space.font_id = FontId::new(1.0, self.fonts.regular.clone());
                        space.background = Color32::TRANSPARENT;
                        space.line_height = Some(size.y.max(self.theme.line_height(style.size)));
                        placed.push((chars, index, size));
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
            }
            let rest = &text[plain_start..];
            push(&mut job, rest, format);
            chars += rest.chars().count();
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
            .map(|(char, index, size)| {
                // An image starting a row belongs to that row, not the end of
                // the one before.
                let mut cursor = CCursor::new(char);
                cursor.prefer_next_row = true;
                let at = galley.pos_from_cursor(cursor);
                let image = &inlines.images[index];
                PlacedImage {
                    rect: Rect::from_min_size(egui::pos2(at.min.x, at.max.y - size.y), size),
                    url: image.url.clone(),
                    tip: if image.title.is_empty() {
                        image.alt.clone()
                    } else {
                        image.title.clone()
                    },
                }
            })
            .collect();
        (galley, links, placed)
    }
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
        | Item::Widget { rect, .. } => *rect = rect.translate(by),
        Item::Line { from, to, .. } => {
            *from += by;
            *to += by;
        }
        Item::Dot { center, .. } => *center += by,
        Item::Sideways { rect, .. } => *rect = rect.translate(by),
        Item::CopyCode { rect, block, .. } => {
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
            image_generation: 0,
            code_generation: 0,
            fold_generation: 0,
            allow_remote: false,
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
                    },
                ));
            }
        });
        output.textures_delta.clear();
        result.expect("ran a frame")
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
}
