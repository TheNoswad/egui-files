//! Editing a table in markdown source: finding the one at the cursor, moving
//! between its cells, and adding, removing and lining up rows and columns.
//! Only the table's own lines change. Positions are in chars, like the editor's.

use std::ops::Range;

use crate::editor::ops::{Edit, char_to_byte};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Align {
    None,
    Left,
    Center,
    Right,
}

/// A table in the source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Table {
    /// Its lines, in bytes, without the last line break.
    pub bytes: Range<usize>,
    pub indent: String,
    pub head: Vec<String>,
    pub aligns: Vec<Align>,
    pub rows: Vec<Vec<String>>,
    /// Where the cursor is: 0 is the head, 1 the line under it, 2 the first row.
    pub row: usize,
    pub column: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TableAction {
    /// Lines up the pipes.
    Format,
    NextCell,
    PreviousCell,
    AddRowBelow,
    AddRowAbove,
    AddColumnRight,
    AddColumnLeft,
    DeleteRow,
    DeleteColumn,
    Align(Align),
}

/// The cells of a row. A `|` in a cell is escaped, even in code, as in GFM.
fn split_row(line: &str) -> Vec<String> {
    let line = line.trim();
    let line = line.strip_prefix('|').unwrap_or(line);
    let mut cells = Vec::new();
    let mut cell = String::new();
    let mut escaped = false;
    for c in line.chars() {
        if c == '|' && !escaped {
            cells.push(cell.trim().to_owned());
            cell.clear();
        } else {
            cell.push(c);
        }
        escaped = c == '\\' && !escaped;
    }
    // Text after the last pipe is a cell; nothing after it isn't.
    if !cell.trim().is_empty() {
        cells.push(cell.trim().to_owned());
    }
    cells
}

/// The alignment of each column, if this is the line under a table's head.
fn split_delimiters(line: &str) -> Option<Vec<Align>> {
    if !line.contains('-') {
        return None;
    }
    split_row(line)
        .iter()
        .map(|cell| {
            let dashes = cell.trim_start_matches(':').trim_end_matches(':');
            if dashes.is_empty() || !dashes.chars().all(|c| c == '-') {
                return None;
            }
            Some(match (cell.starts_with(':'), cell.ends_with(':')) {
                (true, true) => Align::Center,
                (true, false) => Align::Left,
                (false, true) => Align::Right,
                (false, false) => Align::None,
            })
        })
        .collect()
}

/// Whether this byte is in a fenced code block, where nothing is a table.
pub fn in_code_fence(text: &str, byte: usize) -> bool {
    let mut fence: Option<(char, usize)> = None;
    let mut at = 0;
    for line in text.split_inclusive('\n') {
        if at + line.len() > byte {
            break;
        }
        at += line.len();
        let trimmed = line.trim();
        let Some(mark) = trimmed.chars().next().filter(|c| matches!(c, '`' | '~')) else {
            continue;
        };
        let run = trimmed.chars().take_while(|c| *c == mark).count();
        if run < 3 {
            continue;
        }
        fence = match fence {
            None => Some((mark, run)),
            Some((open, len))
                if open == mark && run >= len && trimmed.chars().all(|c| c == mark) =>
            {
                None
            }
            still => still,
        };
    }
    fence.is_some()
}

