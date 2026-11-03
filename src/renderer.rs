//! Which graphics API draws the app on Android: Vulkan, or OpenGL for
//! phones whose display hardware garbles Vulkan's frames (a Moto G20, with
//! a Unisoc T700, showed noise at the bottom while scrolling, gone with
//! "Disable HW overlays"). Chosen before the window opens, so it's kept in
//! a file of its own rather than eframe's saved state.

use std::{path::PathBuf, sync::Mutex};

/// The renderer the app is running with. Android may start the app again
/// in the same process, so it's set on every start.
static STARTED: Mutex<Renderer> = Mutex::new(Renderer::Vulkan);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Renderer {
    Vulkan,
    OpenGl,
}

impl Renderer {
    pub const ALL: [(Self, &'static str); 2] = [(Self::Vulkan, "Vulkan"), (Self::OpenGl, "OpenGL")];

    /// The saved renderer, kept as the one the app runs with; call as the
    /// app starts.
    pub fn start() -> Self {
        let renderer = Self::saved();
        *STARTED.lock().unwrap_or_else(|e| e.into_inner()) = renderer;
        renderer
    }

    /// The renderer the app is running with.
    pub fn started() -> Self {
        *STARTED.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The renderer saved for the next start.
    pub fn saved() -> Self {
        match path().and_then(|p| std::fs::read_to_string(p).ok()) {
            Some(text) if text.trim() == "opengl" => Self::OpenGl,
            _ => Self::Vulkan,
        }
    }

    /// Keeps `self` for the next start.
    pub fn save(self) -> std::io::Result<()> {
        let path = path().ok_or_else(|| std::io::Error::other("no config folder"))?;
        match self {
            Self::Vulkan => match std::fs::remove_file(&path) {
                Err(err) if err.kind() != std::io::ErrorKind::NotFound => Err(err),
                _ => Ok(()),
            },
            Self::OpenGl => {
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                std::fs::write(&path, "opengl\n")
            }
        }
    }
}

/// `egui-files/renderer` in the config folder, which on Android is in the
/// app's private storage (`XDG_CONFIG_HOME`, set at startup).
fn path() -> Option<PathBuf> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| directories::BaseDirs::new().map(|d| d.config_dir().to_path_buf()))?;
    Some(config.join("egui-files/renderer"))
}
