//! Markdown text to a tree of blocks, each knowing which bytes of the source
//! it came from. No egui here: what's on screen is worked out in `layout`.

use std::{collections::HashMap, ops::Range};

use pulldown_cmark::{
    Alignment, CodeBlockKind, Event, HeadingLevel, LinkType, MetadataBlockKind, OffsetIter,
    Options, Parser, Tag, TagEnd,
};

use super::html::{self, Align, Length, Piece};

/// Containers nested deeper than this are flattened: `>>>>…` a thousand times
/// mustn't overflow the stack, here or in the layout.
const MAX_DEPTH: usize = 32;

/// The most columns a table's cell can take up: `colspan` can say anything.
const MAX_COLSPAN: usize = 64;

/// Stands in for an inline image in [`Inlines::text`].
pub const IMAGE_CHAR: char = '\u{FFFC}';

#[derive(Debug, Default)]
pub struct Document {
    pub blocks: Vec<Block>,
    /// Footnote definitions, in the order they're first referred to.
    pub footnotes: Vec<Footnote>,
    pub headings: Vec<Heading>,
    /// Images to fetch from the network; the renderer asks before it does.
    pub has_remote_images: bool,
}

#[derive(Debug, PartialEq)]
pub struct Heading {
    pub level: u8,
    pub text: String,
    pub slug: String,
    pub source: Range<usize>,
}

#[derive(Debug)]
pub struct Footnote {
    pub label: String,
    pub source: Range<usize>,
    pub number: usize,
    pub blocks: Vec<Block>,
}

#[derive(Debug)]
pub struct Block {
    pub source: Range<usize>,
    pub kind: BlockKind,
}

#[derive(Debug)]
pub enum BlockKind {
    Paragraph(Inlines),
    Heading {
        level: u8,
        slug: String,
        text: Inlines,
    },
    Code {
        lang: Option<String>,
        code: String,
    },
    Quote {
        callout: Option<Callout>,
        children: Vec<Block>,
    },
    List {
        /// The first number of an ordered list.
        start: Option<u64>,
        items: Vec<ListItem>,
    },
    Definitions(Vec<Definition>),
    Table {
        aligns: Vec<Alignment>,
        head: Vec<Inlines>,
        rows: Vec<Vec<Inlines>>,
    },
    Rule,
    /// A YAML or TOML block at the top of the file.
    Frontmatter(Vec<(String, String)>),
    /// Only between finding a block of HTML and working out what it says.
    Html(String),
    /// `<details>`: `key` tells it from others when it's opened or closed.
    Details {
        summary: Inlines,
        open: bool,
        key: usize,
        children: Vec<Block>,
    },
    HtmlTable(Vec<HtmlRow>),
    /// Made from raw HTML: drawn, but edited as its source.
    HtmlUnit(Box<Block>),
    /// What HTML sets to one side or the center.
    Aligned {
        align: Align,
        children: Vec<Block>,
    },
    /// `$$…$$` alone in a paragraph.
    Math(String),
}

#[derive(Debug)]
pub struct HtmlRow {
    pub cells: Vec<HtmlCell>,
}

#[derive(Debug)]
pub struct HtmlCell {
    pub text: Inlines,
    pub header: bool,
    pub align: Option<Align>,
    /// How many columns it takes up, at least one.
    pub colspan: usize,
}

#[derive(Debug)]
pub struct ListItem {
    pub source: Range<usize>,
    pub task: Option<Task>,
    pub children: Vec<Block>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Task {
    pub checked: bool,
    /// The `[ ]` in the source.
    pub source: Range<usize>,
}

#[derive(Debug)]
pub struct Definition {
    pub term: Inlines,
    pub details: Vec<Vec<Block>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CalloutFold {
    /// Can't be collapsed.
    None,
    Open,
    Closed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Callout {
    /// As written, in lowercase: `note`, `warning`, or anything else.
    pub kind: String,
    /// The title on its line, or else the kind's name.
    pub title: String,
    pub fold: CalloutFold,
}

/// Text with its formatting, flattened into runs.
#[derive(Clone, Debug, Default)]
pub struct Inlines {
    pub text: String,
    pub runs: Vec<Run>,
    pub images: Vec<InlineImage>,
    /// Footnotes referred to here, with where in `text`, to jump back to.
    pub footnote_refs: Vec<String>,
    /// What raw HTML here names with `id`, for links to go to.
    pub anchors: Vec<String>,
    /// Where the text's first line ends, as a callout's title does.
    first_break: Option<usize>,
    has_display_math: bool,
    /// What's pushed next comes after an HTML tag.
    after_html: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Run {
    /// In [`Inlines::text`].
    pub range: Range<usize>,
    /// In the source, roughly: escapes and entities don't line up within it.
    pub source: Range<usize>,
    pub style: Style,
    pub link: Option<Link>,
    /// An HTML tag comes before it. Runs aren't joined over one: where a
    /// run's chars came from is looked for in its stretch of source, and a
    /// tag's name mustn't be taken for them.
    after_html: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Style {
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
    pub underline: bool,
    pub code: bool,
    pub math: bool,
    pub highlight: bool,
    pub tag: bool,
    pub sup: bool,
    pub sub: bool,
    /// Stands for an HTML tag rather than being in the source itself.
    pub html: bool,
    pub footnote_ref: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    pub url: String,
    pub title: String,
    /// `[[like this]]`: names a note rather than a path.
    pub wiki: bool,
}

#[derive(Clone, Debug)]
pub struct InlineImage {
    pub url: String,
    pub alt: String,
    pub title: String,
    /// Where its [`IMAGE_CHAR`] is in the text.
    pub at: usize,
    /// The whole of it, in the source.
    pub source: Range<usize>,
    /// The size its HTML tag asks for.
    pub width: Option<Length>,
    pub height: Option<Length>,
    /// To one side on its own line, from `align` in its HTML tag.
    pub align: Option<Align>,
    /// The `<source>` of its `<picture>` for each theme.
    pub dark_url: Option<String>,
    pub light_url: Option<String>,
}

impl InlineImage {
    /// What to show in this theme, if anything: a page may have one image for
    /// each, marked as GitHub has them.
    pub fn url_for(&self, dark: bool) -> Option<&str> {
        let themed = if dark {
            &self.dark_url
        } else {
            &self.light_url
        };
        let url = themed.as_deref().unwrap_or(&self.url);
        if let Some(url) = url.strip_suffix("#gh-dark-mode-only") {
            return dark.then_some(url);
        }
        if let Some(url) = url.strip_suffix("#gh-light-mode-only") {
            return (!dark).then_some(url);
        }
        Some(url)
    }
}

/// How much there is to read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub words: usize,
    pub chars: usize,
}

impl Stats {
    /// At 200 words a minute, and at least a minute.
    pub fn minutes(self) -> usize {
        self.words.div_ceil(200).max(1)
    }

    fn add(&mut self, text: &str) {
        self.chars += text
            .chars()
            .filter(|c| !c.is_whitespace() && *c != IMAGE_CHAR)
            .count();
        let mut in_word = false;
        for c in text.chars() {
            // These scripts don't put spaces between words: count characters.
            let wide = matches!(u32::from(c), 0x3040..=0x30FF | 0x3400..=0x9FFF | 0xF900..=0xFAFF);
            if wide {
                self.words += 1;
                in_word = false;
            } else if c.is_alphanumeric() {
                self.words += usize::from(!in_word);
                in_word = true;
            } else if !matches!(c, '\'' | '\u{2019}' | '-') {
                in_word = false;
            }
        }
    }

    fn add_blocks(&mut self, blocks: &[Block]) {
        for block in blocks {
            match &block.kind {
                BlockKind::Paragraph(text) | BlockKind::Heading { text, .. } => {
                    self.add(&text.text)
                }
                BlockKind::HtmlUnit(block) => self.add_blocks(std::slice::from_ref(block)),
                BlockKind::Aligned { children, .. } => self.add_blocks(children),
                BlockKind::Code { code, .. } => self.add(code),
                BlockKind::Quote { callout, children } => {
                    if let Some(callout) = callout {
                        self.add(&callout.title);
                    }
                    self.add_blocks(children);
                }
                BlockKind::List { items, .. } => {
                    for item in items {
                        self.add_blocks(&item.children);
                    }
                }
                BlockKind::Definitions(definitions) => {
                    for definition in definitions {
                        self.add(&definition.term.text);
                        for detail in &definition.details {
                            self.add_blocks(detail);
                        }
                    }
                }
                BlockKind::Table { head, rows, .. } => {
                    for cell in head.iter().chain(rows.iter().flatten()) {
                        self.add(&cell.text);
                    }
                }
                BlockKind::Details {
                    summary, children, ..
                } => {
                    self.add(&summary.text);
                    self.add_blocks(children);
                }
                BlockKind::HtmlTable(rows) => {
                    for cell in rows.iter().flat_map(|row| &row.cells) {
                        self.add(&cell.text.text);
                    }
                }
                BlockKind::Math(text) => self.add(text),
                BlockKind::Rule | BlockKind::Frontmatter(_) | BlockKind::Html(_) => {}
            }
        }
    }
}

impl Document {
    pub fn stats(&self) -> Stats {
        let mut stats = Stats::default();
        stats.add_blocks(&self.blocks);
        for footnote in &self.footnotes {
            stats.add_blocks(&footnote.blocks);
        }
        stats
    }
}

impl Inlines {
    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty() && self.images.is_empty()
    }

    fn push(&mut self, text: &str, source: Range<usize>, style: Style, link: Option<&Link>) {
        if text.is_empty() {
            return;
        }
        let start = self.text.len();
        self.text.push_str(text);
        let end = self.text.len();
        let after_html = std::mem::take(&mut self.after_html);
        if let Some(last) = self.runs.last_mut()
            && last.range.end == start
            && last.style == style
            && last.link.as_ref() == link
            && !after_html
        {
            last.range.end = end;
            last.source.end = last.source.end.max(source.end);
            return;
        }
        self.runs.push(Run {
            range: start..end,
            source,
            style,
            link: link.cloned(),
            after_html,
        });
    }

    /// A `<br>` ending a paragraph, which makes no line in browsers either.
    fn drop_last_html_break(&mut self) {
        let Some(run) = self.runs.last() else {
            return;
        };
        if run.style.html && self.text[run.range.clone()] == *"\n" {
            self.text.truncate(run.range.start);
            self.runs.pop();
        }
    }

    /// Adds text that's in no particular place in the source.
    pub fn push_plain(&mut self, text: &str) {
        self.push(text, 0..0, Style::default(), None);
    }

    /// Adds what's in `other` on a new line.
    fn append_line(&mut self, other: Self) {
        let shift = if self.text.is_empty() {
            0
        } else {
            self.text.push('\n');
            self.text.len()
        };
        self.text.push_str(&other.text);
        self.runs.extend(other.runs.into_iter().map(|mut run| {
            run.range = run.range.start + shift..run.range.end + shift;
            run
        }));
        self.images
            .extend(other.images.into_iter().map(|mut image| {
                image.at += shift;
                image
            }));
        self.anchors.extend(other.anchors);
    }

    /// Takes off what's from `at` on, which must be where a run starts.
    fn split_off(&mut self, at: usize) -> Self {
        let mut tail = Self {
            text: self.text.split_off(at),
            ..Self::default()
        };
        let first = self.runs.partition_point(|run| run.range.start < at);
        tail.runs = self.runs.split_off(first);
        for run in &mut tail.runs {
            run.range = run.range.start - at..run.range.end - at;
        }
        let first = self.images.partition_point(|image| image.at < at);
        tail.images = self.images.split_off(first);
        for image in &mut tail.images {
            image.at -= at;
        }
        if self.first_break.is_some_and(|line| line >= at) {
            tail.first_break = self.first_break.take().map(|line| line - at);
        }
        tail
    }

    /// The plain text, without images, for headings' slugs and the like.
    pub fn plain(&self) -> String {
        self.text.replace(IMAGE_CHAR, "")
    }

    /// Removes the first `len` bytes of the text.
    fn remove_prefix(&mut self, len: usize) {
        self.text.drain(..len);
        self.runs.retain_mut(|run| {
            run.range.start = run.range.start.saturating_sub(len);
            run.range.end = run.range.end.saturating_sub(len);
            !run.range.is_empty()
        });
        self.images.retain_mut(|image| {
            let kept = image.at >= len;
            image.at = image.at.saturating_sub(len);
            kept
        });
    }
}

/// Parses `source`. Never fails: markdown has no invalid input.
pub fn parse(source: &str) -> Document {
    let mut builder = Builder {
        events: Parser::new_ext(source, options())
            .into_offset_iter()
            .peekable(),
        slugs: HashMap::new(),
        footnote_numbers: HashMap::new(),
        footnote_blocks: Vec::new(),
        document: Document::default(),
    };
    let blocks = builder.blocks(None, 0);
    let mut document = builder.document;
    document.blocks = blocks;
    let numbers = builder.footnote_numbers;
    let mut footnotes: Vec<Footnote> = builder
        .footnote_blocks
        .into_iter()
        .map(|(label, source, blocks)| Footnote {
            number: numbers.get(&label).copied().unwrap_or(usize::MAX),
            label,
            source,
            blocks,
        })
        .collect();
    // HTML's are found after the markdown's around them.
    document
        .headings
        .sort_by_key(|heading| heading.source.start);
    // Footnotes nothing refers to go last, numbered after the others.
    footnotes.sort_by_key(|f| f.number);
    let referred = footnotes.iter().filter(|f| f.number != usize::MAX).count();
    for (index, footnote) in footnotes.iter_mut().enumerate().skip(referred) {
        footnote.number = index + 1;
    }
    document.footnotes = footnotes;
    document
}

pub(super) fn options() -> Options {
    Options::ENABLE_TABLES
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_HEADING_ATTRIBUTES
        | Options::ENABLE_YAML_STYLE_METADATA_BLOCKS
        | Options::ENABLE_PLUSES_DELIMITED_METADATA_BLOCKS
        | Options::ENABLE_MATH
        | Options::ENABLE_DEFINITION_LIST
        | Options::ENABLE_SUPERSCRIPT
        | Options::ENABLE_SUBSCRIPT
        | Options::ENABLE_WIKILINKS
}

#[derive(Clone, Copy)]
enum MathKind {
    Inline,
    Display,
}

struct Builder<'a> {
    events: std::iter::Peekable<OffsetIter<'a>>,
    /// How many times each slug was used, to make them unique.
    slugs: HashMap<String, usize>,
    footnote_numbers: HashMap<String, usize>,
    footnote_blocks: Vec<(String, Range<usize>, Vec<Block>)>,
    document: Document,
}

impl Builder<'_> {
    /// Blocks until the `end` tag, which it consumes (or the end of the text),
    /// with blocks of HTML as they're written.
    fn flat_blocks(&mut self, end: Option<TagEnd>, depth: usize) -> Vec<Block> {
        let mut blocks = Vec::new();
        // Ends of containers flattened for being too deep.
        let mut flattened = Vec::new();
        while let Some((event, range)) = self.events.peek().cloned() {
            match event {
                Event::End(tag) if flattened.last() == Some(&tag) => {
                    flattened.pop();
                    self.events.next();
                }
                Event::End(tag) if Some(tag) == end => {
                    self.events.next();
                    return blocks;
                }
                Event::End(_) => {
                    // Not expected, but never a reason to stop or to loop.
                    self.events.next();
                }
                Event::Start(tag) if is_block(&tag) => {
                    self.events.next();
                    if depth >= MAX_DEPTH
                        && let Some(end) = container_end(&tag)
                    {
                        flattened.push(end);
                        continue;
                    }
                    self.block(tag, range, depth, &mut blocks);
                }
                Event::Rule => {
                    self.events.next();
                    blocks.push(Block {
                        source: range,
                        kind: BlockKind::Rule,
                    });
                }
                // Inline content straight in a container: a tight list's items.
                Event::Start(_)
                | Event::Text(_)
                | Event::Code(_)
                | Event::InlineMath(_)
                | Event::DisplayMath(_)
                | Event::InlineHtml(_)
                | Event::Html(_)
                | Event::FootnoteReference(_)
                | Event::SoftBreak
                | Event::HardBreak
                | Event::TaskListMarker(_) => {
                    let inlines = self.inlines(None);
                    let source = inlines_source(&inlines, range);
                    if !inlines.is_empty() {
                        blocks.push(Block {
                            source,
                            kind: BlockKind::Paragraph(inlines),
                        });
                    }
                }
            }
        }
        blocks
    }

