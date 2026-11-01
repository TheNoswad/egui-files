//! Charts that split each folder's share among its contents, one level per
//! ring or column: the Filelight-style sunburst and the icicle (the same
//! thing unrolled into columns, which leaves room for names).

use std::f32::consts::{FRAC_PI_2, TAU};

use egui::{
    Color32, FontId, Mesh, Pos2, Rect, Sense, Shape, Stroke, StrokeKind, Ui, Vec2,
    epaint::TextShape, pos2, vec2,
};

use super::{
    ChartOutput, LayoutKey,
    colors::{self, Painter},
    scan::{Metric, Tree},
};

/// A node's share of the focused folder, as a span of 0..1.
#[derive(Clone, Copy, Debug)]
pub struct Span {
    pub id: u32,
    /// 1 for the focused folder's contents.
    pub depth: u16,
    pub start: f64,
    pub end: f64,
}

impl Span {
    fn contains(&self, at: f64) -> bool {
        self.start <= at && at < self.end
    }
}

/// Spans for `depth` levels below `focus`, leaving out ones narrower than
/// `min_span`. Parents come before their children.
pub fn partition(tree: &Tree, focus: u32, metric: Metric, depth: u16, min_span: f64) -> Vec<Span> {
    let mut spans = Vec::new();
    let total = tree.node(focus).size(metric) as f64;
    if total > 0.0 {
        split(
            tree,
            focus,
            metric,
            0.0,
            1.0 / total,
            1,
            depth,
            min_span,
            &mut spans,
        );
    }
    spans
}

#[allow(clippy::too_many_arguments)]
fn split(
    tree: &Tree,
    id: u32,
    metric: Metric,
    start: f64,
    scale: f64,
    depth: u16,
    max_depth: u16,
    min_span: f64,
    out: &mut Vec<Span>,
) {
    let mut at = start;
    for child in tree.sorted_children(id, metric) {
        let span = tree.node(child).size(metric) as f64 * scale;
        if span < min_span {
            break;
        }
        out.push(Span {
            id: child,
            depth,
            start: at,
            end: at + span,
        });
        if depth < max_depth && tree.node(child).is_dir() {
            split(
                tree,
                child,
                metric,
                at,
                scale,
                depth + 1,
                max_depth,
                min_span,
                out,
            );
        }
        at += span;
    }
}

#[derive(Default)]
pub struct Cache {
    pub(super) key: Option<LayoutKey>,
    pub(super) spans: Vec<Span>,
}

fn hit(spans: &[Span], depth: u16, at: f64) -> Option<u32> {
    spans
        .iter()
        .find(|s| s.depth == depth && s.contains(at))
        .map(|s| s.id)
}

/// Lighter further out, so rings stay apart where colors repeat.
fn shade(color: Color32, depth: u16) -> Color32 {
    color.lerp_to_gamma(
        Color32::WHITE,
        (depth.saturating_sub(1) as f32 * 0.07).min(0.35),
    )
}

/// Text rotated by `angle`, centered on `center`.
fn rotated_text(ui: &Ui, center: Pos2, galley: std::sync::Arc<egui::Galley>, angle: f32) -> Shape {
    let half = galley.size() / 2.0;
    let (sin, cos) = angle.sin_cos();
    let offset = vec2(half.x * cos - half.y * sin, half.x * sin + half.y * cos);
    let _ = ui;
    TextShape::new(center - offset, galley, Color32::PLACEHOLDER)
        .with_angle(angle)
        .into()
}

fn annulus(
    mesh: &mut Mesh,
    center: Pos2,
    inner: f32,
    outer: f32,
    a0: f32,
    a1: f32,
    color: Color32,
) {
    let steps = (((a1 - a0) / 0.03).ceil() as usize).clamp(1, 240);
    let base = mesh.vertices.len() as u32;
    for k in 0..=steps {
        let a = a0 + (a1 - a0) * k as f32 / steps as f32;
        let dir = Vec2::angled(a);
        mesh.colored_vertex(center + dir * inner, color);
        mesh.colored_vertex(center + dir * outer, color);
    }
    for k in 0..steps as u32 {
        let i = base + 2 * k;
        mesh.add_triangle(i, i + 1, i + 3);
        mesh.add_triangle(i, i + 3, i + 2);
    }
}

