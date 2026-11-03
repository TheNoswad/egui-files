//! Syntax highlighting with syntect and the syntaxes and themes bat ships
//! (via two-face). Highlighting runs on a worker thread, a line at a time,
//! and only re-highlights lines whose text or starting state changed.

use std::{
    rc::Rc,
    sync::{
        Arc, OnceLock,
        mpsc::{self, Receiver, Sender, TryRecvError},
    },
    time::Duration,
};

use web_time::Instant;

use egui::{
    Color32, FontId, Stroke,
    text::{ByteIndex, LayoutJob, TextFormat},
};
use syntect::{
    highlighting::{
        Color, FontStyle, HighlightIterator, HighlightState, Highlighter as Styler, Theme,
    },
    parsing::{ParseState, ScopeStack, SyntaxReference, SyntaxSet},
};
use two_face::theme::{EmbeddedLazyThemeSet, EmbeddedThemeName};

/// Larger files are shown without colors: highlighting runs at about a
/// megabyte a second.
pub const MAX_HIGHLIGHT_BYTES: usize = 1024 * 1024;

pub const DEFAULT_DARK_THEME: &str = "OneHalfDark";
pub const DEFAULT_LIGHT_THEME: &str = "OneHalfLight";
pub const PLAIN_TEXT: &str = "Plain Text";

fn syntaxes() -> &'static SyntaxSet {
    static SYNTAXES: OnceLock<SyntaxSet> = OnceLock::new();
    SYNTAXES.get_or_init(two_face::syntax::extra_newlines)
}

fn theme_set() -> &'static EmbeddedLazyThemeSet {
    static THEMES: OnceLock<EmbeddedLazyThemeSet> = OnceLock::new();
    THEMES.get_or_init(two_face::theme::extra)
}

/// Loads the syntaxes and themes in the background, so the first preview
/// doesn't wait for them. Where threads can't start (the browser), they load
/// on first use instead.
pub fn preload() {
    let _ = std::thread::Builder::new().spawn(|| {
        syntaxes();
        theme(DEFAULT_DARK_THEME);
        theme(DEFAULT_LIGHT_THEME);
    });
}

/// Themes that make sense in an app: the `ansi` and `base16` ones encode
/// terminal palette indices instead of colors.
pub fn theme_names() -> impl Iterator<Item = &'static str> {
    EmbeddedLazyThemeSet::theme_names()
        .iter()
        .filter(|name| {
            !matches!(
                name,
                EmbeddedThemeName::Ansi | EmbeddedThemeName::Base16 | EmbeddedThemeName::Base16_256
            )
        })
        .map(|name| name.as_name())
}

/// The named theme, or the default one for its brightness if it's unknown.
pub fn theme(name: &str) -> &'static Theme {
    let find = |name: &str| {
        EmbeddedLazyThemeSet::theme_names()
            .iter()
            .find(|n| n.as_name() == name)
            .copied()
    };
    let name = find(name)
        .or_else(|| find(DEFAULT_DARK_THEME))
        .expect("the default theme is bundled");
    theme_set().get(name)
}

/// Names of all syntaxes, sorted, for choosing one by hand.
pub fn syntax_names() -> Vec<&'static str> {
    let mut names: Vec<_> = syntaxes()
        .syntaxes()
        .iter()
        .filter(|s| !s.hidden)
        .map(|s| s.name.as_str())
        .collect();
    names.sort_by_key(|n| n.to_lowercase());
    names
}

/// The syntax for a file, from its name, its extension, or its first line
/// (`#!/bin/sh`, `<?xml`).
pub fn detect(path: &std::path::Path, first_line: &str) -> &'static str {
    let set = syntaxes();
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    set.find_syntax_by_extension(name)
        .or_else(|| set.find_syntax_by_extension(name.trim_start_matches('.')))
        .or_else(|| set.find_syntax_by_extension(ext))
        .or_else(|| set.find_syntax_by_extension(&ext.to_lowercase()))
        .or_else(|| set.find_syntax_by_first_line(first_line))
        .map_or(PLAIN_TEXT, |s| s.name.as_str())
}

