//! System fonts for scripts the bundled fonts don't have, like Chinese and
//! Arabic. Which fonts exist differs by machine, so they're chosen by what they
//! cover: as few as it takes, with the best known ones first.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use egui::FontData;

use super::fonts::Extra;

/// One letter of each script (and symbols) to find a font for. A font that has
/// the letter is taken to have the script.
const SAMPLES: [char; 26] = [
    '中', 'あ', '한', // Chinese, Japanese, Korean
    'ש', 'م', // Hebrew, Arabic
    'क', 'ব', 'த', 'త', 'ગ', 'ਗ', 'ಕ', 'മ', 'ස', // Indian scripts, Sinhala
    'ก', 'ກ', 'က', 'ក', 'ཀ', // Thai, Lao, Myanmar, Khmer, Tibetan
    'Ա', 'ა', 'አ', // Armenian, Georgian, Ethiopic
    '●', '∑', '★', '⌘', // Symbols egui's fonts lack
];

/// Taken first, in this order, where they cover what's needed. All-in-one fonts
/// come early, for fewer fonts and matching styles.
const PREFERRED: [&str; 13] = [
    "Noto Sans CJK SC",
    "Source Han Sans SC",
    "Noto Sans SC",
    "Microsoft YaHei",
    "PingFang SC",
    "Noto Sans CJK JP",
    "Yu Gothic",
    "Hiragino Sans",
    "Malgun Gothic",
    "Apple SD Gothic Neo",
    "Noto Sans",
    "Segoe UI",
    "Nirmala UI",
];

/// Cover a lot, but not as well as a font made for the script.
const BROAD: [&str; 3] = ["Arial Unicode MS", "Arial", "DejaVu Sans"];

/// Have a glyph for everything, of no use: "this character is missing".
const NEVER: [&str; 3] = ["LastResort", "Last Resort", "Adobe Blank"];

/// A face to choose from.
#[derive(Clone, Debug)]
struct Candidate<T> {
    face: T,
    family: String,
    /// Which of `SAMPLES` it has, by bit.
    covers: u32,
}

/// Lower is better; the family's name settles ties, so it's always the same.
fn rank(family: &str) -> (usize, &str) {
    let least = PREFERRED.len();
    let score = if let Some(index) = PREFERRED.iter().position(|name| *name == family) {
        index
    } else if family.contains("Noto Sans") {
        least
    } else if let Some(index) = BROAD.iter().position(|name| *name == family) {
        least + 1 + index
    } else if family.contains("Serif") {
        least + BROAD.len() + 2
    } else {
        least + BROAD.len() + 1
    };
    (score, family)
}

/// The best faces that between them cover what any of them covers.
fn choose<T: Copy>(candidates: &[Candidate<T>]) -> Vec<T> {
    let mut sorted: Vec<&Candidate<T>> = candidates
        .iter()
        .filter(|c| c.covers != 0 && !NEVER.contains(&c.family.as_str()))
        .collect();
    sorted.sort_by(|a, b| rank(&a.family).cmp(&rank(&b.family)));
    let mut covered = 0;
    let mut chosen = Vec::new();
    for sample in 0..SAMPLES.len() {
        let bit = 1 << sample;
        if covered & bit != 0 {
            continue;
        }
        if let Some(best) = sorted.iter().find(|c| c.covers & bit != 0) {
            covered |= best.covers;
            chosen.push(best.face);
        }
    }
    chosen
}

/// Which samples a font has.
fn coverage(data: &[u8], index: u32) -> u32 {
    use skrifa::MetadataProvider;
    let Ok(font) = skrifa::FontRef::from_index(data, index) else {
        return 0;
    };
    let charmap = font.charmap();
    SAMPLES
        .iter()
        .enumerate()
        .filter(|(_, sample)| charmap.map(**sample).is_some())
        .fold(0, |covers, (bit, _)| covers | 1 << bit)
}

/// The plain upright faces of the system's fonts, and what they cover.
fn candidates(db: &fontdb::Database) -> Vec<Candidate<fontdb::ID>> {
    db.faces()
        .filter(|face| {
            face.weight == fontdb::Weight::NORMAL
                && face.style == fontdb::Style::Normal
                && face.stretch == fontdb::Stretch::Normal
                && !face.monospaced
        })
        .filter_map(|face| {
            let family = face.families.first()?.0.clone();
            let covers = db.with_face_data(face.id, coverage)?;
            Some(Candidate {
                face: face.id,
                family,
                covers,
            })
        })
        .collect()
}

/// A chosen face, with its bold if there is one.
pub struct Choice {
    regular: fontdb::ID,
    bold: Option<fontdb::ID>,
}

/// Chooses fonts for what the bundled ones lack.
pub fn scan(db: &fontdb::Database) -> Vec<Choice> {
    choose(&candidates(db))
        .into_iter()
        .map(|regular| Choice {
            regular,
            bold: bold_of(db, regular),
        })
        .collect()
}

fn bold_of(db: &fontdb::Database, regular: fontdb::ID) -> Option<fontdb::ID> {
    let family = &db.face(regular)?.families.first()?.0;
    let id = db.query(&fontdb::Query {
        families: &[fontdb::Family::Name(family)],
        weight: fontdb::Weight::BOLD,
        ..fontdb::Query::default()
    })?;
    (db.face(id)?.weight.0 >= 600).then_some(id)
}

