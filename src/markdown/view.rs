//! Drawing a rendered document and handling what's clicked on it.

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
};

use web_time::{Duration, Instant};

use egui::{
    Color32, Context, CornerRadius, Id, Pos2, Rect, ScrollArea, Sense, Shape, Stroke, StrokeKind,
    Ui, Vec2, pos2, vec2,
};
use egui_phosphor::regular as icon;

use super::{
    Images,
    drawing::is_drawing,
    images::resolve,
    layout::{
        self, CodeSpans, Fonts, GalleyCache, Hit, ImageSizes, Inputs, Item, Layout, Params,
        PlacedField, TextItem, Theme, Widget, row_rects,
    },
    parse::{self, BlockKind, Document, Link, Stats, Task},
    wysiwyg::{Editable, field_at},
};
use crate::editor::highlight::{self, Highlighter};

/// Code blocks highlighted at once.
const MAX_HIGHLIGHTERS: usize = 2;
/// Space around the text in the pane.
const MARGIN: f32 = 16.0;
/// Documents longer than this, in bytes, are parsed at most once per pause.
const LONG_DOCUMENT: usize = 256 * 1024;
const LONG_DOCUMENT_PAUSE: Duration = Duration::from_millis(250);

/// What the user did to the document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Click {
    Link {
        link: Link,
        new_tab: bool,
    },
    /// Tick or untick the task.
    Task(Task),
    /// Double-clicked this block, on this word as it shows on the page and
    /// how many times it's there before that. `None` is below every block.
    EditBlock {
        source: Option<std::ops::Range<usize>>,
        word: Option<(String, usize)>,
    },
    /// Put the page's cursor here, taking the selection there if `extend`.
    Caret {
        offset: usize,
        extend: bool,
    },
    /// Select the word around here.
    SelectWord(usize),
}

/// A block of the document being edited in place, as its markdown.
pub struct BlockEditor {
    pub id: Id,
    /// What it covers in the source, and what's there.
    pub range: std::ops::Range<usize>,
    pub original: String,
    pub draft: String,
    /// As it was last drawn, for the room to make and for clicks outside it.
    pub height: f32,
    pub rect: Rect,
    pub focus: bool,
    /// Put the cursor here, in chars.
    pub caret: Option<usize>,
}

pub struct Options<'a> {
    /// What local images and links are relative to.
    pub dir: &'a std::path::Path,
    pub syntax_theme: &'a str,
    pub code_fg: Color32,
    pub code_bg: Color32,
    pub allow_remote: bool,
    /// Tasks can be ticked.
    pub editable: bool,
    /// Go to this height next frame.
    pub scroll_to: Option<f32>,
    /// Keep to the width given, without margins, as in the preview pane.
    pub compact: bool,
    /// What to mark as found, and which of it is current.
    pub hits: Option<(&'a [Hit], Option<usize>)>,
    /// The cursor in the page, when its text is being edited.
    pub caret: Option<Caret>,
}

/// Where the cursor and selection are, in the source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Caret {
    pub anchor: usize,
    pub head: usize,
    /// It has no block to be in, and gets a line of its own here.
    pub gap: Option<usize>,
    /// What takes the keys while the page is being edited.
    pub focus: Id,
}

/// A document's parse, layout and what's being highlighted for it.
#[derive(Default)]
pub struct MdView {
    document: Arc<Document>,
    /// The text `document` was made from.
    parsed: Option<Arc<str>>,
    parsed_at: Option<Instant>,
    layout: Layout,
    inputs: Option<Inputs>,
    cache: GalleyCache,
    code_spans: CodeSpans,
    code_theme: String,
    code_generation: u64,
    highlighters: HashMap<u64, Highlighter>,
    /// Callouts folded the other way than they start.
    toggled: HashSet<usize>,
    fold_generation: u64,
    /// Scrolled to last frame, how far down, and how tall the view was.
    pub offset: f32,
    pub viewport_height: f32,
    /// Goes up with each new layout, for what's worked out from it.
    layout_generation: u64,
    stats: Stats,
    lightbox: Option<Lightbox>,
    /// The formulas and diagrams of the last layout.
    drawings: Vec<String>,
    atlas_fill: f32,
    atlas_generation: u64,
    /// The pointer is selecting in the page being edited.
    dragging_caret: bool,
    /// Where the page's top left was last drawn, on the screen.
    pub page_origin: Pos2,
    /// Where the button to open the link at the cursor is, if it's showing.
    pub open_link_rect: Option<Rect>,
}

/// A picture shown large, over everything.
struct Lightbox {
    key: String,
    zoom: f32,
    pan: Vec2,
}

