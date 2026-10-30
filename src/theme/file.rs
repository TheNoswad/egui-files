//! What a theme file holds. Every field is optional: whatever a file leaves
//! out comes from its `base` theme, and in the end from egui's own look.

use std::fmt;

use egui::Color32;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A color written as `"#rrggbb"` or `"#rrggbbaa"` (not premultiplied).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hex(pub Color32);

impl Hex {
    pub fn parse(text: &str) -> Option<Color32> {
        let digits = text.strip_prefix('#')?;
        if !digits.is_ascii() || !matches!(digits.len(), 6 | 8) {
            return None;
        }
        let byte = |i: usize| u8::from_str_radix(&digits[i..i + 2], 16).ok();
        let alpha = if digits.len() == 8 { byte(6)? } else { 255 };
        Some(Color32::from_rgba_unmultiplied(
            byte(0)?,
            byte(2)?,
            byte(4)?,
            alpha,
        ))
    }
}

impl fmt::Display for Hex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [r, g, b, a] = self.0.to_srgba_unmultiplied();
        if a == 255 {
            write!(f, "#{r:02x}{g:02x}{b:02x}")
        } else {
            write!(f, "#{r:02x}{g:02x}{b:02x}{a:02x}")
        }
    }
}

impl Serialize for Hex {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Hex {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let text = String::deserialize(d)?;
        Hex::parse(&text).map(Hex).ok_or_else(|| {
            serde::de::Error::custom(format!("{text:?} isn't a color like \"#1e1e2e\""))
        })
    }
}

pub type Color = Option<Hex>;

fn is_default<T: Default + PartialEq>(value: &T) -> bool {
    *value == T::default()
}

/// A theme as written in its file.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ThemeFile {
    /// Shown in the theme lists; the file's name when left out.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The theme this one starts from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// Whether it's a dark theme; picks egui's dark or light look underneath.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dark: Option<bool>,
    /// The syntax highlighting theme it suggests.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub syntax: Option<String>,
    #[serde(skip_serializing_if = "is_default")]
    pub colors: Colors,
    #[serde(skip_serializing_if = "is_default")]
    pub files: Files,
    #[serde(skip_serializing_if = "is_default")]
    pub icons: Icons,
    #[serde(skip_serializing_if = "is_default")]
    pub terminal: Terminal,
    #[serde(skip_serializing_if = "is_default")]
    pub media: Media,
    #[serde(skip_serializing_if = "is_default")]
    pub fonts: Fonts,
    /// Any of egui's style settings, in egui's own field names; colors may be
    /// written as hex strings.
    #[serde(skip_serializing_if = "toml::Table::is_empty")]
    pub style: toml::Table,
}