/// The bundled syntax's name for `name`, or plain text's if there's none.
pub fn syntax_name(name: &str) -> &'static str {
    syntax(name).name.as_str()
}

fn syntax(name: &str) -> &'static SyntaxReference {
    let set = syntaxes();
    set.find_syntax_by_name(name)
        .unwrap_or_else(|| set.find_syntax_plain_text())
}

pub fn color(color: Color) -> Color32 {
    Color32::from_rgba_unmultiplied(color.r, color.g, color.b, color.a)
}

/// A styled run within one line, in bytes from the line's start.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Span {
    pub start: u32,
    pub end: u32,
    pub color: Color32,
    pub italic: bool,
    pub underline: bool,
}

pub type LineSpans = Arc<[Span]>;

/// How a text is split into lines: after each `\n`, like syntect expects.
pub fn lines(text: &str) -> std::str::SplitInclusive<'_, char> {
    text.split_inclusive('\n')
}

/// Which lines differ between two texts: the first `prefix` lines and last
/// `suffix` lines are the same in both.
#[derive(Debug, PartialEq)]
pub struct LineDiff {
    pub prefix: usize,
    pub suffix: usize,
    pub old_lines: usize,
    pub new_lines: usize,
}

/// How many lines [`lines`] splits a text into.
pub fn line_count(text: &str) -> usize {
    count_newlines(text.as_bytes()) + usize::from(!text.is_empty() && !text.ends_with('\n'))
}

fn count_newlines(bytes: &[u8]) -> usize {
    memchr::memchr_iter(b'\n', bytes).count()
}

/// Compares in chunks first: slice equality is a fast `memcmp`.
fn common_prefix(a: &[u8], b: &[u8]) -> usize {
    const CHUNK: usize = 4096;
    let len = a.len().min(b.len());
    let mut at = 0;
    while at + CHUNK <= len && a[at..at + CHUNK] == b[at..at + CHUNK] {
        at += CHUNK;
    }
    while at < len && a[at] == b[at] {
        at += 1;
    }
    at
}

fn common_suffix(a: &[u8], b: &[u8], max: usize) -> usize {
    const CHUNK: usize = 4096;
    let (mut ea, mut eb) = (a.len(), b.len());
    let mut same = 0;
    while same + CHUNK <= max && a[ea - CHUNK..ea] == b[eb - CHUNK..eb] {
        (ea, eb, same) = (ea - CHUNK, eb - CHUNK, same + CHUNK);
    }
    while same < max && a[ea - 1] == b[eb - 1] {
        (ea, eb, same) = (ea - 1, eb - 1, same + 1);
    }
    same
}

impl LineDiff {
    pub fn new(old: &str, new: &str) -> Self {
        let (old_b, new_b) = (old.as_bytes(), new.as_bytes());
        let same_start = common_prefix(old_b, new_b);
        let max_end = old_b.len().min(new_b.len()) - same_start;
        let same_end = common_suffix(old_b, new_b, max_end);
        let old_lines = line_count(old);
        let new_lines = line_count(new);
        if old.len() == new.len() && same_start == old.len() {
            return Self {
                prefix: old_lines,
                suffix: 0,
                old_lines,
                new_lines,
            };
        }
        // Lines that end inside the common start.
        let prefix = count_newlines(&old_b[..same_start]);
        // Lines that start inside the common end, where both texts start a line:
        // those after a newline in it, plus one right at its start if both
        // texts start a line there.
        let old_tail = old_b.len() - same_end;
        let new_tail = new_b.len() - same_end;
        let mut suffix = 0;
        if old_tail < old_b.len() {
            suffix += count_newlines(&old_b[old_tail..old_b.len() - 1]);
            let starts_line = |bytes: &[u8], at: usize| at == 0 || bytes[at - 1] == b'\n';
            if starts_line(old_b, old_tail) && starts_line(new_b, new_tail) {
                suffix += 1;
            }
        }
        let suffix = suffix
            .min(old_lines - prefix.min(old_lines))
            .min(new_lines - prefix.min(new_lines));
        Self {
            prefix,
            suffix,
            old_lines,
            new_lines,
        }
    }

