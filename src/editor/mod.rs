//! A tab for editing a text file, with syntax highlighting, find and
//! replace, and reloading when another program changes the file.

pub mod document;
pub mod highlight;
pub mod ops;

use std::{
    collections::HashSet,
    ops::Range,
    path::{Path, PathBuf},
    sync::{
        Arc,
        mpsc::{self, Receiver},
    },
};

use egui::{
    Align, Align2, Button, CentralPanel, Color32, Context, FontId, Galley, Id, Key, Layout,
    Modifiers, Panel, Rect, RichText, ScrollArea, Shape, TextBuffer, TextEdit, TextStyle, Ui,
    UiBuilder,
    text::{CCursor, CCursorRange, LayoutJob},
    text_edit::TextEditState,
};
use egui_phosphor::regular as icon;
use serde::{Deserialize, Serialize};

use self::{
    document::{LineEnding, LoadError, Loaded, ReadOnly, Stamp},
    highlight::{Highlighter, LineSpans, ThemeColors},
    ops::{Edit, Indent},
};
use crate::{
    markdown::{
        self, Click, MdView, Mode, Target,
        format::{self, Inline, LineKind},
        layout::Hit,
        parse::{Heading, Stats},
        table::{self, TableAction},
        view::toggle_task,
    },
    tab::{AppAction, Shared},
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    pub dark_theme: String,
    pub light_theme: String,
    /// Use the app theme's suggestion instead of the named theme.
    pub dark_matches_app: bool,
    pub light_matches_app: bool,
    pub markdown: markdown::Prefs,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            dark_theme: highlight::DEFAULT_DARK_THEME.to_owned(),
            light_theme: highlight::DEFAULT_LIGHT_THEME.to_owned(),
            dark_matches_app: true,
            light_matches_app: true,
            markdown: markdown::Prefs::default(),
        }
    }
}

impl Prefs {
    /// The syntax theme for the app theme showing in `ui`.
    pub fn theme(&self, ui: &Ui) -> &'static str {
        let dark = ui.visuals().dark_mode;
        let (name, matches_app) = if dark {
            (&self.dark_theme, self.dark_matches_app)
        } else {
            (&self.light_theme, self.light_matches_app)
        };
        if matches_app {
            highlight::theme_name(&crate::theme::colors(ui.ctx()).syntax)
        } else {
            highlight::theme_name(name)
        }
    }

    /// The View menu's theme picker, for the mode the app is in.
    pub fn menu_ui(&mut self, ui: &mut Ui) {
        let dark = ui.visuals().dark_mode;
        ui.weak(if dark {
            "Used in dark mode"
        } else {
            "Used in light mode"
        });
        let suggested = crate::theme::colors(ui.ctx()).syntax.clone();
        let (current, matches_app) = if dark {
            (&mut self.dark_theme, &mut self.dark_matches_app)
        } else {
            (&mut self.light_theme, &mut self.light_matches_app)
        };
        if ui
            .radio(*matches_app, format!("Match app theme ({suggested})"))
            .clicked()
        {
            *matches_app = true;
        }
        ui.separator();
        ScrollArea::vertical().max_height(420.0).show(ui, |ui| {
            for name in highlight::theme_names() {
                if ui.radio(!*matches_app && current == name, name).clicked() {
                    *current = name.to_owned();
                    *matches_app = false;
                }
            }
        });
    }
}

/// Whether a file can be shown in an editor tab: not a folder, image,
/// video or archive. Binary files are refused once loaded.
pub fn can_edit(path: &Path, is_dir: bool) -> bool {
    !is_dir
        && !crate::images::is_image(path)
        && !crate::video::is_video(path)
        && !crate::archive::is_archive(path)
}

#[derive(Serialize, Deserialize)]
pub struct EditorTab {
    pub id: u64,
    /// The file being edited. Its name also marks saved state as an editor tab.
    pub edit: PathBuf,
    #[serde(default)]
    wrap: bool,
    /// Chosen by hand instead of detected.
    #[serde(default)]
    syntax: Option<String>,
    /// How a markdown document is shown; other files are always source.
    #[serde(default)]
    mode: Mode,
    /// The list of a document's headings is showing.
    #[serde(default)]
    outline: bool,

    /// What `mode` comes to at this width, or source for other files.
    #[serde(skip)]
    shown_mode: Mode,
    #[serde(skip)]
    doc: Doc,
    /// What's rendered, for a markdown document.
    #[serde(skip)]
    md: MarkdownState,
    #[serde(skip)]
    find: Option<FindBar>,
    #[serde(skip)]
    goto: Option<GoTo>,
    /// Selects this range in the text next frame.
    #[serde(skip)]
    select: Option<Range<usize>>,
    #[serde(skip)]
    scroll_to_cursor: bool,
    #[serde(skip)]
    focus_text: bool,
    /// The folder changed: see whether the file did.
    #[serde(skip)]
    check_disk: bool,
    /// Select the next match once matches are found again after a replace.
    #[serde(skip)]
    pending_find_next: bool,
}

