//! Reading delimited text without changing it: rows and cells are spans of
//! the text, so what isn't edited can be written back byte for byte. Lenient,
//! as files in the wild are: no quoting rule is an error.

use std::borrow::Cow;

use memchr::{memchr, memchr3};

/// A row's place in the text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Row {
    pub start: u32,
    /// Where its contents end, before the line break.
    pub end: u32,
    /// Where the next row starts: after the line break, if it has one.
    pub next: u32,
    first_cell: u32,
    cell_count: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Parsed {
    pub rows: Vec<Row>,
    /// The cells of all rows, as they are in the text, quotes included.
    cells: Vec<[u32; 2]>,
    /// The most cells in any row.
    pub columns: usize,
    pub delimiter: u8,
    /// The first line break, for new rows.
    pub terminator: &'static str,
    /// The row with a quote that's never closed: it runs to the end of the text.
    pub unterminated: Option<usize>,
    /// Rows with fewer cells than the others, not counting blank lines.
    pub short_rows: usize,
}

impl Parsed {
    pub fn cell_count(&self, row: usize) -> usize {
        self.rows[row].cell_count as usize
    }

    /// The span of a cell, which must exist.
    pub fn span(&self, row: usize, col: usize) -> std::ops::Range<usize> {
        let [start, end] = self.cells[self.rows[row].first_cell as usize + col];
        start as usize..end as usize
    }

    /// A cell as written in the text; `None` if the row is too short to have it.
    pub fn raw<'a>(&self, text: &'a str, row: usize, col: usize) -> Option<&'a str> {
        (col < self.cell_count(row)).then(|| &text[self.span(row, col)])
    }

    /// A cell's value; empty if the row is too short to have it.
    pub fn value<'a>(&self, text: &'a str, row: usize, col: usize) -> Cow<'a, str> {
        self.raw(text, row, col).map_or(Cow::Borrowed(""), decode)
    }

    pub fn is_blank(&self, row: usize) -> bool {
        let row = self.rows[row];
        row.start == row.end
    }
}

/// Splits `text` into rows and cells. Offsets are `u32`: the editor's file
/// size limit keeps text far below 4 GiB.
pub fn parse(text: &str, delimiter: u8) -> Parsed {
    let bytes = text.as_bytes();
    let mut parsed = Parsed {
        delimiter,
        terminator: "",
        ..Default::default()
    };
    let mut at = 0;
    while at < bytes.len() {
        let start = at;
        let first_cell = parsed.cells.len();
        let row = parsed.rows.len();
        loop {
            let cell = at;
            if bytes.get(at) == Some(&b'"') {
                at += 1;
                loop {
                    match memchr(b'"', &bytes[at..]) {
                        None => {
                            at = bytes.len();
                            parsed.unterminated.get_or_insert(row);
                            break;
                        }
                        Some(quote) if bytes.get(at + quote + 1) == Some(&b'"') => {
                            at += quote + 2;
                        }
                        Some(quote) => {
                            at += quote + 1;
                            break;
                        }
                    }
                }
            }
            // The rest of the field; for a quoted one, anything after its
            // closing quote is kept as it is.
            at += memchr3(delimiter, b'\n', b'\r', &bytes[at..]).unwrap_or(bytes.len() - at);
            parsed.cells.push([cell as u32, at as u32]);
            if bytes.get(at) != Some(&delimiter) {
                break;
            }
            at += 1;
            if at == bytes.len() {
                parsed.cells.push([at as u32, at as u32]);
                break;
            }
        }
        let end = at;
        let terminator = match bytes.get(at) {
            Some(b'\r') if bytes.get(at + 1) == Some(&b'\n') => "\r\n",
            Some(b'\r') => "\r",
            Some(b'\n') => "\n",
            _ => "",
        };
        at += terminator.len();
        if parsed.terminator.is_empty() {
            parsed.terminator = terminator;
        }
        let cell_count = parsed.cells.len() - first_cell;
        parsed.columns = parsed.columns.max(cell_count);
        parsed.rows.push(Row {
            start: start as u32,
            end: end as u32,
            next: at as u32,
            first_cell: first_cell as u32,
            cell_count: cell_count as u32,
        });
    }
    if parsed.terminator.is_empty() {
        parsed.terminator = "\n";
    }
    parsed.short_rows = (0..parsed.rows.len())
        .filter(|&row| !parsed.is_blank(row) && parsed.cell_count(row) < parsed.columns)
        .count();
    parsed
}

