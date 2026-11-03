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
    images::resolve,
    layout::{
        self, CodeSpans, Fonts, GalleyCache, ImageSizes, Inputs, Item, Layout, Params, TextItem,
        Theme, Widget,
    },
    parse::{self, BlockKind, Document, Link, Task},
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
}

impl MdView {
    pub fn layout(&self) -> &Layout {
        &self.layout
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
        self.parsed = Some(Arc::from(text));
        self.parsed_at = Some(Instant::now());
        self.inputs = None;
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

    fn update_layout(&mut self, ui: &Ui, width: f32, options: &Options<'_>, images: &mut Images) {
        let ctx = ui.ctx();
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
            image_generation: images.generation,
            code_generation: self.code_generation,
            fold_generation: self.fold_generation,
            allow_remote: options.allow_remote,
        };
        if self.inputs.as_ref() == Some(&inputs) {
            return;
        }
        // Glyphs of other fonts or scales aren't in the atlas anymore.
        if self.inputs.as_ref().is_none_or(|old| {
            old.font_generation != inputs.font_generation
                || old.pixels_per_point != inputs.pixels_per_point
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
            },
        );
        self.inputs = Some(inputs);
    }

    /// Fills the space given with the document, scrolling within it.
    pub fn show(
        &mut self,
        ui: &mut Ui,
        id: Id,
        options: &Options<'_>,
        images: &mut Images,
    ) -> Vec<Click> {
        self.update_code(ui.ctx(), options.syntax_theme);
        let available = ui.available_width();
        let scroll_bar = ui.style().spacing.scroll.allocated_width() + 2.0;
        let page = (available - MARGIN * 2.0 - scroll_bar)
            .min(Theme::new(ui.visuals(), options.code_fg, options.code_bg).max_width)
            .max(40.0);
        self.update_layout(ui, page, options, images);
        let height = self.layout.height + MARGIN * 2.0;
        let mut clicks = Vec::new();
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
            paint(
                ui,
                &self.layout.items,
                origin,
                visible,
                id,
                &mut Painting {
                    images,
                    dir: options.dir,
                    allow_remote: options.allow_remote,
                    editable: options.editable,
                    clicks: &mut clicks,
                    toggled: &mut self.toggled,
                    fold_generation: &mut self.fold_generation,
                },
            );
        });
        self.offset = output.state.offset.y;
        self.viewport_height = output.inner_rect.height();
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
        self.update_layout(ui, width, options, images);
        let (rect, _) = ui.allocate_exact_size(vec2(width, self.layout.height), Sense::hover());
        let mut clicks = Vec::new();
        paint(
            ui,
            &self.layout.items,
            rect.min,
            ui.clip_rect(),
            id,
            &mut Painting {
                images,
                dir: options.dir,
                allow_remote: options.allow_remote,
                editable: options.editable,
                clicks: &mut clicks,
                toggled: &mut self.toggled,
                fold_generation: &mut self.fold_generation,
            },
        );
        clicks
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
}

struct Painting<'a> {
    images: &'a mut Images,
    dir: &'a std::path::Path,
    allow_remote: bool,
    editable: bool,
    clicks: &'a mut Vec<Click>,
    toggled: &'a mut HashSet<usize>,
    fold_generation: &'a mut u64,
}

/// Draws the items on screen. While text is selected, all of it goes through
/// egui's selection, which drops the selection when it can't see its ends.
fn paint(
    ui: &mut Ui,
    items: &[Item],
    origin: Pos2,
    visible: Rect,
    id: Id,
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
            Item::Text(text) if on_screen || selecting => {
                paint_text(ui, text, origin, item_id, on_screen, painting);
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
                            paint(ui, items, inner, visible, item_id, painting);
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
    let hovered_link = response
        .hover_pos()
        .and_then(|pointer| text.link_at(pointer - origin.to_vec2()));
    if on_screen {
        for image in &text.images {
            let Some(key) = resolve(&image.url, painting.dir, painting.allow_remote) else {
                continue;
            };
            let Some(texture) = painting.images.texture(&key) else {
                continue;
            };
            let image_rect = image.rect.translate(pos.to_vec2());
            ui.painter().image(
                texture.id(),
                image_rect,
                Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                Color32::WHITE,
            );
            if !image.tip.is_empty() && ui.rect_contains_pointer(image_rect) {
                response.clone().on_hover_text(&image.tip);
            }
        }
    }
    if touch {
        ui.painter()
            .galley(pos, text.galley.clone(), Color32::PLACEHOLDER);
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