/// The table the cursor is in, if it's in one.
pub fn at(text: &str, cursor: usize) -> Option<Table> {
    let byte = char_to_byte(text, cursor);
    if in_code_fence(text, byte) {
        return None;
    }
    let lines: Vec<(usize, &str)> = {
        let mut at = 0;
        text.split_inclusive('\n')
            .map(|line| {
                let start = at;
                at += line.len();
                (start, line.trim_end_matches(['\n', '\r']))
            })
            .collect()
    };
    let line = lines
        .iter()
        .rposition(|(start, _)| *start <= byte)
        .unwrap_or(0);
    let is_row = |index: usize| lines[index].1.contains('|');
    if lines.is_empty() || !is_row(line) {
        return None;
    }
    let first = (0..=line).rev().take_while(|i| is_row(*i)).last()?;
    let last = (line..lines.len()).take_while(|i| is_row(*i)).last()?;
    if last == first {
        return None;
    }
    let aligns = split_delimiters(lines[first + 1].1)?;
    let head = split_row(lines[first].1);
    if head.len() != aligns.len() {
        return None;
    }
    let rows = lines[first + 2..=last]
        .iter()
        .map(|(_, line)| split_row(line))
        .collect();
    let (start, first_line) = lines[first];
    let (last_start, last_line) = lines[last];
    let in_line = &text[lines[line].0..byte];
    let unescaped_pipes = {
        let mut count = 0;
        let mut escaped = false;
        for c in in_line.chars() {
            count += usize::from(c == '|' && !escaped);
            escaped = c == '\\' && !escaped;
        }
        count
    };
    let starts_with_pipe = lines[line].1.trim_start().starts_with('|');
    let column = unescaped_pipes.saturating_sub(usize::from(starts_with_pipe));
    Some(Table {
        bytes: start..last_start + last_line.len(),
        indent: first_line[..first_line.len() - first_line.trim_start().len()].to_owned(),
        head,
        aligns,
        rows,
        row: line - first,
        column,
    })
}

impl Table {
    fn columns(&self) -> usize {
        self.aligns.len().max(1)
    }

    /// Writes the table with its pipes lined up, like most formatters do, and says where each cell's
    /// text starts, in chars from the start of the table, by row and column.
    fn write(&self) -> (String, Vec<Vec<usize>>) {
        let columns = self.columns();
        let cell = |row: &[String], column: usize| row.get(column).cloned().unwrap_or_default();
        let mut widths = vec![3; columns];
        for row in std::iter::once(&self.head).chain(&self.rows) {
            for (column, width) in widths.iter_mut().enumerate() {
                *width = (*width).max(cell(row, column).chars().count());
            }
        }
        let mut out = String::new();
        let mut starts = Vec::new();
        let mut write_row = |out: &mut String, row: &[String], aligned: bool| {
            let mut row_starts = Vec::new();
            out.push_str(&self.indent);
            out.push('|');
            for (column, width) in widths.iter().enumerate() {
                let text = cell(row, column);
                let room = width - text.chars().count();
                let before = match self.aligns.get(column) {
                    Some(Align::Right) if aligned => room,
                    Some(Align::Center) if aligned => room / 2,
                    _ => 0,
                };
                out.push(' ');
                out.extend(std::iter::repeat_n(' ', before));
                row_starts.push(out.chars().count());
                out.push_str(&text);
                out.extend(std::iter::repeat_n(' ', room - before));
                out.push_str(" |");
            }
            out.push('\n');
            starts.push(row_starts);
        };
        write_row(&mut out, &self.head, true);
        let delimiters: Vec<String> = widths
            .iter()
            .enumerate()
            .map(|(column, width)| {
                let (left, right) = match self.aligns.get(column) {
                    Some(Align::Left) => (true, false),
                    Some(Align::Right) => (false, true),
                    Some(Align::Center) => (true, true),
                    _ => (false, false),
                };
                let dashes = width - usize::from(left) - usize::from(right);
                format!(
                    "{}{}{}",
                    if left { ":" } else { "" },
                    "-".repeat(dashes),
                    if right { ":" } else { "" }
                )
            })
            .collect();
        write_row(&mut out, &delimiters, false);
        for row in &self.rows {
            write_row(&mut out, row, true);
        }
        out.pop();
        (out, starts)
    }

    fn insert_column(&mut self, at: usize) {
        let at = at.min(self.columns());
        self.aligns.insert(at, Align::None);
        self.head.insert(at.min(self.head.len()), String::new());
        for row in &mut self.rows {
            if at <= row.len() {
                row.insert(at, String::new());
            }
        }
    }

    fn remove_column(&mut self, at: usize) {
        if at >= self.aligns.len() || self.aligns.len() <= 1 {
            return;
        }
        self.aligns.remove(at);
        if at < self.head.len() {
            self.head.remove(at);
        }
        for row in &mut self.rows {
            if at < row.len() {
                row.remove(at);
            }
        }
    }
}

