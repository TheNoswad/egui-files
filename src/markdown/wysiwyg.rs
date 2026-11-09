//! Editing the rendered page itself: a cursor in the text, not a box of markdown.
//!
//! The source stays the only document. The cursor is an offset in the source,
//! and each key is a splice of it, after which it's parsed and laid out again
//! and the cursor is found on the page anew. What makes that work is knowing
//! which bytes of the source each character on the page came from: [`spans`].
//!
//! Markup that doesn't show is left alone: taking out the middle of `**bold**
//! text` takes out only the characters seen. What's typed goes in as it is, so
//! typing `**` or `# ` is writing markdown, as it would be in the source.

use std::ops::Range;

/// What a piece of text on the page is, for what the keys do in it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldKind {
    /// A paragraph, heading or list item: its text runs on into the next.
    Inline,
    /// A code block's text: lines are lines, and it stands alone.
    Code,
    /// A table's cell, which is on one line and has `|` to look out for.
    Cell,
}

/// Text on the page that can be edited, with where each character came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Editable {
    /// For each char of the text, increasing.
    pub spans: Vec<Range<usize>>,
    /// Where the cursor goes when there's no text.
    pub start: usize,
    pub kind: FieldKind,
    /// Where its last line ends. What's on it past the text doesn't show,
    /// spaces and closing marks, and a cursor there is still in this field:
    /// one of the spaces is what's left after taking back a word.
    end: usize,
}

impl Editable {
    pub fn new(spans: Vec<Range<usize>>, start: usize, kind: FieldKind, source: &str) -> Self {
        let high = spans
            .last()
            .map_or(start, |span| span.end)
            .min(source.len());
        let end = if kind == FieldKind::Inline {
            let rest = &source[high..];
            high + rest.find(['\n', '\r']).unwrap_or(rest.len())
        } else {
            high
        };
        Self {
            spans,
            start,
            kind,
            end,
        }
    }

    /// Where its text starts in the source.
    pub fn low(&self) -> usize {
        self.spans.first().map_or(self.start, |span| span.start)
    }

    /// Where its text ends in the source.
    pub fn high(&self) -> usize {
        self.spans.last().map_or(self.start, |span| span.end)
    }

    /// Which char of the text the cursor at this offset is before.
    pub fn char_at(&self, offset: usize) -> usize {
        self.spans.partition_point(|span| span.end <= offset)
    }

    /// The offset of the cursor before this char of the text. What's before it
    /// comes first, so that typing there continues what was being read.
    pub fn offset_at(&self, char: usize) -> usize {
        match char.checked_sub(1).and_then(|last| self.spans.get(last)) {
            Some(span) => span.end,
            None if char == 0 => self.low(),
            None => self.high(),
        }
    }

    fn contains(&self, offset: usize) -> bool {
        (self.low()..=self.end.max(self.high())).contains(&offset)
    }
}

/// Finds where in the source each char of a run of text came from, carrying on
/// from `cursor`. `range` holds it, and maybe more, as with `==marks==` split
/// out of a run after parsing. A char that isn't in the source, as an emoji
/// from a `:shortcode:`, gets all that's between its neighbors.
pub fn spans(
    rendered: &str,
    source: &str,
    range: Range<usize>,
    cursor: &mut usize,
) -> Vec<Range<usize>> {
    let limit = range.end.min(source.len());
    *cursor = (*cursor).max(range.start).min(limit);
    let chars: Vec<char> = rendered.chars().collect();
    let mut out: Vec<Option<Range<usize>>> = Vec::with_capacity(chars.len());
    for (index, &ch) in chars.iter().enumerate() {
        let found = locate(ch, chars.get(index + 1).copied(), source, *cursor, limit);
        if let Some(span) = &found {
            *cursor = span.end;
        }
        out.push(found);
    }
    // What wasn't found takes what's between what was.
    let mut filled = Vec::with_capacity(out.len());
    let mut at = range.start.min(limit);
    for index in 0..out.len() {
        match &out[index] {
            Some(span) => {
                filled.push(span.clone());
                at = span.end;
            }
            None => {
                let next = out[index + 1..]
                    .iter()
                    .flatten()
                    .next()
                    .map_or(limit, |span| span.start);
                filled.push(at..next);
                at = next;
            }
        }
    }
    // What's at the start of the next line of a quote goes with its line break.
    for index in 0..filled.len().saturating_sub(1) {
        let is_break = source
            .as_bytes()
            .get(filled[index].end.wrapping_sub(1))
            .is_some_and(|&byte| byte == b'\n');
        if is_break {
            filled[index].end = filled[index + 1].start;
        }
    }
    filled
}

