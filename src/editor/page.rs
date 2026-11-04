//! The keys while the page itself is being edited (see
//! [`markdown::wysiwyg`](crate::markdown::wysiwyg)). The cursor is the one the
//! source has, so undo, the formatting commands and changing modes all work on
//! it as they do there.
//!
//! An edit needs what's on the page to go by, so each frame takes one and the
//! rest wait for the next, when it's been parsed and laid out again.

use std::ops::Range;

use egui::{Context, Event, Key, Modifiers, text::CCursor, text::CCursorRange};

use super::{Doc, Edit, EditorTab, TextEditState, ops, text_id};
use crate::markdown::{
    Caret, Mode, PageCommand, block_edit, format,
    layout::Layout,
    wysiwyg::{self, Change, Editable, FieldKind},
};

/// How many rows Page Up and Page Down go.
const PAGE_ROWS: usize = 20;

/// Gives the page the keys, if it hasn't got them. Asking when it has would
/// interrupt the input method, and on Android that's the keyboard going down
/// and up again.
fn take_keys(ctx: &Context, tab: u64) {
    let id = text_id(tab);
    if !ctx.memory(|memory| memory.has_focus(id)) {
        ctx.memory_mut(|memory| memory.request_focus(id));
    }
}

/// The cursor in the source, in bytes.
pub(super) fn caret(ctx: &Context, tab: u64, text: &str) -> Caret {
    let id = text_id(tab);
    let range = TextEditState::load(ctx, id)
        .and_then(|state| state.cursor.char_range())
        .unwrap_or_default();
    let anchor = ops::char_to_byte(text, range.secondary.index.0);
    let head = ops::char_to_byte(text, range.primary.index.0);
    Caret {
        anchor,
        head,
        gap: (anchor == head && wysiwyg::in_gap(text, head)).then_some(head),
        focus: id,
    }
}

impl EditorTab {
    /// Tells the undo history how things stand, as a text field does each
    /// frame: it keeps a state once it has settled, so typing goes together.
    fn feed_undo(&self, ctx: &Context) {
        let Doc::Ready(buffer) = &self.doc else {
            return;
        };
        let id = text_id(self.id);
        let mut state = TextEditState::load(ctx, id).unwrap_or_default();
        let cursor = state.cursor.char_range().unwrap_or_default();
        let time = ctx.input(|i| i.time);
        let mut undoer = state.undoer();
        undoer.feed_state(time, &(cursor, buffer.text.clone()));
        state.set_undoer(undoer);
        state.store(ctx, id);
    }

    /// Moves the cursor, in bytes.
    pub(super) fn set_page_caret(&mut self, ctx: &Context, anchor: usize, head: usize) {
        let Doc::Ready(buffer) = &self.doc else {
            return;
        };
        let chars = |offset: usize| {
            let offset = offset.min(buffer.text.len());
            buffer.text.get(..offset).map_or(0, |t| t.chars().count())
        };
        let (anchor, head) = (chars(anchor), chars(head));
        let id = text_id(self.id);
        let mut state = TextEditState::load(ctx, id).unwrap_or_default();
        state.cursor.set_char_range(Some(CCursorRange::two(
            CCursor::new(anchor),
            CCursor::new(head),
        )));
        state.store(ctx, id);
        self.md.reveal = true;
    }

