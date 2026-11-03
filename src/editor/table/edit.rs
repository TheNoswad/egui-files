//! Changes to delimited text, as replacements of the few bytes they touch, so
//! the rest of the file stays exactly as it was and each change can be undone.

use std::{cmp::Ordering, ops::Range};

use super::parse::{self, Parsed};

/// Replaces `remove` bytes at `at`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Splice {
    pub at: usize,
    pub remove: usize,
    pub insert: String,
}

/// Makes the replacements, which are in order and don't overlap. Returns the
/// ones that undo them.
pub fn apply(text: &mut String, splices: &[Splice]) -> Vec<Splice> {
    let change = splices
        .iter()
        .map(|s| s.insert.len() as isize - s.remove as isize)
        .sum::<isize>();
    let mut out = String::with_capacity(text.len().saturating_add_signed(change));
    let mut inverse = Vec::with_capacity(splices.len());
    let mut from = 0;
    for splice in splices {
        out.push_str(&text[from..splice.at]);
        let end = splice.at + splice.remove;
        inverse.push(Splice {
            at: out.len(),
            remove: splice.insert.len(),
            insert: text[splice.at..end].to_owned(),
        });
        out.push_str(&splice.insert);
        from = end;
    }
    out.push_str(&text[from..]);
    *text = out;
    inverse
}

fn is_quoted(raw: &str) -> bool {
    raw.starts_with('"')
}

/// One cell's new value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CellValue {
    pub row: usize,
    pub col: usize,
    pub value: String,
}

/// Sets cells, in rows that exist. A cell past the end of its row makes the
/// row long enough. Cells that already have the value are left alone.
pub fn set_cells(text: &str, parsed: &Parsed, values: &[CellValue]) -> Vec<Splice> {
    let delimiter = parsed.delimiter;
    let mut sorted: Vec<&CellValue> = values.iter().collect();
    sorted.sort_by_key(|cell| (cell.row, cell.col));
    let mut splices = Vec::new();
    for row_cells in sorted.chunk_by(|a, b| a.row == b.row) {
        let row = row_cells[0].row;
        let count = parsed.cell_count(row);
        let (inside, past): (Vec<&CellValue>, Vec<&CellValue>) =
            row_cells.iter().partition(|cell| cell.col < count);
        for cell in inside {
            let span = parsed.span(row, cell.col);
            let raw = &text[span.clone()];
            if parse::decode(raw) != cell.value {
                splices.push(Splice {
                    at: span.start,
                    remove: span.len(),
                    insert: parse::encode(&cell.value, delimiter, is_quoted(raw)).into_owned(),
                });
            }
        }
        let past: Vec<&CellValue> = past.into_iter().filter(|c| !c.value.is_empty()).collect();
        if let Some(last) = past.last() {
            let mut tail = String::new();
            for col in count..=last.col {
                tail.push(delimiter as char);
                if let Some(cell) = past.iter().find(|c| c.col == col) {
                    tail.push_str(&parse::encode(&cell.value, delimiter, false));
                }
            }
            splices.push(Splice {
                at: parsed.rows[row].end as usize,
                remove: 0,
                insert: tail,
            });
        }
    }
    splices
}

/// A row's text: its cells, filled out to `columns`.
fn row_text(cells: &[String], delimiter: u8, columns: usize) -> String {
    let mut out = String::new();
    for col in 0..cells.len().max(columns).max(1) {
        if col > 0 {
            out.push(delimiter as char);
        }
        if let Some(cell) = cells.get(col) {
            out.push_str(&parse::encode(cell, delimiter, false));
        }
    }
    out
}

/// Adds rows at the end, keeping whether the file ends with a line break.
pub fn append_rows(parsed: &Parsed, len: usize, rows: &[Vec<String>]) -> Option<Splice> {
    if rows.is_empty() {
        return None;
    }
    let delimiter = parsed.delimiter;
    let unterminated = parsed.rows.last().is_some_and(|row| row.next == row.end);
    let mut insert = String::new();
    for row in rows {
        if unterminated {
            insert.push_str(parsed.terminator);
        }
        insert.push_str(&row_text(row, delimiter, parsed.columns));
        if !unterminated {
            insert.push_str(parsed.terminator);
        }
    }
    Some(Splice {
        at: len,
        remove: 0,
        insert,
    })
}