/// Where this char is, from `from` on.
fn locate(
    ch: char,
    next: Option<char>,
    source: &str,
    from: usize,
    limit: usize,
) -> Option<Range<usize>> {
    let rest = source.get(from..limit)?;
    // An entity, which shows as something else.
    if let Some(len) = entity_len(rest) {
        let literal = ch == '&' && rest[1..].chars().next() == next;
        if !literal {
            return Some(from..from + len);
        }
    }
    for (at, c) in rest.char_indices() {
        let at = from + at;
        let matched = c == ch || (ch == ' ' && matches!(c, '\n' | '\r'));
        if matched {
            let mut start = at;
            let mut end = at + c.len_utf8();
            if c == '\r' && source[end..].starts_with('\n') {
                end += 1;
            }
            // Its escape goes with it.
            if ch.is_ascii_punctuation() && start > from && source.as_bytes()[start - 1] == b'\\' {
                start -= 1;
            }
            // A hard break is what's before it on the line too.
            if ch == '\n' {
                let line = &source[from..start];
                start = from + line.trim_end_matches(' ').trim_end_matches('\\').len();
            }
            return Some(start..end);
        }
    }
    None
}

/// How long the entity at the start of the text is.
fn entity_len(text: &str) -> Option<usize> {
    let body = text.strip_prefix('&')?;
    let len = body.find(';')?;
    let good = len > 0
        && len <= 32
        && body[..len]
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '#');
    good.then_some(len + 2)
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The word this char is in or just after, by char.
pub fn word_chars(chars: &[char], at: usize) -> (usize, usize) {
    let at = at.min(chars.len());
    let on_word = chars.get(at).is_some_and(|c| is_word(*c));
    let after_word = at > 0 && is_word(chars[at - 1]);
    if !on_word && !after_word {
        return (at, (at + 1).min(chars.len()));
    }
    let start = chars[..at]
        .iter()
        .rposition(|c| !is_word(*c))
        .map_or(0, |i| i + 1);
    let end = chars[at..]
        .iter()
        .position(|c| !is_word(*c))
        .map_or(chars.len(), |i| at + i);
    (start, end)
}

/// The start of the word before this char.
pub fn word_back(chars: &[char], at: usize) -> usize {
    let at = at.min(chars.len());
    let last = chars[..at]
        .iter()
        .rposition(|c| is_word(*c))
        .map_or(0, |i| i + 1);
    chars[..last]
        .iter()
        .rposition(|c| !is_word(*c))
        .map_or(0, |i| i + 1)
}

/// The end of the word after this char.
pub fn word_forward(chars: &[char], at: usize) -> usize {
    let at = at.min(chars.len());
    let start = chars[at..]
        .iter()
        .position(|c| is_word(*c))
        .map_or(chars.len(), |i| at + i);
    chars[start..]
        .iter()
        .position(|c| !is_word(*c))
        .map_or(chars.len(), |i| start + i)
}

/// A change of the source, and where the cursor goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub text: String,
    pub cursor: usize,
}

/// The field that has this offset, or the nearest after it, or the last.
pub fn field_at<'a>(fields: &[&'a Editable], offset: usize) -> Option<(usize, &'a Editable)> {
    let found = fields
        .iter()
        .position(|field| field.contains(offset))
        .or_else(|| {
            fields
                .iter()
                .enumerate()
                .filter(|(_, field)| field.low() > offset)
                .min_by_key(|(_, field)| field.low())
                .map(|(index, _)| index)
        })
        .or_else(|| fields.len().checked_sub(1));
    found.map(|index| (index, fields[index]))
}