    /// Carries out what was asked of the page since last frame.
    pub(super) fn page_input(&mut self, ctx: &Context) {
        self.feed_undo(ctx);
        // The keyboard came up, or the view shrank some other way: what's being
        // typed at shouldn't end up under it.
        let height = self.md.view.viewport_height;
        if height < self.md.page_height - 1.0 {
            self.md.reveal = true;
        }
        self.md.page_height = height;
        // What other things ask for: going to a line, or a match found.
        if let Some(select) = self.select.take() {
            let id = text_id(self.id);
            let mut state = TextEditState::load(ctx, id).unwrap_or_default();
            state.cursor.set_char_range(Some(CCursorRange::two(
                CCursor::new(select.start),
                CCursor::new(select.end),
            )));
            state.store(ctx, id);
            self.md.reveal = true;
        }
        if std::mem::take(&mut self.scroll_to_cursor) {
            self.md.reveal = true;
        }
        if std::mem::take(&mut self.focus_text) {
            take_keys(ctx, self.id);
        }
        if !ctx.memory(|memory| memory.has_focus(text_id(self.id))) {
            return;
        }
        let mut events = std::mem::take(&mut self.md.waiting_keys);
        ctx.input(|i| {
            events.extend(i.events.iter().filter(|event| is_for_page(event)).cloned());
        });
        let mut events = events.into_iter();
        while let Some(event) = events.next() {
            let Doc::Ready(buffer) = &self.doc else {
                return;
            };
            // Parsing is held back for very long documents.
            if !self.md.view.is_current(&buffer.text) {
                self.md.waiting_keys.push(event);
                continue;
            }
            if self.page_event(ctx, event) {
                self.md.waiting_keys.extend(events);
                ctx.request_repaint();
                return;
            }
        }
    }

    /// Whether this changed the text, so that what's next must wait.
    fn page_event(&mut self, ctx: &Context, event: Event) -> bool {
        let Doc::Ready(buffer) = &self.doc else {
            return false;
        };
        let text = buffer.text.clone();
        let caret = caret(ctx, self.id, &text);
        let (min, max) = (caret.anchor.min(caret.head), caret.anchor.max(caret.head));
        let selection = min..max;
        let layout = self.md.view.layout();
        let placed = layout.placed_fields();
        let fields: Vec<&Editable> = placed.iter().map(|placed| placed.field).collect();
        let kind = wysiwyg::field_at(&fields, caret.head).map(|(_, field)| field.kind);
        let typed = |typed: &str| Some(wysiwyg::insert(&text, selection.clone(), typed, kind));
        let change = match &event {
            Event::Text(typed_text) | Event::Paste(typed_text) => typed(typed_text),
            Event::Ime(egui::ImeEvent::Commit(typed_text)) => typed(typed_text),
            Event::Copy | Event::Cut => {
                if !selection.is_empty() {
                    ctx.copy_text(text[selection.clone()].to_owned());
                }
                match event {
                    Event::Cut => wysiwyg::delete_selection(&text, &fields, selection.clone()),
                    _ => None,
                }
            }
            Event::Key {
                key,
                pressed: true,
                modifiers,
                ..
            } => {
                let outcome = page_key(
                    layout,
                    &mut self.md.caret_x,
                    (*key, *modifiers),
                    &text,
                    &caret,
                    &fields,
                );
                match outcome {
                    KeyOutcome::Change(change) => change,
                    KeyOutcome::Edit(edit) => {
                        self.apply_edit(ctx, edit);
                        self.md.caret_x = None;
                        self.md.reveal = true;
                        return true;
                    }
                    KeyOutcome::Moved(anchor, head) => {
                        self.set_page_caret(ctx, anchor, head);
                        return false;
                    }
                    KeyOutcome::Nothing => None,
                }
            }
            _ => None,
        };
        let Some(change) = change else {
            return false;
        };
        // A character at a time, typed or taken back, goes together.
        let typing = match &event {
            Event::Text(_) => true,
            Event::Key { key, .. } => {
                matches!(key, Key::Backspace | Key::Delete) && selection.is_empty()
            }
            _ => false,
        };
        self.apply_change(ctx, change, typing);
        true
    }