impl MdView {
    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    pub fn document(&self) -> &Document {
        &self.document
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    pub fn layout_generation(&self) -> u64 {
        self.layout_generation
    }

    pub fn lightbox_is_open(&self) -> bool {
        self.lightbox.is_some()
    }

    /// Where to scroll to show a match, a little way down the view.
    pub fn offset_of_hit(&self, hit: &Hit) -> f32 {
        (self.layout.y_of_hit(hit) + MARGIN - self.viewport_height / 3.0).max(0.0)
    }

    /// Parses `text` if it isn't what's parsed already. A very long document
    /// isn't parsed on every keystroke: typing would lag.
    pub fn sync(&mut self, ctx: &Context, text: &str) {
        if self.parsed.as_deref() == Some(text) {
            return;
        }
        if text.len() > LONG_DOCUMENT
            && let Some(last) = self.parsed_at
            && last.elapsed() < LONG_DOCUMENT_PAUSE
        {
            ctx.request_repaint_after(LONG_DOCUMENT_PAUSE);
            return;
        }
        crate::theme::fonts::want(text);
        self.document = Arc::new(parse::parse(text));
        self.stats = self.document.stats();
        self.parsed = Some(Arc::from(text));
        self.parsed_at = Some(Instant::now());
        self.inputs = None;
    }

    /// Whether what's parsed and laid out is this text.
    pub fn is_current(&self, text: &str) -> bool {
        self.parsed.as_deref() == Some(text) && self.inputs.is_some()
    }

    /// Where to scroll so the cursor at this source offset shows, if it doesn't.
    pub fn offset_to_show(&self, text: &str, offset: usize) -> Option<f32> {
        let fields = self.layout.placed_fields();
        let refs: Vec<_> = fields.iter().map(|placed| placed.field).collect();
        let (index, _) = super::wysiwyg::field_at(&refs, offset)?;
        let rect = fields[index].cursor_rect(text, offset);
        let (top, bottom) = (rect.min.y + MARGIN, rect.max.y + MARGIN);
        if top < self.offset {
            Some((top - MARGIN).max(0.0))
        } else if bottom > self.offset + self.viewport_height {
            Some(bottom + MARGIN - self.viewport_height)
        } else {
            None
        }
    }

    /// Whether to show the bar offering to load remote images.
    pub fn has_remote_images(&self) -> bool {
        self.document.has_remote_images
    }

    fn update_code(&mut self, ctx: &Context, theme: &str) {
        if self.code_theme != theme {
            self.code_theme = theme.to_owned();
            self.code_spans.clear();
            self.highlighters.clear();
            self.code_generation += 1;
        }
        // In the browser, polling is what does the work.
        self.highlighters.retain(|key, highlighter| {
            let Some(result) = highlighter.poll() else {
                return true;
            };
            self.code_spans.insert(*key, result.lines);
            self.code_generation += 1;
            !result.done
        });
        if self.highlighters.len() >= MAX_HIGHLIGHTERS {
            return;
        }
        let mut wanted = Vec::new();
        collect_code(&self.document.blocks, &mut wanted);
        for footnote in &self.document.footnotes {
            collect_code(&footnote.blocks, &mut wanted);
        }
        let mut current = HashSet::new();
        for (lang, code) in wanted {
            let code = code.strip_suffix('\n').unwrap_or(code);
            let key = layout::code_key(lang, code);
            current.insert(key);
            if self.highlighters.len() >= MAX_HIGHLIGHTERS
                || self.code_spans.contains_key(&key)
                || self.highlighters.contains_key(&key)
                || code.len() > highlight::MAX_HIGHLIGHT_BYTES
            {
                continue;
            }
            let Some(syntax) = highlight::syntax_for_token(lang) else {
                continue;
            };
            let highlighter = Highlighter::new(ctx, syntax, theme);
            highlighter.update(1, Arc::from(code));
            self.highlighters.insert(key, highlighter);
        }
        // Blocks that were edited away; others may be typed into again.
        if self.code_spans.len() > current.len() + 64 {
            self.code_spans.retain(|key, _| current.contains(key));
        }
    }

    fn update_layout(
        &mut self,
        ui: &Ui,
        width: f32,
        options: &Options<'_>,
        images: &mut Images,
        editing: Option<(std::ops::Range<usize>, f32)>,
    ) {
        let ctx = ui.ctx();
        let (text_options, atlas_fill) =
            ctx.fonts(|fonts| (*fonts.options(), fonts.font_atlas_fill_ratio()));
        // It only ever fills up, until egui starts it over.
        if atlas_fill < self.atlas_fill {
            self.atlas_generation += 1;
        }
        self.atlas_fill = atlas_fill;
        let mut theme = Theme::new(ui.visuals(), options.code_fg, options.code_bg);
        if options.compact {
            theme.size = 13.0;
        }
        let inputs = Inputs {
            width: width.round(),
            theme,
            fonts: Fonts::of(ctx),
            pixels_per_point: ctx.pixels_per_point(),
            font_generation: crate::theme::fonts::generation(ctx),
            text_options,
            atlas_generation: self.atlas_generation,
            image_generation: images.generation,
            code_generation: self.code_generation,
            fold_generation: self.fold_generation,
            allow_remote: options.allow_remote,
            editing,
            wysiwyg: options.caret.is_some(),
            gap: options.caret.and_then(|caret| caret.gap),
        };
        if self.inputs.as_ref() == Some(&inputs) {
            return;
        }
        // Glyphs of other fonts or scales aren't in the atlas anymore.
        if self.inputs.as_ref().is_none_or(|old| {
            old.font_generation != inputs.font_generation
                || old.pixels_per_point != inputs.pixels_per_point
                || old.text_options != inputs.text_options
                || old.atlas_generation != inputs.atlas_generation
        }) {
            self.cache.clear();
        }
        let mut sizes = Sizes {
            images,
            dir: options.dir,
            allow_remote: options.allow_remote,
        };
        self.layout = layout::layout(
            &self.document,
            Params {
                ctx,
                inputs: &inputs,
                images: &mut sizes,
                cache: &mut self.cache,
                code_spans: &self.code_spans,
                toggled: &self.toggled,
                previous_drawings: &self.drawings,
                source: self.parsed.as_deref(),
            },
        );
        // What was being typed a moment ago needn't be drawn anymore.
        for key in &self.drawings {
            if !self.layout.drawings.contains(key) {
                images.cancel(key);
            }
        }
        self.drawings.clone_from(&self.layout.drawings);
        self.inputs = Some(inputs);
        self.layout_generation += 1;
    }

    /// Fills the space given with the document, scrolling within it.
    pub fn show(
        &mut self,
        ui: &mut Ui,
        id: Id,
        options: &Options<'_>,
        images: &mut Images,
        mut editor: Option<&mut BlockEditor>,
    ) -> Vec<Click> {
        self.update_code(ui.ctx(), options.syntax_theme);
        let available = ui.available_width();
        let scroll_bar = ui.style().spacing.scroll.allocated_width() + 2.0;
        let page = (available - MARGIN * 2.0 - scroll_bar)
            .min(Theme::new(ui.visuals(), options.code_fg, options.code_bg).max_width)
            .max(40.0);
        let editing = editor.as_ref().map(|e| (e.range.clone(), e.height));
        self.update_layout(ui, page, options, images, editing);
        let height = self.layout.height + MARGIN * 2.0;
        let mut clicks = Vec::new();
        let mut opened = None;
        let mut page_origin = Pos2::ZERO;
        let mut scroll = ScrollArea::vertical()
            .id_salt(id.with("scroll"))
            .auto_shrink(false)
            .scroll_source(crate::touch::scroll_source(ui.ctx()));
        if let Some(offset) = options.scroll_to {
            scroll = scroll.vertical_scroll_offset(offset);
        }
        let output = scroll.show_viewport(ui, |ui, viewport| {
            ui.set_min_size(vec2(available - scroll_bar, height));
            let x = ((available - scroll_bar - page) / 2.0).max(MARGIN);
            let origin = ui.max_rect().min + vec2(x, MARGIN);
            let visible = viewport.translate(ui.max_rect().min.to_vec2());
            page_origin = origin;
            paint(
                ui,
                &self.layout.items,
                origin,
                visible,
                id,
                None,
                &mut Painting {
                    images,
                    dir: options.dir,
                    allow_remote: options.allow_remote,
                    editable: options.editable,
                    clicks: &mut clicks,
                    toggled: &mut self.toggled,
                    fold_generation: &mut self.fold_generation,
                    hits: options.hits,
                    opened: Some(&mut opened),
                    editor: editor.as_deref_mut(),
                    caret: options.caret.map(|caret| PaintedCaret {
                        caret,
                        on: blink_on(ui.ctx()),
                        source: self.parsed.as_deref().unwrap_or_default(),
                        field: self
                            .layout
                            .placed_fields()
                            .iter()
                            .map(|p| p.field)
                            .collect(),
                    }),
                },
            );
        });
        self.offset = output.state.offset.y;
        self.viewport_height = output.inner_rect.height();
        self.page_origin = page_origin;
        if let Some(caret) = options.caret {
            clicks.extend(self.caret_pointer(ui, output.inner_rect, page_origin));
            take_keys(
                ui,
                output.inner_rect,
                page_origin,
                caret,
                &self.layout,
                self.parsed.as_deref(),
            );
            // A click takes the keys from what it isn't on, and the text on the
            // page is on top of what has them.
            let clicked_page = ui.input(|i| {
                i.pointer.any_click()
                    && i.pointer
                        .interact_pos()
                        .is_some_and(|pos| output.inner_rect.contains(pos))
            });
            if clicked_page {
                ui.memory_mut(|memory| memory.request_focus(caret.focus));
            }
            clicks.extend(self.open_link_button(ui, id, page_origin, caret, output.inner_rect));
        } else if options.editable && editor.is_none() && !self.lightbox_is_open() {
            clicks.extend(self.double_clicked(ui, output.inner_rect, page_origin));
        }
        if let Some(key) = opened {
            self.lightbox = Some(Lightbox {
                key,
                zoom: 1.0,
                pan: Vec2::ZERO,
            });
        }
        self.show_lightbox(ui.ctx(), id, images);
        clicks
    }

    /// Shows the whole document in the space given, for something that
    /// scrolls around it.
    pub fn show_inline(
        &mut self,
        ui: &mut Ui,
        id: Id,
        options: &Options<'_>,
        images: &mut Images,
    ) -> Vec<Click> {
        self.update_code(ui.ctx(), options.syntax_theme);
        let width = ui.available_width().max(40.0);
        self.update_layout(ui, width, options, images, None);
        let (rect, _) = ui.allocate_exact_size(vec2(width, self.layout.height), Sense::hover());
        let mut clicks = Vec::new();
        paint(
            ui,
            &self.layout.items,
            rect.min,
            ui.clip_rect(),
            id,
            None,
            &mut Painting {
                images,
                dir: options.dir,
                allow_remote: options.allow_remote,
                editable: options.editable,
                clicks: &mut clicks,
                toggled: &mut self.toggled,
                fold_generation: &mut self.fold_generation,
                hits: options.hits,
                opened: None,
                editor: None,
                caret: None,
            },
        );
        clicks
    }

    /// A double-click on the document, for editing: not on links and the like,
    /// and not when something's in front of it.
    fn double_clicked(&self, ui: &Ui, viewport: Rect, origin: Pos2) -> Option<Click> {
        let pos = ui.input(|i| {
            i.pointer
                .button_double_clicked(egui::PointerButton::Primary)
                .then(|| i.pointer.interact_pos())
                .flatten()
        })?;
        let in_front = ui
            .ctx()
            .layer_id_at(pos)
            .is_some_and(|layer| layer != ui.layer_id());
        if !viewport.contains(pos) || in_front {
            return None;
        }
        let on_page = pos - origin.to_vec2();
        if self.layout.is_control_at(on_page) {
            return None;
        }
        Some(Click::EditBlock {
            source: self.layout.block_at(on_page.y).cloned(),
            word: self.layout.word_at(on_page),
        })
    }

    /// With no Ctrl to hold on a touchscreen, a link the cursor is in gets a
    /// button to open it, by the cursor.
    fn open_link_button(
        &mut self,
        ui: &Ui,
        id: Id,
        origin: Pos2,
        caret: Caret,
        viewport: Rect,
    ) -> Option<Click> {
        self.open_link_rect = None;
        if !crate::touch::is_touch(ui.ctx()) || caret.anchor != caret.head {
            return None;
        }
        let source = self.parsed.as_deref()?;
        let (link, cursor) = self.layout.link_at_offset(source, caret.head)?;
        let cursor = cursor.translate(origin.to_vec2());
        let link = link.clone();
        // Above the cursor, or below when there's no room.
        let height = crate::touch::TOUCH_SIZE;
        let above = cursor.min.y - height - 6.0;
        let y = if above >= viewport.min.y {
            above
        } else {
            cursor.max.y + 6.0
        };
        let area = egui::Area::new(id.with("open-link"))
            .order(egui::Order::Foreground)
            .fixed_pos(pos2(cursor.min.x.max(viewport.min.x + 4.0), y))
            .constrain(true)
            .show(ui.ctx(), |ui| {
                ui.set_max_width(viewport.width() - 8.0);
                egui::Frame::popup(ui.style())
                    .show(ui, |ui| {
                        let label = format!("{} Open", icon::ARROW_SQUARE_OUT);
                        ui.button(label).on_hover_text(&link.url).clicked()
                    })
                    .inner
            });
        self.open_link_rect = Some(area.response.rect);
        area.inner.then_some(Click::Link {
            link,
            new_tab: false,
        })
    }

    /// What the pointer does to the cursor in the page: pressing puts it there,
    /// dragging selects, and a double-click selects a word. A double-click on
    /// what can't be edited on the page opens it as its markdown.
    fn caret_pointer(&mut self, ui: &Ui, viewport: Rect, origin: Pos2) -> Option<Click> {
        let touch = crate::touch::is_touch(ui.ctx());
        let (pressed, down, pos, double, shift, command) = ui.input(|i| {
            let pointer = &i.pointer;
            (
                // On a touchscreen a touch that moves is a scroll.
                if touch {
                    pointer.primary_clicked()
                } else {
                    pointer.primary_pressed()
                },
                pointer.primary_down(),
                pointer.interact_pos(),
                pointer.button_double_clicked(egui::PointerButton::Primary),
                i.modifiers.shift,
                i.modifiers.command,
            )
        });
        if !down {
            self.dragging_caret = false;
        }
        let pos = pos?;
        let on_page = pos - origin.to_vec2();
        if self.dragging_caret && !touch {
            let offset = self.layout.offset_at_pos(on_page)?;
            return Some(Click::Caret {
                offset,
                extend: true,
            });
        }
        let in_front = ui
            .ctx()
            .layer_id_at(pos)
            .is_some_and(|layer| layer != ui.layer_id());
        // The scroll bar is at the right of the viewport.
        let over_page = viewport.contains(pos) && pos.x < viewport.max.x - 12.0;
        if in_front || !over_page || self.layout.is_button_at(on_page) {
            return None;
        }
        // Following a link takes Ctrl; ticking a task is what it always is.
        let on_link = self.layout.is_control_at(on_page) && !self.layout.is_widget_at(on_page);
        if self.layout.is_widget_at(on_page) || (on_link && command) {
            return None;
        }
        let on_text = self
            .layout
            .placed_fields()
            .iter()
            .any(|placed| placed.rect().contains(on_page));
        if double {
            self.dragging_caret = false;
            if on_text {
                return Some(Click::SelectWord(self.layout.offset_at_pos(on_page)?));
            }
            return self
                .layout
                .block_at(on_page.y)
                .cloned()
                .map(|source| Click::EditBlock {
                    source: Some(source),
                    word: None,
                });
        }
        if !pressed {
            return None;
        }
        self.dragging_caret = !touch;
        Some(Click::Caret {
            offset: self.layout.offset_at_pos(on_page)?,
            extend: shift,
        })
    }

    /// Shows the picture opened, if there is one: the wheel or a pinch zooms,
    /// dragging moves it, and a click or Escape closes it.
    fn show_lightbox(&mut self, ctx: &Context, id: Id, images: &mut Images) {
        let Some(lightbox) = &mut self.lightbox else {
            return;
        };
        let size = images.size(&lightbox.key);
        let (Some(size), Some(texture)) = (size, images.texture(&lightbox.key).map(|t| t.id()))
        else {
            self.lightbox = None;
            return;
        };
        let screen = ctx.content_rect();
        let mut close = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
        egui::Area::new(id.with("lightbox"))
            .order(egui::Order::Foreground)
            .fixed_pos(screen.min)
            .show(ctx, |ui| {
                let response = ui.allocate_response(screen.size(), Sense::click_and_drag());
                let (scroll, pinch) = ui.input(|i| (i.smooth_scroll_delta.y, i.zoom_delta()));
                let wheel = (scroll * 0.002).exp();
                lightbox.zoom = (lightbox.zoom * wheel * pinch).clamp(0.1, 16.0);
                lightbox.pan += response.drag_delta();
                close |= response.clicked();
                let painter = ui.painter();
                painter.rect_filled(screen, 0.0, Color32::from_black_alpha(220));
                let margin = 24.0;
                let room = screen.shrink(margin).size();
                // A drawing is here to be read, and has the pixels for twice its
                // size; a picture isn't blown up past its own.
                let most = if is_drawing(&lightbox.key) { 2.0 } else { 1.0 };
                let fit = (room.x / size.x).min(room.y / size.y).min(most);
                let rect = Rect::from_center_size(
                    screen.center() + lightbox.pan,
                    size * fit * lightbox.zoom,
                );
                painter.image(
                    texture,
                    rect,
                    Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                    Color32::WHITE,
                );
                painter.text(
                    pos2(screen.max.x - margin, screen.min.y + margin),
                    egui::Align2::RIGHT_TOP,
                    icon::X,
                    egui::FontId::proportional(20.0),
                    Color32::WHITE,
                );
            });
        // The wheel is for the picture, not what's under it.
        ctx.input_mut(|i| i.smooth_scroll_delta = Vec2::ZERO);
        if close {
            self.lightbox = None;
        }
    }

    /// How far down an anchor is, for scrolling to it.
    pub fn anchor_offset(&self, anchor: &str) -> Option<f32> {
        let key = anchor.strip_prefix('#').unwrap_or(anchor);
        self.layout
            .anchors
            .get(key)
            .or_else(|| self.layout.anchors.get(&parse::slug(key)))
            .map(|y| (y + MARGIN - 8.0).max(0.0))
    }

    /// The height in the scrolling view that shows this source offset.
    pub fn offset_of_source(&self, source: usize) -> f32 {
        self.layout.y_of_source(source) + MARGIN
    }

    pub fn source_of_offset(&self, offset: f32) -> usize {
        self.layout.source_of_y((offset - MARGIN).max(0.0))
    }

    pub fn content_height(&self) -> f32 {
        self.layout.height + MARGIN * 2.0
    }
}

fn collect_code<'a>(blocks: &'a [parse::Block], out: &mut Vec<(&'a str, &'a str)>) {
    for block in blocks {
        match &block.kind {
            BlockKind::Code {
                lang: Some(lang),
                code,
            } => out.push((lang, code)),
            BlockKind::Quote { children, .. } => collect_code(children, out),
            BlockKind::List { items, .. } => {
                for item in items {
                    collect_code(&item.children, out);
                }
            }
            BlockKind::Definitions(definitions) => {
                for definition in definitions {
                    for detail in &definition.details {
                        collect_code(detail, out);
                    }
                }
            }
            _ => {}
        }
    }
}