    /// The changed lines, as indices into the new text's lines.
    pub fn changed(&self) -> std::ops::Range<usize> {
        self.prefix..self.new_lines - self.suffix
    }

    /// Applies the diff to per-line data: changed lines keep the old line's
    /// data where there was one, so colors don't flicker while re-highlighting.
    pub fn splice<T: Clone>(&self, items: &mut Vec<T>, empty: impl Fn() -> T) {
        if items.len() != self.old_lines {
            items.clear();
            items.resize_with(self.new_lines, empty);
            return;
        }
        let old_mid = self.old_lines - self.prefix - self.suffix;
        let new_mid = self.new_lines - self.prefix - self.suffix;
        let kept = old_mid.min(new_mid);
        let start = self.prefix + kept;
        if new_mid > old_mid {
            let extra = std::iter::repeat_with(empty).take(new_mid - old_mid);
            items.splice(start..start, extra);
        } else {
            items.drain(start..start + (old_mid - new_mid));
        }
    }
}

enum Request {
    Text { version: u64, text: Arc<str> },
}

/// Highlighting results for one version of the text.
pub struct Highlighted {
    pub version: u64,
    pub lines: Vec<LineSpans>,
    /// Every line is highlighted; otherwise more results follow.
    pub done: bool,
}

/// Highlights a text on a worker thread as it changes.
pub struct Highlighter {
    requests: Sender<Request>,
    results: Receiver<Highlighted>,
    pub syntax: &'static str,
    pub theme: &'static str,
}

impl Highlighter {
    pub fn new(ctx: &egui::Context, syntax: &str, theme: &str) -> Self {
        let (requests, worker_requests) = mpsc::channel();
        let (worker_results, results) = mpsc::channel();
        let syntax = self::syntax(syntax);
        let theme_ref = self::theme(theme);
        let ctx = ctx.clone();
        std::thread::Builder::new()
            .name("highlight".into())
            .spawn(move || {
                Worker::new(syntax, theme_ref).run(worker_requests, worker_results, ctx);
            })
            .expect("spawn highlighter");
        Self {
            requests,
            results,
            syntax: syntax.name.as_str(),
            theme: theme_name(theme),
        }
    }

    pub fn update(&self, version: u64, text: Arc<str>) {
        let _ = self.requests.send(Request::Text { version, text });
    }

    /// The newest results, if any arrived.
    pub fn poll(&self) -> Option<Highlighted> {
        self.results.try_iter().last()
    }
}

/// The bundled theme's name for `name`, falling back like [`theme`].
pub fn theme_name(name: &str) -> &'static str {
    EmbeddedLazyThemeSet::theme_names()
        .iter()
        .map(|n| n.as_name())
        .find(|n| *n == name)
        .unwrap_or(DEFAULT_DARK_THEME)
}

#[derive(Clone)]
struct State {
    parse: ParseState,
    highlight: HighlightState,
}

impl PartialEq for State {
    fn eq(&self, other: &Self) -> bool {
        self.parse == other.parse && self.highlight == other.highlight
    }
}

/// A highlighted line, with the states before and after it. A line whose
/// starting state is unchanged highlights the same way again.
#[derive(Clone)]
struct Line {
    start: Option<Rc<State>>,
    end: Option<Rc<State>>,
    spans: LineSpans,
}

impl Line {
    fn empty() -> Self {
        Self {
            start: None,
            end: None,
            spans: Arc::new([]),
        }
    }
}

struct Worker {
    styler: Styler<'static>,
    initial: Rc<State>,
    text: Arc<str>,
    lines: Vec<Line>,
}

/// How often partial results are sent while a long text is highlighted.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(60);

impl Worker {
    fn new(syntax: &'static SyntaxReference, theme: &'static Theme) -> Self {
        let styler = Styler::new(theme);
        let initial = Rc::new(State {
            parse: ParseState::new(syntax),
            highlight: HighlightState::new(&styler, ScopeStack::new()),
        });
        Self {
            styler,
            initial,
            text: Arc::from(""),
            lines: Vec::new(),
        }
    }