#[derive(Default)]
enum Doc {
    #[default]
    Unloaded,
    Loading(Receiver<Result<Loaded, LoadError>>),
    Failed(LoadError),
    Ready(Box<Buffer>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Disk {
    InSync,
    /// Another program changed it while we have unsaved changes.
    Changed,
    Deleted,
}

struct Buffer {
    text: String,
    /// What's on disk, to tell whether there are unsaved changes.
    saved: String,
    dirty: bool,
    /// The view version `dirty` was worked out for.
    dirty_version: u64,
    line_ending: LineEnding,
    bom: bool,
    read_only: Option<ReadOnly>,
    stamp: Option<Stamp>,
    disk: Disk,
    reload: Option<Receiver<Result<Loaded, LoadError>>>,
    indent: Indent,
    detected_syntax: &'static str,
    error: Option<String>,
    view: View,
}

impl Buffer {
    fn new(loaded: Loaded, path: &Path) -> Self {
        let first_line = loaded.text.lines().next().unwrap_or("");
        Self {
            detected_syntax: highlight::detect(path, first_line),
            indent: Indent::detect(&loaded.text),
            saved: loaded.text.clone(),
            text: loaded.text,
            dirty: false,
            dirty_version: 0,
            line_ending: loaded.line_ending,
            bom: loaded.bom,
            read_only: loaded.read_only,
            stamp: loaded.stamp,
            disk: Disk::InSync,
            reload: None,
            error: None,
            view: View::default(),
        }
    }

    fn reloaded(&mut self, loaded: Loaded) {
        self.saved = loaded.text.clone();
        self.text = loaded.text;
        self.line_ending = loaded.line_ending;
        self.bom = loaded.bom;
        self.read_only = loaded.read_only;
        self.stamp = loaded.stamp;
        self.disk = Disk::InSync;
        self.dirty = false;
        self.error = None;
    }
}

/// What's needed to draw the text: its colors and its layout.
#[derive(Default)]
struct View {
    /// The text as last laid out, to notice edits.
    shown: Arc<str>,
    version: u64,
    highlighter: Option<Highlighter>,
    spans: Vec<LineSpans>,
    spans_version: u64,
    galley: Option<(GalleyKey, Arc<Galley>)>,
    /// Each line's layout, and the colors it was laid out with.
    lines: Vec<Option<LineGalley>>,
    rows: Option<(usize, RowIndex)>,
    /// Line, column and selected characters, for the status bar.
    status: Option<(u32, usize, usize)>,
}

#[derive(Clone)]
struct LineGalley {
    spans: LineSpans,
    galley: Arc<Galley>,
}

#[derive(Clone, Copy, PartialEq)]
struct GalleyKey {
    version: u64,
    spans_version: u64,
    width: f32,
    font_size: f32,
    color: Color32,
    font_generation: u64,
}

impl View {
    /// Notices a changed text and passes it on to the highlighter.
    fn sync(&mut self, text: &str) {
        if *self.shown == *text {
            return;
        }
        crate::theme::fonts::want(text);
        let diff = highlight::LineDiff::new(&self.shown, text);
        diff.splice(&mut self.lines, || None);
        for line in &mut self.lines[diff.changed()] {
            *line = None;
        }
        if self.highlighter.is_some() {
            diff.splice(&mut self.spans, || Arc::from([]));
            self.spans_version += 1;
        }
        self.shown = Arc::from(text);
        self.version += 1;
        if let Some(highlighter) = &self.highlighter {
            highlighter.update(self.version, self.shown.clone());
        }
    }

    /// Lays out the text a line at a time, reusing the lines that are
    /// unchanged and have the same colors: egui would lay out everything
    /// again after an edit, as it forgets the lines of galleys it didn't
    /// lay out last frame.
    fn layout(
        &mut self,
        ctx: &Context,
        text: &str,
        font: &FontId,
        color: Color32,
        width: f32,
    ) -> Arc<Galley> {
        self.sync(text);
        let width = if width.is_finite() {
            width.round()
        } else {
            width
        };
        let key = GalleyKey {
            version: self.version,
            spans_version: self.spans_version,
            width,
            font_size: font.size,
            color,
            font_generation: crate::theme::fonts::generation(ctx),
        };
        if let Some((cached, galley)) = &self.galley {
            if *cached == key {
                return galley.clone();
            }
            if (
                cached.width,
                cached.font_size,
                cached.color,
                cached.font_generation,
            ) != (width, font.size, color, key.font_generation)
            {
                self.lines.iter_mut().for_each(|line| *line = None);
            }
        }
        let count = highlight::line_count(text);
        if self.lines.len() != count {
            self.lines = vec![None; count];
        }
        let empty: LineSpans = Arc::from([]);
        let mut galleys: Vec<Arc<Galley>> = ctx.fonts_mut(|fonts| {
            highlight::lines(text)
                .zip(&mut self.lines)
                .enumerate()
                .map(|(index, (line, cached))| {
                    let spans = self.spans.get(index).unwrap_or(&empty);
                    if let Some(cached) = cached
                        && (Arc::ptr_eq(&cached.spans, spans)
                            || cached.spans.is_empty() && spans.is_empty())
                    {
                        return cached.galley.clone();
                    }
                    // Like egui: the line break isn't laid out, except a final one.
                    let paragraph = if index + 1 < count {
                        line.strip_suffix('\n').unwrap_or(line)
                    } else {
                        line
                    };
                    let job = highlight::line_job(paragraph, spans, font, color, width);
                    let galley = fonts.layout_job(job);
                    *cached = Some(LineGalley {
                        spans: spans.clone(),
                        galley: galley.clone(),
                    });
                    galley
                })
                .collect()
        });
        if galleys.is_empty() {
            let job = highlight::line_job("", &[], font, color, width);
            galleys.push(ctx.fonts_mut(|f| f.layout_job(job)));
        }
        let mut job = LayoutJob::simple(text.to_owned(), font.clone(), color, width);
        job.wrap.max_width = width;
        let galley = Arc::new(Galley::concat(
            Arc::new(job),
            &galleys,
            ctx.pixels_per_point(),
        ));
        self.galley = Some((key, galley.clone()));
        galley
    }

    /// Line numbers and offsets of the galley's rows.
    fn rows(&mut self, galley: &Arc<Galley>) -> &RowIndex {
        let key = Arc::as_ptr(galley) as usize;
        if self.rows.as_ref().is_none_or(|(k, _)| *k != key) {
            self.rows = Some((key, RowIndex::new(galley)));
        }
        &self.rows.as_ref().expect("just set").1
    }
}

/// For each row of a galley: its 1-based line, whether it starts the line,
/// and its first char.
struct RowIndex {
    line: Vec<u32>,
    starts_line: Vec<bool>,
    first_char: Vec<usize>,
}

impl RowIndex {
    fn new(galley: &Galley) -> Self {
        let rows = galley.rows.len();
        let mut index = Self {
            line: Vec::with_capacity(rows),
            starts_line: Vec::with_capacity(rows),
            first_char: Vec::with_capacity(rows),
        };
        let (mut line, mut starts, mut chars) = (1, true, 0);
        for row in &galley.rows {
            index.line.push(line);
            index.starts_line.push(starts);
            index.first_char.push(chars);
            chars += row.char_count_including_newline().0;
            starts = row.ends_with_newline;
            line += u32::from(row.ends_with_newline);
        }
        index
    }
}

/// A document in its place in the links followed.
struct Visited {
    path: PathBuf,
    offset: f32,
}

/// What a tab keeps about the document it renders.
#[derive(Default)]
struct MarkdownState {
    view: MdView,
    back: Vec<Visited>,
    forward: Vec<Visited>,
    /// Scroll here once the document is loaded and laid out.
    anchor: Option<String>,
    restore_offset: Option<f32>,
    /// The documents whose remote pictures the user chose to load.
    allow_remote: HashSet<PathBuf>,
    /// The rendered pane moved by itself: the source pane follows it.
    preview_jumped: bool,
    /// Where the panes of Split should go next frame.
    preview_to: Option<f32>,
    source_to: Option<f32>,
    /// Where they were last frame, to tell which moved.
    preview_was: f32,
    source_was: f32,
    version_was: u64,
    /// The text version the rendered pane was last asked to catch up with.
    repaint_version: u64,
    /// The source offset at the top of what's showing, and which heading
    /// that's under, for the outline.
    top_byte: usize,
    outline_current: Option<usize>,
    /// Where pasted and dropped pictures go; from the settings.
    image_folder: String,
}

/// Where the source pane is scrolled, for keeping the panes together.
struct SourceScroll {
    offset: f32,
    viewport: f32,
    content: f32,
    galley: Arc<Galley>,
}

struct FindBar {
    /// In the rendered document: what's in `matches`, and where.
    hits: Vec<Hit>,
    query: String,
    replacement: String,
    show_replace: bool,
    match_case: bool,
    whole_word: bool,
    matches: Vec<Range<usize>>,
    /// What `matches` was found for: text version, query and options.
    found_for: Option<(u64, String, bool, bool)>,
    /// The match last selected.
    current: Option<usize>,
    /// Where the cursor was when the search started.
    start: usize,
    focus: bool,
}

struct GoTo {
    line: String,
    focus: bool,
}

enum Command {
    Save,
    Find {
        replace: bool,
    },
    FindNext {
        back: bool,
    },
    GoTo,
    CloseBars,
    ToggleWrap,
    ToggleComment,
    Indent {
        outdent: bool,
    },
    Newline,
    Replace {
        all: bool,
    },
    GoToLine,
    NextMode,
    Back,
    Forward,
    ToggleOutline,
    Inline(Inline),
    Heading(u8),
    Lines(LineKind),
    CodeBlock,
    Link,
    Image,
    Rule,
    InsertTable,
    Table(TableAction),
    /// Text that looks like an address, pasted.
    PasteUrl(String),
    /// Ctrl+V with no text on the clipboard.
    PasteImage,
}

impl EditorTab {
    pub fn new(id: u64, path: PathBuf) -> Self {
        Self {
            id,
            // Markdown is mostly prose, on long lines.
            wrap: markdown::is_markdown(&path),
            edit: path,
            syntax: None,
            mode: Mode::Preview,
            outline: false,
            shown_mode: Mode::Source,
            doc: Doc::Unloaded,
            md: MarkdownState::default(),
            find: None,
            goto: None,
            select: None,
            scroll_to_cursor: false,
            focus_text: true,
            check_disk: false,
            pending_find_next: false,
        }
    }

    /// A tab for reading a markdown document.
    pub fn new_markdown(id: u64, path: PathBuf) -> Self {
        Self {
            mode: Mode::Preview,
            focus_text: false,
            ..Self::new(id, path)
        }
    }

    /// Asked to edit the file: a document being read changes to `mode`.
    pub fn show_for_editing(&mut self, mode: Mode) {
        if self.is_markdown() && self.mode == Mode::Preview {
            self.mode = mode;
        }
        self.focus_text = true;
    }

    /// Scrolls to a heading, once the document is here.
    pub fn go_to_anchor(&mut self, anchor: Option<String>) {
        if anchor.is_some() {
            self.md.anchor = anchor;
        }
    }

    /// Whether the file is shown as markdown.
    fn is_markdown(&self) -> bool {
        match &self.syntax {
            Some(syntax) => highlight::syntax_name(syntax) == "Markdown",
            None => markdown::is_markdown(&self.edit),
        }
    }

    pub fn title(&self) -> String {
        let name = crate::fs::display_name(&self.edit);
        let dirty = if self.is_dirty() { " ●" } else { "" };
        let glyph = if self.is_markdown() && self.mode == Mode::Preview {
            icon::BOOK_OPEN
        } else {
            icon::NOTE_PENCIL
        };
        format!("{glyph} {name}{dirty}")
    }

    pub fn is_dirty(&self) -> bool {
        matches!(&self.doc, Doc::Ready(buffer) if buffer.dirty)
    }

    pub fn dir(&self) -> Option<PathBuf> {
        self.edit.parent().map(Path::to_path_buf)
    }

    /// The file may have changed: check it next frame.
    pub fn mark_stale(&mut self) {
        self.check_disk = true;
    }

    pub fn dirs_changed(&mut self, dirs: &HashSet<PathBuf>) {
        if dirs.contains(&self.edit) || self.dir().is_some_and(|dir| dirs.contains(&dir)) {
            self.check_disk = true;
        }
    }

    /// Writes the text to the file. Errors are also shown in the tab.
    pub fn save(&mut self) -> Result<(), String> {
        let Doc::Ready(buffer) = &mut self.doc else {
            return Ok(());
        };
        if buffer.read_only.is_some() {
            return Err("The file is read-only.".into());
        }
        match document::save(&self.edit, &buffer.text, buffer.line_ending, buffer.bom) {
            Ok(stamp) => {
                buffer.saved = buffer.text.clone();
                buffer.dirty = false;
                buffer.stamp = Some(stamp);
                buffer.disk = Disk::InSync;
                buffer.error = None;
                Ok(())
            }
            Err(err) => {
                let message = format!("Could not save: {err}");
                buffer.error = Some(message.clone());
                Err(message)
            }
        }
    }

    fn text_id(&self) -> Id {
        text_id(self.id)
    }

    /// Loads the file, applies reloads, and reacts to changes on disk.
    fn update_doc(&mut self, ctx: &Context) {
        match &self.doc {
            Doc::Unloaded => self.doc = Doc::Loading(load(&self.edit, ctx)),
            Doc::Loading(receiver) => match receiver.try_recv() {
                Ok(Ok(loaded)) => self.doc = Doc::Ready(Box::new(Buffer::new(loaded, &self.edit))),
                Ok(Err(err)) => self.doc = Doc::Failed(err),
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => self.doc = Doc::Unloaded,
            },
            Doc::Failed(_) | Doc::Ready(_) => {}
        }
        let check = std::mem::take(&mut self.check_disk);
        let reload = if let Doc::Ready(buffer) = &mut self.doc {
            if let Some(receiver) = &buffer.reload
                && let Ok(result) = receiver.try_recv()
            {
                buffer.reload = None;
                match result {
                    Ok(loaded) => {
                        let chars = loaded.text.chars().count();
                        buffer.reloaded(loaded);
                        // Keep the cursor where it was, if it still fits.
                        if let Some(mut state) = TextEditState::load(ctx, text_id(self.id))
                            && let Some(range) = state.cursor.char_range()
                        {
                            let clamp = |c: CCursor| CCursor::new(c.index.0.min(chars));
                            state.cursor.set_char_range(Some(CCursorRange::two(
                                clamp(range.secondary),
                                clamp(range.primary),
                            )));
                            state.store(ctx, text_id(self.id));
                        }
                    }
                    Err(err) => buffer.error = Some(err.to_string()),
                }
            }
            let now = check.then(|| document::stamp(&self.edit));
            match now {
                Some(now) if now != buffer.stamp && buffer.reload.is_none() => match now {
                    None => {
                        buffer.disk = Disk::Deleted;
                        false
                    }
                    Some(_) if buffer.dirty => {
                        buffer.disk = Disk::Changed;
                        false
                    }
                    Some(_) => true,
                },
                _ => false,
            }
        } else {
            matches!(self.doc, Doc::Failed(_)) && check
        };
        if reload {
            match &mut self.doc {
                Doc::Ready(buffer) => buffer.reload = Some(load(&self.edit, ctx)),
                _ => self.doc = Doc::Unloaded,
            }
        }
    }

    pub fn ui(&mut self, ui: &mut Ui, shared: &mut Shared) {
        let ctx = ui.ctx().clone();
        self.update_doc(&ctx);
        let buffer = match &mut self.doc {
            Doc::Unloaded | Doc::Loading(_) => {
                ui.centered_and_justified(|ui| {
                    ui.horizontal_centered(|ui| {
                        ui.spinner();
                        ui.weak("Loading…");
                    });
                });
                return;
            }
            Doc::Failed(err) => {
                let message = err.to_string();
                ui.vertical_centered(|ui| {
                    ui.add_space(32.0);
                    ui.label(RichText::new(icon::FILE_CODE).size(48.0));
                    ui.add_space(8.0);
                    ui.label(message);
                    ui.add_space(8.0);
                    if ui
                        .button(format!(
                            "{} Open with the default app",
                            icon::ARROW_SQUARE_OUT
                        ))
                        .clicked()
                        && let Err(err) = crate::fs::open(&self.edit)
                    {
                        eprintln!("Could not open {}: {err}", self.edit.display());
                    }
                });
                return;
            }
            Doc::Ready(buffer) => buffer,
        };

        let theme = shared.editor.theme(ui).to_owned();
        let syntax = self.syntax.as_deref().unwrap_or(buffer.detected_syntax);
        let is_markdown = match &self.syntax {
            Some(syntax) => highlight::syntax_name(syntax) == "Markdown",
            None => markdown::is_markdown(&self.edit),
        };
        let mode = if is_markdown {
            self.mode.at_width(ui.available_width())
        } else {
            Mode::Source
        };
        self.shown_mode = mode;
        if self.md.image_folder != shared.editor.markdown.image_folder {
            self.md.image_folder = shared.editor.markdown.image_folder.clone();
        }
        // Colors for source that isn't shown can wait until it is.
        if mode != Mode::Preview {
            update_highlighter(&ctx, buffer, syntax, &theme);
        }

        let focused = shared.focused_tab == Some(self.id);
        let text_focused = ctx.memory(|m| m.has_focus(text_id(self.id)));
        let editable = buffer.read_only.is_none();
        let can_edit_text = mode != Mode::Preview;
        let commands = if focused {
            shortcuts(
                &ctx,
                text_focused && editable && can_edit_text,
                is_markdown,
                self.md.view.lightbox_is_open(),
            )
        } else {
            Vec::new()
        };
        for command in commands {
            // What works on the source does nothing while it isn't shown.
            if can_edit_text
                || matches!(
                    command,
                    Command::Save
                        | Command::CloseBars
                        | Command::NextMode
                        | Command::Back
                        | Command::Forward
                        | Command::ToggleOutline
                        | Command::Find { .. }
                        | Command::FindNext { .. }
                )
            {
                self.run(&ctx, command);
            }
        }

        let cursor = self.selection(&ctx).start;
        let Doc::Ready(buffer) = &mut self.doc else {
            return;
        };
        let mut commands = Vec::new();
        let width = ui.available_width();
        let whole = ui.max_rect();
        Panel::top(Id::new(("editor-toolbar", self.id))).show(ui, |ui| {
            let markdown = is_markdown.then_some(MarkdownBar {
                mode: &mut self.mode,
                shown: mode,
                has_room: width >= markdown::MIN_SPLIT_WIDTH,
                can_go_back: !self.md.back.is_empty(),
                can_go_forward: !self.md.forward.is_empty(),
                outline: &mut self.outline,
            });
            toolbar(
                ui,
                buffer,
                &self.edit,
                &mut self.wrap,
                markdown,
                &mut commands,
            );
        });
        if is_markdown && can_edit_text && editable {
            Panel::top(Id::new(("editor-format", self.id))).show(ui, |ui| {
                format_bar(ui, &buffer.text, cursor, &mut commands);
            });
        }
        let mut reload = false;
        if buffer.disk != Disk::InSync || buffer.error.is_some() || buffer.read_only.is_some() {
            Panel::top(Id::new(("editor-banner", self.id)))
                .show(ui, |ui| reload = banner(ui, buffer, &self.edit));
        }
        if let Some(find) = &mut self.find {
            Panel::top(Id::new(("editor-find", self.id))).show(ui, |ui| {
                find_bar(ui, find, editable && can_edit_text, &mut commands);
            });
        }
        if let Some(goto) = &mut self.goto {
            Panel::top(Id::new(("editor-goto", self.id))).show(ui, |ui| {
                goto_bar(ui, goto, &mut commands);
            });
        }
        Panel::bottom(Id::new(("editor-status", self.id))).show(ui, |ui| {
            let stats = is_markdown.then(|| self.md.view.stats());
            status_bar(ui, buffer, &mut self.syntax, self.id, stats, can_edit_text);
        });
        if reload {
            buffer.reload = Some(load(&self.edit, &ctx));
        }
        for command in commands {
            self.run(&ctx, command);
        }

        let Doc::Ready(buffer) = &mut self.doc else {
            return;
        };
        let colors = ThemeColors::of(&theme, ui.visuals());
        // The outline and stats need it whichever way the document shows.
        if is_markdown {
            self.md.view.sync(&ctx, &buffer.text);
        }
        // With no room beside it, the outline takes the place of the document.
        let outline_only = is_markdown && self.outline && width < markdown::MIN_SPLIT_WIDTH;
        let mut heading = None;
        if is_markdown && self.outline {
            let mut show = |ui: &mut Ui| {
                heading = outline_ui(ui, &mut self.md);
            };
            if outline_only {
                CentralPanel::default().show(ui, |ui| show(ui));
            } else {
                Panel::left(Id::new(("editor-outline", self.id)))
                    .resizable(true)
                    .default_size(220.0)
                    .show(ui, |ui| show(ui));
            }
        }
        if mode != Mode::Source || outline_only {
            buffer.view.sync(&buffer.text);
        }
        let mut clicks = Vec::new();
        let mut preview_rect = None;
        if mode != Mode::Source && !outline_only {
            let mut show = |ui: &mut Ui| {
                preview_rect = Some(ui.max_rect());
                clicks = rendered(
                    ui,
                    &mut self.md,
                    Rendered {
                        id: Id::new(("editor-rendered", self.id)),
                        path: &self.edit,
                        theme: &theme,
                        colors: &colors,
                        editable,
                        prefs: &shared.editor.markdown,
                        keyboard: focused && !text_focused,
                        hits: self
                            .find
                            .as_ref()
                            .filter(|_| mode == Mode::Preview)
                            .map(|find| (find.hits.as_slice(), find.current)),
                    },
                    &mut shared.markdown_images,
                );
            };
            if mode == Mode::Split {
                // What the outline left, of which the source keeps some.
                let room = ui.available_width();
                Panel::right(Id::new(("editor-rendered-pane", self.id)))
                    .resizable(true)
                    .default_size(room / 2.0)
                    .min_size(200.0)
                    .max_size((room - 200.0).max(200.0))
                    .frame(egui::Frame::NONE.fill(ui.visuals().panel_fill))
                    .show(ui, |ui| show(ui));
            } else {
                CentralPanel::default()
                    .frame(egui::Frame::NONE.fill(ui.visuals().panel_fill))
                    .show(ui, |ui| show(ui));
            }
        }
        let mut source_scroll = None;
        if mode != Mode::Preview && !outline_only {
            CentralPanel::default()
                .frame(egui::Frame::NONE.fill(colors.background))
                .show(ui, |ui| {
                    let id = text_id(self.id);
                    if let Some(range) = self.select.take() {
                        let mut state = TextEditState::load(&ctx, id).unwrap_or_default();
                        state.cursor.set_char_range(Some(CCursorRange::two(
                            CCursor::new(range.start),
                            CCursor::new(range.end),
                        )));
                        state.store(&ctx, id);
                    }
                    if std::mem::take(&mut self.focus_text) {
                        ctx.memory_mut(|m| m.request_focus(id));
                    }
                    source_scroll = text_area(
                        ui,
                        buffer,
                        TextArea {
                            id,
                            wrap: self.wrap,
                            colors: &colors,
                            matches: self
                                .find
                                .as_ref()
                                .map(|f| (f.matches.as_slice(), f.current)),
                            scroll_to_cursor: std::mem::take(&mut self.scroll_to_cursor),
                            offset: self.md.source_to.take(),
                        },
                    );
                });
        }
        // What was typed reaches the rendered pane and the stats next frame.
        if is_markdown && self.md.repaint_version != buffer.view.version {
            self.md.repaint_version = buffer.view.version;
            ctx.request_repaint();
        }
        if is_markdown && self.outline {
            self.md.top_byte = match &source_scroll {
                Some(source) if mode == Mode::Source => {
                    let char = source
                        .galley
                        .cursor_from_pos(egui::vec2(0.0, source.offset))
                        .index
                        .0;
                    ops::char_to_byte(&buffer.text, char)
                }
                _ => self.md.view.source_of_offset(self.md.view.offset),
            };
        }
        if mode == Mode::Split
            && let (Some(source), Some(rect)) = (&source_scroll, preview_rect)
        {
            sync_panes(
                &ctx,
                &mut self.md,
                &buffer.text,
                buffer.view.version,
                source,
                rect,
            );
        }

        if buffer.view.version != buffer.dirty_version {
            buffer.dirty = buffer.text != buffer.saved;
            buffer.dirty_version = buffer.view.version;
        }
        if let Some(find) = &mut self.find {
            let in_rendered = mode == Mode::Preview;
            let version = if in_rendered {
                self.md.view.layout_generation() | 1 << 63
            } else {
                buffer.view.version
            };
            let key = (
                version,
                find.query.clone(),
                find.match_case,
                find.whole_word,
            );
            if find.found_for.as_ref() != Some(&key) {
                let edited = find
                    .found_for
                    .as_ref()
                    .is_some_and(|(version, query, case, word)| {
                        (query, *case, *word) == (&key.1, key.2, key.3) && *version != key.0
                    });
                if in_rendered {
                    let layout = self.md.view.layout();
                    find.hits = layout.find(&find.query, find.match_case, find.whole_word);
                    find.matches = find.hits.iter().map(|hit| hit.chars.clone()).collect();
                } else {
                    find.hits.clear();
                    find.matches =
                        ops::find(&buffer.text, &find.query, find.match_case, find.whole_word);
                }
                find.current = None;
                find.found_for = Some(key);
                // Typing a search jumps to the first match from where it started.
                if in_rendered && !edited && !find.hits.is_empty() {
                    let view = &self.md.view;
                    let top = view.offset;
                    let index = find
                        .hits
                        .iter()
                        .position(|hit| view.offset_of_hit(hit) >= top)
                        .unwrap_or(0);
                    find.current = Some(index);
                    self.md.preview_to = Some(view.offset_of_hit(&find.hits[index]));
                } else if !in_rendered && !edited && !find.matches.is_empty() {
                    let start = find.start;
                    let index = find
                        .matches
                        .iter()
                        .position(|m| m.start >= start)
                        .unwrap_or(0);
                    find.current = Some(index);
                    self.select = Some(find.matches[index].clone());
                    self.scroll_to_cursor = true;
                }
            }
        }
        if std::mem::take(&mut self.pending_find_next) {
            self.find_next(&ctx, false);
        }
        for click in clicks {
            self.clicked(&ctx, click, &mut shared.app_actions);
        }
        if let Some((start, slug)) = heading {
            match mode {
                Mode::Source => {
                    if let Doc::Ready(buffer) = &self.doc {
                        let at = buffer.text.get(..start).map_or(0, |t| t.chars().count());
                        self.select = Some(at..at);
                        self.scroll_to_cursor = true;
                        // It only scrolls to a cursor it has focus for.
                        self.focus_text = true;
                    }
                }
                _ => self.md.anchor = Some(slug),
            }
            if outline_only {
                self.outline = false;
            }
        }
        if is_markdown
            && editable
            && let Some(files) = crate::dnd::files_dropped_on(&ctx, whole)
        {
            self.drop_files(&ctx, files);
        }
    }

    /// Whether files dropped with this tab focused are for it: a document
    /// that can take them, rather than the folder it's in.
    pub fn takes_files(&self) -> bool {
        self.is_markdown() && matches!(&self.doc, Doc::Ready(b) if b.read_only.is_none())
    }

    /// Puts files into the document, at the cursor if it's showing.
    pub fn drop_files(&mut self, ctx: &Context, files: Vec<PathBuf>) {
        let dir = self.dir().unwrap_or_default();
        match markdown::assets::markdown_for(&dir, &self.md.image_folder, &files) {
            Ok(text) if !text.is_empty() => self.insert_block(ctx, &text),
            Ok(_) => {}
            Err(err) => {
                if let Doc::Ready(buffer) = &mut self.doc {
                    buffer.error = Some(format!("Could not add that: {err}"));
                }
            }
        }
    }

    /// Adds a block of markdown at the cursor, or at the end of the document
    /// when it's only being read.
    fn insert_block(&mut self, ctx: &Context, block: &str) {
        let Doc::Ready(buffer) = &self.doc else {
            return;
        };
        let selection = if self.shown_mode == Mode::Preview {
            let end = buffer.text.chars().count();
            end..end
        } else {
            self.selection(ctx)
        };
        let edit = format::insert_block(&buffer.text, selection, &format!("{block}\n"));
        self.apply_edit(ctx, edit);
    }

    /// Acts on a click in the rendered document.
    fn clicked(&mut self, ctx: &Context, click: Click, actions: &mut Vec<AppAction>) {
        match click {
            Click::Task(task) => {
                let Doc::Ready(buffer) = &self.doc else {
                    return;
                };
                let Some((range, mark)) = toggle_task(&buffer.text, &task) else {
                    return;
                };
                let mut text = buffer.text.clone();
                text.replace_range(range, mark);
                // A task's mark doesn't change the count of chars before the cursor.
                let selection = self.selection(ctx);
                self.apply_edit(ctx, Edit { text, selection });
                self.scroll_to_cursor = false;
            }
            Click::Link { link, new_tab } => {
                let dir = self.dir().unwrap_or_default();
                let target = markdown::links::resolve(&link, &dir);
                self.follow(ctx, target, new_tab, actions);
            }
        }
    }

    fn follow(
        &mut self,
        ctx: &Context,
        target: Target,
        new_tab: bool,
        actions: &mut Vec<AppAction>,
    ) {
        let problem = match target {
            Target::Web(url) => {
                markdown::links::open_web(ctx, url);
                None
            }
            Target::Anchor(anchor) => {
                self.md.anchor = Some(anchor);
                None
            }
            Target::Document(path, anchor) => {
                if !disk::path_exists(&path) {
                    Some(format!("{} doesn't exist.", path.display()))
                } else if new_tab || self.is_dirty() {
                    // Going elsewhere in this tab would lose unsaved changes.
                    actions.push(AppAction::OpenMarkdown { path, anchor });
                    None
                } else {
                    self.go_to(path, anchor, 0.0, true);
                    None
                }
            }
            Target::File(path) => match markdown::links::open_local(&path) {
                markdown::links::Opened::Done => None,
                markdown::links::Opened::Missing => {
                    Some(format!("{} doesn't exist.", path.display()))
                }
                markdown::links::Opened::Reveal => {
                    actions.push(AppAction::Reveal(path));
                    None
                }
                markdown::links::Opened::Archive => {
                    actions.push(AppAction::OpenArchive(path));
                    None
                }
                markdown::links::Opened::Failed(err) => {
                    Some(format!("Could not open {}: {err}", path.display()))
                }
            },
            Target::Refused => Some("This link can't be followed.".to_owned()),
        };
        if let (Some(problem), Doc::Ready(buffer)) = (problem, &mut self.doc) {
            buffer.error = Some(problem);
        }
    }

    /// Shows another document in this tab.
    fn go_to(&mut self, path: PathBuf, anchor: Option<String>, offset: f32, remember: bool) {
        if remember {
            self.md.back.push(Visited {
                path: self.edit.clone(),
                offset: self.md.view.offset,
            });
            self.md.forward.clear();
        }
        self.edit = path;
        self.doc = Doc::Unloaded;
        self.syntax = None;
        self.md.view = MdView::default();
        self.md.anchor = anchor;
        self.md.restore_offset = (offset > 0.0).then_some(offset);
    }

    fn go_back(&mut self, back: bool) {
        // Unsaved changes stay until they're saved or the tab is closed.
        if self.is_dirty() {
            return;
        }
        let (from, to) = if back {
            (&mut self.md.back, &mut self.md.forward)
        } else {
            (&mut self.md.forward, &mut self.md.back)
        };
        let Some(visited) = from.pop() else {
            return;
        };
        to.push(Visited {
            path: self.edit.clone(),
            offset: self.md.view.offset,
        });
        self.go_to(visited.path, None, visited.offset, false);
    }

    fn selection(&self, ctx: &Context) -> Range<usize> {
        TextEditState::load(ctx, self.text_id())
            .and_then(|s| s.cursor.char_range())
            .map_or(0..0, |range| {
                let [min, max] = range.sorted_cursors();
                min.index.0..max.index.0
            })
    }

    /// Replaces the text, as one undo step.
    fn apply_edit(&mut self, ctx: &Context, edit: Edit) {
        let Doc::Ready(buffer) = &mut self.doc else {
            return;
        };
        if buffer.read_only.is_some() {
            return;
        }
        let id = text_id(self.id);
        let mut state = TextEditState::load(ctx, id).unwrap_or_default();
        let before = state.cursor.char_range().unwrap_or_default();
        let mut undoer = state.undoer();
        undoer.add_undo(&(before, buffer.text.clone()));
        state.set_undoer(undoer);
        buffer.text = edit.text;
        state.cursor.set_char_range(Some(CCursorRange::two(
            CCursor::new(edit.selection.start),
            CCursor::new(edit.selection.end),
        )));
        state.store(ctx, id);
        self.scroll_to_cursor = true;
    }

    fn run(&mut self, ctx: &Context, command: Command) {
        let Doc::Ready(buffer) = &self.doc else {
            return;
        };
        let selection = self.selection(ctx);
        match command {
            Command::Save => {
                let _ = self.save();
            }
            Command::NextMode => {
                self.mode = match self.mode {
                    Mode::Source => Mode::Split,
                    Mode::Split => Mode::Preview,
                    Mode::Preview => Mode::Source,
                };
            }
            Command::Back => self.go_back(true),
            Command::Forward => self.go_back(false),
            Command::ToggleOutline => self.outline = !self.outline,
            Command::Inline(style) => {
                let edit = format::toggle_inline(&buffer.text, selection, style);
                self.apply_format(ctx, edit);
            }
            Command::Heading(level) => {
                let edit = format::set_heading(&buffer.text, selection, level);
                self.apply_format(ctx, edit);
            }
            Command::Lines(kind) => {
                let edit = format::toggle_lines(&buffer.text, selection, kind);
                self.apply_format(ctx, edit);
            }
            Command::CodeBlock => {
                let edit = format::code_block(&buffer.text, selection);
                self.apply_format(ctx, edit);
            }
            Command::Link => {
                let edit = format::link(&buffer.text, selection);
                self.apply_format(ctx, edit);
            }
            Command::Image => {
                let edit = format::image(&buffer.text, selection);
                self.apply_format(ctx, edit);
            }
            Command::Rule => {
                let edit = format::rule(&buffer.text, selection);
                self.apply_format(ctx, edit);
            }
            Command::InsertTable => {
                let edit = format::insert_table(&buffer.text, selection);
                self.apply_format(ctx, edit);
            }
            Command::Table(action) => {
                if let Some(edit) = table::apply(&buffer.text, selection.start, action) {
                    self.apply_format(ctx, edit);
                }
            }
            Command::PasteUrl(pasted) => {
                let edit = format::paste_url(&buffer.text, selection.clone(), &pasted)
                    .unwrap_or_else(|| ops::insert(&buffer.text, selection, &pasted));
                self.apply_edit(ctx, edit);
            }
            Command::PasteImage => self.paste_picture(ctx),
            Command::Find { replace } => {
                // What's selected in the source isn't what's being read.
                let selected = (selection.start != selection.end
                    && self.shown_mode != Mode::Preview)
                    .then(|| {
                        let text = &buffer.text;
                        let start = ops::char_to_byte(text, selection.start);
                        let end = ops::char_to_byte(text, selection.end);
                        text[start..end].to_owned()
                    })
                    .filter(|s| !s.contains('\n'));
                let find = self.find.get_or_insert_with(|| FindBar {
                    hits: Vec::new(),
                    query: String::new(),
                    replacement: String::new(),
                    show_replace: false,
                    match_case: false,
                    whole_word: false,
                    matches: Vec::new(),
                    found_for: None,
                    current: None,
                    start: selection.start,
                    focus: true,
                });
                if let Some(selected) = selected {
                    find.query = selected;
                }
                find.show_replace |= replace;
                find.focus = true;
                self.goto = None;
            }
            Command::FindNext { back } => self.find_next(ctx, back),
            Command::GoTo => {
                self.find = None;
                self.goto = Some(GoTo {
                    line: String::new(),
                    focus: true,
                });
            }
            Command::CloseBars => {
                // The text field only keeps a selection while focused.
                if let Some(find) = self.find.take()
                    && self.shown_mode != Mode::Preview
                    && let Some(current) = find.current.and_then(|i| find.matches.get(i))
                {
                    self.select = Some(current.clone());
                }
                self.goto = None;
                self.focus_text = true;
            }
            Command::ToggleWrap => self.wrap = !self.wrap,
            Command::ToggleComment => {
                let syntax = self.syntax.as_deref().unwrap_or(buffer.detected_syntax);
                if let Some(style) = ops::comment_style(highlight::syntax_name(syntax)) {
                    let edit = ops::toggle_comment(&buffer.text, selection, style);
                    self.apply_edit(ctx, edit);
                }
            }
            Command::Indent { outdent } => {
                let text = &buffer.text;
                if self.is_markdown() {
                    let action = if outdent {
                        TableAction::PreviousCell
                    } else {
                        TableAction::NextCell
                    };
                    let edit = table::apply(text, selection.start, action)
                        .or_else(|| format::indent_item(text, selection.clone(), outdent));
                    if let Some(edit) = edit {
                        self.apply_edit(ctx, edit);
                        return;
                    }
                }
                let multiline = {
                    let start = ops::char_to_byte(text, selection.start);
                    let end = ops::char_to_byte(text, selection.end);
                    text[start..end].contains('\n')
                };
                let edit = if outdent || multiline {
                    ops::indent_lines(text, selection, buffer.indent, outdent)
                } else {
                    ops::insert(text, selection, &buffer.indent.text())
                };
                self.apply_edit(ctx, edit);
            }
            Command::Replace { all } => self.replace(ctx, all),
            Command::GoToLine => {
                let Some(goto) = self.goto.take() else {
                    return;
                };
                let mut parts = goto.line.trim().splitn(2, [':', ',']);
                let line = parts.next().and_then(|l| l.trim().parse::<usize>().ok());
                let col = parts.next().and_then(|c| c.trim().parse::<usize>().ok());
                if let Some(line) = line {
                    let start = ops::line_start(&buffer.text, line.max(1));
                    let line_len = buffer.text[ops::char_to_byte(&buffer.text, start)..]
                        .split('\n')
                        .next()
                        .map_or(0, |l| l.chars().count());
                    let at = start + col.unwrap_or(1).saturating_sub(1).min(line_len);
                    self.select = Some(at..at);
                    self.scroll_to_cursor = true;
                }
                self.focus_text = true;
            }
            Command::Newline => {
                if self.is_markdown()
                    && let Some(edit) = format::continue_list(&buffer.text, selection.clone())
                {
                    self.apply_edit(ctx, edit);
                    return;
                }
                let indent = ops::indent_at(&buffer.text, selection.start).to_owned();
                let edit = ops::insert(&buffer.text, selection, &format!("\n{indent}"));
                self.apply_edit(ctx, edit);
            }
        }
    }

    /// Selects the next (or previous) match, wrapping around. Without a
    /// current match, starts from the cursor.
    fn find_next(&mut self, ctx: &Context, back: bool) {
        let cursor = self.selection(ctx);
        let Some(find) = &mut self.find else {
            return;
        };
        let count = find.matches.len();
        if count == 0 {
            return;
        }
        let index = match find.current.filter(|&i| i < count) {
            Some(current) if back => (current + count - 1) % count,
            Some(current) => (current + 1) % count,
            None if back => find
                .matches
                .iter()
                .rposition(|m| m.start < cursor.start)
                .unwrap_or(count - 1),
            None => find
                .matches
                .iter()
                .position(|m| m.start >= cursor.start)
                .unwrap_or(0),
        };
        find.current = Some(index);
        if self.shown_mode == Mode::Preview {
            if let Some(hit) = find.hits.get(index) {
                self.md.preview_to = Some(self.md.view.offset_of_hit(hit));
            }
            return;
        }
        self.select = Some(find.matches[index].clone());
        self.scroll_to_cursor = true;
    }

    /// A command from the toolbar took focus from the text: it goes back.
    fn apply_format(&mut self, ctx: &Context, edit: Edit) {
        self.apply_edit(ctx, edit);
        self.focus_text = true;
    }

    /// Saves the picture on the clipboard beside the document and adds it.
    fn paste_picture(&mut self, ctx: &Context) {
        #[cfg(not(any(target_os = "android", target_arch = "wasm32")))]
        {
            let Some((width, height, rgba)) = markdown::assets::clipboard_picture() else {
                return;
            };
            let dir = self.dir().unwrap_or_default();
            let folder = self.md.image_folder.clone();
            match markdown::assets::save_pasted(&dir, &folder, width, height, rgba) {
                Ok(text) => self.insert_block(ctx, &text),
                Err(err) => {
                    if let Doc::Ready(buffer) = &mut self.doc {
                        buffer.error = Some(format!("Could not save the picture: {err}"));
                    }
                }
            }
        }
        #[cfg(any(target_os = "android", target_arch = "wasm32"))]
        let _ = ctx;
    }

    /// Replaces the current match and moves on, or replaces every match.
    fn replace(&mut self, ctx: &Context, all: bool) {
        let cursor = self.selection(ctx);
        let (Some(find), Doc::Ready(buffer)) = (&self.find, &self.doc) else {
            return;
        };
        let with = find.replacement.clone();
        if all {
            if find.matches.is_empty() {
                return;
            }
            let text = ops::replace_all(&buffer.text, &find.matches, &with);
            let cursor = cursor.start.min(text.chars().count());
            self.apply_edit(
                ctx,
                Edit {
                    text,
                    selection: cursor..cursor,
                },
            );
        } else if let Some(current) = find.current.and_then(|i| find.matches.get(i)) {
            let edit = ops::insert(&buffer.text, current.clone(), &with);
            self.apply_edit(ctx, edit);
            // Matches are found again for the new text next frame; then
            // select the one after the cursor.
            self.pending_find_next = true;
        } else {
            self.find_next(ctx, false);
        }
    }
}

fn text_id(tab: u64) -> Id {
    Id::new(("editor-text", tab))
}

/// Reads the file on a thread.
fn load(path: &Path, ctx: &Context) -> Receiver<Result<Loaded, LoadError>> {
    let (sender, receiver) = mpsc::channel();
    let path = path.to_path_buf();
    let ctx = ctx.clone();
    crate::background::spawn("load-document", move || {
        let _ = sender.send(document::load(&path));
        ctx.request_repaint();
    });
    receiver
}

fn update_highlighter(ctx: &Context, buffer: &mut Buffer, syntax: &str, theme: &str) {
    let view = &mut buffer.view;
    let wanted = buffer.text.len() <= highlight::MAX_HIGHLIGHT_BYTES
        && highlight::syntax_name(syntax) != highlight::PLAIN_TEXT;
    if !wanted {
        if view.highlighter.take().is_some() || !view.spans.is_empty() {
            view.spans.clear();
            view.spans_version += 1;
        }
        return;
    }
    let current = view.highlighter.as_ref().is_some_and(|h| {
        h.syntax == highlight::syntax_name(syntax) && h.theme == highlight::theme_name(theme)
    });
    if !current {
        let highlighter = Highlighter::new(ctx, syntax, theme);
        highlighter.update(view.version, view.shown.clone());
        view.highlighter = Some(highlighter);
    }
    if let Some(result) = view.highlighter.as_ref().and_then(Highlighter::poll)
        && result.version == view.version
    {
        view.spans = result.lines;
        view.spans_version += 1;
    }
}

/// Keys the editor handles itself, taken before the text field sees them.
fn shortcuts(
    ctx: &Context,
    text_focused: bool,
    is_markdown: bool,
    lightbox_open: bool,
) -> Vec<Command> {
    use crate::keymap::Command as Global;
    let cmd = Modifiers::COMMAND;
    let keymap = crate::keymap::get(ctx);
    ctx.input_mut(|i| {
        let mut commands = Vec::new();
        if is_markdown {
            if i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::V) {
                commands.push(Command::NextMode);
            }
            if i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::O) {
                commands.push(Command::ToggleOutline);
            }
            if text_focused {
                let cmd_shift = Modifiers::COMMAND | Modifiers::SHIFT;
                if i.consume_key(cmd_shift, Key::X) {
                    commands.push(Command::Inline(Inline::Strike));
                }
                for (key, command) in [
                    (Key::B, Command::Inline(Inline::Bold)),
                    (Key::I, Command::Inline(Inline::Italic)),
                    (Key::E, Command::Inline(Inline::Code)),
                    (Key::K, Command::Link),
                    (Key::Num0, Command::Heading(0)),
                    (Key::Num1, Command::Heading(1)),
                    (Key::Num2, Command::Heading(2)),
                    (Key::Num3, Command::Heading(3)),
                    (Key::Num4, Command::Heading(4)),
                    (Key::Num5, Command::Heading(5)),
                    (Key::Num6, Command::Heading(6)),
                ] {
                    if i.consume_key(Modifiers::COMMAND, key) {
                        commands.push(command);
                    }
                }
            }
            if keymap.pressed(i, Global::Back)
                || i.pointer.button_pressed(egui::PointerButton::Extra1)
            {
                commands.push(Command::Back);
            }
            if keymap.pressed(i, Global::Forward)
                || i.pointer.button_pressed(egui::PointerButton::Extra2)
            {
                commands.push(Command::Forward);
            }
        }
        let mut key = |mods, key, command| {
            if i.consume_key(mods, key) {
                commands.push(command);
            }
        };
        key(cmd, Key::S, Command::Save);
        key(cmd, Key::F, Command::Find { replace: false });
        key(cmd, Key::H, Command::Find { replace: true });
        key(cmd, Key::G, Command::GoTo);
        key(cmd, Key::Slash, Command::ToggleComment);
        key(Modifiers::ALT, Key::Z, Command::ToggleWrap);
        key(Modifiers::SHIFT, Key::F3, Command::FindNext { back: true });
        key(Modifiers::NONE, Key::F3, Command::FindNext { back: false });
        // Escape closes a picture opened over everything first.
        if !lightbox_open {
            key(Modifiers::NONE, Key::Escape, Command::CloseBars);
        }
        if text_focused {
            // Instead of the text field inserting a plain tab or line break.
            i.events.retain(|event| match event {
                egui::Event::Paste(pasted) if is_markdown && looks_like_address(pasted) => {
                    commands.push(Command::PasteUrl(pasted.clone()));
                    false
                }
                egui::Event::Key {
                    key: Key::V,
                    pressed: true,
                    repeat: false,
                    modifiers,
                    ..
                } if is_markdown && modifiers.command => {
                    commands.push(Command::PasteImage);
                    false
                }
                egui::Event::Key {
                    key: Key::Tab,
                    pressed,
                    modifiers,
                    ..
                } if !modifiers.command && !modifiers.alt => {
                    if *pressed {
                        commands.push(Command::Indent {
                            outdent: modifiers.shift,
                        });
                    }
                    false
                }
                egui::Event::Key {
                    key: Key::Enter,
                    pressed,
                    modifiers,
                    ..
                } if modifiers.is_none() => {
                    if *pressed {
                        commands.push(Command::Newline);
                    }
                    false
                }
                _ => true,
            });
        }
        commands
    })
}