/// Image sizes for the layout, from the store.
struct Sizes<'a> {
    images: &'a mut Images,
    dir: &'a std::path::Path,
    allow_remote: bool,
}

impl ImageSizes for Sizes<'_> {
    fn size(&mut self, url: &str) -> Option<Vec2> {
        let key = resolve(url, self.dir, self.allow_remote)?;
        self.images.size(&key)
    }

    fn ready_size(&self, key: &str) -> Option<Vec2> {
        self.images.ready_size(key)
    }

    fn depth(&self, key: &str) -> f32 {
        self.images.depth(key)
    }

    fn error(&self, key: &str) -> Option<String> {
        self.images.error(key).map(str::to_owned)
    }
}

struct Painting<'a> {
    images: &'a mut Images,
    dir: &'a std::path::Path,
    allow_remote: bool,
    editable: bool,
    clicks: &'a mut Vec<Click>,
    toggled: &'a mut HashSet<usize>,
    fold_generation: &'a mut u64,
    hits: Option<(&'a [Hit], Option<usize>)>,
    /// Takes the picture clicked on, where they can be opened.
    opened: Option<&'a mut Option<String>>,
    editor: Option<&'a mut BlockEditor>,
    caret: Option<PaintedCaret<'a>>,
}

/// The cursor in the page, for drawing.
struct PaintedCaret<'a> {
    caret: Caret,
    /// It blinks.
    on: bool,
    source: &'a str,
    field: Vec<&'a Editable>,
}