    fn run(
        mut self,
        requests: Receiver<Request>,
        results: Sender<Highlighted>,
        ctx: egui::Context,
    ) {
        let mut pending = requests.recv().ok();
        while let Some(Request::Text { version, text }) = pending.take() {
            // Only the newest text matters.
            let (version, text) = match requests.try_iter().last() {
                Some(Request::Text { version, text }) => (version, text),
                None => (version, text),
            };
            let diff = LineDiff::new(&self.text, &text);
            diff.splice(&mut self.lines, Line::empty);
            // Their old colors stay until they're redone, but not their states.
            for line in &mut self.lines[diff.changed()] {
                line.start = None;
            }
            self.text = text;
            let send = |lines: &[Line], done| {
                let lines = lines.iter().map(|l| l.spans.clone()).collect();
                let sent = results.send(Highlighted {
                    version,
                    lines,
                    done,
                });
                ctx.request_repaint();
                sent.is_ok()
            };
            match self.highlight(&requests, &send) {
                Ok(Some(next)) => pending = Some(next),
                Ok(None) => {
                    if !send(&self.lines, true) {
                        return;
                    }
                    pending = requests.recv().ok();
                }
                // The UI is gone.
                Err(()) => return,
            }
        }
    }

    /// Brings every line up to date. Returns early with a newer request if
    /// one arrives.
    fn highlight(
        &mut self,
        requests: &Receiver<Request>,
        send: &dyn Fn(&[Line], bool) -> bool,
    ) -> Result<Option<Request>, ()> {
        let set = syntaxes();
        let text = self.text.clone();
        let mut last_sent = Instant::now();
        let mut state = self.initial.clone();
        for (index, line) in lines(&text).enumerate() {
            let record = &mut self.lines[index];
            if let (Some(start), Some(end)) = (&record.start, &record.end)
                && (Rc::ptr_eq(start, &state) || **start == *state)
            {
                state = end.clone();
                continue;
            }
            let mut next = (*state).clone();
            let ops = next.parse.parse_line(line, set).unwrap_or_default();
            let mut spans = Vec::new();
            let mut offset = 0;
            for (style, piece) in
                HighlightIterator::new(&mut next.highlight, &ops, line, &self.styler)
            {
                let end = offset + piece.len();
                if !piece.trim().is_empty() {
                    spans.push(Span {
                        start: offset as u32,
                        end: end as u32,
                        color: color(style.foreground),
                        italic: style.font_style.contains(FontStyle::ITALIC),
                        underline: style.font_style.contains(FontStyle::UNDERLINE),
                    });
                }
                offset = end;
            }
            let next = Rc::new(next);
            *record = Line {
                start: Some(state),
                end: Some(next.clone()),
                spans: spans.into(),
            };
            state = next;

            if index % 32 == 0 {
                match requests.try_recv() {
                    Ok(request) => return Ok(Some(request)),
                    Err(TryRecvError::Disconnected) => return Err(()),
                    Err(TryRecvError::Empty) => {}
                }
                if last_sent.elapsed() > PROGRESS_INTERVAL {
                    if !send(&self.lines, false) {
                        return Err(());
                    }
                    last_sent = Instant::now();
                }
            }
        }
        Ok(None)
    }
}

/// Colors and decorations that come from the theme rather than the text.
pub struct ThemeColors {
    pub background: Color32,
    pub foreground: Color32,
    pub caret: Option<Color32>,
    pub selection: Option<Color32>,
    pub gutter_foreground: Option<Color32>,
    pub line_highlight: Option<Color32>,
    pub find_highlight: Option<Color32>,
}

impl ThemeColors {
    pub fn of(name: &str, visuals: &egui::Visuals) -> Self {
        let settings = &theme(name).settings;
        Self {
            background: settings.background.map_or(visuals.extreme_bg_color, color),
            foreground: settings.foreground.map_or(visuals.text_color(), color),
            caret: settings.caret.map(color),
            selection: settings.selection.map(color),
            gutter_foreground: settings.gutter_foreground.map(color),
            line_highlight: settings.line_highlight.map(color),
            find_highlight: settings.find_highlight.map(color),
        }
    }
}