/// Whether the cursor here has no block to be in, as after Enter at the end
/// of a paragraph: on an empty line among blank ones, or in an empty document.
pub fn in_gap(text: &str, offset: usize) -> bool {
    let Some(before) = text.get(..offset) else {
        return false;
    };
    let after = &text[offset..];
    let line = before.rfind('\n').map_or(0, |at| at + 1);
    let (this, below) = after.split_once('\n').unwrap_or((after, ""));
    if !before[line..].trim().is_empty() || !this.trim().is_empty() {
        return false;
    }
    // Right under a line of text, what's typed would go on with it.
    let above = before[..line].trim_end_matches(['\r', '\n']);
    let above_blank = line == 0
        || above.is_empty()
        || before[..line]
            .lines()
            .next_back()
            .is_some_and(|l| l.trim().is_empty());
    let below_blank = below.lines().next().is_none_or(|l| l.trim().is_empty());
    above_blank && below_blank
}

/// Puts text in place of the selection.
pub fn insert(text: &str, selection: Range<usize>, typed: &str, kind: Option<FieldKind>) -> Change {
    let typed = if kind == Some(FieldKind::Cell) {
        typed.replace('|', "\\|").replace(['\n', '\r'], " ")
    } else {
        typed.to_owned()
    };
    let cursor = selection.start + typed.len();
    let mut text = text.to_owned();
    text.replace_range(selection, &typed);
    Change { text, cursor }
}

/// Takes out what's seen of the selection, and leaves the markup around it.
pub fn delete_selection(
    text: &str,
    fields: &[&Editable],
    selection: Range<usize>,
) -> Option<Change> {
    if selection.is_empty() {
        return None;
    }
    let (first_index, first) = field_at(fields, selection.start)?;
    let (last_index, last) = field_at(fields, selection.end)?;
    let plain = |selection: Range<usize>| {
        let mut out = text.to_owned();
        out.replace_range(selection.clone(), "");
        Change {
            text: out,
            cursor: selection.start,
        }
    };
    // The whole document, or what isn't all in text that can be kept apart.
    if !first.contains(selection.start) || !last.contains(selection.end) {
        return Some(plain(selection));
    }
    let mut cuts: Vec<Range<usize>> = Vec::new();
    let seen = |field: &Editable, range: Range<usize>, cuts: &mut Vec<Range<usize>>| {
        let (from, to) = (field.char_at(range.start), field.char_at(range.end));
        for span in &field.spans[from..to] {
            cuts.push(span.clone());
        }
    };
    if first_index == last_index {
        seen(first, selection.clone(), &mut cuts);
        // Spaces typed after the text, which haven't a char of their own.
        if selection.end > first.high() {
            cuts.push(first.high().max(selection.start)..selection.end);
        }
    } else {
        if first.kind != FieldKind::Inline || last.kind != FieldKind::Inline {
            return Some(plain(selection));
        }
        seen(first, selection.start..first.high(), &mut cuts);
        seen(last, last.low()..selection.end, &mut cuts);
        cuts.push(block_end(text, first)..last.low());
    }
    Some(cut(text, cuts, selection.start))
}

/// Takes these out of the text, and tidies up where they were.
fn cut(text: &str, mut cuts: Vec<Range<usize>>, cursor: usize) -> Change {
    cuts.retain(|range| !range.is_empty());
    cuts.sort_by_key(|range| range.start);
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    let mut new_cursor = cursor;
    // Where each was, in what's left.
    let mut places = Vec::with_capacity(cuts.len());
    for range in &cuts {
        if range.start < at {
            continue;
        }
        out.push_str(&text[at..range.start]);
        places.push(out.len());
        if range.start < cursor {
            new_cursor -= range.end.min(cursor) - range.start;
        }
        at = range.end;
    }
    out.push_str(&text[at..]);
    let mut change = Change {
        text: out,
        cursor: new_cursor,
    };
    // From the end, so the places before stay where they are.
    for place in places.into_iter().rev() {
        tidy(&mut change, place);
    }
    change
}

/// Markup left with nothing in it would show as what it is.
fn tidy(change: &mut Change, place: usize) {
    let before = &change.text[..place];
    let after = &change.text[place..];
    let empty = ["**", "__", "~~", "==", "*", "_", "~", "`"]
        .into_iter()
        .find(|mark| before.ends_with(mark) && after.starts_with(mark))
        .map(|mark| place - mark.len()..place + mark.len())
        .or_else(|| {
            let close = after.strip_prefix("](")?.find(')')? + 2;
            before.ends_with('[').then(|| place - 1..place + close + 1)
        })
        .or_else(|| {
            let len = after.strip_prefix("</")?.find('>')? + 2;
            let open = before.rfind('<')?;
            let tag = before[open + 1..].strip_suffix('>')?;
            let name = tag.split_whitespace().next()?;
            name.eq_ignore_ascii_case(&after[2..len])
                .then(|| open..place + len + 1)
        });
    let Some(empty) = empty else {
        return;
    };
    change.text.replace_range(empty.clone(), "");
    if change.cursor >= empty.end {
        change.cursor -= empty.len();
    } else if change.cursor > empty.start {
        change.cursor = empty.start;
    }
}