/// Blinks with egui's own timing, and asks to be drawn when that changes.
fn blink_on(ctx: &Context) -> bool {
    const ON: f64 = 0.8;
    const OFF: f64 = 0.4;
    let time = ctx.input(|i| i.time) % (ON + OFF);
    let (on, left) = if time < ON {
        (true, ON - time)
    } else {
        (false, ON + OFF - time)
    };
    ctx.request_repaint_after(std::time::Duration::from_secs_f64(left));
    on
}

/// Keeps the keys for the page, and tells the system where text would go,
/// which is what brings up an on-screen keyboard.
fn take_keys(
    ui: &Ui,
    viewport: Rect,
    origin: Pos2,
    caret: Caret,
    layout: &Layout,
    source: Option<&str>,
) {
    let response = ui.interact(viewport, caret.focus, Sense::focusable_noninteractive());
    if !response.has_focus() {
        return;
    }
    ui.memory_mut(|memory| {
        memory.set_focus_lock_filter(
            caret.focus,
            egui::EventFilter {
                tab: true,
                horizontal_arrows: true,
                vertical_arrows: true,
                escape: false,
            },
        );
    });
    let fields = layout.placed_fields();
    let refs: Vec<&Editable> = fields.iter().map(|placed| placed.field).collect();
    let at = field_at(&refs, caret.head).map(|(index, _)| &fields[index]);
    let cursor = at.map_or(viewport, |placed| {
        placed
            .cursor_rect(source.unwrap_or_default(), caret.head)
            .translate(origin.to_vec2())
    });
    ui.ctx().output_mut(|output| {
        output.mutable_text_under_cursor = true;
        output.ime = Some(egui::output::IMEOutput {
            rect: viewport,
            cursor_rect: cursor,
            purpose: Default::default(),
            should_interrupt_composition: false,
        });
    });
}