fn looks_like_address(text: &str) -> bool {
    let text = text.trim();
    ["http://", "https://", "mailto:"]
        .iter()
        .any(|scheme| text.starts_with(scheme))
        && !text.contains(char::is_whitespace)
}

/// The toolbar's part for a markdown document.
struct MarkdownBar<'a> {
    mode: &'a mut Mode,
    /// What's showing, which may differ from `mode` if there's no room.
    shown: Mode,
    has_room: bool,
    can_go_back: bool,
    can_go_forward: bool,
    outline: &'a mut bool,
}

fn toolbar(
    ui: &mut Ui,
    buffer: &Buffer,
    path: &Path,
    wrap: &mut bool,
    markdown: Option<MarkdownBar<'_>>,
    commands: &mut Vec<Command>,
) {
    ui.horizontal(|ui| {
        let button = |ui: &mut Ui, enabled: bool, glyph: &str, tip: &str| {
            ui.add_enabled(enabled, Button::new(RichText::new(glyph).size(16.0)))
                .on_hover_text(tip)
                .clicked()
        };
        let editable = buffer.read_only.is_none();
        if button(
            ui,
            editable && buffer.dirty,
            icon::FLOPPY_DISK,
            "Save (Ctrl+S)",
        ) {
            commands.push(Command::Save);
        }
        let mut source_shown = true;
        if let Some(markdown) = markdown {
            source_shown = markdown.shown != Mode::Preview;
            ui.separator();
            ui.toggle_value(markdown.outline, RichText::new(icon::TREE_VIEW).size(16.0))
                .on_hover_text("Outline (Ctrl+Shift+O)");
            if button(
                ui,
                markdown.can_go_back,
                icon::ARROW_LEFT,
                "Back (Alt+Left)",
            ) {
                commands.push(Command::Back);
            }
            if button(
                ui,
                markdown.can_go_forward,
                icon::ARROW_RIGHT,
                "Forward (Alt+Right)",
            ) {
                commands.push(Command::Forward);
            }
            ui.separator();
            let modes = [
                (Mode::Source, icon::CODE),
                (Mode::Split, icon::COLUMNS),
                (Mode::Preview, icon::BOOK_OPEN),
            ];
            for (mode, glyph) in modes {
                if mode == Mode::Split && !markdown.has_room {
                    continue;
                }
                let selected = markdown.shown == mode;
                let label = RichText::new(glyph).size(16.0);
                if ui
                    .selectable_label(selected, label)
                    .on_hover_text(format!("{} (Ctrl+Shift+V to change)", mode.label()))
                    .clicked()
                {
                    *markdown.mode = mode;
                }
            }
        }
        ui.separator();
        if button(ui, true, icon::MAGNIFYING_GLASS, "Find (Ctrl+F)") {
            commands.push(Command::Find { replace: false });
        }
        if source_shown {
            if button(ui, editable, icon::SWAP, "Replace (Ctrl+H)") {
                commands.push(Command::Find { replace: true });
            }
            if button(ui, true, icon::LIST_NUMBERS, "Go to line (Ctrl+G)") {
                commands.push(Command::GoTo);
            }
            ui.separator();
            ui.toggle_value(wrap, RichText::new(icon::TEXT_ALIGN_LEFT).size(16.0))
                .on_hover_text("Wrap long lines (Alt+Z)");
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.add(egui::Label::new(RichText::new(path.display().to_string()).weak()).truncate());
        });
    });
}

