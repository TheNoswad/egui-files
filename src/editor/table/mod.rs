//! Delimited text as a table, in an editor tab and in the preview pane. The
//! tab's text stays the document: the table is parsed from it, and each edit
//! replaces only the bytes it changes (see [`edit`]). Sorting and filtering
//! only change what's shown, until the sort is applied to the file.
//!
//! Rows are drawn only while on screen, and everything goes through one big
//! widget that works out what's under the pointer, so a table of a million
//! rows costs what a screenful does.

pub mod edit;
pub mod parse;

use std::{
    borrow::Cow,
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    ops::{Range, RangeInclusive},
    path::Path,
};

use egui::{
    Align2, Button, Context, CursorIcon, Event, EventFilter, FontId, Grid, Id, Key,
    KeyboardShortcut, Layout, Modifiers, Pos2, Rect, Response, RichText, ScrollArea, Stroke,
    StrokeKind, TextEdit, TextStyle, Ui, UiBuilder, ViewportCommand, pos2, text::CCursor,
    text::CCursorRange, text_edit::TextEditState, vec2,
};
use egui_phosphor::regular as icon;
use serde::{Deserialize, Serialize};

use self::{
    edit::{CellValue, Splice},
    parse::Parsed,
};
use crate::touch;

/// Whether a file is shown as a table.
pub fn is_table_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("csv") || e.eq_ignore_ascii_case("tsv"))
}

/// What's kept of a table with its tab.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Options {
    /// Show the plain text instead.
    pub text: bool,
    /// Chosen by hand instead of detected.
    pub delimiter: Option<char>,
    /// Chosen by hand instead of detected.
    pub header: Option<bool>,
}

fn detected_delimiter(text: &str, path: &Path) -> u8 {
    if path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("tsv"))
    {
        b'\t'
    } else {
        parse::detect_delimiter(text)
    }
}

/// Hash of a text, to tell whether it changed while the table wasn't shown.
fn text_hash(text: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Whole {
    Cells,
    Rows,
    Columns,
}

/// In rows as shown, which sorting and filtering make different from the file's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Selection {
    anchor: (usize, usize),
    cursor: (usize, usize),
    whole: Whole,
}

impl Selection {
    fn at(cell: (usize, usize)) -> Self {
        Self {
            anchor: cell,
            cursor: cell,
            whole: Whole::Cells,
        }
    }

    fn rows(&self, rows: usize) -> RangeInclusive<usize> {
        if self.whole == Whole::Columns {
            0..=rows.saturating_sub(1)
        } else {
            self.anchor.0.min(self.cursor.0)..=self.anchor.0.max(self.cursor.0)
        }
    }

    fn columns(&self, columns: usize) -> RangeInclusive<usize> {
        if self.whole == Whole::Rows {
            0..=columns.saturating_sub(1)
        } else {
            self.anchor.1.min(self.cursor.1)..=self.anchor.1.max(self.cursor.1)
        }
    }

    fn contains(&self, cell: (usize, usize), rows: usize, columns: usize) -> bool {
        self.rows(rows).contains(&cell.0) && self.columns(columns).contains(&cell.1)
    }

    fn is_single(&self) -> bool {
        self.whole == Whole::Cells && self.anchor == self.cursor
    }
}

struct CellEdit {
    /// In the file.
    row: usize,
    col: usize,
    /// In the rows shown; `None` for the header.
    shown_row: Option<usize>,
    text: String,
    original: String,
    /// Just started: it gets the focus this frame.
    fresh: bool,
}

/// Which way to go after finishing with a cell.
#[derive(Clone, Copy)]
enum Move {
    Stay,
    Down,
    Up,
    Next,
    Previous,
}

pub struct Find {
    pub query: String,
    pub match_case: bool,
    /// Show only the rows with a match.
    pub filter: bool,
    focus: bool,
    /// In the rows shown, in order.
    matches: Vec<(u32, u32)>,
    current: Option<usize>,
    /// What's shown and found was worked out for: query, case and filter.
    rows_for: Option<(String, bool, bool)>,
    /// What `matches` was worked out for, with the text's revision.
    matches_for: Option<(String, bool, bool, u64)>,
}

impl Find {
    fn normalized(&self, value: &str) -> String {
        if self.match_case {
            value.to_owned()
        } else {
            value.to_lowercase()
        }
    }

    fn query_normalized(&self) -> String {
        self.normalized(&self.query)
    }
}

/// Undoes a change, and puts the selection back.
struct Step {
    splices: Vec<Splice>,
    selection: Selection,
}

/// The steps of history kept, and the bytes of text in them.
const MAX_HISTORY_STEPS: usize = 500;
const MAX_HISTORY_BYTES: usize = 64 * 1024 * 1024;

fn step_bytes(step: &Step) -> usize {
    step.splices.iter().map(|s| s.insert.len()).sum()
}

enum Command {
    Undo,
    Redo,
    InsertRow { before: bool },
    DeleteRows,
    InsertColumn { before: bool },
    DeleteColumns,
    Clear,
    Paste(String),
    Sort(Option<Sort>),
    ApplySort,
    EditCursor,
    EditHeader(usize),
    SelectAll,
    SelectColumn(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Sort {
    col: usize,
    descending: bool,
}

/// What's under the pointer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Zone {
    Corner,
    Header(usize),
    /// The edge between a column and the next, which resizes it.
    Edge(usize),
    Gutter(usize),
    Cell(usize, usize),
}

enum Drag {
    None,
    Select,
    Resize {
        col: usize,
        from_x: f32,
        from_width: f32,
    },
}

pub struct Table {
    parsed: Parsed,
    header: bool,
    detected_header: bool,
    detected_delimiter: u8,
    sort: Option<Sort>,
    /// Rows of the file shown, in order. Not the header.
    shown: Vec<u32>,
    pub find: Option<Find>,
    numeric: Vec<bool>,
    widths: Vec<f32>,
    selection: Selection,
    editing: Option<CellEdit>,
    undo: Vec<Step>,
    redo: Vec<Step>,
    pending: Vec<Command>,
    /// Bumped whenever the text changes.
    revision: u64,
    changed: bool,
    /// Edited since the plain text was last shown.
    edited: bool,
    /// The text when the plain text was switched to.
    left_at: Option<u64>,
    scroll_to: Option<(usize, usize)>,
    focus: bool,
    drag: Drag,
    menu: Option<Zone>,
    /// Where a long press began, and whether it has moved off that cell.
    hold: Option<((usize, usize), bool)>,
    #[cfg(test)]
    drawn: Option<Drawn>,
}

impl Table {
    pub fn new(text: &str, path: &Path, options: &Options) -> Self {
        let mut table = Self {
            parsed: Parsed::default(),
            header: false,
            detected_header: false,
            detected_delimiter: b',',
            sort: None,
            shown: Vec::new(),
            find: None,
            numeric: Vec::new(),
            widths: Vec::new(),
            selection: Selection::at((0, 0)),
            editing: None,
            undo: Vec::new(),
            redo: Vec::new(),
            pending: Vec::new(),
            revision: 0,
            changed: false,
            edited: false,
            left_at: None,
            scroll_to: None,
            focus: true,
            drag: Drag::None,
            menu: None,
            hold: None,
            #[cfg(test)]
            drawn: None,
        };
        table.configure(text, path, options);
        table
    }

    /// Applies the tab's options, as when they're changed: reads the text anew.
    pub fn configure(&mut self, text: &str, path: &Path, options: &Options) {
        self.detected_delimiter = detected_delimiter(text, path);
        let delimiter = options
            .delimiter
            .filter(char::is_ascii)
            .map_or(self.detected_delimiter, |c| c as u8);
        self.parsed = parse::parse(text, delimiter);
        self.detected_header = parse::detect_header(text, &self.parsed);
        self.header = options.header.unwrap_or(self.detected_header);
        self.editing = None;
        self.sort = self.sort.filter(|s| s.col < self.parsed.columns);
        self.widths.clear();
        self.after_parse(text, true);
    }

    /// The text was replaced from outside the table.
    pub fn text_replaced(&mut self, text: &str, path: &Path, options: &Options) {
        self.undo.clear();
        self.redo.clear();
        self.edited = false;
        self.configure(text, path, options);
    }

    /// The plain text is being shown instead. Returns whether its own undo
    /// history no longer applies.
    pub fn leaving(&mut self, text: &mut String) -> bool {
        self.commit_edit(text, Move::Stay);
        self.left_at = Some(text_hash(text));
        std::mem::take(&mut self.edited)
    }

    /// The table is being shown again, maybe of a text changed meanwhile.
    pub fn returning(&mut self, text: &str, path: &Path, options: &Options) {
        if self.left_at.take() != Some(text_hash(text)) {
            self.text_replaced(text, path, options);
        }
        self.focus = true;
    }

    fn first_body_row(&self) -> usize {
        usize::from(self.header && !self.parsed.rows.is_empty())
    }

    fn after_parse(&mut self, text: &str, rebuild: bool) {
        self.revision += 1;
        let first = self.first_body_row();
        self.numeric = (0..self.parsed.columns)
            .map(|col| parse::is_numeric_column(text, &self.parsed, first, col))
            .collect();
        if rebuild {
            self.rebuild_shown(text);
        }
        self.clamp_selection();
    }