/// Draws the items on screen. While text is selected, all of it goes through
/// egui's selection, which drops the selection when it can't see its ends.
fn paint(
    ui: &mut Ui,
    items: &[Item],
    origin: Pos2,
    visible: Rect,
    id: Id,
    outer: Option<usize>,
    painting: &mut Painting<'_>,
) {
    let selecting = ui
        .ctx()
        .plugin::<egui::text_selection::LabelSelectionState>()
        .lock()
        .has_selection();
    let local_visible = visible.translate(-origin.to_vec2());
    for (index, item) in items.iter().enumerate() {
        let on_screen = item.rect().intersects(local_visible);
        let item_id = id.with(index);
        match item {
            // Even off screen: it has the keyboard's focus to keep.
            Item::Editor { rect } => {
                if let Some(editor) = painting.editor.as_deref_mut() {
                    paint_editor(ui, rect.translate(origin.to_vec2()), editor);
                }
            }
            Item::Text(text) if on_screen || selecting => {
                paint_text(ui, text, origin, item_id, on_screen, painting);
                if on_screen {
                    let key = outer.map_or((index, None), |outer| (outer, Some(index)));
                    paint_hits(ui, text, origin, key, painting);
                }
            }
            Item::Sideways {
                rect,
                content_width,
                items,
            } if on_screen || selecting => {
                let rect = rect.translate(origin.to_vec2());
                let source = crate::touch::scroll_source(ui.ctx());
                ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
                    ScrollArea::horizontal()
                        .id_salt(item_id)
                        .scroll_source(source)
                        .auto_shrink(false)
                        .show_viewport(ui, |ui, viewport| {
                            ui.set_min_size(vec2(*content_width, rect.height()));
                            let inner = ui.max_rect().min;
                            let visible = viewport.translate(inner.to_vec2());
                            paint(ui, items, inner, visible, item_id, Some(index), painting);
                        });
                });
            }
            _ if !on_screen => {}
            Item::Text(_) | Item::Sideways { .. } => {}
            Item::Marker { pos, galley } => {
                ui.painter().galley(
                    *pos + origin.to_vec2(),
                    galley.clone(),
                    Color32::PLACEHOLDER,
                );
            }
            Item::Fill {
                rect,
                color,
                radius,
            } => {
                ui.painter().rect_filled(
                    rect.translate(origin.to_vec2()),
                    CornerRadius::same(*radius as u8),
                    *color,
                );
            }
            Item::Stroke {
                rect,
                stroke,
                radius,
            } => {
                ui.painter().rect_stroke(
                    rect.translate(origin.to_vec2()),
                    CornerRadius::same(*radius as u8),
                    *stroke,
                    StrokeKind::Inside,
                );
            }
            Item::Line { from, to, stroke } => {
                let by = origin.to_vec2();
                ui.painter().line_segment([*from + by, *to + by], *stroke);
            }
            Item::Dot {
                center,
                radius,
                color,
                filled,
            } => {
                let center = *center + origin.to_vec2();
                if *filled {
                    ui.painter().circle_filled(center, *radius, *color);
                } else {
                    ui.painter()
                        .circle_stroke(center, *radius, Stroke::new(1.0, *color));
                }
            }
            Item::Square { rect, color } => {
                ui.painter()
                    .rect_filled(rect.translate(origin.to_vec2()), 0.0, *color);
            }
            Item::Widget { rect, widget, on } => {
                paint_widget(ui, *rect, widget, *on, origin, item_id, painting);
            }
            Item::Picture { rect, key } => {
                if let Some(texture) = painting.images.texture(key) {
                    ui.painter().image(
                        texture.id(),
                        snapped(ui, rect.translate(origin.to_vec2())),
                        Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                        Color32::WHITE,
                    );
                }
            }
            Item::Enlarge { rect, block, key } => {
                let by = origin.to_vec2();
                if ui.rect_contains_pointer(block.translate(by)) {
                    let button = ui.put(
                        rect.translate(by),
                        egui::Button::new(egui_phosphor::regular::ARROWS_OUT),
                    );
                    if button.on_hover_text("Enlarge").clicked()
                        && let Some(opened) = &mut painting.opened
                    {
                        **opened = Some(key.clone());
                    }
                }
            }
            Item::CopyCode { rect, block, code } => {
                let block = block.translate(origin.to_vec2());
                if ui.rect_contains_pointer(block) {
                    let rect = rect.translate(origin.to_vec2());
                    let button = ui.put(rect, egui::Button::new(icon::COPY).frame(true));
                    if button.on_hover_text("Copy").clicked() {
                        ui.ctx().copy_text(code.clone());
                    }
                }
            }
        }
    }
}

