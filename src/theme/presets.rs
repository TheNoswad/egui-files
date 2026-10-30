//! The themes that ship with the app.

use egui::Color32;

use super::file::{Colors, Files, Hex, Icons, Media, Terminal, ThemeFile};

pub const EGUI_DARK: &str = "egui Dark";
pub const EGUI_LIGHT: &str = "egui Light";

/// A palette in the shape most color schemes come in.
struct Palette {
    name: &'static str,
    dark: bool,
    syntax: &'static str,
    /// Panels, windows and sunken areas.
    bg: u32,
    surface: u32,
    input: u32,
    border: u32,
    /// Widgets at rest, hovered and pressed.
    widget: u32,
    hover: u32,
    active: u32,
    text: u32,
    strong: u32,
    weak: u32,
    accent: u32,
    /// Behind selections; a blend of the accent and background otherwise.
    selection: Option<u32>,
    red: u32,
    orange: u32,
    yellow: u32,
    green: u32,
    cyan: u32,
    blue: u32,
    magenta: u32,
    ansi: [u32; 16],
    /// The terminal's background; the input color otherwise.
    term_bg: Option<u32>,
}

fn rgb(rgb: u32) -> Color32 {
    let [_, r, g, b] = rgb.to_be_bytes();
    Color32::from_rgb(r, g, b)
}

fn hex(value: u32) -> Option<Hex> {
    Some(Hex(rgb(value)))
}

/// `a` moved `t` of the way towards `b`.
fn mix(a: u32, b: u32, t: f32) -> u32 {
    let [_, ar, ag, ab] = a.to_be_bytes();
    let [_, br, bg, bb] = b.to_be_bytes();
    let f = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round() as u32;
    (f(ar, br) << 16) | (f(ag, bg) << 8) | f(ab, bb)
}

impl Palette {
    fn theme(&self) -> ThemeFile {
        let selection = self
            .selection
            .unwrap_or_else(|| mix(self.accent, self.bg, if self.dark { 0.55 } else { 0.7 }));
        ThemeFile {
            name: Some(self.name.to_owned()),
            base: None,
            dark: Some(self.dark),
            syntax: Some(self.syntax.to_owned()),
            colors: Colors {
                background: hex(self.bg),
                surface: hex(self.surface),
                input: hex(self.input),
                border: hex(self.border),
                text: hex(self.text),
                strong_text: hex(self.strong),
                weak_text: hex(self.weak),
                widget: hex(self.widget),
                widget_hover: hex(self.hover),
                widget_active: hex(self.active),
                accent: hex(self.accent),
                selection: hex(selection),
                selection_text: hex(self.strong),
                link: None,
                warning: hex(if self.dark { self.yellow } else { self.orange }),
                error: hex(self.red),
            },
            files: Files {
                symlink: hex(self.cyan),
                executable: hex(self.green),
                ..Files::default()
            },
            icons: Icons {
                folder: hex(self.blue),
                image: hex(self.magenta),
                audio: hex(self.cyan),
                video: hex(self.red),
                pdf: hex(self.red),
                archive: hex(self.orange),
                code: hex(self.green),
                text: hex(self.weak),
                document: hex(self.blue),
                link: hex(self.cyan),
                file: hex(self.weak),
            },
            terminal: Terminal {
                foreground: hex(self.text),
                background: hex(self.term_bg.unwrap_or(self.input)),
                cursor: hex(self.strong),
                ansi: Some(self.ansi.iter().map(|&c| Hex(rgb(c))).collect()),
                ..Terminal::default()
            },
            media: Media {
                backdrop: hex(if self.dark { self.input } else { self.surface }),
                ..Media::default()
            },
            ..ThemeFile::default()
        }
    }
}

/// Every preset, in menu order.
pub fn all() -> Vec<ThemeFile> {
    let egui = |name: &str, dark: bool| ThemeFile {
        name: Some(name.to_owned()),
        dark: Some(dark),
        ..ThemeFile::default()
    };
    let mut themes = vec![egui(EGUI_DARK, true), egui(EGUI_LIGHT, false)];
    themes.extend(PALETTES.iter().map(Palette::theme));
    themes
}

