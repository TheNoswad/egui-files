//! Formatting commands: what the toolbar and shortcuts do to markdown source.
//! Each is a change to the text around the selection, and nothing else moves.
//! Positions are in chars, like the editor's.

use std::ops::Range;

use super::table::{self, in_code_fence};
use crate::editor::ops::{Edit, char_to_byte};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Inline {
    Bold,
    Italic,
    Strike,
    Code,
    Highlight,
}

impl Inline {
    fn mark(self) -> &'static str {
        match self {
            Self::Bold => "**",
            Self::Italic => "*",
            Self::Strike => "~~",
            Self::Code => "`",
            Self::Highlight => "==",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    Bullet,
    Numbered,
    Task,
    Quote,
}

fn chars(text: &str, bytes: usize) -> usize {
    text[..bytes].chars().count()
}

fn byte_range(text: &str, selection: &Range<usize>) -> Range<usize> {
    let start = char_to_byte(text, selection.start);
    start..start + char_to_byte(&text[start..], selection.end - selection.start)
}

/// Replaces `bytes` and selects `select`, in bytes of the new text.
fn splice(text: &str, bytes: Range<usize>, with: &str, select: Range<usize>) -> Edit {
    let mut new = text.to_owned();
    new.replace_range(bytes, with);
    Edit {
        selection: chars(&new, select.start)..chars(&new, select.end),
        text: new,
    }
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The word around a position, in bytes, if it's in or beside one.
fn word_at(text: &str, byte: usize) -> Option<Range<usize>> {
    let start = text[..byte].rfind(|c: char| !is_word(c)).map_or(0, |i| {
        i + text[i..].chars().next().map_or(1, char::len_utf8)
    });
    let end = text[byte..]
        .find(|c: char| !is_word(c))
        .map_or(text.len(), |i| byte + i);
    (start < end).then_some(start..end)
}

/// Whether `mark` is around this text: `*` in `**bold**` isn't italic, as the
/// asterisks there are bold's, but all of them in `***both***` count.
fn marked_by(before: &str, after: &str, inline: Inline) -> bool {
    let mark = inline.mark();
    if !matches!(inline, Inline::Bold | Inline::Italic) {
        return before.ends_with(mark) && after.starts_with(mark);
    }
    let left = before.chars().rev().take_while(|c| *c == '*').count();
    let right = after.chars().take_while(|c| *c == '*').count();
    match inline {
        Inline::Bold => left.min(right) >= 2,
        _ => matches!(left.min(right), 1 | 3),
    }
}

/// Bolds or unbolds the selection, or whichever style it is. With nothing
/// selected: the word at the cursor, or else a pair of marks to type in.
pub fn toggle_inline(text: &str, selection: Range<usize>, inline: Inline) -> Edit {
    let mark = inline.mark();
    let mut bytes = byte_range(text, &selection);
    if text[bytes.clone()].contains('\n') {
        return if inline == Inline::Code {
            code_block(text, selection)
        } else {
            toggle_each_line(text, bytes, inline)
        };
    }
    if bytes.is_empty() {
        // Between a pair with nothing in it: take them away again.
        if marked_by(&text[..bytes.start], &text[bytes.start..], inline)
            && let Some(start) = bytes.start.checked_sub(mark.len())
        {
            let at = start..start;
            return splice(text, start..bytes.end + mark.len(), "", at);
        }
        match word_at(text, bytes.start) {
            Some(word) => bytes = word,
            None => {
                let with = format!("{mark}{mark}");
                let at = bytes.start + mark.len();
                return splice(text, bytes, &with, at..at);
            }
        }
    }
    // `**word **` isn't bold in markdown: the space stays out.
    let selected = &text[bytes.clone()];
    let start = bytes.start + (selected.len() - selected.trim_start().len());
    let end = (bytes.end - (selected.len() - selected.trim_end().len())).max(start);
    let inner = &text[start..end];
    if inner.is_empty() {
        let with = format!("{mark}{mark}");
        let at = start + mark.len();
        return splice(text, start..end, &with, at..at);
    }
    if marked_by(&text[..start], &text[end..], inline) {
        let to = start - mark.len();
        return splice(text, to..end + mark.len(), inner, to..to + inner.len());
    }
    // The marks may have been selected too.
    if let Some(bare) = without_marks(inner, inline) {
        return splice(text, start..end, bare, start..start + bare.len());
    }
    let with = format!("{mark}{inner}{mark}");
    let selected = start + mark.len()..start + mark.len() + inner.len();
    splice(text, start..end, &with, selected)
}

/// The text inside this style's marks, if they're the first and last of it.
fn without_marks(text: &str, inline: Inline) -> Option<&str> {
    let mark = inline.mark();
    let bare = text.strip_prefix(mark)?.strip_suffix(mark)?;
    // Italic's asterisk isn't half of bold's.
    let opened = text.chars().take_while(|c| *c == '*').count();
    (!(inline == Inline::Italic && opened == 2)).then_some(bare)
}

/// A style can't span a blank line, so several lines get it line by line.
fn toggle_each_line(text: &str, bytes: Range<usize>, inline: Inline) -> Edit {
    let lines = line_bytes(text, bytes);
    let mark = inline.mark();
    let block = &text[lines.clone()];
    let filled = || block.split('\n').filter(|line| !line.trim().is_empty());
    let all_marked = filled().all(|line| without_marks(line.trim(), inline).is_some());
    let with: Vec<String> = block
        .split('\n')
        .map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                line.to_owned()
            } else if all_marked && let Some(bare) = without_marks(trimmed, inline) {
                bare.to_owned()
            } else {
                format!("{mark}{trimmed}{mark}")
            }
        })
        .collect();
    let with = with.join("\n");
    let select = lines.start..lines.start + with.len();
    splice(text, lines, &with, select)
}

