//! Platforms without an app list yet: only custom commands work.

use std::{io, path::PathBuf};

use super::{App, Handlers, Launch, Subject, Target};

pub struct Catalog;

impl Catalog {
    pub fn load() -> Self {
        Self
    }

    pub fn is_stale(&self) -> bool {
        false
    }

    pub fn handlers(&self, _subjects: &[Subject]) -> Handlers {
        Handlers::default()
    }

    pub fn all_apps(&self, _subjects: &[Subject]) -> Vec<App> {
        Vec::new()
    }

    pub fn launch(&self, target: &Target, paths: &[PathBuf]) -> io::Result<Vec<Launch>> {
        super::launch_command(target, paths)
    }
}

pub fn icon_image(_icon: &str, _size: u32) -> Option<egui::ColorImage> {
    None
}