    fn rebuild_shown(&mut self, text: &str) {
        let first = self.first_body_row();
        self.shown = match self.sort {
            Some(sort) => edit::sorted_rows(text, &self.parsed, first, sort.col, sort.descending),
            None => (first as u32..self.parsed.rows.len() as u32).collect(),
        };
        if let Some(find) = &self.find
            && find.filter
            && !find.query.is_empty()
        {
            let query = find.query_normalized();
            let parsed = &self.parsed;
            self.shown.retain(|&row| {
                (0..parsed.cell_count(row as usize)).any(|col| {
                    find.normalized(&parsed.value(text, row as usize, col))
                        .contains(&query)
                })
            });
        }
    }

    fn clamp_selection(&mut self) {
        let last = (
            self.shown.len().saturating_sub(1),
            self.parsed.columns.saturating_sub(1),
        );
        let clamp = |cell: &mut (usize, usize)| {
            cell.0 = cell.0.min(last.0);
            cell.1 = cell.1.min(last.1);
        };
        clamp(&mut self.selection.anchor);
        clamp(&mut self.selection.cursor);
    }

    /// Whether the cell being edited differs from what's in the text.
    pub fn has_uncommitted_edit(&self) -> bool {
        self.editing.as_ref().is_some_and(|e| e.text != e.original)
    }

    /// Writes the cell being edited to the text, as saving does first.
    pub fn commit_pending(&mut self, text: &mut String) {
        self.commit_edit(text, Move::Stay);
    }

    fn commit_edit(&mut self, text: &mut String, then: Move) {
        let Some(edit) = self.editing.take() else {
            return;
        };
        let splices = edit::set_cells(
            text,
            &self.parsed,
            &[CellValue {
                row: edit.row,
                col: edit.col,
                value: edit.text,
            }],
        );
        self.record(text, splices, false);
        self.move_cursor(then);
        self.focus = true;
    }

    fn move_cursor(&mut self, direction: Move) {
        let (row, col) = self.selection.cursor;
        let rows = self.shown.len();
        let columns = self.parsed.columns;
        if rows == 0 || columns == 0 {
            return;
        }
        let target = match direction {
            Move::Stay => return,
            Move::Down => ((row + 1).min(rows - 1), col),
            Move::Up => (row.saturating_sub(1), col),
            Move::Next if col + 1 < columns => (row, col + 1),
            Move::Next if row + 1 < rows => (row + 1, 0),
            Move::Next => (row, col),
            Move::Previous if col > 0 => (row, col - 1),
            Move::Previous if row > 0 => (row - 1, columns - 1),
            Move::Previous => (row, col),
        };
        self.selection = Selection::at(target);
        self.scroll_to = Some(target);
    }

    /// Makes a change to the text as one undo step.
    fn record(&mut self, text: &mut String, splices: Vec<Splice>, rebuild: bool) {
        if splices.is_empty() {
            return;
        }
        let inverse = edit::apply(text, &splices);
        self.undo.push(Step {
            splices: inverse,
            selection: self.selection,
        });
        self.redo.clear();
        let mut bytes: usize = self.undo.iter().map(step_bytes).sum();
        while self.undo.len() > MAX_HISTORY_STEPS
            || (bytes > MAX_HISTORY_BYTES && self.undo.len() > 1)
        {
            bytes -= step_bytes(&self.undo.remove(0));
        }
        self.reparse(text, rebuild);
    }

    fn reparse(&mut self, text: &str, rebuild: bool) {
        let rows = self.parsed.rows.len();
        self.parsed = parse::parse(text, self.parsed.delimiter);
        // Only a change of rows makes the rows shown wrong. A row whose value
        // changed stays where it is until sorted again, or it would jump away
        // from under the cursor.
        let rebuild = rebuild || self.parsed.rows.len() != rows;
        if self.parsed.columns != self.widths.len() {
            self.widths.clear();
        }
        self.after_parse(text, rebuild);
        self.changed = true;
        self.edited = true;
    }

    fn step_history(&mut self, text: &mut String, redo: bool) {
        let (from, to) = if redo {
            (&mut self.redo, &mut self.undo)
        } else {
            (&mut self.undo, &mut self.redo)
        };
        let Some(step) = from.pop() else {
            return;
        };
        let inverse = edit::apply(text, &step.splices);
        to.push(Step {
            splices: inverse,
            selection: self.selection,
        });
        self.selection = step.selection;
        self.reparse(text, true);
        self.scroll_to = Some(self.selection.cursor);
    }

    /// The rows of the file selected, in file order.
    fn selected_file_rows(&self) -> Vec<usize> {
        if self.shown.is_empty() {
            return Vec::new();
        }
        let range = self.selection.rows(self.shown.len());
        let mut rows: Vec<usize> = self.shown[*range.start()..=*range.end()]
            .iter()
            .map(|&row| row as usize)
            .collect();
        rows.sort_unstable();
        rows
    }

    fn selected_columns(&self) -> RangeInclusive<usize> {
        self.selection.columns(self.parsed.columns)
    }

    /// Selects a row of the file, if it's shown.
    fn select_file_row(&mut self, row: usize, col: usize) {
        let shown = self.shown.iter().position(|&r| r as usize == row);
        if let Some(shown) = shown {
            self.selection = Selection::at((shown, col));
            self.scroll_to = Some((shown, col));
        }
    }

    /// What's selected, as the clipboard holds it.
    fn selected_values(&self, text: &str) -> Vec<Vec<String>> {
        if self.shown.is_empty() {
            return Vec::new();
        }
        let columns = self.selected_columns();
        let rows = self.selection.rows(self.shown.len());
        self.shown[*rows.start()..=*rows.end()]
            .iter()
            .map(|&row| {
                columns
                    .clone()
                    .map(|col| self.parsed.value(text, row as usize, col).into_owned())
                    .collect()
            })
            .collect()
    }

    fn clear_selected(&mut self, text: &mut String) {
        let columns = self.selected_columns();
        let values: Vec<CellValue> = self
            .selected_file_rows()
            .into_iter()
            .flat_map(|row| {
                columns.clone().map(move |col| CellValue {
                    row,
                    col,
                    value: String::new(),
                })
            })
            .collect();
        let splices = edit::set_cells(text, &self.parsed, &values);
        self.record(text, splices, false);
    }

    fn paste(&mut self, text: &mut String, pasted: &str) {
        let pasted = edit::from_clipboard(pasted);
        if pasted.is_empty() {
            return;
        }
        let rows_selected = self.selection.rows(self.shown.len());
        let columns_selected = self.selected_columns();
        let top = *rows_selected.start();
        let left = *columns_selected.start();
        let mut values = Vec::new();
        let mut new_rows = Vec::new();
        // One value fills what's selected.
        if pasted.len() == 1 && pasted[0].len() == 1 && !self.selection.is_single() {
            for row in self.selected_file_rows() {
                for col in columns_selected.clone() {
                    values.push(CellValue {
                        row,
                        col,
                        value: pasted[0][0].clone(),
                    });
                }
            }
        } else {
            for (index, cells) in pasted.iter().enumerate() {
                match self.shown.get(top + index) {
                    Some(&row) => {
                        for (offset, value) in cells.iter().enumerate() {
                            values.push(CellValue {
                                row: row as usize,
                                col: left + offset,
                                value: value.clone(),
                            });
                        }
                    }
                    None => {
                        let mut row = vec![String::new(); left];
                        row.extend(cells.iter().cloned());
                        new_rows.push(row);
                    }
                }
            }
        }
        let mut splices = edit::set_cells(text, &self.parsed, &values);
        splices.extend(edit::append_rows(&self.parsed, text.len(), &new_rows));
        let width = pasted.iter().map(Vec::len).max().unwrap_or(1).max(1);
        let selection = (!self.selection.is_single() && pasted.len() == 1 && width == 1)
            .then_some(self.selection);
        self.record(text, splices, false);
        self.selection = selection.unwrap_or_else(|| {
            let last = (top + pasted.len() - 1, left + width - 1);
            Selection {
                anchor: (top, left),
                cursor: last,
                whole: Whole::Cells,
            }
        });
        self.clamp_selection();
    }