/// From the start of the first line of `bytes` to the end of its last.
fn line_bytes(text: &str, bytes: Range<usize>) -> Range<usize> {
    let start = text[..bytes.start].rfind('\n').map_or(0, |i| i + 1);
    let end = text[bytes.end..]
        .find('\n')
        .map_or(text.len(), |i| bytes.end + i);
    start..end
}

/// Changes each line of the selection, keeping the cursor on its text if
/// nothing's selected, and otherwise selecting the lines.
fn change_lines(
    text: &str,
    selection: &Range<usize>,
    change: impl Fn(&str, usize) -> String,
) -> Edit {
    let bytes = byte_range(text, selection);
    let lines = line_bytes(text, bytes.clone());
    let block = &text[lines.clone()];
    let with: Vec<String> = block
        .split('\n')
        .enumerate()
        .map(|(index, line)| change(line, index))
        .collect();
    let with = with.join("\n");
    let mut new = text.to_owned();
    new.replace_range(lines.clone(), &with);
    if bytes.is_empty() {
        // It keeps its distance from the end of the line.
        let from_end = lines.end - bytes.start;
        let at = (lines.start + with.len())
            .saturating_sub(from_end)
            .max(lines.start);
        let char = chars(&new, at);
        Edit {
            text: new,
            selection: char..char,
        }
    } else {
        let start = chars(&new, lines.start);
        let end = chars(&new, lines.start + with.len());
        Edit {
            text: new,
            selection: start..end,
        }
    }
}

/// `# ` of this level on the selected lines, or none. The same level again
/// takes it off.
pub fn set_heading(text: &str, selection: Range<usize>, level: u8) -> Edit {
    let wanted = "#".repeat(usize::from(level.min(6)));
    change_lines(text, &selection, |line, _| {
        let (indent, rest) = split_indent(line);
        let hashes = rest.chars().take_while(|c| *c == '#').count();
        let is_heading = (1..=6).contains(&hashes)
            && rest[hashes..]
                .chars()
                .next()
                .is_none_or(char::is_whitespace);
        let (current, content) = if is_heading {
            (&rest[..hashes], rest[hashes..].trim_start())
        } else {
            ("", rest)
        };
        if content.is_empty() && level > 0 && !is_heading {
            return format!("{indent}{wanted} ");
        }
        if current == wanted || level == 0 {
            format!("{indent}{content}")
        } else {
            format!("{indent}{wanted} {content}")
        }
    })
}

fn split_indent(line: &str) -> (&str, &str) {
    let rest = line.trim_start_matches([' ', '\t']);
    (&line[..line.len() - rest.len()], rest)
}

/// What starts a list item or quote on a line.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Prefix<'a> {
    indent: &'a str,
    kind: PrefixKind<'a>,
    /// All of it, spaces and all, up to the line's text.
    len: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PrefixKind<'a> {
    Bullet(char),
    Number { value: u64, close: char },
    Task(&'a str),
    Quote,
}