/// Applies an action to the table at the cursor. `None` if there's none there.
pub fn apply(text: &str, cursor: usize, action: TableAction) -> Option<Edit> {
    let mut table = at(text, cursor)?;
    let columns = table.columns();
    // Where to put the cursor: by line (as `row` counts them) and column.
    let mut to = (table.row, table.column.min(columns - 1));
    // The line under the head has no text to be in.
    if to.0 == 1 {
        to.0 = 0;
    }
    match action {
        TableAction::Format => {}
        TableAction::NextCell => {
            if to.1 + 1 < columns {
                to.1 += 1;
            } else {
                to = (if to.0 == 0 { 2 } else { to.0 + 1 }, 0);
                if to.0 - 2 >= table.rows.len() {
                    table.rows.push(vec![String::new(); columns]);
                }
            }
        }
        TableAction::PreviousCell => {
            if to.1 > 0 {
                to.1 -= 1;
            } else if to.0 > 0 {
                to = (if to.0 == 2 { 0 } else { to.0 - 1 }, columns - 1);
            }
        }
        TableAction::AddRowBelow => {
            let at = if to.0 == 0 { 0 } else { to.0 - 1 };
            table.rows.insert(at, vec![String::new(); columns]);
            to = (at + 2, 0);
        }
        TableAction::AddRowAbove => {
            // Nothing goes above the head.
            let at = to.0.saturating_sub(2);
            table.rows.insert(at, vec![String::new(); columns]);
            to = (at + 2, 0);
        }
        TableAction::AddColumnRight => {
            table.insert_column(to.1 + 1);
            to.1 += 1;
        }
        TableAction::AddColumnLeft => table.insert_column(to.1),
        TableAction::DeleteRow => {
            if to.0 >= 2 {
                table.rows.remove(to.0 - 2);
                to.0 = if to.0 - 2 < table.rows.len() {
                    to.0
                } else if table.rows.is_empty() {
                    0
                } else {
                    to.0 - 1
                };
            }
        }
        TableAction::DeleteColumn => {
            table.remove_column(to.1);
            to.1 = to.1.min(table.columns() - 1);
        }
        TableAction::Align(align) => {
            if let Some(slot) = table.aligns.get_mut(to.1) {
                *slot = align;
            }
        }
    }
    let (written, starts) = table.write();
    let mut new = text.to_owned();
    new.replace_range(table.bytes.clone(), &written);
    let before = text[..table.bytes.start].chars().count();
    let at = before
        + starts
            .get(to.0)
            .and_then(|row| row.get(to.1))
            .copied()
            .unwrap_or(0);
    // Selects the cell's text, so typing replaces it.
    let selection_end = match action {
        TableAction::NextCell | TableAction::PreviousCell => {
            let row = if to.0 == 0 {
                Some(&table.head)
            } else {
                table.rows.get(to.0.wrapping_sub(2))
            };
            at + row
                .and_then(|row| row.get(to.1))
                .map_or(0, |cell| cell.chars().count())
        }
        _ => at,
    };
    Some(Edit {
        text: new,
        selection: at..selection_end,
    })
}

/// A new table, for inserting.
pub fn template() -> &'static str {
    "| Column | Column |\n| ------ | ------ |\n|        |        |\n"
}

#[cfg(test)]
mod tests {
    use super::*;

    const TABLE: &str = "before\n\n| a | b |\n|:--|--:|\n| 1 | 2 |\n| 3 | 4 |\n\nafter\n";

    fn cursor_at(text: &str, needle: &str) -> usize {
        text[..text.find(needle).unwrap()].chars().count()
    }

    fn run(action: TableAction, needle: &str) -> Edit {
        apply(TABLE, cursor_at(TABLE, needle), action).unwrap()
    }

    fn selected(edit: &Edit) -> String {
        edit.text
            .chars()
            .skip(edit.selection.start)
            .take(edit.selection.end - edit.selection.start)
            .collect()
    }

    /// The text around the table never changes.
    fn assert_outside_kept(edit: &Edit) {
        assert!(edit.text.starts_with("before\n\n|"), "{}", edit.text);
        assert!(edit.text.ends_with("|\n\nafter\n"), "{}", edit.text);
    }