pub fn sunburst(
    ui: &mut Ui,
    cache: &mut Cache,
    painter: &Painter,
    focus: u32,
    selected: Option<u32>,
    linked_hover: Option<u32>,
) -> ChartOutput {
    let tree = painter.tree;
    let (rect, response) = ui.allocate_exact_size(ui.available_size(), Sense::click());
    let center = rect.center();
    let radius = (rect.width().min(rect.height()) / 2.0 - 6.0).max(20.0);
    let hole = (radius * 0.2).max(36.0).min(radius * 0.5);
    let depth = (((radius - hole) / 34.0).floor() as u16).clamp(1, 8);
    let ring = (radius - hole) / depth as f32;
    // The icicle shares the cache, with other spans.
    let key = LayoutKey::new(tree, focus, painter.metric, rect).with(1);
    if super::refresh(&mut cache.key, key, ui.ctx()) {
        // About a third of a degree.
        cache.spans = partition(tree, focus, painter.metric, depth, 0.001);
    }
    let angle = |at: f64| at as f32 * TAU - FRAC_PI_2;
    let radii = |depth: u16| {
        let inner = hole + ring * (depth - 1) as f32;
        (inner + 1.0, inner + ring - 1.0)
    };

    let p = ui.painter_at(rect);
    let mut mesh = Mesh::default();
    for span in &cache.spans {
        let (inner, outer) = radii(span.depth);
        // A pixel apart along the middle of the ring.
        let gap = (0.7 / ((inner + outer) / 2.0)).min((span.end - span.start) as f32 * TAU / 4.0);
        let color = shade(painter.color(span.id), span.depth);
        annulus(
            &mut mesh,
            center,
            inner,
            outer,
            angle(span.start) + gap,
            angle(span.end) - gap,
            color,
        );
    }
    p.add(mesh);
    let folder_fill = painter
        .palette
        .folder
        .lerp_to_gamma(painter.color(focus), 0.25);
    p.circle_filled(center, hole - 1.0, folder_fill);

    // Names along the rings where they fit.
    let font = FontId::proportional(11.0);
    let mut shapes = Vec::new();
    for span in &cache.spans {
        let (inner, outer) = radii(span.depth);
        let middle = (inner + outer) / 2.0;
        let sweep = (span.end - span.start) as f32 * TAU;
        let room = sweep * middle - 6.0;
        if room < 28.0 || outer - inner < 13.0 {
            continue;
        }
        let fill = shade(painter.color(span.id), span.depth);
        let text = colors::text_on(fill);
        let galley = super::elided(&p, tree.name(span.id), font.clone(), text, room);
        let mid = (angle(span.start) + angle(span.end)) / 2.0;
        let pos = center + Vec2::angled(mid) * middle;
        // Along the ring, never upside down.
        let mut rotation = mid + FRAC_PI_2;
        if mid.rem_euclid(TAU) < TAU / 2.0 && mid.rem_euclid(TAU) > 0.0 {
            rotation -= TAU / 2.0;
        }
        let shape = rotated_text(ui, pos, galley, rotation);
        if let Shape::Text(mut text_shape) = shape {
            text_shape.fallback_color = text;
            shapes.push(Shape::Text(text_shape));
        }
    }
    p.extend(shapes);
    let ink = ui.visuals().strong_text_color();
    let focus_name = super::elided(&p, tree.name(focus), font.clone(), ink, hole * 1.7);
    let size = p.layout_no_wrap(
        crate::fs::human_size(tree.node(focus).size(painter.metric)),
        font,
        ui.visuals().weak_text_color(),
    );
    let h = focus_name.size().y;
    p.galley(center - vec2(focus_name.size().x / 2.0, h), focus_name, ink);
    p.galley(center - vec2(size.size().x / 2.0, 0.0), size, ink);

    let mut on_center = false;
    let hovered = response.hover_pos().and_then(|pos| {
        let offset = pos - center;
        let distance = offset.length();
        if distance < hole {
            on_center = true;
            return None;
        }
        let depth = ((distance - hole) / ring) as u16 + 1;
        let at = ((offset.angle() + FRAC_PI_2).rem_euclid(TAU) / TAU) as f64;
        hit(&cache.spans, depth, at)
    });
    let outline = |id: Option<u32>, stroke: Stroke| {
        for span in cache.spans.iter().filter(|s| Some(s.id) == id) {
            let (inner, outer) = radii(span.depth);
            let (a0, a1) = (angle(span.start), angle(span.end));
            let steps = (((a1 - a0) / 0.03).ceil() as usize).clamp(1, 240);
            let mut points: Vec<Pos2> = (0..=steps)
                .map(|k| center + Vec2::angled(a0 + (a1 - a0) * k as f32 / steps as f32) * outer)
                .collect();
            points.extend(
                (0..=steps).rev().map(|k| {
                    center + Vec2::angled(a0 + (a1 - a0) * k as f32 / steps as f32) * inner
                }),
            );
            p.add(Shape::closed_line(points, stroke));
        }
    };
    outline(
        selected,
        Stroke::new(2.5, ui.visuals().selection.stroke.color),
    );
    outline(hovered.or(linked_hover), Stroke::new(1.5, ink));
    if on_center {
        p.circle_stroke(center, hole - 1.0, Stroke::new(1.5, ink));
    }
    ChartOutput::new(ui, response, hovered, on_center)
}