/// Lays out one line (without its line break) with its spans; spans that
/// don't fit, from before an edit, are left out.
pub fn line_job(
    line: &str,
    spans: &[Span],
    font: &FontId,
    foreground: Color32,
    wrap_width: f32,
) -> LayoutJob {
    let mut job = LayoutJob {
        text: line.to_owned(),
        ..Default::default()
    };
    job.wrap.max_width = wrap_width;
    let plain = TextFormat::simple(font.clone(), foreground);
    let section = |job: &mut LayoutJob, range: std::ops::Range<usize>, format: TextFormat| {
        if !range.is_empty() {
            job.sections.push(egui::text::LayoutSection {
                leading_space: 0.0,
                byte_range: ByteIndex(range.start)..ByteIndex(range.end),
                format,
            });
        }
    };
    let mut at = 0;
    for span in spans {
        let (start, end) = (span.start as usize, (span.end as usize).min(line.len()));
        if start < at
            || start >= end
            || !line.is_char_boundary(start)
            || !line.is_char_boundary(end)
        {
            break;
        }
        section(&mut job, at..start, plain.clone());
        let format = TextFormat {
            color: span.color,
            italics: span.italic,
            underline: if span.underline {
                Stroke::new(1.0, span.color)
            } else {
                Stroke::NONE
            },
            ..plain.clone()
        };
        section(&mut job, start..end, format);
        at = end;
    }
    section(&mut job, at..line.len(), plain);
    job
}