    fn run(&mut self, text: &mut String, command: Command, editable: bool) {
        // Everything but looking needs to be allowed.
        if !editable
            && !matches!(
                command,
                Command::Sort(_) | Command::SelectAll | Command::SelectColumn(_)
            )
        {
            return;
        }
        match command {
            Command::Undo => self.step_history(text, false),
            Command::Redo => self.step_history(text, true),
            Command::InsertRow { before } => {
                let selected = self.selected_file_rows();
                let at = match (selected.first(), selected.last()) {
                    (Some(&first), _) if before => first,
                    (_, Some(&last)) => last + 1,
                    _ => self.parsed.rows.len(),
                };
                let splices = edit::insert_rows(&self.parsed, text.len(), at, 1);
                // The new row is empty: a filter would hide it.
                if let Some(find) = &mut self.find {
                    find.filter = false;
                }
                let col = self.selection.cursor.1;
                self.record(text, splices, true);
                self.select_file_row(at, col);
            }
            Command::DeleteRows => {
                let rows = self.selected_file_rows();
                let splices = edit::delete_rows(&self.parsed, &rows);
                self.record(text, splices, true);
                self.selection.whole = Whole::Cells;
                self.selection.anchor = self.selection.cursor;
                self.clamp_selection();
            }
            Command::InsertColumn { before } => {
                let columns = self.selected_columns();
                let at = if before {
                    *columns.start()
                } else {
                    (*columns.end() + 1).min(self.parsed.columns)
                };
                let splices = edit::insert_column(&self.parsed, at);
                self.record(text, splices, true);
                let row = self.selection.cursor.0;
                self.selection = Selection::at((row, at));
                self.clamp_selection();
                self.scroll_to = Some(self.selection.cursor);
            }
            Command::DeleteColumns => {
                let columns = self.selected_columns();
                let removed = *columns.start()..*columns.end() + 1;
                let splices = edit::delete_columns(&self.parsed, removed.clone());
                self.sort = self.sort.and_then(|mut sort| {
                    if removed.contains(&sort.col) {
                        return None;
                    }
                    if sort.col >= removed.end {
                        sort.col -= removed.len();
                    }
                    Some(sort)
                });
                self.record(text, splices, true);
                self.selection.whole = Whole::Cells;
                self.selection.anchor = self.selection.cursor;
                self.clamp_selection();
            }
            Command::Clear => self.clear_selected(text),
            Command::Paste(pasted) => self.paste(text, &pasted),
            Command::Sort(sort) => {
                self.sort = sort;
                self.rebuild_shown(text);
                self.clamp_selection();
            }
            Command::ApplySort => {
                if let Some(sort) = self.sort {
                    let first = self.first_body_row();
                    let order =
                        edit::sorted_rows(text, &self.parsed, first, sort.col, sort.descending);
                    let splices = edit::reorder_rows(text, &self.parsed, first, &order);
                    self.sort = None;
                    self.record(text, splices, true);
                    self.rebuild_shown(text);
                }
            }
            Command::EditCursor => self.begin_edit_cursor(text, None),
            Command::EditHeader(col) => {
                if self.header {
                    let value = self.parsed.value(text, 0, col).into_owned();
                    self.editing = Some(CellEdit {
                        row: 0,
                        col,
                        shown_row: None,
                        text: value.clone(),
                        original: value,
                        fresh: true,
                    });
                }
            }
            Command::SelectAll => {
                self.selection = Selection {
                    anchor: (0, 0),
                    cursor: (
                        self.shown.len().saturating_sub(1),
                        self.parsed.columns.saturating_sub(1),
                    ),
                    whole: Whole::Cells,
                };
            }
            Command::SelectColumn(col) => {
                self.selection = Selection {
                    anchor: (0, col),
                    cursor: (self.shown.len().saturating_sub(1), col),
                    whole: Whole::Columns,
                };
            }
        }
    }

    fn begin_edit_cursor(&mut self, text: &str, typed: Option<String>) {
        let (shown_row, col) = self.selection.cursor;
        let Some(&row) = self.shown.get(shown_row) else {
            return;
        };
        if col >= self.parsed.columns {
            return;
        }
        let row = row as usize;
        let original = self.parsed.value(text, row, col).into_owned();
        self.selection = Selection::at((shown_row, col));
        self.scroll_to = Some((shown_row, col));
        self.editing = Some(CellEdit {
            row,
            col,
            shown_row: Some(shown_row),
            text: typed.unwrap_or_else(|| original.clone()),
            original,
            fresh: true,
        });
    }

    pub fn open_find(&mut self) {
        let find = self.find.get_or_insert_with(|| Find {
            query: String::new(),
            match_case: false,
            filter: false,
            focus: true,
            matches: Vec::new(),
            current: None,
            rows_for: None,
            matches_for: None,
        });
        find.focus = true;
    }

    pub fn close_find(&mut self, text: &str) {
        let had_filter = self.find.take().is_some_and(|find| find.filter);
        if had_filter {
            self.rebuild_shown(text);
            self.clamp_selection();
        }
        self.focus = true;
    }

    pub fn find_next(&mut self, back: bool) {
        let cursor = self.selection.cursor;
        let Some(find) = &mut self.find else {
            return;
        };
        let count = find.matches.len();
        if count == 0 {
            return;
        }
        let at = |m: &(u32, u32)| (m.0 as usize, m.1 as usize);
        let index = match find.current.filter(|&i| i < count) {
            Some(current) if back => (current + count - 1) % count,
            Some(current) => (current + 1) % count,
            None if back => find
                .matches
                .iter()
                .rposition(|m| at(m) < cursor)
                .unwrap_or(count - 1),
            None => find
                .matches
                .iter()
                .position(|m| at(m) >= cursor)
                .unwrap_or(0),
        };
        find.current = Some(index);
        let target = at(&find.matches[index]);
        self.selection = Selection::at(target);
        self.scroll_to = Some(target);
    }

    /// Works out what the find bar's search shows and finds, once it's changed.
    fn refresh_find(&mut self, text: &str) {
        let Some(find) = &self.find else {
            return;
        };
        let rows_key = (find.query.clone(), find.match_case, find.filter);
        let query_changed = find.rows_for.as_ref() != Some(&rows_key);
        if query_changed {
            // Also what the row filter sees: a filter turned off shows all again.
            self.rebuild_shown(text);
            self.clamp_selection();
        }
        let revision = self.revision;
        let Some(find) = &mut self.find else {
            return;
        };
        find.rows_for = Some(rows_key.clone());
        let matches_key = (rows_key.0, rows_key.1, rows_key.2, revision);
        let matches_changed = find.matches_for.as_ref() != Some(&matches_key);
        if !matches_changed && !query_changed {
            return;
        }
        find.matches_for = Some(matches_key);
        find.matches.clear();
        find.current = None;
        if !find.query.is_empty() {
            let query = find.query_normalized();
            'rows: for (shown, &row) in self.shown.iter().enumerate() {
                for col in 0..self.parsed.cell_count(row as usize) {
                    if find
                        .normalized(&self.parsed.value(text, row as usize, col))
                        .contains(&query)
                    {
                        find.matches.push((shown as u32, col as u32));
                        if find.matches.len() >= super::ops::MAX_MATCHES {
                            break 'rows;
                        }
                    }
                }
            }
        }
        // Typing a search jumps to the first match from the cursor.
        if query_changed && !find.matches.is_empty() {
            self.find_next(false);
        }
    }

    fn measure_widths(&mut self, ui: &Ui, text: &str, font: &FontId) {
        if self.widths.len() == self.parsed.columns {
            return;
        }
        const SAMPLE: usize = 50;
        const PADDING: f32 = 2.0 * CELL_PADDING + 4.0;
        let color = ui.visuals().text_color();
        let measure = |value: &str| {
            ui.painter()
                .layout_no_wrap(shown_value(value).into_owned(), font.clone(), color)
                .size()
                .x
        };
        self.widths = (0..self.parsed.columns)
            .map(|col| {
                let mut width = if self.header {
                    measure(&self.parsed.value(text, 0, col)) + SORT_ARROW_WIDTH
                } else {
                    measure(&column_letters(col)) + SORT_ARROW_WIDTH
                };
                for &row in self.shown.iter().take(SAMPLE) {
                    width = width.max(measure(&self.parsed.value(text, row as usize, col)));
                }
                (width + PADDING).clamp(MIN_COLUMN_WIDTH, MAX_AUTO_COLUMN_WIDTH)
            })
            .collect();
    }

    /// Where each column starts, and where the last one ends.
    fn column_starts(&self) -> Vec<f32> {
        let mut starts = Vec::with_capacity(self.widths.len() + 1);
        let mut x = 0.0;
        starts.push(x);
        for width in &self.widths {
            x += *width;
            starts.push(x);
        }
        starts
    }

    /// The table. Returns whether the text changed.
    pub fn body(
        &mut self,
        ui: &mut Ui,
        id: Id,
        text: &mut String,
        editable: bool,
        focused: bool,
    ) -> bool {
        let ctx = ui.ctx().clone();
        for command in std::mem::take(&mut self.pending) {
            self.commit_edit(text, Move::Stay);
            self.run(text, command, editable);
        }
        let font = TextStyle::Body.resolve(ui.style());
        let touch = touch::is_touch(&ctx);

        let row_height = if touch {
            touch::TOUCH_SIZE
        } else {
            (font.size * 1.7).round().max(ui.spacing().interact_size.y)
        };

        let page = (ui.available_height() / row_height).max(1.0) as usize;
        // Not those meant for a menu that's open.
        let keys_are_ours = focused
            && !egui::Popup::is_any_open(&ctx)
            && ctx.memory(|m| m.focused().is_none_or(|focus| focus == id));
        if keys_are_ours && self.editing.is_none() {
            self.keyboard(&ctx, text, editable, page);
        }
        self.refresh_find(text);
        self.measure_widths(ui, text, &font);

        if self.shown.is_empty() && self.parsed.rows.len() <= self.first_body_row() {
            ui.vertical_centered(|ui| {
                ui.add_space(32.0);
                ui.weak(if self.parsed.rows.is_empty() {
                    "This file is empty. Add a row to start."
                } else {
                    "There are no rows under the header."
                });
            });
        }

        let digits = (self.parsed.rows.len().max(1)).ilog10() as usize + 1;
        let digit_width = ui.fonts_mut(|f| f.glyph_width(&font, '0'));
        let gutter = digits.max(2) as f32 * digit_width + 2.0 * CELL_PADDING;
        let prefix = self.column_starts();
        let columns_width = prefix.last().copied().unwrap_or(0.0);

        ScrollArea::both()
            .id_salt(("table-scroll", id))
            .auto_shrink(false)
            .scroll_source(touch::scroll_source(&ctx))
            .show_viewport(ui, |ui, viewport| {
                let origin = ui.max_rect().min;
                let header_height = row_height;
                let content = vec2(
                    gutter + columns_width,
                    header_height + self.shown.len() as f32 * row_height,
                );
                let size = content.max(viewport.size());
                let rect = Rect::from_min_size(origin, size);
                ui.advance_cursor_after_rect(rect);
                let geometry = Geometry {
                    origin,
                    viewport,
                    screen: viewport.translate(origin.to_vec2()),
                    gutter,
                    header_height,
                    row_height,
                    prefix: &prefix,
                    rows: self.shown.len(),
                };
                #[cfg(test)]
                {
                    self.drawn = Some(Drawn {
                        body_left: origin.x + gutter,
                        top: origin.y,
                        row_height,
                        prefix: prefix.clone(),
                    });
                }
                let sense = touch::item_sense(&ctx);
                let response = ui.interact(rect, id, sense);
                // An edit it finishes changes a cell's value, but not how many
                // rows and columns there are, so `geometry` stays right.
                self.pointer(&ctx, &response, &geometry, text, editable, touch);
                if response.has_focus() {
                    ui.memory_mut(|m| {
                        m.set_focus_lock_filter(
                            id,
                            EventFilter {
                                tab: true,
                                horizontal_arrows: true,
                                vertical_arrows: true,
                                escape: true,
                            },
                        );
                    });
                }
                if std::mem::take(&mut self.focus) && !touch {
                    response.request_focus();
                }
                if let Some(target) = self.scroll_to.take() {
                    // With room for the header and row numbers drawn over it.
                    let cell = geometry.cell(target.0, target.1);
                    let top_left = cell.min - vec2(gutter, header_height);
                    ui.scroll_to_rect(Rect::from_min_max(top_left, cell.max), None);
                }
                self.paint(ui, &geometry, text, &font);
                self.cell_editor(ui, &geometry, text, id, &font, touch);
                response.context_menu(|ui| self.menu_ui(ui, editable));
            });
        std::mem::take(&mut self.changed)
    }