fn prefix(line: &str) -> Option<Prefix<'_>> {
    let (indent, rest) = split_indent(line);
    if let Some(after) = rest.strip_prefix('>') {
        let space = usize::from(after.starts_with(' '));
        return Some(Prefix {
            indent,
            kind: PrefixKind::Quote,
            len: indent.len() + 1 + space,
        });
    }
    let (kind, marker_len) = match rest.chars().next()? {
        c @ ('-' | '*' | '+') => (PrefixKind::Bullet(c), 1),
        c if c.is_ascii_digit() => {
            let digits = rest.chars().take_while(char::is_ascii_digit).count();
            let close = rest[digits..]
                .chars()
                .next()
                .filter(|c| matches!(c, '.' | ')'))?;
            let value = rest[..digits].parse().ok()?;
            (PrefixKind::Number { value, close }, digits + 1)
        }
        _ => return None,
    };
    let after = &rest[marker_len..];
    // A marker needs a space after it, or the end of the line.
    if !(after.is_empty() || after.starts_with(' ')) {
        return None;
    }
    // `---` and `* * *` are rules.
    let is_rule = matches!(kind, PrefixKind::Bullet(_))
        && rest.chars().filter(|c| !c.is_whitespace()).count() >= 3
        && rest
            .chars()
            .all(|c| c.is_whitespace() || rest.starts_with(c));
    if is_rule {
        return None;
    }
    let spaces = after.len() - after.trim_start_matches(' ').len();
    let text_at = marker_len + spaces.min(4);
    let task = rest
        .get(text_at..text_at + 3)
        .filter(|mark| matches!(*mark, "[ ]" | "[x]" | "[X]"));
    if let Some(mark) = task
        && matches!(kind, PrefixKind::Bullet(_) | PrefixKind::Number { .. })
    {
        let after_mark = &rest[text_at + 3..];
        if after_mark.is_empty() || after_mark.starts_with(' ') {
            let space = usize::from(after_mark.starts_with(' '));
            // A numbered task continues as a numbered one: see `continue_list`.
            let kind = match kind {
                PrefixKind::Number { .. } => kind,
                _ => PrefixKind::Task(mark),
            };
            return Some(Prefix {
                indent,
                kind,
                len: indent.len() + text_at + 3 + space,
            });
        }
    }
    Some(Prefix {
        indent,
        kind,
        len: indent.len() + text_at,
    })
}

/// Makes the selected lines bullets, numbers, tasks or quotes, or takes that
/// away if they all are already.
pub fn toggle_lines(text: &str, selection: Range<usize>, kind: LineKind) -> Edit {
    let bytes = byte_range(text, &selection);
    let lines = line_bytes(text, bytes);
    let is_kind = |line: &str| match prefix(line).map(|p| p.kind) {
        Some(PrefixKind::Bullet(_)) => kind == LineKind::Bullet,
        Some(PrefixKind::Number { .. }) => kind == LineKind::Numbered,
        Some(PrefixKind::Task(_)) => kind == LineKind::Task,
        Some(PrefixKind::Quote) => kind == LineKind::Quote,
        None => false,
    };
    let filled = |line: &&str| !line.trim().is_empty();
    let block = &text[lines];
    let remove = block.split('\n').filter(filled).count() > 0
        && block.split('\n').filter(filled).all(is_kind);
    change_lines(text, &selection, |line, index| {
        if line.trim().is_empty() && block.contains('\n') {
            return line.to_owned();
        }
        let found = prefix(line);
        let (indent, content) = match &found {
            // A quote stays around whatever else is in it, so a list can be
            // quoted, but one kind of list replaces another.
            Some(p) if kind != LineKind::Quote || p.kind == PrefixKind::Quote => {
                (p.indent, &line[p.len..])
            }
            _ => {
                let (indent, rest) = split_indent(line);
                (indent, rest)
            }
        };
        if remove {
            return format!("{indent}{content}");
        }
        match kind {
            LineKind::Bullet => format!("{indent}- {content}"),
            LineKind::Numbered => format!("{indent}{}. {content}", index + 1),
            LineKind::Task => format!("{indent}- [ ] {content}"),
            LineKind::Quote => format!("{indent}> {content}"),
        }
    })
}