/// Lays out `text` with `spans` (one entry per line; missing or stale ones
/// fall back to plain text).
pub fn layout_job(
    text: &str,
    spans: &[LineSpans],
    font: FontId,
    foreground: Color32,
    wrap_width: f32,
) -> LayoutJob {
    let mut job = LayoutJob {
        text: text.to_owned(),
        ..Default::default()
    };
    job.wrap.max_width = wrap_width;
    let plain = TextFormat::simple(font.clone(), foreground);
    let mut offset = 0;
    let push = |job: &mut LayoutJob, range: std::ops::Range<usize>, format: &TextFormat| {
        if range.is_empty() {
            return;
        }
        // Merge with the previous section when it looks the same.
        if let Some(last) = job.sections.last_mut()
            && last.byte_range.end.0 == range.start
            && last.format == *format
        {
            last.byte_range.end = ByteIndex(range.end);
            return;
        }
        job.sections.push(egui::text::LayoutSection {
            leading_space: 0.0,
            byte_range: ByteIndex(range.start)..ByteIndex(range.end),
            format: format.clone(),
        });
    };
    for (index, line) in lines(text).enumerate() {
        let line_end = offset + line.len();
        let mut at = offset;
        if let Some(line_spans) = spans.get(index) {
            for span in line_spans.iter() {
                let start = offset + span.start as usize;
                let end = offset + span.end as usize;
                if start < at
                    || end > line_end
                    || !text.is_char_boundary(start)
                    || !text.is_char_boundary(end)
                {
                    // Spans from before an edit that no longer fit.
                    break;
                }
                push(&mut job, at..start, &plain);
                let format = TextFormat {
                    color: span.color,
                    italics: span.italic,
                    underline: if span.underline {
                        Stroke::new(1.0, span.color)
                    } else {
                        Stroke::NONE
                    },
                    ..plain.clone()
                };
                push(&mut job, start..end, &format);
                at = end;
            }
        }
        push(&mut job, at..line_end, &plain);
        offset = line_end;
    }
    job
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diff(old: &str, new: &str) -> (usize, usize) {
        let d = LineDiff::new(old, new);
        (d.prefix, d.suffix)
    }

    #[test]
    fn line_diff_finds_the_changed_lines() {
        assert_eq!(diff("a\nb\nc\n", "a\nX\nc\n"), (1, 1));
        assert_eq!(diff("a\nb\nc", "a\nb\nc"), (3, 0));
        assert_eq!(diff("a\nb\nc", "a\nbX\nc"), (1, 1));
        // Inserting a line.
        assert_eq!(diff("a\nb\nc\n", "a\nb\nnew\nc\n"), (2, 1));
        // Typing at the end, with and without a final newline.
        assert_eq!(diff("a\nb", "a\nbc"), (1, 0));
        assert_eq!(diff("a\nb\n", "a\nb\nc"), (2, 0));
        // Repeated lines can't confuse it into overlapping.
        let d = LineDiff::new("x\nx\nx\n", "x\nx\n");
        assert!(d.prefix + d.suffix <= d.new_lines);
        assert_eq!(diff("", "abc"), (0, 0));
        assert_eq!(diff("abc", ""), (0, 0));
        // Joining two lines.
        assert_eq!(diff("a\nb\nc\nd\n", "a\nbc\nd\n"), (1, 1));
    }

    #[test]
    fn splice_keeps_unchanged_lines_aligned() {
        let old = "a\nb\nc\nd\n";
        let new = "a\nb\nnew\nnew2\nc\nd\n";
        let mut items: Vec<String> = lines(old).map(str::to_owned).collect();
        LineDiff::new(old, new).splice(&mut items, String::new);
        assert_eq!(items.len(), lines(new).count());
        assert_eq!(items[0], "a\n");
        assert_eq!(items[4], "c\n");
        assert_eq!(items[5], "d\n");

        let newer = "a\nd\n";
        LineDiff::new(new, newer).splice(&mut items, String::new);
        assert_eq!(items, ["a\n", "d\n"]);
    }

    #[test]
    fn detects_syntaxes() {
        use std::path::Path;
        assert_eq!(detect(Path::new("main.rs"), ""), "Rust");
        assert_eq!(detect(Path::new("Cargo.toml"), ""), "TOML");
        assert_eq!(detect(Path::new("Makefile"), ""), "Makefile");
        assert_eq!(
            detect(Path::new("script"), "#!/bin/sh"),
            "Bourne Again Shell (bash)"
        );
        assert_eq!(detect(Path::new("notes"), "hello"), PLAIN_TEXT);
        assert_eq!(detect(Path::new("README.MD"), ""), "Markdown");
    }

    fn highlight(text: &str, syntax: &str) -> Highlighted {
        let ctx = egui::Context::default();
        let highlighter = Highlighter::new(&ctx, syntax, DEFAULT_DARK_THEME);
        highlighter.update(1, Arc::from(text));
        loop {
            if let Some(result) = highlighter
                .results
                .recv_timeout(Duration::from_secs(10))
                .ok()
                && result.done
            {
                return result;
            }
        }
    }

    #[test]
    fn highlights_and_rehighlights_incrementally() {
        let ctx = egui::Context::default();
        let highlighter = Highlighter::new(&ctx, "Rust", DEFAULT_DARK_THEME);
        let wait = |version| loop {
            let result = highlighter
                .results
                .recv_timeout(Duration::from_secs(10))
                .expect("results");
            if result.done && result.version == version {
                return result;
            }
        };
        highlighter.update(1, Arc::from("fn main() {}\nlet x = 1;\n"));
        let first = wait(1);
        assert_eq!(first.lines.len(), 2);
        assert!(
            first.lines[0].len() > 1,
            "fn and main differ: {:?}",
            first.lines[0]
        );

        // Opening a block comment recolors everything after it.
        highlighter.update(2, Arc::from("/*fn main() {}\nlet x = 1;\n"));
        let commented = wait(2);
        let comment_color = commented.lines[0].last().unwrap().color;
        assert!(
            commented.lines[1].iter().all(|s| s.color == comment_color),
            "{:?} vs {:?}",
            commented.lines[0],
            commented.lines[1]
        );

        // The unchanged line reuses its spans.
        highlighter.update(3, Arc::from("fn main() {}\nlet x = 1;\n// end\n"));
        let third = wait(3);
        assert_eq!(third.lines[1], first.lines[1]);
    }

    #[test]
    fn layout_job_covers_the_text_even_with_stale_spans() {
        let result = highlight("fn main() {}\n", "Rust");
        let text = "fn ma\n";
        let job = layout_job(
            text,
            &result.lines,
            FontId::monospace(12.0),
            Color32::WHITE,
            f32::INFINITY,
        );
        let mut covered = 0;
        for section in &job.sections {
            assert_eq!(section.byte_range.start.0, covered);
            covered = section.byte_range.end.0;
        }
        assert_eq!(covered, text.len());
    }
}