/// A cell's value, from how it's written.
pub fn decode(raw: &str) -> Cow<'_, str> {
    let Some(mut rest) = raw.strip_prefix('"') else {
        return Cow::Borrowed(raw);
    };
    if let Some(inner) = rest.strip_suffix('"')
        && !inner.contains('"')
    {
        return Cow::Borrowed(inner);
    }
    let mut value = String::with_capacity(rest.len());
    while let Some(quote) = rest.find('"') {
        value.push_str(&rest[..quote]);
        rest = &rest[quote + 1..];
        if rest.starts_with('"') {
            value.push('"');
            rest = &rest[1..];
        } else {
            break;
        }
    }
    value.push_str(rest);
    Cow::Owned(value)
}

/// How to write a value in a cell. A cell that had quotes keeps them.
pub fn encode(value: &str, delimiter: u8, quoted: bool) -> Cow<'_, str> {
    let needs_quotes = value
        .bytes()
        .any(|b| b == delimiter || matches!(b, b'"' | b'\n' | b'\r'));
    if !needs_quotes && !quoted {
        return Cow::Borrowed(value);
    }
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for part in value.split_inclusive('"') {
        out.push_str(part);
        if part.ends_with('"') {
            out.push('"');
        }
    }
    out.push('"');
    Cow::Owned(out)
}

/// The delimiters detection chooses between, the likeliest first.
pub const DELIMITERS: [(u8, &str); 4] = [
    (b',', "Comma"),
    (b';', "Semicolon"),
    (b'\t', "Tab"),
    (b'|', "Pipe"),
];

/// How much of the text detection reads, and how many rows of it.
const SAMPLE_BYTES: usize = 64 * 1024;
const SAMPLE_ROWS: usize = 100;

/// The start of `text`, on a character boundary, and whether that's all of it.
fn sample(text: &str) -> (&str, bool) {
    if text.len() <= SAMPLE_BYTES {
        return (text, true);
    }
    let mut end = SAMPLE_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], false)
}

/// The delimiter that splits the rows most evenly, preferring anything to a
/// comma, then most columns. Comma if none splits them at all.
pub fn detect_delimiter(text: &str) -> u8 {
    let (sample, whole) = sample(text);
    let mut best = (b',', 0.0, 0);
    for (delimiter, _) in DELIMITERS {
        let parsed = parse(sample, delimiter);
        let mut rows = parsed.rows.len().min(SAMPLE_ROWS);
        // The last row of a cut-off sample is cut off too.
        if !whole && rows == parsed.rows.len() && rows > 1 {
            rows -= 1;
        }
        let counts: Vec<usize> = (0..rows)
            .filter(|&row| !parsed.is_blank(row))
            .map(|row| parsed.cell_count(row))
            .collect();
        let Some(mode) = mode(&counts) else {
            continue;
        };
        if mode.0 < 2 {
            continue;
        }
        let even = mode.1 as f32 / counts.len() as f32;
        // Commas also turn up in numbers and text (`1,5;2,5`), so on a tie
        // the rarer character is what's separating the columns.
        let tie = even == best.1;
        if even > best.1
            || (tie && best.0 == b',' && delimiter != b',')
            || (tie && (best.0 == b',') == (delimiter == b',') && mode.0 > best.2)
        {
            best = (delimiter, even, mode.0);
        }
    }
    best.0
}

/// The commonest value, and how many times it occurs.
fn mode(values: &[usize]) -> Option<(usize, usize)> {
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    sorted
        .chunk_by(|a, b| a == b)
        .map(|run| (run[0], run.len()))
        .max_by_key(|&(_, count)| count)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Empty,
    Text,
    /// A number, a date, an email address, a link: not what names a column.
    Data,
}

fn kind(value: &str) -> Kind {
    let value = value.trim();
    let date_like = value.bytes().any(|b| b.is_ascii_digit())
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'-' | b'/' | b':' | b'.' | b' ' | b'T'));
    if value.is_empty() {
        Kind::Empty
    } else if number(value).is_some()
        || date_like
        || value.contains("://")
        || value.contains('@')
        || matches!(
            value.to_ascii_lowercase().as_str(),
            "true" | "false" | "yes" | "no"
        )
    {
        Kind::Data
    } else {
        Kind::Text
    }
}