/// Catppuccin, from its four flavors' named colors.
#[allow(clippy::too_many_arguments)]
const fn catppuccin(
    name: &'static str,
    syntax: &'static str,
    dark: bool,
    [
        crust,
        mantle,
        base,
        surface0,
        surface1,
        surface2,
        overlay1,
        subtext0,
        subtext1,
        text,
    ]: [u32; 10],
    [red, peach, yellow, green, teal, sky, blue, mauve, pink]: [u32; 9],
) -> Palette {
    let _ = (sky, mauve);
    let (black, white, bright_black, bright_white) = if dark {
        (surface1, subtext1, surface2, subtext0)
    } else {
        (subtext1, surface2, subtext0, surface1)
    };
    Palette {
        name,
        dark,
        syntax,
        bg: mantle,
        surface: base,
        input: if dark { crust } else { base },
        border: surface0,
        widget: surface0,
        hover: surface1,
        active: surface2,
        text,
        strong: text,
        weak: overlay1,
        accent: blue,
        selection: None,
        red,
        orange: peach,
        yellow,
        green,
        cyan: teal,
        blue,
        magenta: pink,
        ansi: [
            black,
            red,
            green,
            yellow,
            blue,
            pink,
            teal,
            white, //
            bright_black,
            red,
            green,
            yellow,
            blue,
            pink,
            teal,
            bright_white,
        ],
        term_bg: Some(base),
    }
}

const SOLARIZED: [u32; 16] = [
    0x073642, 0xdc322f, 0x859900, 0xb58900, 0x268bd2, 0xd33682, 0x2aa198, 0xeee8d5, //
    0x002b36, 0xcb4b16, 0x586e75, 0x657b83, 0x839496, 0x6c71c4, 0x93a1a1, 0xfdf6e3,
];

