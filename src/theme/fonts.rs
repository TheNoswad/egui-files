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
/// and kept, must be laid out again, or it draws from a texture that's gone.
pub fn generation(ctx: &egui::Context) -> u64 {
    let (old, new, from): (u64, u64, u64) = ctx
        .data(|d| d.get_temp(generation_id()))
        .unwrap_or_default();
    if root_pass(ctx) >= from { new } else { old }
}

fn generation_id() -> egui::Id {
    egui::Id::new("font-generation")
}

fn root_pass(ctx: &egui::Context) -> u64 {
    ctx.cumulative_pass_nr_for(egui::ViewportId::ROOT)
}

pub fn set_fonts(ctx: &egui::Context, fonts: FontDefinitions) {
    ctx.set_fonts(fonts);
    // New fonts are used from the next pass: until then, what's laid out is
    // still with the old ones.
    let current = generation(ctx);
    let next = (current, current + 1, root_pass(ctx) + 1);
    ctx.data_mut(|d| d.insert_temp(generation_id(), next));
    ctx.request_repaint();
}

/// Fonts for what the UI's and documents' own fonts don't have. Does nothing
/// but in the browser, where it may fetch one.
#[cfg(target_arch = "wasm32")]
pub fn want(text: &str) {
    super::web_fonts::want(text);
}

#[cfg(not(target_arch = "wasm32"))]
pub fn want(_text: &str) {}

/// A font to fall back on, for scripts the others lack.
#[derive(Clone)]
pub struct Extra {
    pub key: String,
    pub regular: Arc<FontData>,
    pub bold: Option<Arc<FontData>>,
}

/// The system's fonts, and which to fall back on.
struct Scan {
    db: fontdb::Database,
    #[cfg(not(target_arch = "wasm32"))]
    choices: Vec<super::fallback::Choice>,
}

/// The installed font families, found on a background thread since scanning
/// the system's fonts takes a moment.
#[derive(Default)]
pub struct SystemFonts {
    scan: Arc<OnceLock<Scan>>,
    started: bool,
    /// The choice installed into egui: `(ui, mono)`, and how many fallbacks.
    installed: Option<(Option<String>, Option<String>, usize)>,
    families: Option<Families>,
    extras: Vec<Extra>,
    #[cfg(not(target_arch = "wasm32"))]
    extras_loaded: bool,
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
        // The browser has no fonts to scan, nor threads to do it on.
        #[cfg(target_arch = "wasm32")]
        let _ = ctx;
        #[cfg(not(target_arch = "wasm32"))]
        {
            let scan = self.scan.clone();
            let ctx = ctx.clone();
            let spawned = std::thread::Builder::new()
                .name("system-fonts".into())
                .spawn(move || {
                    let mut db = fontdb::Database::new();
                    db.load_system_fonts();
                    // fontdb doesn't know where Android's are.
                    #[cfg(target_os = "android")]
                    db.load_fonts_dir("/system/fonts");
                    let choices = super::fallback::scan(&db);
                    let _ = scan.set(Scan { db, choices });
                    ctx.request_repaint();
                });
            if let Err(err) = spawned {
                eprintln!("system fonts: {err}");
            }
        }
    }

    /// The families to choose from, once they've been found.
    pub fn families(&mut self, ctx: &egui::Context) -> Option<&Families> {
        self.start(ctx);
        if self.families.is_none() {
            let db = &self.scan.get()?.db;
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

    /// Makes egui use these fonts; `None` keeps egui's own. Adds the fonts to
    /// fall back on as they become available.
    pub fn install(&mut self, ctx: &egui::Context, ui: Option<&str>, mono: Option<&str>) {
        self.start(ctx);
        let scan = self.scan.get();
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(scan) = scan
            && !std::mem::replace(&mut self.extras_loaded, true)
        {
            self.extras = super::fallback::load(&scan.db, &scan.choices);
        }
        #[cfg(target_arch = "wasm32")]
        self.extras.extend(super::web_fonts::poll(ctx));
        let wanted = (
            ui.map(str::to_owned),
            mono.map(str::to_owned),
            self.extras.len(),
        );
        if self.installed.as_ref() == Some(&wanted) {
            return;
        }
        let none = fontdb::Database::new();
        let db = match scan {
            Some(scan) => &scan.db,
            // Keep egui's fonts until the system's have been found.
            None if ui.is_some() || mono.is_some() => return,
            None => &none,
        };
        // At startup with nothing to add, what's there already is right.
        if self.installed.is_some() || wanted != (None, None, 0) {
            set_fonts(ctx, definitions(db, ui, mono, &self.extras));
        }
        self.installed = Some(wanted);
    }
}

/// egui's fonts and the icon font, with `ui` and `mono` put first.
pub fn definitions(
    db: &fontdb::Database,
    ui: Option<&str>,
    mono: Option<&str>,
    extras: &[Extra],
) -> FontDefinitions {
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
    // After everything else: they're only for what the others don't have.
    for extra in extras {
        fonts
            .font_data
            .insert(extra.key.clone(), extra.regular.clone());
        let bold_key = format!("{}:bold", extra.key);
        if let Some(bold) = &extra.bold {
            fonts.font_data.insert(bold_key.clone(), bold.clone());
        }
        for (family, chain) in &mut fonts.families {
            let bold = matches!(family, FontFamily::Name(name) if &**name == DOC_BOLD || &**name == DOC_BOLD_ITALIC);
            chain.push(if bold && extra.bold.is_some() {
                bold_key.clone()
            } else {
                extra.key.clone()
            });
        }
    }
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
    fn new_fonts_count_from_the_pass_they_take_effect_in() {
        let ctx = egui::Context::default();
        let run = |check: &dyn Fn(&egui::Context)| {
            let mut output = ctx.run_ui(egui::RawInput::default(), |ui| check(ui.ctx()));
            output.textures_delta.clear();
        };
        run(&|ctx| {
            assert_eq!(generation(ctx), 0);
            set_fonts(ctx, FontDefinitions::default());
            assert_eq!(generation(ctx), 0, "still the old fonts in this pass");
        });
        run(&|ctx| assert_eq!(generation(ctx), 1));
        run(&|ctx| assert_eq!(generation(ctx), 1));
    }

    #[test]
    fn fallbacks_come_last_and_bold_where_they_can() {
        let font = || Arc::new(FontData::from_static(INTER_REGULAR));
        let extras = [
            Extra {
                key: "a".to_owned(),
                regular: font(),
                bold: Some(font()),
            },
            Extra {
                key: "b".to_owned(),
                regular: font(),
                bold: None,
            },
        ];
        let fonts = definitions(&fontdb::Database::new(), None, None, &extras);
        let chain = |family: FontFamily| fonts.families[&family].clone();
        for family in [FontFamily::Proportional, FontFamily::Monospace] {
            let chain = chain(family);
            assert_eq!(chain[chain.len() - 2..], ["a", "b"]);
        }
        let bold = chain(FontFamily::Name(DOC_BOLD.into()));
        assert_eq!(bold[bold.len() - 2..], ["a:bold", "b"]);
        let regular = chain(FontFamily::Name(DOC_REGULAR.into()));
        assert_eq!(regular[regular.len() - 2..], ["a", "b"]);
        for chain in fonts.families.values() {
            for key in chain {
                assert!(fonts.font_data.contains_key(key), "{key}");
            }
        }
    }

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
        let definitions = definitions(&fonts.scan.get().unwrap().db, None, Some(&mono), &[]);
        let key = format!("system:{mono}");
        assert!(definitions.font_data.contains_key(&key));
        assert_eq!(definitions.families[&FontFamily::Monospace][0], key);
    }
}