    fn block(&mut self, tag: Tag<'_>, range: Range<usize>, depth: usize, blocks: &mut Vec<Block>) {
        let kind = match tag {
            Tag::Paragraph => {
                let inlines = self.inlines(Some(TagEnd::Paragraph));
                match display_math(&inlines) {
                    Some(math) => BlockKind::Math(math),
                    None => BlockKind::Paragraph(inlines),
                }
            }
            Tag::Heading { level, id, .. } => {
                let text = self.inlines(Some(TagEnd::Heading(level)));
                let level = heading_level(level);
                let plain = text.plain();
                let wanted = id.map_or_else(|| slug(&plain), |id| id.to_string());
                let slug = self.unique_slug(wanted);
                self.document.headings.push(Heading {
                    level,
                    text: plain,
                    slug: slug.clone(),
                    source: range.clone(),
                });
                BlockKind::Heading { level, slug, text }
            }
            Tag::CodeBlock(kind) => {
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => info
                        .split(|c: char| c.is_whitespace() || c == ',' || c == '{')
                        .next()
                        .filter(|lang| !lang.is_empty())
                        .map(str::to_owned),
                    CodeBlockKind::Indented => None,
                };
                BlockKind::Code {
                    lang,
                    code: self.raw_text(TagEnd::CodeBlock),
                }
            }
            Tag::HtmlBlock => BlockKind::Html(self.raw_text(TagEnd::HtmlBlock)),
            Tag::MetadataBlock(kind) => {
                let raw = self.raw_text(TagEnd::MetadataBlock(kind));
                BlockKind::Frontmatter(frontmatter(&raw, kind))
            }
            Tag::BlockQuote(kind) => {
                let mut children = self.blocks(Some(TagEnd::BlockQuote(kind)), depth + 1);
                let callout = take_callout(&mut children);
                BlockKind::Quote { callout, children }
            }
            Tag::List(start) => BlockKind::List {
                start,
                items: self.list_items(depth),
            },
            Tag::DefinitionList => BlockKind::Definitions(self.definitions(depth)),
            Tag::Table(aligns) => {
                let (head, rows) = self.table();
                BlockKind::Table { aligns, head, rows }
            }
            Tag::FootnoteDefinition(label) => {
                let label = label.to_string();
                let children = self.blocks(Some(TagEnd::FootnoteDefinition), depth + 1);
                self.footnote_blocks.push((label, range, children));
                return;
            }
            // Parts of a container, out of place: not reachable from
            // pulldown-cmark, but dropping the text would be worse.
            other => {
                let Some(end) = tag_end(&other) else { return };
                let children = self.blocks(Some(end), depth + 1);
                blocks.extend(children);
                return;
            }
        };
        blocks.push(Block {
            source: range,
            kind,
        });
    }

    fn list_items(&mut self, depth: usize) -> Vec<ListItem> {
        let mut items = Vec::new();
        while let Some((event, range)) = self.events.next() {
            match event {
                Event::Start(Tag::Item) => {
                    // The marker, if it's there, comes before anything else.
                    let task = match self.events.peek() {
                        Some((Event::TaskListMarker(checked), marker)) => Some(Task {
                            checked: *checked,
                            source: marker.clone(),
                        }),
                        _ => None,
                    };
                    if task.is_some() {
                        self.events.next();
                    }
                    items.push(ListItem {
                        source: range,
                        task,
                        children: self.blocks(Some(TagEnd::Item), depth + 1),
                    });
                }
                Event::End(TagEnd::List(_)) => break,
                _ => {}
            }
        }
        items
    }

    fn definitions(&mut self, depth: usize) -> Vec<Definition> {
        let mut definitions: Vec<Definition> = Vec::new();
        while let Some((event, _)) = self.events.next() {
            match event {
                Event::Start(Tag::DefinitionListTitle) => definitions.push(Definition {
                    term: self.inlines(Some(TagEnd::DefinitionListTitle)),
                    details: Vec::new(),
                }),
                Event::Start(Tag::DefinitionListDefinition) => {
                    let details = self.blocks(Some(TagEnd::DefinitionListDefinition), depth + 1);
                    if let Some(last) = definitions.last_mut() {
                        last.details.push(details);
                    }
                }
                Event::End(TagEnd::DefinitionList) => break,
                _ => {}
            }
        }
        definitions
    }

    fn table(&mut self) -> (Vec<Inlines>, Vec<Vec<Inlines>>) {
        let mut head = Vec::new();
        let mut rows: Vec<Vec<Inlines>> = Vec::new();
        let mut in_head = false;
        while let Some((event, _)) = self.events.next() {
            match event {
                Event::Start(Tag::TableHead) => in_head = true,
                Event::End(TagEnd::TableHead) => in_head = false,
                Event::Start(Tag::TableRow) => rows.push(Vec::new()),
                Event::Start(Tag::TableCell) => {
                    let cell = self.inlines(Some(TagEnd::TableCell));
                    if in_head {
                        head.push(cell);
                    } else if let Some(row) = rows.last_mut() {
                        row.push(cell);
                    }
                }
                Event::End(TagEnd::Table) => break,
                _ => {}
            }
        }
        (head, rows)
    }

    /// The text up to `end`, for blocks whose content isn't markdown.
    fn raw_text(&mut self, end: TagEnd) -> String {
        let mut text = String::new();
        for (event, _) in self.events.by_ref() {
            match event {
                Event::End(tag) if tag == end => break,
                Event::Text(chunk) | Event::Html(chunk) => text.push_str(&chunk),
                _ => {}
            }
        }
        text
    }

    /// Blocks until the `end` tag, with the HTML among them made into what
    /// it says: a tag may be opened in one block and closed some blocks on,
    /// with markdown between them, so this can only be done with all of them.
    fn blocks(&mut self, end: Option<TagEnd>, depth: usize) -> Vec<Block> {
        let mut nest = HtmlNest::default();
        for block in self.flat_blocks(end, depth) {
            nest.end = nest.end.max(block.source.end);
            match block.kind {
                BlockKind::Html(html) => self.html_block(html, block.source, &mut nest, depth),
                BlockKind::Paragraph(inlines) if has_aligned_image(&inlines) => {
                    for piece in split_aligned_images(inlines, &block.source) {
                        nest.push(piece);
                    }
                }
                kind => nest.push(Block {
                    source: block.source,
                    kind,
                }),
            }
        }
        while !nest.frames.is_empty() {
            self.close_frame(&mut nest);
        }
        nest.blocks
    }

    /// One block of HTML, whose tags may leave things open for those after it.
    fn html_block(&mut self, html: String, range: Range<usize>, nest: &mut HtmlNest, depth: usize) {
        for (index, piece) in html::pieces(&html).into_iter().enumerate() {
            let in_pre = matches!(
                nest.frames.last(),
                Some(HtmlFrame {
                    kind: FrameKind::Pre { .. },
                    ..
                })
            );
            match piece {
                // A cell is a line of text: what's blocks elsewhere are its lines.
                Piece::Open(tag)
                    if nest.in_cell()
                        && html::is_block_tag(&tag.name)
                        && !html::is_table_part(&tag.name) =>
                {
                    nest.line_break(&range);
                }
                Piece::Close(name)
                    if nest.in_cell()
                        && html::is_block_tag(&name)
                        && !html::is_table_part(&name)
                        && name != "table" => {}
                Piece::Open(tag) if html::is_block_tag(&tag.name) => {
                    self.flush_html_text(nest, &range);
                    // Each piece has a place of its own in the source.
                    self.open_frame(tag, &range, range.start + index, nest, depth);
                }
                Piece::Close(name) if html::is_block_tag(&name) => {
                    self.flush_html_text(nest, &range);
                    if let Some(index) = nest.frames.iter().rposition(|frame| frame.name == name) {
                        nest.end = nest.end.max(range.end);
                        while nest.frames.len() > index {
                            self.close_frame(nest);
                        }
                    }
                }
                Piece::Text(text) if in_pre => {
                    if let Some(HtmlFrame {
                        kind: FrameKind::Pre { code, .. },
                        ..
                    }) = nest.frames.last_mut()
                    {
                        code.push_str(&text);
                    }
                }
                _ if in_pre => {}
                Piece::Text(text) => nest.text(&text, &range),
                piece => {
                    if matches!(&piece, Piece::Open(_)) {
                        nest.put_space(&range);
                    }
                    let open = &mut nest.open;
                    self.html_piece(piece, &range, &mut nest.line, open, Style::default(), None);
                }
            }
        }
        self.flush_html_text(nest, &range);
    }

