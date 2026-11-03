//! System fonts for the UI and monospace text.

use std::sync::{Arc, OnceLock};

use egui::{FontData, FontDefinitions, FontFamily};

/// The font families of rendered documents, which have real bold and italic.
pub const DOC_REGULAR: &str = "doc-regular";
pub const DOC_BOLD: &str = "doc-bold";
pub const DOC_ITALIC: &str = "doc-italic";
pub const DOC_BOLD_ITALIC: &str = "doc-bold-italic";

pub const INTER_REGULAR: &[u8] = include_bytes!("../../assets/fonts/inter/Inter-Regular.ttf");
pub const INTER_BOLD: &[u8] = include_bytes!("../../assets/fonts/inter/Inter-Bold.ttf");
const INTER_ITALIC: &[u8] = include_bytes!("../../assets/fonts/inter/Inter-Italic.ttf");
const INTER_BOLD_ITALIC: &[u8] = include_bytes!("../../assets/fonts/inter/Inter-BoldItalic.ttf");

/// Goes up with each change of fonts: what's laid out with the old ones,
/// and kept, is laid out again.
pub fn generation(ctx: &egui::Context) -> u64 {
    ctx.data(|d| d.get_temp(generation_id())).unwrap_or(0)
}

fn generation_id() -> egui::Id {
    egui::Id::new("font-generation")
}

fn set_fonts(ctx: &egui::Context, fonts: FontDefinitions) {
    ctx.set_fonts(fonts);
    let next = generation(ctx) + 1;
    ctx.data_mut(|d| d.insert_temp(generation_id(), next));
}

/// The installed font families, found on a background thread since scanning
/// the system's fonts takes a moment.
#[derive(Default)]
pub struct SystemFonts {
    db: Arc<OnceLock<fontdb::Database>>,
    started: bool,
    /// The choice installed into egui: `(ui, mono)`.
    installed: Option<(Option<String>, Option<String>)>,
    families: Option<Families>,
}

/// Family names, sorted.
pub struct Families {
    pub all: Vec<String>,
    pub mono: Vec<String>,
}

impl SystemFonts {
    fn start(&mut self, ctx: &egui::Context) {
        if std::mem::replace(&mut self.started, true) {
            return;
        }
        let db = self.db.clone();
        let ctx = ctx.clone();
        let spawned = std::thread::Builder::new()
            .name("system-fonts".into())
            .spawn(move || {
                let mut fonts = fontdb::Database::new();
                fonts.load_system_fonts();
                let _ = db.set(fonts);
                ctx.request_repaint();
            });
        if let Err(err) = spawned {
            eprintln!("system fonts: {err}");
        }
    }

    /// The families to choose from, once they've been found.
    pub fn families(&mut self, ctx: &egui::Context) -> Option<&Families> {
        self.start(ctx);
        if self.families.is_none() {
            let db = self.db.get()?;
            let mut all = Vec::new();
            let mut mono = Vec::new();
            for face in db.faces() {
                let Some((family, _)) = face.families.first() else {
                    continue;
                };
                all.push(family.clone());
                if face.monospaced {
                    mono.push(family.clone());
                }
            }
            for list in [&mut all, &mut mono] {
                list.sort_by_key(|f| f.to_lowercase());
                list.dedup();
            }
            self.families = Some(Families { all, mono });
        }
        self.families.as_ref()
    }

    /// Makes egui use these fonts; `None` keeps egui's own.
    pub fn install(&mut self, ctx: &egui::Context, ui: Option<&str>, mono: Option<&str>) {
        let wanted = (ui.map(str::to_owned), mono.map(str::to_owned));
        if self.installed.as_ref() == Some(&wanted) {
            return;
        }
        if wanted.0.is_some() || wanted.1.is_some() {
            self.start(ctx);
            // Keep egui's fonts until the system's have been found.
            let Some(db) = self.db.get() else { return };
            set_fonts(ctx, definitions(db, ui, mono));
        } else if self.installed.is_some() {
            set_fonts(ctx, definitions(&fontdb::Database::new(), None, None));
        }
        self.installed = Some(wanted);
    }
}