/// The end of the block this field's text is in: past markup closing on its
/// last line, without the spaces and line breaks after it.
fn block_end(text: &str, field: &Editable) -> usize {
    let high = field.high().min(text.len());
    let rest = &text[high..];
    let line = rest.find('\n').map_or(rest, |end| &rest[..end]);
    high + line.trim_end().len()
}

/// Where HTML tags start, among those right before `end`.
fn before_tags(text: &str, end: usize) -> usize {
    let mut end = end;
    while let Some(head) = text[..end].strip_suffix('>')
        && let Some(open) = head.rfind(['<', '\n'])
        && text[open + 1..].starts_with(|c: char| c.is_ascii_alphabetic() || c == '/')
        && text.as_bytes()[open] == b'<'
    {
        end = open;
    }
    end
}

/// The markup at the start of a field's line, that Backspace takes first: the
/// last of it, so that a list in a quote stays in the quote.
fn marker(text: &str, field: &Editable) -> Option<Range<usize>> {
    let low = field.low().min(text.len());
    let line = text[..low].rfind('\n').map_or(0, |at| at + 1);
    // HTML tags are a part of the text's styling, and stay.
    let end = before_tags(text, low).max(line);
    let prefix = &text[line..end];
    let trimmed = prefix.trim_end();
    if trimmed.is_empty() {
        return None;
    }
    let token = if trimmed.ends_with('>') {
        trimmed.len() - 1
    } else if trimmed.ends_with(']') && trimmed.contains('[') {
        // A task's box, which has a space in it.
        trimmed.rfind('[').unwrap_or(0)
    } else {
        trimmed
            .rfind(|c: char| c.is_whitespace() || c == '>')
            .map_or(0, |at| at + 1)
    };
    Some(line + token..end)
}

/// Whether two fields are neighbors with nothing but blank lines and the
/// markers of a line between them, so that joining them loses nothing.
fn can_join(text: &str, first: &Editable, second: &Editable) -> bool {
    if first.kind != FieldKind::Inline || second.kind != FieldKind::Inline {
        return false;
    }
    let (from, to) = (block_end(text, first), second.low());
    if from > to {
        return false;
    }
    let line = text[..to].rfind('\n').map_or(0, |at| at + 1);
    let gap = &text[from..line.max(from)];
    gap.chars().all(|c| c.is_whitespace() || c == '>')
}

/// Backspace at the cursor.
pub fn backspace(text: &str, fields: &[&Editable], cursor: usize) -> Option<Change> {
    let (index, field) = field_at(fields, cursor)?;
    if cursor > field.high() && text[field.high()..cursor].ends_with(' ') {
        let space = cursor - 1..cursor;
        return Some(cut(text, vec![space], cursor));
    }
    // On a blank line that's in no block: that line goes.
    if !field.contains(cursor) {
        let kept = text[..cursor].trim_end().len();
        if text[kept..cursor].contains('\n') {
            let blank = kept..cursor;
            return Some(cut(text, vec![blank], kept));
        }
    }
    let char = field.char_at(cursor);
    if let Some(span) = char.checked_sub(1).and_then(|last| field.spans.get(last)) {
        return Some(cut(text, vec![span.clone()], span.start));
    }
    if field.kind != FieldKind::Inline {
        return None;
    }
    if let Some(marker) = marker(text, field) {
        let at = marker.start;
        let mut change = cut(text, vec![marker], at);
        // What was left of its line goes too, unless a cursor can be there.
        if field.spans.is_empty() && !in_gap(&change.text, change.cursor) {
            let line_is_blank = change.text[..change.cursor]
                .rsplit('\n')
                .next()
                .is_some_and(|line| line.trim().is_empty());
            if line_is_blank {
                let kept = change.text[..change.cursor].trim_end().len();
                change.text.replace_range(kept..change.cursor, "");
                change.cursor = kept;
            }
        }
        return Some(change);
    }
    let previous = index.checked_sub(1).and_then(|at| fields.get(at));
    if let Some(previous) = previous
        && can_join(text, previous, field)
    {
        let from = block_end(text, previous);
        return Some(Change {
            text: format!(
                "{}{}",
                &text[..from],
                &text[before_tags(text, field.low())..]
            ),
            cursor: from,
        });
    }
    // An empty line with nothing to join: what's below comes up to it.
    if field.spans.is_empty() {
        let rest = &text[cursor..];
        let blank = rest.len() - rest.trim_start().len();
        if blank > 0 && blank < rest.len() {
            let mut out = text.to_owned();
            out.replace_range(cursor..cursor + blank, "");
            return Some(Change { text: out, cursor });
        }
    }
    None
}

