//! Text operations for the editor. Positions are in chars, like egui's text
//! cursors.

use std::ops::Range;

/// The byte offset of char index `char_index`, or the text's length past its end.
pub fn char_to_byte(text: &str, char_index: usize) -> usize {
    text.char_indices()
        .nth(char_index)
        .map_or(text.len(), |(byte, _)| byte)
}

/// The char index where 1-based `line` starts, clamped to the last line.
pub fn line_start(text: &str, line: usize) -> usize {
    let mut chars = 0;
    for (index, l) in text.split_inclusive('\n').enumerate() {
        if index + 1 == line {
            return chars;
        }
        chars += l.chars().count();
    }
    // Past the end: the start of the last line.
    let last = text.rfind('\n').map_or(0, |i| i + 1);
    text[..last].chars().count()
}

/// What one press of Tab inserts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Indent {
    Tab,
    Spaces(usize),
}

impl Indent {
    pub fn text(self) -> String {
        match self {
            Self::Tab => "\t".to_owned(),
            Self::Spaces(n) => " ".repeat(n),
        }
    }

    pub fn label(self) -> String {
        match self {
            Self::Tab => "Tabs".to_owned(),
            Self::Spaces(n) => format!("{n} spaces"),
        }
    }

    /// Guesses from the leading whitespace of the first lines.
    pub fn detect(text: &str) -> Self {
        let (mut tabs, mut spaces, mut twos) = (0, 0, 0);
        for line in text.lines().take(2000) {
            if line.trim().is_empty() {
                continue;
            }
            if line.starts_with('\t') {
                tabs += 1;
            } else if line.starts_with(' ') {
                spaces += 1;
                let width = line.len() - line.trim_start_matches(' ').len();
                if width % 4 == 2 {
                    twos += 1;
                }
            }
        }
        if tabs > spaces {
            Self::Tab
        } else if twos * 5 > spaces && twos > 0 {
            Self::Spaces(2)
        } else {
            Self::Spaces(4)
        }
    }
}

/// A text after an edit, with the selection it should have.
#[derive(Debug, PartialEq)]
pub struct Edit {
    pub text: String,
    pub selection: Range<usize>,
}

/// A change within one line, in chars from the line's start.
struct LineEdit {
    line: usize,
    col: usize,
    remove: usize,
    insert: String,
}

struct LineInfo<'a> {
    /// Without the line break.
    text: &'a str,
    start: usize,
}

fn line_infos(text: &str) -> Vec<LineInfo<'_>> {
    let mut start = 0;
    let mut infos: Vec<LineInfo<'_>> = text
        .split_inclusive('\n')
        .map(|line| {
            let info = LineInfo {
                text: line.strip_suffix('\n').unwrap_or(line),
                start,
            };
            start += line.chars().count();
            info
        })
        .collect();
    if text.is_empty() || text.ends_with('\n') {
        infos.push(LineInfo { text: "", start });
    }
    infos
}