    fn keyboard(&mut self, ctx: &Context, text: &mut String, editable: bool, page: usize) {
        let rows = self.shown.len();
        let columns = self.parsed.columns;
        let events = ctx.input(|i| i.events.clone());
        let mut swallow_text = false;
        for event in events {
            match event {
                Event::Copy => self.copy(ctx, text),
                Event::Cut => {
                    self.copy(ctx, text);
                    self.run(text, Command::Clear, editable);
                }
                Event::Paste(pasted) => self.run(text, Command::Paste(pasted), editable),
                Event::Text(typed) if !swallow_text => {
                    if editable && rows > 0 && columns > 0 {
                        self.begin_edit_cursor(text, Some(typed));
                        return;
                    }
                }
                Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } => {
                    let command = modifiers.command;
                    let shift = modifiers.shift;
                    match key {
                        Key::Z if command && shift => self.run(text, Command::Redo, editable),
                        Key::Z if command => self.run(text, Command::Undo, editable),
                        Key::Y if command => self.run(text, Command::Redo, editable),
                        Key::A if command => self.run(text, Command::SelectAll, editable),
                        Key::Space if command => {
                            let col = self.selection.cursor.1;
                            self.run(text, Command::SelectColumn(col), editable);
                            swallow_text = true;
                        }
                        Key::Space if shift => {
                            let row = self.selection.cursor.0;
                            self.selection = Selection {
                                anchor: (row, 0),
                                cursor: (row, columns.saturating_sub(1)),
                                whole: Whole::Rows,
                            };
                            swallow_text = true;
                        }
                        Key::Delete | Key::Backspace if modifiers.is_none() => {
                            self.run(text, Command::Clear, editable);
                        }
                        Key::F2 if modifiers.is_none() => {
                            self.run(text, Command::EditCursor, editable);
                            return;
                        }
                        Key::Escape => {
                            if self.find.is_some() {
                                self.close_find(text);
                            } else {
                                self.selection = Selection::at(self.selection.cursor);
                            }
                        }
                        Key::Enter => {
                            self.move_cursor(if shift { Move::Up } else { Move::Down });
                        }
                        Key::Tab => {
                            self.move_cursor(if shift { Move::Previous } else { Move::Next });
                        }
                        Key::ArrowUp
                        | Key::ArrowDown
                        | Key::ArrowLeft
                        | Key::ArrowRight
                        | Key::Home
                        | Key::End
                        | Key::PageUp
                        | Key::PageDown
                            if rows > 0 && columns > 0 =>
                        {
                            let target =
                                navigate(key, command, self.selection.cursor, rows, columns, page);
                            if shift {
                                self.selection.cursor = target;
                            } else {
                                self.selection = Selection::at(target);
                            }
                            self.scroll_to = Some(target);
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    }

    fn copy(&self, ctx: &Context, text: &str) {
        let values = self.selected_values(text);
        if !values.is_empty() {
            ctx.copy_text(edit::to_clipboard(&values));
        }
    }

    fn pointer(
        &mut self,
        ctx: &Context,
        response: &Response,
        geometry: &Geometry,
        text: &mut String,
        editable: bool,
        touch: bool,
    ) {
        let (shift, down, pressed) = ctx.input(|i| {
            (
                i.modifiers.shift,
                i.pointer.primary_down(),
                i.pointer.primary_pressed(),
            )
        });
        let zone = |pos: Pos2| geometry.zone(pos);

        if let Some(pos) = response.hover_pos()
            && matches!(zone(pos), Some(Zone::Edge(_)))
            && !touch
        {
            ctx.set_cursor_icon(CursorIcon::ResizeHorizontal);
        }

        if !touch && pressed && response.hovered() {
            self.commit_edit(text, Move::Stay);
            let hit = ctx.input(|i| i.pointer.press_origin()).and_then(zone);
            match hit {
                Some(Zone::Cell(row, col)) => {
                    if shift {
                        self.selection.cursor = (row, col);
                        self.selection.whole = Whole::Cells;
                    } else {
                        self.selection = Selection::at((row, col));
                    }
                    self.drag = Drag::Select;
                }
                Some(Zone::Gutter(row)) => {
                    let last = self.parsed.columns.saturating_sub(1);
                    let anchor = if shift { self.selection.anchor.0 } else { row };
                    self.selection = Selection {
                        anchor: (anchor, 0),
                        cursor: (row, last),
                        whole: Whole::Rows,
                    };
                    self.drag = Drag::Select;
                }
                Some(Zone::Corner) => self.run(text, Command::SelectAll, editable),
                Some(Zone::Edge(col)) => {
                    if let Some(pos) = ctx.input(|i| i.pointer.press_origin())
                        && let Some(&from_width) = self.widths.get(col)
                    {
                        self.drag = Drag::Resize {
                            col,
                            from_x: pos.x,
                            from_width,
                        };
                    }
                }
                Some(Zone::Header(_)) | None => {}
            }
            self.focus = true;
        }

        if !down {
            self.drag = Drag::None;
        } else if let Some(pos) = response.interact_pointer_pos() {
            match self.drag {
                Drag::None => {}
                Drag::Select => {
                    if !self.shown.is_empty() {
                        let row = geometry.row_clamped(pos.y);
                        if self.selection.whole == Whole::Rows {
                            self.selection.cursor.0 = row;
                            self.scroll_to = Some((row, geometry.column_clamped(pos.x)));
                        } else if self.parsed.columns > 0 {
                            let col = geometry.column_clamped(pos.x);
                            self.selection.cursor = (row, col);
                            if response.dragged() {
                                self.scroll_to = Some((row, col));
                            }
                        }
                    }
                }
                Drag::Resize {
                    col,
                    from_x,
                    from_width,
                } => {
                    if let Some(width) = self.widths.get_mut(col) {
                        *width = (from_width + pos.x - from_x).max(MIN_COLUMN_WIDTH);
                    }
                }
            }
        }

        if touch {
            self.touch_gestures(ctx, response, geometry, text, editable);
        }

        if response.clicked()
            && let Some(pos) = response.interact_pointer_pos()
            && let Some(Zone::Header(col)) = zone(pos)
        {
            let sort = match self.sort {
                Some(sort) if sort.col == col && !sort.descending => Some(Sort {
                    col,
                    descending: true,
                }),
                Some(sort) if sort.col == col => None,
                _ => Some(Sort {
                    col,
                    descending: false,
                }),
            };
            self.run(text, Command::Sort(sort), editable);
        }

        if !touch
            && editable
            && response.double_clicked()
            && let Some(pos) = response.interact_pointer_pos()
            && let Some(Zone::Cell(row, col)) = zone(pos)
        {
            self.selection = Selection::at((row, col));
            self.begin_edit_cursor(text, None);
        }

        if response.secondary_clicked()
            && let Some(pos) = response.interact_pointer_pos()
        {
            self.commit_edit(text, Move::Stay);
            let rows = self.shown.len();
            let columns = self.parsed.columns;
            let hit = zone(pos);
            self.menu = hit;
            match hit {
                Some(Zone::Cell(row, col)) => {
                    if !self.selection.contains((row, col), rows, columns) {
                        self.selection = Selection::at((row, col));
                    }
                }
                Some(Zone::Gutter(row)) => {
                    if !self.selection.rows(rows).contains(&row)
                        || self.selection.whole != Whole::Rows
                    {
                        self.selection = Selection {
                            anchor: (row, 0),
                            cursor: (row, columns.saturating_sub(1)),
                            whole: Whole::Rows,
                        };
                    }
                }
                Some(Zone::Header(col) | Zone::Edge(col)) => {
                    if !self.selection.columns(columns).contains(&col)
                        || self.selection.whole != Whole::Columns
                    {
                        self.run(text, Command::SelectColumn(col), editable);
                    }
                }
                Some(Zone::Corner) | None => {}
            }
        }
    }

    /// Tap to select, tap the selected cell to edit, long press and drag to
    /// select more. A long press let go becomes a right click (see `touch`).
    fn touch_gestures(
        &mut self,
        ctx: &Context,
        response: &Response,
        geometry: &Geometry,
        text: &mut String,
        editable: bool,
    ) {
        if touch::holding(ctx) {
            let pos = ctx.input(|i| i.pointer.latest_pos());
            if let Some(Zone::Cell(row, col)) = pos.and_then(|pos| geometry.zone(pos)) {
                if touch::long_press_started(ctx) {
                    let (rows, columns) = (self.shown.len(), self.parsed.columns);
                    if !self.selection.contains((row, col), rows, columns) {
                        self.selection = Selection::at((row, col));
                    }
                    self.hold = Some(((row, col), false));
                } else if let Some((start, moved)) = &mut self.hold
                    && (*moved || *start != (row, col))
                {
                    if !*moved {
                        self.selection = Selection::at(*start);
                    }
                    *moved = true;
                    self.selection.cursor = (row, col);
                }
            }
        } else {
            self.hold = None;
        }
        if response.clicked()
            && let Some(pos) = response.interact_pointer_pos()
        {
            self.commit_edit(text, Move::Stay);
            match geometry.zone(pos) {
                Some(Zone::Cell(row, col)) => {
                    if editable && self.selection.is_single() && self.selection.cursor == (row, col)
                    {
                        self.begin_edit_cursor(text, None);
                    } else {
                        self.selection = Selection::at((row, col));
                    }
                }
                Some(Zone::Gutter(row)) => {
                    self.selection = Selection {
                        anchor: (row, 0),
                        cursor: (row, self.parsed.columns.saturating_sub(1)),
                        whole: Whole::Rows,
                    };
                }
                Some(Zone::Corner) => self.run(text, Command::SelectAll, editable),
                _ => {}
            }
        }
    }

    fn paint(&self, ui: &Ui, geometry: &Geometry, text: &str, font: &FontId) {
        let visuals = ui.visuals();
        let painter = ui.painter().with_clip_rect(geometry.screen);
        let body_clip = geometry.body();
        let body = painter.with_clip_rect(body_clip);
        let grid_line = visuals.widgets.noninteractive.bg_stroke;
        let text_color = visuals.text_color();
        let rows = self.shown.len();
        let columns = self.parsed.columns;
        let selection_fill = visuals.selection.bg_fill.gamma_multiply(0.45);
        let find = self.find.as_ref().filter(|f| !f.query.is_empty());
        let query = find.map(Find::query_normalized);
        let current_match = find
            .and_then(|f| f.current.and_then(|i| f.matches.get(i)))
            .map(|m| (m.0 as usize, m.1 as usize));
        let match_fill = visuals.warn_fg_color.gamma_multiply(0.3);

        let visible_rows = geometry.visible_rows();
        let visible_columns = geometry.visible_columns();

        for shown in visible_rows.clone() {
            let row = self.shown[shown] as usize;
            let row_rect = geometry.row_rect(shown);
            if shown % 2 == 1 {
                body.rect_filled(row_rect, 0.0, visuals.faint_bg_color);
            }
            for col in visible_columns.clone() {
                let cell = geometry.cell(shown, col);
                let exists = col < self.parsed.cell_count(row);
                let value = self.parsed.value(text, row, col);
                if self.selection.contains((shown, col), rows, columns) {
                    body.rect_filled(cell, 0.0, selection_fill);
                }
                if let (Some(find), Some(query)) = (find, &query)
                    && find.normalized(&value).contains(query.as_str())
                {
                    body.rect_filled(cell, 0.0, match_fill);
                }
                if !exists && !self.parsed.is_blank(row) {
                    // Past the end of a short row.
                    body.rect_filled(cell, 0.0, visuals.extreme_bg_color.gamma_multiply(0.6));
                }
                if !value.is_empty() {
                    let galley = body.layout_no_wrap(
                        shown_value(&value).into_owned(),
                        font.clone(),
                        text_color,
                    );
                    let y = cell.center().y - galley.size().y / 2.0;
                    let right = self.numeric.get(col).copied().unwrap_or(false);
                    let x = if right {
                        cell.max.x - CELL_PADDING - galley.size().x
                    } else {
                        cell.min.x + CELL_PADDING
                    };
                    let clip = cell.shrink2(vec2(1.0, 0.0)).intersect(body_clip);
                    body.with_clip_rect(clip)
                        .galley(pos2(x, y), galley, text_color);
                }
                body.vline(cell.max.x, cell.y_range(), grid_line);
                if current_match == Some((shown, col)) {
                    body.rect_stroke(
                        cell,
                        0.0,
                        Stroke::new(2.0, visuals.warn_fg_color),
                        StrokeKind::Inside,
                    );
                }
            }
            body.hline(row_rect.x_range(), row_rect.max.y, grid_line);
        }

        // The cursor, unless a cell editor covers it.
        if self.editing.is_none() && rows > 0 && columns > 0 {
            let (row, col) = self.selection.cursor;
            body.rect_stroke(
                geometry.cell(row, col),
                0.0,
                Stroke::new(2.0, visuals.selection.stroke.color),
                StrokeKind::Inside,
            );
        }

        // Drawn over the rows they stay above when scrolling.
        let chrome = visuals.widgets.noninteractive.weak_bg_fill;
        let strong = visuals.strong_text_color();
        let weak = visuals.weak_text_color();
        let header_band = geometry.header_band();
        let header_clip = header_band.with_min_x(geometry.screen.min.x + geometry.gutter);
        painter.rect_filled(header_band, 0.0, chrome);
        let header = painter.with_clip_rect(header_clip);
        let selected_columns = self.selection.columns(columns);
        for col in visible_columns {
            let cell = geometry.header_cell(col);
            if rows > 0 && selected_columns.contains(&col) {
                header.rect_filled(cell, 0.0, selection_fill);
            }
            let name = if self.header {
                self.parsed.value(text, 0, col)
            } else {
                Cow::Borrowed("")
            };
            let (label, color) = if name.is_empty() {
                (column_letters(col), weak)
            } else {
                (shown_value(&name).into_owned(), strong)
            };
            let galley = header.layout_no_wrap(label, font.clone(), color);
            let y = cell.center().y - galley.size().y / 2.0;
            let mut clip = cell.shrink2(vec2(1.0, 0.0)).intersect(header_clip);
            if let Some(sort) = self.sort.filter(|s| s.col == col) {
                clip.max.x -= SORT_ARROW_WIDTH;
                header.text(
                    pos2(cell.max.x - CELL_PADDING, cell.center().y),
                    Align2::RIGHT_CENTER,
                    if sort.descending {
                        icon::CARET_DOWN
                    } else {
                        icon::CARET_UP
                    },
                    font.clone(),
                    strong,
                );
            }
            header
                .with_clip_rect(clip)
                .galley(pos2(cell.min.x + CELL_PADDING, y), galley, color);
            header.vline(cell.max.x, cell.y_range(), grid_line);
        }
        painter.hline(header_band.x_range(), header_band.max.y, grid_line);

        let gutter_band = geometry.gutter_band();
        painter.rect_filled(gutter_band, 0.0, chrome);
        let numbers = painter.with_clip_rect(gutter_band.with_min_y(header_band.max.y));
        let selected_rows = self.selection.rows(rows);
        for shown in visible_rows {
            let row_rect = geometry.row_rect(shown);
            let cell = Rect::from_x_y_ranges(gutter_band.x_range(), row_rect.y_range());
            if selected_rows.contains(&shown) {
                numbers.rect_filled(cell, 0.0, selection_fill);
            }
            numbers.text(
                pos2(cell.max.x - CELL_PADDING, cell.center().y),
                Align2::RIGHT_CENTER,
                (self.shown[shown] + 1).to_string(),
                font.clone(),
                weak,
            );
            numbers.hline(cell.x_range(), cell.max.y, grid_line);
        }
        painter.vline(gutter_band.max.x, gutter_band.y_range(), grid_line);
        painter.rect_filled(geometry.corner(), 0.0, chrome);
    }

    /// The text field over a cell being edited. Keys that finish with the cell
    /// are taken here, before it sees them.
    fn cell_editor(
        &mut self,
        ui: &mut Ui,
        geometry: &Geometry,
        text: &mut String,
        table_id: Id,
        font: &FontId,
        touch: bool,
    ) {
        let Some(edit) = &mut self.editing else {
            return;
        };
        let cell = match edit.shown_row {
            Some(row) => geometry.cell(row, edit.col),
            None => geometry.header_cell(edit.col),
        };
        let id = table_id.with("cell-editor");
        let (cancel, finish) = ui.input_mut(|i| {
            let tab = |i: &mut egui::InputState, shift| {
                i.consume_shortcut(&KeyboardShortcut::new(
                    if shift {
                        Modifiers::SHIFT
                    } else {
                        Modifiers::NONE
                    },
                    Key::Tab,
                ))
            };
            let finish = if i.modifiers.shift && tab(i, true) {
                Some(Move::Previous)
            } else if tab(i, false) {
                Some(Move::Next)
            } else if !i.modifiers.shift && i.consume_key(Modifiers::NONE, Key::Enter) {
                Some(Move::Down)
            } else {
                None
            };
            (i.consume_key(Modifiers::NONE, Key::Escape), finish)
        });
        if cancel {
            self.editing = None;
            self.focus = true;
            return;
        }
        if let Some(finish) = finish {
            let then = if edit.shown_row.is_none() {
                Move::Stay
            } else {
                finish
            };
            self.commit_edit(text, then);
            return;
        }

        // Grows to fit what's typed, over the cells beside and below it.
        let lines = edit.text.split('\n').count().clamp(1, 8);
        let color = ui.visuals().text_color();
        let wanted = ui
            .painter()
            .layout_no_wrap(
                edit.text
                    .lines()
                    .max_by_key(|l| l.len())
                    .unwrap_or("")
                    .to_owned(),
                font.clone(),
                color,
            )
            .size()
            .x
            + 3.0 * CELL_PADDING;
        let room = (geometry.screen.max.x - cell.min.x).max(cell.width());
        let width = wanted.clamp(cell.width(), room.max(MIN_COLUMN_WIDTH));
        let line_height = ui.fonts_mut(|f| f.row_height(font));
        let height = cell
            .height()
            .max(lines as f32 * line_height + 2.0 * CELL_PADDING / 2.0);
        let rect = Rect::from_min_size(cell.min, vec2(width, height));
        let visuals = ui.visuals();
        let fill = visuals.extreme_bg_color;
        let stroke = visuals.selection.stroke.color;
        ui.painter().rect_filled(rect, 0.0, fill);
        if edit.fresh {
            let mut state = TextEditState::load(ui.ctx(), id).unwrap_or_default();
            let end = CCursor::new(edit.text.chars().count());
            state.cursor.set_char_range(Some(CCursorRange::one(end)));
            state.store(ui.ctx(), id);
        }
        let output = ui
            .scope_builder(
                UiBuilder::new()
                    .max_rect(rect.shrink2(vec2(CELL_PADDING - 2.0, 1.0)))
                    .layout(Layout::top_down(egui::Align::Min)),
                |ui| {
                    ui.add(
                        TextEdit::multiline(&mut edit.text)
                            .id(id)
                            .font(font.clone())
                            .frame(egui::Frame::NONE)
                            .margin(vec2(2.0, 0.0))
                            .desired_width(f32::INFINITY)
                            .desired_rows(lines)
                            .return_key(KeyboardShortcut::new(Modifiers::SHIFT, Key::Enter))
                            .event_filter(EventFilter {
                                tab: true,
                                horizontal_arrows: true,
                                vertical_arrows: true,
                                escape: true,
                            }),
                    )
                },
            )
            .inner;
        ui.painter()
            .rect_stroke(rect, 0.0, Stroke::new(2.0, stroke), StrokeKind::Inside);
        if edit.fresh {
            edit.fresh = false;
            output.request_focus();
            ui.memory_mut(|m| {
                m.set_focus_lock_filter(
                    id,
                    EventFilter {
                        tab: true,
                        horizontal_arrows: true,
                        vertical_arrows: true,
                        escape: true,
                    },
                );
            });
        } else if output.lost_focus() {
            self.commit_edit(text, Move::Stay);
            if touch {
                self.focus = false;
            }
        }
    }

    fn menu_ui(&mut self, ui: &mut Ui, editable: bool) {
        let columns = self.selected_columns();
        let row_count = self.selection.rows(self.shown.len()).count();
        let column_count = columns.clone().count();
        let plural = |count: usize, one: &'static str, many: String| {
            if count == 1 { one.to_owned() } else { many }
        };
        let mut chosen = None;
        let mut item = |ui: &mut Ui, enabled: bool, label: String, command: fn() -> Command| {
            if ui.add_enabled(enabled, Button::new(label)).clicked() {
                chosen = Some(command());
            }
        };
        match self.menu {
            Some(Zone::Header(col) | Zone::Edge(col)) => {
                if ui
                    .button(format!("{} Sort ascending", icon::SORT_ASCENDING))
                    .clicked()
                {
                    self.pending.push(Command::Sort(Some(Sort {
                        col,
                        descending: false,
                    })));
                    ui.close();
                }
                if ui
                    .button(format!("{} Sort descending", icon::SORT_DESCENDING))
                    .clicked()
                {
                    self.pending.push(Command::Sort(Some(Sort {
                        col,
                        descending: true,
                    })));
                    ui.close();
                }
                if ui
                    .add_enabled(self.sort.is_some(), Button::new("File order"))
                    .clicked()
                {
                    self.pending.push(Command::Sort(None));
                    ui.close();
                }
                ui.separator();
                if ui.button("Select column").clicked() {
                    self.pending.push(Command::SelectColumn(col));
                    ui.close();
                }
                if self.header
                    && editable
                    && ui
                        .button(format!("{} Rename column", icon::PENCIL_SIMPLE))
                        .clicked()
                {
                    self.pending.push(Command::EditHeader(col));
                    ui.close();
                }
            }
            Some(Zone::Gutter(_)) => {}
            _ => {
                if editable
                    && ui
                        .button(format!("{} Edit cell", icon::PENCIL_SIMPLE))
                        .clicked()
                {
                    self.pending.push(Command::EditCursor);
                    ui.close();
                }
                if ui.button(format!("{} Copy", icon::COPY)).clicked() {
                    ui.ctx().send_viewport_cmd(ViewportCommand::RequestCopy);
                    ui.close();
                }
                if editable {
                    if ui.button(format!("{} Cut", icon::SCISSORS)).clicked() {
                        ui.ctx().send_viewport_cmd(ViewportCommand::RequestCut);
                        ui.close();
                    }
                    if ui.button(format!("{} Paste", icon::CLIPBOARD)).clicked() {
                        ui.ctx().send_viewport_cmd(ViewportCommand::RequestPaste);
                        ui.close();
                    }
                    if ui.button(format!("{} Clear", icon::ERASER)).clicked() {
                        self.pending.push(Command::Clear);
                        ui.close();
                    }
                }
            }
        }
        if !editable {
            return;
        }
        ui.separator();
        let is_header = matches!(self.menu, Some(Zone::Header(_) | Zone::Edge(_)));
        if !is_header {
            item(ui, true, "Insert row above".into(), || Command::InsertRow {
                before: true,
            });
            item(ui, true, "Insert row below".into(), || Command::InsertRow {
                before: false,
            });
            item(
                ui,
                !self.shown.is_empty(),
                plural(row_count, "Delete row", format!("Delete {row_count} rows")),
                || Command::DeleteRows,
            );
            ui.separator();
        }
        item(ui, true, "Insert column left".into(), || {
            Command::InsertColumn { before: true }
        });
        item(ui, true, "Insert column right".into(), || {
            Command::InsertColumn { before: false }
        });
        item(
            ui,
            self.parsed.columns > 0,
            plural(
                column_count,
                "Delete column",
                format!("Delete {column_count} columns"),
            ),
            || Command::DeleteColumns,
        );
        if let Some(command) = chosen {
            self.pending.push(command);
            ui.close();
        }
    }

    pub fn toolbar(&mut self, ui: &mut Ui, editable: bool, options: &mut Options) -> Bar {
        let mut bar = Bar::default();
        let button = |ui: &mut Ui, enabled: bool, glyph: &str, tip: &str| {
            ui.add_enabled(enabled, Button::new(RichText::new(glyph).size(16.0)))
                .on_hover_text(tip)
                .clicked()
        };
        if button(ui, true, icon::MAGNIFYING_GLASS, "Find (Ctrl+F)") {
            bar.find = true;
        }
        if button(
            ui,
            editable && !self.undo.is_empty(),
            icon::ARROW_COUNTER_CLOCKWISE,
            "Undo (Ctrl+Z)",
        ) {
            self.pending.push(Command::Undo);
        }
        if button(
            ui,
            editable && !self.redo.is_empty(),
            icon::ARROW_CLOCKWISE,
            "Redo (Ctrl+Y)",
        ) {
            self.pending.push(Command::Redo);
        }
        ui.separator();
        if button(ui, editable, icon::ROWS_PLUS_BOTTOM, "Add a row below") {
            self.pending.push(Command::InsertRow { before: false });
        }
        if button(
            ui,
            editable,
            icon::COLUMNS_PLUS_RIGHT,
            "Add a column to the right",
        ) {
            self.pending.push(Command::InsertColumn { before: false });
        }
        if let Some(sort) = self.sort {
            ui.separator();
            let label = format!("{} Apply sort to file", icon::SORT_ASCENDING);
            if ui
                .add_enabled(editable, Button::new(label))
                .on_hover_text("Rewrite the rows in the order shown")
                .clicked()
            {
                self.pending.push(Command::ApplySort);
            }
            if ui
                .button("File order")
                .on_hover_text(format!(
                    "Stop sorting by column {}",
                    column_letters(sort.col)
                ))
                .clicked()
            {
                self.pending.push(Command::Sort(None));
            }
        }
        ui.separator();
        delimiter_picker(ui, self, options, &mut bar);
        let mut header = self.header;
        if ui
            .checkbox(&mut header, "Header")
            .on_hover_text("The first row names the columns")
            .changed()
        {
            options.header = Some(header);
            bar.reconfigure = true;
        }
        bar
    }

    pub fn find_bar(&mut self, ui: &mut Ui, text: &str) {
        let mut close = false;
        let mut next = None;
        let Some(find) = &mut self.find else {
            return;
        };
        ui.horizontal_wrapped(|ui| {
            let response = ui.add(
                TextEdit::singleline(&mut find.query)
                    .hint_text("Find in cells")
                    .desired_width(220.0),
            );
            if std::mem::take(&mut find.focus) {
                response.request_focus();
            }
            if response.lost_focus() {
                if ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
                    close = true;
                } else if ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter)) {
                    next = Some(ui.input(|i| i.modifiers.shift));
                    response.request_focus();
                }
            }
            ui.toggle_value(&mut find.match_case, "Aa")
                .on_hover_text("Match case");
            ui.toggle_value(&mut find.filter, icon::FUNNEL)
                .on_hover_text("Show only the rows with a match");
            if ui
                .button(icon::CARET_UP)
                .on_hover_text("Previous (Shift+F3)")
                .clicked()
            {
                next = Some(true);
            }
            if ui
                .button(icon::CARET_DOWN)
                .on_hover_text("Next (F3)")
                .clicked()
            {
                next = Some(false);
            }
            if !find.query.is_empty() {
                let count = find.matches.len();
                let more = if count >= super::ops::MAX_MATCHES {
                    "+"
                } else {
                    ""
                };
                match find.current {
                    _ if count == 0 => ui.colored_label(ui.visuals().warn_fg_color, "No results"),
                    Some(current) => ui.weak(format!("{} of {count}{more}", current + 1)),
                    None => ui.weak(format!("{count}{more} found")),
                };
            }
            if ui.button(icon::X).on_hover_text("Close (Esc)").clicked() {
                close = true;
            }
        });
        if close {
            self.close_find(text);
        } else if let Some(back) = next {
            self.find_next(back);
        }
    }