/// Delete at the cursor.
pub fn delete_forward(text: &str, fields: &[&Editable], cursor: usize) -> Option<Change> {
    let (index, field) = field_at(fields, cursor)?;
    if let Some(span) = field.spans.get(field.char_at(cursor)) {
        return Some(cut(text, vec![span.clone()], cursor.min(span.start)));
    }
    let next = fields.get(index + 1)?;
    if !can_join(text, field, next) {
        return None;
    }
    let from = block_end(text, field);
    Some(Change {
        text: format!("{}{}", &text[..from], &text[next.low()..]),
        cursor: cursor.max(field.high()).min(from),
    })
}

/// Enter at the cursor, once the lists have had their say: a new paragraph, or
/// a new line in a code block. Nothing in a table's cell.
pub fn enter(text: &str, fields: &[&Editable], selection: Range<usize>) -> Option<Change> {
    let kind = field_at(fields, selection.start).map(|(_, field)| field.kind);
    if kind == Some(FieldKind::Cell) {
        return None;
    }
    let line = text[..selection.start].rfind('\n').map_or(0, |at| at + 1);
    let prefix: String = text[line..selection.start]
        .chars()
        .take_while(|c| c.is_whitespace() || *c == '>')
        .collect();
    if kind == Some(FieldKind::Code) {
        return Some(insert(text, selection, &format!("\n{prefix}"), kind));
    }
    // At the very start of a block, it moves down whole.
    if let Some((_, field)) = field_at(fields, selection.start)
        && selection.is_empty()
        && selection.start <= field.low()
        && !field.spans.is_empty()
    {
        let mut out = text.to_owned();
        out.insert_str(line, "\n\n");
        return Some(Change {
            text: out,
            cursor: field.low() + 2,
        });
    }
    let blank = prefix.trim_end();
    Some(insert(
        text,
        selection,
        &format!("\n{blank}\n{prefix}"),
        kind,
    ))
}

/// Shift+Enter: a line break within the paragraph.
pub fn line_break(text: &str, fields: &[&Editable], selection: Range<usize>) -> Option<Change> {
    let kind = field_at(fields, selection.start).map(|(_, field)| field.kind);
    if kind != Some(FieldKind::Inline) {
        return enter(text, fields, selection);
    }
    let line = text[..selection.start].rfind('\n').map_or(0, |at| at + 1);
    let prefix: String = text[line..selection.start]
        .chars()
        .take_while(|c| c.is_whitespace() || *c == '>')
        .collect();
    Some(insert(text, selection, &format!("  \n{prefix}"), kind))
}