/// Whether the first row names the columns: it's all text, and in some column
/// the rows below are mostly something else.
pub fn detect_header(text: &str, parsed: &Parsed) -> bool {
    if parsed.rows.len() < 2 {
        return false;
    }
    let first_all_text =
        (0..parsed.cell_count(0)).all(|col| kind(&parsed.value(text, 0, col)) == Kind::Text);
    if !first_all_text {
        return false;
    }
    let below = 1..parsed.rows.len().min(SAMPLE_ROWS + 1);
    (0..parsed.cell_count(0)).any(|col| {
        let mut data = 0;
        let mut filled = 0;
        for row in below.clone() {
            match kind(&parsed.value(text, row, col)) {
                Kind::Empty => {}
                Kind::Text => filled += 1,
                Kind::Data => {
                    data += 1;
                    filled += 1;
                }
            }
        }
        filled > 0 && data * 5 >= filled * 4
    })
}

/// Whether a column is mostly numbers, to line them up on the right.
pub fn is_numeric_column(text: &str, parsed: &Parsed, first_row: usize, col: usize) -> bool {
    let (mut numbers, mut filled) = (0, 0);
    let rows = first_row..parsed.rows.len().min(first_row + SAMPLE_ROWS);
    for row in rows {
        let value = parsed.value(text, row, col);
        if !value.trim().is_empty() {
            filled += 1;
            numbers += usize::from(number(&value).is_some());
        }
    }
    filled > 0 && numbers * 5 >= filled * 4
}