/// Warnings about the file. Returns whether to reload it from disk.
fn banner(ui: &mut Ui, buffer: &mut Buffer, path: &Path) -> bool {
    let mut reload = false;
    let warn = ui.visuals().warn_fg_color;
    if let Some(error) = &buffer.error {
        let mut dismiss = false;
        ui.horizontal(|ui| {
            ui.colored_label(
                ui.visuals().error_fg_color,
                format!("{} {error}", icon::WARNING),
            );
            dismiss = ui.small_button(icon::X).on_hover_text("Dismiss").clicked();
        });
        if dismiss {
            buffer.error = None;
        }
    }
    match buffer.disk {
        Disk::InSync => {}
        Disk::Changed => {
            ui.horizontal(|ui| {
                ui.colored_label(
                    warn,
                    format!("{} Another program changed this file.", icon::WARNING),
                );
                if ui.button("Reload, losing my changes").clicked() {
                    reload = true;
                }
                if ui.button("Keep my version").clicked() {
                    buffer.stamp = document::stamp(path);
                    buffer.disk = Disk::InSync;
                }
            });
        }
        Disk::Deleted => {
            ui.colored_label(
                warn,
                format!(
                    "{} The file was deleted or moved. Saving creates it again.",
                    icon::WARNING
                ),
            );
        }
    }
    match buffer.read_only {
        None => {}
        Some(ReadOnly::Permissions) => {
            ui.weak(format!(
                "{} Read-only: you don't have permission to change this file.",
                icon::LOCK
            ));
        }
        Some(ReadOnly::NotUtf8) => {
            ui.weak(format!(
                "{} Read-only: the file isn't UTF-8 text, so invalid bytes are shown as �.",
                icon::LOCK
            ));
        }
    }
    reload
}