/// The markdown of the block being edited, in its place on the page.
fn paint_editor(ui: &mut Ui, rect: Rect, editor: &mut BlockEditor) {
    if let Some(caret) = editor.caret.take() {
        let mut state =
            egui::text_edit::TextEditState::load(ui.ctx(), editor.id).unwrap_or_default();
        let cursor = egui::text::CCursor::new(caret);
        state
            .cursor
            .set_char_range(Some(egui::text::CCursorRange::one(cursor)));
        state.store(ui.ctx(), editor.id);
    }
    let output = ui
        .scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
            egui::TextEdit::multiline(&mut editor.draft)
                .id(editor.id)
                .code_editor()
                .desired_width(rect.width())
                .desired_rows(1)
                .margin(egui::Margin::same(8))
                .show(ui)
        })
        .inner;
    if std::mem::take(&mut editor.focus) {
        output.response.request_focus();
    }
    editor.rect = output.response.rect;
    let height = output.response.rect.height().round();
    if (height - editor.height).abs() >= 1.0 {
        editor.height = height;
        ui.ctx().request_repaint();
    }
}

/// On the screen's pixels: a formula drawn at exactly their size stays sharp.
fn snapped(ui: &Ui, rect: Rect) -> Rect {
    use egui::emath::GuiRounding as _;
    let min = rect.min.round_to_pixels(ui.ctx().pixels_per_point());
    Rect::from_min_size(min, rect.size())
}

/// Marks what was found in this text.
fn paint_hits(
    ui: &Ui,
    text: &TextItem,
    origin: Pos2,
    key: (usize, Option<usize>),
    painting: &Painting<'_>,
) {
    let Some((hits, current)) = painting.hits else {
        return;
    };
    let first = hits.partition_point(|hit| hit.item < key);
    let pos = text.pos + origin.to_vec2();
    let fill = ui.visuals().selection.bg_fill.gamma_multiply(0.6);
    let stroke = Stroke::new(1.5, ui.visuals().strong_text_color());
    for (index, hit) in hits.iter().enumerate().skip(first) {
        if hit.item != key {
            break;
        }
        for rect in row_rects(&text.galley, &hit.chars) {
            let rect = rect.translate(pos.to_vec2());
            ui.painter().rect_filled(rect, 2.0, fill);
            if Some(index) == current {
                ui.painter()
                    .rect_stroke(rect.expand(1.0), 2.0, stroke, StrokeKind::Outside);
            }
        }
    }
}