fn line_of(infos: &[LineInfo<'_>], char_index: usize) -> usize {
    infos
        .partition_point(|info| info.start <= char_index)
        .saturating_sub(1)
}

/// The lines a selection touches. A selection ending at the very start of a
/// line doesn't include that line.
fn selected_lines(infos: &[LineInfo<'_>], selection: &Range<usize>) -> Range<usize> {
    let first = line_of(infos, selection.start);
    let mut last = line_of(infos, selection.end);
    if last > first && infos[last].start == selection.end {
        last -= 1;
    }
    first..last + 1
}

fn apply(text: &str, infos: &[LineInfo<'_>], edits: &[LineEdit], selection: Range<usize>) -> Edit {
    let mut out = String::with_capacity(text.len() + edits.len() * 4);
    let map_col = |line: usize, col: usize| {
        let mut new = col;
        for edit in edits.iter().filter(|e| e.line == line) {
            let inserted = edit.insert.chars().count();
            if col >= edit.col + edit.remove && col > edit.col {
                new = new + inserted - edit.remove;
            } else if col > edit.col {
                // Inside removed text.
                new = new - (col - edit.col) + inserted.min(col - edit.col);
            }
        }
        new
    };
    let mut new_starts = Vec::with_capacity(infos.len());
    let mut edits_iter = edits.iter().peekable();
    let mut chars = 0;
    for (index, info) in infos.iter().enumerate() {
        new_starts.push(chars);
        let mut col = 0;
        let mut rest = info.text;
        let line_start = out.len();
        while let Some(edit) = edits_iter.next_if(|e| e.line == index) {
            let skip = char_to_byte(rest, edit.col - col);
            out.push_str(&rest[..skip]);
            rest = &rest[skip..];
            let removed = char_to_byte(rest, edit.remove);
            rest = &rest[removed..];
            out.push_str(&edit.insert);
            col = edit.col + edit.remove;
        }
        out.push_str(rest);
        chars += out[line_start..].chars().count();
        if index + 1 < infos.len() {
            out.push('\n');
            chars += 1;
        }
    }
    let map = |pos: usize| {
        let line = line_of(infos, pos);
        new_starts[line] + map_col(line, pos - infos[line].start)
    };
    Edit {
        selection: map(selection.start)..map(selection.end),
        text: out,
    }
}

/// Replaces the selection with `insert`, leaving the cursor after it.
pub fn insert(text: &str, selection: Range<usize>, insert: &str) -> Edit {
    let start = char_to_byte(text, selection.start);
    let end = start + char_to_byte(&text[start..], selection.end - selection.start);
    let cursor = selection.start + insert.chars().count();
    Edit {
        text: [&text[..start], insert, &text[end..]].concat(),
        selection: cursor..cursor,
    }
}

/// Indents (or outdents) every line the selection touches.
pub fn indent_lines(text: &str, selection: Range<usize>, indent: Indent, outdent: bool) -> Edit {
    let infos = line_infos(text);
    let lines = selected_lines(&infos, &selection);
    let width = match indent {
        Indent::Tab => 4,
        Indent::Spaces(n) => n,
    };
    let edits: Vec<LineEdit> = lines
        .filter_map(|line| {
            let info = &infos[line];
            if outdent {
                let remove = if info.text.starts_with('\t') {
                    1
                } else {
                    info.text
                        .chars()
                        .take(width)
                        .take_while(|&c| c == ' ')
                        .count()
                };
                (remove > 0).then(|| LineEdit {
                    line,
                    col: 0,
                    remove,
                    insert: String::new(),
                })
            } else {
                (!info.text.is_empty()).then(|| LineEdit {
                    line,
                    col: 0,
                    remove: 0,
                    insert: indent.text(),
                })
            }
        })
        .collect();
    apply(text, &infos, &edits, selection)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Comment {
    Line(&'static str),
    Block(&'static str, &'static str),
}

/// How a language comments out a line, by syntect syntax name.
pub fn comment_style(syntax: &str) -> Option<Comment> {
    use Comment::{Block, Line};
    Some(match syntax {
        "Rust" | "C" | "C++" | "C#" | "Java" | "JavaScript" | "JavaScript (Babel)"
        | "TypeScript" | "TypeScriptReact" | "Go" | "Swift" | "Kotlin" | "Scala" | "Dart"
        | "Zig" | "Groovy" | "PHP" | "Objective-C" | "Objective-C++" | "D" | "Protocol Buffer"
        | "GLSL" | "WGSL" | "Less" | "SCSS" | "Solidity" | "Verilog" | "SystemVerilog" | "F#"
        | "QML" | "Odin" | "jsonnet" | "JSON" | "ActionScript" | "Stylus" | "Graphviz (DOT)"
        | "Pascal" | "LLVM" => match syntax {
            "LLVM" => Line(";"),
            _ => Line("//"),
        },
        "Python"
        | "Ruby"
        | "Perl"
        | "Bourne Again Shell (bash)"
        | "Fish"
        | "TOML"
        | "YAML"
        | "Makefile"
        | "CMake"
        | "CMakeCache"
        | "Dockerfile"
        | "Dockerfile (with bash)"
        | "Nix"
        | "R"
        | "Elixir"
        | "Julia"
        | "Nim"
        | "PowerShell"
        | "Crystal"
        | "GraphQL"
        | "Terraform"
        | "Git Config"
        | "Git Ignore"
        | "Git Attributes"
        | "Requirements.txt"
        | "SSH Config"
        | "SSHD Config"
        | "nginx"
        | "Apache Conf"
        | "CoffeeScript"
        | "Tcl"
        | "Puppet"
        | "fstab"
        | "Hosts File"
        | "resolv"
        | "DotENV"
        | "Java Properties"
        | "AWK"
        | "Crontab"
        | "Gomod"
        | "Ninja"
        | "Salt State (SLS)"
        | "Rego"
        | "Robot Framework"
        | "GDScript (Godot Engine)"
        | "Cabal"
        | "debsources"
        | "Known Hosts"
        | "Authorized Keys"
        | "gnuplot"
        | "varlink"
        | "Vyper"
        | "Diff"
        | "JQ" => match syntax {
            "Cabal" => Line("--"),
            _ => Line("#"),
        },
        "SQL" | "Lua" | "Haskell" | "Elm" | "Ada" | "PureScript" | "VHDL" | "Idris" | "Lean 4"
        | "AppleScript" => Line("--"),
        "Lisp" | "Clojure" | "Racket" | "x86_64 Assembly" | "INI" | "NSIS" | "ARM Assembly" => {
            match syntax {
                "ARM Assembly" => Line("@"),
                _ => Line(";"),
            }
        }
        "TeX" | "LaTeX" | "Erlang" | "MATLAB" | "BibTeX" => Line("%"),
        "VimL" => Line("\""),
        "Batch File" => Line("::"),
        "Typst" => Line("//"),
        "Fortran (Modern)" => Line("!"),
        "CSS" => Block("/*", "*/"),
        "HTML" | "XML" | "Markdown" | "MultiMarkdown" | "Vue Component" | "Svelte"
        | "HTML (Jinja2)" | "HTML (Twig)" | "HTML (EEx)" | "HTML (Rails)" | "HTML (ASP)"
        | "NAnt Build File" => Block("<!--", "-->"),
        "OCaml" | "SML" | "camlp4" => Block("(*", "*)"),
        "Jinja2" => Block("{#", "#}"),
        _ => return None,
    })
}

/// Comments out the selected lines, or uncomments them if they all are.
pub fn toggle_comment(text: &str, selection: Range<usize>, style: Comment) -> Edit {
    let infos = line_infos(text);
    let lines: Vec<usize> = selected_lines(&infos, &selection)
        .filter(|&l| !infos[l].text.trim().is_empty())
        .collect();
    let indent_of = |text: &str| text.chars().take_while(|c| c.is_whitespace()).count();
    let (open, close) = match style {
        Comment::Line(token) => (token, ""),
        Comment::Block(open, close) => (open, close),
    };
    let commented = !lines.is_empty()
        && lines.iter().all(|&l| {
            let trimmed = infos[l].text.trim();
            trimmed.starts_with(open) && trimmed.ends_with(close)
        });
    let mut edits = Vec::new();
    if commented {
        for &line in &lines {
            let text = infos[line].text;
            let col = indent_of(text);
            let after = &text[char_to_byte(text, col) + open.len()..];
            let space = usize::from(after.starts_with(' '));
            edits.push(LineEdit {
                line,
                col,
                remove: open.chars().count() + space,
                insert: String::new(),
            });
            if !close.is_empty() {
                let trimmed_end = text.trim_end();
                let end_col = trimmed_end.chars().count();
                let before_close = &trimmed_end[..trimmed_end.len() - close.len()];
                let space =
                    usize::from(before_close.ends_with(' ') && before_close.len() > open.len());
                let close_len = close.chars().count();
                edits.push(LineEdit {
                    line,
                    col: end_col - close_len - space,
                    remove: close_len + space,
                    insert: String::new(),
                });
            }
        }
    } else {
        let col = lines
            .iter()
            .map(|&l| indent_of(infos[l].text))
            .min()
            .unwrap_or(0);
        for &line in &lines {
            edits.push(LineEdit {
                line,
                col,
                remove: 0,
                insert: format!("{open} "),
            });
            if !close.is_empty() {
                edits.push(LineEdit {
                    line,
                    col: infos[line].text.trim_end().chars().count(),
                    remove: 0,
                    insert: format!(" {close}"),
                });
            }
        }
    }
    apply(text, &infos, &edits, selection)
}

/// The leading whitespace of the line `char_index` is on, up to it: what a
/// new line after it starts with.
pub fn indent_at(text: &str, char_index: usize) -> &str {
    let before = &text[..char_to_byte(text, char_index)];
    let line = &before[before.rfind('\n').map_or(0, |i| i + 1)..];
    &line[..line.len() - line.trim_start_matches([' ', '\t']).len()]
}

/// Searches stop after this many matches.
pub const MAX_MATCHES: usize = 100_000;

/// Where `query` occurs in `text`, as char ranges.
pub fn find(text: &str, query: &str, match_case: bool, whole_word: bool) -> Vec<Range<usize>> {
    if query.is_empty() {
        return Vec::new();
    }
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let bounded = |start: usize, end: usize| {
        !whole_word
            || (!text[..start].chars().next_back().is_some_and(is_word)
                && !text[end..].chars().next().is_some_and(is_word))
    };
    let mut bytes: Vec<Range<usize>> = Vec::new();
    if match_case {
        for (start, m) in text.match_indices(query) {
            if bounded(start, start + m.len()) {
                bytes.push(start..start + m.len());
            }
            if bytes.len() >= MAX_MATCHES {
                break;
            }
        }
    } else if query.is_ascii() {
        let (hay, needle) = (text.as_bytes(), query.as_bytes());
        let mut at = 0;
        while at + needle.len() <= hay.len() && bytes.len() < MAX_MATCHES {
            if hay[at..at + needle.len()].eq_ignore_ascii_case(needle)
                && text.is_char_boundary(at)
                && bounded(at, at + needle.len())
            {
                bytes.push(at..at + needle.len());
                at += needle.len();
            } else {
                at += 1;
            }
        }
    } else {
        let needle: Vec<char> = query.chars().flat_map(char::to_lowercase).collect();
        let mut skip_until = 0;
        for (start, _) in text.char_indices() {
            if start < skip_until {
                continue;
            }
            let mut end = start;
            let mut matched = 0;
            // Advance through original chars while their lowercase forms match.
            for c in text[start..].chars() {
                let lower: Vec<char> = c.to_lowercase().collect();
                if needle.len() < matched + lower.len()
                    || needle[matched..matched + lower.len()] != lower[..]
                {
                    break;
                }
                matched += lower.len();
                end += c.len_utf8();
                if matched == needle.len() {
                    break;
                }
            }
            if matched == needle.len() && bounded(start, end) {
                bytes.push(start..end);
                skip_until = end;
                if bytes.len() >= MAX_MATCHES {
                    break;
                }
            }
        }
    }
    // Bytes to chars, in one pass.
    let mut chars = 0;
    let mut last = 0;
    bytes
        .into_iter()
        .map(|range| {
            chars += text[last..range.start].chars().count();
            let start = chars;
            chars += text[range.clone()].chars().count();
            last = range.end;
            start..chars
        })
        .collect()
}

/// Replaces the given char ranges (sorted, not overlapping) with `with`.
pub fn replace_all(text: &str, ranges: &[Range<usize>], with: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.char_indices().map(|(i, _)| i).chain([text.len()]);
    let mut char_pos = 0;
    let mut byte_of = |target: usize| {
        let mut byte = 0;
        while char_pos <= target {
            byte = chars.next().unwrap_or(text.len());
            char_pos += 1;
        }
        byte
    };
    let mut last = 0;
    for range in ranges {
        let start = byte_of(range.start);
        let end = byte_of(range.end);
        out.push_str(&text[last..start]);
        out.push_str(with);
        last = end;
    }
    out.push_str(&text[last..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sel(text: &str, from: &str, to: &str) -> Range<usize> {
        let start = text[..text.find(from).unwrap()].chars().count();
        let end = text[..text.find(to).unwrap()].chars().count();
        start..end
    }

    #[test]
    fn line_starts() {
        let text = "ab\ncdé\nf";
        assert_eq!(line_start(text, 1), 0);
        assert_eq!(line_start(text, 2), 3);
        assert_eq!(line_start(text, 3), 7);
        assert_eq!(line_start(text, 99), 7);
    }

    #[test]
    fn detects_indentation() {
        assert_eq!(Indent::detect("fn a() {\n    b();\n}\n"), Indent::Spaces(4));
        assert_eq!(Indent::detect("a:\n  b:\n    c: 1\n"), Indent::Spaces(2));
        assert_eq!(Indent::detect("int a() {\n\tb();\n}\n"), Indent::Tab);
        assert_eq!(Indent::detect("plain\n"), Indent::Spaces(4));
    }

    #[test]
    fn indents_and_outdents_selected_lines() {
        let text = "a\nb\n\nc\nd";
        let selection = sel(text, "a", "c");
        let edit = indent_lines(text, selection.clone(), Indent::Spaces(4), false);
        // The selection ends at the start of "c", so "c" stays; blank lines too.
        assert_eq!(edit.text, "    a\n    b\n\nc\nd");
        assert_eq!(edit.selection, 0..13);
        let back = indent_lines(&edit.text, edit.selection, Indent::Spaces(4), true);
        assert_eq!(back.text, text);
        assert_eq!(back.selection, selection);

        // Outdenting removes at most one level, and tabs.
        let edit = indent_lines("\t\tx\n  y\n", 0..5, Indent::Spaces(4), true);
        assert_eq!(edit.text, "\tx\ny\n");
    }

    #[test]
    fn indent_keeps_a_cursor_on_its_text() {
        let text = "let x;\n";
        let edit = indent_lines(text, 4..4, Indent::Tab, false);
        assert_eq!(edit.text, "\tlet x;\n");
        assert_eq!(edit.selection, 5..5);
        let edit = indent_lines("    ab", 2..2, Indent::Spaces(4), true);
        assert_eq!(edit.text, "ab");
        assert_eq!(edit.selection, 0..0);
    }

    #[test]
    fn toggles_line_comments() {
        let text = "fn a() {\n    b();\n\n    c();\n}\n";
        let selection = sel(text, "    b", "}");
        let edit = toggle_comment(text, selection.clone(), Comment::Line("//"));
        assert_eq!(edit.text, "fn a() {\n    // b();\n\n    // c();\n}\n");
        let back = toggle_comment(&edit.text, edit.selection, Comment::Line("//"));
        assert_eq!(back.text, text);
        assert_eq!(back.selection, selection);

        // Mixed lines get commented, not uncommented.
        let edit = toggle_comment("# a\nb\n", 0..5, Comment::Line("#"));
        assert_eq!(edit.text, "# # a\n# b\n");
    }

    #[test]
    fn toggles_block_comments() {
        let text = "<p>hi</p>\n";
        let edit = toggle_comment(text, 0..0, Comment::Block("<!--", "-->"));
        assert_eq!(edit.text, "<!-- <p>hi</p> -->\n");
        let back = toggle_comment(&edit.text, 0..0, Comment::Block("<!--", "-->"));
        assert_eq!(back.text, text);
    }

    #[test]
    fn new_lines_keep_indentation() {
        assert_eq!(indent_at("fn a() {\n    let x", 17), "    ");
        assert_eq!(indent_at("\tfoo", 1), "\t");
        assert_eq!(indent_at("x", 1), "");
    }

    #[test]
    fn finds_matches() {
        let text = "Foo foo é FOO food";
        assert_eq!(find(text, "foo", true, false), [4..7, 14..17]);
        assert_eq!(
            find(text, "foo", false, false),
            [0..3, 4..7, 10..13, 14..17]
        );
        assert_eq!(find(text, "foo", false, true), [0..3, 4..7, 10..13]);
        assert_eq!(find("ÉCOLE école", "école", false, false), [0..5, 6..11]);
        assert!(find(text, "", false, false).is_empty());
        // Positions after a multibyte char are in chars.
        let after_e = find("é x", "x", true, false);
        assert_eq!((after_e.len(), after_e[0].clone()), (1, 2..3));
    }

    #[test]
    fn replaces_all_matches() {
        let text = "a é a b a";
        let matches = find(text, "a", true, true);
        assert_eq!(replace_all(text, &matches, "xy"), "xy é xy b xy");
        assert_eq!(replace_all(text, &[], "xy"), text);
    }
}