    /// Puts a change of the source in, as an edit of its own, or if it's typing
    /// as part of what's being typed.
    fn apply_change(&mut self, ctx: &Context, change: Change, typing: bool) {
        let Doc::Ready(buffer) = &mut self.doc else {
            return;
        };
        if buffer.read_only.is_some() {
            return;
        }
        let selection = {
            let at = change
                .text
                .get(..change.cursor)
                .map_or(0, |t| t.chars().count());
            at..at
        };
        self.md.caret_x = None;
        self.md.reveal = true;
        if !typing {
            self.apply_edit(
                ctx,
                Edit {
                    text: change.text,
                    selection,
                },
            );
            return;
        }
        // Typing goes in together until it pauses: see `feed_undo`.
        let id = text_id(self.id);
        let mut state = TextEditState::load(ctx, id).unwrap_or_default();
        buffer.text = change.text;
        let cursor = CCursorRange::one(CCursor::new(selection.start));
        state.cursor.set_char_range(Some(cursor));
        state.store(ctx, id);
    }

    /// What a click in the page does to the cursor.
    pub(super) fn page_clicked(&mut self, ctx: &Context, offset: usize, extend: bool) {
        let Doc::Ready(buffer) = &self.doc else {
            return;
        };
        let anchor = if extend {
            caret(ctx, self.id, &buffer.text).anchor
        } else {
            offset
        };
        self.md.caret_x = None;
        self.set_page_caret(ctx, anchor, offset);
        take_keys(ctx, self.id);
        // Sent away with Back, it comes back for a tap. Only then: interrupting
        // is what brings it back, and with it up, is a flicker. What says it's
        // up is a moment behind, so it's asked for once while it's on its way.
        const COMING: f64 = 1.5;
        let now = ctx.input(|i| i.time);
        let asked = self.md.keyboard_asked.is_some_and(|at| now - at < COMING);
        if crate::touch::keyboard_up(ctx) == Some(false) && !asked {
            self.md.keyboard_asked = Some(now);
            ctx.memory_mut(egui::Memory::interrupt_ime);
        }
    }

    /// A selection dragged out on a touchscreen: where it's going is in sight.
    pub(super) fn page_selection(&mut self, ctx: &Context, anchor: usize, head: usize) {
        self.md.caret_x = None;
        self.set_page_caret(ctx, anchor, head);
        self.md.reveal = false;
        take_keys(ctx, self.id);
    }

    /// A button of the bar over a selection.
    pub(super) fn page_command(&mut self, ctx: &Context, command: PageCommand) {
        match command {
            PageCommand::Copy => self.md.waiting_keys.push(Event::Copy),
            PageCommand::Cut => self.md.waiting_keys.push(Event::Cut),
            PageCommand::SelectAll => {
                if let Doc::Ready(buffer) = &self.doc {
                    let end = buffer.text.len();
                    self.set_page_caret(ctx, 0, end);
                    self.md.reveal = false;
                }
            }
            PageCommand::Paste => {
                // Comes back as an event, except from Android's own clipboard.
                #[cfg(target_os = "android")]
                if let Some(text) = crate::android::clipboard_text() {
                    self.md.waiting_keys.push(Event::Paste(text));
                }
                #[cfg(not(target_os = "android"))]
                ctx.send_viewport_cmd(egui::ViewportCommand::RequestPaste);
            }
        }
        take_keys(ctx, self.id);
        ctx.request_repaint();
    }

    /// A double-click: the word there.
    pub(super) fn page_select_word(&mut self, ctx: &Context, offset: usize) {
        if let Some(word) = self.md.view.layout().word_around(offset) {
            self.md.caret_x = None;
            self.set_page_caret(ctx, word.start, word.end);
        }
    }

    /// Whether the page is what's being edited now.
    pub(super) fn editing_page(&self) -> bool {
        self.shown_mode == Mode::Wysiwyg && self.md.block.is_none()
    }
}