    #[test]
    fn finds_the_table_and_the_cell() {
        let table = at(TABLE, cursor_at(TABLE, "4")).unwrap();
        assert_eq!(table.head, ["a", "b"]);
        assert_eq!(table.aligns, [Align::Left, Align::Right]);
        assert_eq!(table.rows, [["1", "2"], ["3", "4"]]);
        assert_eq!((table.row, table.column), (3, 1));
        let table = at(TABLE, cursor_at(TABLE, "a")).unwrap();
        assert_eq!((table.row, table.column), (0, 0));
    }

    #[test]
    fn what_isnt_a_table_isnt_found() {
        for (text, needle) in [
            ("just text", "text"),
            ("a | b\nno table", "a"),
            ("| a |\n| b |\n", "a"),
            ("| a | b |\n|---|\n| 1 | 2 |\n", "a"),
            ("```\n| a |\n|---|\n```\n", "a"),
            ("~~~~\n| a |\n|---|\n~~~\n", "a"),
        ] {
            assert_eq!(at(text, cursor_at(text, needle)), None, "{text:?}");
        }
    }

    #[test]
    fn a_table_after_a_closed_fence_is_found() {
        let text = "```\ncode\n```\n\n| a |\n|---|\n";
        assert!(at(text, cursor_at(text, "a")).is_some());
    }

    #[test]
    fn tables_without_outer_pipes_work() {
        let text = "a | b\n---|---\n1 | 2\n";
        let table = at(text, cursor_at(text, "2")).unwrap();
        assert_eq!(table.head, ["a", "b"]);
        assert_eq!(table.column, 1);
        let edit = apply(text, cursor_at(text, "2"), TableAction::Format).unwrap();
        assert_eq!(edit.text, "| a   | b   |\n| --- | --- |\n| 1   | 2   |\n");
    }

    #[test]
    fn formatting_lines_up_pipes_and_keeps_alignment() {
        let text = "| Name | N |\n|:-|-:|\n| long name here | 1 |\n";
        let edit = apply(text, 2, TableAction::Format).unwrap();
        assert_eq!(
            edit.text,
            "| Name           |   N |\n\
             | :------------- | --: |\n\
             | long name here |   1 |\n"
        );
    }

    #[test]
    fn formatting_twice_changes_nothing() {
        let once = run(TableAction::Format, "1");
        assert_outside_kept(&once);
        let cursor = cursor_at(&once.text, "1");
        let twice = apply(&once.text, cursor, TableAction::Format).unwrap();
        assert_eq!(once.text, twice.text);
    }

    #[test]
    fn escaped_pipes_stay_in_their_cell() {
        let text = "| a \\| b | c |\n|---|---|\n| `x\\|y` | z |\n";
        let table = at(text, 2).unwrap();
        assert_eq!(table.head, ["a \\| b", "c"]);
        assert_eq!(table.rows[0], ["`x\\|y`", "z"]);
        let edit = apply(text, 2, TableAction::Format).unwrap();
        assert!(edit.text.contains("a \\| b"));
    }

    #[test]
    fn tab_moves_through_cells_and_selects_them() {
        let edit = run(TableAction::NextCell, "1");
        assert_eq!(selected(&edit), "2");
        assert_outside_kept(&edit);
        let edit = run(TableAction::NextCell, "2");
        assert_eq!(selected(&edit), "3", "wraps to the next row");
        let edit = run(TableAction::NextCell, "b |");
        assert_eq!(selected(&edit), "1", "from the head past the line under it");
        let edit = run(TableAction::PreviousCell, "3");
        assert_eq!(selected(&edit), "2");
        let edit = run(TableAction::PreviousCell, "1");
        assert_eq!(selected(&edit), "b", "back into the head");
    }

    #[test]
    fn tab_past_the_end_adds_a_row() {
        let edit = run(TableAction::NextCell, "4");
        assert_outside_kept(&edit);
        let table = at(&edit.text, edit.selection.start).unwrap();
        assert_eq!(table.rows.len(), 3);
        assert_eq!((table.row, table.column), (4, 0));
    }

