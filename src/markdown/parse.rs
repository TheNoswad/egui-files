//! Markdown text to a tree of blocks, each knowing which bytes of the source
//! it came from. No egui here: what's on screen is worked out in `layout`.

use std::{collections::HashMap, ops::Range};

use pulldown_cmark::{
    Alignment, CodeBlockKind, Event, HeadingLevel, LinkType, MetadataBlockKind, OffsetIter,
    Options, Parser, Tag, TagEnd,
};

/// Containers nested deeper than this are flattened: `>>>>…` a thousand times
/// mustn't overflow the stack, here or in the layout.
const MAX_DEPTH: usize = 32;

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
    /// Shown as it's written, for now.
    Html(String),
    /// `$$…$$` alone in a paragraph.
    Math(String),
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
    /// Where the text's first line ends, as a callout's title does.
    first_break: Option<usize>,
    has_display_math: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Run {
    /// In [`Inlines::text`].
    pub range: Range<usize>,
    /// In the source, roughly: escapes and entities don't line up within it.
    pub source: Range<usize>,
    pub style: Style,
    pub link: Option<Link>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Style {
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
    pub code: bool,
    pub math: bool,
    pub highlight: bool,
    pub tag: bool,
    pub sup: bool,
    pub sub: bool,
    /// Raw HTML, shown as written.
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
        if let Some(last) = self.runs.last_mut()
            && last.range.end == start
            && last.style == style
            && last.link.as_ref() == link
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
        });
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
        .map(|(label, blocks)| Footnote {
            number: numbers.get(&label).copied().unwrap_or(usize::MAX),
            label,
            blocks,
        })
        .collect();
    // Footnotes nothing refers to go last, numbered after the others.
    footnotes.sort_by_key(|f| f.number);
    let referred = footnotes.iter().filter(|f| f.number != usize::MAX).count();
    for (index, footnote) in footnotes.iter_mut().enumerate().skip(referred) {
        footnote.number = index + 1;
    }
    document.footnotes = footnotes;
    document
}

fn options() -> Options {
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
    footnote_blocks: Vec<(String, Vec<Block>)>,
    document: Document,
}

impl Builder<'_> {
    /// Blocks until the `end` tag, which it consumes (or the end of the text).
    fn blocks(&mut self, end: Option<TagEnd>, depth: usize) -> Vec<Block> {
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
                self.footnote_blocks.push((label, children));
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

    /// Inline content up to `end` (which it consumes); without one, up to
    /// the next block, as in a tight list's item.
    fn inlines(&mut self, end: Option<TagEnd>) -> Inlines {
        let mut out = Inlines::default();
        let mut style = Style::default();
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
            let link = links.last();
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
                    let style = Style {
                        html: true,
                        ..style
                    };
                    out.push(&text, range, style, link);
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
                    });
                    out.push(IMAGE_CHAR.encode_utf8(&mut placeholder), range, style, link);
                }
                _ => {}
            }
            style.bold = bold > 0;
            style.italic = italic > 0;
            style.strike = strike > 0;
            style.sup = sup > 0;
            style.sub = sub > 0;
        }
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
        has_display_math: inlines.has_display_math,
        ..Inlines::default()
    };
    let mut images = std::mem::take(&mut inlines.images).into_iter().peekable();
    let mut highlight = false;
    let mut first_break = inlines.first_break;
    for run in &inlines.runs {
        let text = &inlines.text[run.range.clone()];
        if first_break.is_some_and(|at| at == run.range.start) {
            out.first_break = Some(out.text.len());
            first_break = None;
        }
        if !is_plain(run) {
            let style = Style {
                highlight,
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
                highlight,
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
    fn raw_html_is_kept_as_written() {
        let doc = parse("<div>\nhi\n</div>\n\nA <kbd>b</kbd>\n");
        assert!(matches!(&doc.blocks[0].kind, BlockKind::Html(h) if h.contains("<div>")));
        let BlockKind::Paragraph(inlines) = &doc.blocks[1].kind else {
            panic!("expected a paragraph");
        };
        assert_eq!(styled(inlines, |s| s.html), ["<kbd>", "</kbd>"]);
    }

    #[test]
    fn deep_nesting_is_flattened_rather_than_overflowing() {
        let source = format!("{}deep\n", ">".repeat(50_000));
        let doc = parse(&source);
        assert!(!doc.blocks.is_empty());
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
        ] {
            let _ = parse(source);
        }
    }
}
