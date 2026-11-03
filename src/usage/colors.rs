//! How the charts color files and folders: by file type, by the folder they
//! sit in, or by how long ago they changed.

use std::collections::HashMap;

use egui::{Color32, Theme};
use serde::{Deserialize, Serialize};

use super::scan::{CATEGORIES, Category, Kind, Metric, NONE, Tree};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ColorMode {
    #[default]
    Type,
    Folder,
    Age,
}

impl ColorMode {
    pub const ALL: [Self; 3] = [Self::Type, Self::Folder, Self::Age];

    pub fn label(self) -> &'static str {
        match self {
            Self::Type => "File type",
            Self::Folder => "Top folder",
            Self::Age => "Age",
        }
    }
}

/// Eight hues in an order that keeps neighbours apart for color-blind
/// readers, stepped separately for light and dark backgrounds.
const SERIES_LIGHT: [u32; 8] = [
    0x2a78d6, 0xeb6834, 0x1baf7a, 0xeda100, 0xe87ba4, 0x008300, 0x6250d6, 0xe34948,
];
const SERIES_DARK: [u32; 8] = [
    0x3987e5, 0xd95926, 0x199e70, 0xc98500, 0xd55181, 0x008300, 0x9085e9, 0xe66767,
];
const GRAY_LIGHT: u32 = 0x9a9893;
const GRAY_DARK: u32 = 0x6b6a66;

/// One hue, light to dark on light backgrounds; old files get the strongest
/// step so they stand out.
const AGE_LIGHT: [u32; 7] = [
    0x86b6ef, 0x6da7ec, 0x5598e7, 0x3987e5, 0x2a78d6, 0x1c5cab, 0x0d366b,
];
const AGE_DARK: [u32; 7] = [
    0x184f95, 0x1c5cab, 0x256abf, 0x2a78d6, 0x3987e5, 0x6da7ec, 0x9ec5f4,
];

const DAY: i64 = 24 * 60 * 60;
/// Upper ends of the age buckets.
const AGES: [(i64, &str); 7] = [
    (7 * DAY, "This week"),
    (31 * DAY, "This month"),
    (183 * DAY, "6 months"),
    (365 * DAY, "1 year"),
    (2 * 365 * DAY, "2 years"),
    (5 * 365 * DAY, "5 years"),
    (i64::MAX, "Older"),
];

fn hex(rgb: u32) -> Color32 {
    Color32::from_rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

#[derive(Clone)]
pub struct Palette {
    pub categories: [Color32; CATEGORIES],
    pub series: [Color32; 8],
    pub gray: Color32,
    pub ages: [Color32; 7],
    /// Behind folders' contents in the treemap.
    pub folder: Color32,
}

impl Palette {
    pub fn new(theme: Theme) -> Self {
        let dark = theme == Theme::Dark;
        let series = (if dark { SERIES_DARK } else { SERIES_LIGHT }).map(hex);
        let gray = hex(if dark { GRAY_DARK } else { GRAY_LIGHT });
        let mut categories = [gray; CATEGORIES];
        categories[..8].copy_from_slice(&series);
        Self {
            categories,
            series,
            gray,
            ages: (if dark { AGE_DARK } else { AGE_LIGHT }).map(hex),
            folder: if dark {
                Color32::from_gray(38)
            } else {
                Color32::from_gray(222)
            },
        }
    }

    pub fn category(&self, category: Category) -> Color32 {
        self.categories[category.index()]
    }
}

pub fn age_bucket(now: i64, modified: i64) -> Option<usize> {
    (modified > 0).then(|| {
        let age = (now - modified).max(0);
        AGES.iter().position(|&(end, _)| age < end).unwrap_or(6)
    })
}

pub fn age_labels() -> impl Iterator<Item = &'static str> {
    AGES.iter().map(|&(_, label)| label)
}

pub fn now() -> i64 {
    disk::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// Colors nodes for one frame of a chart rooted at `focus`.
pub struct Painter<'a> {
    pub tree: &'a Tree,
    pub metric: Metric,
    pub mode: ColorMode,
    pub palette: &'a Palette,
    pub highlight: Option<Category>,
    focus: u32,
    /// The biggest folders and files right inside `focus`, by color slot.
    top: HashMap<u32, usize>,
    now: i64,
}

impl<'a> Painter<'a> {
    pub fn new(
        tree: &'a Tree,
        focus: u32,
        metric: Metric,
        mode: ColorMode,
        palette: &'a Palette,
        highlight: Option<Category>,
    ) -> Self {
        let top = tree
            .sorted_children(focus, metric)
            .into_iter()
            .take(8)
            .enumerate()
            .map(|(slot, id)| (id, slot))
            .collect();
        Self {
            tree,
            metric,
            mode,
            palette,
            highlight,
            focus,
            top,
            now: now(),
        }
    }

    pub fn color(&self, id: u32) -> Color32 {
        let node = self.tree.node(id);
        let color = match self.mode {
            ColorMode::Type => match node.kind {
                Kind::Symlink | Kind::Mount => self.palette.gray,
                _ => self.palette.category(node.category(self.metric)),
            },
            ColorMode::Folder => {
                let mut top = id;
                loop {
                    let parent = self.tree.node(top).parent;
                    if parent == self.focus || parent == NONE {
                        break;
                    }
                    top = parent;
                }
                self.top
                    .get(&top)
                    .map_or(self.palette.gray, |&slot| self.palette.series[slot])
            }
            ColorMode::Age => age_bucket(self.now, node.newest)
                .map_or(self.palette.gray, |bucket| self.palette.ages[bucket]),
        };
        if self.dimmed(id) {
            color.gamma_multiply(0.18)
        } else {
            color
        }
    }

    /// Shown faintly: not the highlighted file type (folders count when at
    /// least a tenth of them is).
    pub fn dimmed(&self, id: u32) -> bool {
        let Some(highlight) = self.highlight else {
            return false;
        };
        let node = self.tree.node(id);
        match node.kind {
            Kind::File(category) => category != highlight,
            Kind::Dir => node.totals.category(self.metric, highlight) * 10 < node.size(self.metric),
            Kind::Symlink | Kind::Mount => true,
        }
    }
}

/// Black or white, whichever reads better on `fill`.
pub fn text_on(fill: Color32) -> Color32 {
    let [r, g, b, _] = fill.to_srgba_unmultiplied();
    let luma = 0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32;
    if luma > 150.0 {
        Color32::from_gray(15)
    } else {
        Color32::from_gray(245)
    }
}
