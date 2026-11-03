//! System fonts for the UI and monospace text.

use std::sync::{Arc, OnceLock};

use egui::{FontData, FontDefinitions, FontFamily};

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
            ctx.set_fonts(definitions(db, ui, mono));
        } else if self.installed.is_some() {
            ctx.set_fonts(definitions(&fontdb::Database::new(), None, None));
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
    fonts
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
