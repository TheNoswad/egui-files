//! Set scenes: the app laid out a known way, for screenshots
//! (scripts/screenshots.sh) and for the web demo's start.
//!
//! `EGUI_FILES_SCENE=<name>` starts the desktop app in a scene rather than as
//! it was saved, without the real drives or phones, and without the mouse.
//! With `EGUI_FILES_SCREENSHOT=<file.png>` too, the app saves the window to
//! the file once the scene has settled, and quits. `EGUI_FILES_THEME=light`
//! or `dark` picks the theme.

// The web demo only starts in a scene (see FilesApp::new); the rest is for
// the desktop app's screenshots.
#![cfg_attr(target_arch = "wasm32", allow(dead_code))]

use std::path::PathBuf;
#[cfg(not(target_arch = "wasm32"))]
use std::sync::OnceLock;

use web_time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scene {
    /// The home folder with its README.md previewed beside it.
    Markdown,
    /// Two tabs side by side, and the terminal under them.
    Split,
    /// Pictures as thumbnails, beside an archive's contents.
    Archive,
    /// The mobile layout at a phone's size.
    Phone,
}

impl Scene {
    pub const ALL: [Self; 4] = [Self::Markdown, Self::Split, Self::Archive, Self::Phone];

    pub fn name(self) -> &'static str {
        match self {
            Self::Markdown => "markdown",
            Self::Split => "split",
            Self::Archive => "archive",
            Self::Phone => "phone",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.name() == name)
    }

    /// The window's size, in points.
    pub fn size(self) -> egui::Vec2 {
        match self {
            // A Pixel's screen.
            Self::Phone => egui::vec2(412.0, 915.0),
            _ => egui::vec2(1280.0, 800.0),
        }
    }

    pub fn mobile(self) -> bool {
        self == Self::Phone
    }

    /// What's typed into the terminal, once its shell has started.
    pub fn typed(self) -> Option<&'static str> {
        match self {
            Self::Split => Some("ls -F"),
            _ => None,
        }
    }
}

/// The scene the app was started in, and what to do with it.
pub struct Staged {
    pub scene: Scene,
    pub theme: Option<egui::Theme>,
    /// Where to save the screenshot.
    pub shot: Option<PathBuf>,
}

/// From the environment, read once.
#[cfg(not(target_arch = "wasm32"))]
pub fn staged() -> Option<&'static Staged> {
    static STAGED: OnceLock<Option<Staged>> = OnceLock::new();
    STAGED
        .get_or_init(|| {
            let name = std::env::var("EGUI_FILES_SCENE").ok()?;
            let Some(scene) = Scene::from_name(&name) else {
                let names: Vec<&str> = Scene::ALL.iter().map(|s| s.name()).collect();
                eprintln!("EGUI_FILES_SCENE: no scene {name:?} (there are {names:?})");
                std::process::exit(2);
            };
            let theme = match std::env::var("EGUI_FILES_THEME").as_deref() {
                Ok("light") => Some(egui::Theme::Light),
                Ok("dark") => Some(egui::Theme::Dark),
                _ => None,
            };
            let shot = std::env::var_os("EGUI_FILES_SCREENSHOT").map(PathBuf::from);
            Some(Staged { scene, theme, shot })
        })
        .as_ref()
}

/// The web demo has no environment, and stages nothing.
#[cfg(target_arch = "wasm32")]
pub fn staged() -> Option<&'static Staged> {
    None
}

/// How long after the start the terminal is typed into: its shell is
/// prompting by then.
const TYPE_AFTER: Duration = Duration::from_millis(2500);

/// How long a scene is given to settle before its screenshot: folders
/// listed, thumbnails and the preview made, the terminal's command run.
const SHOOT_AFTER: Duration = Duration::from_secs(6);

/// Taking a screenshot of a staged scene, frame by frame.
pub struct Shooting {
    started: Instant,
    typed: bool,
    asked: bool,
}

impl Shooting {
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
            typed: false,
            asked: false,
        }
    }

    /// What to type into whatever has the keyboard, once, when it's time.
    pub fn type_now(&mut self, scene: Scene) -> Option<&'static str> {
        let text = scene.typed()?;
        if self.typed || self.started.elapsed() < TYPE_AFTER {
            return None;
        }
        self.typed = true;
        Some(text)
    }

    /// Whether to ask eframe for the window's pixels: once, when it's time.
    pub fn shoot_now(&mut self) -> bool {
        if self.asked || self.started.elapsed() < wait() {
            return false;
        }
        self.asked = true;
        true
    }
}

/// `SHOOT_AFTER`, or `EGUI_FILES_SCREENSHOT_AFTER` seconds for slow machines.
fn wait() -> Duration {
    std::env::var("EGUI_FILES_SCREENSHOT_AFTER")
        .ok()
        .and_then(|s| s.parse().ok())
        .map(Duration::from_secs_f64)
        .unwrap_or(SHOOT_AFTER)
}

/// Saves a screenshot as PNG.
#[cfg(not(target_arch = "wasm32"))]
pub fn save(image: &egui::ColorImage, path: &std::path::Path) -> Result<(), String> {
    let [w, h] = image.size;
    let bytes: Vec<u8> = image
        .pixels
        .iter()
        .flat_map(|p| {
            let [r, g, b, _] = p.to_srgba_unmultiplied();
            // The window has no see-through parts.
            [r, g, b, 255]
        })
        .collect();
    let image = image::RgbaImage::from_raw(w as u32, h as u32, bytes)
        .ok_or("the screenshot's size doesn't match its pixels")?;
    image.save(path).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scenes_are_found_by_name() {
        for scene in Scene::ALL {
            assert_eq!(Scene::from_name(scene.name()), Some(scene));
        }
        assert_eq!(Scene::from_name("nope"), None);
    }

    #[test]
    fn the_terminal_is_typed_into_before_the_screenshot() {
        assert!(TYPE_AFTER < SHOOT_AFTER);
    }
}