/// Fences the selected lines, or adds an empty block to type in.
pub fn code_block(text: &str, selection: Range<usize>) -> Edit {
    let bytes = byte_range(text, &selection);
    let lines = if bytes.is_empty() && text[line_bytes(text, bytes.clone())].trim().is_empty() {
        bytes.clone()
    } else {
        line_bytes(text, bytes)
    };
    let inner = &text[lines.clone()];
    // A fence must be longer than any in the text it holds.
    let longest = inner
        .lines()
        .map(|line| line.trim_start().chars().take_while(|c| *c == '`').count())
        .max()
        .unwrap_or(0);
    let fence = "`".repeat(longest.max(2) + 1);
    let before = needs_line_break(text, lines.start);
    let after = !text[lines.end..].starts_with('\n') && lines.end < text.len();
    let with = format!(
        "{}{fence}\n{inner}{}{fence}{}",
        if before { "\n" } else { "" },
        if inner.is_empty() || inner.ends_with('\n') {
            ""
        } else {
            "\n"
        },
        if after { "\n" } else { "" },
    );
    let start = lines.start + usize::from(before) + fence.len() + 1;
    if inner.is_empty() {
        // Between the fences; on a line of its own to type on.
        let with = with.replacen(&format!("{fence}\n"), &format!("{fence}\n\n"), 1);
        return splice(text, lines, &with, start..start);
    }
    splice(text, lines, &with, start..start + inner.len())
}

/// Whether a block put at this byte would start in the middle of a line.
fn needs_line_break(text: &str, byte: usize) -> bool {
    byte > 0 && !text[..byte].ends_with('\n')
}

/// Puts a block on lines of its own, at the cursor.
pub fn insert_block(text: &str, selection: Range<usize>, block: &str) -> Edit {
    let bytes = byte_range(text, &selection);
    let before = if bytes.start == 0 || text[..bytes.start].ends_with("\n\n") {
        ""
    } else if text[..bytes.start].ends_with('\n') {
        "\n"
    } else {
        "\n\n"
    };
    // The block ends its own line: what follows needs one blank line between.
    let rest = &text[bytes.end..];
    let after = if rest.is_empty() || rest.starts_with('\n') {
        ""
    } else {
        "\n"
    };
    // A blank line is there already: the block's own line break would be a second.
    let block = if rest.starts_with("\n\n") {
        block.strip_suffix('\n').unwrap_or(block)
    } else {
        block
    };
    let with = format!("{before}{block}{after}");
    let at = bytes.start + before.len() + block.len();
    splice(text, bytes, &with, at..at)
}

pub fn rule(text: &str, selection: Range<usize>) -> Edit {
    insert_block(text, selection, "---\n")
}

pub fn insert_table(text: &str, selection: Range<usize>) -> Edit {
    let edit = insert_block(text, selection.clone(), table::template());
    // On the first heading, to name it.
    let bytes = byte_range(text, &selection);
    let start = edit.text[bytes.start..]
        .find('|')
        .map_or(0, |i| bytes.start + i)
        + 2;
    let end = start + "Column".len();
    Edit {
        selection: chars(&edit.text, start)..chars(&edit.text, end),
        text: edit.text,
    }
}

/// `[selection](url)`, with the part still to fill in selected.
pub fn link(text: &str, selection: Range<usize>) -> Edit {
    let bytes = byte_range(text, &selection);
    let bytes = if bytes.is_empty() {
        word_at(text, bytes.start).unwrap_or(bytes)
    } else {
        bytes
    };
    let selected = &text[bytes.clone()];
    if selected.is_empty() {
        let at = bytes.start + 1;
        return splice(text, bytes, "[text](url)", at..at + 4);
    }
    let with = format!("[{selected}](url)");
    let at = bytes.start + selected.len() + 3;
    splice(text, bytes, &with, at..at + 3)
}

pub fn image(text: &str, selection: Range<usize>) -> Edit {
    let bytes = byte_range(text, &selection);
    let selected = &text[bytes.clone()];
    let with = format!("![{selected}](path)");
    let at = bytes.start + selected.len() + 4;
    splice(text, bytes, &with, at..at + 4)
}

/// Pasting an address over selected text links the text to it.
pub fn paste_url(text: &str, selection: Range<usize>, pasted: &str) -> Option<Edit> {
    let bytes = byte_range(text, &selection);
    let selected = &text[bytes.clone()];
    let pasted = pasted.trim();
    let is_url = ["http://", "https://", "mailto:"]
        .iter()
        .any(|scheme| pasted.starts_with(scheme))
        && !pasted.contains(char::is_whitespace);
    if !is_url
        || selected.is_empty()
        || selected.contains('\n')
        || selected.contains("://")
        || in_code_fence(text, bytes.start)
    {
        return None;
    }
    let with = format!("[{selected}]({pasted})");
    let at = bytes.start + with.len();
    Some(splice(text, bytes, &with, at..at))
}