/// Makes the chosen fonts ready for egui.
pub fn load(db: &fontdb::Database, choices: &[Choice]) -> Vec<Extra> {
    choices
        .iter()
        .filter_map(|choice| {
            let family = db.face(choice.regular)?.families.first()?.0.clone();
            Some(Extra {
                key: format!("fallback:{family}"),
                regular: Arc::new(load_face(db, choice.regular)?),
                bold: choice.bold.and_then(|id| load_face(db, id)).map(Arc::new),
            })
        })
        .collect()
}

fn load_face(db: &fontdb::Database, id: fontdb::ID) -> Option<FontData> {
    let face = db.face(id)?;
    let path = match &face.source {
        fontdb::Source::File(path) | fontdb::Source::SharedFile(path, _) => Some(path.as_path()),
        fontdb::Source::Binary(_) => None,
    };
    if let Some(bytes) = path.and_then(mapped) {
        let mut font = FontData::from_static(bytes);
        font.index = face.index;
        return Some(font);
    }
    db.with_face_data(id, |data, index| {
        let mut font = FontData::from_owned(data.to_vec());
        font.index = index;
        font
    })
}

/// A font file in memory without reading it: only the glyphs used take up any.
/// A Chinese font is tens of megabytes. Each file is mapped once, for good.
fn mapped(path: &Path) -> Option<&'static [u8]> {
    static MAPS: Mutex<Option<HashMap<PathBuf, &'static [u8]>>> = Mutex::new(None);
    let mut maps = MAPS.lock().ok()?;
    let maps = maps.get_or_insert_default();
    if let Some(bytes) = maps.get(path) {
        return Some(bytes);
    }
    let file = std::fs::File::open(path).ok()?;
    // SAFETY: font files aren't written to in place: packages replace them. The
    // font database maps them the same way.
    let map = unsafe { memmap2::Mmap::map(&file) }.ok()?;
    let bytes: &'static [u8] = Box::leak(Box::new(map));
    maps.insert(path.to_owned(), bytes);
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(face: u32, family: &str, samples: &[char]) -> Candidate<u32> {
        let covers = samples.iter().fold(0, |covers, sample| {
            let bit = SAMPLES.iter().position(|s| s == sample).unwrap();
            covers | 1 << bit
        });
        Candidate {
            face,
            family: family.to_owned(),
            covers,
        }
    }

    #[test]
    fn as_few_fonts_as_it_takes() {
        let chosen = choose(&[
            candidate(1, "Some Chinese", &['中']),
            candidate(2, "Noto Sans CJK SC", &['中', 'あ', '한']),
            candidate(3, "Noto Sans Hebrew", &['ש']),
            candidate(4, "Noto Sans Arabic", &['م']),
            candidate(5, "Noto Sans Hebrew Extra", &['ש']),
        ]);
        assert_eq!(chosen, [2, 3, 4]);
    }

    #[test]
    fn known_fonts_win_and_serifs_lose() {
        let chosen = choose(&[
            candidate(1, "Fancy Serif", &['ש']),
            candidate(2, "Plain", &['ש']),
            candidate(3, "Noto Sans Hebrew", &['ש']),
            candidate(4, "Arial", &['ש']),
        ]);
        assert_eq!(chosen, [3]);
        let chosen = choose(&[candidate(2, "Plain", &['ש']), candidate(4, "Arial", &['ש'])]);
        assert_eq!(chosen, [4]);
        let chosen = choose(&[
            candidate(1, "Fancy Serif", &['ש']),
            candidate(2, "Plain", &['ש']),
        ]);
        assert_eq!(chosen, [2]);
    }

    #[test]
    fn fonts_of_placeholders_and_of_nothing_useful_are_left_out() {
        let chosen = choose(&[
            candidate(1, "LastResort", &['中', 'ש']),
            candidate(2, "Latin Only", &[]),
        ]);
        assert!(chosen.is_empty());
    }

    #[test]
    fn the_samples_fit_their_bits() {
        assert!(SAMPLES.len() <= u32::BITS as usize);
    }

    #[test]
    fn coverage_comes_from_the_font() {
        assert_eq!(coverage(b"not a font", 0), 0);
        let inter = coverage(super::super::fonts::INTER_REGULAR, 0);
        let bit = |sample| 1 << SAMPLES.iter().position(|s| *s == sample).unwrap();
        assert_eq!(inter & bit('中'), 0, "Inter has no Chinese");
    }

    /// What's on this machine varies, so only what follows from it is checked.
    #[test]
    fn this_machines_fonts_load() {
        let mut db = fontdb::Database::new();
        db.load_system_fonts();
        let choices = scan(&db);
        let extras = load(&db, &choices);
        assert_eq!(extras.len(), choices.len());
        for choice in &choices {
            let regular = db.face(choice.regular).unwrap();
            assert_eq!(regular.weight, fontdb::Weight::NORMAL);
        }
        eprintln!(
            "fallbacks: {:?}",
            extras.iter().map(|e| &e.key).collect::<Vec<_>>()
        );
    }
}