    /// Text gathered so far becomes a block, unless a heading, summary or
    /// cell is collecting it.
    fn flush_html_text(&mut self, nest: &mut HtmlNest, range: &Range<usize>) {
        if matches!(
            nest.frames.last(),
            Some(HtmlFrame {
                kind: FrameKind::Heading { .. } | FrameKind::Summary | FrameKind::Cell { .. },
                ..
            })
        ) {
            return;
        }
        nest.space = false;
        let mut line = std::mem::take(&mut nest.line);
        line.drop_last_html_break();
        if line.is_empty() {
            return;
        }
        for piece in split_aligned_images(line, range) {
            nest.push(Block {
                source: range.clone(),
                kind: BlockKind::HtmlUnit(Box::new(piece)),
            });
        }
    }

    fn open_frame(
        &mut self,
        tag: html::Tag,
        range: &Range<usize>,
        key: usize,
        nest: &mut HtmlNest,
        depth: usize,
    ) {
        // A block ends the paragraph it's in, and an item the item before.
        if nest.frames.last().is_some_and(|frame| frame.name == "p") {
            self.close_frame(nest);
        }
        if tag.name == "li" {
            let list = nest
                .frames
                .iter()
                .rposition(|frame| matches!(frame.kind, FrameKind::List { .. }))
                .map_or(0, |at| at + 1);
            if let Some(at) = nest.frames[list..].iter().position(|f| f.name == "li") {
                while nest.frames.len() > list + at {
                    self.close_frame(nest);
                }
            }
        }
        if tag.name == "col" {
            return;
        }
        if tag.name == "hr" {
            return nest.push(Block {
                source: range.clone(),
                kind: BlockKind::Rule,
            });
        }
        // Rows and cells end what came before them in the table.
        let table = nest
            .frames
            .iter()
            .rposition(|frame| matches!(frame.kind, FrameKind::Table { .. }));
        if let Some(table) = table {
            let is_row = |frame: &HtmlFrame| matches!(frame.kind, FrameKind::Row { .. });
            match tag.name.as_str() {
                "tr" | "thead" | "tbody" | "tfoot" => {
                    while nest.frames.len() > table + 1 {
                        self.close_frame(nest);
                    }
                }
                "td" | "th" => match nest.frames[table..].iter().rposition(is_row) {
                    Some(row) => {
                        while nest.frames.len() > table + row + 1 {
                            self.close_frame(nest);
                        }
                    }
                    // A cell with no row has one made for it.
                    None => nest.frames.push(HtmlFrame {
                        name: "tr".to_owned(),
                        kind: FrameKind::Row { cells: Vec::new() },
                        align: None,
                        start: range.start,
                        children: Vec::new(),
                        items: Vec::new(),
                    }),
                },
                _ => {}
            }
        }
        // As with markdown's containers, there's a limit to how deep it goes.
        if depth + nest.frames.len() >= MAX_DEPTH {
            return;
        }
        let kind = match tag.name.as_str() {
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => FrameKind::Heading {
                level: tag.name.as_bytes()[1] - b'0',
                id: tag.attribute("id").map(str::to_owned),
            },
            "ul" | "dir" | "menu" => FrameKind::List { start: None },
            "ol" => FrameKind::List {
                start: Some(
                    tag.attribute("start")
                        .and_then(|start| start.trim().parse().ok())
                        .unwrap_or(1),
                ),
            },
            "li" => FrameKind::Item,
            "blockquote" => FrameKind::Quote,
            "details" => FrameKind::Details {
                summary: None,
                open: tag.attribute("open").is_some(),
                key,
            },
            "summary"
                if matches!(
                    nest.frames.last(),
                    Some(HtmlFrame {
                        kind: FrameKind::Details { .. },
                        ..
                    })
                ) =>
            {
                FrameKind::Summary
            }
            "table" => FrameKind::Table { rows: Vec::new() },
            "tr" if table.is_some() => FrameKind::Row { cells: Vec::new() },
            "td" | "th" if table.is_some() => FrameKind::Cell {
                header: tag.name == "th",
                colspan: tag
                    .attribute("colspan")
                    .and_then(|span| span.trim().parse().ok())
                    .unwrap_or(1)
                    .clamp(1, MAX_COLSPAN),
            },
            "pre" => FrameKind::Pre {
                lang: tag.attribute("lang").map(str::to_owned),
                code: String::new(),
            },
            _ => FrameKind::Container,
        };
        let align = match tag.name.as_str() {
            "center" => Some(Align::Center),
            _ => tag.attribute("align").and_then(Align::parse),
        };
        nest.frames.push(HtmlFrame {
            name: tag.name,
            kind,
            align,
            start: range.start,
            children: Vec::new(),
            items: Vec::new(),
        });
    }

    fn close_frame(&mut self, nest: &mut HtmlNest) {
        let Some(frame) = nest.frames.pop() else {
            return;
        };
        let source = frame.start..nest.end.max(frame.start);
        let mut children = frame.children;
        // A cell's or row's `align` is for the table to use.
        let frame_is_cell = matches!(frame.kind, FrameKind::Cell { .. } | FrameKind::Row { .. });
        let kind = match frame.kind {
            FrameKind::Container => None,
            FrameKind::Heading { level, id } => {
                let text = std::mem::take(&mut nest.line);
                nest.space = false;
                (!text.is_empty()).then(|| self.html_heading(level, id, text, &source))
            }
            FrameKind::List { start } => {
                let mut items = frame.items;
                // Whatever's in a list outside its items.
                if !children.is_empty() {
                    items.push(ListItem {
                        source: source.clone(),
                        task: None,
                        children: std::mem::take(&mut children),
                    });
                }
                (!items.is_empty()).then_some(BlockKind::List { start, items })
            }
            FrameKind::Item => {
                if let Some(HtmlFrame {
                    kind: FrameKind::List { .. },
                    items,
                    ..
                }) = nest.frames.last_mut()
                {
                    items.push(ListItem {
                        source,
                        task: None,
                        children,
                    });
                    return;
                }
                None
            }
            FrameKind::Quote => Some(BlockKind::Quote {
                callout: None,
                children: std::mem::take(&mut children),
            }),
            FrameKind::Details { summary, open, key } => Some(BlockKind::Details {
                summary: summary.unwrap_or_default(),
                open,
                key,
                children: std::mem::take(&mut children),
            }),
            FrameKind::Summary => {
                let text = std::mem::take(&mut nest.line);
                nest.space = false;
                if let Some(HtmlFrame {
                    kind: FrameKind::Details { summary, .. },
                    ..
                }) = nest.frames.last_mut()
                {
                    *summary = Some(text);
                }
                None
            }
            FrameKind::Table { rows } => {
                // What wasn't in a cell comes before the table, as in browsers.
                for block in std::mem::take(&mut children) {
                    nest.push(block);
                }
                (!rows.is_empty()).then(|| {
                    BlockKind::HtmlUnit(Box::new(Block {
                        source: source.clone(),
                        kind: BlockKind::HtmlTable(rows),
                    }))
                })
            }
            FrameKind::Row { cells } => {
                if let Some(table) = nest
                    .frames
                    .iter_mut()
                    .rev()
                    .find_map(|frame| match &mut frame.kind {
                        FrameKind::Table { rows } => Some(rows),
                        _ => None,
                    })
                    && !cells.is_empty()
                {
                    table.push(HtmlRow { cells });
                }
                None
            }
            FrameKind::Cell { header, colspan } => {
                let mut text = std::mem::take(&mut nest.line);
                nest.space = false;
                text.drop_last_html_break();
                // Markdown between the cell's tags, as far as a cell can hold it.
                children.retain_mut(|child| match paragraph_mut(child) {
                    Some(paragraph) => {
                        text.append_line(std::mem::take(paragraph));
                        false
                    }
                    None => true,
                });
                if let Some(HtmlFrame {
                    kind: FrameKind::Row { cells },
                    align,
                    ..
                }) = nest.frames.last_mut()
                {
                    cells.push(HtmlCell {
                        text,
                        header,
                        align: frame.align.or(*align),
                        colspan,
                    });
                }
                None
            }
            FrameKind::Pre { lang, code } => {
                let code = code.strip_prefix('\n').unwrap_or(&code).to_owned();
                Some(BlockKind::HtmlUnit(Box::new(Block {
                    source: source.clone(),
                    kind: BlockKind::Code { lang, code },
                })))
            }
        };
        let mut blocks: Vec<Block> = kind
            .map(|kind| Block {
                source: source.clone(),
                kind: match kind {
                    heading @ BlockKind::Heading { .. } => BlockKind::HtmlUnit(Box::new(Block {
                        source: source.clone(),
                        kind: heading,
                    })),
                    kind => kind,
                },
            })
            .into_iter()
            .collect();
        blocks.append(&mut children);
        match frame.align.filter(|_| !frame_is_cell) {
            Some(align) if !blocks.is_empty() => nest.push(Block {
                source,
                kind: BlockKind::Aligned {
                    align,
                    children: blocks,
                },
            }),
            _ => blocks.into_iter().for_each(|block| nest.push(block)),
        }
    }

    fn html_heading(
        &mut self,
        level: u8,
        id: Option<String>,
        text: Inlines,
        source: &Range<usize>,
    ) -> BlockKind {
        let plain = text.plain();
        let slug = self.unique_slug(id.unwrap_or_else(|| slug(&plain)));
        self.document.headings.push(Heading {
            level,
            text: plain,
            slug: slug.clone(),
            source: source.clone(),
        });
        BlockKind::Heading { level, slug, text }
    }

    /// One tag of raw HTML in a line of text. `marks` is the style markdown
    /// gives the text here, and `link` the link it's in.
    fn html_piece(
        &mut self,
        piece: Piece,
        range: &Range<usize>,
        out: &mut Inlines,
        open: &mut html::Open,
        marks: Style,
        link: Option<&Link>,
    ) {
        let tag = match piece {
            Piece::Close(name) => return open.close(&name),
            _ if open.hidden() => return,
            Piece::Text(text) => {
                let link = open.link().or(link);
                return out.push(&text, range.clone(), open.apply(marks), link);
            }
            Piece::Open(tag) => tag,
        };
        let style = open.apply(marks);
        let id = tag
            .attribute("id")
            .or_else(|| tag.attribute("name").filter(|_| tag.name == "a"));
        out.anchors.extend(id.map(str::to_owned));
        match tag.name.as_str() {
            "br" => {
                out.first_break.get_or_insert(out.text.len());
                let style = Style {
                    html: true,
                    ..style
                };
                out.push("\n", range.clone(), style, open.link().or(link));
            }
            "picture" => open.start_picture(),
            "source" => open.picture_source(&tag),
            "img" => {
                let Some(src) = tag.attribute("src") else {
                    return;
                };
                let (dark_url, light_url) = open.picture_urls();
                self.document.has_remote_images |= is_remote(src)
                    || [&dark_url, &light_url]
                        .into_iter()
                        .flatten()
                        .any(|url| is_remote(url));
                let align = tag
                    .attribute("align")
                    .and_then(Align::parse)
                    .filter(|align| *align != Align::Center);
                // On its own, it can be taken out of the line.
                out.after_html |= align.is_some();
                out.images.push(InlineImage {
                    url: src.to_owned(),
                    alt: tag.attribute("alt").unwrap_or_default().to_owned(),
                    title: tag.attribute("title").unwrap_or_default().to_owned(),
                    at: out.text.len(),
                    source: range.clone(),
                    width: tag.attribute("width").and_then(Length::parse),
                    height: tag.attribute("height").and_then(Length::parse),
                    align,
                    dark_url,
                    light_url,
                });
                let mut placeholder = [0; 4];
                // Code is left as it is by `extras`, which an image can't be.
                let style = Style {
                    code: false,
                    ..style
                };
                let link = open.link().or(link);
                out.push(
                    IMAGE_CHAR.encode_utf8(&mut placeholder),
                    range.clone(),
                    style,
                    link,
                );
                out.after_html |= align.is_some();
            }
            _ => open.open(&tag),
        }
    }