fn page_key(
    layout: &Layout,
    caret_x: &mut Option<f32>,
    (key, modifiers): (Key, Modifiers),
    text: &str,
    caret: &Caret,
    fields: &[&Editable],
) -> KeyOutcome {
    let (shift, word) = (modifiers.shift, modifiers.command);
    let selection = caret.anchor.min(caret.head)..caret.anchor.max(caret.head);
    let chars = |range: &Range<usize>| {
        let count = |offset: usize| text[..offset].chars().count();
        count(range.start)..count(range.end)
    };
    let moved = |head: Option<usize>| match head {
        Some(head) => KeyOutcome::Moved(if shift { caret.anchor } else { head }, head),
        None => KeyOutcome::Nothing,
    };
    let horizontal = |back: bool| {
        // With a selection and no Shift, the arrows go to its end.
        if !shift && !selection.is_empty() && !word {
            let end = if back { selection.start } else { selection.end };
            return KeyOutcome::Moved(end, end);
        }
        moved(layout.offset_along(caret.head, back, word))
    };
    match key {
        Key::ArrowLeft => {
            *caret_x = None;
            horizontal(true)
        }
        Key::ArrowRight => {
            *caret_x = None;
            horizontal(false)
        }
        Key::ArrowUp | Key::ArrowDown | Key::PageUp | Key::PageDown => {
            let down = matches!(key, Key::ArrowDown | Key::PageDown);
            let rows = if matches!(key, Key::PageUp | Key::PageDown) {
                PAGE_ROWS
            } else {
                1
            };
            let placed = layout.placed_fields();
            let x = caret_x.or_else(|| {
                let (index, _) = wysiwyg::field_at(fields, caret.head)?;
                Some(placed[index].cursor_rect(text, caret.head).min.x)
            });
            let Some(x) = x else {
                return KeyOutcome::Nothing;
            };
            let mut head = caret.head;
            for _ in 0..rows {
                match layout.offset_vertically(text, head, x, down) {
                    Some(next) => head = next,
                    None => {
                        // Off the top or bottom: as far as it goes.
                        if rows == 1 {
                            head = document_edge(text, fields, down);
                        }
                        break;
                    }
                }
            }
            *caret_x = Some(x);
            moved(Some(head))
        }
        Key::Home | Key::End => {
            *caret_x = None;
            let end = key == Key::End;
            if word {
                return moved(Some(document_edge(text, fields, end)));
            }
            moved(layout.row_edge(caret.head, end))
        }
        Key::A if word => KeyOutcome::Moved(0, text.len()),
        Key::Backspace | Key::Delete => {
            let back = key == Key::Backspace;
            if !selection.is_empty() {
                return KeyOutcome::Change(wysiwyg::delete_selection(
                    text,
                    fields,
                    selection.clone(),
                ));
            }
            if word && let Some(to) = layout.offset_along(caret.head, back, true) {
                let range = caret.head.min(to)..caret.head.max(to);
                return KeyOutcome::Change(wysiwyg::delete_selection(text, fields, range));
            }
            let change = if back {
                wysiwyg::backspace(text, fields, caret.head)
            } else {
                wysiwyg::delete_forward(text, fields, caret.head)
            };
            match change {
                Some(change) => KeyOutcome::Change(Some(change)),
                None => past_the_edge(text, layout, fields, caret.head, back),
            }
        }
        Key::Enter => {
            let kind = wysiwyg::field_at(fields, caret.head).map(|(_, field)| field.kind);
            if shift {
                return KeyOutcome::Change(wysiwyg::line_break(text, fields, selection));
            }
            if kind == Some(FieldKind::Inline)
                && in_list_item(text, fields, caret.head)
                && let Some(edit) = format::continue_list(text, chars(&selection))
            {
                return KeyOutcome::Edit(edit);
            }
            KeyOutcome::Change(wysiwyg::enter(text, fields, selection))
        }
        Key::Tab => {
            let kind = wysiwyg::field_at(fields, caret.head).map(|(_, field)| field.kind);
            match kind {
                Some(FieldKind::Cell) => match wysiwyg::next_cell(fields, caret.head, shift) {
                    Some(next) => KeyOutcome::Moved(next, next),
                    None => KeyOutcome::Nothing,
                },
                Some(FieldKind::Code) => {
                    KeyOutcome::Change(Some(wysiwyg::insert(text, selection, "    ", kind)))
                }
                _ => match format::indent_item(text, chars(&selection), shift) {
                    Some(edit) => KeyOutcome::Edit(edit),
                    None => KeyOutcome::Nothing,
                },
            }
        }
        _ => KeyOutcome::Nothing,
    }
}