const PALETTES: &[Palette] = &[
    catppuccin(
        "Catppuccin Latte",
        "Catppuccin Latte",
        false,
        [
            0xdce0e8, 0xe6e9ef, 0xeff1f5, 0xccd0da, 0xbcc0cc, 0xacb0be, 0x8c8fa1, 0x6c6f85,
            0x5c5f77, 0x4c4f69,
        ],
        [
            0xd20f39, 0xfe640b, 0xdf8e1d, 0x40a02b, 0x179299, 0x04a5e5, 0x1e66f5, 0x8839ef,
            0xea76cb,
        ],
    ),
    catppuccin(
        "Catppuccin Frappé",
        "Catppuccin Frappe",
        true,
        [
            0x232634, 0x292c3c, 0x303446, 0x414559, 0x51576d, 0x626880, 0x838ba7, 0xa5adce,
            0xb5bfe2, 0xc6d0f5,
        ],
        [
            0xe78284, 0xef9f76, 0xe5c890, 0xa6d189, 0x81c8be, 0x99d1db, 0x8caaee, 0xca9ee6,
            0xf4b8e4,
        ],
    ),
    catppuccin(
        "Catppuccin Macchiato",
        "Catppuccin Macchiato",
        true,
        [
            0x181926, 0x1e2030, 0x24273a, 0x363a4f, 0x494d64, 0x5b6078, 0x8087a2, 0xa5adcb,
            0xb8c0e0, 0xcad3f5,
        ],
        [
            0xed8796, 0xf5a97f, 0xeed49f, 0xa6da95, 0x8bd5ca, 0x91d7e3, 0x8aadf4, 0xc6a0f6,
            0xf5bde6,
        ],
    ),
    catppuccin(
        "Catppuccin Mocha",
        "Catppuccin Mocha",
        true,
        [
            0x11111b, 0x181825, 0x1e1e2e, 0x313244, 0x45475a, 0x585b70, 0x7f849c, 0xa6adc8,
            0xbac2de, 0xcdd6f4,
        ],
        [
            0xf38ba8, 0xfab387, 0xf9e2af, 0xa6e3a1, 0x94e2d5, 0x89dceb, 0x89b4fa, 0xcba6f7,
            0xf5c2e7,
        ],
    ),
    Palette {
        name: "Nord",
        dark: true,
        syntax: "Nord",
        bg: 0x2e3440,
        surface: 0x3b4252,
        input: 0x272c36,
        border: 0x434c5e,
        widget: 0x3b4252,
        hover: 0x434c5e,
        active: 0x4c566a,
        text: 0xd8dee9,
        strong: 0xeceff4,
        weak: 0x7b88a1,
        accent: 0x88c0d0,
        selection: Some(0x4c566a),
        red: 0xbf616a,
        orange: 0xd08770,
        yellow: 0xebcb8b,
        green: 0xa3be8c,
        cyan: 0x8fbcbb,
        blue: 0x81a1c1,
        magenta: 0xb48ead,
        ansi: [
            0x3b4252, 0xbf616a, 0xa3be8c, 0xebcb8b, 0x81a1c1, 0xb48ead, 0x88c0d0, 0xe5e9f0, //
            0x4c566a, 0xbf616a, 0xa3be8c, 0xebcb8b, 0x81a1c1, 0xb48ead, 0x8fbcbb, 0xeceff4,
        ],
        term_bg: Some(0x2e3440),
    },
    Palette {
        name: "Gruvbox Dark",
        dark: true,
        syntax: "gruvbox-dark",
        bg: 0x282828,
        surface: 0x32302f,
        input: 0x1d2021,
        border: 0x504945,
        widget: 0x3c3836,
        hover: 0x504945,
        active: 0x665c54,
        text: 0xebdbb2,
        strong: 0xfbf1c7,
        weak: 0x928374,
        accent: 0xfabd2f,
        selection: Some(0x665c54),
        red: 0xfb4934,
        orange: 0xfe8019,
        yellow: 0xfabd2f,
        green: 0xb8bb26,
        cyan: 0x8ec07c,
        blue: 0x83a598,
        magenta: 0xd3869b,
        ansi: [
            0x282828, 0xcc241d, 0x98971a, 0xd79921, 0x458588, 0xb16286, 0x689d6a, 0xa89984, //
            0x928374, 0xfb4934, 0xb8bb26, 0xfabd2f, 0x83a598, 0xd3869b, 0x8ec07c, 0xebdbb2,
        ],
        term_bg: Some(0x282828),
    },
    Palette {
        name: "Gruvbox Light",
        dark: false,
        syntax: "gruvbox-light",
        bg: 0xf2e5bc,
        surface: 0xfbf1c7,
        input: 0xf9f5d7,
        border: 0xd5c4a1,
        widget: 0xebdbb2,
        hover: 0xd5c4a1,
        active: 0xbdae93,
        text: 0x3c3836,
        strong: 0x282828,
        weak: 0x7c6f64,
        accent: 0xb57614,
        selection: Some(0xd5c4a1),
        red: 0x9d0006,
        orange: 0xaf3a03,
        yellow: 0xb57614,
        green: 0x79740e,
        cyan: 0x427b58,
        blue: 0x076678,
        magenta: 0x8f3f71,
        ansi: [
            0xfbf1c7, 0xcc241d, 0x98971a, 0xd79921, 0x458588, 0xb16286, 0x689d6a, 0x7c6f64, //
            0x928374, 0x9d0006, 0x79740e, 0xb57614, 0x076678, 0x8f3f71, 0x427b58, 0x3c3836,
        ],
        term_bg: Some(0xfbf1c7),
    },
    Palette {
        name: "Dracula",
        dark: true,
        syntax: "Dracula",
        bg: 0x21222c,
        surface: 0x282a36,
        input: 0x191a21,
        border: 0x44475a,
        widget: 0x343746,
        hover: 0x44475a,
        active: 0x565970,
        text: 0xf8f8f2,
        strong: 0xffffff,
        weak: 0x6272a4,
        accent: 0xbd93f9,
        selection: Some(0x44475a),
        red: 0xff5555,
        orange: 0xffb86c,
        yellow: 0xf1fa8c,
        green: 0x50fa7b,
        cyan: 0x8be9fd,
        blue: 0xbd93f9,
        magenta: 0xff79c6,
        ansi: [
            0x21222c, 0xff5555, 0x50fa7b, 0xf1fa8c, 0xbd93f9, 0xff79c6, 0x8be9fd, 0xf8f8f2, //
            0x6272a4, 0xff6e6e, 0x69ff94, 0xffffa5, 0xd6acff, 0xff92df, 0xa4ffff, 0xffffff,
        ],
        term_bg: Some(0x282a36),
    },
    Palette {
        name: "Solarized Dark",
        dark: true,
        syntax: "Solarized (dark)",
        bg: 0x002b36,
        surface: 0x073642,
        input: 0x00212b,
        border: 0x174652,
        widget: 0x073642,
        hover: 0x174652,
        active: 0x2a5561,
        text: 0x93a1a1,
        strong: 0xeee8d5,
        weak: 0x657b83,
        accent: 0x268bd2,
        selection: Some(0x174652),
        red: 0xdc322f,
        orange: 0xcb4b16,
        yellow: 0xb58900,
        green: 0x859900,
        cyan: 0x2aa198,
        blue: 0x268bd2,
        magenta: 0xd33682,
        ansi: SOLARIZED,
        term_bg: Some(0x002b36),
    },
    Palette {
        name: "Solarized Light",
        dark: false,
        syntax: "Solarized (light)",
        bg: 0xeee8d5,
        surface: 0xfdf6e3,
        input: 0xfdf6e3,
        border: 0xd9d2bf,
        widget: 0xe4ddc8,
        hover: 0xd9d2bf,
        active: 0xc9c2ae,
        text: 0x586e75,
        strong: 0x073642,
        weak: 0x93a1a1,
        accent: 0x268bd2,
        selection: Some(0xd9d2bf),
        red: 0xdc322f,
        orange: 0xcb4b16,
        yellow: 0xb58900,
        green: 0x859900,
        cyan: 0x2aa198,
        blue: 0x268bd2,
        magenta: 0xd33682,
        ansi: SOLARIZED,
        term_bg: Some(0xfdf6e3),
    },
    Palette {
        name: "One Dark",
        dark: true,
        syntax: "OneHalfDark",
        bg: 0x21252b,
        surface: 0x282c34,
        input: 0x1b1d23,
        border: 0x3e4451,
        widget: 0x2c313a,
        hover: 0x3e4451,
        active: 0x4b5263,
        text: 0xabb2bf,
        strong: 0xdcdfe4,
        weak: 0x5c6370,
        accent: 0x61afef,
        selection: Some(0x3e4451),
        red: 0xe06c75,
        orange: 0xd19a66,
        yellow: 0xe5c07b,
        green: 0x98c379,
        cyan: 0x56b6c2,
        blue: 0x61afef,
        magenta: 0xc678dd,
        ansi: [
            0x282c34, 0xe06c75, 0x98c379, 0xe5c07b, 0x61afef, 0xc678dd, 0x56b6c2, 0xabb2bf, //
            0x5c6370, 0xe06c75, 0x98c379, 0xe5c07b, 0x61afef, 0xc678dd, 0x56b6c2, 0xffffff,
        ],
        term_bg: Some(0x282c34),
    },
    Palette {
        name: "Tokyo Night",
        dark: true,
        syntax: "TwoDark",
        bg: 0x16161e,
        surface: 0x1a1b26,
        input: 0x13131a,
        border: 0x292e42,
        widget: 0x222436,
        hover: 0x292e42,
        active: 0x3b4261,
        text: 0xa9b1d6,
        strong: 0xc0caf5,
        weak: 0x565f89,
        accent: 0x7aa2f7,
        selection: Some(0x283457),
        red: 0xf7768e,
        orange: 0xff9e64,
        yellow: 0xe0af68,
        green: 0x9ece6a,
        cyan: 0x7dcfff,
        blue: 0x7aa2f7,
        magenta: 0xbb9af7,
        ansi: [
            0x15161e, 0xf7768e, 0x9ece6a, 0xe0af68, 0x7aa2f7, 0xbb9af7, 0x7dcfff, 0xa9b1d6, //
            0x414868, 0xf7768e, 0x9ece6a, 0xe0af68, 0x7aa2f7, 0xbb9af7, 0x7dcfff, 0xc0caf5,
        ],
        term_bg: Some(0x1a1b26),
    },
    Palette {
        name: "Breeze Dark",
        dark: true,
        syntax: "OneHalfDark",
        bg: 0x202326,
        surface: 0x292c30,
        input: 0x141618,
        border: 0x3f4449,
        widget: 0x292c30,
        hover: 0x33373c,
        active: 0x3e4247,
        text: 0xfcfcfc,
        strong: 0xffffff,
        weak: 0xa1a9b1,
        accent: 0x3daee9,
        selection: Some(0x2a5d7c),
        red: 0xda4453,
        orange: 0xf67400,
        yellow: 0xfdbc4b,
        green: 0x27ae60,
        cyan: 0x1abc9c,
        blue: 0x1d99f3,
        magenta: 0x9b59b6,
        ansi: [
            0x232627, 0xed1515, 0x11d116, 0xf67400, 0x1d99f3, 0x9b59b6, 0x1abc9c, 0xfcfcfc, //
            0x7f8c8d, 0xc0392b, 0x1cdc9a, 0xfdbc4b, 0x3daee9, 0x8e44ad, 0x16a085, 0xffffff,
        ],
        term_bg: Some(0x232627),
    },
    Palette {
        name: "Breeze Light",
        dark: false,
        syntax: "OneHalfLight",
        bg: 0xeff0f1,
        surface: 0xf7f7f7,
        input: 0xffffff,
        border: 0xcdd0d3,
        widget: 0xfcfcfc,
        hover: 0xe3e5e7,
        active: 0xd3d6d9,
        text: 0x232629,
        strong: 0x000000,
        weak: 0x707d8a,
        accent: 0x3daee9,
        selection: Some(0xb6dff6),
        red: 0xda4453,
        orange: 0xf67400,
        yellow: 0xc9900a,
        green: 0x27ae60,
        cyan: 0x16a085,
        blue: 0x2980b9,
        magenta: 0x8e44ad,
        ansi: [
            0x232627, 0xed1515, 0x11d116, 0xf67400, 0x1d99f3, 0x9b59b6, 0x1abc9c, 0xfcfcfc, //
            0x7f8c8d, 0xc0392b, 0x1cdc9a, 0xfdbc4b, 0x3daee9, 0x8e44ad, 0x16a085, 0xffffff,
        ],
        term_bg: Some(0xffffff),
    },
    Palette {
        name: "Adwaita Dark",
        dark: true,
        syntax: "OneHalfDark",
        bg: 0x222226,
        surface: 0x2e2e32,
        input: 0x1d1d20,
        border: 0x3a3a3e,
        widget: 0x36363a,
        hover: 0x414145,
        active: 0x4c4c50,
        text: 0xffffff,
        strong: 0xffffff,
        weak: 0x9a9a9d,
        accent: 0x78aeed,
        selection: Some(0x2a4b73),
        red: 0xff7b63,
        orange: 0xffa348,
        yellow: 0xf8e45c,
        green: 0x8ff0a4,
        cyan: 0x93ddc2,
        blue: 0x78aeed,
        magenta: 0xdc8add,
        ansi: [
            0x241f31, 0xc01c28, 0x2ec27e, 0xf5c211, 0x1e78e4, 0x9841bb, 0x0ab9dc, 0xc0bfbc, //
            0x5e5c64, 0xed333b, 0x57e389, 0xf8e45c, 0x51a1ff, 0xc061cb, 0x4fd2fd, 0xf6f5f4,
        ],
        term_bg: Some(0x1d1d20),
    },
    Palette {
        name: "Adwaita Light",
        dark: false,
        syntax: "OneHalfLight",
        bg: 0xebebed,
        surface: 0xfafafb,
        input: 0xffffff,
        border: 0xd6d6d8,
        widget: 0xe1e1e3,
        hover: 0xd6d6d8,
        active: 0xc8c8cb,
        text: 0x323232,
        strong: 0x000000,
        weak: 0x77767b,
        accent: 0x1c71d8,
        selection: Some(0xc4daf6),
        red: 0xc01c28,
        orange: 0xc64600,
        yellow: 0x9c6e03,
        green: 0x1b8553,
        cyan: 0x0b7e8e,
        blue: 0x1c71d8,
        magenta: 0x9141ac,
        ansi: [
            0x241f31, 0xc01c28, 0x2ec27e, 0xf5c211, 0x1e78e4, 0x9841bb, 0x0ab9dc, 0xc0bfbc, //
            0x5e5c64, 0xed333b, 0x57e389, 0xf8e45c, 0x51a1ff, 0xc061cb, 0x4fd2fd, 0xf6f5f4,
        ],
        term_bg: Some(0xffffff),
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_have_unique_names_and_known_syntax_themes() {
        let themes = all();
        let mut names: Vec<_> = themes.iter().map(|t| t.name.clone().unwrap()).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), themes.len());
        let syntaxes: Vec<_> = crate::editor::highlight::theme_names().collect();
        for theme in &themes {
            if let Some(syntax) = &theme.syntax {
                assert!(syntaxes.contains(&syntax.as_str()), "{syntax}");
            }
        }
    }

    #[test]
    fn presets_survive_toml() {
        for theme in all() {
            let text = toml::to_string(&theme).unwrap();
            let back: ThemeFile = toml::from_str(&text).unwrap();
            assert_eq!(back, theme, "{text}");
        }
    }
}