/// egui's fonts and the icon font, with `ui` and `mono` put first.
pub fn definitions(db: &fontdb::Database, ui: Option<&str>, mono: Option<&str>) -> FontDefinitions {
    let mut fonts = FontDefinitions::default();
    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
    for (family, name) in [
        (FontFamily::Proportional, ui),
        (FontFamily::Monospace, mono),
    ] {
        let Some(name) = name else { continue };
        let Some(data) = load(db, name) else {
            eprintln!("font {name:?} isn't installed");
            continue;
        };
        let key = format!("system:{name}");
        fonts.font_data.insert(key.clone(), Arc::new(data));
        fonts.families.entry(family).or_default().insert(0, key);
    }
    document_fonts(&mut fonts, db, ui);
    fonts
}

/// The families of rendered documents: the UI font's own styles where it has
/// them, then Inter, then whatever else the UI falls back on, like emoji.
/// Icons aren't reached this way: Inter has glyphs of its own where they are,
/// so draw them in `FontFamily::Proportional`.
fn document_fonts(fonts: &mut FontDefinitions, db: &fontdb::Database, ui: Option<&str>) {
    let fallbacks = fonts
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    let styles = [
        (
            DOC_REGULAR,
            INTER_REGULAR,
            fontdb::Weight::NORMAL,
            fontdb::Style::Normal,
        ),
        (
            DOC_BOLD,
            INTER_BOLD,
            fontdb::Weight::BOLD,
            fontdb::Style::Normal,
        ),
        (
            DOC_ITALIC,
            INTER_ITALIC,
            fontdb::Weight::NORMAL,
            fontdb::Style::Italic,
        ),
        (
            DOC_BOLD_ITALIC,
            INTER_BOLD_ITALIC,
            fontdb::Weight::BOLD,
            fontdb::Style::Italic,
        ),
    ];
    for (family, bytes, weight, style) in styles {
        let mut chain = Vec::new();
        if let Some(name) = ui
            && let Some(data) = load_style(db, name, weight, style)
        {
            let key = format!("system:{name}:{family}");
            fonts.font_data.insert(key.clone(), Arc::new(data));
            chain.push(key);
        }
        let inter = format!("inter:{family}");
        fonts
            .font_data
            .insert(inter.clone(), Arc::new(FontData::from_static(bytes)));
        chain.push(inter);
        chain.extend(fallbacks.iter().cloned());
        fonts
            .families
            .insert(FontFamily::Name(family.into()), chain);
    }
}

/// A family's face in this style, only if it really has one: asking for an
/// italic gets the upright face of a family without any.
fn load_style(
    db: &fontdb::Database,
    family: &str,
    weight: fontdb::Weight,
    style: fontdb::Style,
) -> Option<FontData> {
    let id = db.query(&fontdb::Query {
        families: &[fontdb::Family::Name(family)],
        weight,
        style,
        ..fontdb::Query::default()
    })?;
    let face = db.face(id)?;
    let bold_enough = (weight == fontdb::Weight::BOLD) == (face.weight.0 >= 600);
    let slanted = (style == fontdb::Style::Italic) == (face.style != fontdb::Style::Normal);
    if !bold_enough || !slanted {
        return None;
    }
    db.with_face_data(id, |data, index| {
        let mut font = FontData::from_owned(data.to_vec());
        font.index = index;
        font
    })
}

fn load(db: &fontdb::Database, family: &str) -> Option<FontData> {
    let query = fontdb::Query {
        families: &[fontdb::Family::Name(family)],
        ..fontdb::Query::default()
    };
    let id = db.query(&query)?;
    db.with_face_data(id, |data, index| {
        let mut font = FontData::from_owned(data.to_vec());
        font.index = index;
        font
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_fonts_load_by_family_name() {
        let ctx = egui::Context::default();
        let mut fonts = SystemFonts::default();
        let start = web_time::Instant::now();
        let families = loop {
            if let Some(families) = fonts.families(&ctx) {
                break families;
            }
            assert!(start.elapsed().as_secs() < 20, "fonts are found");
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        eprintln!(
            "{} families, {} monospace, in {:?}",
            families.all.len(),
            families.mono.len(),
            start.elapsed()
        );
        let Some(mono) = families.mono.first().cloned() else {
            return;
        };
        let definitions = definitions(fonts.db.get().unwrap(), None, Some(&mono));
        let key = format!("system:{mono}");
        assert!(definitions.font_data.contains_key(&key));
        assert_eq!(definitions.families[&FontFamily::Monospace][0], key);
    }
}