    #[test]
    fn rows_come_and_go() {
        let edit = run(TableAction::AddRowBelow, "1");
        assert_outside_kept(&edit);
        let table = at(&edit.text, edit.selection.start).unwrap();
        assert_eq!(table.rows, [vec!["1", "2"], vec!["", ""], vec!["3", "4"]]);
        assert_eq!(table.row, 3, "on the new row");

        let edit = run(TableAction::AddRowAbove, "1");
        let table = at(&edit.text, edit.selection.start).unwrap();
        assert_eq!(table.rows, [vec!["", ""], vec!["1", "2"], vec!["3", "4"]]);
        assert_eq!(table.row, 2);

        let edit = run(TableAction::AddRowAbove, "a");
        let table = at(&edit.text, edit.selection.start).unwrap();
        assert_eq!(table.head, ["a", "b"], "nothing goes above the head");

        let edit = run(TableAction::DeleteRow, "1");
        let table = at(&edit.text, edit.selection.start).unwrap();
        assert_eq!(table.rows, [["3", "4"]]);

        let edit = run(TableAction::DeleteRow, "a");
        let table = at(&edit.text, edit.selection.start).unwrap();
        assert_eq!(table.head, ["a", "b"], "the head stays");
    }

    #[test]
    fn columns_come_and_go() {
        let edit = run(TableAction::AddColumnRight, "1");
        assert_outside_kept(&edit);
        let table = at(&edit.text, edit.selection.start).unwrap();
        assert_eq!(table.head, ["a", "", "b"]);
        assert_eq!(table.aligns, [Align::Left, Align::None, Align::Right]);
        assert_eq!(table.rows[1], ["3", "", "4"]);
        assert_eq!(table.column, 1, "in the new column");

        let edit = run(TableAction::AddColumnLeft, "1");
        let table = at(&edit.text, edit.selection.start).unwrap();
        assert_eq!(table.head, ["", "a", "b"]);

        let edit = run(TableAction::DeleteColumn, "2");
        let table = at(&edit.text, edit.selection.start).unwrap();
        assert_eq!(table.head, ["a"]);
        assert_eq!(table.rows, [["1"], ["3"]]);
    }

    #[test]
    fn the_last_column_stays() {
        let text = "| a |\n|---|\n| 1 |\n";
        let edit = apply(text, 2, TableAction::DeleteColumn).unwrap();
        assert!(edit.text.contains("| a "), "{}", edit.text);
    }

    #[test]
    fn rows_short_of_cells_are_filled_in() {
        let text = "| a | b |\n|---|---|\n| 1 |\n";
        let edit = apply(text, 2, TableAction::Format).unwrap();
        assert_eq!(edit.text, "| a   | b   |\n| --- | --- |\n| 1   |     |\n");
    }

    #[test]
    fn cells_past_the_head_stay() {
        let text = "| a |\n|---|\n| 1 | extra |\n";
        let table = at(text, cursor_at(text, "1")).unwrap();
        assert_eq!(table.rows[0], ["1", "extra"]);
    }

    #[test]
    fn alignment_changes_the_line_under_the_head() {
        let edit = run(TableAction::Align(Align::Center), "1");
        let table = at(&edit.text, edit.selection.start).unwrap();
        assert_eq!(table.aligns, [Align::Center, Align::Right]);
    }

    #[test]
    fn indented_tables_stay_indented() {
        let text = "  | a |\n  |---|\n  | 1 |\n";
        let edit = apply(text, 3, TableAction::AddRowBelow).unwrap();
        assert!(edit.text.lines().all(|line| line.starts_with("  |")));
    }

    #[test]
    fn non_ascii_text_keeps_positions_right() {
        let text = "é\n\n| é | 日本語 |\n|---|---|\n| ñ | ü |\n";
        let edit = apply(text, cursor_at(text, "ñ"), TableAction::NextCell).unwrap();
        assert_eq!(selected(&edit), "ü");
    }

    #[test]
    fn crlf_lines_are_read() {
        let text = "| a |\r\n|---|\r\n| 1 |\r\n";
        let table = at(text, 2).unwrap();
        assert_eq!(table.rows, [["1"]]);
    }

    #[test]
    fn the_template_is_a_table() {
        let text = template();
        assert_eq!(at(text, 2).unwrap().head, ["Column", "Column"]);
        assert_eq!(apply(text, 2, TableAction::Format).unwrap().text, text);
    }
}
