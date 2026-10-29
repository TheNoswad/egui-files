//! The kinds of tab the dock can hold.

use std::{collections::HashSet, path::PathBuf};

use egui::Ui;
use serde::{Deserialize, Serialize};

use crate::{
    tab::{BrowserTab, Shared},
    trash::{self, TrashTab},
};

/// Untagged so state saved before trash tabs existed (plain `BrowserTab`s)
/// still loads.
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
// Only a handful of tabs exist, so the size difference doesn't matter.
#[allow(clippy::large_enum_variant)]
pub enum DockTab {
    Browser(BrowserTab),
    Trash(TrashTab),
}

impl DockTab {
    pub fn id(&self) -> u64 {
        match self {
            Self::Browser(tab) => tab.id,
            Self::Trash(tab) => tab.id,
        }
    }

    pub fn title(&self) -> String {
        match self {
            Self::Browser(tab) => tab.title(),
            Self::Trash(tab) => tab.title(),
        }
    }

    pub fn browser(&self) -> Option<&BrowserTab> {
        match self {
            Self::Browser(tab) => Some(tab),
            Self::Trash(_) => None,
        }
    }

    pub fn is_trash(&self) -> bool {
        matches!(self, Self::Trash(_))
    }

    pub fn ui(&mut self, ui: &mut Ui, shared: &mut Shared) {
        match self {
            Self::Browser(tab) => tab.ui(ui, shared),
            Self::Trash(tab) => tab.ui(ui, shared),
        }
    }

    pub fn mark_stale(&mut self) {
        match self {
            Self::Browser(tab) => tab.mark_stale(),
            Self::Trash(tab) => tab.mark_stale(),
        }
    }

    pub fn dirs_changed(&mut self, dirs: &HashSet<PathBuf>) {
        match self {
            Self::Browser(tab) => tab.dirs_changed(dirs),
            Self::Trash(tab) => {
                if trash::watched_dir().is_some_and(|dir| dirs.contains(&dir)) {
                    tab.mark_stale();
                }
            }
        }
    }

    /// Folders to watch for changes while this tab is open.
    pub fn shown_dirs(&self) -> Vec<PathBuf> {
        match self {
            Self::Browser(tab) => tab.shown_dirs().cloned().collect(),
            Self::Trash(_) => trash::watched_dir().into_iter().collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use egui_dock::DockState;

    use super::*;

    #[test]
    fn loads_state_saved_before_trash_tabs() {
        // What earlier versions saved: a dock of plain browser tabs.
        let old = DockState::new(vec![BrowserTab::new(7, PathBuf::from("/tmp"))]);
        let saved = ron::ser::to_string(&old).unwrap();
        let loaded: DockState<DockTab> = ron::from_str(&saved).unwrap();
        let tabs: Vec<_> = loaded.iter_all_tabs().map(|(_, t)| t).collect();
        assert_eq!(tabs.len(), 1);
        let browser = tabs[0].browser().expect("a browser tab");
        assert_eq!((browser.id, &browser.path), (7, &PathBuf::from("/tmp")));

        // Mixed docks round-trip with the right kinds.
        let mut new = DockState::new(vec![DockTab::Browser(BrowserTab::new(
            1,
            PathBuf::from("/"),
        ))]);
        new.push_to_first_leaf(DockTab::Trash(TrashTab::new(2)));
        let saved = ron::ser::to_string(&new).unwrap();
        let loaded: DockState<DockTab> = ron::from_str(&saved).unwrap();
        let kinds: Vec<_> = loaded
            .iter_all_tabs()
            .map(|(_, t)| (t.id(), t.is_trash()))
            .collect();
        assert_eq!(kinds, [(1, false), (2, true)]);
    }
}
