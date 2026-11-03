//! The terminal panel's tabs in the browser, which can't run a shell: each
//! says so. Keeps the native terminal's API, and its saved layout.

use std::path::{Path, PathBuf};

use egui::{Id, Ui};
use serde::{Deserialize, Serialize};

use super::{FollowState, TerminalResponse};

#[derive(Default, Serialize, Deserialize)]
pub struct Terminal {
    pub id: u64,
    pub follow: bool,
    #[serde(default)]
    pub dir: Option<PathBuf>,
}

impl Terminal {
    pub fn new(id: u64, follow: bool) -> Self {
        Self {
            id,
            follow,
            dir: None,
        }
    }

    pub fn with_command(id: u64, command: crate::open_with::Launch) -> Self {
        Self {
            dir: command.dir,
            ..Self::new(id, false)
        }
    }

    pub fn remember_dir(&mut self) {}

    pub fn widget_id(&self) -> Id {
        Id::new(("terminal", self.id))
    }

    pub fn request_focus(&mut self) {}

    pub fn title(&self) -> Option<&str> {
        None
    }

    pub fn set_follow(&mut self, follow: bool) {
        self.follow = follow;
    }

    pub fn ui(
        &mut self,
        ui: &mut Ui,
        _folder: Option<&Path>,
        _follow: FollowState,
    ) -> TerminalResponse {
        ui.centered_and_justified(|ui| {
            ui.weak("The terminal needs the desktop app: a browser can't run a shell.");
        });
        TerminalResponse::default()
    }
}