    /// The problems with the file worth a line above the table.
    pub fn warnings(&self, ui: &mut Ui) {
        let warn = ui.visuals().warn_fg_color;
        if let Some(row) = self.parsed.unterminated {
            ui.colored_label(
                warn,
                format!(
                    "{} A quote in row {} is never closed, so it runs to the end of the file. \
                     Switch to Text to fix it.",
                    icon::WARNING,
                    row + 1
                ),
            );
        }
    }

    pub fn has_warnings(&self) -> bool {
        self.parsed.unterminated.is_some()
    }

    pub fn status(&self, ui: &mut Ui) {
        let rows = self.parsed.rows.len() - self.first_body_row().min(self.parsed.rows.len());
        let shown = self.shown.len();
        let plural = |count: usize, one: &str| {
            if count == 1 {
                format!("{count} {one}")
            } else {
                format!("{count} {one}s")
            }
        };
        if shown == rows {
            ui.weak(plural(rows, "row"));
        } else {
            ui.weak(format!("{shown} of {} shown", plural(rows, "row")));
        }
        ui.weak(plural(self.parsed.columns, "column"));
        if self.parsed.short_rows > 0 {
            ui.weak(format!(
                "{} {} shorter than the others",
                plural(self.parsed.short_rows, "row"),
                if self.parsed.short_rows == 1 {
                    "is"
                } else {
                    "are"
                },
            ))
            .on_hover_text("Filled out when you type in their missing cells");
        }
        if shown > 0 && self.parsed.columns > 0 && !self.selection.is_single() {
            let cells = self.selection.rows(shown).count()
                * self.selection.columns(self.parsed.columns).count();
            ui.weak(format!("{cells} selected"));
        }
    }
}