/// Declares a struct of optional colors with a list of its fields, so the
/// editor can show them all.
macro_rules! color_group {
    ($(#[$meta:meta])* $name:ident { $($(#[doc = $doc:literal])* $field:ident: $label:literal,)* }) => {
        $(#[$meta])*
        #[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
        #[serde(default)]
        pub struct $name {
            $(
                $(#[doc = $doc])*
                #[serde(skip_serializing_if = "Option::is_none")]
                pub $field: Color,
            )*
        }

        impl $name {
            /// `(label, hover text, field)` for each color.
            pub fn fields_mut(&mut self) -> Vec<(&'static str, &'static str, &mut Color)> {
                vec![$(($label, concat!($($doc,)*), &mut self.$field)),*]
            }
        }
    };
}

color_group! {
    /// Colors for egui's widgets, each setting several of egui's own.
    Colors {
        /// Panels, the menu bar and tab bars.
        background: "Background",
        /// Windows, menus and popups.
        surface: "Surface",
        /// Text boxes, lists and other sunken areas.
        input: "Input",
        /// Separators and window outlines.
        border: "Border",
        /// Most text.
        text: "Text",
        /// Headings and text on hovered or pressed widgets.
        strong_text: "Strong text",
        /// Hints, hidden files and other secondary text.
        weak_text: "Weak text",
        /// Buttons and other widgets at rest.
        widget: "Widget",
        /// Widgets under the pointer.
        widget_hover: "Widget hovered",
        /// Widgets being pressed.
        widget_active: "Widget pressed",
        /// The text cursor and the default link color.
        accent: "Accent",
        /// Behind selected text and items.
        selection: "Selection",
        /// Selected text and items, and focus outlines.
        selection_text: "Selected text",
        /// Links; the accent when left out.
        link: "Link",
        /// Warnings.
        warning: "Warning",
        /// Errors.
        error: "Error",
    }
}

color_group! {
    /// File lists, the folder tree and places.
    Files {
        /// Behind selected files; the theme's selection when left out.
        selection: "Selection",
        /// Names of selected files.
        selection_text: "Selected text",
        /// Behind the file under the pointer.
        hover: "Hovered",
        /// Every other row of the details view.
        stripe: "Stripe",
        /// Names of hidden files; weak text when left out.
        hidden: "Hidden files",
        /// Names of symbolic links.
        symlink: "Symlinks",
        /// Names of executable files.
        executable: "Executables",
        /// The outline of a folder files are dragged over.
        drop_target: "Drop target",
    }
}

color_group! {
    /// File icons, by kind of file. Left out, an icon takes its name's color.
    Icons {
        folder: "Folders",
        image: "Images",
        audio: "Audio",
        video: "Video",
        pdf: "PDF",
        archive: "Archives",
        code: "Code",
        text: "Text",
        document: "Documents",
        link: "Links",
        file: "Other files",
    }
}

color_group! {
    /// Image and video previews.
    Media {
        /// Behind videos and around images.
        backdrop: "Backdrop",
        /// The circle behind a paused video's play button.
        overlay: "Play button background",
        /// A paused video's play button.
        overlay_icon: "Play button",
    }
}

/// The built-in terminal's colors.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Terminal {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub foreground: Color,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background: Color,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Color,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selection: Color,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selection_text: Color,
    /// The 16 ANSI colors: black, red, green, yellow, blue, magenta, cyan and
    /// white, then their bright versions.
    #[serde(skip_serializing_if = "Option::is_none", deserialize_with = "sixteen")]
    pub ansi: Option<Vec<Hex>>,
}

fn sixteen<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Vec<Hex>>, D::Error> {
    let colors = Vec::<Hex>::deserialize(d)?;
    if colors.len() != 16 {
        return Err(serde::de::Error::custom(format!(
            "ansi needs 16 colors, not {}",
            colors.len()
        )));
    }
    Ok(Some(colors))
}

impl Terminal {
    pub fn fields_mut(&mut self) -> Vec<(&'static str, &'static str, &mut Color)> {
        vec![
            ("Text", "", &mut self.foreground),
            ("Background", "", &mut self.background),
            ("Cursor", "", &mut self.cursor),
            ("Selection", "", &mut self.selection),
            ("Selected text", "", &mut self.selection_text),
        ]
    }
}

/// Fonts, by family name as the system knows them.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Fonts {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ui: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mono: Option<String>,
    /// Body text size in points; the other text styles scale with it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mono_size: Option<f32>,
}

impl Fonts {
    /// `self`, with what it leaves out taken from `under`.
    pub fn over(&self, under: &Fonts) -> Fonts {
        Fonts {
            ui: self.ui.clone().or_else(|| under.ui.clone()),
            mono: self.mono.clone().or_else(|| under.mono.clone()),
            size: self.size.or(under.size),
            mono_size: self.mono_size.or(under.mono_size),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trips() {
        for text in ["#1e1e2e", "#ff000080"] {
            let color = Hex::parse(text).unwrap();
            assert_eq!(Hex(color).to_string(), text);
        }
        assert_eq!(Hex::parse("#12345"), None);
        assert_eq!(Hex::parse("123456"), None);
    }

    #[test]
    fn partial_files_parse() {
        let file: ThemeFile = toml::from_str(
            r##"
            base = "Nord"
            [colors]
            accent = "#bf616a"
            [style.visuals]
            window_corner_radius = { nw = 0, ne = 0, sw = 0, se = 0 }
            "##,
        )
        .unwrap();
        assert_eq!(file.base.as_deref(), Some("Nord"));
        assert!(file.colors.accent.is_some());
        assert!(file.colors.text.is_none());
        assert!(file.style.contains_key("visuals"));
        let text = toml::to_string(&file).unwrap();
        assert!(!text.contains("[icons]"), "{text}");
    }

    #[test]
    fn ansi_needs_sixteen_colors() {
        let result: Result<ThemeFile, _> = toml::from_str("[terminal]\nansi = [\"#000000\"]");
        assert!(result.is_err());
    }
}
