//! Android: the system picks the app. The submenu offers one item, which
//! shows Android's own list of apps for the file.

use std::{io, path::PathBuf};

use super::{App, Handlers, Launch, Subject, Target};

/// The one "app": Android's chooser.
const CHOOSER: &str = "android:chooser";

pub struct Catalog;

impl Catalog {
    pub fn load() -> Self {
        Self
    }

    pub fn is_stale(&self) -> bool {
        false
    }

    pub fn handlers(&self, _subjects: &[Subject]) -> Handlers {
        Handlers {
            default: Some(App {
                id: CHOOSER.to_owned(),
                name: "Choose an app…".to_owned(),
                ..App::default()
            }),
            ..Handlers::default()
        }
    }

    pub fn all_apps(&self, _subjects: &[Subject]) -> Vec<App> {
        Vec::new()
    }

    pub fn launch(&self, target: &Target, paths: &[PathBuf]) -> io::Result<Vec<Launch>> {
        match target {
            Target::App(app) if app.id == CHOOSER => {
                // An app is handed one file to view.
                let [path] = paths else {
                    return Err(io::Error::other("Android opens one file at a time"));
                };
                crate::android::open(path, true)?;
                Ok(Vec::new())
            }
            _ => super::launch_command(target, paths),
        }
    }
}

pub fn icon_image(_icon: &str, _size: u32) -> Option<egui::ColorImage> {
    None
}