/// What the table's toolbar asks of its tab.
#[derive(Default)]
pub struct Bar {
    pub find: bool,
    /// The options changed: read the text with them.
    pub reconfigure: bool,
}

fn delimiter_picker(ui: &mut Ui, table: &Table, options: &mut Options, bar: &mut Bar) {
    let name = |byte: u8| {
        parse::DELIMITERS
            .iter()
            .find(|(d, _)| *d == byte)
            .map_or("Other", |(_, name)| name)
    };
    let id = ui.id().with("table-delimiter");
    egui::ComboBox::from_id_salt(id)
        .selected_text(name(table.parsed.delimiter))
        .show_ui(ui, |ui| {
            if ui
                .selectable_label(
                    options.delimiter.is_none(),
                    format!("Detect ({})", name(table.detected_delimiter)),
                )
                .clicked()
            {
                options.delimiter = None;
                bar.reconfigure = true;
            }
            ui.separator();
            for (byte, label) in parse::DELIMITERS {
                let selected = options.delimiter == Some(byte as char);
                if ui.selectable_label(selected, label).clicked() {
                    options.delimiter = Some(byte as char);
                    bar.reconfigure = true;
                }
            }
        })
        .response
        .on_hover_text("What separates the columns");
}

/// The switch between a table and its plain text. Returns whether it changed.
pub fn mode_switch(ui: &mut Ui, text: &mut bool) -> bool {
    let mut changed = false;
    if ui
        .selectable_label(!*text, format!("{} Table", icon::TABLE))
        .clicked()
        && *text
    {
        *text = false;
        changed = true;
    }
    if ui
        .selectable_label(*text, format!("{} Text", icon::FILE_TEXT))
        .clicked()
        && !*text
    {
        *text = true;
        changed = true;
    }
    changed
}

