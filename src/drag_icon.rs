//! Renders the drag label into a pixel buffer, so the platform can show it
//! while a drag is outside the window (the app can only draw inside it).
//!
//! The label is laid out and tessellated by egui like any other shape, then
//! rasterized on the CPU against egui's font atlas: it matches the in-app
//! label exactly, fonts and theme included.

use egui::{
    ClippedPrimitive, Color32, Context, Pos2, Rect, TextStyle, epaint::ClippedShape,
    epaint::Primitive, epaint::Shadow,
};

/// A premultiplied ARGB image (bytes in B, G, R, A order, Wayland's
/// `argb8888`).
pub struct DragIcon {
    pub width: u32,
    pub height: u32,
    /// Physical pixels per logical pixel.
    pub scale: i32,
    pub bgra: Vec<u8>,
}

/// Renders `text` in a popup frame like the in-app drag label.
pub fn render(ctx: &Context, text: String) -> Option<DragIcon> {
    let style = ctx.global_style();
    let mut frame = egui::Frame::popup(&style);
    frame.shadow = Shadow::NONE;
    let font = TextStyle::Body.resolve(&style);
    let color = style.visuals.text_color();
    let galley = ctx.fonts_mut(|f| f.layout_no_wrap(text, font, color));

    let content = Rect::from_min_size(Pos2::ZERO, galley.size());
    let shift = frame.widget_rect(content).min.to_vec2();
    let content = content.translate(-shift);
    let bounds = frame.widget_rect(content);
    let shapes = vec![
        frame.paint(content),
        egui::Shape::galley(content.min, galley, color),
    ];

    // The compositor takes integer buffer scales; points are zoomed too.
    let scale = ctx.native_pixels_per_point().unwrap_or(1.0).ceil().max(1.0);
    let ppp = scale * ctx.zoom_factor();
    let width = (bounds.width() * ppp).ceil() as u32;
    let height = (bounds.height() * ppp).ceil() as u32;
    if width == 0 || height == 0 || width > 4096 || height > 4096 {
        return None;
    }

    let clipped = shapes
        .into_iter()
        .map(|shape| ClippedShape {
            clip_rect: Rect::EVERYTHING,
            shape,
        })
        .collect();
    let primitives = ctx.tessellate(clipped, ppp);
    let atlas = ctx.fonts(|f| f.image());

    let mut pixels = vec![Color32::TRANSPARENT; (width * height) as usize];
    for ClippedPrimitive { primitive, .. } in primitives {
        let Primitive::Mesh(mesh) = primitive else {
            continue;
        };
        for tri in mesh.indices.as_chunks::<3>().0 {
            let v = tri.map(|i| mesh.vertices[i as usize]);
            fill_triangle(&mut pixels, width, height, ppp, &v, &atlas);
        }
    }

    let bgra = pixels
        .iter()
        .flat_map(|c| [c.b(), c.g(), c.r(), c.a()])
        .collect();
    Some(DragIcon {
        width,
        height,
        scale: scale as i32,
        bgra,
    })
}

fn fill_triangle(
    pixels: &mut [Color32],
    width: u32,
    height: u32,
    ppp: f32,
    v: &[egui::epaint::Vertex; 3],
    atlas: &egui::ColorImage,
) {
    let p = v.map(|v| v.pos * ppp);
    let edge = |a: Pos2, b: Pos2, c: Pos2| (b.x - a.x) * (c.y - a.y) - (b.y - a.y) * (c.x - a.x);
    let area = edge(p[0], p[1], p[2]);
    if area.abs() < f32::EPSILON {
        return;
    }
    let min_x = p
        .iter()
        .map(|p| p.x)
        .fold(f32::INFINITY, f32::min)
        .floor()
        .max(0.0) as u32;
    let min_y = p
        .iter()
        .map(|p| p.y)
        .fold(f32::INFINITY, f32::min)
        .floor()
        .max(0.0) as u32;
    let max_x = (p.iter().map(|p| p.x).fold(0.0, f32::max).ceil() as u32).min(width);
    let max_y = (p.iter().map(|p| p.y).fold(0.0, f32::max).ceil() as u32).min(height);
    let colors = v.map(|v| v.color.to_array().map(f32::from));

    for y in min_y..max_y {
        for x in min_x..max_x {
            let c = Pos2::new(x as f32 + 0.5, y as f32 + 0.5);
            // Barycentric weights; all the same sign as `area` means inside.
            let w = [
                edge(p[1], p[2], c) / area,
                edge(p[2], p[0], c) / area,
                edge(p[0], p[1], c) / area,
            ];
            if w.iter().any(|w| *w < 0.0) {
                continue;
            }
            let uv = v[0].uv.to_vec2() * w[0] + v[1].uv.to_vec2() * w[1] + v[2].uv.to_vec2() * w[2];
            let tex = sample(atlas, uv.x, uv.y);
            let src: [f32; 4] = std::array::from_fn(|i| {
                let vc = colors[0][i] * w[0] + colors[1][i] * w[1] + colors[2][i] * w[2];
                vc * tex[i] / 255.0
            });
            // Premultiplied "over".
            let dst = &mut pixels[(y * width + x) as usize];
            let keep = 1.0 - src[3] / 255.0;
            let out: [u8; 4] = std::array::from_fn(|i| {
                (src[i] + f32::from(dst.to_array()[i]) * keep)
                    .round()
                    .clamp(0.0, 255.0) as u8
            });
            *dst = Color32::from_rgba_premultiplied(out[0], out[1], out[2], out[3]);
        }
    }
}

/// Bilinear sample of the atlas at normalized coordinates.
fn sample(atlas: &egui::ColorImage, u: f32, v: f32) -> [f32; 4] {
    let [w, h] = atlas.size;
    let x = (u * w as f32 - 0.5).clamp(0.0, (w - 1) as f32);
    let y = (v * h as f32 - 0.5).clamp(0.0, (h - 1) as f32);
    let (x0, y0) = (x.floor() as usize, y.floor() as usize);
    let (x1, y1) = ((x0 + 1).min(w - 1), (y0 + 1).min(h - 1));
    let (fx, fy) = (x - x0 as f32, y - y0 as f32);
    let px = |x: usize, y: usize| atlas.pixels[y * w + x].to_array().map(f32::from);
    let (a, b, c, d) = (px(x0, y0), px(x1, y0), px(x0, y1), px(x1, y1));
    std::array::from_fn(|i| {
        let top = a[i] + (b[i] - a[i]) * fx;
        let bottom = c[i] + (d[i] - c[i]) * fx;
        top + (bottom - top) * fy
    })
}