/// Adds `count` empty rows before row `at`, or at the end.
pub fn insert_rows(parsed: &Parsed, len: usize, at: usize, count: usize) -> Vec<Splice> {
    let empty = vec![Vec::new(); count];
    match parsed.rows.get(at) {
        Some(row) => {
            let line = row_text(&[], parsed.delimiter, parsed.columns) + parsed.terminator;
            vec![Splice {
                at: row.start as usize,
                remove: 0,
                insert: line.repeat(count),
            }]
        }
        None => append_rows(parsed, len, &empty).into_iter().collect(),
    }
}

/// Merges ranges that are in order, then turns them into removals.
fn removals(ranges: impl IntoIterator<Item = Range<usize>>) -> Vec<Splice> {
    let mut merged: Vec<Range<usize>> = Vec::new();
    for range in ranges {
        match merged.last_mut() {
            Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
            _ => merged.push(range),
        }
    }
    merged
        .into_iter()
        .filter(|range| !range.is_empty())
        .map(|range| Splice {
            at: range.start,
            remove: range.len(),
            insert: String::new(),
        })
        .collect()
}

/// Removes rows, given in ascending order. Removing the last row of a file
/// without a final line break takes the line break before it, so it still
/// has none.
pub fn delete_rows(parsed: &Parsed, rows: &[usize]) -> Vec<Splice> {
    // The rows removed from the end of the file, in one run.
    let mut tail = parsed.rows.len();
    for &index in rows.iter().rev() {
        if index + 1 == tail {
            tail = index;
        }
    }
    let final_break = parsed.rows.last().is_none_or(|row| row.next != row.end);
    removals(rows.iter().map(|&index| {
        let row = parsed.rows[index];
        if !final_break && index == tail && index > 0 {
            parsed.rows[index - 1].end as usize..row.next as usize
        } else {
            row.start as usize..row.next as usize
        }
    }))
}

/// Adds an empty column before column `at`. Rows too short to reach it, and
/// blank lines, stay as they are.
pub fn insert_column(parsed: &Parsed, at: usize) -> Vec<Splice> {
    let delimiter = (parsed.delimiter as char).to_string();
    (0..parsed.rows.len())
        .filter(|&row| !parsed.is_blank(row))
        .filter_map(|row| {
            let count = parsed.cell_count(row);
            let position = match at.cmp(&count) {
                Ordering::Less => parsed.span(row, at).start,
                Ordering::Equal => parsed.rows[row].end as usize,
                Ordering::Greater => return None,
            };
            Some(Splice {
                at: position,
                remove: 0,
                insert: delimiter.clone(),
            })
        })
        .collect()
}

/// Removes columns `columns.start..columns.end`, with the delimiters beside them.
pub fn delete_columns(parsed: &Parsed, columns: Range<usize>) -> Vec<Splice> {
    removals((0..parsed.rows.len()).filter_map(|row| {
        let count = parsed.cell_count(row);
        if columns.start >= count {
            return None;
        }
        let last = columns.end.min(count) - 1;
        Some(if last + 1 < count {
            parsed.span(row, columns.start).start..parsed.span(row, last + 1).start
        } else if columns.start > 0 {
            parsed.span(row, columns.start - 1).end..parsed.span(row, last).end
        } else {
            parsed.span(row, 0).start..parsed.span(row, last).end
        })
    }))
}

/// A cell's place in a sort.
enum SortKey {
    Number(f64),
    Text(String),
    Empty,
}

fn sort_key(value: &str) -> SortKey {
    if value.trim().is_empty() {
        SortKey::Empty
    } else if let Some(number) = parse::number(value) {
        SortKey::Number(number)
    } else {
        SortKey::Text(value.to_lowercase())
    }
}

