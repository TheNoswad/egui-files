//! A terminal running the user's shell, shown in the terminal panel.
//!
//! The terminal itself is in `emulator`, on `alacritty_terminal`. On Linux a
//! terminal set to follow the focused folder `cd`s along while it sits at a
//! prompt, and `cd` in its shell navigates the focused tab. The browser can't
//! run a shell, so the web build has only `unsupported`'s placeholder.

use std::path::PathBuf;

use egui::{Color32, Ui};
use egui_dock::Split;

#[cfg(not(target_arch = "wasm32"))]
mod emulator;
#[cfg(target_arch = "wasm32")]
mod unsupported;

#[cfg(not(target_arch = "wasm32"))]
pub use emulator::Terminal;
#[cfg(target_arch = "wasm32")]
pub use unsupported::Terminal;

/// What the panel should do after a terminal was drawn.
#[derive(Default)]
pub struct TerminalResponse {
    /// The shell changed folder; show it in the focused tab.
    pub navigate: Option<PathBuf>,
    /// The shell exited or the user closed the terminal.
    pub close: bool,
    /// Open another terminal next to this one, or in a new tab when `None`.
    pub open: Option<Option<Split>>,
    /// Change which terminals follow the focused folder.
    pub follow: Option<Follow>,
}

/// A change to which terminals follow the focused folder.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Follow {
    /// This terminal follows, or stops.
    This(bool),
    /// Every terminal follows, or none does.
    All(bool),
    /// Terminals opened from now on follow.
    New(bool),
}

/// The panel-wide side of folder following, for the menus.
#[derive(Clone, Copy, Default)]
pub struct FollowState {
    /// Every terminal follows.
    pub all: bool,
    /// New terminals follow.
    pub new: bool,
}

/// Buttons to open another terminal.
pub fn open_menu(ui: &mut Ui, response: &mut TerminalResponse) {
    for (label, shortcut, open) in [
        ("New tab", "Ctrl+Shift+T", None),
        ("Split right", "Ctrl+Shift+E", Some(Split::Right)),
        ("Split below", "Ctrl+Shift+O", Some(Split::Below)),
    ] {
        if ui
            .add(egui::Button::new(label).shortcut_text(shortcut))
            .clicked()
        {
            response.open = Some(open);
            ui.close();
        }
    }
}

/// Menu items for folder following; `follow` is this terminal's setting.
pub fn follow_menu(ui: &mut Ui, follow: bool, state: FollowState) -> Option<Follow> {
    let mut change = None;
    let mut this = follow;
    if ui
        .checkbox(&mut this, "Follow folder")
        .on_hover_text(
            "Keep the shell in the focused tab's folder, and the tab in the shell's (Ctrl+Shift+L)",
        )
        .changed()
    {
        change = Some(Follow::This(this));
    }
    let all = if state.all {
        "No terminal follows"
    } else {
        "All terminals follow"
    };
    if ui
        .add(egui::Button::new(all).shortcut_text("Ctrl+Shift+Alt+L"))
        .clicked()
    {
        change = Some(Follow::All(!state.all));
    }
    let mut new = state.new;
    if ui
        .checkbox(&mut new, "New terminals follow")
        .on_hover_text("Terminals opened from now on start out following the folder")
        .changed()
    {
        change = Some(Follow::New(new));
    }
    if change.is_some() {
        ui.close();
    }
    change
}

const DARK_ANSI: [u32; 16] = [
    0x000000, 0xcd3131, 0x0dbc79, 0xe5e510, 0x2472c8, 0xbc3fbc, 0x11a8cd, 0xe5e5e5, //
    0x666666, 0xf14c4c, 0x23d18b, 0xf5f543, 0x3b8eea, 0xd670d6, 0x29b8db, 0xffffff,
];
const LIGHT_ANSI: [u32; 16] = [
    0x000000, 0xcd3131, 0x00a000, 0x949800, 0x0451a5, 0xbc05bc, 0x0598bc, 0x555555, //
    0x666666, 0xcd3131, 0x14ce14, 0xb5ba00, 0x0451a5, 0xbc05bc, 0x0598bc, 0xa5a5a5,
];

fn hex(rgb: u32) -> Color32 {
    let [_, r, g, b] = rgb.to_be_bytes();
    Color32::from_rgb(r, g, b)
}

/// The ANSI colors for themes that don't set their own.
pub fn default_ansi(dark: bool) -> [Color32; 16] {
    if dark { DARK_ANSI } else { LIGHT_ANSI }.map(hex)
}
