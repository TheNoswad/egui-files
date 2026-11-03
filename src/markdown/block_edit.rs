//! Editing one block of a rendered document in place. The block's markdown is
//! edited on its own, and goes back as a single splice of the source: nothing
//! outside its range changes.

use std::ops::Range;

/// What to edit for a block the parser found at `source`: whole lines, without
/// the blank lines after it. An indented block's range starts past its indent.
pub fn block_range(text: &str, source: &Range<usize>) -> Range<usize> {
    let end = source.end.min(text.len());
    let start = source.start.min(end);
    let line = text[..start].rfind('\n').map_or(0, |i| i + 1);
    let start = if text[line..start].trim().is_empty() {
        line
    } else {
        start
    };
    let end = start + text[start..end].trim_end().len();
    start..end
}

/// The new text after editing a block. Emptying one takes it away with the
/// blank lines after it; a new one is added at the end as a paragraph of its own.
pub fn splice(text: &str, range: &Range<usize>, draft: &str) -> String {
    let draft = draft.trim_end();
    let (before, after) = (&text[..range.start], &text[range.end..]);
    if range.is_empty() {
        if draft.is_empty() {
            return text.to_owned();
        }
        let before = before.trim_end();
        let gap = if before.is_empty() { "" } else { "\n\n" };
        let tail = if after.is_empty() { "\n" } else { "" };
        return format!("{before}{gap}{draft}{tail}{after}");
    }
    if draft.is_empty() {
        let after = after.trim_start_matches(['\n', '\r']);
        if after.is_empty() {
            let before = before.trim_end();
            return if before.is_empty() {
                String::new()
            } else {
                format!("{before}\n")
            };
        }
        return format!("{before}{after}");
    }
    format!("{before}{draft}{after}")
}

/// Where to put the cursor for a double-click on the `nth` time this word
/// shows on the page: there in the source, as near as can be told. In chars.
pub fn caret_for(draft: &str, word: &str, nth: usize) -> usize {
    let end = || draft.chars().count();
    if word.is_empty() {
        return end();
    }
    let found = draft
        .match_indices(word)
        .nth(nth)
        .or_else(|| draft.match_indices(word).next());
    found.map_or_else(end, |(at, _)| draft[..at].chars().count())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_range_is_whole_lines_without_trailing_blanks() {
        let text = "one\n\n    indented\n\nlast\n\n\n";
        assert_eq!(&text[block_range(text, &(0..5))], "one");
        let at = text.find("indented").unwrap();
        assert_eq!(&text[block_range(text, &(at..at + 9))], "    indented");
        let at = text.find("last").unwrap();
        assert_eq!(&text[block_range(text, &(at..text.len()))], "last");
    }

    #[test]
    fn out_of_range_sources_are_clamped() {
        assert_eq!(block_range("ab", &(5..9)), 2..2);
    }

    #[test]
    fn only_the_block_changes() {
        let text = "one\n\ntwo  \n\n\nthree\r\n";
        let at = text.find("two").unwrap();
        let range = at..at + 5;
        assert_eq!(splice(text, &range, "TWO"), "one\n\nTWO\n\n\nthree\r\n");
        assert_eq!(splice(text, &range, "two  "), "one\n\ntwo\n\n\nthree\r\n");
        assert_eq!(
            splice(text, &(0..3), "a\n\nb\n\n"),
            "a\n\nb\n\ntwo  \n\n\nthree\r\n"
        );
    }

    #[test]
    fn emptying_a_block_takes_its_blank_lines_too() {
        let text = "one\n\ntwo\n\nthree\n";
        assert_eq!(splice(text, &(5..8), ""), "one\n\nthree\n");
        assert_eq!(splice(text, &(5..8), "  \n "), "one\n\nthree\n");
        assert_eq!(splice(text, &(0..3), ""), "two\n\nthree\n");
        assert_eq!(splice(text, &(10..15), ""), "one\n\ntwo\n");
        assert_eq!(splice("only\n", &(0..4), ""), "");
    }

    #[test]
    fn a_new_block_goes_on_the_end() {
        assert_eq!(splice("one\n", &(4..4), "two"), "one\n\ntwo\n");
        assert_eq!(splice("one", &(3..3), "two"), "one\n\ntwo\n");
        assert_eq!(splice("one\n\n\n", &(6..6), "two"), "one\n\ntwo\n");
        assert_eq!(splice("", &(0..0), "first"), "first\n");
        assert_eq!(splice("one\n", &(4..4), ""), "one\n", "nothing typed");
    }

    #[test]
    fn the_cursor_lands_on_the_word_clicked() {
        let draft = "**bold** word and word, 日本語 word";
        assert_eq!(caret_for(draft, "word", 0), 9);
        assert_eq!(caret_for(draft, "word", 1), 18);
        assert_eq!(caret_for(draft, "日本語", 0), 24);
        assert_eq!(caret_for(draft, "word", 2), 28, "chars, not bytes");
        assert_eq!(caret_for(draft, "word", 9), 9, "past the last: the first");
        let end = draft.chars().count();
        assert_eq!(caret_for(draft, "absent", 0), end);
        assert_eq!(caret_for(draft, "", 0), end);
    }
}
