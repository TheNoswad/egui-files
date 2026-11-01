//! WizTree-style treemap: every file is a rectangle sized by the space it
//! takes, nested inside its folders, laid out with the squarified algorithm
//! (Bruls, Huizing, van Wijk) and shaded like a cushion.

use egui::{Color32, FontId, Mesh, Pos2, Rect, Sense, Stroke, StrokeKind, Ui, pos2, vec2};

use super::{
    ChartOutput, LayoutKey,
    colors::{self, Painter},
    scan::{Metric, Tree},
};

/// Gap around a folder's contents.
const PAD: f32 = 2.0;
/// Smallest rectangle side worth laying out.
const MIN_SIDE: f32 = 2.0;
/// Rectangles smaller than this stay part of their folder's fill.
const MIN_AREA: f64 = 6.0;
/// The most rectangles laid out, so huge trees stay quick to draw.
const BUDGET: usize = 30_000;

pub struct Item {
    pub rect: Rect,
    pub id: u32,
    pub depth: u16,
    /// A folder with room for its name along the top.
    pub header: bool,
}

#[derive(Default)]
pub struct Cache {
    key: Option<LayoutKey>,
    pub(super) items: Vec<Item>,
}

/// Splits `rect` into rectangles with the given areas, keeping them as
/// close to squares as it can. `areas` should be biggest first.
pub fn squarify(areas: &[f64], rect: Rect) -> Vec<Rect> {
    let worst = |min: f64, max: f64, sum: f64, side: f64| {
        let side2 = side * side;
        (side2 * max / (sum * sum)).max(sum * sum / (side2 * min))
    };
    let mut out = Vec::with_capacity(areas.len());
    let mut r = rect;
    let mut i = 0;
    while i < areas.len() {
        if areas[i] <= 0.0 || r.width() <= 0.0 || r.height() <= 0.0 {
            out.push(Rect::NOTHING);
            i += 1;
            continue;
        }
        let side = r.width().min(r.height()) as f64;
        let (mut min, mut max, mut sum) = (areas[i], areas[i], areas[i]);
        let mut ratio = worst(min, max, sum, side);
        let mut end = i + 1;
        while end < areas.len() && areas[end] > 0.0 {
            let (min2, max2, sum2) = (min.min(areas[end]), max.max(areas[end]), sum + areas[end]);
            let ratio2 = worst(min2, max2, sum2, side);
            if ratio2 > ratio {
                break;
            }
            (min, max, sum, ratio) = (min2, max2, sum2, ratio2);
            end += 1;
        }
        let thickness = (sum / side) as f32;
        if r.width() >= r.height() {
            // A column down the left side.
            let mut y = r.min.y;
            for &area in &areas[i..end] {
                let height = (area / thickness as f64) as f32;
                out.push(Rect::from_min_size(
                    pos2(r.min.x, y),
                    vec2(thickness, height),
                ));
                y += height;
            }
            r.min.x += thickness;
        } else {
            // A row along the top.
            let mut x = r.min.x;
            for &area in &areas[i..end] {
                let width = (area / thickness as f64) as f32;
                out.push(Rect::from_min_size(
                    pos2(x, r.min.y),
                    vec2(width, thickness),
                ));
                x += width;
            }
            r.min.y += thickness;
        }
        i = end;
    }
    out
}

pub fn layout(tree: &Tree, focus: u32, rect: Rect, metric: Metric, header: f32) -> Vec<Item> {
    let mut items = Vec::new();
    place(tree, focus, rect, 0, metric, header, &mut items);
    items
}

fn place(
    tree: &Tree,
    id: u32,
    rect: Rect,
    depth: u16,
    metric: Metric,
    header_height: f32,
    out: &mut Vec<Item>,
) {
    let node = tree.node(id);
    let header =
        depth > 0 && node.is_dir() && rect.height() > header_height * 2.5 && rect.width() > 50.0;
    out.push(Item {
        rect,
        id,
        depth,
        header,
    });
    if !node.is_dir() || out.len() >= BUDGET {
        return;
    }
    let mut inner = if depth == 0 { rect } else { rect.shrink(PAD) };
    if header {
        inner.min.y = rect.min.y + header_height;
    }
    let total = node.size(metric) as f64;
    if inner.width() < MIN_SIDE || inner.height() < MIN_SIDE || total <= 0.0 {
        return;
    }
    let scale = inner.area() as f64 / total;
    let mut shown = Vec::new();
    let mut areas = Vec::new();
    for child in tree.sorted_children(id, metric) {
        let area = tree.node(child).size(metric) as f64 * scale;
        if area < MIN_AREA {
            break;
        }
        shown.push(child);
        areas.push(area);
    }
    // What's left (the small ones, and space not yet counted in a running
    // scan) goes last and stays the folder's color.
    let rest = inner.area() as f64 - areas.iter().sum::<f64>();
    if rest > 0.0 {
        areas.push(rest);
    }
    let rects = squarify(&areas, inner);
    for (child, rect) in shown.into_iter().zip(rects) {
        if rect.width() >= MIN_SIDE && rect.height() >= MIN_SIDE {
            place(tree, child, rect, depth + 1, metric, header_height, out);
        }
    }
}