fn paint_text(
    ui: &mut Ui,
    text: &TextItem,
    origin: Pos2,
    id: Id,
    on_screen: bool,
    painting: &mut Painting<'_>,
) {
    let pos = text.pos + origin.to_vec2();
    let rect = text.galley.rect.translate(pos.to_vec2());
    let touch = crate::touch::is_touch(ui.ctx());
    // Dragging scrolls on a touchscreen, so text can't be selected that way.
    let sense = if touch {
        Sense::click()
    } else {
        Sense::click_and_drag()
    };
    let response = ui.interact(rect, id, sense);
    let editing = painting.caret.is_some();
    let command = ui.input(|i| i.modifiers.command);
    // While the page is being edited, a click there puts the cursor there.
    // When a finger lifts there's no pointer to hover: a tap is where it was.
    let pointer = response.hover_pos().or_else(|| {
        response
            .clicked()
            .then(|| response.interact_pointer_pos())
            .flatten()
    });
    let hovered_link = pointer
        .filter(|_| !editing || command)
        .and_then(|pointer| text.link_at(pointer - origin.to_vec2()));
    if on_screen {
        paint_selection(ui, text, pos, painting);
    }
    if editing && text.editable.is_some() && response.hovered() && hovered_link.is_none() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::Text);
    }
    let mut image_under = None;
    if on_screen {
        for image in &text.images {
            let Some(key) = resolve(&image.url, painting.dir, painting.allow_remote) else {
                continue;
            };
            let Some(texture) = painting.images.texture(&key) else {
                continue;
            };
            let image_rect = snapped(ui, image.rect.translate(pos.to_vec2()));
            ui.painter().image(
                texture.id(),
                image_rect,
                Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                Color32::WHITE,
            );
            // A formula is part of the line: it has nothing more to show. And
            // while editing, a click on a picture is a place for the cursor.
            let under = pointer.is_some_and(|pointer| image_rect.contains(pointer));
            if under && !is_drawing(&key) && !editing {
                image_under = Some(key);
                if !image.tip.is_empty() {
                    response.clone().on_hover_text(&image.tip);
                }
            }
        }
    }
    if touch || editing {
        ui.painter()
            .galley(pos, text.galley.clone(), Color32::PLACEHOLDER);
        if on_screen {
            paint_caret(ui, text, origin, painting);
        }
    } else {
        egui::text_selection::LabelSelectionState::label_text_selection(
            ui,
            &response,
            pos,
            text.galley.clone(),
            Color32::PLACEHOLDER,
            Stroke::NONE,
        );
    }
    let Some(link) = hovered_link else {
        // A picture that's a link goes there instead.
        if let (Some(key), Some(opened)) = (image_under, &mut painting.opened) {
            ui.ctx().set_cursor_icon(egui::CursorIcon::ZoomIn);
            if response.clicked() {
                **opened = Some(key);
            }
        }
        return;
    };
    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    let tip = if link.title.is_empty() {
        link.url.as_str()
    } else {
        link.title.as_str()
    };
    let response = response.on_hover_text_at_pointer(tip);
    if response.clicked() {
        let new_tab = ui.input(|i| i.modifiers.command);
        painting.clicks.push(Click::Link {
            link: link.clone(),
            new_tab,
        });
    } else if response.middle_clicked() {
        painting.clicks.push(Click::Link {
            link: link.clone(),
            new_tab: true,
        });
    }
}

/// What's selected in this text, behind it.
fn paint_selection(ui: &Ui, text: &TextItem, pos: Pos2, painting: &Painting<'_>) {
    let (Some(painted), Some(field)) = (&painting.caret, &text.editable) else {
        return;
    };
    let (anchor, head) = (painted.caret.anchor, painted.caret.head);
    if anchor == head {
        return;
    }
    let (min, max) = (anchor.min(head), anchor.max(head));
    if max < field.low() || min > field.high() {
        return;
    }
    let chars = field.char_at(min)..field.char_at(max);
    if chars.is_empty() {
        return;
    }
    let fill = ui.visuals().selection.bg_fill;
    for rect in row_rects(&text.galley, &chars) {
        ui.painter()
            .rect_filled(rect.translate(pos.to_vec2()), 0.0, fill);
    }
}

/// The cursor, if it's in this text.
fn paint_caret(ui: &Ui, text: &TextItem, origin: Pos2, painting: &Painting<'_>) {
    let (Some(painted), Some(field)) = (&painting.caret, &text.editable) else {
        return;
    };
    let head = painted.caret.head;
    let Some((_, at)) = field_at(&painted.field, head) else {
        return;
    };
    if !std::ptr::eq(at, field) || !painted.on {
        return;
    }
    if !ui.memory(|memory| memory.has_focus(painted.caret.focus)) {
        return;
    }
    let placed = PlacedField {
        text,
        field,
        origin: origin.to_vec2(),
    };
    let rect = placed.cursor_rect(painted.source, head);
    let stroke = ui.visuals().text_cursor.stroke;
    ui.painter().vline(rect.min.x, rect.y_range(), stroke);
}

