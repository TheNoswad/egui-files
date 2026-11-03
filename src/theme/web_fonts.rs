//! The browser has no system fonts to fall back on, and these are too big to
//! put in the wasm, so they're files beside the page (see `web/fonts.sh`), and
//! only fetched once text in that script comes along.

const JAPANESE: &str = "NotoSansJP-Regular.otf";
const CHINESE: &str = "NotoSansSC-Regular.otf";
const KOREAN: &str = "NotoSansKR-Regular.otf";

/// The smaller fonts, by the ranges of their scripts.
const SCRIPTS: [(&str, &[(u32, u32)]); 8] = [
    ("NotoSansArmenian-Regular.ttf", &[(0x530, 0x58F)]),
    (
        "NotoSansHebrew-Regular.ttf",
        &[(0x590, 0x5FF), (0xFB1D, 0xFB4F)],
    ),
    (
        "NotoSansArabic-Regular.ttf",
        &[
            (0x600, 0x6FF),
            (0x750, 0x77F),
            (0xFB50, 0xFDFF),
            (0xFE70, 0xFEFF),
        ],
    ),
    ("NotoSansDevanagari-Regular.ttf", &[(0x900, 0x97F)]),
    ("NotoSansBengali-Regular.ttf", &[(0x980, 0x9FF)]),
    ("NotoSansTamil-Regular.ttf", &[(0xB80, 0xBFF)]),
    ("NotoSansThai-Regular.ttf", &[(0xE00, 0xE7F)]),
    ("NotoSansGeorgian-Regular.ttf", &[(0x10A0, 0x10FF)]),
];

fn is_kana(c: u32) -> bool {
    matches!(c, 0x3040..=0x30FF | 0x31F0..=0x31FF)
}

fn is_han(c: u32) -> bool {
    matches!(c, 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF | 0x20000..=0x2FFFF)
}

fn is_hangul(c: u32) -> bool {
    matches!(c, 0x1100..=0x11FF | 0x3130..=0x318F | 0xAC00..=0xD7AF)
}

/// The fonts this text needs. Text with kana is Japanese, whose font has the
/// Chinese characters it uses as well; those are 4 to 8 MB each, so it's worth
/// not fetching both.
fn files_for(text: &str) -> Vec<&'static str> {
    let mut files = Vec::new();
    let (mut kana, mut han, mut hangul) = (false, false, false);
    for c in text.chars().map(u32::from).filter(|c| *c >= 0x530) {
        kana |= is_kana(c);
        han |= is_han(c);
        hangul |= is_hangul(c);
        for (file, ranges) in SCRIPTS {
            if !files.contains(&file) && ranges.iter().any(|(from, to)| (*from..=*to).contains(&c))
            {
                files.push(file);
            }
        }
    }
    if kana {
        files.push(JAPANESE);
    } else if han {
        files.push(CHINESE);
    }
    if hangul {
        files.push(KOREAN);
    }
    files
}

#[cfg(target_arch = "wasm32")]
mod fetching {
    use std::{cell::RefCell, collections::HashSet, sync::Arc};

    use egui::FontData;

    use super::files_for;
    use crate::theme::fonts::Extra;

    #[derive(Default)]
    struct State {
        asked: HashSet<&'static str>,
        to_fetch: Vec<&'static str>,
        arrived: Vec<Extra>,
    }

    thread_local! {
        static STATE: RefCell<State> = RefCell::default();
    }

    /// Notes which fonts this text needs.
    pub fn want(text: &str) {
        let files = files_for(text);
        if files.is_empty() {
            return;
        }
        STATE.with_borrow_mut(|state| {
            for file in files {
                if state.asked.insert(file) {
                    state.to_fetch.push(file);
                }
            }
        });
    }

    /// Starts what was asked for since the last call, and returns what's here.
    pub fn poll(ctx: &egui::Context) -> Vec<Extra> {
        STATE.with_borrow_mut(|state| {
            for file in state.to_fetch.drain(..) {
                let ctx = ctx.clone();
                let request = ehttp::Request::get(format!("fonts/{file}"));
                ehttp::fetch(request, move |response| {
                    let bytes = match response {
                        Ok(response) if response.ok => response.bytes,
                        Ok(response) => {
                            log::warn!("font {file}: {}", response.status);
                            return;
                        }
                        Err(err) => {
                            log::warn!("font {file}: {err}");
                            return;
                        }
                    };
                    // A host may answer with a page for a missing file.
                    if skrifa::FontRef::new(&bytes).is_err() {
                        log::warn!("font {file}: not a font");
                        return;
                    }
                    let extra = Extra {
                        key: format!("web:{file}"),
                        regular: Arc::new(FontData::from_owned(bytes)),
                        bold: None,
                    };
                    STATE.with_borrow_mut(|state| state.arrived.push(extra));
                    ctx.request_repaint();
                });
            }
            std::mem::take(&mut state.arrived)
        })
    }
}

#[cfg(target_arch = "wasm32")]
pub use fetching::{poll, want};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_needs_nothing() {
        for text in ["", "hello", "café naïve", "αβγ Привет", "→ ✓ € ©", "🚀"] {
            assert_eq!(files_for(text), Vec::<&str>::new(), "{text}");
        }
    }

    #[test]
    fn each_script_gets_its_font() {
        assert_eq!(files_for("שלום"), ["NotoSansHebrew-Regular.ttf"]);
        assert_eq!(files_for("مرحبا"), ["NotoSansArabic-Regular.ttf"]);
        assert_eq!(files_for("สวัสดี"), ["NotoSansThai-Regular.ttf"]);
        assert_eq!(files_for("नमस्ते"), ["NotoSansDevanagari-Regular.ttf"]);
        assert_eq!(files_for("中文"), [CHINESE]);
        assert_eq!(files_for("한국어"), [KOREAN]);
    }

    #[test]
    fn japanese_doesnt_also_fetch_chinese() {
        assert_eq!(files_for("日本語のテキスト"), [JAPANESE]);
        assert_eq!(files_for("日本語"), [CHINESE], "no kana: it can't tell");
        assert_eq!(files_for("日本語の 한국어"), [JAPANESE, KOREAN]);
    }

    #[test]
    fn a_font_is_named_once() {
        assert_eq!(files_for("שלום שלום"), ["NotoSansHebrew-Regular.ttf"]);
    }
}