/// The rows from `first` on, in order of a column: numbers by value, then
/// text ignoring case; descending reverses those. Empty cells go last either
/// way, and rows that compare equal keep their order.
pub fn sorted_rows(
    text: &str,
    parsed: &Parsed,
    first: usize,
    col: usize,
    descending: bool,
) -> Vec<u32> {
    let mut keyed: Vec<(u32, SortKey)> = (first..parsed.rows.len())
        .map(|row| (row as u32, sort_key(&parsed.value(text, row, col))))
        .collect();
    keyed.sort_by(|(_, a), (_, b)| {
        let ascending = match (a, b) {
            (SortKey::Empty, SortKey::Empty) => return Ordering::Equal,
            (SortKey::Empty, _) => return Ordering::Greater,
            (_, SortKey::Empty) => return Ordering::Less,
            (SortKey::Number(a), SortKey::Number(b)) => a.total_cmp(b),
            (SortKey::Number(_), SortKey::Text(_)) => Ordering::Less,
            (SortKey::Text(_), SortKey::Number(_)) => Ordering::Greater,
            (SortKey::Text(a), SortKey::Text(b)) => a.cmp(b),
        };
        if descending {
            ascending.reverse()
        } else {
            ascending
        }
    });
    keyed.into_iter().map(|(row, _)| row).collect()
}

/// Rewrites the rows from `first` on in `order`. Each row keeps its own bytes;
/// the file's final line break, or its lack, stays at the end.
pub fn reorder_rows(text: &str, parsed: &Parsed, first: usize, order: &[u32]) -> Vec<Splice> {
    let Some(last) = parsed.rows.last().filter(|_| first < parsed.rows.len()) else {
        return Vec::new();
    };
    if order
        .iter()
        .enumerate()
        .all(|(i, &row)| row as usize == first + i)
    {
        return Vec::new();
    }
    let start = parsed.rows[first].start as usize;
    let end = last.next as usize;
    let final_break = &text[last.end as usize..end];
    let mut insert = String::with_capacity(end - start + parsed.terminator.len());
    for (index, &row) in order.iter().enumerate() {
        let row = parsed.rows[row as usize];
        insert.push_str(&text[row.start as usize..row.end as usize]);
        if index + 1 < order.len() {
            let own = &text[row.end as usize..row.next as usize];
            insert.push_str(if own.is_empty() {
                parsed.terminator
            } else {
                own
            });
        } else {
            insert.push_str(final_break);
        }
    }
    vec![Splice {
        at: start,
        remove: end - start,
        insert,
    }]
}

/// Cells as the clipboard holds them, for spreadsheets: tabs between cells,
/// quotes around ones with tabs, line breaks or quotes.
pub fn to_clipboard(rows: &[Vec<String>]) -> String {
    let lines: Vec<String> = rows
        .iter()
        .map(|cells| {
            cells
                .iter()
                .map(|cell| parse::encode(cell, b'\t', false))
                .collect::<Vec<_>>()
                .join("\t")
        })
        .collect();
    lines.join("\n")
}