/// Enter in a list or quote: the next line continues it. In an empty item, it
/// ends it instead. `None` when Enter does nothing special here.
pub fn continue_list(text: &str, selection: Range<usize>) -> Option<Edit> {
    let bytes = byte_range(text, &selection);
    if in_code_fence(text, bytes.start) {
        return None;
    }
    let line_start = text[..bytes.start].rfind('\n').map_or(0, |i| i + 1);
    let line_end = text[bytes.end..]
        .find('\n')
        .map_or(text.len(), |i| bytes.end + i);
    let line = &text[line_start..line_end];
    let found = prefix(line)?;
    // Before the marker there's nothing to continue.
    if bytes.start < line_start + found.len {
        return None;
    }
    let rest = text[line_start + found.len..bytes.start].trim().is_empty()
        && text[bytes.end..line_end].trim().is_empty();
    if rest {
        // An empty item ends the list; the line stays, empty.
        return Some(splice(
            text,
            line_start..line_end,
            "",
            line_start..line_start,
        ));
    }
    let next = match found.kind {
        PrefixKind::Bullet(mark) => format!("{}{mark} ", found.indent),
        PrefixKind::Number { value, close } => {
            let is_task = line[found.indent.len()..found.len].ends_with([']', ' '])
                && line[..found.len].contains('[');
            let task = if is_task { "[ ] " } else { "" };
            format!("{}{}{close} {task}", found.indent, value + 1)
        }
        PrefixKind::Task(_) => {
            let marker = &line[found.indent.len()..];
            let mark = marker.chars().next().unwrap_or('-');
            format!("{}{mark} [ ] ", found.indent)
        }
        PrefixKind::Quote => format!("{}> ", found.indent),
    };
    let with = format!("\n{next}");
    let at = bytes.start + with.len();
    Some(splice(text, bytes, &with, at..at))
}