pub fn icicle(
    ui: &mut Ui,
    cache: &mut Cache,
    painter: &Painter,
    focus: u32,
    selected: Option<u32>,
    linked_hover: Option<u32>,
) -> ChartOutput {
    let tree = painter.tree;
    let (rect, response) = ui.allocate_exact_size(ui.available_size(), Sense::click());
    // The focused folder gets a narrow column on the left; clicking it goes up.
    let first = 26.0;
    let depth = (((rect.width() - first) / 130.0).floor() as u16).clamp(1, 10);
    let column = (rect.width() - first) / depth as f32;
    let key = LayoutKey::new(tree, focus, painter.metric, rect).with(2);
    if super::refresh(&mut cache.key, key, ui.ctx()) {
        let min_span = 1.0 / rect.height().max(1.0) as f64;
        cache.spans = partition(tree, focus, painter.metric, depth, min_span);
    }
    let span_rect = |span: &Span| {
        let x = rect.left() + first + column * (span.depth - 1) as f32;
        let y0 = rect.top() + span.start as f32 * rect.height();
        let y1 = rect.top() + span.end as f32 * rect.height();
        Rect::from_min_max(pos2(x, y0), pos2(x + column, y1))
    };

    let p = ui.painter_at(rect);
    let focus_rect = Rect::from_min_size(rect.min, vec2(first, rect.height()));
    let focus_fill = painter
        .palette
        .folder
        .lerp_to_gamma(painter.color(focus), 0.25);
    p.rect_filled(focus_rect.shrink(0.5), 2.0, focus_fill);
    let mut mesh = Mesh::default();
    for span in &cache.spans {
        let r = span_rect(span);
        let r = if r.height() >= 3.0 {
            r.shrink2(vec2(1.0, 0.5))
        } else {
            r.shrink2(vec2(1.0, 0.0))
        };
        mesh.add_colored_rect(r, painter.color(span.id));
    }
    p.add(mesh);

    let font = FontId::proportional(11.0);
    let line = ui.fonts_mut(|f| f.row_height(&font));
    for span in &cache.spans {
        let r = span_rect(span);
        if r.height() < line + 2.0 {
            continue;
        }
        let fill = painter.color(span.id);
        let text = colors::text_on(fill);
        let name = super::elided(&p, tree.name(span.id), font.clone(), text, column - 10.0);
        let two_lines = r.height() >= 2.0 * line + 4.0;
        let top = if two_lines {
            r.center().y - line
        } else {
            r.center().y - line / 2.0
        };
        p.galley(pos2(r.left() + 5.0, top), name, text);
        if two_lines {
            let size = crate::fs::human_size(tree.node(span.id).size(painter.metric));
            p.text(
                pos2(r.left() + 5.0, top + line),
                egui::Align2::LEFT_TOP,
                size,
                font.clone(),
                text.gamma_multiply(0.8),
            );
        }
    }
    // The focused folder's name, up its column.
    let ink = ui.visuals().strong_text_color();
    let name = super::elided(&p, tree.name(focus), font, ink, rect.height() - 12.0);
    let pos = focus_rect.center();
    if let Shape::Text(mut text) = rotated_text(ui, pos, name, -FRAC_PI_2) {
        text.fallback_color = ink;
        p.add(Shape::Text(text));
    }

    let mut on_focus = false;
    let hovered = response.hover_pos().and_then(|pos| {
        if pos.x < rect.left() + first {
            on_focus = true;
            return None;
        }
        let depth = ((pos.x - rect.left() - first) / column) as u16 + 1;
        let at = ((pos.y - rect.top()) / rect.height()) as f64;
        hit(&cache.spans, depth, at)
    });
    let outline = |id: Option<u32>, stroke: Stroke| {
        for span in cache.spans.iter().filter(|s| Some(s.id) == id) {
            p.rect_stroke(span_rect(span).shrink(0.5), 0.0, stroke, StrokeKind::Inside);
        }
    };
    outline(
        selected,
        Stroke::new(2.5, ui.visuals().selection.stroke.color),
    );
    outline(hovered.or(linked_hover), Stroke::new(1.5, ink));
    if on_focus {
        p.rect_stroke(
            focus_rect.shrink(0.5),
            2.0,
            Stroke::new(1.5, ink),
            StrokeKind::Inside,
        );
    }
    ChartOutput::new(ui, response, hovered, on_focus)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spans_nest_inside_their_parents() {
        let spans = [
            Span {
                id: 1,
                depth: 1,
                start: 0.0,
                end: 0.6,
            },
            Span {
                id: 2,
                depth: 2,
                start: 0.0,
                end: 0.4,
            },
            Span {
                id: 3,
                depth: 1,
                start: 0.6,
                end: 1.0,
            },
        ];
        assert_eq!(hit(&spans, 1, 0.5), Some(1));
        assert_eq!(hit(&spans, 2, 0.3), Some(2));
        assert_eq!(hit(&spans, 2, 0.5), None);
        assert_eq!(hit(&spans, 1, 0.6), Some(3));
    }
}