/// The start of the next or previous cell.
pub fn next_cell(fields: &[&Editable], cursor: usize, back: bool) -> Option<usize> {
    let (index, field) = field_at(fields, cursor)?;
    if field.kind != FieldKind::Cell {
        return None;
    }
    let neighbor = if back {
        index.checked_sub(1).and_then(|at| fields.get(at))
    } else {
        fields.get(index + 1)
    }?;
    (neighbor.kind == FieldKind::Cell).then(|| neighbor.low())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn align(rendered: &str, source: &str) -> Vec<String> {
        let mut cursor = 0;
        spans(rendered, source, 0..source.len(), &mut cursor)
            .into_iter()
            .map(|span| source[span].to_owned())
            .collect()
    }

    #[test]
    fn each_char_is_found_past_what_is_hidden() {
        assert_eq!(align("ab", "**a**b"), ["a", "b"]);
        assert_eq!(align("a*b", r"a\*b"), ["a", r"\*", "b"]);
        assert_eq!(align("a&b", "a&amp;b"), ["a", "&amp;", "b"]);
        assert_eq!(align("<a", "&lt;a"), ["&lt;", "a"]);
        assert_eq!(align("a&b;", "a&b;"), ["a", "&", "b", ";"]);
        assert_eq!(align("日本", "日本"), ["日", "本"]);
    }

    #[test]
    fn line_breaks_take_what_starts_the_next_line() {
        assert_eq!(align("a b", "a\n> b"), ["a", "\n> ", "b"]);
        assert_eq!(align("a b", "a\r\nb"), ["a", "\r\n", "b"]);
        assert_eq!(align("a\nb", "a  \nb"), ["a", "  \n", "b"]);
        assert_eq!(align("a\nb", "a\\\nb"), ["a", "\\\n", "b"]);
    }

    #[test]
    fn what_is_not_in_the_source_gets_all_between_its_neighbors() {
        assert_eq!(align("a🚀b", "a:rocket:b"), ["a", ":rocket:", "b"]);
        assert_eq!(align("©b", "&copy;b"), ["&copy;", "b"]);
    }

    #[test]
    fn a_run_carries_on_from_the_last() {
        let source = "==a== ==a==";
        let mut cursor = 0;
        let first = spans("a", source, 0..source.len(), &mut cursor);
        let second = spans("a", source, 0..source.len(), &mut cursor);
        assert_eq!(first[0], 2..3);
        assert_eq!(second[0], 8..9);
    }

    fn field(text: &str, needle: &str, kind: FieldKind) -> Editable {
        let start = text.find(needle).unwrap();
        let spans = needle
            .char_indices()
            .map(|(at, c)| start + at..start + at + c.len_utf8())
            .collect();
        Editable::new(spans, start, kind, text)
    }

    #[test]
    fn the_cursor_maps_both_ways_past_hidden_markup() {
        let text = "**ab**";
        let f = field(text, "ab", FieldKind::Inline);
        assert_eq!(f.char_at(2), 0);
        assert_eq!(f.char_at(3), 1);
        assert_eq!(f.char_at(4), 2);
        assert_eq!(f.char_at(6), 2, "after the closing marks");
        assert_eq!(f.offset_at(0), 2);
        assert_eq!(f.offset_at(2), 4, "inside the marks: typing stays bold");
    }

    #[test]
    fn backspace_takes_what_shows_and_any_markup_left_empty() {
        let text = "x **a** y";
        let f = field(text, "x ", FieldKind::Inline);
        let mut spans = f.spans;
        let a = field(text, "a", FieldKind::Inline).spans;
        let y = field(text, " y", FieldKind::Inline).spans;
        spans.extend(a);
        spans.extend(y);
        let whole = Editable::new(spans, 0, FieldKind::Inline, text);
        let fields = [&whole];
        // Just after the a, inside the marks.
        let change = backspace(text, &fields, 5).unwrap();
        assert_eq!(change.text, "x  y");
        assert_eq!(change.cursor, 2);
        // After the closing marks, the same.
        let change = backspace(text, &fields, 7).unwrap();
        assert_eq!(change.text, "x  y");
        assert_eq!(change.cursor, 2);
    }

    #[test]
    fn backspace_at_the_start_takes_the_marker_then_joins() {
        let text = "one\n\n## two\n";
        let one = field(text, "one", FieldKind::Inline);
        let two = field(text, "two", FieldKind::Inline);
        let fields = [&one, &two];
        let at = two.low();
        let change = backspace(text, &fields, at).unwrap();
        assert_eq!(change.text, "one\n\ntwo\n");
        assert_eq!(change.cursor, 5);

        let text = "one\n\ntwo\n";
        let one = field(text, "one", FieldKind::Inline);
        let two = field(text, "two", FieldKind::Inline);
        let fields = [&one, &two];
        let change = backspace(text, &fields, two.low()).unwrap();
        assert_eq!(change.text, "onetwo\n");
        assert_eq!(change.cursor, 3);
    }

    #[test]
    fn markers_go_one_at_a_time() {
        let text = "> - two\n";
        let two = field(text, "two", FieldKind::Inline);
        let change = backspace(text, &[&two], two.low()).unwrap();
        assert_eq!(change.text, "> two\n");
        let text = "> two\n";
        let two = field(text, "two", FieldKind::Inline);
        let change = backspace(text, &[&two], two.low()).unwrap();
        assert_eq!(change.text, "two\n");
        assert_eq!(change.cursor, 0);
        let text = ">> two\n";
        let two = field(text, "two", FieldKind::Inline);
        let change = backspace(text, &[&two], two.low()).unwrap();
        assert_eq!(change.text, ">two\n", "one level of a quote");
    }

    #[test]
    fn what_is_between_text_is_not_lost_by_joining() {
        let text = "one\n\n---\n\ntwo\n";
        let one = field(text, "one", FieldKind::Inline);
        let two = field(text, "two", FieldKind::Inline);
        let fields = [&one, &two];
        assert_eq!(backspace(text, &fields, two.low()), None);
        assert_eq!(delete_forward(text, &fields, one.high()), None);
    }

    #[test]
    fn code_and_cells_do_not_join() {
        let text = "one\n\n```\ntwo\n```\n";
        let one = field(text, "one", FieldKind::Inline);
        let two = field(text, "two", FieldKind::Code);
        assert_eq!(backspace(text, &[&one, &two], two.low()), None);
        assert_eq!(delete_forward(text, &[&one, &two], one.high()), None);
    }

    #[test]
    fn delete_takes_the_next_char_or_joins_the_next_block() {
        let text = "one\n\n## two\n";
        let one = field(text, "one", FieldKind::Inline);
        let two = field(text, "two", FieldKind::Inline);
        let fields = [&one, &two];
        let change = delete_forward(text, &fields, 1).unwrap();
        assert_eq!(change.text, "oe\n\n## two\n");
        assert_eq!(change.cursor, 1);
        let change = delete_forward(text, &fields, 3).unwrap();
        assert_eq!(change.text, "onetwo\n");
        assert_eq!(change.cursor, 3);
    }

    #[test]
    fn deleting_a_selection_keeps_the_markup_around_it() {
        let text = "**bold** text";
        let bold = field(text, "bold", FieldKind::Inline);
        let tail = field(text, " text", FieldKind::Inline);
        let mut spans = bold.spans;
        spans.extend(tail.spans);
        let whole = Editable::new(spans, 0, FieldKind::Inline, text);
        // "ld te"
        let change = delete_selection(text, &[&whole], 4..11).unwrap();
        assert_eq!(change.text, "**bo**xt");
        assert_eq!(change.cursor, 4);
    }

    #[test]
    fn markup_left_empty_goes_wherever_the_selection_started() {
        let text = "a *it* text b";
        let a = field(text, "a ", FieldKind::Inline);
        let it = field(text, "it", FieldKind::Inline);
        let rest = field(text, " text b", FieldKind::Inline);
        let mut spans = a.spans;
        spans.extend(it.spans);
        spans.extend(rest.spans);
        let whole = Editable::new(spans, 0, FieldKind::Inline, text);
        // From just before the opening mark, as a click puts it, to the end of "text".
        let change = delete_selection(text, &[&whole], 2..text.find(" b").unwrap()).unwrap();
        assert_eq!(change.text, "a  b");
        assert_eq!(change.cursor, 2);
    }

    #[test]
    fn a_selection_over_blocks_joins_them() {
        let text = "**one**\n\n## two\n";
        let one = field(text, "one", FieldKind::Inline);
        let two = field(text, "two", FieldKind::Inline);
        let change = delete_selection(text, &[&one, &two], 3..text.find("wo").unwrap()).unwrap();
        assert_eq!(change.text, "**o**wo\n");
        assert_eq!(change.cursor, 3);
    }

    #[test]
    fn enter_makes_a_paragraph_keeping_a_quote() {
        let text = "abcd";
        let f = field(text, "abcd", FieldKind::Inline);
        let change = enter(text, &[&f], 2..2).unwrap();
        assert_eq!(change.text, "ab\n\ncd");
        assert_eq!(change.cursor, 4);

        let text = "> abcd";
        let f = field(text, "abcd", FieldKind::Inline);
        let change = enter(text, &[&f], 4..4).unwrap();
        assert_eq!(change.text, "> ab\n>\n> cd");
        assert_eq!(change.cursor, 9);
    }

    #[test]
    fn enter_at_a_blocks_start_moves_it_down() {
        let text = "## ab";
        let f = field(text, "ab", FieldKind::Inline);
        let change = enter(text, &[&f], 3..3).unwrap();
        assert_eq!(change.text, "\n\n## ab");
        assert_eq!(change.cursor, 5);
    }

    #[test]
    fn enter_in_code_is_a_line_and_in_a_cell_is_nothing() {
        let text = "```\nab\n```\n";
        let code = field(text, "ab", FieldKind::Code);
        let change = enter(text, &[&code], 5..5).unwrap();
        assert_eq!(change.text, "```\na\nb\n```\n");
        let text = "| a |";
        let cell = field(text, "a", FieldKind::Cell);
        assert_eq!(enter(text, &[&cell], 2..2), None);
    }

    #[test]
    fn a_pipe_in_a_cell_is_escaped() {
        let change = insert("| a |", 3..3, "|", Some(FieldKind::Cell));
        assert_eq!(change.text, "| a\\| |");
        assert_eq!(change.cursor, 5);
        let change = insert("a", 1..1, "|", Some(FieldKind::Inline));
        assert_eq!(change.text, "a|");
    }

    #[test]
    fn a_line_break_keeps_the_quote() {
        let text = "> ab";
        let f = field(text, "ab", FieldKind::Inline);
        let change = line_break(text, &[&f], 3..3).unwrap();
        assert_eq!(change.text, "> a  \n> b");
        assert_eq!(change.cursor, 8);
    }

    #[test]
    fn spaces_after_the_text_are_taken_back_one_at_a_time() {
        let text = "ab  ";
        let f = field(text, "ab", FieldKind::Inline);
        let change = backspace(text, &[&f], 4).unwrap();
        assert_eq!(change.text, "ab ");
        assert_eq!(change.cursor, 3);
    }

    /// Past the last char, but on its line: this field's, not the next one's.
    #[test]
    fn a_space_after_the_text_is_in_that_block_and_not_the_next() {
        let text = "# A \n\n> next\n";
        let head = field(text, "A", FieldKind::Inline);
        let next = field(text, "next", FieldKind::Inline);
        let fields = [&head, &next];
        let (index, _) = field_at(&fields, 4).unwrap();
        assert_eq!(index, 0);
        let change = backspace(text, &fields, 4).unwrap();
        assert_eq!(change.text, "# A\n\n> next\n");
        assert_eq!(change.cursor, 3);
    }

    #[test]
    fn html_tags_left_empty_go() {
        let text = "a <kbd>x</kbd> c";
        let field = field(text, "x", FieldKind::Inline);
        let change = backspace(text, &[&field], 8).expect("a change");
        assert_eq!((change.text.as_str(), change.cursor), ("a  c", 2));
    }

    #[test]
    fn backspace_at_the_start_keeps_html_tags() {
        let text = "z\n\n<kbd>y</kbd>";
        let (first, second) = (
            field(text, "z", FieldKind::Inline),
            field(text, "y", FieldKind::Inline),
        );
        let change = backspace(text, &[&first, &second], 8).expect("a change");
        assert_eq!(change.text, "z<kbd>y</kbd>");

        let text = "> <kbd>y</kbd>";
        let only = field(text, "y", FieldKind::Inline);
        let change = backspace(text, &[&only], 7).expect("a change");
        assert_eq!(change.text, "<kbd>y</kbd>");
    }

    #[test]
    fn links_left_empty_go_with_their_address() {
        let text = "[a](http://x) b";
        let f = field(text, "a", FieldKind::Inline);
        let change = backspace(text, &[&f], 2).unwrap();
        assert_eq!(change.text, " b");
        assert_eq!(change.cursor, 0);
    }

    #[test]
    fn empty_lines_among_blank_ones_are_gaps() {
        assert!(in_gap("", 0));
        assert!(in_gap("one\n\n", 5));
        assert!(in_gap("one\n\n\n\ntwo", 5));
        assert!(!in_gap("one\n\ntwo", 5), "at the start of a block");
        assert!(!in_gap("one\n", 4), "right under its text");
        assert!(!in_gap("one", 3));
    }

    #[test]
    fn tabs_go_between_cells() {
        let text = "| a | b |";
        let a = field(text, "a", FieldKind::Cell);
        let b = field(text, "b", FieldKind::Cell);
        let fields = [&a, &b];
        assert_eq!(next_cell(&fields, a.low(), false), Some(b.low()));
        assert_eq!(next_cell(&fields, b.low(), true), Some(a.low()));
        assert_eq!(next_cell(&fields, b.low(), false), None);
    }
}