    /// Inline content up to `end` (which it consumes); without one, up to
    /// the next block, as in a tight list's item.
    fn inlines(&mut self, end: Option<TagEnd>) -> Inlines {
        let mut out = Inlines::default();
        // The style markdown gives the text here, and what raw HTML adds.
        let mut marks = Style::default();
        let mut open = html::Open::default();
        // How many of each are open, as they nest and may repeat.
        let (mut bold, mut italic, mut strike, mut sup, mut sub) = (0, 0, 0, 0, 0);
        let mut links: Vec<Link> = Vec::new();
        while let Some((event, range)) = self.events.peek().cloned() {
            match event {
                Event::End(tag) if Some(tag) == end => {
                    self.events.next();
                    break;
                }
                Event::Start(tag) if is_block(&tag) => break,
                Event::End(tag) if !is_inline_end(tag) => break,
                Event::Rule => break,
                _ => {
                    self.events.next();
                }
            }
            let link = open.link().or(links.last());
            let style = open.apply(marks);
            // What's in `<style>` and the like isn't to be seen.
            if open.hidden()
                && matches!(
                    event,
                    Event::Text(_)
                        | Event::Code(_)
                        | Event::InlineMath(_)
                        | Event::DisplayMath(_)
                        | Event::SoftBreak
                        | Event::HardBreak
                        | Event::FootnoteReference(_)
                )
            {
                continue;
            }
            let event_kind = if matches!(event, Event::DisplayMath(_)) {
                MathKind::Display
            } else {
                MathKind::Inline
            };
            match event {
                Event::Text(text) => out.push(&text, range, style, link),
                Event::Code(text) => {
                    let style = Style {
                        code: true,
                        ..style
                    };
                    out.push(&text, range, style, link);
                }
                Event::InlineMath(text) | Event::DisplayMath(text) => {
                    out.has_display_math |= matches!(event_kind, MathKind::Display);
                    let style = Style {
                        math: true,
                        ..style
                    };
                    out.push(&text, range, style, link);
                }
                Event::InlineHtml(text) | Event::Html(text) => {
                    let link = links.last();
                    for piece in html::pieces(&text) {
                        self.html_piece(piece, &range, &mut out, &mut open, marks, link);
                    }
                    out.after_html = true;
                }
                Event::SoftBreak => {
                    out.first_break.get_or_insert(out.text.len());
                    out.push(" ", range, style, link);
                }
                Event::HardBreak => {
                    out.first_break.get_or_insert(out.text.len());
                    out.push("\n", range, style, link);
                }
                Event::FootnoteReference(label) => {
                    let next = self.footnote_numbers.len() + 1;
                    let number = *self
                        .footnote_numbers
                        .entry(label.to_string())
                        .or_insert(next);
                    let link = Link {
                        url: format!("#fn:{label}"),
                        title: String::new(),
                        wiki: false,
                    };
                    let style = Style {
                        footnote_ref: true,
                        ..style
                    };
                    out.push(&number.to_string(), range, style, Some(&link));
                    out.footnote_refs.push(label.to_string());
                }
                Event::Start(Tag::Emphasis) => italic += 1,
                Event::End(TagEnd::Emphasis) => italic -= 1,
                Event::Start(Tag::Strong) => bold += 1,
                Event::End(TagEnd::Strong) => bold -= 1,
                Event::Start(Tag::Strikethrough) => strike += 1,
                Event::End(TagEnd::Strikethrough) => strike -= 1,
                Event::Start(Tag::Superscript) => sup += 1,
                Event::End(TagEnd::Superscript) => sup -= 1,
                Event::Start(Tag::Subscript) => sub += 1,
                Event::End(TagEnd::Subscript) => sub -= 1,
                Event::Start(Tag::Link {
                    link_type,
                    dest_url,
                    title,
                    ..
                }) => links.push(Link {
                    url: dest_url.to_string(),
                    title: title.to_string(),
                    wiki: matches!(link_type, LinkType::WikiLink { .. }),
                }),
                Event::End(TagEnd::Link) => {
                    links.pop();
                }
                Event::Start(Tag::Image {
                    dest_url, title, ..
                }) => {
                    let alt = self.image_alt();
                    if is_remote(&dest_url) {
                        self.document.has_remote_images = true;
                    }
                    let mut placeholder = [0; 4];
                    let at = out.text.len();
                    out.images.push(InlineImage {
                        url: dest_url.to_string(),
                        alt,
                        title: title.to_string(),
                        at,
                        source: range.clone(),
                        width: None,
                        height: None,
                        align: None,
                        dark_url: None,
                        light_url: None,
                    });
                    out.push(IMAGE_CHAR.encode_utf8(&mut placeholder), range, style, link);
                }
                _ => {}
            }
            marks.bold = bold > 0;
            marks.italic = italic > 0;
            marks.strike = strike > 0;
            marks.sup = sup > 0;
            marks.sub = sub > 0;
        }
        out.drop_last_html_break();
        extras(&mut out);
        out
    }

    /// An image's description, which markdown allows formatting in, as text.
    fn image_alt(&mut self) -> String {
        let mut alt = String::new();
        let mut depth = 0;
        for (event, _) in self.events.by_ref() {
            match event {
                Event::End(TagEnd::Image) if depth == 0 => break,
                Event::Start(Tag::Image { .. }) => depth += 1,
                Event::End(TagEnd::Image) => depth -= 1,
                Event::Text(text) | Event::Code(text) => alt.push_str(&text),
                _ => {}
            }
        }
        alt
    }

    fn unique_slug(&mut self, wanted: String) -> String {
        let count = self.slugs.entry(wanted.clone()).or_insert(0);
        *count += 1;
        if *count == 1 {
            wanted
        } else {
            let unique = format!("{wanted}-{}", *count - 1);
            self.slugs.insert(unique.clone(), 1);
            unique
        }
    }
}

#[derive(Debug)]
enum FrameKind {
    /// Holds blocks, and maybe sets them to one side.
    Container,
    Heading {
        level: u8,
        id: Option<String>,
    },
    List {
        start: Option<u64>,
    },
    Item,
    Quote,
    Details {
        summary: Option<Inlines>,
        open: bool,
        key: usize,
    },
    Summary,
    Table {
        rows: Vec<HtmlRow>,
    },
    Row {
        cells: Vec<HtmlCell>,
    },
    Cell {
        header: bool,
        colspan: usize,
    },
    Pre {
        lang: Option<String>,
        code: String,
    },
}

/// A block tag that's open.
#[derive(Debug)]
struct HtmlFrame {
    name: String,
    kind: FrameKind,
    align: Option<Align>,
    start: usize,
    children: Vec<Block>,
    items: Vec<ListItem>,
}

/// HTML's blocks being put together, over the blocks of a markdown container.
#[derive(Default)]
struct HtmlNest {
    blocks: Vec<Block>,
    frames: Vec<HtmlFrame>,
    /// The line of text being gathered.
    line: Inlines,
    open: html::Open,
    /// White space between words, seen but not yet put down.
    space: bool,
    /// How far the source has been read.
    end: usize,
}

impl HtmlNest {
    fn push(&mut self, block: Block) {
        match self.frames.last_mut() {
            Some(frame) => frame.children.push(block),
            None => self.blocks.push(block),
        }
    }

    fn in_cell(&self) -> bool {
        matches!(
            self.frames.last(),
            Some(HtmlFrame {
                kind: FrameKind::Cell { .. },
                ..
            })
        )
    }

    /// A new line in the text being gathered, if there's anything on this one.
    fn line_break(&mut self, range: &Range<usize>) {
        self.space = false;
        if !self.line.is_empty() && !self.line.text.ends_with('\n') {
            let style = Style {
                html: true,
                ..Style::default()
            };
            self.line.push("\n", range.clone(), style, None);
        }
    }

    fn line_ends_with_aligned_image(&self) -> bool {
        self.line.images.last().is_some_and(|image| {
            image.align.is_some() && image.at + IMAGE_CHAR.len_utf8() == self.line.text.len()
        })
    }

    /// The white space seen between what's in a line of HTML, as one space.
    fn put_space(&mut self, range: &Range<usize>) {
        if std::mem::take(&mut self.space)
            && !self.line.is_empty()
            && !self.line.text.ends_with('\n')
            && !self.line_ends_with_aligned_image()
        {
            let style = self.open.apply(Style::default());
            self.line.push(" ", range.clone(), style, self.open.link());
        }
    }

    /// Text in HTML, where white space is all the same and doesn't add up.
    fn text(&mut self, text: &str, range: &Range<usize>) {
        if self.open.hidden() {
            return;
        }
        let words = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if words.is_empty() {
            self.space |= !text.is_empty();
            return;
        }
        self.space |= text.starts_with(char::is_whitespace);
        self.put_space(range);
        let style = self.open.apply(Style::default());
        self.line
            .push(&words, range.clone(), style, self.open.link());
        self.space = text.ends_with(char::is_whitespace);
    }
}

/// The text of a paragraph, whether markdown's or HTML's.
fn paragraph_mut(block: &mut Block) -> Option<&mut Inlines> {
    match &mut block.kind {
        BlockKind::Paragraph(inlines) => Some(inlines),
        BlockKind::HtmlUnit(inner) => paragraph_mut(inner),
        _ => None,
    }
}

fn has_aligned_image(inlines: &Inlines) -> bool {
    inlines.images.iter().any(|image| image.align.is_some())
}

/// A paragraph, with images set to one side taken out to lines of their own.
fn split_aligned_images(mut inlines: Inlines, source: &Range<usize>) -> Vec<Block> {
    let paragraph = |inlines| Block {
        source: source.clone(),
        kind: BlockKind::Paragraph(inlines),
    };
    let aligned: Vec<(usize, Align)> = inlines
        .images
        .iter()
        .filter_map(|image| Some((image.at, image.align?)))
        .collect();
    let mut pieces = Vec::new();
    for (at, align) in aligned.into_iter().rev() {
        let after = inlines.split_off(at + IMAGE_CHAR.len_utf8());
        if !after.is_empty() {
            pieces.push(paragraph(after));
        }
        let image = inlines.split_off(at);
        pieces.push(Block {
            source: source.clone(),
            kind: BlockKind::Aligned {
                align,
                children: vec![paragraph(image)],
            },
        });
    }
    if !inlines.is_empty() {
        pieces.push(paragraph(inlines));
    }
    pieces.reverse();
    pieces
}