/// Tab on a list item: moves the whole line, by as much as nests it under the
/// item before. `None` if the cursor isn't on one.
pub fn indent_item(text: &str, selection: Range<usize>, outdent: bool) -> Option<Edit> {
    let bytes = byte_range(text, &selection);
    if text[bytes.clone()].contains('\n') || in_code_fence(text, bytes.start) {
        return None;
    }
    let line_start = text[..bytes.start].rfind('\n').map_or(0, |i| i + 1);
    let line_end = text[bytes.end..]
        .find('\n')
        .map_or(text.len(), |i| bytes.end + i);
    let line = &text[line_start..line_end];
    let found = prefix(line)?;
    if found.kind == PrefixKind::Quote {
        return None;
    }
    // The item before says how far in its text starts.
    let before = text[..line_start].trim_end_matches('\n');
    let previous = before.rsplit('\n').next().unwrap_or("");
    let width = prefix(previous)
        .filter(|p| p.kind != PrefixKind::Quote)
        .map_or(2, |p| {
            let marker = p.len - p.indent.len();
            // A task's box isn't part of what nesting lines up with.
            if matches!(p.kind, PrefixKind::Task(_)) {
                2
            } else {
                marker
            }
        });
    let (indent, rest) = split_indent(line);
    let new_line = if outdent {
        let keep = indent.len().saturating_sub(width);
        format!("{}{rest}", &indent[..keep])
    } else {
        format!("{}{indent}{rest}", " ".repeat(width))
    };
    let moved = new_line.len() as isize - line.len() as isize;
    let shift = |at: usize| (at as isize + moved).max(line_start as isize) as usize;
    let select = shift(bytes.start)..shift(bytes.end);
    Some(splice(text, line_start..line_end, &new_line, select))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `|` marks a cursor, or two mark a selection.
    fn marked(text: &str) -> (String, Range<usize>) {
        let start = text[..text.find('|').unwrap()].chars().count();
        let plain: String = text.chars().filter(|c| *c != '|').collect();
        let end = text.rfind('|').filter(|at| *at != text.find('|').unwrap());
        let end = end.map_or(start, |at| text[..at].chars().count() - 1);
        (plain, start..end)
    }

    /// Shows the result the same way.
    fn shown(edit: &Edit) -> String {
        let mut out = String::new();
        for (index, c) in edit.text.chars().enumerate() {
            if index == edit.selection.start || index == edit.selection.end {
                out.push('|');
            }
            out.push(c);
        }
        let count = edit.text.chars().count();
        if edit.selection.end == count || edit.selection.start == count {
            out.push('|');
        }
        out
    }

    fn inline(text: &str, style: Inline) -> String {
        let (text, selection) = marked(text);
        shown(&toggle_inline(&text, selection, style))
    }

    #[test]
    fn styles_wrap_the_selection_and_keep_it_selected() {
        assert_eq!(inline("a |word| b", Inline::Bold), "a **|word|** b");
        assert_eq!(inline("a |word| b", Inline::Italic), "a *|word|* b");
        assert_eq!(inline("a |word| b", Inline::Strike), "a ~~|word|~~ b");
        assert_eq!(inline("a |word| b", Inline::Code), "a `|word|` b");
        assert_eq!(inline("a |word| b", Inline::Highlight), "a ==|word|== b");
    }

    #[test]
    fn toggling_again_takes_them_away() {
        for style in [
            Inline::Bold,
            Inline::Italic,
            Inline::Strike,
            Inline::Code,
            Inline::Highlight,
        ] {
            let (text, selection) = marked("a |word| b");
            let on = toggle_inline(&text, selection, style);
            let off = toggle_inline(&on.text, on.selection, style);
            assert_eq!(shown(&off), "a |word| b", "{style:?}");
        }
    }

    #[test]
    fn selected_marks_count_too() {
        assert_eq!(inline("a |**word**| b", Inline::Bold), "a |word| b");
    }

    #[test]
    fn bold_and_italic_tell_their_asterisks_apart() {
        assert_eq!(inline("**|word|**", Inline::Italic), "***|word|***");
        assert_eq!(inline("*|word|*", Inline::Bold), "***|word|***");
        assert_eq!(inline("***|word|***", Inline::Bold), "*|word|*");
        assert_eq!(inline("***|word|***", Inline::Italic), "**|word|**");
        assert_eq!(inline("**|word|**", Inline::Bold), "|word|");
    }

    #[test]
    fn spaces_stay_out_of_the_marks() {
        assert_eq!(inline("a| word |b", Inline::Bold), "a **|word|** b");
        assert_eq!(inline("a|   |b", Inline::Bold), "a   **|**b");
    }

    #[test]
    fn without_a_selection_the_word_at_the_cursor_is_used() {
        assert_eq!(inline("a wo|rd b", Inline::Bold), "a **|word|** b");
        assert_eq!(inline("a **wo|rd** b", Inline::Bold), "a |word| b");
        assert_eq!(inline("é wö|rd", Inline::Bold), "é **|wörd|**");
    }

    #[test]
    fn without_a_word_a_pair_is_inserted_and_can_be_undone() {
        assert_eq!(inline("a | b", Inline::Bold), "a **|** b");
        assert_eq!(inline("a **|** b", Inline::Bold), "a | b");
        assert_eq!(inline("|", Inline::Code), "`|`");
    }

    #[test]
    fn several_lines_are_styled_one_at_a_time() {
        assert_eq!(inline("|one\n\ntwo|", Inline::Bold), "|**one**\n\n**two**|");
        assert_eq!(inline("|**one**\n\n**two**|", Inline::Bold), "|one\n\ntwo|");
    }

    #[test]
    fn code_over_several_lines_is_a_block() {
        assert_eq!(inline("|a\nb|", Inline::Code), "```\n|a\nb|\n```");
    }

    fn heading(text: &str, level: u8) -> String {
        let (text, selection) = marked(text);
        shown(&set_heading(&text, selection, level))
    }

    #[test]
    fn headings_are_set_changed_and_taken_off() {
        assert_eq!(heading("ti|tle", 2), "## ti|tle");
        assert_eq!(heading("# ti|tle", 3), "### ti|tle");
        assert_eq!(heading("## ti|tle", 2), "ti|tle");
        assert_eq!(heading("## ti|tle", 0), "ti|tle");
        assert_eq!(heading("|", 1), "# |");
        assert_eq!(heading("#hash|tag", 1), "# #hash|tag", "not a heading yet");
        assert_eq!(heading("|one\ntwo|", 1), "|# one\n# two|");
    }

    fn lines(text: &str, kind: LineKind) -> String {
        let (text, selection) = marked(text);
        shown(&toggle_lines(&text, selection, kind))
    }

    #[test]
    fn lines_become_lists_and_back() {
        assert_eq!(lines("on|e", LineKind::Bullet), "- on|e");
        assert_eq!(lines("- on|e", LineKind::Bullet), "on|e");
        assert_eq!(lines("on|e", LineKind::Task), "- [ ] on|e");
        assert_eq!(lines("- [x] on|e", LineKind::Task), "on|e");
        assert_eq!(lines("on|e", LineKind::Quote), "> on|e");
        assert_eq!(lines("> on|e", LineKind::Quote), "on|e");
        assert_eq!(lines("on|e", LineKind::Numbered), "1. on|e");
        assert_eq!(lines("7. on|e", LineKind::Numbered), "on|e");
    }

    #[test]
    fn one_kind_of_list_replaces_another() {
        assert_eq!(lines("- on|e", LineKind::Numbered), "1. on|e");
        assert_eq!(lines("1. on|e", LineKind::Bullet), "- on|e");
        assert_eq!(lines("- on|e", LineKind::Task), "- [ ] on|e");
        assert_eq!(lines("- [ ] on|e", LineKind::Bullet), "- on|e");
        assert_eq!(
            lines("- on|e", LineKind::Quote),
            "> - on|e",
            "a quoted list"
        );
    }

    #[test]
    fn numbers_count_up_and_blank_lines_are_left() {
        assert_eq!(
            lines("|one\n\ntwo\nthree|", LineKind::Numbered),
            "|1. one\n\n3. two\n4. three|"
        );
        assert_eq!(lines("|- a\n\n- b|", LineKind::Bullet), "|a\n\nb|");
    }

    #[test]
    fn indents_are_kept() {
        assert_eq!(lines("  on|e", LineKind::Bullet), "  - on|e");
        assert_eq!(lines("  - on|e", LineKind::Bullet), "  on|e");
    }

    #[test]
    fn rules_arent_lists() {
        assert_eq!(prefix("---"), None);
        assert_eq!(prefix("* * *"), None);
        assert_eq!(prefix("-word"), None);
        assert_eq!(prefix("1.word"), None);
        assert!(prefix("-").is_some());
        assert!(prefix("- a").is_some());
    }

    fn enter(text: &str) -> Option<String> {
        let (text, selection) = marked(text);
        continue_list(&text, selection).map(|edit| shown(&edit))
    }

    #[test]
    fn enter_continues_lists_and_quotes() {
        assert_eq!(enter("- one|").unwrap(), "- one\n- |");
        assert_eq!(enter("* one|").unwrap(), "* one\n* |");
        assert_eq!(enter("+ one|").unwrap(), "+ one\n+ |");
        assert_eq!(enter("1. one|").unwrap(), "1. one\n2. |");
        assert_eq!(enter("9) one|").unwrap(), "9) one\n10) |");
        assert_eq!(enter("- [x] one|").unwrap(), "- [x] one\n- [ ] |");
        assert_eq!(enter("1. [ ] one|").unwrap(), "1. [ ] one\n2. [ ] |");
        assert_eq!(enter("> one|").unwrap(), "> one\n> |");
        assert_eq!(enter("  - one|").unwrap(), "  - one\n  - |");
    }

    #[test]
    fn enter_in_the_middle_splits_the_item() {
        assert_eq!(enter("- on|e").unwrap(), "- on\n- |e");
        assert_eq!(enter("- |word| x").unwrap(), "- \n- | x");
    }

    #[test]
    fn enter_on_an_empty_item_ends_the_list() {
        assert_eq!(enter("- a\n- |").unwrap(), "- a\n|");
        assert_eq!(enter("1. a\n2. |").unwrap(), "1. a\n|");
        assert_eq!(enter("- [ ] |").unwrap(), "|");
        assert_eq!(enter("> |").unwrap(), "|");
    }

    #[test]
    fn enter_does_nothing_special_elsewhere() {
        assert_eq!(enter("plain|"), None);
        assert_eq!(enter("|- one"), None, "before the marker");
        assert_eq!(enter("---|"), None);
        assert_eq!(enter("```\n- one|\n```"), None, "in code");
    }

    fn tab(text: &str, outdent: bool) -> Option<String> {
        let (text, selection) = marked(text);
        indent_item(&text, selection, outdent).map(|edit| shown(&edit))
    }

    #[test]
    fn tab_nests_items_under_the_one_before() {
        assert_eq!(tab("- a\n- b|", false).unwrap(), "- a\n  - b|");
        assert_eq!(tab("1. a\n2. b|", false).unwrap(), "1. a\n   2. b|");
        assert_eq!(tab("10. a\n11. b|", false).unwrap(), "10. a\n    11. b|");
        assert_eq!(
            tab("- [ ] a\n- [ ] b|", false).unwrap(),
            "- [ ] a\n  - [ ] b|"
        );
    }

    #[test]
    fn shift_tab_brings_them_back() {
        assert_eq!(tab("- a\n  - b|", true).unwrap(), "- a\n- b|");
        assert_eq!(tab("- b|", true).unwrap(), "- b|");
    }

    #[test]
    fn tab_elsewhere_is_left_to_the_editor() {
        assert_eq!(tab("plain|", false), None);
        assert_eq!(tab("> quote|", false), None);
        assert_eq!(tab("|- a\n- b|", false), None, "several lines");
        assert_eq!(tab("```\n- a|\n```", false), None);
    }

    #[test]
    fn code_blocks_fence_and_outfence() {
        let (text, selection) = marked("|a|");
        assert_eq!(shown(&code_block(&text, selection)), "```\n|a|\n```");
        let (text, selection) = marked("x\n|");
        assert_eq!(shown(&code_block(&text, selection)), "x\n```\n|\n```");
        let (text, selection) = marked("|```\na\n```|");
        assert!(code_block(&text, selection).text.starts_with("````\n```"));
        let (text, selection) = marked("one tw|o\nthree");
        assert_eq!(
            code_block(&text, selection).text,
            "```\none two\n```\nthree"
        );
    }

    #[test]
    fn links_select_what_to_fill_in() {
        let (text, selection) = marked("see |this| now");
        assert_eq!(shown(&link(&text, selection)), "see [this](|url|) now");
        let (text, selection) = marked("see th|is now");
        assert_eq!(shown(&link(&text, selection)), "see [this](|url|) now");
        let (text, selection) = marked("a | b");
        assert_eq!(shown(&link(&text, selection)), "a [|text|](url) b");
        let (text, selection) = marked("|alt|");
        assert_eq!(shown(&image(&text, selection)), "![alt](|path|)");
    }

    #[test]
    fn pasting_an_address_over_text_links_it() {
        let paste = |text: &str, url: &str| {
            let (text, selection) = marked(text);
            paste_url(&text, selection, url).map(|edit| shown(&edit))
        };
        assert_eq!(
            paste("see |this| now", "https://a.com/b").unwrap(),
            "see [this](https://a.com/b)| now"
        );
        assert_eq!(
            paste("see |this|", "https://a.com\n").unwrap(),
            "see [this](https://a.com)|"
        );
        assert_eq!(paste("nothing | selected", "https://a.com"), None);
        assert_eq!(paste("|this|", "plain words"), None);
        assert_eq!(paste("|this|", "https://a.com and more"), None);
        assert_eq!(paste("|https://old.com|", "https://a.com"), None);
        assert_eq!(paste("|two\nlines|", "https://a.com"), None);
        assert_eq!(paste("```\n|code|\n```", "https://a.com"), None);
    }

    #[test]
    fn blocks_get_lines_of_their_own() {
        let insert = |text: &str| {
            let (text, selection) = marked(text);
            shown(&rule(&text, selection))
        };
        assert_eq!(insert("|"), "---\n|");
        assert_eq!(insert("a|"), "a\n\n---\n|");
        assert_eq!(insert("a\n|"), "a\n\n---\n|");
        assert_eq!(insert("a\n\n|"), "a\n\n---\n|");
        assert_eq!(insert("a|b"), "a\n\n---\n|\nb");
        assert_eq!(insert("a|\nb"), "a\n\n---\n|\nb");
        assert_eq!(insert("a|\n\nb"), "a\n\n---|\n\nb");
    }

    #[test]
    fn a_new_table_has_its_first_heading_selected() {
        let (text, selection) = marked("a|");
        let edit = insert_table(&text, selection);
        let selected: String = edit
            .text
            .chars()
            .skip(edit.selection.start)
            .take(edit.selection.end - edit.selection.start)
            .collect();
        assert_eq!(selected, "Column");
        assert!(table::at(&edit.text, edit.selection.start).is_some());
    }

    #[test]
    fn positions_in_non_ascii_text_stay_right() {
        assert_eq!(inline("é |日本語| ü", Inline::Bold), "é **|日本語|** ü");
        assert_eq!(enter("- 日本語|").unwrap(), "- 日本語\n- |");
        assert_eq!(lines("日本|語", LineKind::Bullet), "- 日本|語");
    }
}
