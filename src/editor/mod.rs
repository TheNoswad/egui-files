//! A tab for editing a text file, with syntax highlighting, find and
//! replace, and reloading when another program changes the file.

pub mod document;
pub mod highlight;
pub mod ops;
pub mod table;

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
use crate::tab::Shared;

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    pub dark_theme: String,
    pub light_theme: String,
    /// Use the app theme's suggestion instead of the named theme.
    pub dark_matches_app: bool,
    pub light_matches_app: bool,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            dark_theme: highlight::DEFAULT_DARK_THEME.to_owned(),
            light_theme: highlight::DEFAULT_LIGHT_THEME.to_owned(),
            dark_matches_app: true,
            light_matches_app: true,
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

    #[serde(skip)]
    doc: Doc,
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
}

impl View {
    /// Notices a changed text and passes it on to the highlighter.
    fn sync(&mut self, text: &str) {
        if *self.shown == *text {
            return;
        }
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
        };
        if let Some((cached, galley)) = &self.galley {
            if *cached == key {
                return galley.clone();
            }
            if (cached.width, cached.font_size, cached.color) != (width, font.size, color) {
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

struct FindBar {
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
    Find { replace: bool },
    FindNext { back: bool },
    GoTo,
    CloseBars,
    ToggleWrap,
    ToggleComment,
    Indent { outdent: bool },
    Newline,
    Replace { all: bool },
    GoToLine,
}

impl EditorTab {
    pub fn new(id: u64, path: PathBuf) -> Self {
        Self {
            id,
            edit: path,
            wrap: false,
            syntax: None,
            doc: Doc::Unloaded,
            find: None,
            goto: None,
            select: None,
            scroll_to_cursor: false,
            focus_text: true,
            check_disk: false,
            pending_find_next: false,
        }
    }

    pub fn title(&self) -> String {
        let name = crate::fs::display_name(&self.edit);
        let dirty = if self.is_dirty() { " ●" } else { "" };
        format!("{} {name}{dirty}", icon::NOTE_PENCIL)
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
        update_highlighter(&ctx, buffer, syntax, &theme);

        let focused = shared.focused_tab == Some(self.id);
        let text_focused = ctx.memory(|m| m.has_focus(text_id(self.id)));
        let editable = buffer.read_only.is_none();
        let commands = if focused {
            shortcuts(&ctx, text_focused && editable)
        } else {
            Vec::new()
        };
        for command in commands {
            self.run(&ctx, command);
        }

        let Doc::Ready(buffer) = &mut self.doc else {
            return;
        };
        let mut commands = Vec::new();
        Panel::top(Id::new(("editor-toolbar", self.id))).show(ui, |ui| {
            toolbar(ui, buffer, &self.edit, &mut self.wrap, &mut commands)
        });
        let mut reload = false;
        if buffer.disk != Disk::InSync || buffer.error.is_some() || buffer.read_only.is_some() {
            Panel::top(Id::new(("editor-banner", self.id)))
                .show(ui, |ui| reload = banner(ui, buffer, &self.edit));
        }
        if let Some(find) = &mut self.find {
            Panel::top(Id::new(("editor-find", self.id))).show(ui, |ui| {
                find_bar(ui, find, editable, &mut commands);
            });
        }
        if let Some(goto) = &mut self.goto {
            Panel::top(Id::new(("editor-goto", self.id))).show(ui, |ui| {
                goto_bar(ui, goto, &mut commands);
            });
        }
        Panel::bottom(Id::new(("editor-status", self.id))).show(ui, |ui| {
            status_bar(ui, buffer, &mut self.syntax, self.id);
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
                text_area(
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
                    },
                );
            });

        if buffer.view.version != buffer.dirty_version {
            buffer.dirty = buffer.text != buffer.saved;
            buffer.dirty_version = buffer.view.version;
        }
        if let Some(find) = &mut self.find {
            let key = (
                buffer.view.version,
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
                find.matches =
                    ops::find(&buffer.text, &find.query, find.match_case, find.whole_word);
                find.current = None;
                find.found_for = Some(key);
                // Typing a search jumps to the first match from where it started.
                if !edited && !find.matches.is_empty() {
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
            Command::Find { replace } => {
                let selected = (selection.start != selection.end)
                    .then(|| {
                        let text = &buffer.text;
                        let start = ops::char_to_byte(text, selection.start);
                        let end = ops::char_to_byte(text, selection.end);
                        text[start..end].to_owned()
                    })
                    .filter(|s| !s.contains('\n'));
                let find = self.find.get_or_insert_with(|| FindBar {
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
        self.select = Some(find.matches[index].clone());
        self.scroll_to_cursor = true;
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
fn shortcuts(ctx: &Context, text_focused: bool) -> Vec<Command> {
    let cmd = Modifiers::COMMAND;
    ctx.input_mut(|i| {
        let mut commands = Vec::new();
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
        key(Modifiers::NONE, Key::Escape, Command::CloseBars);
        if text_focused {
            // Instead of the text field inserting a plain tab or line break.
            i.events.retain(|event| match event {
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

fn toolbar(
    ui: &mut Ui,
    buffer: &Buffer,
    path: &Path,
    wrap: &mut bool,
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
        ui.separator();
        if button(ui, true, icon::MAGNIFYING_GLASS, "Find (Ctrl+F)") {
            commands.push(Command::Find { replace: false });
        }
        if button(ui, editable, icon::SWAP, "Replace (Ctrl+H)") {
            commands.push(Command::Find { replace: true });
        }
        if button(ui, true, icon::LIST_NUMBERS, "Go to line (Ctrl+G)") {
            commands.push(Command::GoTo);
        }
        ui.separator();
        ui.toggle_value(wrap, RichText::new(icon::TEXT_ALIGN_LEFT).size(16.0))
            .on_hover_text("Wrap long lines (Alt+Z)");
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

fn status_bar(ui: &mut Ui, buffer: &Buffer, syntax: &mut Option<String>, id: u64) {
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
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if let Some((line, col, selected)) = buffer.view.status {
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
}

/// The text with line numbers beside it.
fn text_area(ui: &mut Ui, buffer: &mut Buffer, area: TextArea<'_>) {
    let TextArea {
        id,
        wrap,
        colors,
        matches,
        scroll_to_cursor,
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
        placed = Some(output.inner);
    });
    let Some((galley, origin, cursor)) = placed else {
        return;
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
            let dir = std::env::temp_dir()
                .join(format!("egui-files-editor-{name}-{}", std::process::id()));
            let _ = disk::remove_dir_all(&dir);
            disk::create_dir_all(&dir).unwrap();
            let path = dir.join("main.rs");
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