/// Adds a rectangle lit from its middle, darker toward its corners.
fn cushion(mesh: &mut Mesh, rect: Rect, color: Color32) {
    if rect.width() < 5.0 || rect.height() < 5.0 {
        mesh.add_colored_rect(rect, color);
        return;
    }
    let light = color.lerp_to_gamma(Color32::WHITE, 0.25);
    let dark = color.lerp_to_gamma(Color32::BLACK, 0.25);
    let base = mesh.vertices.len() as u32;
    let center = rect.center() - rect.size() * 0.08;
    mesh.colored_vertex(center, light);
    for corner in [
        rect.left_top(),
        rect.right_top(),
        rect.right_bottom(),
        rect.left_bottom(),
    ] {
        mesh.colored_vertex(corner, dark);
    }
    for k in 0..4 {
        mesh.add_triangle(base, base + 1 + k, base + 1 + (k + 1) % 4);
    }
}

pub fn show(
    ui: &mut Ui,
    cache: &mut Cache,
    painter: &Painter,
    focus: u32,
    selected: Option<u32>,
    linked_hover: Option<u32>,
) -> ChartOutput {
    let tree = painter.tree;
    let (rect, response) = ui.allocate_exact_size(ui.available_size(), Sense::click());
    let font = FontId::proportional(11.0);
    let header = ui.fonts_mut(|f| f.row_height(&font)) + 3.0;
    let key = LayoutKey::new(tree, focus, painter.metric, rect);
    if super::refresh(&mut cache.key, key, ui.ctx()) {
        cache.items = layout(tree, focus, rect, painter.metric, header);
    }

    let p = ui.painter_at(rect);
    p.rect_filled(rect, 0.0, painter.palette.folder);
    let mut mesh = Mesh::default();
    let mut labels = Vec::new();
    for item in &cache.items {
        if item.depth == 0 {
            continue;
        }
        let node = tree.node(item.id);
        let color = painter.color(item.id);
        if node.is_dir() {
            let fill = painter.palette.folder.lerp_to_gamma(color, 0.3);
            mesh.add_colored_rect(item.rect, fill.lerp_to_gamma(Color32::BLACK, 0.12));
            mesh.add_colored_rect(item.rect.shrink(1.0), fill);
            if item.header {
                labels.push((item, fill));
            }
        } else {
            cushion(&mut mesh, item.rect.shrink(0.5), color);
            if item.rect.width() > 44.0 && item.rect.height() > header {
                labels.push((item, color));
            }
        }
    }
    p.add(mesh);
    for (item, fill) in labels {
        let text_color = colors::text_on(fill);
        let width = item.rect.width() - 6.0;
        let name = super::elided(&p, tree.name(item.id), font.clone(), text_color, width);
        let pos = item.rect.left_top() + vec2(3.0, 1.5);
        p.galley(pos, name, text_color);
        let size_fits = !item.header && item.rect.height() > header * 2.2;
        if item.header || size_fits {
            let size = crate::fs::human_size(tree.node(item.id).size(painter.metric));
            let galley = p.layout_no_wrap(size, font.clone(), text_color.gamma_multiply(0.8));
            let pos = if item.header {
                pos2(item.rect.right() - galley.size().x - 3.0, pos.y)
            } else {
                pos + vec2(0.0, header - 2.0)
            };
            if item.header && galley.size().x + 60.0 > width {
                continue;
            }
            p.galley(pos, galley, text_color);
        }
    }

    let pointer = response.hover_pos();
    let hovered = pointer.and_then(|pos| hit(&cache.items, pos));
    let outline = |id: Option<u32>, stroke: Stroke| {
        if let Some(item) = id.and_then(|id| cache.items.iter().find(|i| i.id == id)) {
            p.rect_stroke(item.rect.shrink(0.5), 0.0, stroke, StrokeKind::Inside);
        }
    };
    let ink = ui.visuals().strong_text_color();
    outline(
        selected,
        Stroke::new(2.5, ui.visuals().selection.stroke.color),
    );
    outline(selected, Stroke::new(1.0, ui.visuals().extreme_bg_color));
    outline(hovered.or(linked_hover), Stroke::new(1.5, ink));

    ChartOutput::new(ui, response, hovered, false)
}

fn hit(items: &[Item], pos: Pos2) -> Option<u32> {
    // Children come after their folders, so the last match is the deepest.
    items
        .iter()
        .rev()
        .find(|i| i.depth > 0 && i.rect.contains(pos))
        .map(|i| i.id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn squarify_fills_the_rectangle() {
        let rect = Rect::from_min_size(Pos2::ZERO, vec2(600.0, 400.0));
        let areas = [6.0, 6.0, 4.0, 3.0, 2.0, 2.0, 1.0].map(|a| a * 240_000.0 / 24.0);
        let rects = squarify(&areas, rect);
        assert_eq!(rects.len(), areas.len());
        for (r, a) in rects.iter().zip(areas) {
            assert!((r.area() as f64 - a).abs() < 1.0, "{r:?} should be {a}");
            assert!(rect.expand(0.01).contains_rect(*r));
            // Squarified: no long slivers for these sizes.
            assert!(r.aspect_ratio().max(1.0 / r.aspect_ratio()) < 3.0, "{r:?}");
        }
        // Paper's example: 6, 6 go in a column first.
        assert_eq!(rects[0].min, Pos2::ZERO);
        assert!((rects[1].min.x).abs() < 0.01);
    }
}