const CELL_PADDING: f32 = 6.0;
const SORT_ARROW_WIDTH: f32 = 18.0;
const MIN_COLUMN_WIDTH: f32 = 36.0;
const MAX_AUTO_COLUMN_WIDTH: f32 = 320.0;
/// How near a column's edge resizes it instead of sorting.
const EDGE_GRAB: f32 = 4.0;
/// The most of a cell laid out: more wouldn't fit, and layout takes time.
const MAX_SHOWN_CHARS: usize = 200;

/// What a cell shows of its value: the first line, not too long.
fn shown_value(value: &str) -> Cow<'_, str> {
    let line = value.split(['\n', '\r']).next().unwrap_or("");
    let cut = line.char_indices().nth(MAX_SHOWN_CHARS).map(|(at, _)| at);
    let line = &line[..cut.unwrap_or(line.len())];
    if line.len() < value.len() {
        Cow::Owned(format!("{line}…"))
    } else {
        Cow::Borrowed(line)
    }
}

/// A, B, ... Z, AA, AB, ...
fn column_letters(col: usize) -> String {
    let mut letters = Vec::new();
    let mut col = col + 1;
    while col > 0 {
        col -= 1;
        letters.push(b'A' + (col % 26) as u8);
        col /= 26;
    }
    letters.reverse();
    String::from_utf8_lossy(&letters).into_owned()
}

fn navigate(
    key: Key,
    jump: bool,
    (row, col): (usize, usize),
    rows: usize,
    columns: usize,
    page: usize,
) -> (usize, usize) {
    let (last_row, last_col) = (rows - 1, columns - 1);
    match key {
        Key::ArrowUp if jump => (0, col),
        Key::ArrowDown if jump => (last_row, col),
        Key::ArrowLeft if jump => (row, 0),
        Key::ArrowRight if jump => (row, last_col),
        Key::ArrowUp => (row.saturating_sub(1), col),
        Key::ArrowDown => ((row + 1).min(last_row), col),
        Key::ArrowLeft => (row, col.saturating_sub(1)),
        Key::ArrowRight => (row, (col + 1).min(last_col)),
        Key::Home if jump => (0, 0),
        Key::End if jump => (last_row, last_col),
        Key::Home => (row, 0),
        Key::End => (row, last_col),
        Key::PageUp => (row.saturating_sub(page), col),
        Key::PageDown => ((row + page).min(last_row), col),
        _ => (row, col),
    }
}

/// Where things are, in screen coordinates. Row offsets go through `f64`: past
/// a few million points `f32` can't place them on a whole pixel.
struct Geometry<'a> {
    origin: Pos2,
    /// In the scrolled content.
    viewport: Rect,
    /// On screen.
    screen: Rect,
    gutter: f32,
    header_height: f32,
    row_height: f32,
    /// Where each column starts past the gutter, and where the last ends.
    prefix: &'a [f32],
    rows: usize,
}