/// Cells from the clipboard. Text that isn't valid in that form, like a lone
/// quote, is taken as it is, split at tabs and line breaks.
pub fn from_clipboard(text: &str) -> Vec<Vec<String>> {
    let parsed = parse::parse(text, b'\t');
    if parsed.unterminated.is_some() {
        return text
            .trim_end_matches(['\r', '\n'])
            .split('\n')
            .map(|line| {
                line.trim_end_matches('\r')
                    .split('\t')
                    .map(str::to_owned)
                    .collect()
            })
            .collect();
    }
    (0..parsed.rows.len())
        .map(|row| {
            (0..parsed.cell_count(row))
                .map(|col| parsed.value(text, row, col).into_owned())
                .collect()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MESSY: &str = "name , \"qty\",note\r\n\"Al\",  1 ,\"a,b\"\r\nBo,2,x\r\n";

    fn parsed(text: &str) -> Parsed {
        parse::parse(text, b',')
    }

    /// Applies a change, checks undoing restores the text, returns the result.
    fn changed(text: &str, splices: &[Splice]) -> String {
        let mut after = text.to_owned();
        let inverse = apply(&mut after, splices);
        let mut back = after.clone();
        apply(&mut back, &inverse);
        assert_eq!(back, text, "undo");
        after
    }

    fn cell(row: usize, col: usize, value: &str) -> CellValue {
        CellValue {
            row,
            col,
            value: value.to_owned(),
        }
    }

    #[test]
    fn setting_a_cell_changes_only_that_cell() {
        let splices = set_cells(MESSY, &parsed(MESSY), &[cell(2, 1, "9")]);
        assert_eq!(
            changed(MESSY, &splices),
            "name , \"qty\",note\r\n\"Al\",  1 ,\"a,b\"\r\nBo,9,x\r\n"
        );
    }

    #[test]
    fn setting_a_cell_quotes_as_needed() {
        // Keeps quotes it had, adds them when the value needs them.
        let splices = set_cells(
            MESSY,
            &parsed(MESSY),
            &[cell(1, 0, "Cy"), cell(2, 2, "y,z")],
        );
        assert_eq!(
            changed(MESSY, &splices),
            "name , \"qty\",note\r\n\"Cy\",  1 ,\"a,b\"\r\nBo,2,\"y,z\"\r\n"
        );
    }

    #[test]
    fn setting_the_same_value_changes_nothing() {
        let same = [
            cell(0, 1, " \"qty\""),
            cell(1, 2, "a,b"),
            cell(1, 1, "  1 "),
        ];
        assert!(set_cells(MESSY, &parsed(MESSY), &same).is_empty());
    }

    #[test]
    fn setting_past_the_end_of_a_row_fills_it_out() {
        let text = "a,b,c\nd\n";
        assert_eq!(
            changed(text, &set_cells(text, &parsed(text), &[cell(1, 2, "x")])),
            "a,b,c\nd,,x\n"
        );
        // Together with its last cell, and skipping cells left empty.
        let both = [cell(1, 0, "D"), cell(1, 1, "e"), cell(1, 3, "")];
        assert_eq!(
            changed(text, &set_cells(text, &parsed(text), &both)),
            "a,b,c\nD,e\n"
        );
        // Clearing a cell that isn't there changes nothing.
        assert!(set_cells(text, &parsed(text), &[cell(1, 2, "")]).is_empty());
        // A blank line.
        let blank = "a,b\n\nc,d\n";
        assert_eq!(
            changed(blank, &set_cells(blank, &parsed(blank), &[cell(1, 1, "x")])),
            "a,b\n,x\nc,d\n"
        );
    }

    #[test]
    fn inserts_rows() {
        let text = "a,b\r\nc,d\r\n";
        let p = parsed(text);
        assert_eq!(
            changed(text, &insert_rows(&p, text.len(), 1, 2)),
            "a,b\r\n,\r\n,\r\nc,d\r\n"
        );
        assert_eq!(
            changed(text, &insert_rows(&p, text.len(), 2, 1)),
            "a,b\r\nc,d\r\n,\r\n"
        );
        // No final line break before, none after.
        let bare = "a,b\nc,d";
        assert_eq!(
            changed(bare, &insert_rows(&parsed(bare), bare.len(), 2, 1)),
            "a,b\nc,d\n,"
        );
        assert_eq!(changed("", &insert_rows(&parsed(""), 0, 0, 1)), "\n");
    }

    #[test]
    fn appends_rows_with_values() {
        let text = "a,b\n";
        let splice = append_rows(&parsed(text), text.len(), &[vec!["x,y".to_owned()]]).unwrap();
        assert_eq!(changed(text, &[splice]), "a,b\n\"x,y\",\n");
    }

    #[test]
    fn deletes_rows() {
        let text = "a\nb\nc\nd\n";
        let p = parsed(text);
        assert_eq!(changed(text, &delete_rows(&p, &[0])), "b\nc\nd\n");
        assert_eq!(changed(text, &delete_rows(&p, &[1, 2])), "a\nd\n");
        assert_eq!(changed(text, &delete_rows(&p, &[3])), "a\nb\nc\n");
        assert_eq!(changed(text, &delete_rows(&p, &[0, 1, 2, 3])), "");
        // No final line break before, none after.
        let bare = "a\nb\nc";
        let p = parsed(bare);
        assert_eq!(changed(bare, &delete_rows(&p, &[2])), "a\nb");
        assert_eq!(changed(bare, &delete_rows(&p, &[1, 2])), "a");
    }

    #[test]
    fn inserts_columns() {
        let text = "a,\"b\"\nc\n\nd,e,f\n";
        let p = parsed(text);
        assert_eq!(
            changed(text, &insert_column(&p, 0)),
            ",a,\"b\"\n,c\n\n,d,e,f\n"
        );
        assert_eq!(
            changed(text, &insert_column(&p, 1)),
            "a,,\"b\"\nc,\n\nd,,e,f\n"
        );
        // The short row doesn't reach it.
        assert_eq!(
            changed(text, &insert_column(&p, 2)),
            "a,\"b\",\nc\n\nd,e,,f\n"
        );
    }

    #[test]
    fn deletes_columns() {
        let text = "a,\"b,c\",d\ne\nf,g\n";
        let p = parsed(text);
        assert_eq!(changed(text, &delete_columns(&p, 0..1)), "\"b,c\",d\n\ng\n");
        assert_eq!(changed(text, &delete_columns(&p, 1..2)), "a,d\ne\nf\n");
        assert_eq!(
            changed(text, &delete_columns(&p, 2..3)),
            "a,\"b,c\"\ne\nf,g\n"
        );
        assert_eq!(changed(text, &delete_columns(&p, 1..3)), "a\ne\nf\n");
        assert_eq!(changed(text, &delete_columns(&p, 0..3)), "\n\n\n");
    }

    #[test]
    fn sorts_numbers_text_and_empties() {
        let text = "h\n10\n9\nbanana\n\nApple\n\"1,000\"\n";
        let sorted = |descending| {
            let p = parsed(text);
            sorted_rows(text, &p, 1, 0, descending)
                .iter()
                .map(|&row| p.value(text, row as usize, 0).into_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(sorted(false), ["9", "10", "1,000", "Apple", "banana", ""]);
        assert_eq!(sorted(true), ["banana", "Apple", "1,000", "10", "9", ""]);
    }

    #[test]
    fn sorting_keeps_equal_rows_in_order() {
        let text = "1,a\n2,b\n1,c\n";
        let p = parsed(text);
        assert_eq!(sorted_rows(text, &p, 0, 0, false), [0, 2, 1]);
        assert_eq!(sorted_rows(text, &p, 0, 0, true), [1, 0, 2]);
    }

    #[test]
    fn reordering_keeps_row_bytes_and_the_final_break() {
        let text = "h\n\"b\" ,1\na,2\n";
        let p = parsed(text);
        assert_eq!(
            changed(text, &reorder_rows(text, &p, 1, &[2, 1])),
            "h\na,2\n\"b\" ,1\n"
        );
        assert!(reorder_rows(text, &p, 1, &[1, 2]).is_empty());
        // Without a final line break, the row now last has none either.
        let bare = "b\na";
        let p = parsed(bare);
        assert_eq!(changed(bare, &reorder_rows(bare, &p, 0, &[1, 0])), "a\nb");
    }

    #[test]
    fn clipboard_round_trips() {
        let rows = vec![
            vec!["a".to_owned(), "b\tc".to_owned()],
            vec!["two\nlines".to_owned(), "say \"hi\"".to_owned()],
        ];
        let text = to_clipboard(&rows);
        assert_eq!(text, "a\t\"b\tc\"\n\"two\nlines\"\t\"say \"\"hi\"\"\"");
        assert_eq!(from_clipboard(&text), rows);
    }

    #[test]
    fn reads_what_spreadsheets_and_people_paste() {
        assert_eq!(from_clipboard("a\tb\r\nc\td\r\n"), [["a", "b"], ["c", "d"]]);
        assert_eq!(from_clipboard("just text"), [["just text"]]);
        assert_eq!(from_clipboard("5\" disk"), [["5\" disk"]]);
        // A lone opening quote isn't a cell running to the end.
        assert_eq!(
            from_clipboard("\"open\tb\nc"),
            [vec!["\"open", "b"], vec!["c"]]
        );
    }
}