/// The number a cell shows, for sorting: `1200`, `-3.5`, `1,200.50`,
/// `1.200,50`, `3,5`, `$12`, `40%`.
pub fn number(value: &str) -> Option<f64> {
    let value = value.trim();
    let value = value.strip_suffix('%').unwrap_or(value).trim_end();
    let (negative, value) = match value.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, value.strip_prefix('+').unwrap_or(value)),
    };
    let value = value.trim_start_matches(['$', '€', '£', '¥']);
    if !value.starts_with(|c: char| c.is_ascii_digit() || c == '.' || c == ',') {
        return None;
    }
    if !value
        .bytes()
        .all(|b| b.is_ascii_digit() || matches!(b, b'.' | b',' | b'e' | b'E' | b'+' | b'-'))
    {
        return None;
    }
    // The last of two separators is the decimal one; a lone comma is one
    // unless it splits off exactly three digits.
    let cleaned = match (value.rfind('.'), value.rfind(',')) {
        (Some(dot), Some(comma)) if comma > dot => value.replace('.', "").replace(',', "."),
        (_, Some(comma)) if value.matches(',').count() == 1 && value.len() - comma - 1 != 3 => {
            if value.contains('.') {
                value.replace(',', "")
            } else {
                value.replace(',', ".")
            }
        }
        _ => value.replace(',', ""),
    };
    let number: f64 = cleaned.parse().ok()?;
    Some(if negative { -number } else { number })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cells(text: &str, delimiter: u8) -> Vec<Vec<String>> {
        let parsed = parse(text, delimiter);
        (0..parsed.rows.len())
            .map(|row| {
                (0..parsed.cell_count(row))
                    .map(|col| parsed.value(text, row, col).into_owned())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn splits_rows_and_cells() {
        assert_eq!(cells("a,b\nc,d\n", b','), [["a", "b"], ["c", "d"]]);
        // No final line break, and one of each kind.
        assert_eq!(
            cells("a,b\r\nc,d\re,f", b','),
            [["a", "b"], ["c", "d"], ["e", "f"]]
        );
        assert_eq!(parse("", b',').rows.len(), 0);
        assert_eq!(cells("a,", b','), [["a", ""]]);
        assert_eq!(cells(",\n", b','), [["", ""]]);
    }

    #[test]
    fn reads_quotes() {
        assert_eq!(
            cells("\"a,b\",\"say \"\"hi\"\"\"\n\"two\nlines\",x\n", b','),
            [["a,b", "say \"hi\""], ["two\nlines", "x"]]
        );
        // Quotes only count at the start of a cell.
        assert_eq!(cells("5\" disk,x\n", b','), [["5\" disk", "x"]]);
        // Text after the closing quote is kept.
        assert_eq!(cells("\"a\"b,c\n", b','), [["ab", "c"]]);
    }

    #[test]
    fn notices_an_unclosed_quote_and_short_rows() {
        let parsed = parse("a,b\n\"c,d\ne,f\n", b',');
        assert_eq!(parsed.unterminated, Some(1));
        assert_eq!(parsed.rows.len(), 2);
        assert_eq!(parse("a,b,c\nd\n\ne,f,g\n", b',').short_rows, 1);
    }

    #[test]
    fn uses_the_first_line_break_for_new_rows() {
        assert_eq!(parse("a\r\nb\n", b',').terminator, "\r\n");
        assert_eq!(parse("a", b',').terminator, "\n");
        // Not one inside a cell.
        assert_eq!(parse("\"a\r\nb\"\nc\n", b',').terminator, "\n");
    }

    #[test]
    fn encodes_only_what_needs_it() {
        assert_eq!(encode("plain", b',', false), "plain");
        assert_eq!(encode("plain", b',', true), "\"plain\"");
        assert_eq!(encode("a,b", b',', false), "\"a,b\"");
        assert_eq!(encode("a,b", b';', false), "a,b");
        assert_eq!(encode("a;b", b';', false), "\"a;b\"");
        assert_eq!(encode("say \"hi\"", b',', false), "\"say \"\"hi\"\"\"");
        assert_eq!(encode("two\nlines", b',', false), "\"two\nlines\"");
        for value in ["", "a", "a,b", "\"", "\"\"", "a\"b\"", "x\ny"] {
            assert_eq!(decode(&encode(value, b',', false)), value);
            assert_eq!(decode(&encode(value, b',', true)), value);
        }
    }

    #[test]
    fn detects_delimiters() {
        assert_eq!(detect_delimiter("a,b,c\n1,2,3\n"), b',');
        assert_eq!(detect_delimiter("a;b;c\n1,5;2,5;3,5\n"), b';');
        // European decimals: the comma splits just as evenly, or into more.
        assert_eq!(detect_delimiter("caf\u{e9};1,5\nna\u{ef}ve;2,5\n"), b';');
        assert_eq!(detect_delimiter("1,5;2,5\n3,5;4,5\n"), b';');
        // An odd semicolon in the text of a comma-separated file doesn't win.
        assert_eq!(detect_delimiter("a,b\nc;d,e\nf,g\n"), b',');
        assert_eq!(detect_delimiter("a\tb\n1\t2\n"), b'\t');
        assert_eq!(detect_delimiter("a|b\n1|2\n"), b'|');
        assert_eq!(detect_delimiter("single column\nof text\n"), b',');
        assert_eq!(detect_delimiter(""), b',');
        // A comma in a quoted cell doesn't count.
        assert_eq!(detect_delimiter("\"a,b\";c\n\"d,e\";f\n"), b';');
    }

    #[test]
    fn detects_headers() {
        let header = |text: &str| detect_header(text, &parse(text, b','));
        assert!(header("Month,Rent\nJanuary,950\nFebruary,950\n"));
        assert!(header("Name,Joined\nSam,2024-01-05\nAl,2023-11-30\n"));
        assert!(header("Name,Email\nSam,sam@example.org\n"));
        assert!(!header("January,950\nFebruary,950\n"));
        // Nothing tells the first row from the rest.
        assert!(!header("Name,City\nSam,Oslo\nAl,Rome\n"));
        assert!(!header("Name,Rent\n"));
        assert!(!header(""));
    }

    #[test]
    fn reads_numbers() {
        for (text, expected) in [
            ("12", 12.0),
            (" -3.5 ", -3.5),
            ("+7", 7.0),
            ("1,200", 1200.0),
            ("1,200,300", 1_200_300.0),
            ("1,200.50", 1200.5),
            ("1.200,50", 1200.5),
            ("3,5", 3.5),
            ("$12", 12.0),
            ("-$12", -12.0),
            ("40%", 40.0),
            ("1e3", 1000.0),
            (".5", 0.5),
        ] {
            assert_eq!(number(text), Some(expected), "{text}");
        }
        for text in [
            "",
            "abc",
            "NaN",
            "inf",
            "12 apples",
            "1-2-3",
            "2024-01-05",
            "-",
        ] {
            assert_eq!(number(text), None, "{text}");
        }
    }
}