fn paint_widget(
    ui: &mut Ui,
    rect: Rect,
    widget: &Widget,
    on: bool,
    origin: Pos2,
    id: Id,
    painting: &mut Painting<'_>,
) {
    let rect = rect.translate(origin.to_vec2());
    match widget {
        Widget::Task(task) => {
            let sense = if painting.editable {
                Sense::click()
            } else {
                Sense::hover()
            };
            let response = ui.interact(rect, id, sense);
            let visuals = ui.visuals();
            let color = if response.hovered() {
                visuals.hyperlink_color
            } else {
                visuals.weak_text_color()
            };
            let boxed = rect.shrink(1.0);
            if on {
                ui.painter()
                    .rect_filled(boxed, 3.0, visuals.hyperlink_color);
                let tick = [
                    boxed.lerp_inside(vec2(0.22, 0.52)),
                    boxed.lerp_inside(vec2(0.42, 0.72)),
                    boxed.lerp_inside(vec2(0.78, 0.28)),
                ];
                let stroke = Stroke::new(1.8, visuals.extreme_bg_color);
                ui.painter().add(Shape::line(tick.to_vec(), stroke));
            } else {
                ui.painter()
                    .rect_stroke(boxed, 3.0, Stroke::new(1.2, color), StrokeKind::Inside);
            }
            if painting.editable && response.clicked() {
                painting.clicks.push(Click::Task(task.clone()));
            }
            if painting.editable && response.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
        }
        Widget::Fold(source) => {
            let response = ui.interact(rect, id, Sense::click());
            let glyph = if on {
                icon::CARET_DOWN
            } else {
                icon::CARET_RIGHT
            };
            ui.painter().text(
                pos2(rect.max.x - 10.0, rect.center().y),
                egui::Align2::RIGHT_CENTER,
                glyph,
                egui::FontId::proportional(14.0),
                ui.visuals().weak_text_color(),
            );
            if response.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            if response.clicked() {
                if !painting.toggled.remove(source) {
                    painting.toggled.insert(*source);
                }
                *painting.fold_generation += 1;
            }
        }
    }
}

/// The new text after ticking or unticking a task: only its mark changes.
pub fn toggle_task(text: &str, task: &Task) -> Option<(std::ops::Range<usize>, &'static str)> {
    let current = text.get(task.source.clone())?;
    let (is_task, checked) = match current {
        "[ ]" => (true, false),
        "[x]" | "[X]" => (true, true),
        _ => (false, false),
    };
    is_task.then(|| (task.source.clone(), if checked { "[ ]" } else { "[x]" }))
}

/// The folder a document's relative links are from.
pub fn base_dir(path: &std::path::Path) -> PathBuf {
    path.parent().map_or_else(PathBuf::new, PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs a pass laying the document out, returning how many times it has been.
    fn pass(ctx: &Context, view: &mut MdView, images: &mut Images, dark: bool) -> u64 {
        let options = Options {
            dir: std::path::Path::new("/"),
            syntax_theme: "",
            code_fg: Color32::WHITE,
            code_bg: Color32::BLACK,
            allow_remote: false,
            editable: false,
            scroll_to: None,
            compact: false,
            hits: None,
            caret: None,
        };
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            // As a click in the menus does it: before the page, in the same pass.
            let visuals = if dark {
                egui::Visuals::dark()
            } else {
                egui::Visuals::light()
            };
            ui.ctx().set_visuals(visuals.clone());
            *ui.visuals_mut() = visuals;
            view.update_layout(ui, 400.0, &options, images, None);
        });
        output.textures_delta.clear();
        view.layout_generation
    }

    /// egui starts its font atlas over the pass after the theme changes, by
    /// which time the page is laid out with the new colors and the old atlas.
    /// Kept, those lines draw from the wrong places: garbled.
    #[test]
    fn the_page_is_laid_out_again_when_the_font_atlas_starts_over() {
        let ctx = Context::default();
        let mut view = MdView {
            document: std::sync::Arc::new(parse::parse("Some text")),
            ..MdView::default()
        };
        let mut images = Images::default();
        let mut laid_out = 0;
        for _ in 0..3 {
            laid_out = pass(&ctx, &mut view, &mut images, true);
        }
        assert_eq!(
            pass(&ctx, &mut view, &mut images, true),
            laid_out,
            "settled"
        );

        let colors_changed = pass(&ctx, &mut view, &mut images, false);
        assert!(colors_changed > laid_out);
        let mut last = colors_changed;
        let mut later = 0;
        for _ in 0..3 {
            let now = pass(&ctx, &mut view, &mut images, false);
            later += now - last;
            last = now;
        }
        assert!(later >= 1, "laid out again once the atlas had started over");
        assert_eq!(
            pass(&ctx, &mut view, &mut images, false),
            last,
            "and settles"
        );
    }

    #[test]
    fn a_font_atlas_that_shrinks_has_started_over() {
        let ctx = Context::default();
        let mut view = MdView {
            document: std::sync::Arc::new(parse::parse("Some text")),
            ..MdView::default()
        };
        let mut images = Images::default();
        let mut laid_out = 0;
        for _ in 0..3 {
            laid_out = pass(&ctx, &mut view, &mut images, true);
        }
        view.atlas_fill = 2.0;
        assert!(pass(&ctx, &mut view, &mut images, true) > laid_out);
    }

    #[test]
    fn toggling_a_task_changes_only_its_mark() {
        let text = "- [ ] one\n- [x] two\n- [X] three\n";
        let doc = parse::parse(text);
        let BlockKind::List { items, .. } = &doc.blocks[0].kind else {
            panic!("expected a list");
        };
        let splice = |index: usize| {
            let task = items[index].task.as_ref().unwrap();
            let (range, mark) = toggle_task(text, task).unwrap();
            let mut new = text.to_owned();
            new.replace_range(range, mark);
            new
        };
        assert_eq!(splice(0), "- [x] one\n- [x] two\n- [X] three\n");
        assert_eq!(splice(1), "- [ ] one\n- [ ] two\n- [X] three\n");
        assert_eq!(splice(2), "- [ ] one\n- [x] two\n- [ ] three\n");
    }

    #[test]
    fn a_stale_task_does_nothing() {
        let task = Task {
            checked: false,
            source: 2..5,
        };
        assert_eq!(toggle_task("no task here", &task), None);
        assert_eq!(toggle_task("x", &task), None);
    }
}