fn find_bar(ui: &mut Ui, find: &mut FindBar, editable: bool, commands: &mut Vec<Command>) {
    let field_width = 220.0;
    ui.horizontal(|ui| {
        let response = ui.add(
            TextEdit::singleline(&mut find.query)
                .hint_text("Find")
                .desired_width(field_width),
        );
        if std::mem::take(&mut find.focus) {
            response.request_focus();
        }
        if response.lost_focus() && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter)) {
            let back = ui.input(|i| i.modifiers.shift);
            commands.push(Command::FindNext { back });
            response.request_focus();
        }
        ui.toggle_value(&mut find.match_case, "Aa")
            .on_hover_text("Match case");
        ui.toggle_value(&mut find.whole_word, RichText::new("ab").underline())
            .on_hover_text("Whole words");
        if ui
            .button(icon::CARET_UP)
            .on_hover_text("Previous (Shift+F3)")
            .clicked()
        {
            commands.push(Command::FindNext { back: true });
        }
        if ui
            .button(icon::CARET_DOWN)
            .on_hover_text("Next (F3)")
            .clicked()
        {
            commands.push(Command::FindNext { back: false });
        }
        let count = find.matches.len();
        if !find.query.is_empty() {
            let more = if count >= ops::MAX_MATCHES { "+" } else { "" };
            match find.current {
                _ if count == 0 => ui.colored_label(ui.visuals().warn_fg_color, "No results"),
                Some(current) => ui.weak(format!("{} of {count}{more}", current + 1)),
                None => ui.weak(format!("{count}{more} found")),
            };
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui.button(icon::X).on_hover_text("Close (Esc)").clicked() {
                commands.push(Command::CloseBars);
            }
            if editable {
                ui.toggle_value(&mut find.show_replace, "Replace");
            }
        });
    });
    if find.show_replace && editable {
        ui.horizontal(|ui| {
            let response = ui.add(
                TextEdit::singleline(&mut find.replacement)
                    .hint_text("Replace with")
                    .desired_width(field_width),
            );
            let enter = response.lost_focus()
                && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter));
            if ui.button("Replace").clicked() || enter {
                commands.push(Command::Replace { all: false });
                if enter {
                    response.request_focus();
                }
            }
            if ui.button("Replace all").clicked() {
                commands.push(Command::Replace { all: true });
            }
        });
    }
}

fn goto_bar(ui: &mut Ui, goto: &mut GoTo, commands: &mut Vec<Command>) {
    ui.horizontal(|ui| {
        ui.label("Go to line");
        let response = ui.add(
            TextEdit::singleline(&mut goto.line)
                .hint_text("line[:column]")
                .desired_width(120.0),
        );
        if std::mem::take(&mut goto.focus) {
            response.request_focus();
        }
        if (response.lost_focus() && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter)))
            || ui.button("Go").clicked()
        {
            commands.push(Command::GoToLine);
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui.button(icon::X).on_hover_text("Close (Esc)").clicked() {
                commands.push(Command::CloseBars);
            }
        });
    });
}

/// The formatting commands for markdown source, scrolling if there's no room.
fn format_bar(ui: &mut Ui, text: &str, cursor: usize, commands: &mut Vec<Command>) {
    ScrollArea::horizontal()
        .id_salt("format-bar")
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
        .scroll_source(crate::touch::scroll_source(ui.ctx()))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let mut button = |ui: &mut Ui, glyph: &str, tip: &str, command: Command| {
                    if ui
                        .add(Button::new(RichText::new(glyph).size(16.0)))
                        .on_hover_text(tip)
                        .clicked()
                    {
                        commands.push(command);
                    }
                };
                for (glyph, tip, style) in [
                    (icon::TEXT_B, "Bold (Ctrl+B)", Inline::Bold),
                    (icon::TEXT_ITALIC, "Italic (Ctrl+I)", Inline::Italic),
                    (
                        icon::TEXT_STRIKETHROUGH,
                        "Strikethrough (Ctrl+Shift+X)",
                        Inline::Strike,
                    ),
                    (icon::HIGHLIGHTER, "Highlight", Inline::Highlight),
                    (icon::CODE_SIMPLE, "Inline code (Ctrl+E)", Inline::Code),
                ] {
                    button(ui, glyph, tip, Command::Inline(style));
                }
                ui.separator();
                button(ui, icon::LINK, "Link (Ctrl+K)", Command::Link);
                button(ui, icon::IMAGE, "Picture", Command::Image);
                ui.separator();
                let mut chosen = None;
                ui.menu_button(RichText::new(icon::TEXT_H).size(16.0), |ui| {
                    for level in 1..=6u8 {
                        if ui
                            .button(format!("Heading {level} (Ctrl+{level})"))
                            .clicked()
                        {
                            chosen = Some(Command::Heading(level));
                        }
                    }
                    if ui.button("Plain text (Ctrl+0)").clicked() {
                        chosen = Some(Command::Heading(0));
                    }
                })
                .response
                .on_hover_text("Heading");
                let mut button = |ui: &mut Ui, glyph: &str, tip: &str, command: Command| {
                    if ui
                        .add(Button::new(RichText::new(glyph).size(16.0)))
                        .on_hover_text(tip)
                        .clicked()
                    {
                        commands.push(command);
                    }
                };
                for (glyph, tip, kind) in [
                    (icon::LIST_BULLETS, "Bullets", LineKind::Bullet),
                    (icon::LIST_NUMBERS, "Numbers", LineKind::Numbered),
                    (icon::LIST_CHECKS, "Tasks", LineKind::Task),
                    (icon::QUOTES, "Quote", LineKind::Quote),
                ] {
                    button(ui, glyph, tip, Command::Lines(kind));
                }
                ui.separator();
                button(ui, icon::CODE_BLOCK, "Code block", Command::CodeBlock);
                button(ui, icon::MINUS, "Horizontal rule", Command::Rule);
                ui.menu_button(RichText::new(icon::TABLE).size(16.0), |ui| {
                    table_menu(ui, text, cursor, &mut chosen);
                })
                .response
                .on_hover_text("Table");
                commands.extend(chosen);
            });
        });
}

fn table_menu(ui: &mut Ui, text: &str, cursor: usize, chosen: &mut Option<Command>) {
    use crate::markdown::table::Align as TableAlign;
    if table::at(text, cursor).is_none() {
        if ui.button("Insert a table").clicked() {
            *chosen = Some(Command::InsertTable);
        }
        return;
    }
    for (label, action) in [
        ("Add a row below", TableAction::AddRowBelow),
        ("Add a row above", TableAction::AddRowAbove),
        ("Add a column to the right", TableAction::AddColumnRight),
        ("Add a column to the left", TableAction::AddColumnLeft),
        ("Delete this row", TableAction::DeleteRow),
        ("Delete this column", TableAction::DeleteColumn),
        (
            "Align this column left",
            TableAction::Align(TableAlign::Left),
        ),
        ("Center this column", TableAction::Align(TableAlign::Center)),
        (
            "Align this column right",
            TableAction::Align(TableAlign::Right),
        ),
        ("Line up the table", TableAction::Format),
    ] {
        if ui.button(label).clicked() {
            *chosen = Some(Command::Table(action));
        }
    }
}

/// A document's headings. Returns where the one clicked on is: its source
/// offset and anchor.
fn outline_ui(ui: &mut Ui, md: &mut MarkdownState) -> Option<(usize, String)> {
    let headings: &[Heading] = &md.view.document().headings;
    let top = md.top_byte;
    let current = headings.iter().rposition(|h| h.source.start <= top);
    let moved = std::mem::replace(&mut md.outline_current, current) != current;
    let least = headings.iter().map(|h| h.level).min().unwrap_or(1);
    let mut clicked = None;
    ui.add_space(4.0);
    if headings.is_empty() {
        ui.weak("No headings");
    }
    ScrollArea::vertical()
        .id_salt("outline")
        .auto_shrink(false)
        .scroll_source(crate::touch::scroll_source(ui.ctx()))
        .show(ui, |ui| {
            for (index, heading) in headings.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.add_space(f32::from(heading.level - least) * 12.0);
                    let selected = Some(index) == current;
                    let response =
                        ui.add(egui::Button::selectable(selected, &heading.text).truncate());
                    if selected && moved {
                        response.scroll_to_me(None);
                    }
                    if response.clicked() {
                        clicked = Some((heading.source.start, heading.slug.clone()));
                    }
                });
            }
        });
    clicked
}