impl Geometry<'_> {
    fn row_y(&self, row: usize) -> f32 {
        let offset = row as f64 * f64::from(self.row_height) - f64::from(self.viewport.min.y);
        self.screen.min.y + self.header_height + offset as f32
    }

    fn column_x(&self, col: usize) -> f32 {
        self.origin.x + self.gutter + self.prefix[col]
    }

    fn cell(&self, row: usize, col: usize) -> Rect {
        let y = self.row_y(row);
        Rect::from_min_max(
            pos2(self.column_x(col), y),
            pos2(self.column_x(col + 1), y + self.row_height),
        )
    }

    fn row_rect(&self, row: usize) -> Rect {
        let y = self.row_y(row);
        Rect::from_min_max(
            pos2(self.screen.min.x, y),
            pos2(self.screen.max.x, y + self.row_height),
        )
    }

    fn header_cell(&self, col: usize) -> Rect {
        Rect::from_min_max(
            pos2(self.column_x(col), self.screen.min.y),
            pos2(
                self.column_x(col + 1),
                self.screen.min.y + self.header_height,
            ),
        )
    }

    fn header_band(&self) -> Rect {
        self.screen
            .with_max_y(self.screen.min.y + self.header_height)
    }

    fn gutter_band(&self) -> Rect {
        self.screen.with_max_x(self.screen.min.x + self.gutter)
    }

    fn corner(&self) -> Rect {
        self.header_band().with_max_x(self.gutter_band().max.x)
    }

    fn body(&self) -> Rect {
        self.screen
            .with_min_x(self.screen.min.x + self.gutter)
            .with_min_y(self.screen.min.y + self.header_height)
    }

    fn visible_rows(&self) -> Range<usize> {
        let top = f64::from(self.viewport.min.y) / f64::from(self.row_height);
        let first = (top.max(0.0) as usize).min(self.rows);
        let count = ((self.screen.height() - self.header_height) / self.row_height).ceil() as usize;
        first..(first + count + 1).min(self.rows)
    }

    fn visible_columns(&self) -> Range<usize> {
        let columns = self.prefix.len().saturating_sub(1);
        let left = self.viewport.min.x;
        let right = self.viewport.max.x - self.gutter;
        let first = self.prefix[1..].partition_point(|&end| end <= left);
        let last = self.prefix.partition_point(|&start| start < right);
        first.min(columns)..last.min(columns)
    }

    fn row_clamped(&self, y: f32) -> usize {
        let from_top = f64::from(y - self.screen.min.y - self.header_height);
        let row = (from_top + f64::from(self.viewport.min.y)) / f64::from(self.row_height);
        (row.max(0.0) as usize).min(self.rows.saturating_sub(1))
    }

    fn column_clamped(&self, x: f32) -> usize {
        let at = x - self.origin.x - self.gutter;
        self.prefix[1..]
            .partition_point(|&end| end <= at)
            .min(self.prefix.len().saturating_sub(2))
    }

    fn zone(&self, pos: Pos2) -> Option<Zone> {
        let columns = self.prefix.len().saturating_sub(1);
        let in_header = pos.y < self.screen.min.y + self.header_height;
        let in_gutter = pos.x < self.screen.min.x + self.gutter;
        if in_header && in_gutter {
            return Some(Zone::Corner);
        }
        let at = pos.x - self.origin.x - self.gutter;
        if in_header {
            // The edge a little past the last column counts too.
            let nearest = self
                .prefix
                .iter()
                .enumerate()
                .skip(1)
                .find(|(_, end)| (**end - at).abs() <= EDGE_GRAB)
                .map(|(index, _)| index - 1);
            if let Some(col) = nearest {
                return Some(Zone::Edge(col));
            }
            let col = self.prefix[1..].partition_point(|&end| end <= at);
            return (col < columns && at >= 0.0).then_some(Zone::Header(col));
        }
        let from_top = f64::from(pos.y - self.screen.min.y - self.header_height);
        let row = (from_top + f64::from(self.viewport.min.y)) / f64::from(self.row_height);
        let row = (row >= 0.0 && (row as usize) < self.rows).then_some(row as usize)?;
        if in_gutter {
            return Some(Zone::Gutter(row));
        }
        let col = self.prefix[1..].partition_point(|&end| end <= at);
        (col < columns && at >= 0.0).then_some(Zone::Cell(row, col))
    }
}

/// How many rows and columns of a table the preview pane shows, and how much
/// of each cell.
const PREVIEW_ROWS: usize = 50;
const PREVIEW_COLUMNS: usize = 30;
const PREVIEW_CHARS: usize = 60;

/// The start of a table, for the preview pane.
pub struct Preview {
    rows: Vec<Vec<String>>,
    header: bool,
    /// How many rows and columns the part read has.
    total_rows: usize,
    columns: usize,
    /// There's more to the file than was read.
    truncated: bool,
}

impl Preview {
    /// `truncated`: `text` is only the start of the file.
    pub fn new(text: &str, path: &Path, truncated: bool) -> Self {
        let parsed = parse::parse(text, detected_delimiter(text, path));
        let mut total_rows = parsed.rows.len();
        // The last row read is probably cut off.
        if truncated && total_rows > 1 {
            total_rows -= 1;
        }
        let header = parse::detect_header(text, &parsed);
        let rows = (0..total_rows.min(PREVIEW_ROWS))
            .map(|row| {
                (0..parsed.cell_count(row).min(PREVIEW_COLUMNS))
                    .map(|col| {
                        let value = parsed.value(text, row, col);
                        let shown = shown_value(&value);
                        match shown.char_indices().nth(PREVIEW_CHARS) {
                            Some((at, _)) => format!("{}…", &shown[..at]),
                            None => shown.into_owned(),
                        }
                    })
                    .collect()
            })
            .collect();
        Self {
            rows,
            header,
            total_rows: total_rows - usize::from(header && total_rows > 0),
            columns: parsed.columns,
            truncated,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn ui(&self, ui: &mut Ui) {
        let frame = egui::Frame::group(ui.style());
        frame.show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.with_layout(Layout::top_down(egui::Align::Min), |ui| {
                ScrollArea::horizontal()
                    .id_salt("table-preview")
                    .scroll_source(touch::scroll_source(ui.ctx()))
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        Grid::new("table-preview-grid")
                            .striped(true)
                            .show(ui, |ui| {
                                for (index, row) in self.rows.iter().enumerate() {
                                    for cell in row {
                                        let mut label = RichText::new(cell).size(11.0);
                                        if self.header && index == 0 {
                                            label = label.strong();
                                        }
                                        ui.add(egui::Label::new(label).extend());
                                    }
                                    ui.end_row();
                                }
                            });
                    });
                let shown = self.rows.len() - usize::from(self.header);
                let rows = if self.truncated {
                    format!("{}+ rows", self.total_rows)
                } else if self.total_rows == 1 {
                    "1 row".to_owned()
                } else {
                    format!("{} rows", self.total_rows)
                };
                let columns = if self.columns == 1 {
                    "1 column".to_owned()
                } else {
                    format!("{} columns", self.columns)
                };
                if shown < self.total_rows || self.truncated {
                    ui.weak(format!("First {shown} of {rows}, {columns}"));
                } else {
                    ui.weak(format!("{rows}, {columns}"));
                }
            });
        });
    }
}

/// What the editor's tests look at.
#[cfg(test)]
impl Table {
    pub fn cursor(&self) -> (usize, usize) {
        self.selection.cursor
    }

    pub fn shown_rows(&self) -> &[u32] {
        &self.shown
    }

    pub fn header_shown(&self) -> bool {
        self.header
    }

    pub fn is_editing(&self) -> bool {
        self.editing.is_some()
    }

    pub fn column_count(&self) -> usize {
        self.parsed.columns
    }

    pub fn match_count(&self) -> usize {
        self.find.as_ref().map_or(0, |f| f.matches.len())
    }

    pub fn cell_value(&self, text: &str, row: usize, col: usize) -> String {
        self.parsed.value(text, row, col).into_owned()
    }

    pub fn header_value(&self, text: &str, col: usize) -> String {
        self.cell_value(text, 0, col)
    }

    pub fn set_edit_text(&mut self, text: &str) {
        self.editing.as_mut().expect("editing").text = text.to_owned();
    }

    pub fn queue_delete_rows(&mut self) {
        self.pending.push(Command::DeleteRows);
    }

    pub fn queue_apply_sort(&mut self) {
        self.pending.push(Command::ApplySort);
    }

    /// Where a header cell or a cell was last drawn.
    pub fn header_center(&self, col: usize) -> Pos2 {
        let drawn = self.drawn.as_ref().expect("drawn");
        pos2(
            drawn.body_left + drawn.prefix[col] + self.widths[col] / 2.0,
            drawn.top + drawn.row_height / 2.0,
        )
    }

    pub fn cell_center(&self, row: usize, col: usize) -> Pos2 {
        let center = self.header_center(col);
        let drawn = self.drawn.as_ref().expect("drawn");
        pos2(center.x, center.y + (row + 1) as f32 * drawn.row_height)
    }
}

/// Where the table was last drawn, unscrolled, for tests to point at.
#[cfg(test)]
struct Drawn {
    body_left: f32,
    top: f32,
    row_height: f32,
    prefix: Vec<f32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_columns_like_a_spreadsheet() {
        assert_eq!(column_letters(0), "A");
        assert_eq!(column_letters(25), "Z");
        assert_eq!(column_letters(26), "AA");
        assert_eq!(column_letters(27), "AB");
        assert_eq!(column_letters(702), "AAA");
    }

    #[test]
    fn shows_the_first_line_of_a_cell() {
        assert_eq!(shown_value("plain"), "plain");
        assert_eq!(shown_value("two\nlines"), "two…");
        assert_eq!(shown_value("two\r\nlines"), "two…");
        assert_eq!(shown_value("\nlater"), "…");
        let long = "é".repeat(MAX_SHOWN_CHARS + 5);
        assert_eq!(shown_value(&long).chars().count(), MAX_SHOWN_CHARS + 1);
    }

    #[test]
    fn recognizes_table_files() {
        for name in ["a.csv", "a.tsv", "A.CSV", "b.Tsv"] {
            assert!(is_table_file(Path::new(name)), "{name}");
        }
        for name in ["a.txt", "csv", "a.csv.gz"] {
            assert!(!is_table_file(Path::new(name)), "{name}");
        }
    }

    #[test]
    fn previews_the_start_of_a_table() {
        let preview = Preview::new("Name,Age\nAl,30\nBo,4", Path::new("a.csv"), true);
        // The row cut off by the end of what was read is left out.
        assert_eq!(preview.rows.len(), 2);
        assert!(preview.header);
        assert_eq!(preview.total_rows, 1);
        assert_eq!(preview.columns, 2);

        let tabs = Preview::new("a\tb\n", Path::new("a.tsv"), false);
        assert_eq!(tabs.rows, [["a", "b"]]);
        assert!(!tabs.header);
    }
}