/// Where the text starts or ends. Before it would be typing in front of a
/// heading's marks, and after it on a line of its own.
fn document_edge(text: &str, fields: &[&Editable], end: bool) -> usize {
    let edge = if end {
        fields.last().map(|field| field.high())
    } else {
        fields.first().map(|field| field.low())
    };
    edge.unwrap_or(if end { text.len() } else { 0 })
}

/// Whether the cursor's line starts with a list's marker. A quote continues
/// too in the source, but on the page Enter there is a new paragraph.
fn in_list_item(text: &str, fields: &[&Editable], offset: usize) -> bool {
    let Some((_, field)) = wysiwyg::field_at(fields, offset) else {
        return false;
    };
    let low = field.low().min(text.len());
    let line = text[..low].rfind('\n').map_or(0, |at| at + 1);
    let marker = text[line..low].trim_start_matches(|c: char| c.is_whitespace() || c == '>');
    !marker.trim().is_empty()
}

/// Backspace or Delete with nothing of the text's to take, at the edge of a
/// field: a block with no text next to it goes, as does a code block left
/// empty. Otherwise the cursor goes on to the next text: that's what it took to
/// get out of a code block or a table, and taking those whole would be a lot.
fn past_the_edge(
    text: &str,
    layout: &Layout,
    fields: &[&Editable],
    head: usize,
    back: bool,
) -> KeyOutcome {
    let Some((index, field)) = wysiwyg::field_at(fields, head) else {
        return KeyOutcome::Nothing;
    };
    let goes = |source: &Range<usize>, cursor: Option<usize>| {
        let range = block_edit::block_range(text, source);
        KeyOutcome::Change(Some(Change {
            text: block_edit::splice(text, &range, ""),
            cursor: cursor.unwrap_or(range.start),
        }))
    };
    let blocks = &layout.blocks;
    if field.kind == FieldKind::Code
        && field.spans.is_empty()
        && let Some((source, ..)) = blocks.iter().find(|(source, ..)| source.contains(&head))
    {
        return goes(source, None);
    }
    let no_text = |source: &Range<usize>| !fields.iter().any(|f| source.contains(&f.low()));
    if field.kind != FieldKind::Cell {
        let next_to = if back {
            let low = field.low();
            blocks
                .iter()
                .filter(|(source, ..)| source.end <= low)
                .max_by_key(|(source, ..)| source.end)
        } else {
            let high = field.high();
            blocks
                .iter()
                .filter(|(source, ..)| source.start >= high)
                .min_by_key(|(source, ..)| source.start)
        };
        if let Some((source, ..)) = next_to
            && no_text(source)
        {
            return goes(source, (!back).then_some(head));
        }
    }
    let neighbor = if back {
        index.checked_sub(1).and_then(|at| fields.get(at))
    } else {
        fields.get(index + 1)
    };
    match neighbor {
        Some(neighbor) => {
            let to = if back {
                neighbor.high()
            } else {
                neighbor.low()
            };
            KeyOutcome::Moved(to, to)
        }
        None => KeyOutcome::Nothing,
    }
}

enum KeyOutcome {
    Change(Option<Change>),
    Edit(Edit),
    Moved(usize, usize),
    Nothing,
}

fn is_for_page(event: &Event) -> bool {
    matches!(
        event,
        Event::Text(_)
            | Event::Paste(_)
            | Event::Copy
            | Event::Cut
            | Event::Ime(egui::ImeEvent::Commit(_))
            | Event::Key { pressed: true, .. }
    )
}