fn status_bar(
    ui: &mut Ui,
    buffer: &Buffer,
    syntax: &mut Option<String>,
    id: u64,
    stats: Option<Stats>,
    source_shown: bool,
) {
    ui.horizontal(|ui| {
        let detected = buffer.detected_syntax;
        let current = syntax.as_deref().unwrap_or(detected);
        egui::ComboBox::from_id_salt(("editor-syntax", id))
            .selected_text(highlight::syntax_name(current))
            .height(400.0)
            .show_ui(ui, |ui| {
                if ui
                    .selectable_label(syntax.is_none(), format!("Detect ({detected})"))
                    .clicked()
                {
                    *syntax = None;
                }
                ui.separator();
                for name in highlight::syntax_names() {
                    if ui
                        .selectable_label(syntax.as_deref() == Some(name), name)
                        .clicked()
                    {
                        *syntax = Some(name.to_owned());
                    }
                }
            });
        if buffer.text.len() > highlight::MAX_HIGHLIGHT_BYTES {
            ui.weak("Colors off: file too large");
        }
        if let Some(stats) = stats {
            ui.weak(format!(
                "{} words, {} min read",
                stats.words,
                stats.minutes()
            ));
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if let Some((line, col, selected)) = buffer.view.status.filter(|_| source_shown) {
                ui.weak(buffer.line_ending.label());
                ui.weak(if buffer.bom {
                    "UTF-8 with BOM"
                } else {
                    "UTF-8"
                });
                ui.weak(buffer.indent.label());
                if selected > 0 {
                    ui.weak(format!("({selected} selected)"));
                }
                ui.weak(format!("Ln {line}, Col {col}"));
            }
        });
    });
}

struct TextArea<'a> {
    id: Id,
    wrap: bool,
    colors: &'a ThemeColors,
    /// Find matches, and which one is selected.
    matches: Option<(&'a [Range<usize>], Option<usize>)>,
    scroll_to_cursor: bool,
    /// Scroll to this height.
    offset: Option<f32>,
}

/// The text with line numbers beside it.
fn text_area(ui: &mut Ui, buffer: &mut Buffer, area: TextArea<'_>) -> Option<SourceScroll> {
    let TextArea {
        id,
        wrap,
        colors,
        matches,
        scroll_to_cursor,
        offset,
    } = area;
    let font = TextStyle::Monospace.resolve(ui.style());
    let rect = ui.max_rect();
    let lines = buffer
        .view
        .rows
        .as_ref()
        .and_then(|(_, rows)| rows.line.last().copied())
        .unwrap_or(1);
    let digit = ui.fonts_mut(|f| f.glyph_width(&font, '0'));
    let gutter_width = lines.to_string().len().max(3) as f32 * digit + 20.0;
    let gutter = rect.with_max_x(rect.min.x + gutter_width);
    let text_rect = rect.with_min_x(gutter.max.x);

    let editable = buffer.read_only.is_none();
    let mut placed = None;
    let mut viewport = text_rect;
    let mut scrolled = (0.0, 0.0);
    ui.scope_builder(UiBuilder::new().max_rect(text_rect), |ui| {
        let visuals = ui.visuals_mut();
        if let Some(caret) = colors.caret {
            visuals.text_cursor.stroke.color = caret;
        }
        if let Some(selection) = colors.selection {
            visuals.selection.bg_fill = selection;
        }
        let scroll = if wrap {
            ScrollArea::vertical()
        } else {
            ScrollArea::both()
        };
        let scroll = match offset {
            Some(offset) => scroll.vertical_scroll_offset(offset),
            None => scroll,
        };
        let output = scroll
            .id_salt(("editor-scroll", id))
            .auto_shrink(false)
            .show(ui, |ui| {
                let behind = ui.painter().add(Shape::Noop);
                let view = &mut buffer.view;
                let mut layouter = |ui: &Ui, text: &dyn TextBuffer, wrap_width: f32| {
                    let width = if wrap { wrap_width } else { f32::INFINITY };
                    view.layout(ui.ctx(), text.as_str(), &font, colors.foreground, width)
                };
                let mut read_only = buffer.text.as_str();
                let text: &mut dyn TextBuffer = if editable {
                    &mut buffer.text
                } else {
                    &mut read_only
                };
                let output = TextEdit::multiline(text)
                    .id(id)
                    .code_editor()
                    .frame(egui::Frame::NONE)
                    .desired_width(f32::INFINITY)
                    .min_size(ui.available_size())
                    .layouter(&mut layouter)
                    .show(ui);
                let galley = output.galley.clone();
                let origin = output.galley_pos.to_vec2();
                let cursor = output.cursor_range;
                let clip = ui.clip_rect();

                let mut shapes = Vec::new();
                if let Some(cursor) = cursor
                    && cursor.is_empty()
                    && let Some(color) = colors.line_highlight
                {
                    let row = galley.layout_from_cursor(cursor.primary).row;
                    if let Some(row) = galley.rows.get(row) {
                        let y = origin.y + row.min_y()..=origin.y + row.max_y();
                        shapes.push(Shape::rect_filled(
                            Rect::from_x_y_ranges(clip.x_range(), y),
                            0.0,
                            color,
                        ));
                    }
                }
                if let Some((matches, current)) = matches {
                    let visible = galley.cursor_from_pos(clip.min.to_vec2() - origin).index.0
                        ..galley.cursor_from_pos(clip.max.to_vec2() - origin).index.0 + 1;
                    let first = matches.partition_point(|m| m.end < visible.start);
                    let fill = colors
                        .find_highlight
                        .map_or(ui.visuals().selection.bg_fill, |c| c.gamma_multiply(0.6));
                    for (index, m) in matches.iter().enumerate().skip(first) {
                        if m.start > visible.end {
                            break;
                        }
                        let start = galley.pos_from_cursor(CCursor::new(m.start));
                        let end = galley.pos_from_cursor(CCursor::new(m.end));
                        let rect = if (start.min.y - end.min.y).abs() < 1.0 {
                            Rect::from_min_max(start.min, end.max)
                        } else {
                            Rect::from_min_max(
                                start.min,
                                egui::pos2(galley.rect.max.x, start.max.y),
                            )
                        }
                        .translate(origin);
                        if Some(index) == current {
                            shapes.push(Shape::rect_stroke(
                                rect.expand(1.0),
                                2.0,
                                egui::Stroke::new(1.5, colors.foreground),
                                egui::StrokeKind::Outside,
                            ));
                        }
                        shapes.push(Shape::rect_filled(rect, 2.0, fill));
                    }
                }
                ui.painter().set(behind, Shape::Vec(shapes));

                if scroll_to_cursor && let Some(cursor) = cursor {
                    let rect = galley.pos_from_cursor(cursor.primary).translate(origin);
                    ui.scroll_to_rect(rect.expand(24.0), Some(Align::Center));
                }
                (galley, origin, cursor)
            });
        viewport = output.inner_rect;
        scrolled = (output.state.offset.y, output.content_size.y);
        placed = Some(output.inner);
    });
    let (galley, origin, cursor) = placed?;
    let source_scroll = SourceScroll {
        offset: scrolled.0,
        viewport: viewport.height(),
        content: scrolled.1,
        galley: galley.clone(),
    };

    // Line numbers for the rows on screen.
    let rows = buffer.view.rows(&galley);
    let cursor_row = cursor.map(|c| galley.layout_from_cursor(c.primary).row);
    if let Some(row) = cursor_row
        && let Some(cursor) = cursor
        && row < rows.line.len()
    {
        let line_start = (0..=row).rev().find(|&r| rows.starts_line[r]).unwrap_or(0);
        let col = cursor.primary.index.0 - rows.first_char[line_start] + 1;
        let selected = cursor.primary.index.0.abs_diff(cursor.secondary.index.0);
        buffer.view.status = Some((rows.line[row], col, selected));
    }
    let rows = buffer.view.rows(&galley);
    let current_line = cursor_row.and_then(|r| rows.line.get(r).copied());
    let painter = ui.painter_at(gutter.with_min_y(viewport.min.y).with_max_y(viewport.max.y));
    let weak = colors
        .gutter_foreground
        .unwrap_or(ui.visuals().weak_text_color());
    let first = galley
        .rows
        .partition_point(|row| origin.y + row.max_y() < viewport.min.y);
    for (index, row) in galley.rows.iter().enumerate().skip(first) {
        let y = origin.y + row.min_y();
        if y > viewport.max.y {
            break;
        }
        if !rows.starts_line[index] {
            continue;
        }
        let line = rows.line[index];
        painter.text(
            egui::pos2(gutter.max.x - 12.0, y),
            Align2::RIGHT_TOP,
            line.to_string(),
            font.clone(),
            if Some(line) == current_line {
                colors.foreground
            } else {
                weak
            },
        );
    }
    Some(source_scroll)
}

struct Rendered<'a> {
    id: Id,
    path: &'a Path,
    theme: &'a str,
    colors: &'a ThemeColors,
    editable: bool,
    prefs: &'a markdown::Prefs,
    /// Takes the keys that scroll, as no text field does.
    keyboard: bool,
    hits: Option<(&'a [Hit], Option<usize>)>,
}

/// The rendered document, and the bar offering its remote pictures.
fn rendered(
    ui: &mut Ui,
    md: &mut MarkdownState,
    rendered: Rendered<'_>,
    images: &mut markdown::Images,
) -> Vec<Click> {
    let allow_remote = match rendered.prefs.remote_images {
        markdown::RemoteImages::Always => true,
        markdown::RemoteImages::Never => false,
        markdown::RemoteImages::Ask => md.allow_remote.contains(rendered.path),
    };
    if md.view.has_remote_images()
        && !allow_remote
        && rendered.prefs.remote_images == markdown::RemoteImages::Ask
    {
        Panel::top(rendered.id.with("remote-images")).show(ui, |ui| {
            ui.horizontal(|ui| {
                if ui
                    .button(format!("{} Load", icon::IMAGE))
                    .on_hover_text("Fetching them tells those servers it's being read")
                    .clicked()
                {
                    md.allow_remote.insert(rendered.path.to_path_buf());
                }
                ui.add(
                    egui::Label::new(
                        RichText::new("Pictures on other servers aren't loaded.").weak(),
                    )
                    .truncate(),
                );
            });
        });
    }
    let dir = markdown::view::base_dir(rendered.path);
    let mut scroll_to = md.preview_to.take();
    if let Some(anchor) = &md.anchor
        && !md.view.layout().items.is_empty()
    {
        scroll_to = md.view.anchor_offset(anchor).or(scroll_to);
        md.anchor = None;
        md.preview_jumped = true;
    }
    if let Some(offset) = md.restore_offset
        && !md.view.layout().items.is_empty()
    {
        scroll_to = Some(offset);
        md.restore_offset = None;
        md.preview_jumped = true;
    }
    if rendered.keyboard {
        scroll_to = scroll_keys(ui.ctx(), &md.view).or(scroll_to);
    }
    let options = markdown::Options {
        dir: &dir,
        syntax_theme: rendered.theme,
        code_fg: rendered.colors.foreground,
        code_bg: rendered.colors.background,
        allow_remote,
        editable: rendered.editable,
        scroll_to,
        compact: false,
        hits: rendered.hits,
    };
    md.view.show(ui, rendered.id, &options, images)
}

/// Where the scrolling keys go from here, if one was pressed.
fn scroll_keys(ctx: &Context, view: &MdView) -> Option<f32> {
    if ctx.egui_wants_keyboard_input() {
        return None;
    }
    let (line, page) = (40.0, (view.viewport_height - 40.0).max(40.0));
    let delta = ctx.input(|i| {
        let pressed = |key| i.key_pressed(key) && i.modifiers.is_none();
        if pressed(Key::ArrowDown) {
            Some(line)
        } else if pressed(Key::ArrowUp) {
            Some(-line)
        } else if pressed(Key::PageDown) || pressed(Key::Space) {
            Some(page)
        } else if pressed(Key::PageUp) {
            Some(-page)
        } else if pressed(Key::Home) {
            Some(f32::NEG_INFINITY)
        } else if pressed(Key::End) {
            Some(f32::INFINITY)
        } else {
            None
        }
    })?;
    let bottom = (view.content_height() - view.viewport_height).max(0.0);
    Some((view.offset + delta).clamp(0.0, bottom))
}