/// Whether a link or image is fetched over the network.
pub fn is_remote(url: &str) -> bool {
    let lower = url.get(..8).unwrap_or(url).to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

fn inlines_source(inlines: &Inlines, fallback: Range<usize>) -> Range<usize> {
    let start = inlines
        .runs
        .first()
        .map_or(fallback.start, |r| r.source.start);
    let end = inlines.runs.last().map_or(fallback.end, |r| r.source.end);
    start..end.max(start)
}

fn is_block(tag: &Tag<'_>) -> bool {
    !matches!(
        tag,
        Tag::Emphasis
            | Tag::Strong
            | Tag::Strikethrough
            | Tag::Superscript
            | Tag::Subscript
            | Tag::Link { .. }
            | Tag::Image { .. }
    )
}

fn is_inline_end(tag: TagEnd) -> bool {
    matches!(
        tag,
        TagEnd::Emphasis
            | TagEnd::Strong
            | TagEnd::Strikethrough
            | TagEnd::Superscript
            | TagEnd::Subscript
            | TagEnd::Link
            | TagEnd::Image
    )
}

/// The end of a tag that holds other blocks, which can be left out.
fn container_end(tag: &Tag<'_>) -> Option<TagEnd> {
    match tag {
        Tag::BlockQuote(kind) => Some(TagEnd::BlockQuote(*kind)),
        Tag::List(start) => Some(TagEnd::List(start.is_some())),
        Tag::Item => Some(TagEnd::Item),
        Tag::DefinitionList => Some(TagEnd::DefinitionList),
        Tag::DefinitionListTitle => Some(TagEnd::DefinitionListTitle),
        Tag::DefinitionListDefinition => Some(TagEnd::DefinitionListDefinition),
        _ => None,
    }
}

fn tag_end(tag: &Tag<'_>) -> Option<TagEnd> {
    match tag {
        Tag::Item => Some(TagEnd::Item),
        Tag::DefinitionListTitle => Some(TagEnd::DefinitionListTitle),
        Tag::DefinitionListDefinition => Some(TagEnd::DefinitionListDefinition),
        Tag::TableHead => Some(TagEnd::TableHead),
        Tag::TableRow => Some(TagEnd::TableRow),
        Tag::TableCell => Some(TagEnd::TableCell),
        _ => None,
    }
}

fn heading_level(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

/// A heading's anchor, as GitHub makes them.
pub fn slug(text: &str) -> String {
    text.trim()
        .chars()
        .filter_map(|c| match c {
            ' ' | '-' => Some('-'),
            '_' => Some('_'),
            c if c.is_alphanumeric() => Some(c),
            _ => None,
        })
        .flat_map(char::to_lowercase)
        .collect()
}

/// `$$…$$` as the only thing in a paragraph is a block of its own.
fn display_math(inlines: &Inlines) -> Option<String> {
    match inlines.runs.as_slice() {
        [run] if run.style.math && inlines.has_display_math => {
            Some(inlines.text[run.range.clone()].to_owned())
        }
        _ => None,
    }
}

/// `key: value` lines; indented lines belong to the key before them.
fn frontmatter(raw: &str, kind: MetadataBlockKind) -> Vec<(String, String)> {
    let separator = match kind {
        MetadataBlockKind::YamlStyle => ':',
        MetadataBlockKind::PlusesStyle => '=',
    };
    let mut fields: Vec<(String, String)> = Vec::new();
    for line in raw.lines() {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let continues = line.starts_with([' ', '\t']) || line.starts_with("- ");
        match line.split_once(separator) {
            Some((key, value)) if !continues => {
                let value = value.trim().trim_matches(['"', '\'']);
                fields.push((key.trim().to_owned(), value.to_owned()));
            }
            _ => {
                if let Some((_, value)) = fields.last_mut() {
                    if !value.is_empty() {
                        value.push_str(", ");
                    }
                    value.push_str(line.trim().trim_start_matches("- "));
                } else {
                    fields.push((line.trim().to_owned(), String::new()));
                }
            }
        }
    }
    fields
}

/// Takes `[!kind]+ title` off the start of a quote's first paragraph.
fn take_callout(children: &mut Vec<Block>) -> Option<Callout> {
    let first = children.first_mut()?;
    let BlockKind::Paragraph(inlines) = &mut first.kind else {
        return None;
    };
    let rest = inlines.text.strip_prefix("[!")?;
    let close = rest.find(']')?;
    let kind = &rest[..close];
    if kind.is_empty()
        || !kind
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
    {
        return None;
    }
    let kind = kind.to_lowercase();
    let mut consumed = 2 + close + 1;
    let fold = match inlines.text[consumed..].chars().next() {
        Some('+') => {
            consumed += 1;
            CalloutFold::Open
        }
        Some('-') => {
            consumed += 1;
            CalloutFold::Closed
        }
        _ => CalloutFold::None,
    };
    let title_end = inlines.first_break.unwrap_or(inlines.text.len());
    if title_end < consumed {
        return None;
    }
    let title = inlines.text[consumed..title_end].trim().to_owned();
    // The line break goes too.
    let remove = (title_end + 1).min(inlines.text.len());
    inlines.remove_prefix(remove);
    if inlines.is_empty() {
        children.remove(0);
    }
    let title = if title.is_empty() {
        let mut chars = kind.chars();
        chars.next().map_or_else(String::new, |first| {
            first.to_uppercase().chain(chars).collect()
        })
    } else {
        title
    };
    Some(Callout { kind, title, fold })
}

/// What pulldown-cmark doesn't parse: `==highlights==`, `#tags` and `:emoji:`.
fn extras(inlines: &mut Inlines) {
    let is_plain = |run: &Run| {
        !run.style.code && !run.style.math && !run.style.html && !run.style.footnote_ref
    };
    if !inlines.runs.iter().any(|run| {
        is_plain(run)
            && inlines.text[run.range.clone()].contains(['=', '#', ':', '^', '~', 'w', 'W'])
    }) {
        return;
    }
    let highlights = highlight_marks(inlines, &is_plain);
    let mut out = Inlines {
        footnote_refs: std::mem::take(&mut inlines.footnote_refs),
        anchors: std::mem::take(&mut inlines.anchors),
        has_display_math: inlines.has_display_math,
        ..Inlines::default()
    };
    let mut images = std::mem::take(&mut inlines.images).into_iter().peekable();
    let mut highlight = false;
    let mut first_break = inlines.first_break;
    for run in &inlines.runs {
        let text = &inlines.text[run.range.clone()];
        out.after_html = run.after_html;
        if first_break.is_some_and(|at| at == run.range.start) {
            out.first_break = Some(out.text.len());
            first_break = None;
        }
        if !is_plain(run) {
            let style = Style {
                highlight: highlight || run.style.highlight,
                ..run.style
            };
            out.push(text, run.source.clone(), style, run.link.as_ref());
            continue;
        }
        let mut at = 0;
        while at < text.len() {
            let absolute = run.range.start + at;
            let rest = &text[at..];
            if first_break.is_some_and(|at| at == absolute) {
                out.first_break = Some(out.text.len());
                first_break = None;
            }
            if highlights.contains(&absolute) {
                highlight = !highlight;
                at += 2;
                continue;
            }
            let before = inlines.text[..absolute].chars().next_back();
            let style = Style {
                highlight: highlight || run.style.highlight,
                ..run.style
            };
            let link = run.link.as_ref();
            if rest.starts_with(IMAGE_CHAR)
                && let Some(mut image) = images.next_if(|image| image.at == absolute)
            {
                image.at = out.text.len();
                out.images.push(image);
            }
            if let Some(len) = bare_url_len(rest, before).filter(|_| run.link.is_none()) {
                let url = &rest[..len];
                let scheme = if url.to_ascii_lowercase().starts_with("www.") {
                    "https://"
                } else {
                    ""
                };
                let bare = Link {
                    url: format!("{scheme}{url}"),
                    title: String::new(),
                    wiki: false,
                };
                out.push(url, run.source.clone(), style, Some(&bare));
                at += len;
            } else if let Some(len) = tag_len(rest, before) {
                let style = Style { tag: true, ..style };
                out.push(&rest[..len], run.source.clone(), style, link);
                at += len;
            } else if let Some((emoji, len)) = emoji(rest, before) {
                out.push(emoji, run.source.clone(), style, link);
                at += len;
            } else if let Some((inner, len, is_sup)) = script(rest) {
                let style = Style {
                    sup: is_sup,
                    sub: !is_sup,
                    ..style
                };
                out.push(inner, run.source.clone(), style, link);
                at += len;
            } else {
                let c = rest.chars().next().map_or(1, char::len_utf8);
                out.push(&rest[..c], run.source.clone(), style, link);
                at += c;
            }
        }
    }
    if let Some(at) = first_break {
        out.first_break = Some(at.min(out.text.len()));
    }
    *inlines = out;
}

/// Where the `==` that pair up are, in the text.
fn highlight_marks(inlines: &Inlines, is_plain: &impl Fn(&Run) -> bool) -> Vec<usize> {
    let mut marks = Vec::new();
    let mut open: Option<usize> = None;
    for run in inlines.runs.iter().filter(|run| is_plain(run)) {
        let text = &inlines.text[run.range.clone()];
        for (at, _) in text.match_indices("==") {
            let absolute = run.range.start + at;
            // `===` isn't a mark, nor are the ends of what's already paired.
            if marks.last().is_some_and(|&m: &usize| absolute < m + 2)
                || open.is_some_and(|o| absolute < o + 2)
            {
                continue;
            }
            let next = inlines.text[absolute + 2..].chars().next();
            let previous = inlines.text[..absolute].chars().next_back();
            if next == Some('=') || previous == Some('=') {
                continue;
            }
            match open {
                None if next.is_some_and(|c| !c.is_whitespace()) => open = Some(absolute),
                Some(start) if previous.is_some_and(|c| !c.is_whitespace()) => {
                    marks.push(start);
                    marks.push(absolute);
                    open = None;
                }
                _ => {}
            }
        }
    }
    marks
}

/// How long a web address written as plain text at the start of `text` is, as
/// GitHub finds them: pulldown-cmark only takes `<these>`. Punctuation at its
/// end is left out, and so is a `)` it didn't open.
fn bare_url_len(text: &str, before: Option<char>) -> Option<usize> {
    if before.is_some_and(char::is_alphanumeric) {
        return None;
    }
    let lower = text.get(..8).unwrap_or(text).to_ascii_lowercase();
    let prefix = ["https://", "http://", "www."]
        .into_iter()
        .find(|prefix| lower.starts_with(prefix))?;
    let end = text
        .find(|c: char| c.is_whitespace() || c == '<' || c == IMAGE_CHAR)
        .unwrap_or(text.len());
    let mut url = &text[..end];
    loop {
        let trimmed =
            url.trim_end_matches(['.', ',', ':', ';', '!', '?', '\'', '"', '*', '_', '~']);
        let unopened =
            trimmed.ends_with(')') && trimmed.matches(')').count() > trimmed.matches('(').count();
        let shorter = if unopened {
            &trimmed[..trimmed.len() - 1]
        } else {
            trimmed
        };
        if shorter.len() == url.len() {
            break;
        }
        url = shorter;
    }
    (url.len() > prefix.len()).then_some(url.len())
}

/// How long a `#tag` at the start of `text` is. A tag follows a space or
/// starts the text, and has a letter in it: `#1` and `a#b` aren't tags.
fn tag_len(text: &str, before: Option<char>) -> Option<usize> {
    let rest = text.strip_prefix('#')?;
    if before.is_some_and(|c| !c.is_whitespace() && c != '(') {
        return None;
    }
    let len = rest
        .find(|c: char| !(c.is_alphanumeric() || matches!(c, '_' | '-' | '/')))
        .unwrap_or(rest.len());
    let tag = &rest[..len];
    tag.chars().any(char::is_alphabetic).then_some(len + 1)
}

/// `^this^` or `~this~` inside a word, as in H~2~O, which pulldown-cmark only
/// takes at a word's edges. No spaces within. Returns the text in it, how long
/// the whole is, and whether it's a superscript.
fn script(text: &str) -> Option<(&str, usize, bool)> {
    let mark = text.chars().next().filter(|c| matches!(c, '^' | '~'))?;
    let rest = &text[1..];
    let len = rest.find(mark)?;
    let inner = &rest[..len];
    (!inner.is_empty() && !inner.contains(char::is_whitespace)).then_some((
        inner,
        len + 2,
        mark == '^',
    ))
}

/// The emoji for a `:shortcode:` at the start of `text`, and how long it is.
fn emoji(text: &str, before: Option<char>) -> Option<(&'static str, usize)> {
    let rest = text.strip_prefix(':')?;
    if before.is_some_and(char::is_alphanumeric) {
        return None;
    }
    let len = rest.find(':')?;
    let code = &rest[..len];
    if code.is_empty()
        || !code
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '+' | '-'))
    {
        return None;
    }
    Some((emojis::get_by_shortcode(code)?.as_str(), len + 2))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paragraph(doc: &Document) -> &Inlines {
        match &doc.blocks[0].kind {
            BlockKind::Paragraph(inlines) => inlines,
            other => panic!("expected a paragraph, got {other:?}"),
        }
    }

    fn styled(inlines: &Inlines, of: impl Fn(&Style) -> bool) -> Vec<&str> {
        inlines
            .runs
            .iter()
            .filter(|run| of(&run.style))
            .map(|run| &inlines.text[run.range.clone()])
            .collect()
    }

    #[test]
    fn inline_formatting_flattens_into_runs() {
        let doc = parse("a **b *c*** ~~d~~ `e` H~2~O x^2^");
        let inlines = paragraph(&doc);
        assert_eq!(inlines.text, "a b c d e H2O x2");
        assert_eq!(styled(inlines, |s| s.bold), ["b ", "c"]);
        assert_eq!(styled(inlines, |s| s.italic), ["c"]);
        assert_eq!(styled(inlines, |s| s.strike), ["d"]);
        assert_eq!(styled(inlines, |s| s.code), ["e"]);
        assert_eq!(styled(inlines, |s| s.sub), ["2"]);
        assert_eq!(styled(inlines, |s| s.sup), ["2"]);
    }

    #[test]
    fn words_are_counted_in_what_is_read() {
        let stats = |source: &str| parse(source).stats().words;
        assert_eq!(stats(""), 0);
        assert_eq!(stats("one two, three-four don't"), 4);
        assert_eq!(
            stats("**bold** [link text](http://x.y/z/w)"),
            3,
            "not the address"
        );
        assert_eq!(stats("---\ntitle: skipped words\n---\n\nbody"), 1);
        assert_eq!(
            stats("# Head\n\n- a\n- b\n\n| c | d |\n|---|---|\n| e | f |\n"),
            7
        );
        assert_eq!(stats("> [!note] Title here\n> body"), 3);
        assert_eq!(
            stats("text[^n]\n\n[^n]: in a note"),
            4,
            "the reference isn't a word"
        );
        assert_eq!(stats("日本語 中文"), 5, "no spaces between words there");
        assert_eq!(stats("![alt words](p.png)"), 0);
        assert_eq!(parse("ab cd").stats().chars, 4);
    }

    #[test]
    fn reading_time_rounds_up() {
        let minutes = |words| Stats { words, chars: 0 }.minutes();
        assert_eq!(minutes(0), 1);
        assert_eq!(minutes(200), 1);
        assert_eq!(minutes(201), 2);
    }

    #[test]
    fn scripts_work_at_word_edges_and_inside_words() {
        let doc = parse("~a~ ^b^ H~2~O x^2^ but not a~b c~d or 2^3 4^5");
        let inlines = paragraph(&doc);
        assert_eq!(inlines.text, "a b H2O x2 but not a~b c~d or 2^3 4^5");
        assert_eq!(styled(inlines, |s| s.sub), ["a", "2"]);
        assert_eq!(styled(inlines, |s| s.sup), ["b", "2"]);
    }

    #[test]
    fn tight_list_items_keep_what_they_start_with() {
        let doc = parse(
            "- [a link](x.md) then\n- **bold** then\n- *it* then\n- ![pic](p.png) then\n- ~~s~~\n",
        );
        let BlockKind::List { items, .. } = &doc.blocks[0].kind else {
            panic!("expected a list");
        };
        let first = |index: usize| match &items[index].children[..] {
            [
                Block {
                    kind: BlockKind::Paragraph(inlines),
                    ..
                },
            ] => inlines,
            other => panic!("expected one paragraph, got {other:?}"),
        };
        assert_eq!(first(0).runs[0].link.as_ref().unwrap().url, "x.md");
        assert_eq!(first(0).text, "a link then");
        assert_eq!(styled(first(1), |s| s.bold), ["bold"]);
        assert_eq!(styled(first(2), |s| s.italic), ["it"]);
        assert_eq!(first(3).images.len(), 1);
        assert_eq!(styled(first(4), |s| s.strike), ["s"]);
        let doc = parse("- [ ] **bold task** then\n");
        let BlockKind::List { items, .. } = &doc.blocks[0].kind else {
            panic!("expected a list");
        };
        assert!(items[0].task.is_some());
    }

    #[test]
    fn bare_addresses_become_links() {
        let links = |source: &str| -> Vec<(String, String)> {
            let doc = parse(source);
            let inlines = paragraph(&doc);
            inlines
                .runs
                .iter()
                .filter_map(|run| {
                    let link = run.link.as_ref()?;
                    Some((inlines.text[run.range.clone()].to_owned(), link.url.clone()))
                })
                .collect()
        };
        let pair = |text: &str, url: &str| (text.to_owned(), url.to_owned());
        assert_eq!(
            links("See https://a.com/b, (http://c.com/d) and www.e.com."),
            [
                pair("https://a.com/b", "https://a.com/b"),
                pair("http://c.com/d", "http://c.com/d"),
                pair("www.e.com", "https://www.e.com"),
            ]
        );
        assert_eq!(
            links("https://en.wikipedia.org/wiki/A_(b) ok"),
            [pair(
                "https://en.wikipedia.org/wiki/A_(b)",
                "https://en.wikipedia.org/wiki/A_(b)"
            )]
        );
        // Not addresses, or already something else.
        for source in [
            "xhttps://a.com",
            "www.",
            "https://",
            "`https://a.com`",
            "wow www",
        ] {
            assert_eq!(links(source), [], "{source}");
        }
        assert_eq!(
            links("[t](https://a.com/x) text"),
            [pair("t", "https://a.com/x")]
        );
        assert_eq!(
            links("[https://shown.com](https://b.com)"),
            [pair("https://shown.com", "https://b.com")]
        );
    }

    #[test]
    fn blocks_know_their_source() {
        let source = "# One\n\nSome text\n\n- a\n- b\n";
        let doc = parse(source);
        assert_eq!(doc.blocks.len(), 3);
        assert_eq!(&source[doc.blocks[0].source.clone()], "# One\n");
        assert_eq!(&source[doc.blocks[1].source.clone()], "Some text\n");
        assert!(matches!(doc.blocks[2].kind, BlockKind::List { .. }));
    }

    #[test]
    fn heading_slugs_are_like_githubs_and_unique() {
        let doc = parse("# Hello, World!\n\n## Hello, World!\n\n## a_b-c {#custom}\n");
        let slugs: Vec<_> = doc.headings.iter().map(|h| h.slug.as_str()).collect();
        assert_eq!(slugs, ["hello-world", "hello-world-1", "custom"]);
        assert_eq!(doc.headings[1].level, 2);
    }

    #[test]
    fn task_markers_point_at_their_source() {
        let source = "- [ ] open\n- [x] done\n- plain\n";
        let doc = parse(source);
        let BlockKind::List { items, .. } = &doc.blocks[0].kind else {
            panic!("expected a list");
        };
        let open = items[0].task.as_ref().unwrap();
        assert!(!open.checked);
        assert_eq!(&source[open.source.clone()], "[ ]");
        let done = items[1].task.as_ref().unwrap();
        assert!(done.checked);
        assert_eq!(&source[done.source.clone()], "[x]");
        assert!(items[2].task.is_none());
    }

    #[test]
    fn tight_and_loose_list_items_both_hold_paragraphs() {
        for source in ["- a\n- b\n", "- a\n\n- b\n"] {
            let doc = parse(source);
            let BlockKind::List { items, .. } = &doc.blocks[0].kind else {
                panic!("expected a list");
            };
            assert_eq!(items.len(), 2, "{source:?}");
            assert!(matches!(items[1].children[0].kind, BlockKind::Paragraph(_)));
        }
    }

    #[test]
    fn nested_lists_stay_nested() {
        let doc = parse("1. a\n   - b\n     - c\n2. d\n");
        let BlockKind::List { start, items } = &doc.blocks[0].kind else {
            panic!("expected a list");
        };
        assert_eq!(*start, Some(1));
        assert_eq!(items.len(), 2);
        assert!(matches!(items[0].children[1].kind, BlockKind::List { .. }));
    }

    #[test]
    fn tables_have_alignments_and_cells() {
        let doc = parse("| a | b |\n|:--|--:|\n| 1 | **2** |\n| 3 |\n");
        let BlockKind::Table { aligns, head, rows } = &doc.blocks[0].kind else {
            panic!("expected a table");
        };
        assert_eq!(aligns, &[Alignment::Left, Alignment::Right]);
        assert_eq!(head[1].text, "b");
        assert_eq!(rows.len(), 2);
        assert_eq!(styled(&rows[0][1], |s| s.bold), ["2"]);
    }

    #[test]
    fn code_blocks_keep_their_text_and_language() {
        let doc = parse("```rust title=x\nfn a() {}\n```\n\n    indented\n");
        let BlockKind::Code { lang, code } = &doc.blocks[0].kind else {
            panic!("expected code");
        };
        assert_eq!(lang.as_deref(), Some("rust"));
        assert_eq!(code, "fn a() {}\n");
        let BlockKind::Code { lang, code } = &doc.blocks[1].kind else {
            panic!("expected code");
        };
        assert_eq!(*lang, None);
        assert_eq!(code, "indented\n");
    }

    #[test]
    fn frontmatter_becomes_fields() {
        let doc = parse("---\ntitle: \"A note\"\ntags:\n  - one\n  - two\n---\n\nBody\n");
        let BlockKind::Frontmatter(fields) = &doc.blocks[0].kind else {
            panic!("expected frontmatter, got {:?}", doc.blocks[0].kind);
        };
        assert_eq!(fields[0], ("title".to_owned(), "A note".to_owned()));
        assert_eq!(fields[1], ("tags".to_owned(), "one, two".to_owned()));
    }

    #[test]
    fn footnotes_are_numbered_by_first_reference() {
        let doc = parse("A[^b] and[^a] again[^b].\n\n[^a]: First.\n[^b]: Second.\n[^c]: Unused.\n");
        let numbers: Vec<_> = doc
            .footnotes
            .iter()
            .map(|f| (f.label.as_str(), f.number))
            .collect();
        assert_eq!(numbers, [("b", 1), ("a", 2), ("c", 3)]);
        assert_eq!(doc.blocks.len(), 1);
        let inlines = paragraph(&doc);
        assert_eq!(styled(inlines, |s| s.footnote_ref), ["1", "2", "1"]);
    }

    #[test]
    fn alerts_and_callouts_lose_their_marker() {
        let doc = parse("> [!NOTE]\n> Body text\n");
        let BlockKind::Quote { callout, children } = &doc.blocks[0].kind else {
            panic!("expected a quote");
        };
        let callout = callout.as_ref().unwrap();
        assert_eq!(callout.kind, "note");
        assert_eq!(callout.title, "Note");
        assert_eq!(callout.fold, CalloutFold::None);
        let BlockKind::Paragraph(body) = &children[0].kind else {
            panic!("expected a paragraph");
        };
        assert_eq!(body.text, "Body text");

        let doc = parse("> [!tip]- Read this\n> Body\n");
        let BlockKind::Quote { callout, .. } = &doc.blocks[0].kind else {
            panic!("expected a quote");
        };
        let callout = callout.as_ref().unwrap();
        assert_eq!(callout.title, "Read this");
        assert_eq!(callout.fold, CalloutFold::Closed);

        let doc = parse("> [!warning]\n");
        let BlockKind::Quote { callout, children } = &doc.blocks[0].kind else {
            panic!("expected a quote");
        };
        assert!(callout.is_some());
        assert!(children.is_empty());
    }

    #[test]
    fn a_plain_quote_is_not_a_callout() {
        for source in ["> Just text\n", "> [not one]\n", "> [!not one]\n"] {
            let doc = parse(source);
            let BlockKind::Quote { callout, .. } = &doc.blocks[0].kind else {
                panic!("expected a quote");
            };
            assert_eq!(*callout, None, "{source:?}");
        }
    }

    #[test]
    fn highlights_pair_up_and_stay_out_of_code() {
        let doc = parse("a ==b c== d `==e==` ==f");
        let inlines = paragraph(&doc);
        assert_eq!(inlines.text, "a b c d ==e== ==f");
        assert_eq!(styled(inlines, |s| s.highlight), ["b c"]);
        // Not marks: a comparison, and a setext-looking line in a sentence.
        let doc = parse("x == y and a === b");
        assert_eq!(paragraph(&doc).text, "x == y and a === b");
    }

    #[test]
    fn tags_need_a_letter_and_a_space_before() {
        let doc = parse("#todo in a#b issue #12 and #sub/tag.");
        let inlines = paragraph(&doc);
        assert_eq!(styled(inlines, |s| s.tag), ["#todo", "#sub/tag"]);
    }

    #[test]
    fn emoji_shortcodes_become_emoji() {
        let doc = parse("Ship it :rocket: at 10:30:15 or :not_an_emoji:");
        assert_eq!(
            paragraph(&doc).text,
            "Ship it 🚀 at 10:30:15 or :not_an_emoji:"
        );
    }

    #[test]
    fn wikilinks_are_marked() {
        let doc = parse("See [[Other note|that note]] and [[Plain]].");
        let inlines = paragraph(&doc);
        assert_eq!(inlines.text, "See that note and Plain.");
        let links: Vec<_> = inlines
            .runs
            .iter()
            .filter_map(|run| run.link.as_ref())
            .collect();
        assert_eq!(links[0].url, "Other note");
        assert!(links[0].wiki);
        assert_eq!(links[1].url, "Plain");
    }

    #[test]
    fn images_sit_in_the_text_and_keep_their_place_through_extras() {
        let doc = parse("==a== ![alt *text*](x.png \"title\") :rocket:");
        let inlines = paragraph(&doc);
        assert_eq!(inlines.images.len(), 1);
        let image = &inlines.images[0];
        assert_eq!(image.alt, "alt text");
        assert_eq!(image.title, "title");
        assert!(inlines.text[image.at..].starts_with(IMAGE_CHAR));
        assert!(!doc.has_remote_images);
        assert!(parse("![](HTTPS://x/y.png)").has_remote_images);
    }

    #[test]
    fn linked_images_keep_the_link() {
        let doc = parse("[![badge](b.svg)](https://example.com)");
        let inlines = paragraph(&doc);
        assert_eq!(inlines.images.len(), 1);
        assert_eq!(inlines.runs.len(), 1);
        assert_eq!(
            inlines.runs[0].link.as_ref().map(|l| l.url.as_str()),
            Some("https://example.com")
        );
    }

    #[test]
    fn math_is_marked_and_a_lone_display_formula_is_a_block() {
        let doc = parse("Euler: $e^{i\\pi}+1=0$ here");
        assert_eq!(styled(paragraph(&doc), |s| s.math), ["e^{i\\pi}+1=0"]);
        let doc = parse("$$\nx^2\n$$\n");
        assert!(matches!(&doc.blocks[0].kind, BlockKind::Math(m) if m.trim() == "x^2"));
        // Inline math alone in a paragraph stays inline.
        let doc = parse("$x$\n");
        assert!(matches!(doc.blocks[0].kind, BlockKind::Paragraph(_)));
    }

    #[test]
    fn definition_lists_have_terms_and_details() {
        let doc = parse("Term\n: One\n: Two\n");
        let BlockKind::Definitions(definitions) = &doc.blocks[0].kind else {
            panic!("expected definitions, got {:?}", doc.blocks[0].kind);
        };
        assert_eq!(definitions[0].term.text, "Term");
        assert_eq!(definitions[0].details.len(), 2);
    }

    #[test]
    fn no_block_is_left_as_html() {
        fn check(blocks: &[Block]) {
            for block in blocks {
                assert!(!matches!(block.kind, BlockKind::Html(_)), "{block:?}");
            }
        }
        let source = "<table><tr><td>a</td></tr></table>\n\n<details>b</details>\n\n<!-- c -->\n";
        check(&parse(source).blocks);
    }

    fn table_rows(block: &Block) -> &[HtmlRow] {
        match &unit(block).kind {
            BlockKind::HtmlTable(rows) => rows,
            other => panic!("expected a table: {other:?}"),
        }
    }

    fn cell_texts(rows: &[HtmlRow]) -> Vec<Vec<&str>> {
        rows.iter()
            .map(|row| row.cells.iter().map(|c| c.text.text.as_str()).collect())
            .collect()
    }

    #[test]
    fn html_tables() {
        let doc = parse(concat!(
            "<table>\n<thead>\n<tr><th>a</th><th align=right>b</th></tr>\n</thead>\n",
            "<tbody>\n<tr align=center><td colspan=\"2\"><b>c</b></td></tr>\n",
            "<tr><td>d<br>e</td><td><a href=x><img src=i></a></td></tr>\n</tbody>\n</table>\n",
        ));
        let rows = table_rows(&doc.blocks[0]);
        assert_eq!(
            cell_texts(rows),
            [vec!["a", "b"], vec!["c"], vec!["d\ne", "\u{FFFC}"]]
        );
        assert!(rows[0].cells.iter().all(|cell| cell.header));
        assert_eq!(rows[0].cells[1].align, Some(Align::Right));
        assert_eq!(rows[1].cells[0].colspan, 2);
        assert_eq!(rows[1].cells[0].align, Some(Align::Center));
        assert!(!rows[1].cells[0].header);
        assert_eq!(rows[2].cells[1].text.images.len(), 1);
    }

    #[test]
    fn html_table_tags_may_be_left_out() {
        let doc = parse("<table><tr><td>a<td>b<tr><th>c</table>\n\n<table><td>d</td></table>\n");
        assert_eq!(
            cell_texts(table_rows(&doc.blocks[0])),
            [vec!["a", "b"], vec!["c"]]
        );
        assert_eq!(cell_texts(table_rows(&doc.blocks[1])), [vec!["d"]]);
    }

    #[test]
    fn block_tags_in_a_cell_are_its_lines() {
        let doc =
            parse("<table><tr><td><p>a</p><div>b</div><ul><li>c</li></ul></td></tr></table>\n");
        assert_eq!(cell_texts(table_rows(&doc.blocks[0])), [vec!["a\nb\nc"]]);
    }

    #[test]
    fn markdown_in_a_cell() {
        let doc = parse("<table>\n<tr>\n<td>\n\n**a**\n\n</td>\n</tr>\n</table>\n");
        let rows = table_rows(&doc.blocks[0]);
        assert_eq!(cell_texts(rows), [vec!["a"]]);
        assert_eq!(styled(&rows[0].cells[0].text, |s| s.bold), ["a"]);
    }

    #[test]
    fn what_a_cell_cannot_hold_comes_before_the_table() {
        let doc = parse("<table>\n<tr>\n<td>\n\n```\nx\n```\n\n</td>\n</tr>\n</table>\n");
        assert!(matches!(doc.blocks[0].kind, BlockKind::Code { .. }));
        assert_eq!(cell_texts(table_rows(&doc.blocks[1])), [vec![""]]);
    }

    #[test]
    fn cells_outside_a_table_are_just_text() {
        let doc = parse("<tr><td>a</td></tr>\n");
        assert_eq!(html_text(&doc.blocks[0]), "a");
    }

    #[test]
    fn colspans_are_kept_in_reason() {
        for (written, colspan) in [("0", 1), ("x", 1), ("99999999", MAX_COLSPAN), ("3", 3)] {
            let doc = parse(&format!("<table><td colspan={written}>a</table>"));
            assert_eq!(table_rows(&doc.blocks[0])[0].cells[0].colspan, colspan);
        }
    }

    fn details(block: &Block) -> (&Inlines, bool, usize, &[Block]) {
        match &block.kind {
            BlockKind::Details {
                summary,
                open,
                key,
                children,
            } => (summary, *open, *key, children),
            other => panic!("expected details: {other:?}"),
        }
    }

    #[test]
    fn details_hold_markdown_between_their_tags() {
        let doc = parse(
            "<details>\n<summary>Some <b>title</b></summary>\n\nHidden **text**.\n\n</details>\n\nAfter\n",
        );
        assert_eq!(doc.blocks.len(), 2);
        let (summary, open, _, children) = details(&doc.blocks[0]);
        assert_eq!(summary.text, "Some title");
        assert_eq!(styled(summary, |s| s.bold), ["title"]);
        assert!(!open);
        assert!(matches!(&children[0].kind, BlockKind::Paragraph(i) if i.text == "Hidden text."));
    }

    #[test]
    fn details_can_start_open_and_each_has_its_own_key() {
        let doc = parse(
            "<details open><summary>a</summary>b</details>\n<details><summary>c</summary>d</details>\n",
        );
        assert_eq!(doc.blocks.len(), 2);
        let (first, second) = (details(&doc.blocks[0]), details(&doc.blocks[1]));
        assert!(first.1);
        assert!(!second.1);
        assert_ne!(first.2, second.2);
        assert_eq!(html_text(&first.3[0]), "b");
    }

    #[test]
    fn nested_details_and_ones_without_a_summary() {
        let doc = parse(
            "<details><details><summary>in</summary>x</details></details>\n\n<details>y</details>\n",
        );
        let (outer, _, _, children) = details(&doc.blocks[0]);
        assert!(outer.is_empty());
        assert_eq!(details(&children[0]).0.text, "in");
        assert!(details(&doc.blocks[1]).0.is_empty());
    }

    #[test]
    fn stats_count_what_shows() {
        let doc = parse(
            "<table><tr><th title=\"no\">one</th><td>two</td></tr></table>\n\n<details><summary>three</summary>four</details>\n",
        );
        assert_eq!(doc.stats().words, 4);
    }

    fn unit(block: &Block) -> &Block {
        match &block.kind {
            BlockKind::HtmlUnit(inner) => inner,
            other => panic!("expected a unit: {other:?}"),
        }
    }

    fn html_text(block: &Block) -> &str {
        match &unit(block).kind {
            BlockKind::Paragraph(inlines) => &inlines.text,
            other => panic!("expected a paragraph: {other:?}"),
        }
    }

    fn aligned(block: &Block) -> (Align, &[Block]) {
        match &block.kind {
            BlockKind::Aligned { align, children } => (*align, children),
            other => panic!("expected aligned blocks: {other:?}"),
        }
    }

    #[test]
    fn a_div_is_transparent_and_paragraphs_are_blocks() {
        let doc = parse("<div>\n<p>a <b>b</b></p>\n<p>c\n<p>d\n</div>\n");
        let texts: Vec<_> = doc.blocks.iter().map(html_text).collect();
        assert_eq!(texts, ["a b", "c", "d"]);
    }

    #[test]
    fn alignment_covers_what_is_inside() {
        let doc = parse(
            "<div align=\"center\">\n  <img src=\"a.png\">\n  <p>c</p>\n</div>\n\n<center>x</center>\n\n<p align=right>y</p>\n",
        );
        assert_eq!(doc.blocks.len(), 3);
        let (align, children) = aligned(&doc.blocks[0]);
        assert_eq!(align, Align::Center);
        assert_eq!(children.len(), 2);
        assert_eq!(html_text(&children[1]), "c");
        assert_eq!(aligned(&doc.blocks[1]).0, Align::Center);
        let (align, children) = aligned(&doc.blocks[2]);
        assert_eq!(align, Align::Right);
        assert_eq!(html_text(&children[0]), "y");
    }

    #[test]
    fn markdown_between_html_tags_stays_markdown() {
        let doc = parse("<div align=\"center\">\n\n**a**\n\n</div>\n\nb\n");
        assert_eq!(doc.blocks.len(), 2);
        let (_, children) = aligned(&doc.blocks[0]);
        let BlockKind::Paragraph(inlines) = &children[0].kind else {
            panic!("expected a markdown paragraph");
        };
        assert_eq!(styled(inlines, |s| s.bold), ["a"]);
    }

    #[test]
    fn markdown_with_no_blank_lines_in_html_is_literal() {
        let doc = parse("<div>\n**a**\n</div>\n");
        assert_eq!(html_text(&doc.blocks[0]), "**a**");
    }

    #[test]
    fn a_block_left_open_ends_with_its_markdown_container() {
        let doc = parse("> <div align=right>\n>\n> a\n\nb\n");
        assert_eq!(doc.blocks.len(), 2);
        let BlockKind::Quote { children, .. } = &doc.blocks[0].kind else {
            panic!("expected a quote");
        };
        assert_eq!(aligned(&children[0]).0, Align::Right);
        assert!(matches!(doc.blocks[1].kind, BlockKind::Paragraph(_)));
    }

    #[test]
    fn html_headings_join_the_outline_in_order() {
        let doc =
            parse("# one\n\n<h2 align=center>two <i>x</i></h2>\n\n<h3 id=z>three</h3>\n\n# four\n");
        let names: Vec<_> = doc.headings.iter().map(|h| h.text.as_str()).collect();
        assert_eq!(names, ["one", "two x", "three", "four"]);
        assert_eq!(doc.headings[1].slug, "two-x");
        assert_eq!(doc.headings[2].slug, "z");
        let (_, children) = aligned(&doc.blocks[1]);
        assert!(matches!(
            unit(&children[0]).kind,
            BlockKind::Heading { level: 2, .. }
        ));
    }

    #[test]
    fn html_lists() {
        let doc = parse("<ol start=3>\n<li>a\n<li>b<ul><li>c</li></ul></li>\n</ol>\n");
        let BlockKind::List { start, items } = &doc.blocks[0].kind else {
            panic!("expected a list");
        };
        assert_eq!(*start, Some(3));
        assert_eq!(items.len(), 2);
        assert_eq!(html_text(&items[0].children[0]), "a");
        assert!(matches!(items[1].children[1].kind, BlockKind::List { .. }));
    }

    #[test]
    fn html_quotes_rules_and_pre() {
        let doc = parse(
            "<blockquote>a</blockquote>\n\n<hr>\n\n<pre lang=\"rs\">\n  x <y>&amp;\n\n  z</pre>\n",
        );
        assert!(matches!(doc.blocks[0].kind, BlockKind::Quote { .. }));
        assert!(matches!(doc.blocks[1].kind, BlockKind::Rule));
        let BlockKind::Code { lang, code } = &unit(&doc.blocks[2]).kind else {
            panic!("expected code");
        };
        assert_eq!(lang.as_deref(), Some("rs"));
        assert_eq!(code, "  x &\n\n  z");
    }

    #[test]
    fn comments_among_block_tags_do_not_show() {
        let doc = parse("<div>\n<!-- a -->\n<p>b</p>\n</div>\n");
        assert_eq!(doc.blocks.len(), 1);
        assert_eq!(html_text(&doc.blocks[0]), "b");
    }

    #[test]
    fn stray_block_tags_do_nothing() {
        let doc = parse("</div>\n\nb\n");
        assert_eq!(doc.blocks.len(), 1);
        assert!(matches!(doc.blocks[0].kind, BlockKind::Paragraph(_)));
    }

    #[test]
    fn images_to_the_side_get_lines_of_their_own() {
        let doc = parse("before <img src=a align=right> after\n");
        let [first, image, last] = doc.blocks.as_slice() else {
            panic!("expected three blocks: {:?}", doc.blocks);
        };
        assert!(matches!(&first.kind, BlockKind::Paragraph(i) if i.text == "before "));
        let (align, children) = aligned(image);
        assert_eq!(align, Align::Right);
        assert!(
            matches!(&children[0].kind, BlockKind::Paragraph(i) if i.images.len() == 1 && i.images[0].at == 0)
        );
        assert!(matches!(&last.kind, BlockKind::Paragraph(i) if i.text == " after"));
    }

    #[test]
    fn images_to_the_side_in_a_block_of_html() {
        let doc = parse("<img src=a align=\"left\">\ntext\n");
        assert_eq!(doc.blocks.len(), 2);
        assert_eq!(aligned(unit(&doc.blocks[0])).0, Align::Left);
        assert_eq!(html_text(&doc.blocks[1]), "text");
    }

    #[test]
    fn a_picture_has_an_image_for_each_theme() {
        let doc = parse(concat!(
            "<picture>\n",
            "<source media=\"(prefers-color-scheme: dark)\" srcset=\"d.png 2x, e.png\">\n",
            "<source media=\"(prefers-color-scheme: light)\" srcset=\"l.png\">\n",
            "<img src=\"f.png\" alt=\"A\">\n",
            "</picture>\n",
        ));
        let BlockKind::Paragraph(inlines) = &unit(&doc.blocks[0]).kind else {
            panic!("expected a paragraph");
        };
        let image = &inlines.images[0];
        assert_eq!(image.url_for(true), Some("d.png"));
        assert_eq!(image.url_for(false), Some("l.png"));
        assert_eq!(image.url, "f.png");
        assert!(!doc.has_remote_images);
    }

    #[test]
    fn a_picture_in_a_line_and_remote_sources() {
        let doc = parse(
            "a <picture><source media=\"(prefers-color-scheme: dark)\" srcset=\"https://x/d.png\"><img src=\"f.png\"></picture> <img src=g.png>",
        );
        let inlines = paragraph(&doc);
        assert_eq!(inlines.images[0].url_for(true), Some("https://x/d.png"));
        assert_eq!(inlines.images[0].url_for(false), Some("f.png"));
        assert_eq!(inlines.images[1].url_for(true), Some("g.png"));
        assert!(doc.has_remote_images);
    }

    #[test]
    fn images_marked_for_a_theme() {
        let doc = parse("![a](d.png#gh-dark-mode-only) <img src=\"l.png#gh-light-mode-only\">");
        let images = &paragraph(&doc).images;
        assert_eq!(images[0].url_for(true), Some("d.png"));
        assert_eq!(images[0].url_for(false), None);
        assert_eq!(images[1].url_for(true), None);
        assert_eq!(images[1].url_for(false), Some("l.png"));
    }

    #[test]
    fn inline_html_styles_the_text_between_its_tags() {
        let doc = parse(
            "<b>a</b> <i>b</i> <kbd>c</kbd> <sub>d</sub> <sup>e</sup> <u>f</u> <s>g</s> <mark>h</mark>",
        );
        let inlines = paragraph(&doc);
        assert_eq!(inlines.text, "a b c d e f g h");
        assert_eq!(styled(inlines, |s| s.bold), ["a"]);
        assert_eq!(styled(inlines, |s| s.italic), ["b"]);
        assert_eq!(styled(inlines, |s| s.code), ["c"]);
        assert_eq!(styled(inlines, |s| s.sub), ["d"]);
        assert_eq!(styled(inlines, |s| s.sup), ["e"]);
        assert_eq!(styled(inlines, |s| s.underline), ["f"]);
        assert_eq!(styled(inlines, |s| s.strike), ["g"]);
        assert_eq!(styled(inlines, |s| s.highlight), ["h"]);
    }

    #[test]
    fn inline_html_mixes_with_markdown() {
        let doc = parse("**a <i>b</i>** <b>*c*</b>");
        let inlines = paragraph(&doc);
        assert_eq!(styled(inlines, |s| s.bold && s.italic), ["b", "c"]);
    }

    #[test]
    fn unknown_tags_go_and_their_content_stays() {
        let doc = parse("a <font color=red>b</font> <span>c</span>");
        assert_eq!(paragraph(&doc).text, "a b c");
    }

    #[test]
    fn hidden_tags_take_their_content_with_them() {
        let doc = parse("a<style>b *c*</style>d");
        assert_eq!(paragraph(&doc).text, "ad");
    }

    #[test]
    fn comments_in_a_line_do_not_show() {
        for source in ["a<!-- b -->c", "a<!-- b\nb -->c", "> a<!-- b\n> b -->c"] {
            let doc = parse(source);
            let inlines = match &doc.blocks[0].kind {
                BlockKind::Quote { children, .. } => match &children[0].kind {
                    BlockKind::Paragraph(inlines) => inlines,
                    _ => panic!("expected a paragraph"),
                },
                BlockKind::Paragraph(inlines) => inlines,
                _ => panic!("expected a paragraph"),
            };
            assert_eq!(inlines.text, "ac", "{source}");
        }
    }

    #[test]
    fn comment_blocks_are_not_blocks() {
        let doc = parse("a\n\n<!-- b\n\nc -->\n\nd\n");
        assert_eq!(doc.blocks.len(), 2);
        assert_eq!(doc.stats().words, 2);
    }

    #[test]
    fn unclosed_tags_end_with_their_paragraph() {
        let doc = parse("<b>a\n\nb</i>\n");
        let BlockKind::Paragraph(first) = &doc.blocks[0].kind else {
            panic!("expected a paragraph");
        };
        let BlockKind::Paragraph(second) = &doc.blocks[1].kind else {
            panic!("expected a paragraph");
        };
        assert_eq!(styled(first, |s| s.bold), ["a"]);
        assert!(styled(second, |s| s.bold || s.italic).is_empty());
    }

    #[test]
    fn line_breaks_from_html() {
        let doc = parse("a<br>b<br/>c<br>");
        let inlines = paragraph(&doc);
        assert_eq!(inlines.text, "a\nb\nc");
    }

    #[test]
    fn html_links_and_anchors() {
        let doc = parse("<a id=top></a><a href=\"x.md\" title=T>y</a> z");
        let inlines = paragraph(&doc);
        assert_eq!(inlines.anchors, ["top"]);
        let link = inlines.runs[0].link.as_ref().expect("a link");
        assert_eq!((link.url.as_str(), link.title.as_str()), ("x.md", "T"));
        assert_eq!(inlines.runs[1].link, None);
    }

    #[test]
    fn html_images_are_images() {
        let doc = parse("a <a href=l><img src=\"p.png\" alt=\"P\" width=\"50%\" height=20></a>");
        let inlines = paragraph(&doc);
        let image = &inlines.images[0];
        assert_eq!((image.url.as_str(), image.alt.as_str()), ("p.png", "P"));
        assert_eq!(image.width, Some(Length::Percent(50.0)));
        assert_eq!(image.height, Some(Length::Points(20.0)));
        assert!(inlines.text[image.at..].starts_with(IMAGE_CHAR));
        let run = inlines.runs.last().expect("a run");
        assert_eq!(run.link.as_ref().map(|l| l.url.as_str()), Some("l"));
        assert!(!doc.has_remote_images);
        assert!(parse("a <img src=https://x/y.png>").has_remote_images);
    }

    #[test]
    fn an_image_in_code_is_still_an_image() {
        let doc = parse("<code><img src=a.png></code>");
        assert_eq!(paragraph(&doc).images.len(), 1);
    }

    #[test]
    fn runs_do_not_span_what_is_hidden() {
        let doc = parse("x<span>a</span>y");
        let inlines = paragraph(&doc);
        assert_eq!(inlines.text, "xay");
        let sources: Vec<_> = inlines.runs.iter().map(|r| r.source.clone()).collect();
        assert_eq!(sources, [0..1, 7..8, 15..16]);
    }

    #[test]
    fn a_block_of_inline_html_is_a_line_of_text() {
        let doc = parse("<img src=\"a.png\">\n\n<span>\n  one\n  <b>two</b> &amp; x\n</span>\n");
        let BlockKind::Paragraph(image) = &unit(&doc.blocks[0]).kind else {
            panic!("expected a line of HTML");
        };
        assert_eq!(image.images.len(), 1);
        let BlockKind::Paragraph(text) = &unit(&doc.blocks[1]).kind else {
            panic!("expected a line of HTML");
        };
        assert_eq!(text.text, "one two & x");
        assert_eq!(styled(text, |s| s.bold), ["two"]);
    }

    #[test]
    fn markdown_in_a_line_of_html_is_literal() {
        let doc = parse("<span>\n**a**\n</span>\n");
        assert_eq!(html_text(&doc.blocks[0]), "**a**");
    }

    #[test]
    fn deep_nesting_is_flattened_rather_than_overflowing() {
        let source = format!("{}deep\n", ">".repeat(50_000));
        let doc = parse(&source);
        assert!(!doc.blocks.is_empty());
        let source = format!(
            "<div>\n{}x\n",
            "<blockquote><div align=center><ul><li>".repeat(20_000)
        );
        assert!(!parse(&source).blocks.is_empty());
        let source = "- a\n".repeat(10) + &"  ".repeat(5_000) + "- b\n";
        let _ = parse(&source);
    }

    #[test]
    fn hostile_input_does_not_panic() {
        for source in [
            "",
            "\n",
            "[",
            "]]",
            "[[",
            "==",
            "====",
            "$$",
            "|",
            "| a |\n|---|\n",
            "---\n",
            "---\n---\n",
            ":",
            "::",
            "#",
            "> [!",
            "> [!]",
            "> [!a]-",
            "- [ ]",
            "[^a]",
            "[^a]:",
            "![",
            "![]()",
            "é==é==é #é :é:",
            "\u{FFFC}",
            "==\u{FFFC}== ![a](b)",
            "<",
            "<>",
            "</",
            "<!--",
            "<br",
            "<img",
            "<img src>",
            "<a href>x",
            "</a></a>",
            "<b><i></b></i>",
            "<script>",
            "<img src=\u{FFFC}>\u{FFFC}",
            "<img width=9999999999999 src=a>",
            "<div",
            "</div></div>",
            "<ul><li>",
            "<li>a</ol>",
            "<pre>",
            "<h1>",
            "<h1>\n\na\n\n</h1>",
            "<picture><source>",
            "<table>",
            "<table><td>",
            "<td>",
            "</td></tr></table>",
            "<table><td colspan=-1>",
            "<table><tr></table>",
            "<table><col><caption>a</caption></table>",
            "<details>",
            "<summary>a</summary>",
            "<details><summary>",
            "</details>",
            "<source srcset=>",
            "a<img src=a align=right>",
            "<img src=a align=right><img src=b align=left>",
        ] {
            let _ = parse(source);
        }
    }
}