/// Keeps Split's panes showing the same part of the document: the pane under
/// the pointer leads, and the source pane leads when it's being typed in.
fn sync_panes(
    ctx: &Context,
    md: &mut MarkdownState,
    text: &str,
    version: u64,
    source: &SourceScroll,
    preview_rect: Rect,
) {
    let jumped = std::mem::take(&mut md.preview_jumped);
    let preview_leads = jumped
        || ctx
            .pointer_hover_pos()
            .is_some_and(|pointer| preview_rect.contains(pointer));
    let source_moved = (source.offset - md.source_was).abs() > 0.5;
    let preview_moved = (md.view.offset - md.preview_was).abs() > 0.5;
    let edited = md.version_was != version;
    md.version_was = version;
    let view = &md.view;
    if preview_leads && (preview_moved || jumped) {
        let bottom = (view.content_height() - view.viewport_height).max(0.0);
        let along_bottom = view.offset >= bottom - 1.0;
        let to = if along_bottom {
            (source.content - source.viewport).max(0.0)
        } else {
            let byte = view.source_of_offset(view.offset);
            let chars = text.get(..byte).map_or(0, |t| t.chars().count());
            source.galley.pos_from_cursor(CCursor::new(chars)).min.y
        };
        md.source_to = Some(to);
        md.source_was = to;
        md.preview_was = view.offset;
        ctx.request_repaint();
    } else if !preview_leads && (source_moved || edited) {
        let along_bottom = source.offset >= source.content - source.viewport - 1.0;
        let to = if along_bottom && source.offset > 0.0 {
            (view.content_height() - view.viewport_height).max(0.0)
        } else {
            let char = source
                .galley
                .cursor_from_pos(egui::vec2(0.0, source.offset))
                .index
                .0;
            view.offset_of_source(ops::char_to_byte(text, char))
        };
        md.preview_to = Some(to);
        md.preview_was = to;
        md.source_was = source.offset;
        ctx.request_repaint();
    } else {
        md.source_was = source.offset;
        md.preview_was = view.offset;
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use egui::{Event, RawInput, pos2, vec2};

    use super::*;
    use crate::{images::ImageCache, jobs::Jobs, preview::TextCache, video::Videos};

    struct Harness {
        ctx: Context,
        shared: Shared,
        tab: EditorTab,
        path: PathBuf,
        dir: PathBuf,
    }

    impl Harness {
        fn new(name: &str, contents: &str) -> Self {
            Self::with_file(name, "main.rs", contents)
        }

        fn with_file(name: &str, file: &str, contents: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("egui-files-editor-{name}-{}", std::process::id()));
            let _ = disk::remove_dir_all(&dir);
            disk::create_dir_all(&dir).unwrap();
            let path = dir.join(file);
            disk::write(&path, contents).unwrap();
            let ctx = Context::default();
            let shared = Shared {
                show_hidden: false,
                show_thumbnails: false,
                icon_size: 64.0,
                clipboard: None,
                focused_tab: Some(1),
                preview_shown: false,
                app_actions: Vec::new(),
                usage: Default::default(),
                properties: Default::default(),
                images: ImageCache::new(&ctx),
                videos: Videos::new(&ctx),
                texts: TextCache::default(),
                markdown_images: Default::default(),
                jobs: Jobs::new(&ctx),
                archives: crate::archive::Listings::new(&ctx),
                archive_prefs: Default::default(),
                open_with: Default::default(),
                editor: Default::default(),
            };
            let mut harness = Self {
                ctx,
                shared,
                tab: EditorTab::new(1, path.clone()),
                path,
                dir,
            };
            harness.until(|h| matches!(h.tab.doc, Doc::Ready(_)));
            harness
        }

        fn frame(&mut self, events: Vec<Event>) {
            let input = RawInput {
                screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(900.0, 600.0))),
                events,
                ..Default::default()
            };
            let mut output = self
                .ctx
                .run_ui(input, |ui| self.tab.ui(ui, &mut self.shared));
            output.textures_delta.clear();
        }

        /// Runs frames until `done`. Waits by the clock, not a count of
        /// frames: the first highlight loads the syntaxes, which takes
        /// seconds in a debug build with the other tests running.
        fn until(&mut self, done: impl Fn(&Self) -> bool) {
            let start = web_time::Instant::now();
            while start.elapsed() < Duration::from_secs(30) {
                self.frame(Vec::new());
                if done(self) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            panic!("timed out");
        }

        fn key(&mut self, modifiers: Modifiers, key: Key) {
            self.frame(vec![Event::Key {
                key,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers,
            }]);
            self.frame(Vec::new());
        }

        fn type_text(&mut self, text: &str) {
            self.frame(vec![Event::Text(text.to_owned())]);
            self.frame(Vec::new());
        }

        fn select(&mut self, range: Range<usize>) {
            self.tab.select = Some(range);
            self.frame(Vec::new());
        }

        fn buffer(&self) -> &Buffer {
            match &self.tab.doc {
                Doc::Ready(buffer) => buffer,
                _ => panic!("not loaded"),
            }
        }

        fn text(&self) -> &str {
            &self.buffer().text
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            let _ = disk::remove_dir_all(&self.dir);
        }
    }

    const CODE: &str = "fn main() {\n    let x = 1;\n}\n";

    fn press(key: Key, modifiers: Modifiers) -> Event {
        Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        }
    }

    fn text_of(h: &Harness) -> String {
        match &h.tab.doc {
            Doc::Ready(buffer) => buffer.text.clone(),
            _ => panic!("not loaded"),
        }
    }

    /// New fonts mean a new glyph texture: text kept from before would draw
    /// garbage from it, and fonts can arrive any time after startup.
    #[test]
    fn source_is_laid_out_again_when_fonts_change() {
        let mut h = Harness::new("font-change", "fn main() {}\n");
        h.frame(vec![]);
        h.frame(vec![]);
        let galley = |h: &Harness| match &h.tab.doc {
            Doc::Ready(buffer) => buffer.view.galley.as_ref().unwrap().1.clone(),
            _ => panic!("not loaded"),
        };
        let before = galley(&h);
        h.frame(vec![]);
        assert!(
            Arc::ptr_eq(&before, &galley(&h)),
            "kept while nothing changes"
        );
        crate::theme::fonts::set_fonts(&h.ctx, egui::FontDefinitions::default());
        h.frame(vec![]);
        h.frame(vec![]);
        assert!(!Arc::ptr_eq(&before, &galley(&h)));
    }

    fn select(h: &mut Harness, range: Range<usize>) {
        h.tab.select = Some(range);
        h.tab.focus_text = true;
        h.frame(vec![]);
        h.frame(vec![]);
    }

    #[test]
    fn shortcuts_format_the_selection() {
        let mut h = Harness::with_file("md-format", "a.md", "one two\n");
        h.tab.mode = Mode::Source;
        h.frame(vec![]);
        select(&mut h, 4..7);
        h.frame(vec![press(Key::B, Modifiers::COMMAND)]);
        h.frame(vec![]);
        assert_eq!(text_of(&h), "one **two**\n");
        assert_eq!(h.tab.selection(&h.ctx), 6..9, "still selected");
        h.frame(vec![press(Key::B, Modifiers::COMMAND)]);
        h.frame(vec![]);
        assert_eq!(text_of(&h), "one two\n");
        h.frame(vec![press(Key::Num2, Modifiers::COMMAND)]);
        h.frame(vec![]);
        assert_eq!(text_of(&h), "## one two\n");
    }

    #[test]
    fn formatting_is_only_for_markdown() {
        let mut h = Harness::new("md-format-other", "one two\n");
        h.frame(vec![]);
        select(&mut h, 4..7);
        h.frame(vec![press(Key::B, Modifiers::COMMAND)]);
        h.frame(vec![]);
        assert_eq!(text_of(&h), "one two\n");
    }

    #[test]
    fn formatting_is_one_undo_step() {
        let mut h = Harness::with_file("md-undo", "a.md", "one\n");
        h.tab.mode = Mode::Source;
        h.frame(vec![]);
        select(&mut h, 0..3);
        h.frame(vec![press(Key::B, Modifiers::COMMAND)]);
        h.frame(vec![]);
        assert_eq!(text_of(&h), "**one**\n");
        h.frame(vec![press(Key::Z, Modifiers::COMMAND)]);
        h.frame(vec![]);
        assert_eq!(text_of(&h), "one\n");
    }

    #[test]
    fn enter_continues_a_list_and_tab_moves_through_a_table() {
        let mut h = Harness::with_file("md-keys-edit", "a.md", "- one\n");
        h.tab.mode = Mode::Source;
        h.frame(vec![]);
        select(&mut h, 5..5);
        h.frame(vec![press(Key::Enter, Modifiers::NONE)]);
        h.frame(vec![]);
        assert_eq!(text_of(&h), "- one\n- \n");

        let mut h =
            Harness::with_file("md-keys-table", "a.md", "| a | b |\n|---|---|\n| 1 | 2 |\n");
        h.tab.mode = Mode::Source;
        h.frame(vec![]);
        select(&mut h, 2..2);
        h.frame(vec![press(Key::Tab, Modifiers::NONE)]);
        h.frame(vec![]);
        assert!(text_of(&h).contains("| --- | --- |"), "{}", text_of(&h));
        let selection = h.tab.selection(&h.ctx);
        assert_eq!(selection.len(), 1);
    }

    #[test]
    fn pasting_an_address_over_text_makes_a_link() {
        let mut h = Harness::with_file("md-paste", "a.md", "see this\n");
        h.tab.mode = Mode::Source;
        h.frame(vec![]);
        select(&mut h, 4..8);
        h.frame(vec![Event::Paste("https://a.com".into())]);
        h.frame(vec![]);
        assert_eq!(text_of(&h), "see [this](https://a.com)\n");
        select(&mut h, 0..0);
        h.frame(vec![Event::Paste("https://b.com".into())]);
        h.frame(vec![]);
        assert!(
            text_of(&h).starts_with("https://b.com"),
            "nothing selected: plain"
        );
        h.frame(vec![Event::Paste(" other words".into())]);
        h.frame(vec![]);
        assert!(
            text_of(&h).contains(" other words"),
            "other text pastes as ever"
        );
    }

    #[test]
    fn dropped_pictures_are_added_at_the_end_of_a_document_being_read() {
        let mut h = Harness::with_file("md-drop", "a.md", "text\n");
        h.tab.mode = Mode::Preview;
        h.frame(vec![]);
        h.frame(vec![]);
        let picture = h.dir.join("p.png");
        disk::write(&picture, "pixels").unwrap();
        assert!(h.tab.takes_files());
        let ctx = h.ctx.clone();
        h.tab.drop_files(&ctx, vec![picture]);
        assert_eq!(text_of(&h), "text\n\n![p](p.png)\n");
        assert!(
            h.tab.is_dirty() || {
                h.frame(vec![]);
                h.tab.is_dirty()
            }
        );
    }

    #[test]
    fn other_files_dont_take_drops() {
        let mut h = Harness::new("md-no-drop", "fn main() {}\n");
        h.frame(vec![]);
        assert!(!h.tab.takes_files());
    }

    #[test]
    fn find_in_a_rendered_document_searches_what_it_says() {
        let source =
            "# Title\n\nA **word** here, and a [word](address) there.\n\n`address` once.\n";
        let mut h = Harness::with_file("md-find", "a.md", source);
        h.tab.mode = Mode::Preview;
        h.frame(vec![]);
        h.frame(vec![press(Key::F, Modifiers::COMMAND)]);
        h.frame(vec![]);
        let find = h.tab.find.as_mut().expect("the bar opens in Preview");
        find.query = "word".to_owned();
        h.frame(vec![]);
        h.frame(vec![]);
        let find = h.tab.find.as_ref().unwrap();
        assert_eq!(find.hits.len(), 2);
        assert_eq!(find.current, Some(0));
        h.frame(vec![press(Key::F3, Modifiers::NONE)]);
        h.frame(vec![]);
        assert_eq!(h.tab.find.as_ref().unwrap().current, Some(1));
        h.frame(vec![press(Key::F3, Modifiers::NONE)]);
        h.frame(vec![]);
        assert_eq!(h.tab.find.as_ref().unwrap().current, Some(0), "wraps");

        h.tab.find.as_mut().unwrap().query = "address".to_owned();
        h.frame(vec![]);
        h.frame(vec![]);
        assert_eq!(
            h.tab.find.as_ref().unwrap().hits.len(),
            1,
            "the link's isn't on the page"
        );

        // In the source, the same search sees the markup.
        h.tab.mode = Mode::Source;
        h.frame(vec![]);
        h.frame(vec![]);
        let find = h.tab.find.as_ref().unwrap();
        assert!(find.hits.is_empty());
        assert_eq!(find.matches.len(), 2);
        assert_eq!(text_of(&h), source, "searching changes nothing");
    }

    #[test]
    fn closing_find_in_preview_leaves_the_source_selection_alone() {
        let mut h = Harness::with_file("md-find-close", "a.md", "word word\n");
        h.tab.mode = Mode::Preview;
        h.frame(vec![]);
        h.frame(vec![press(Key::F, Modifiers::COMMAND)]);
        h.tab.find.as_mut().unwrap().query = "word".to_owned();
        h.frame(vec![]);
        h.frame(vec![press(Key::Escape, Modifiers::NONE)]);
        h.frame(vec![]);
        assert!(h.tab.find.is_none());
        assert_eq!(h.tab.select, None);
    }

    #[test]
    fn the_outline_lists_headings_and_goes_to_them() {
        let source = "# One\n\n## Two\n\ntext\n";
        let mut h = Harness::with_file("md-outline", "a.md", source);
        h.tab.mode = Mode::Source;
        h.frame(vec![]);
        h.frame(vec![press(Key::O, Modifiers::COMMAND | Modifiers::SHIFT)]);
        h.frame(vec![]);
        assert!(h.tab.outline);
        let headings = &h.tab.md.view.document().headings;
        assert_eq!(headings.len(), 2);
        assert_eq!(headings[1].slug, "two");
        assert_eq!(h.tab.md.view.stats().words, 3);
    }

    #[test]
    fn keys_in_preview_leave_the_hidden_text_alone() {
        let source = "- one\n- two\n";
        let mut h = Harness::with_file("md-keys", "note.md", source);
        h.tab.mode = Mode::Preview;
        h.tab.focus_text = true;
        h.frame(vec![]);
        h.frame(vec![
            Event::Text("x".into()),
            press(Key::Enter, Modifiers::NONE),
            press(Key::Tab, Modifiers::NONE),
            press(Key::Slash, Modifiers::COMMAND),
            press(Key::Backspace, Modifiers::NONE),
        ]);
        h.frame(vec![]);
        assert_eq!(text_of(&h), source);
        assert!(!h.tab.is_dirty());
    }

    #[test]
    fn other_files_stay_source_whatever_their_mode_says() {
        let mut h = Harness::new("md-other", "fn main() {}\n");
        h.tab.mode = Mode::Preview;
        h.frame(vec![]);
        h.frame(vec![Event::Text("x".into())]);
        h.frame(vec![]);
        assert!(text_of(&h).contains('x'), "still edits");
    }

    #[test]
    fn ticking_a_task_changes_its_mark_and_nothing_else() {
        let source = "# T\n\n- [ ] é one\n- [x] two  \n\t\nend\n";
        let mut h = Harness::with_file("md-task", "note.md", source);
        h.tab.mode = Mode::Preview;
        h.frame(vec![]);
        h.frame(vec![]);
        let at = source.find("[ ]").unwrap();
        let ctx = h.ctx.clone();
        h.tab.clicked(
            &ctx,
            Click::Task(markdown::parse::Task {
                checked: false,
                source: at..at + 3,
            }),
            &mut Vec::new(),
        );
        h.frame(vec![]);
        h.frame(vec![]);
        assert_eq!(text_of(&h), source.replacen("[ ]", "[x]", 1));
        assert!(h.tab.is_dirty());
    }

    fn link(url: &str) -> Click {
        Click::Link {
            link: markdown::parse::Link {
                url: url.to_owned(),
                title: String::new(),
                wiki: false,
            },
            new_tab: false,
        }
    }

    #[test]
    fn links_go_to_documents_and_back() {
        let mut h = Harness::with_file("md-links", "a.md", "# A\n");
        let other = h.dir.join("b.md");
        disk::write(&other, "# B\n").unwrap();
        h.tab.mode = Mode::Preview;
        h.frame(vec![]);
        let ctx = h.ctx.clone();
        let mut actions = Vec::new();

        h.tab.clicked(&ctx, link("nowhere.md"), &mut actions);
        assert_eq!(h.tab.edit, h.path, "a missing document goes nowhere");
        assert!(matches!(&h.tab.doc, Doc::Ready(b) if b.error.is_some()));

        h.tab.clicked(&ctx, link("b.md"), &mut actions);
        assert_eq!(h.tab.edit, other);
        h.until(|h| matches!(&h.tab.doc, Doc::Ready(b) if b.text == "# B\n"));
        assert!(actions.is_empty());

        h.tab.go_back(true);
        assert_eq!(h.tab.edit, h.path);
        h.until(|h| matches!(&h.tab.doc, Doc::Ready(b) if b.text == "# A\n"));
        h.tab.go_back(false);
        assert_eq!(h.tab.edit, other);
    }

    #[test]
    fn a_link_leaves_unsaved_changes_in_their_tab() {
        let mut h = Harness::with_file("md-dirty", "a.md", "# A\n");
        disk::write(h.dir.join("b.md"), "# B\n").unwrap();
        h.tab.mode = Mode::Split;
        h.tab.focus_text = true;
        h.frame(vec![]);
        h.frame(vec![Event::Text("x".into())]);
        h.frame(vec![]);
        assert!(h.tab.is_dirty());
        let ctx = h.ctx.clone();
        let mut actions = Vec::new();
        h.tab.clicked(&ctx, link("b.md"), &mut actions);
        assert_eq!(h.tab.edit, h.path);
        assert!(matches!(
            actions.as_slice(),
            [AppAction::OpenMarkdown { path, .. }] if path.ends_with("b.md")
        ));
        // And Back doesn't throw them away either.
        h.tab.md.back.push(Visited {
            path: h.dir.join("b.md"),
            offset: 0.0,
        });
        h.tab.go_back(true);
        assert_eq!(h.tab.edit, h.path);
    }

    #[test]
    fn a_link_to_a_program_reveals_it() {
        let mut h = Harness::with_file("md-program", "a.md", "# A\n");
        disk::write(h.dir.join("setup.bat"), "echo hi\n").unwrap();
        h.tab.mode = Mode::Preview;
        h.frame(vec![]);
        let ctx = h.ctx.clone();
        let mut actions = Vec::new();
        h.tab.clicked(&ctx, link("setup.bat"), &mut actions);
        assert!(matches!(
            actions.as_slice(),
            [AppAction::Reveal(path)] if path.ends_with("setup.bat")
        ));
    }

    #[test]
    fn opening_reads_and_editing_edits() {
        let mut tab = EditorTab::new_markdown(1, "/x/a.md".into());
        assert_eq!(tab.mode, Mode::Preview);
        tab.show_for_editing(Mode::Split);
        assert_eq!(tab.mode, Mode::Split);
        // Not taken out of what the user chose.
        tab.show_for_editing(Mode::Source);
        assert_eq!(tab.mode, Mode::Split);
        tab.mode = Mode::Source;
        tab.show_for_editing(Mode::Split);
        assert_eq!(tab.mode, Mode::Source);
        // What Edit makes of a new tab.
        let mut tab = EditorTab::new(1, "/x/a.md".into());
        tab.show_for_editing(Mode::Split);
        assert_eq!(tab.mode, Mode::Split);
    }

    #[test]
    fn typing_keeps_indentation_and_saves() {
        let mut h = Harness::new("typing", CODE);
        // After "let x = 1;".
        let end_of_let = CODE.find(";\n").unwrap() + 1;
        h.select(end_of_let..end_of_let);
        h.key(Modifiers::NONE, Key::Enter);
        h.type_text("y();");
        assert_eq!(h.text(), "fn main() {\n    let x = 1;\n    y();\n}\n");
        assert!(h.tab.is_dirty());
        assert!(h.tab.title().ends_with('●'));

        h.key(Modifiers::COMMAND, Key::S);
        assert!(!h.tab.is_dirty());
        assert_eq!(disk::read_to_string(&h.path).unwrap(), h.text());
    }

    #[test]
    fn tab_indents_and_ctrl_slash_comments_with_undo() {
        let mut h = Harness::new("indent", CODE);
        // Select the first two lines.
        h.select(0..20);
        h.key(Modifiers::NONE, Key::Tab);
        assert_eq!(h.text(), "    fn main() {\n        let x = 1;\n}\n");
        h.key(Modifiers::SHIFT, Key::Tab);
        assert_eq!(h.text(), CODE);

        h.key(Modifiers::COMMAND, Key::Slash);
        assert_eq!(h.text(), "// fn main() {\n//     let x = 1;\n}\n");
        h.key(Modifiers::COMMAND, Key::Z);
        assert_eq!(h.text(), CODE);
    }

    #[test]
    fn find_selects_matches() {
        let mut h = Harness::new("find", "a x b x c x\n");
        h.key(Modifiers::COMMAND, Key::F);
        h.type_text("x");
        let find = h.tab.find.as_ref().expect("find bar open");
        assert_eq!(find.query, "x");
        assert_eq!(find.matches, [2..3, 6..7, 10..11]);
        // Typing the search selected the first match.
        assert_eq!(find.current, Some(0));

        h.key(Modifiers::NONE, Key::F3);
        h.key(Modifiers::NONE, Key::F3);
        assert_eq!(h.tab.find.as_ref().unwrap().current, Some(2));
        h.key(Modifiers::NONE, Key::F3);
        assert_eq!(h.tab.find.as_ref().unwrap().current, Some(0));
        h.key(Modifiers::SHIFT, Key::F3);
        assert_eq!(h.tab.find.as_ref().unwrap().current, Some(2));

        // Replacing one moves on to the next.
        h.tab.find.as_mut().unwrap().replacement = "yy".into();
        h.tab.run(&h.ctx.clone(), Command::Replace { all: false });
        h.frame(Vec::new());
        h.frame(Vec::new());
        assert_eq!(h.text(), "a x b x c yy\n");
        assert_eq!(h.tab.find.as_ref().unwrap().current, Some(0));

        // Replace all, as one undo step.
        h.key(Modifiers::NONE, Key::Escape);
        assert!(h.tab.find.is_none());
        // Back in the text, with the match selected.
        assert_eq!(h.tab.selection(&h.ctx), 2..3);

        h.key(Modifiers::COMMAND, Key::H);
        h.tab.find.as_mut().unwrap().replacement = "z".into();
        h.tab.run(&h.ctx.clone(), Command::Replace { all: true });
        h.frame(Vec::new());
        assert_eq!(h.text(), "a z b z c yy\n");
        // One undo step.
        h.key(Modifiers::NONE, Key::Escape);
        h.key(Modifiers::COMMAND, Key::Z);
        assert_eq!(h.text(), "a x b x c yy\n");
    }

    #[test]
    fn goes_to_line() {
        let mut h = Harness::new("goto", CODE);
        h.key(Modifiers::COMMAND, Key::G);
        h.type_text("2:5");
        h.key(Modifiers::NONE, Key::Enter);
        assert!(h.tab.goto.is_none());
        // Line 2, column 5: after the indentation.
        assert_eq!(h.tab.selection(&h.ctx), 16..16);
    }

    #[test]
    fn reloads_external_changes_unless_edited() {
        let mut h = Harness::new("reload", CODE);
        std::thread::sleep(Duration::from_millis(20));
        disk::write(&h.path, "changed\n").unwrap();
        h.tab.mark_stale();
        h.until(|h| h.text() == "changed\n");
        assert!(!h.tab.is_dirty());

        h.select(0..0);
        h.type_text("my ");
        assert!(h.tab.is_dirty());
        std::thread::sleep(Duration::from_millis(20));
        disk::write(&h.path, "changed again\n").unwrap();
        h.tab.mark_stale();
        h.frame(Vec::new());
        assert!(h.buffer().disk == Disk::Changed);
        assert_eq!(h.text(), "my changed\n");

        disk::remove_file(&h.path).unwrap();
        h.tab.mark_stale();
        h.frame(Vec::new());
        assert!(h.buffer().disk == Disk::Deleted);
        h.key(Modifiers::COMMAND, Key::S);
        assert_eq!(disk::read_to_string(&h.path).unwrap(), "my changed\n");
        assert!(h.buffer().disk == Disk::InSync);
    }

    #[test]
    fn long_lines_scroll_or_wrap() {
        let long = "x".repeat(600);
        let mut h = Harness::new("wrap", &format!("{long}\nshort\n"));
        let text_width = |h: &Harness| {
            h.ctx.viewport(|viewport| {
                let widgets = &viewport.prev_pass.widgets;
                widgets
                    .get(text_id(1))
                    .map(|w| w.rect.width())
                    .expect("text field")
            })
        };
        let rows = |h: &Harness| h.buffer().view.galley.as_ref().unwrap().1.rows.len();
        h.frame(Vec::new());
        assert!(
            text_width(&h) > 900.0,
            "scrolls sideways: {}",
            text_width(&h)
        );
        assert_eq!(rows(&h), 3);

        h.key(Modifiers::ALT, Key::Z);
        assert!(h.tab.wrap);
        assert!(text_width(&h) < 900.0);
        assert!(rows(&h) > 3);
        // Wrapped rows don't count as lines.
        let index = RowIndex::new(&h.buffer().view.galley.as_ref().unwrap().1);
        assert_eq!(index.line.last(), Some(&3));
        assert_eq!(index.starts_line.iter().filter(|&&s| s).count(), 3);

        h.select(605..605);
        assert_eq!(h.buffer().view.status, Some((2, 5, 0)));
    }

    #[test]
    fn highlights_while_editing() {
        let mut h = Harness::new("colors", CODE);
        // Colored, not just the empty placeholders lines get until then.
        h.until(|h| {
            let spans = &h.buffer().view.spans;
            spans.len() == 3 && spans.iter().all(|line| !line.is_empty())
        });
        let before = h.buffer().view.spans.clone();
        h.select(0..0);
        h.key(Modifiers::NONE, Key::Enter);
        h.until(|h| h.buffer().view.spans.len() == 4 && h.buffer().view.spans[1] == before[0]);
        assert!(h.buffer().view.spans[0].is_empty());
    }
}
